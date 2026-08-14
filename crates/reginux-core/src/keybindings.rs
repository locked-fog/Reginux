use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use anyhow::{anyhow, bail, Result};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::config::KeybindingsConfig;

/// Stable action identifiers.  A key binding is policy; an action is API.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Action(pub String);

impl Action {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }
}

impl fmt::Display for Action {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A normalized terminal key stroke.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeyStroke {
    pub code: KeyCode,
    pub modifiers: KeyModifiers,
}

impl KeyStroke {
    pub fn from_event(event: KeyEvent) -> Self {
        normalize_stroke(event.code, event.modifiers)
    }
}

/// Vim-like multi-key sequence.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeySequence(pub Vec<KeyStroke>);

impl KeySequence {
    pub fn is_prefix_of(&self, other: &[KeyStroke]) -> bool {
        self.0.len() <= other.len() && self.0.iter().zip(other).all(|(a, b)| a == b)
    }

    pub fn display(&self) -> String {
        self.0
            .iter()
            .map(format_keystroke)
            .collect::<Vec<_>>()
            .join(" ")
    }
}

#[derive(Clone, Debug)]
struct Binding {
    context: String,
    sequence: KeySequence,
    display: String,
    action: Action,
}

/// Runtime keymap constructed from the user configuration.
#[derive(Clone, Debug)]
pub struct Keymap {
    bindings: Vec<Binding>,
}

impl Keymap {
    pub fn from_config(config: &KeybindingsConfig) -> Result<Self> {
        let mut bindings = Vec::new();
        for (context, map) in config.contexts() {
            for (display, action) in map {
                let sequence = parse_key_sequence(display)
                    .map_err(|error| anyhow!("invalid binding {display:?}: {error}"))?;
                let action = Action::new(action.clone());
                if !known_action(&action.0) {
                    bail!("unknown action {action}");
                }
                bindings.push(Binding {
                    context: context.to_owned(),
                    sequence,
                    display: display.clone(),
                    action,
                });
            }
        }

        let map = Self { bindings };
        map.validate()?;
        Ok(map)
    }

    pub fn validate(&self) -> Result<()> {
        let mut exact = BTreeSet::new();
        for binding in &self.bindings {
            let key = format!("{}\0{}", binding.context, binding.sequence.display());
            if !exact.insert(key) {
                bail!(
                    "duplicate key sequence in context {}: {}",
                    binding.context,
                    binding.display
                );
            }
        }

        for left in &self.bindings {
            for right in &self.bindings {
                if contexts_can_be_active_together(&left.context, &right.context)
                    && left.sequence.0.len() < right.sequence.0.len()
                    && left.sequence.is_prefix_of(&right.sequence.0)
                {
                    bail!(
                        "sequence prefix conflict across active contexts {} and {}: {} is a prefix of {}",
                        left.context,
                        right.context,
                        left.display,
                        right.display
                    );
                }
            }
        }
        Ok(())
    }

    pub fn resolve(&self, context: &str, sequence: &[KeyStroke]) -> Option<Action> {
        self.resolve_contexts(&[context], sequence)
    }

    pub fn resolve_contexts(&self, contexts: &[&str], sequence: &[KeyStroke]) -> Option<Action> {
        self.bindings
            .iter()
            .filter(|binding| {
                binding.context == "global"
                    || contexts.iter().any(|context| binding.context == *context)
            })
            .filter(|binding| binding.sequence.0.as_slice() == sequence)
            .min_by_key(|binding| {
                contexts
                    .iter()
                    .position(|context| binding.context == *context)
                    .unwrap_or(contexts.len())
            })
            .map(|binding| binding.action.clone())
    }

    pub fn is_prefix(&self, context: &str, sequence: &[KeyStroke]) -> bool {
        self.is_prefix_contexts(&[context], sequence)
    }

    pub fn is_prefix_contexts(&self, contexts: &[&str], sequence: &[KeyStroke]) -> bool {
        self.bindings.iter().any(|binding| {
            (binding.context == "global"
                || contexts.iter().any(|context| binding.context == *context))
                && binding.sequence.0.len() > sequence.len()
                && sequence
                    .iter()
                    .zip(binding.sequence.0.iter())
                    .all(|(left, right)| left == right)
        })
    }

    pub fn hints(&self, context: &str) -> String {
        self.hints_contexts(&[context])
    }

    pub fn hints_contexts(&self, contexts: &[&str]) -> String {
        let mut seen = BTreeSet::new();
        let mut output = Vec::new();
        for context in contexts.iter().copied().chain(std::iter::once("global")) {
            for binding in &self.bindings {
                if binding.context != context {
                    continue;
                }
                if seen.insert(binding.action.0.clone()) {
                    output.push(format!(
                        "{} {}",
                        binding.display,
                        action_label(&binding.action.0)
                    ));
                    if output.len() == 7 {
                        break;
                    }
                }
            }
            if output.len() == 7 {
                break;
            }
        }
        if self.bindings.iter().any(|binding| {
            (binding.context == "global"
                || contexts.iter().any(|context| binding.context == *context))
                && !seen.contains(&binding.action.0)
        }) {
            output.push("… ? Help".to_owned());
        }
        output.join("   ")
    }

    pub fn help_text(&self) -> String {
        let mut by_context: BTreeMap<&str, Vec<&Binding>> = BTreeMap::new();
        for binding in &self.bindings {
            by_context
                .entry(&binding.context)
                .or_default()
                .push(binding);
        }

        let mut output = String::from("Keybindings\n\n");
        for (context, mut bindings) in by_context {
            bindings.sort_by(|a, b| a.display.cmp(&b.display));
            output.push_str(&format!("{}\n{}\n", context, "─".repeat(context.len())));
            for binding in bindings {
                output.push_str(&format!(
                    "{:18} {}\n",
                    binding.display,
                    action_label(&binding.action.0)
                ));
            }
            output.push('\n');
        }
        output
    }

    pub fn action_bindings(&self, action: &str) -> Vec<String> {
        self.bindings
            .iter()
            .filter(|binding| binding.action.0 == action)
            .map(|binding| format!("{}: {}", binding.context, binding.display))
            .collect()
    }
}

fn contexts_can_be_active_together(left: &str, right: &str) -> bool {
    left == right
        || left == "global"
        || right == "global"
        || matches!((left, right), ("form", "browser") | ("browser", "form"))
}

impl Default for Keymap {
    fn default() -> Self {
        Self::from_config(&KeybindingsConfig::default())
            .unwrap_or_else(|_| Self { bindings: vec![] })
    }
}

pub fn known_action(action: &str) -> bool {
    matches!(
        action,
        "navigation.down"
            | "navigation.up"
            | "navigation.left"
            | "navigation.right"
            | "navigation.top"
            | "navigation.bottom"
            | "navigation.page_up"
            | "navigation.page_down"
            | "navigation.activate"
            | "view.next"
            | "view.form"
            | "view.raw"
            | "view.diff"
            | "view.info"
            | "view.plugins"
            | "scope.next"
            | "scope.previous"
            | "config.edit"
            | "config.reload"
            | "changes.undo"
            | "changes.apply"
            | "search.open"
            | "search.next"
            | "search.previous"
            | "help.keybindings"
            | "plugin.approve"
            | "plugin.revoke"
            | "application.back"
            | "application.quit"
    )
}

pub fn action_label(action: &str) -> &'static str {
    match action {
        "navigation.down" => "Move down",
        "navigation.up" => "Move up",
        "navigation.left" => "Back",
        "navigation.right" => "Open",
        "navigation.top" => "Top",
        "navigation.bottom" => "Bottom",
        "navigation.page_up" => "Page up",
        "navigation.page_down" => "Page down",
        "navigation.activate" => "Activate",
        "view.next" => "Next view",
        "view.form" => "Form",
        "view.raw" => "Raw",
        "view.diff" => "Diff",
        "view.info" => "Info",
        "view.plugins" => "Plugins",
        "scope.next" => "Next scope",
        "scope.previous" => "Previous scope",
        "config.edit" => "Edit",
        "config.reload" => "Reload",
        "changes.undo" => "Undo",
        "changes.apply" => "Apply",
        "search.open" => "Search",
        "search.next" => "Next result",
        "search.previous" => "Previous result",
        "help.keybindings" => "Help",
        "plugin.approve" => "Approve plugin",
        "plugin.revoke" => "Revoke approval",
        "application.back" => "Back",
        "application.quit" => "Quit",
        _ => "Unknown",
    }
}

pub fn parse_key_sequence(spec: &str) -> Result<KeySequence> {
    if spec.trim().is_empty() {
        bail!("empty key sequence");
    }

    let mut strokes = Vec::new();
    for token in spec.split_whitespace() {
        if token.contains('+') {
            strokes.push(parse_modified_stroke(token)?);
        } else if is_named_key(token) {
            strokes.push(parse_named_stroke(token)?);
        } else {
            let chars: Vec<char> = token.chars().collect();
            if chars.is_empty() {
                bail!("empty key token");
            }
            strokes.extend(chars.into_iter().map(|ch| KeyStroke {
                code: KeyCode::Char(ch),
                modifiers: KeyModifiers::empty(),
            }));
        }
    }
    Ok(KeySequence(strokes))
}

fn parse_modified_stroke(token: &str) -> Result<KeyStroke> {
    let mut parts = token.split('+').collect::<Vec<_>>();
    let key = parts.pop().ok_or_else(|| anyhow!("missing key"))?;
    let mut modifiers = KeyModifiers::empty();
    for modifier in parts {
        match modifier.to_ascii_lowercase().as_str() {
            "ctrl" | "control" => modifiers.insert(KeyModifiers::CONTROL),
            "alt" | "meta" => modifiers.insert(KeyModifiers::ALT),
            "shift" => modifiers.insert(KeyModifiers::SHIFT),
            _ => bail!("unknown modifier {modifier:?}"),
        }
    }
    let stroke = parse_named_stroke(key).or_else(|_| {
        let mut chars = key.chars();
        let ch = chars.next().ok_or_else(|| anyhow!("missing key"))?;
        if chars.next().is_some() {
            bail!("modified key must be one character or a named key")
        }
        Ok(KeyStroke {
            code: KeyCode::Char(ch),
            modifiers: KeyModifiers::empty(),
        })
    })?;
    Ok(normalize_stroke(stroke.code, modifiers))
}

fn parse_named_stroke(name: &str) -> Result<KeyStroke> {
    let code = match name.to_ascii_lowercase().as_str() {
        "up" => KeyCode::Up,
        "down" => KeyCode::Down,
        "left" => KeyCode::Left,
        "right" => KeyCode::Right,
        "enter" | "return" => KeyCode::Enter,
        "esc" | "escape" => KeyCode::Esc,
        "tab" => KeyCode::Tab,
        "backspace" => KeyCode::Backspace,
        "delete" => KeyCode::Delete,
        "home" => KeyCode::Home,
        "end" => KeyCode::End,
        "pageup" | "page_up" => KeyCode::PageUp,
        "pagedown" | "page_down" => KeyCode::PageDown,
        "space" => KeyCode::Char(' '),
        _ => return Err(anyhow!("unknown named key {name:?}")),
    };
    Ok(KeyStroke {
        code,
        modifiers: KeyModifiers::empty(),
    })
}

fn is_named_key(name: &str) -> bool {
    parse_named_stroke(name).is_ok()
}

fn normalize_modifiers(mut modifiers: KeyModifiers) -> KeyModifiers {
    modifiers.remove(KeyModifiers::SUPER);
    modifiers
}

fn normalize_stroke(mut code: KeyCode, modifiers: KeyModifiers) -> KeyStroke {
    let mut modifiers = normalize_modifiers(modifiers);
    if let KeyCode::Char(ch) = code {
        if modifiers.contains(KeyModifiers::SHIFT) {
            // Terminals normally report the already-shifted printable
            // character (`Q`, `?`, `!`). Keeping SHIFT as well would make
            // bindings such as `Q` and `?` impossible to resolve.
            code = KeyCode::Char(if ch.is_ascii_alphabetic() {
                ch.to_ascii_uppercase()
            } else {
                ch
            });
            modifiers.remove(KeyModifiers::SHIFT);
        }
        if modifiers.contains(KeyModifiers::CONTROL) {
            if let KeyCode::Char(ch) = code {
                code = KeyCode::Char(ch.to_ascii_lowercase());
            }
        }
    }
    KeyStroke { code, modifiers }
}

fn format_keystroke(stroke: &KeyStroke) -> String {
    let mut prefix = String::new();
    if stroke.modifiers.contains(KeyModifiers::CONTROL) {
        prefix.push_str("Ctrl+");
    }
    if stroke.modifiers.contains(KeyModifiers::ALT) {
        prefix.push_str("Alt+");
    }
    if stroke.modifiers.contains(KeyModifiers::SHIFT) {
        prefix.push_str("Shift+");
    }
    let key = match stroke.code {
        KeyCode::Char(' ') => "Space".to_owned(),
        KeyCode::Char(ch) => ch.to_string(),
        KeyCode::Enter => "Enter".to_owned(),
        KeyCode::Esc => "Esc".to_owned(),
        KeyCode::Tab => "Tab".to_owned(),
        KeyCode::Backspace => "Backspace".to_owned(),
        KeyCode::Delete => "Delete".to_owned(),
        KeyCode::Up => "Up".to_owned(),
        KeyCode::Down => "Down".to_owned(),
        KeyCode::Left => "Left".to_owned(),
        KeyCode::Right => "Right".to_owned(),
        KeyCode::Home => "Home".to_owned(),
        KeyCode::End => "End".to_owned(),
        KeyCode::PageUp => "PageUp".to_owned(),
        KeyCode::PageDown => "PageDown".to_owned(),
        other => format!("{other:?}"),
    };
    format!("{prefix}{key}")
}
