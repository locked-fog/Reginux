use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::filesystem::{home_dir, read_text_or_empty};
use crate::model::{
    Backend, ConfigEntry, EditCapability, KeyValueSeparator, Privilege, SourceRef, SourceScope,
    ValueType,
};

pub struct ProviderContext {
    pub home: PathBuf,
}

impl Default for ProviderContext {
    fn default() -> Self {
        Self { home: home_dir() }
    }
}

pub trait Provider {
    fn id(&self) -> &'static str;
    fn label(&self) -> &'static str;
    fn probe(&self, _context: &ProviderContext) -> bool {
        true
    }
    fn discover(&self, context: &ProviderContext) -> Result<Vec<ConfigEntry>>;
}

pub struct HostnameProvider;
pub struct LocaleProvider;
pub struct EnvironmentProvider;
pub struct SysctlProvider;
pub struct HostsProvider;

pub fn builtin_providers() -> Vec<Box<dyn Provider>> {
    vec![
        Box::new(HostnameProvider),
        Box::new(LocaleProvider),
        Box::new(EnvironmentProvider),
        Box::new(SysctlProvider),
        Box::new(HostsProvider),
    ]
}

impl Provider for HostnameProvider {
    fn id(&self) -> &'static str {
        "linux.hostname"
    }

    fn label(&self) -> &'static str {
        "Hostname"
    }

    fn discover(&self, _context: &ProviderContext) -> Result<Vec<ConfigEntry>> {
        let path = PathBuf::from("/etc/hostname");
        let value = read_text_or_empty(&path)?.trim().to_owned();
        Ok(vec![ConfigEntry {
            id: "linux.hostname".to_owned(),
            label: "Hostname".to_owned(),
            section: "System / Identity".to_owned(),
            description: "The system hostname used by the local host identity provider.".to_owned(),
            value: if value.is_empty() {
                "<unset>".to_owned()
            } else {
                value
            },
            default_value: None,
            value_type: ValueType::String,
            source: SourceRef::file("hostname", path.clone(), SourceScope::System),
            edit_capability: EditCapability::File,
            privilege: Privilege::System,
            provider: self.id().to_owned(),
            validation: "non-empty UTF-8 line".to_owned(),
            backend: Backend::WholeFile { path },
            metadata: vec![("backend".to_owned(), "file".to_owned())],
        }])
    }
}

impl Provider for LocaleProvider {
    fn id(&self) -> &'static str {
        "linux.locale"
    }

    fn label(&self) -> &'static str {
        "Locale"
    }

    fn discover(&self, _context: &ProviderContext) -> Result<Vec<ConfigEntry>> {
        let path = PathBuf::from("/etc/locale.conf");
        let values = parse_equals_file(&path)?;
        let mut keys = BTreeSet::new();
        for (key, _) in &values {
            keys.insert(key.clone());
        }
        for key in ["LANG", "LC_TIME", "LC_NUMERIC", "LC_MESSAGES"] {
            keys.insert(key.to_owned());
        }
        let mut entries = Vec::new();
        for key in keys {
            let value = values
                .iter()
                .find(|(candidate, _)| candidate == &key)
                .map(|(_, value)| value.clone())
                .unwrap_or_else(|| "<inherit>".to_owned());
            entries.push(ConfigEntry {
                id: format!("linux.locale.{key}"),
                label: key.clone(),
                section: "System / Locale".to_owned(),
                description: format!("Locale variable {key} from /etc/locale.conf."),
                value,
                default_value: None,
                value_type: ValueType::String,
                source: SourceRef::file("locale", path.clone(), SourceScope::System),
                edit_capability: EditCapability::File,
                privilege: Privilege::System,
                provider: self.id().to_owned(),
                validation: "UTF-8 locale name or <inherit>".to_owned(),
                backend: Backend::KeyValue {
                    path: path.clone(),
                    key,
                    separator: KeyValueSeparator::Equals,
                },
                metadata: vec![("format".to_owned(), "key=value".to_owned())],
            });
        }
        Ok(entries)
    }
}

impl Provider for EnvironmentProvider {
    fn id(&self) -> &'static str {
        "linux.environment"
    }

    fn label(&self) -> &'static str {
        "Environment"
    }

    fn discover(&self, _context: &ProviderContext) -> Result<Vec<ConfigEntry>> {
        let path = PathBuf::from("/etc/environment");
        let values = parse_equals_file(&path)?;
        let source_was_empty = values.is_empty();
        let mut entries = Vec::new();
        for (key, value) in values {
            if is_sensitive_environment_value(&key, &value) {
                continue;
            }
            entries.push(ConfigEntry {
                id: format!("linux.environment.{key}"),
                label: key.clone(),
                section: "System / Environment".to_owned(),
                description: format!("System environment variable {key}."),
                value,
                default_value: None,
                value_type: ValueType::String,
                source: SourceRef::file("environment", path.clone(), SourceScope::System),
                edit_capability: EditCapability::File,
                privilege: Privilege::System,
                provider: self.id().to_owned(),
                validation: "key=value".to_owned(),
                backend: Backend::KeyValue {
                    path: path.clone(),
                    key,
                    separator: KeyValueSeparator::Equals,
                },
                metadata: vec![("format".to_owned(), "key=value".to_owned())],
            });
        }
        if source_was_empty {
            entries.push(raw_file_entry(
                "linux.environment.file",
                "Environment file",
                "System / Environment",
                &path,
                self.id(),
                true,
            ));
        }
        Ok(entries)
    }
}

impl Provider for SysctlProvider {
    fn id(&self) -> &'static str {
        "linux.sysctl"
    }

    fn label(&self) -> &'static str {
        "Sysctl"
    }

    fn discover(&self, _context: &ProviderContext) -> Result<Vec<ConfigEntry>> {
        let mut sources = vec![PathBuf::from("/etc/sysctl.conf")];
        let directory = Path::new("/etc/sysctl.d");
        if let Ok(items) = fs::read_dir(directory) {
            let mut paths = items
                .filter_map(|item| item.ok().map(|item| item.path()))
                .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("conf"))
                .collect::<Vec<_>>();
            paths.sort();
            sources.extend(paths);
        }

        let mut entries = Vec::new();
        for path in sources {
            for (key, value) in parse_sysctl_file(&path)? {
                entries.push(ConfigEntry {
                    id: format!("linux.sysctl.{key}"),
                    label: key.clone(),
                    section: "System / Kernel / Sysctl".to_owned(),
                    description: format!("Kernel sysctl value from {}.", path.display()),
                    value,
                    default_value: None,
                    value_type: ValueType::String,
                    source: SourceRef::file("sysctl", path.clone(), SourceScope::System),
                    edit_capability: EditCapability::File,
                    privilege: Privilege::System,
                    provider: self.id().to_owned(),
                    validation: "non-empty sysctl value".to_owned(),
                    backend: Backend::KeyValue {
                        path: path.clone(),
                        key,
                        separator: KeyValueSeparator::Equals,
                    },
                    metadata: vec![("format".to_owned(), "key = value".to_owned())],
                });
            }
        }
        if entries.is_empty() {
            let path = PathBuf::from("/etc/sysctl.conf");
            entries.push(raw_file_entry(
                "linux.sysctl.file",
                "Sysctl configuration file",
                "System / Kernel / Sysctl",
                &path,
                self.id(),
                true,
            ));
        }
        Ok(entries)
    }
}

impl Provider for HostsProvider {
    fn id(&self) -> &'static str {
        "linux.hosts"
    }

    fn label(&self) -> &'static str {
        "Hosts"
    }

    fn discover(&self, _context: &ProviderContext) -> Result<Vec<ConfigEntry>> {
        let path = PathBuf::from("/etc/hosts");
        Ok(vec![raw_file_entry(
            "linux.hosts.file",
            "Hosts file",
            "System / Identity",
            &path,
            self.id(),
            true,
        )])
    }
}

fn raw_file_entry(
    id: &str,
    label: &str,
    section: &str,
    path: &Path,
    provider: &str,
    writable: bool,
) -> ConfigEntry {
    ConfigEntry {
        id: id.to_owned(),
        label: label.to_owned(),
        section: section.to_owned(),
        description: format!("Raw configuration source at {}.", path.display()),
        value: "Raw file".to_owned(),
        default_value: None,
        value_type: ValueType::Raw,
        source: SourceRef::file("raw", path.to_path_buf(), SourceScope::System),
        edit_capability: if writable {
            EditCapability::File
        } else {
            EditCapability::None
        },
        privilege: Privilege::System,
        provider: provider.to_owned(),
        validation: "UTF-8 text without NUL bytes".to_owned(),
        backend: Backend::WholeFile {
            path: path.to_path_buf(),
        },
        metadata: vec![("view".to_owned(), "raw".to_owned())],
    }
}

fn parse_equals_file(path: &Path) -> Result<Vec<(String, String)>> {
    let text = read_text_or_empty(path)?;
    Ok(text
        .lines()
        .filter_map(|line| {
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                return None;
            }
            let (key, value) = trimmed.split_once('=')?;
            Some((
                key.trim().to_owned(),
                value.trim().trim_matches('"').to_owned(),
            ))
        })
        .collect())
}

fn parse_sysctl_file(path: &Path) -> Result<Vec<(String, String)>> {
    parse_equals_file(path).with_context(|| format!("parse sysctl source {}", path.display()))
}

fn is_sensitive_environment_value(key: &str, value: &str) -> bool {
    let upper = key.to_ascii_uppercase();
    let sensitive_key = upper
        .split(|ch: char| !ch.is_ascii_alphanumeric())
        .any(|part| {
            matches!(
                part,
                "PASSWORD"
                    | "PASSWD"
                    | "SECRET"
                    | "TOKEN"
                    | "CREDENTIAL"
                    | "CREDENTIALS"
                    | "AUTH"
                    | "COOKIE"
                    | "APIKEY"
            )
        })
        || upper.ends_with("_API_KEY")
        || upper.ends_with("_PRIVATE_KEY");
    let url_contains_userinfo = value
        .split_once("://")
        .map(|(_, remainder)| {
            remainder
                .split('/')
                .next()
                .unwrap_or(remainder)
                .contains('@')
        })
        .unwrap_or(false);
    sensitive_key || url_contains_userinfo
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn environment_provider_filters_likely_secrets() {
        assert!(is_sensitive_environment_value("API_TOKEN", "abc"));
        assert!(is_sensitive_environment_value(
            "HTTPS_PROXY",
            "https://user:password@example.invalid:443"
        ));
        assert!(!is_sensitive_environment_value("LANG", "en_US.UTF-8"));
        assert!(!is_sensitive_environment_value(
            "CODEX_PROXY_CERT",
            "/etc/ssl/cert.pem"
        ));
    }
}
