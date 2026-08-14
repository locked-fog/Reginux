//! Formatting-preserving structured Schema readers and writers.

use anyhow::{anyhow, bail, Context, Result};
use kdl::{KdlDocument, KdlEntry, KdlNode, KdlValue};
use toml_edit::{DocumentMut, Item, Value as TomlValue};

use crate::model::ValueType;

pub fn is_structured(format: &str) -> bool {
    matches!(format, "toml" | "ini" | "kdl")
}

pub fn find_value(text: &str, key: &str, format: &str) -> Result<Option<String>> {
    match format {
        "kitty" | "whitespace" => Ok(find_line_value(text, key, ' ')),
        "key_value" | "equals" => Ok(find_line_value(text, key, '=')),
        "toml" => find_toml(text, key),
        "ini" => Ok(find_ini(text, key)),
        "kdl" => find_kdl(text, key),
        other => bail!("unsupported structured format {other}"),
    }
}

pub fn replace_value(
    text: &str,
    key: &str,
    value: &str,
    format: &str,
    value_type: &ValueType,
    insert: Option<&str>,
) -> Result<String> {
    match format {
        "kitty" | "whitespace" => replace_line_value(text, key, value, ' ', insert),
        "key_value" | "equals" => replace_line_value(text, key, value, '=', insert),
        "toml" => replace_toml(text, key, value, value_type, insert),
        "ini" => replace_ini(text, key, value, insert),
        "kdl" => replace_kdl(text, key, value, value_type, insert),
        other => bail!("unsupported structured format {other}"),
    }
}

pub fn remove_value(text: &str, key: &str, format: &str) -> Result<String> {
    match format {
        "kitty" | "whitespace" => Ok(remove_line_value(text, key, ' ')),
        "key_value" | "equals" => Ok(remove_line_value(text, key, '=')),
        "toml" => remove_toml(text, key),
        "ini" => Ok(remove_ini(text, key)),
        "kdl" => remove_kdl(text, key),
        other => bail!("unsupported structured format {other}"),
    }
}

fn find_line_value(text: &str, key: &str, separator: char) -> Option<String> {
    text.lines()
        .filter_map(|line| parse_line_value(line, key, separator))
        .next_back()
}

fn parse_line_value(line: &str, key: &str, separator: char) -> Option<String> {
    let trimmed = line.trim();
    if trimmed.is_empty() || trimmed.starts_with('#') {
        return None;
    }
    let raw = if separator == '=' {
        let (candidate, value) = trimmed.split_once('=')?;
        (candidate.trim() == key).then_some(value)?
    } else {
        let end = trimmed.find(char::is_whitespace)?;
        (trimmed[..end] == *key).then_some(trimmed[end..].trim_start())?
    };
    Some(strip_line_comment(raw).trim().trim_matches('"').to_owned())
}

fn replace_line_value(
    text: &str,
    key: &str,
    value: &str,
    separator: char,
    insert: Option<&str>,
) -> Result<String> {
    let target = text
        .lines()
        .enumerate()
        .filter(|(_, line)| parse_line_value(line, key, separator).is_some())
        .map(|(index, _)| index)
        .last();
    if target.is_none() && insert != Some("end") && insert != Some("section") {
        bail!("field is absent and no deterministic insert strategy was declared");
    }
    let mut lines = text.lines().map(str::to_owned).collect::<Vec<_>>();
    if let Some(index) = target {
        let line = &lines[index];
        let (prefix, raw) = if separator == '=' {
            let offset = line.find('=').unwrap_or(line.len());
            (&line[..offset + 1], &line[offset + 1..])
        } else {
            let start = line.len() - line.trim_start().len();
            let end = start + key.len();
            let spacing = line[end..]
                .chars()
                .take_while(|ch| ch.is_whitespace())
                .map(char::len_utf8)
                .sum::<usize>();
            (&line[..end + spacing], &line[end + spacing..])
        };
        let comment = line_comment_suffix(raw);
        let quote = raw.trim_start().starts_with('"');
        let encoded = if quote {
            format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
        } else {
            value.to_owned()
        };
        lines[index] = format!("{prefix}{encoded}{comment}");
    } else {
        lines.push(if separator == '=' {
            format!("{key}={value}")
        } else {
            format!("{key} {value}")
        });
    }
    Ok(with_trailing_newline(lines))
}

fn remove_line_value(text: &str, key: &str, separator: char) -> String {
    with_trailing_newline(
        text.lines()
            .filter(|line| parse_line_value(line, key, separator).is_none())
            .map(str::to_owned)
            .collect(),
    )
}

fn strip_line_comment(value: &str) -> &str {
    let suffix = line_comment_suffix(value);
    &value[..value.len() - suffix.len()]
}

fn line_comment_suffix(value: &str) -> &str {
    let mut quote = None;
    let mut escaped = false;
    for (index, ch) in value.char_indices() {
        if escaped {
            escaped = false;
        } else if ch == '\\' && quote.is_some() {
            escaped = true;
        } else if matches!(ch, '\'' | '"') {
            quote = if quote == Some(ch) {
                None
            } else if quote.is_none() {
                Some(ch)
            } else {
                quote
            };
        } else if ch == '#'
            && quote.is_none()
            && (index == 0 || value[..index].ends_with(char::is_whitespace))
        {
            return &value[index.saturating_sub(1)..];
        }
    }
    &value[value.len()..]
}

fn key_parts(key: &str) -> Result<Vec<&str>> {
    let parts = key.split('.').collect::<Vec<_>>();
    if parts.is_empty()
        || parts.iter().any(|part| {
            part.is_empty()
                || !part
                    .chars()
                    .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-'))
        })
    {
        bail!("structured key must be a dotted identifier");
    }
    Ok(parts)
}

fn find_toml(text: &str, key: &str) -> Result<Option<String>> {
    let document = text.parse::<DocumentMut>().context("parse Schema TOML")?;
    let parts = key_parts(key)?;
    let mut table: &dyn toml_edit::TableLike = document.as_table();
    for part in &parts[..parts.len() - 1] {
        let Some(item) = table.get(part) else {
            return Ok(None);
        };
        let Some(next) = item.as_table_like() else {
            return Ok(None);
        };
        table = next;
    }
    Ok(table
        .get(parts[parts.len() - 1])
        .and_then(Item::as_value)
        .map(toml_value_string))
}

fn replace_toml(
    text: &str,
    key: &str,
    value: &str,
    value_type: &ValueType,
    insert: Option<&str>,
) -> Result<String> {
    let mut document = text.parse::<DocumentMut>().context("parse Schema TOML")?;
    let parts = key_parts(key)?;
    let encoded = encode_toml(value, value_type)?;
    let exists = find_toml(text, key)?.is_some();
    if !exists && insert != Some("section") && insert != Some("end") {
        bail!("field is absent and no deterministic insert strategy was declared");
    }
    let mut table = document.as_table_mut();
    for part in &parts[..parts.len() - 1] {
        if !table.contains_key(part) {
            if insert != Some("section") {
                bail!("TOML parent table {part} is absent; use insert=\"section\"");
            }
            table.insert(part, Item::Table(toml_edit::Table::new()));
        }
        table = table
            .get_mut(part)
            .and_then(Item::as_table_mut)
            .ok_or_else(|| anyhow!("TOML path component {part} is not a table"))?;
    }
    let key = parts[parts.len() - 1];
    if let Some(current) = table.get_mut(key).and_then(Item::as_value_mut) {
        let decor = current.decor().clone();
        let mut encoded = encoded;
        *encoded.decor_mut() = decor;
        *current = encoded;
    } else {
        table.insert(key, Item::Value(encoded));
    }
    Ok(document.to_string())
}

fn remove_toml(text: &str, key: &str) -> Result<String> {
    let mut document = text.parse::<DocumentMut>().context("parse Schema TOML")?;
    let parts = key_parts(key)?;
    let mut table = document.as_table_mut();
    for part in &parts[..parts.len() - 1] {
        let Some(next) = table.get_mut(part).and_then(Item::as_table_mut) else {
            return Ok(text.to_owned());
        };
        table = next;
    }
    table.remove(parts[parts.len() - 1]);
    Ok(document.to_string())
}

fn encode_toml(value: &str, value_type: &ValueType) -> Result<TomlValue> {
    Ok(match value_type {
        ValueType::Boolean => TomlValue::from(parse_bool(value)?),
        ValueType::Integer => TomlValue::from(value.trim().parse::<i64>()?),
        ValueType::Float => TomlValue::from(value.trim().parse::<f64>()?),
        _ => TomlValue::from(value),
    })
}

fn toml_value_string(value: &TomlValue) -> String {
    if let Some(value) = value.as_str() {
        value.to_owned()
    } else if let Some(value) = value.as_integer() {
        value.to_string()
    } else if let Some(value) = value.as_float() {
        value.to_string()
    } else if let Some(value) = value.as_bool() {
        value.to_string()
    } else {
        value.to_string().trim().to_owned()
    }
}

fn ini_parts(key: &str) -> Result<(Option<&str>, &str)> {
    let parts = key_parts(key)?;
    match parts.as_slice() {
        [key] => Ok((None, key)),
        [section, key] => Ok((Some(section), key)),
        _ => bail!("INI keys use key or section.key"),
    }
}

fn find_ini(text: &str, key: &str) -> Option<String> {
    let (wanted_section, wanted_key) = ini_parts(key).ok()?;
    let mut section = None;
    let mut found = None;
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            section = Some(trimmed[1..trimmed.len() - 1].trim());
            continue;
        }
        if section != wanted_section || trimmed.starts_with(['#', ';']) {
            continue;
        }
        if let Some((candidate, value)) = trimmed.split_once('=') {
            if candidate.trim() == wanted_key {
                found = Some(strip_ini_comment(value).trim().trim_matches('"').to_owned());
            }
        }
    }
    found
}

fn replace_ini(text: &str, key: &str, value: &str, insert: Option<&str>) -> Result<String> {
    let (wanted_section, wanted_key) = ini_parts(key)?;
    let exists = find_ini(text, key).is_some();
    if !exists && !matches!(insert, Some("section" | "end")) {
        bail!("field is absent and no deterministic insert strategy was declared");
    }
    let mut lines = text.lines().map(str::to_owned).collect::<Vec<_>>();
    let mut section = None;
    let mut target = None;
    let mut section_end = lines.len();
    for (index, line) in lines.iter().enumerate() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            if section == wanted_section {
                section_end = index;
            }
            section = Some(trimmed[1..trimmed.len() - 1].trim());
            continue;
        }
        if section == wanted_section {
            section_end = index + 1;
            if !trimmed.starts_with(['#', ';'])
                && trimmed
                    .split_once('=')
                    .is_some_and(|(candidate, _)| candidate.trim() == wanted_key)
            {
                target = Some(index);
            }
        }
    }
    if let Some(index) = target {
        let indent = lines[index].len() - lines[index].trim_start().len();
        lines[index] = format!("{}{wanted_key}={value}", &lines[index][..indent]);
    } else if let Some(section) = wanted_section {
        let header = format!("[{section}]");
        if let Some(index) = lines.iter().position(|line| line.trim() == header) {
            lines.insert(section_end.max(index + 1), format!("{wanted_key}={value}"));
        } else if insert == Some("section") {
            if !lines.is_empty() && !lines.last().is_some_and(String::is_empty) {
                lines.push(String::new());
            }
            lines.push(header);
            lines.push(format!("{wanted_key}={value}"));
        } else {
            bail!("INI section {section} is absent; use insert=\"section\"");
        }
    } else {
        lines.push(format!("{wanted_key}={value}"));
    }
    Ok(with_trailing_newline(lines))
}

fn remove_ini(text: &str, key: &str) -> String {
    let Ok((wanted_section, wanted_key)) = ini_parts(key) else {
        return text.to_owned();
    };
    let mut section = None;
    let lines = text
        .lines()
        .filter(|line| {
            let trimmed = line.trim();
            if trimmed.starts_with('[') && trimmed.ends_with(']') {
                section = Some(trimmed[1..trimmed.len() - 1].trim());
                return true;
            }
            !(section == wanted_section
                && !trimmed.starts_with(['#', ';'])
                && trimmed
                    .split_once('=')
                    .is_some_and(|(candidate, _)| candidate.trim() == wanted_key))
        })
        .map(str::to_owned)
        .collect();
    with_trailing_newline(lines)
}

fn strip_ini_comment(value: &str) -> &str {
    value
        .find(['#', ';'])
        .map(|index| &value[..index])
        .unwrap_or(value)
}

fn find_kdl(text: &str, key: &str) -> Result<Option<String>> {
    let document = text.parse::<KdlDocument>().context("parse Schema KDL")?;
    let parts = key_parts(key)?;
    Ok(kdl_find_node(&document, &parts)
        .and_then(|node| node.get(0))
        .map(kdl_value_string))
}

fn replace_kdl(
    text: &str,
    key: &str,
    value: &str,
    value_type: &ValueType,
    insert: Option<&str>,
) -> Result<String> {
    let mut document = text.parse::<KdlDocument>().context("parse Schema KDL")?;
    let parts = key_parts(key)?;
    let encoded = encode_kdl(value, value_type)?;
    if let Some(node) = kdl_find_node_mut(&mut document, &parts) {
        if let Some(current) = node
            .entries_mut()
            .iter_mut()
            .find(|entry| entry.name().is_none())
        {
            current.set_value(encoded.clone());
            if let Some(format) = current.format_mut() {
                format.value_repr = encoded.to_string();
            }
        } else {
            node.entries_mut().push(KdlEntry::new(encoded));
        }
    } else {
        if !matches!(insert, Some("section" | "end")) {
            bail!("field is absent and no deterministic insert strategy was declared");
        }
        let (parents, name) = parts.split_at(parts.len() - 1);
        let target = if parents.is_empty() {
            &mut document
        } else {
            let parent = kdl_find_node_mut(&mut document, parents)
                .ok_or_else(|| anyhow!("KDL parent path is absent"))?;
            parent.ensure_children()
        };
        let mut node = KdlNode::new(name[0]);
        node.entries_mut().push(KdlEntry::new(encoded));
        target.nodes_mut().push(node);
    }
    Ok(document.to_string())
}

fn remove_kdl(text: &str, key: &str) -> Result<String> {
    let mut document = text.parse::<KdlDocument>().context("parse Schema KDL")?;
    let parts = key_parts(key)?;
    let (parents, name) = parts.split_at(parts.len() - 1);
    let target = if parents.is_empty() {
        Some(&mut document)
    } else {
        kdl_find_node_mut(&mut document, parents).and_then(|node| node.children_mut().as_mut())
    };
    if let Some(target) = target {
        target
            .nodes_mut()
            .retain(|node| node.name().value() != name[0]);
    }
    Ok(document.to_string())
}

fn kdl_find_node<'a>(document: &'a KdlDocument, parts: &[&str]) -> Option<&'a KdlNode> {
    let node = document.get(parts.first()?)?;
    if parts.len() == 1 {
        Some(node)
    } else {
        kdl_find_node(node.children()?, &parts[1..])
    }
}

fn kdl_find_node_mut<'a>(document: &'a mut KdlDocument, parts: &[&str]) -> Option<&'a mut KdlNode> {
    let node = document.get_mut(parts.first()?)?;
    if parts.len() == 1 {
        Some(node)
    } else {
        kdl_find_node_mut(node.children_mut().as_mut()?, &parts[1..])
    }
}

fn encode_kdl(value: &str, value_type: &ValueType) -> Result<KdlValue> {
    Ok(match value_type {
        ValueType::Boolean => KdlValue::Bool(parse_bool(value)?),
        ValueType::Integer => KdlValue::Integer(value.trim().parse::<i128>()?),
        ValueType::Float => KdlValue::Float(value.trim().parse::<f64>()?),
        _ => KdlValue::String(value.to_owned()),
    })
}

fn kdl_value_string(value: &KdlValue) -> String {
    match value {
        KdlValue::String(value) => value.clone(),
        KdlValue::Integer(value) => value.to_string(),
        KdlValue::Float(value) => value.to_string(),
        KdlValue::Bool(value) => value.to_string(),
        KdlValue::Null => "<unset>".to_owned(),
    }
}

fn parse_bool(value: &str) -> Result<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "true" | "yes" | "on" | "1" => Ok(true),
        "false" | "no" | "off" | "0" => Ok(false),
        _ => bail!("expected a boolean"),
    }
}

fn with_trailing_newline(lines: Vec<String>) -> String {
    let mut output = lines.join("\n");
    if !output.is_empty() && !output.ends_with('\n') {
        output.push('\n');
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn toml_round_trip_preserves_comments() {
        let text = "# header\n[editor]\ntab = 4 # keep\n";
        let updated =
            replace_value(text, "editor.tab", "2", "toml", &ValueType::Integer, None).unwrap();
        assert!(updated.contains("# header"));
        assert!(updated.contains("tab = 2 # keep"));
        assert_eq!(
            find_value(&updated, "editor.tab", "toml")
                .unwrap()
                .as_deref(),
            Some("2")
        );
    }

    #[test]
    fn ini_and_kdl_are_typed_and_deterministic() {
        let ini = replace_value(
            "[ui]\nscale=1\n",
            "ui.scale",
            "2",
            "ini",
            &ValueType::Integer,
            None,
        )
        .unwrap();
        assert_eq!(
            find_value(&ini, "ui.scale", "ini").unwrap().as_deref(),
            Some("2")
        );
        let kdl = replace_value(
            "input {\n  repeat-delay 300\n}\n",
            "input.repeat-delay",
            "250",
            "kdl",
            &ValueType::Integer,
            None,
        )
        .unwrap();
        assert_eq!(
            find_value(&kdl, "input.repeat-delay", "kdl")
                .unwrap()
                .as_deref(),
            Some("250")
        );
    }

    #[test]
    fn absent_fields_require_an_explicit_insertion_strategy() {
        assert!(replace_value("", "font_size", "12", "kitty", &ValueType::Integer, None).is_err());
        let inserted = replace_value(
            "# settings\n",
            "font_size",
            "12",
            "kitty",
            &ValueType::Integer,
            Some("end"),
        )
        .unwrap();
        assert_eq!(
            find_value(&inserted, "font_size", "kitty")
                .unwrap()
                .as_deref(),
            Some("12")
        );
    }
}
