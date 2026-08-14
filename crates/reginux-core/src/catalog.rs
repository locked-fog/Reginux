use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};

use crate::config::AppConfig;
use crate::filesystem::config_dir;
use crate::model::{
    Backend, ConfigEntry, ConfigFile, EditCapability, Privilege, SourceRef, SourceScope, ValueType,
};
use crate::plugin::{discover_plugins, PluginPolicy};
use crate::provider::{builtin_providers, ProviderContext};

pub use crate::plugin::PluginSummary;

pub struct DiscoverOptions {
    pub app_config: AppConfig,
    pub plugin_directories: Vec<String>,
    pub plugin_policy: PluginPolicy,
    /// Kept for API compatibility. Generic directory scanning is removed;
    /// only built-in and manifest-declared sources are indexed.
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
    pub files: Vec<ConfigFile>,
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

        // Deliberately do not scan configuration directories here.  Only
        // built-in providers and plugin-declared sources are allowed to enter
        // the catalog; see `config_files` below for the file-oriented index.

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

        let files = config_files(&entries);
        let plugins = plugin_discovery.summaries;

        entries.sort_by(|left, right| {
            left.section
                .cmp(&right.section)
                .then_with(|| left.label.cmp(&right.label))
                .then_with(|| left.id.cmp(&right.id))
        });
        Self {
            entries,
            files,
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
        self.files = config_files(&self.entries);
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

fn config_files(entries: &[ConfigEntry]) -> Vec<ConfigFile> {
    let mut grouped = BTreeMap::<PathBuf, ConfigFile>::new();
    for entry in entries {
        let Some(path) = entry.source_path() else {
            continue;
        };
        let file = grouped
            .entry(path.to_path_buf())
            .or_insert_with(|| ConfigFile {
                path: path.to_path_buf(),
                scope: if entry.source.is_system() {
                    SourceScope::System
                } else {
                    SourceScope::User
                },
                providers: Vec::new(),
                entry_count: 0,
                exists: path.exists(),
                editable: false,
            });
        if !file.providers.contains(&entry.provider) {
            file.providers.push(entry.provider.clone());
        }
        file.entry_count += 1;
        file.editable |= entry.is_editable();
    }
    grouped.into_values().collect()
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
    fn declared_file_index_keeps_missing_sources_without_directory_scan() {
        let path = PathBuf::from("/tmp/reginux-declared-file-that-does-not-exist");
        let entries = vec![test_entry("declared", &path, "plugin.test")];
        let files = config_files(&entries);
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].path, path);
        assert!(!files[0].exists);
        assert_eq!(files[0].providers, vec!["plugin.test"]);
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
            files: Vec::new(),
            plugins: vec![test_plugin(id, false)],
            diagnostics: Vec::new(),
        };
        let mut refreshed = Catalog {
            entries: Vec::new(),
            files: Vec::new(),
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
