use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use crate::config::AppConfig;
use crate::filesystem::{config_dir, home_dir, is_system_path, read_regular_file_limited};
use crate::model::{
    Backend, ConfigEntry, EditCapability, Privilege, SourceRef, SourceScope, ValueType,
};
use crate::plugin::{discover_plugins, PluginPolicy};
use crate::provider::{builtin_providers, ProviderContext};

pub use crate::plugin::PluginSummary;

pub struct DiscoverOptions {
    pub app_config: AppConfig,
    pub plugin_directories: Vec<String>,
    pub plugin_policy: PluginPolicy,
    pub include_generic_files: bool,
}

impl Default for DiscoverOptions {
    fn default() -> Self {
        let config = AppConfig::default();
        Self {
            plugin_directories: config.plugins.directories.clone(),
            plugin_policy: PluginPolicy {
                approved_adapters: config.plugins.approved_adapters.clone(),
                approved_transforms: config.plugins.approved_transforms.clone(),
                temporary_approvals: HashSet::new(),
                refresh_runtime: false,
            },
            app_config: config,
            include_generic_files: true,
        }
    }
}

pub struct Catalog {
    pub entries: Vec<ConfigEntry>,
    pub plugins: Vec<PluginSummary>,
    pub diagnostics: Vec<String>,
}

impl Catalog {
    pub fn discover(options: DiscoverOptions) -> Self {
        let mut entries = Vec::new();
        let mut diagnostics = Vec::new();
        let context = ProviderContext::default();

        for provider in builtin_providers() {
            if !provider.probe(&context) {
                continue;
            }
            match provider.discover(&context) {
                Ok(mut discovered) => entries.append(&mut discovered),
                Err(error) => diagnostics.push(format!("{}: {error}", provider.id())),
            }
        }

        entries.extend(self_entries(&options.app_config));

        let plugin_discovery =
            discover_plugins(&options.plugin_directories, &options.plugin_policy);
        entries.extend(plugin_discovery.entries);
        diagnostics.extend(plugin_discovery.diagnostics);

        if options.include_generic_files {
            let known_sources = entries
                .iter()
                .filter_map(|entry| entry.source_path().map(Path::to_path_buf))
                .collect::<HashSet<_>>();
            entries.extend(generic_file_entries(&known_sources));
        }

        let id_counts = entries.iter().fold(HashMap::new(), |mut counts, entry| {
            *counts.entry(entry.id.clone()).or_insert(0usize) += 1;
            counts
        });
        for (id, count) in &id_counts {
            if *count > 1 {
                diagnostics.push(format!(
                    "configuration id {id} has {count} providers; every conflicting entry was disabled"
                ));
            }
        }
        entries.retain(|entry| id_counts.get(&entry.id) == Some(&1));

        remove_cross_provider_source_conflicts(&mut entries, &mut diagnostics);

        let identity_counts = entries.iter().filter_map(write_identity).fold(
            HashMap::new(),
            |mut counts, identity| {
                *counts.entry(identity).or_insert(0usize) += 1;
                counts
            },
        );
        for (identity, count) in &identity_counts {
            if *count > 1 {
                diagnostics.push(format!(
                    "write target {identity} has {count} owners; every ambiguous writer was disabled"
                ));
            }
        }
        entries.retain(|entry| {
            write_identity(entry)
                .and_then(|identity| identity_counts.get(&identity).copied())
                .unwrap_or(1)
                == 1
        });

        let plugins = plugin_discovery.summaries;

        entries.sort_by(|left, right| {
            left.section
                .cmp(&right.section)
                .then_with(|| left.label.cmp(&right.label))
                .then_with(|| left.id.cmp(&right.id))
        });
        Self {
            entries,
            plugins,
            diagnostics,
        }
    }

    pub fn search_indices(&self, query: &str) -> Vec<usize> {
        let query = query.to_lowercase();
        self.entries
            .iter()
            .enumerate()
            .filter(|(_, entry)| entry.searchable_text().contains(&query))
            .map(|(index, _)| index)
            .collect()
    }

    pub fn retain_stale_from(&mut self, previous: &Catalog) {
        for plugin in &mut self.plugins {
            if plugin.last_error.is_none() {
                continue;
            }
            let Some(old_plugin) = previous
                .plugins
                .iter()
                .find(|candidate| candidate.id == plugin.id && candidate.last_error.is_none())
            else {
                continue;
            };
            let error = plugin
                .last_error
                .clone()
                .unwrap_or_else(|| "refresh failed".to_owned());
            plugin.status = "stale snapshot; refresh failed".to_owned();
            plugin.stale = true;
            plugin.captured_at = old_plugin.captured_at.clone();
            let provider = format!("plugin.{}", plugin.id);
            for old_entry in previous
                .entries
                .iter()
                .filter(|entry| entry.provider == provider)
            {
                if self.entries.iter().any(|entry| entry.id == old_entry.id) {
                    continue;
                }
                let mut stale = old_entry.clone();
                stale.edit_capability = EditCapability::None;
                stale.privilege = Privilege::ReadOnly;
                stale.backend = Backend::ReadOnly {
                    reason: "runtime snapshot is stale; refresh successfully before editing"
                        .to_owned(),
                };
                stale.metadata.push(("stale".to_owned(), "true".to_owned()));
                stale
                    .metadata
                    .push(("last_error".to_owned(), error.clone()));
                self.entries.push(stale);
            }
        }
        self.entries.sort_by(|left, right| {
            left.section
                .cmp(&right.section)
                .then_with(|| left.label.cmp(&right.label))
                .then_with(|| left.id.cmp(&right.id))
        });
    }
}

fn write_identity(entry: &ConfigEntry) -> Option<String> {
    match &entry.backend {
        Backend::KeyValue { path, key, .. } | Backend::SchemaField { path, key, .. } => {
            Some(format!("{}::{key}", path.display()))
        }
        Backend::TomlField {
            path, section, key, ..
        } => Some(format!(
            "{}::{}.{}",
            path.display(),
            section.as_deref().unwrap_or(""),
            key
        )),
        Backend::WholeFile { path } => Some(format!("{}::<whole-file>", path.display())),
        Backend::TransformField {
            path, start, end, ..
        } => Some(format!("{}::{start}..{end}", path.display())),
        Backend::AdapterField { .. } | Backend::ReadOnly { .. } => None,
    }
}

fn remove_cross_provider_source_conflicts(
    entries: &mut Vec<ConfigEntry>,
    diagnostics: &mut Vec<String>,
) {
    let path_providers = entries
        .iter()
        .filter_map(|entry| {
            entry
                .backend
                .path()
                .map(|path| (path.clone(), entry.provider.clone()))
        })
        .fold(
            HashMap::<PathBuf, HashSet<String>>::new(),
            |mut owners, (path, provider)| {
                owners.entry(path).or_default().insert(provider);
                owners
            },
        );
    for (path, providers) in &path_providers {
        if providers.len() > 1 {
            diagnostics.push(format!(
                "source {} has {} plugin/provider owners; every writer was disabled",
                path.display(),
                providers.len()
            ));
        }
    }
    entries.retain(|entry| {
        entry
            .backend
            .path()
            .and_then(|path| path_providers.get(path))
            .is_none_or(|providers| providers.len() == 1)
    });
}

fn self_entries(config: &AppConfig) -> Vec<ConfigEntry> {
    let path = config_dir().join("config.toml");
    let mut entries = vec![
        toml_entry(
            "reginux.interface.default_view",
            "Default view",
            "Interface",
            "View opened on startup: form, raw, diff, info, or plugins.",
            config.interface.default_view.clone(),
            ValueType::Enum,
            &path,
            Some("interface"),
            "default_view",
        ),
        toml_entry(
            "reginux.interface.show_key_hints",
            "Show key hints",
            "Interface",
            "Whether the bottom context-aware shortcut hint line is shown.",
            config.interface.show_key_hints.to_string(),
            ValueType::Boolean,
            &path,
            Some("interface"),
            "show_key_hints",
        ),
        toml_entry(
            "reginux.interface.key_sequence_timeout_ms",
            "Key sequence timeout",
            "Interface",
            "Milliseconds to wait for a Vim-style multi-key sequence.",
            config.interface.key_sequence_timeout_ms.to_string(),
            ValueType::Integer,
            &path,
            Some("interface"),
            "key_sequence_timeout_ms",
        ),
        toml_entry(
            "reginux.interface.confirm_before_apply",
            "Confirm before apply",
            "Interface",
            "Ask for confirmation before writing staged changes.",
            config.interface.confirm_before_apply.to_string(),
            ValueType::Boolean,
            &path,
            Some("interface"),
            "confirm_before_apply",
        ),
        toml_entry(
            "reginux.editor.command",
            "External editor command",
            "Editor",
            "Program and arguments used for Raw external editing. {file} is substituted safely.",
            config.editor.command.clone(),
            ValueType::String,
            &path,
            Some("editor"),
            "command",
        ),
        toml_entry(
            "reginux.editor.use_environment_editor",
            "Use environment editor",
            "Editor",
            "Prefer VISUAL and EDITOR over the explicit editor command.",
            config.editor.use_environment_editor.to_string(),
            ValueType::Boolean,
            &path,
            Some("editor"),
            "use_environment_editor",
        ),
        toml_entry(
            "reginux.safety.backup_before_apply",
            "Backup before apply",
            "Safety",
            "Create a durable backup before replacing an existing source file.",
            config.safety.backup_before_apply.to_string(),
            ValueType::Boolean,
            &path,
            Some("safety"),
            "backup_before_apply",
        ),
        toml_entry(
            "reginux.safety.allow_system_writes",
            "Allow system writes",
            "Safety",
            "Permit reviewed system file plans to request the installed polkit helper.",
            config.safety.allow_system_writes.to_string(),
            ValueType::Boolean,
            &path,
            Some("safety"),
            "allow_system_writes",
        ),
    ];
    if let Some(entry) = entries
        .iter_mut()
        .find(|entry| entry.id == "reginux.interface.default_view")
    {
        entry
            .metadata
            .push(("values".to_owned(), "form|raw|diff|info|plugins".to_owned()));
        entry.validation = "enum; values=form,raw,diff,info,plugins".to_owned();
    }
    if let Some(entry) = entries
        .iter_mut()
        .find(|entry| entry.id == "reginux.interface.key_sequence_timeout_ms")
    {
        entry.metadata.push(("min".to_owned(), "50".to_owned()));
        entry.metadata.push(("max".to_owned(), "5000".to_owned()));
        entry.validation = "integer; min=50; max=5000".to_owned();
    }
    entries
}

#[allow(clippy::too_many_arguments)]
fn toml_entry(
    id: &str,
    label: &str,
    section: &str,
    description: &str,
    value: String,
    value_type: ValueType,
    path: &Path,
    toml_section: Option<&str>,
    key: &str,
) -> ConfigEntry {
    ConfigEntry {
        id: id.to_owned(),
        label: label.to_owned(),
        section: format!("Reginux / {section}"),
        description: description.to_owned(),
        value,
        default_value: None,
        value_type: value_type.clone(),
        source: SourceRef::file("reginux_config", path.to_path_buf(), SourceScope::User),
        edit_capability: EditCapability::File,
        privilege: Privilege::User,
        provider: "reginux.self".to_owned(),
        validation: value_type.as_str().to_owned(),
        backend: Backend::TomlField {
            path: path.to_path_buf(),
            section: toml_section.map(str::to_owned),
            key: key.to_owned(),
            value_type,
        },
        metadata: vec![("self_config".to_owned(), "true".to_owned())],
    }
}

fn generic_file_entries(known_sources: &HashSet<PathBuf>) -> Vec<ConfigEntry> {
    let mut result = Vec::new();
    let mut seen = known_sources.clone();
    scan_tree(
        &home_dir().join(".config"),
        &home_dir().join(".config"),
        "Config Files / User",
        3,
        120,
        &mut seen,
        &mut result,
    );
    scan_tree(
        Path::new("/etc"),
        Path::new("/etc"),
        "Config Files / System",
        2,
        120,
        &mut seen,
        &mut result,
    );
    result
}

fn scan_tree(
    scan_root: &Path,
    directory: &Path,
    section: &str,
    max_depth: usize,
    limit: usize,
    seen: &mut HashSet<PathBuf>,
    output: &mut Vec<ConfigEntry>,
) {
    if output.len() >= limit || !directory.is_dir() || is_sensitive_path(directory) {
        return;
    }
    let Ok(items) = fs::read_dir(directory) else {
        return;
    };
    let mut paths = items
        .filter_map(|item| item.ok().map(|item| item.path()))
        .collect::<Vec<_>>();
    paths.sort();
    for path in paths {
        if output.len() >= limit {
            break;
        }
        if path.is_dir() {
            if max_depth > 0 {
                scan_tree(
                    scan_root,
                    &path,
                    section,
                    max_depth - 1,
                    limit,
                    seen,
                    output,
                );
            }
            continue;
        }
        if !is_safe_generic_text_file(&path) || !seen.insert(path.clone()) {
            continue;
        }
        let relative = path
            .strip_prefix(scan_root)
            .unwrap_or(&path)
            .display()
            .to_string();
        let privilege = if is_system_path(&path) {
            Privilege::System
        } else {
            Privilege::User
        };
        let (edit_capability, effective_privilege, backend) = if privilege == Privilege::System {
            (
                EditCapability::None,
                Privilege::ReadOnly,
                Backend::ReadOnly {
                    reason: "generic system files have no privileged authorization policy; install a trusted Schema plugin"
                        .to_owned(),
                },
            )
        } else {
            (
                EditCapability::File,
                privilege.clone(),
                Backend::WholeFile { path: path.clone() },
            )
        };
        output.push(ConfigEntry {
            id: format!(
                "files.{}.{}",
                if privilege == Privilege::System {
                    "system"
                } else {
                    "user"
                },
                path.to_string_lossy().replace('/', ".")
            ),
            label: relative.clone(),
            section: section.to_owned(),
            description: format!("Generic raw configuration file at {}.", path.display()),
            value: "Raw file".to_owned(),
            default_value: None,
            value_type: ValueType::Raw,
            source: SourceRef::file(
                "generic",
                path.clone(),
                if privilege == Privilege::System {
                    SourceScope::System
                } else {
                    SourceScope::User
                },
            ),
            edit_capability,
            privilege: effective_privilege,
            provider: "generic.files".to_owned(),
            validation: "UTF-8 text without NUL bytes".to_owned(),
            backend,
            metadata: vec![
                ("relative".to_owned(), relative),
                ("generic".to_owned(), "true".to_owned()),
            ],
        });
    }
}

fn is_safe_generic_text_file(path: &Path) -> bool {
    const MAX_GENERIC_FILE_SIZE: u64 = 1024 * 1024;

    if is_sensitive_path(path) {
        return false;
    }
    let Ok(metadata) = fs::symlink_metadata(path) else {
        return false;
    };
    if metadata.file_type().is_symlink()
        || !metadata.is_file()
        || metadata.len() > MAX_GENERIC_FILE_SIZE
    {
        return false;
    }
    let blocked_extension = path
        .extension()
        .and_then(|value| value.to_str())
        .is_some_and(|extension| {
            matches!(
                extension.to_ascii_lowercase().as_str(),
                "db" | "sqlite"
                    | "sqlite3"
                    | "key"
                    | "pem"
                    | "p12"
                    | "pfx"
                    | "der"
                    | "crt"
                    | "lock"
                    | "sock"
                    | "bin"
            )
        });
    if blocked_extension {
        return false;
    }
    read_regular_file_limited(path, MAX_GENERIC_FILE_SIZE)
        .ok()
        .is_some_and(|bytes| !bytes.contains(&0) && std::str::from_utf8(&bytes).is_ok())
}

fn is_sensitive_path(path: &Path) -> bool {
    path.components().any(|component| {
        let value = component.as_os_str().to_string_lossy().to_ascii_lowercase();
        matches!(
            value.as_str(),
            "shadow"
                | "shadow-"
                | "gshadow"
                | "gshadow-"
                | "secrets"
                | "secret"
                | "credentials"
                | "credential"
                | "keyrings"
                | "keyring"
                | "gnupg"
        ) || value.contains("password")
            || value.contains("private_key")
            || (value.starts_with("ssh_host_") && value.ends_with("_key"))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_entry(id: &str, path: &Path, provider: &str) -> ConfigEntry {
        ConfigEntry {
            id: id.to_owned(),
            label: id.to_owned(),
            section: "Test".to_owned(),
            description: String::new(),
            value: "value".to_owned(),
            default_value: None,
            value_type: ValueType::String,
            source: SourceRef::file("test", path.to_path_buf(), SourceScope::User),
            edit_capability: EditCapability::File,
            privilege: Privilege::User,
            provider: provider.to_owned(),
            validation: "string".to_owned(),
            backend: Backend::WholeFile {
                path: path.to_path_buf(),
            },
            metadata: Vec::new(),
        }
    }

    fn test_plugin(id: &str, failed: bool) -> PluginSummary {
        PluginSummary {
            id: id.to_owned(),
            name: "Test plugin".to_owned(),
            kind: "adapter".to_owned(),
            path: PathBuf::from("/tmp/test-plugin"),
            status: if failed { "error" } else { "loaded" }.to_owned(),
            permissions: Vec::new(),
            trust: "external code".to_owned(),
            approval: "approved".to_owned(),
            digest: Some("sha256:test".to_owned()),
            sources: Vec::new(),
            capabilities: vec!["snapshot".to_owned()],
            captured_at: (!failed).then(|| "2026-08-12T00:00:00Z".to_owned()),
            last_error: failed.then(|| "service unavailable".to_owned()),
            last_error_at: failed.then(|| "2026-08-12T00:01:00Z".to_owned()),
            stale: false,
        }
    }

    #[test]
    fn generic_scanning_blocks_sensitive_path_patterns() {
        assert!(is_sensitive_path(Path::new("/etc/shadow")));
        assert!(is_sensitive_path(Path::new(
            "/etc/ssh/ssh_host_ed25519_key"
        )));
        assert!(is_sensitive_path(Path::new("/tmp/password-store/config")));
        assert!(!is_sensitive_path(Path::new("/etc/ssh/sshd_config")));
    }

    #[test]
    fn cross_provider_source_ownership_disables_every_writer() {
        let path = Path::new("/tmp/shared.conf");
        let mut entries = vec![
            test_entry("one", path, "plugin.one"),
            test_entry("two", path, "plugin.two"),
        ];
        let mut diagnostics = Vec::new();
        remove_cross_provider_source_conflicts(&mut entries, &mut diagnostics);
        assert!(entries.is_empty());
        assert!(diagnostics
            .iter()
            .any(|message| message.contains("every writer was disabled")));
    }

    #[test]
    fn failed_refresh_retains_a_read_only_stale_snapshot() {
        let id = "org.reginux.test.stale";
        let mut entry = test_entry(
            "runtime.field",
            Path::new("/tmp/runtime"),
            &format!("plugin.{id}"),
        );
        entry.source = SourceRef::Command {
            plugin_id: id.to_owned(),
            operation_id: "snapshot".to_owned(),
            program: PathBuf::from("/usr/bin/true"),
        };
        let previous = Catalog {
            entries: vec![entry],
            plugins: vec![test_plugin(id, false)],
            diagnostics: Vec::new(),
        };
        let mut refreshed = Catalog {
            entries: Vec::new(),
            plugins: vec![test_plugin(id, true)],
            diagnostics: Vec::new(),
        };
        refreshed.retain_stale_from(&previous);
        assert!(refreshed.plugins[0].stale);
        assert_eq!(refreshed.entries.len(), 1);
        assert_eq!(refreshed.entries[0].privilege, Privilege::ReadOnly);
        assert!(!refreshed.entries[0].is_editable());
    }
}
