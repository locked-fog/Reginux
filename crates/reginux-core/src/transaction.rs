use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use chrono::Local;
use sha2::{Digest, Sha256};

use crate::filesystem::{
    atomic_write_checked, file_mode, home_dir, make_working_copy, read_bytes_or_empty,
    read_regular_file, read_regular_file_limited, remove_regular_file_checked, write_backup,
};
use crate::model::SourceScope;
use crate::model::{
    AdapterBinding, AdapterInvocation, Backend, ConfigEntry, KeyValueSeparator, ValueType,
};
use crate::plugin::{
    compensate_adapter_binding, execute_adapter_binding, plan_transform_edit,
    validate_adapter_binding, TransformEdit,
};
use crate::privileged::{
    invoke_privileged_helper, PrivilegedFileEdit, PrivilegedRequest, SystemAuthorization,
    HELPER_FILE_LIMIT, HELPER_PROTOCOL_VERSION,
};

const USER_STAGED_FILE_LIMIT: usize = 8 * 1024 * 1024;

fn validate_public_stage_path(path: &Path) -> Result<()> {
    if !path.is_absolute() {
        bail!("public staging paths must be absolute");
    }
    if path
        .components()
        .any(|component| matches!(component, Component::ParentDir))
    {
        bail!("public staging paths may not contain '..'");
    }
    if crate::filesystem::is_system_path(path) {
        bail!("public staging cannot target system paths; stage a declared system entry instead");
    }
    let home = home_dir();
    let temporary = std::env::temp_dir();
    if !path.starts_with(&home) && !path.starts_with(&temporary) {
        bail!(
            "public staging path {} is outside HOME and the temporary directory",
            path.display()
        );
    }
    Ok(())
}

#[derive(Clone, Debug)]
struct StagedFile {
    original: Vec<u8>,
    staged: Vec<u8>,
    mode: Option<u32>,
    existed: bool,
    authorization: Option<SystemAuthorization>,
}

#[derive(Clone, Debug)]
struct StagedAdapter {
    entry_id: String,
    old_value: String,
    new_value: String,
    binding: AdapterBinding,
}

#[derive(Clone, Debug)]
struct StagedEntryValue {
    value: String,
    path: Option<PathBuf>,
}

#[derive(Clone, Debug, Default)]
struct TransactionSnapshot {
    files: BTreeMap<PathBuf, StagedFile>,
    adapters: BTreeMap<String, StagedAdapter>,
    entry_values: BTreeMap<String, StagedEntryValue>,
}

#[derive(Clone, Debug)]
pub struct ValidationIssue {
    pub path: PathBuf,
    pub message: String,
}

#[derive(Debug)]
pub struct ApplyReport {
    pub transaction_id: String,
    pub files: Vec<PathBuf>,
    pub backups: Vec<PathBuf>,
    pub adapter_operations: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct ChangeSummary {
    pub path: PathBuf,
    pub added_lines: usize,
    pub removed_lines: usize,
    pub creates_file: bool,
}

#[derive(Clone, Debug)]
pub struct AdapterChangeSummary {
    pub entry_id: String,
    pub operation_id: String,
    pub transport: String,
    pub typed_arguments: String,
    pub guarantee: String,
    pub scope: String,
    pub has_precondition: bool,
    pub has_validation: bool,
    pub has_verification: bool,
    pub has_compensation: bool,
}

/// A staged transaction over files and declared adapter operations.
#[derive(Default)]
pub struct Transaction {
    files: BTreeMap<PathBuf, StagedFile>,
    adapters: BTreeMap<String, StagedAdapter>,
    entry_values: BTreeMap<String, StagedEntryValue>,
    undo_stack: Vec<TransactionSnapshot>,
}

impl Transaction {
    pub fn changed_count(&self) -> usize {
        self.files.values().filter(|file| file.is_changed()).count() + self.adapters.len()
    }

    pub fn changed_paths(&self) -> Vec<PathBuf> {
        self.files
            .iter()
            .filter(|(_, file)| file.is_changed())
            .map(|(path, _)| path.clone())
            .collect()
    }

    pub fn staged_adapter_plugin_ids(&self) -> Vec<String> {
        let mut ids = self
            .adapters
            .values()
            .map(|adapter| adapter.binding.plugin_id.clone())
            .collect::<Vec<_>>();
        ids.sort();
        ids.dedup();
        ids
    }

    pub fn has_system_changes(&self) -> bool {
        self.changed_paths()
            .iter()
            .any(|path| crate::filesystem::is_system_path(path))
            || self
                .adapters
                .values()
                .any(|adapter| adapter.binding.scope == SourceScope::System)
    }

    pub fn content_for(&self, path: &Path) -> Result<Vec<u8>> {
        if let Some(file) = self.files.get(path) {
            return Ok(file.staged.clone());
        }
        read_bytes_or_empty(path)
    }

    pub fn stage_bytes(&mut self, path: &Path, staged: Vec<u8>) -> Result<()> {
        validate_public_stage_path(path)?;
        self.stage_bytes_internal(path, staged)
    }

    fn stage_bytes_internal(&mut self, path: &Path, staged: Vec<u8>) -> Result<()> {
        let limit = if crate::filesystem::is_system_path(path) {
            HELPER_FILE_LIMIT
        } else {
            USER_STAGED_FILE_LIMIT
        };
        if staged.len() > limit {
            bail!(
                "staged content for {} exceeds the {} byte transaction limit",
                path.display(),
                limit
            );
        }
        let captured = if self.files.contains_key(path) {
            None
        } else {
            Some(capture_original(path)?)
        };
        self.push_undo();
        let entry = self
            .files
            .entry(path.to_path_buf())
            .or_insert_with(|| captured.expect("new staged path has captured original"));
        entry.staged = staged;
        Ok(())
    }

    pub fn stage_entry(&mut self, entry: &ConfigEntry, value: &str) -> Result<()> {
        validate_entry_value(entry, value)?;
        let backend = entry.backend.clone();
        let result = match backend {
            Backend::KeyValue {
                path,
                key,
                separator,
            } => {
                let current = self.content_for(&path)?;
                let text = String::from_utf8(current)
                    .with_context(|| format!("decode {} as UTF-8", path.display()))?;
                let updated =
                    if entry.id.starts_with("linux.locale.") && value.trim() == "<inherit>" {
                        remove_key_value(&text, &key, &separator)
                    } else {
                        replace_key_value(&text, &key, value, separator)
                    };
                self.stage_bytes_internal(&path, updated.into_bytes())
            }
            Backend::SchemaField {
                path,
                key,
                format,
                insert,
                ..
            } => {
                let current = self.content_for(&path)?;
                let text = String::from_utf8(current)
                    .with_context(|| format!("decode {} as UTF-8", path.display()))?;
                let updated = if value.trim() == "<unset>" {
                    crate::structured::remove_value(&text, &key, &format)?
                } else {
                    crate::structured::replace_value(
                        &text,
                        &key,
                        value,
                        &format,
                        &entry.value_type,
                        insert.as_deref(),
                    )?
                };
                self.stage_bytes_internal(&path, updated.into_bytes())
            }
            Backend::TomlField {
                path,
                section,
                key,
                value_type,
            } => {
                let current = self.content_for(&path)?;
                let text = String::from_utf8(current)
                    .with_context(|| format!("decode {} as UTF-8", path.display()))?;
                let normalized = if value_type == ValueType::Boolean {
                    normalize_boolean(value)?
                } else {
                    value.trim().to_owned()
                };
                let updated =
                    replace_toml_field(&text, section.as_deref(), &key, &normalized, &value_type);
                toml::from_str::<toml::Value>(&updated)
                    .with_context(|| format!("updated {} is not valid TOML", path.display()))?;
                self.stage_bytes_internal(&path, updated.into_bytes())
            }
            Backend::WholeFile { path } => {
                let bytes = if entry.id == "linux.hostname" {
                    format!("{}\n", value.trim()).into_bytes()
                } else {
                    value.as_bytes().to_vec()
                };
                self.stage_bytes_internal(&path, bytes)
            }
            Backend::TransformField {
                path,
                source_id,
                script,
                expected_script_digest,
                plan_entrypoint,
                binding,
                expected_digest,
                ..
            } => {
                let current = self.content_for(&path)?;
                let text = String::from_utf8(current)
                    .with_context(|| format!("decode {} as UTF-8", path.display()))?;
                if !self.files.contains_key(&path) && sha256_text(&text) != expected_digest {
                    bail!("transform source changed after discovery; reload before editing");
                }
                let current_script = read_regular_file(&script)
                    .with_context(|| format!("read transform {}", script.display()))?;
                if format!("sha256:{:x}", Sha256::digest(&current_script)) != expected_script_digest
                {
                    bail!("transform script changed after approval; reload and approve it again");
                }
                let mut edits = plan_transform_edit(
                    &script,
                    &expected_script_digest,
                    &plan_entrypoint,
                    &source_id,
                    &text,
                    &binding,
                    value,
                )?;
                validate_transform_edits(&text, &source_id, &mut edits)?;
                let mut updated = text;
                for edit in edits {
                    updated.replace_range(edit.start..edit.end, &edit.replacement);
                }
                self.stage_bytes_internal(&path, updated.into_bytes())
            }
            Backend::AdapterField { binding } => {
                if binding.precondition.is_none() || binding.verification.is_none() {
                    bail!("editable adapter operations require precondition and verification");
                }
                validate_adapter_binding(&binding, &entry.value, value)?;
                self.push_undo();
                self.adapters.insert(
                    entry.id.clone(),
                    StagedAdapter {
                        entry_id: entry.id.clone(),
                        old_value: entry.value.clone(),
                        new_value: value.to_owned(),
                        binding: *binding,
                    },
                );
                Ok(())
            }
            Backend::ReadOnly { reason } => bail!("read-only entry: {reason}"),
        };
        if result.is_ok() {
            if entry.privilege == crate::model::Privilege::System {
                if let Some(path) = entry.backend.path() {
                    let authorization = system_authorization(entry)?;
                    let staged = self
                        .files
                        .get_mut(path)
                        .ok_or_else(|| anyhow!("system entry did not stage its declared source"))?;
                    staged.authorization = Some(authorization);
                }
            }
            self.entry_values.insert(
                entry.id.clone(),
                StagedEntryValue {
                    value: value.to_owned(),
                    path: entry.backend.path().cloned(),
                },
            );
        }
        result
    }

    pub fn stage_raw(&mut self, path: &Path, bytes: Vec<u8>) -> Result<()> {
        self.stage_bytes(path, bytes)
    }

    pub fn working_copy(&self, path: &Path) -> Result<PathBuf> {
        let contents = self.content_for(path)?;
        make_working_copy(path, &contents)
    }

    pub fn discard_path(&mut self, path: &Path) -> bool {
        if self.files.contains_key(path) {
            self.push_undo();
            self.files.remove(path);
            self.entry_values
                .retain(|_, staged| staged.path.as_deref() != Some(path));
            true
        } else {
            false
        }
    }

    pub fn has_changes_for(&self, path: &Path) -> bool {
        self.files.get(path).is_some_and(StagedFile::is_changed)
    }

    pub fn has_changes_for_entry(&self, entry: &ConfigEntry) -> bool {
        self.adapters.contains_key(&entry.id)
            || entry
                .source_path()
                .is_some_and(|path| self.has_changes_for(path))
    }

    pub fn discard_entry(&mut self, entry: &ConfigEntry) -> bool {
        if let Some(path) = entry.source_path() {
            return self.discard_path(path);
        }
        if self.adapters.contains_key(&entry.id) {
            self.push_undo();
            self.adapters.remove(&entry.id);
            self.entry_values.remove(&entry.id);
            return true;
        }
        false
    }

    pub fn undo(&mut self) -> bool {
        if let Some(snapshot) = self.undo_stack.pop() {
            self.files = snapshot.files;
            self.adapters = snapshot.adapters;
            self.entry_values = snapshot.entry_values;
            true
        } else {
            false
        }
    }

    pub fn value_for_entry(&self, entry: &ConfigEntry) -> Result<String> {
        if let Some(staged) = self.entry_values.get(&entry.id) {
            return Ok(staged.value.clone());
        }
        let Some(path) = entry.backend.path() else {
            return Ok(entry.value.clone());
        };
        let bytes = self.content_for(path)?;
        let text = String::from_utf8(bytes)
            .with_context(|| format!("decode {} as UTF-8", path.display()))?;
        match &entry.backend {
            Backend::KeyValue { key, separator, .. } => {
                Ok(find_key_value(&text, key, separator).unwrap_or_else(|| "<unset>".to_owned()))
            }
            Backend::SchemaField { key, format, .. } => {
                Ok(crate::structured::find_value(&text, key, format)?
                    .unwrap_or_else(|| "<unset>".to_owned()))
            }
            Backend::TomlField { section, key, .. } => {
                Ok(find_toml_field(&text, section.as_deref(), key)
                    .unwrap_or_else(|| "<unset>".to_owned()))
            }
            Backend::WholeFile { .. } => {
                if entry.id == "linux.hostname" {
                    Ok(text.trim().to_owned())
                } else {
                    Ok(text)
                }
            }
            _ => Ok(entry.value.clone()),
        }
    }

    pub fn validate(&self) -> Vec<ValidationIssue> {
        let mut issues = Vec::new();
        for (path, file) in &self.files {
            if !file.is_changed() {
                continue;
            }
            if file.staged.contains(&0) {
                issues.push(ValidationIssue {
                    path: path.clone(),
                    message: "file contains a NUL byte".to_owned(),
                });
            }
            if std::str::from_utf8(&file.staged).is_err() {
                issues.push(ValidationIssue {
                    path: path.clone(),
                    message: "file is not valid UTF-8".to_owned(),
                });
                continue;
            }
            let text = std::str::from_utf8(&file.staged).expect("UTF-8 checked above");
            if is_reginux_config_path(path) {
                let result = toml::from_str::<crate::config::AppConfig>(text)
                    .context("parse Reginux TOML")
                    .and_then(|config| crate::config::validate_app_config(&config));
                if let Err(error) = result {
                    issues.push(ValidationIssue {
                        path: path.clone(),
                        message: format!("invalid Reginux configuration: {error}"),
                    });
                }
            }
            if path == Path::new("/etc/hostname") {
                let hostname = text.trim();
                let valid = !hostname.is_empty()
                    && !hostname.contains('\n')
                    && hostname.len() <= 253
                    && hostname.split('.').all(valid_hostname_label);
                if !valid {
                    issues.push(ValidationIssue {
                        path: path.clone(),
                        message: "hostname must contain valid DNS-style labels".to_owned(),
                    });
                }
            }
        }
        for adapter in self.adapters.values() {
            if adapter.new_value.contains('\0') || adapter.new_value.contains('\n') {
                issues.push(ValidationIssue {
                    path: PathBuf::from(format!("adapter:{}", adapter.entry_id)),
                    message: "adapter value must be a single line without NUL bytes".to_owned(),
                });
            }
        }
        issues
    }

    pub fn diff(&self) -> String {
        let mut output = String::new();
        for (path, file) in &self.files {
            if !file.is_changed() {
                continue;
            }
            output.push_str(&format!(
                "--- a/{}\n+++ b/{}\n",
                path.display(),
                path.display()
            ));
            output.push_str("@@\n");
            let before = String::from_utf8_lossy(&file.original);
            let after = String::from_utf8_lossy(&file.staged);
            output.push_str(&line_diff(&before, &after));
        }
        for adapter in self.adapters.values() {
            let guarantee = adapter.binding.guarantee.as_str();
            let scope = adapter.binding.scope.as_str();
            output.push_str(&format!(
                "--- adapter/{}/{}\n+++ adapter/{}/{}\n@@ {} ({guarantee}; {scope} scope) @@\n-{}\n+{}\n",
                adapter.binding.plugin_id,
                adapter.binding.operation_id,
                adapter.binding.plugin_id,
                adapter.binding.operation_id,
                adapter.entry_id,
                adapter.old_value,
                adapter.new_value
            ));
        }
        if output.is_empty() {
            "No staged changes.\n".to_owned()
        } else {
            output
        }
    }

    pub fn change_summaries(&self) -> Vec<ChangeSummary> {
        self.files
            .iter()
            .filter(|(_, file)| file.is_changed())
            .map(|(path, file)| {
                let (added_lines, removed_lines) = line_change_counts(
                    &String::from_utf8_lossy(&file.original),
                    &String::from_utf8_lossy(&file.staged),
                );
                ChangeSummary {
                    path: path.clone(),
                    added_lines,
                    removed_lines,
                    creates_file: !file.existed,
                }
            })
            .collect()
    }

    pub fn adapter_change_summaries(&self) -> Vec<AdapterChangeSummary> {
        self.adapters
            .values()
            .map(|adapter| AdapterChangeSummary {
                entry_id: adapter.entry_id.clone(),
                operation_id: adapter.binding.operation_id.clone(),
                transport: adapter.binding.invocation.transport_name().to_owned(),
                typed_arguments: invocation_preview(
                    &adapter.binding.invocation,
                    &adapter.old_value,
                    &adapter.new_value,
                ),
                guarantee: adapter.binding.guarantee.as_str().to_owned(),
                scope: adapter.binding.scope.as_str().to_owned(),
                has_precondition: adapter.binding.precondition.is_some(),
                has_validation: adapter.binding.validation.is_some(),
                has_verification: adapter.binding.verification.is_some(),
                has_compensation: adapter.binding.compensation.is_some(),
            })
            .collect()
    }

    pub fn apply(&mut self, backup: bool) -> Result<ApplyReport> {
        let issues = self.validate();
        if !issues.is_empty() {
            let message = issues
                .iter()
                .map(|issue| format!("{}: {}", issue.path.display(), issue.message))
                .collect::<Vec<_>>()
                .join("; ");
            bail!("validation failed: {message}");
        }
        let paths = self.changed_paths();
        if paths.is_empty() && self.adapters.is_empty() {
            return Ok(ApplyReport {
                transaction_id: String::new(),
                files: vec![],
                backups: vec![],
                adapter_operations: vec![],
            });
        }

        // Preflight every path before creating backups or changing anything.
        // This prevents stale staged data from overwriting an external edit.
        for path in &paths {
            let file = self
                .files
                .get(path)
                .ok_or_else(|| anyhow!("staged file disappeared"))?;
            verify_unchanged(path, file)?;
        }

        let transaction_id = Local::now().format("%Y%m%dT%H%M%S%.6f").to_string();
        let user_paths = paths
            .iter()
            .filter(|path| !crate::filesystem::is_system_path(path))
            .cloned()
            .collect::<Vec<_>>();
        let system_paths = paths
            .iter()
            .filter(|path| crate::filesystem::is_system_path(path))
            .cloned()
            .collect::<Vec<_>>();
        let mut backups = Vec::new();
        // All backups are prepared before the first source file is replaced.
        for path in &user_paths {
            let file = self
                .files
                .get(path)
                .ok_or_else(|| anyhow!("staged file disappeared"))?;
            if backup && file.existed {
                backups.push(write_backup(&transaction_id, path, &file.original)?);
            }
        }

        let mut applied = Vec::new();
        for path in &user_paths {
            let file = self
                .files
                .get(path)
                .ok_or_else(|| anyhow!("staged file disappeared"))?;
            // Re-check immediately before each replacement as well as during
            // the all-file preflight. This narrows the window in which an
            // external writer could otherwise be overwritten.
            let write_result = verify_unchanged(path, file).and_then(|()| {
                atomic_write_checked(
                    path,
                    file.existed.then_some(file.original.as_slice()),
                    &file.staged,
                    file.mode,
                )
            });
            if let Err(write_error) = write_result {
                let rollback_errors = rollback_applied(&applied, &self.files);
                if rollback_errors.is_empty() {
                    bail!(
                        "failed to write {}; restored {} previously written file(s): {write_error}",
                        path.display(),
                        applied.len()
                    );
                }
                bail!(
                    "failed to write {}; rollback was incomplete: {write_error}; {}",
                    path.display(),
                    rollback_errors.join("; ")
                );
            }
            applied.push(path.clone());
        }

        let mut system_applied = false;
        if !system_paths.is_empty() {
            let request =
                privileged_request(&transaction_id, backup, &system_paths, &self.files, false)?;
            match invoke_privileged_helper(&request) {
                Ok(response) => {
                    backups.extend(response.backups);
                    system_applied = true;
                }
                Err(error) => {
                    let rollback_errors = rollback_applied(&applied, &self.files);
                    if rollback_errors.is_empty() {
                        bail!("system transaction failed; user files were restored: {error}");
                    }
                    bail!(
                        "system transaction failed and user rollback was incomplete: {error}; {}",
                        rollback_errors.join("; ")
                    );
                }
            }
        }
        let mut applied_adapters = Vec::new();
        for adapter in self.adapters.values() {
            if let Err(error) =
                execute_adapter_binding(&adapter.binding, &adapter.old_value, &adapter.new_value)
            {
                let mut rollback_errors = rollback_adapters(&applied_adapters);
                if system_applied {
                    match privileged_request(
                        &format!("{transaction_id}.rollback"),
                        false,
                        &system_paths,
                        &self.files,
                        true,
                    )
                    .and_then(|request| invoke_privileged_helper(&request).map(|_| ()))
                    {
                        Ok(()) => {}
                        Err(error) => rollback_errors.push(format!("system files: {error}")),
                    }
                }
                rollback_errors.extend(rollback_applied(&applied, &self.files));
                if rollback_errors.is_empty() {
                    bail!(
                        "adapter operation {} failed; prior changes were restored: {error}",
                        adapter.entry_id
                    );
                }
                bail!(
                    "adapter operation {} failed and rollback was incomplete: {error}; {}",
                    adapter.entry_id,
                    rollback_errors.join("; ")
                );
            }
            applied_adapters.push(adapter.clone());
        }
        self.files.clear();
        self.adapters.clear();
        self.entry_values.clear();
        self.undo_stack.clear();
        Ok(ApplyReport {
            transaction_id,
            files: paths,
            backups,
            adapter_operations: applied_adapters
                .into_iter()
                .map(|adapter| adapter.entry_id)
                .collect(),
        })
    }

    fn push_undo(&mut self) {
        self.undo_stack.push(TransactionSnapshot {
            files: self.files.clone(),
            adapters: self.adapters.clone(),
            entry_values: self.entry_values.clone(),
        });
        if self.undo_stack.len() > 50 {
            self.undo_stack.remove(0);
        }
    }
}

fn invocation_preview(invocation: &AdapterInvocation, old_value: &str, new_value: &str) -> String {
    let text = match invocation {
        AdapterInvocation::Command(invocation) => format!(
            "{} {}",
            invocation.program.display(),
            invocation
                .args
                .iter()
                .map(|argument| argument
                    .replace("${old_value}", old_value)
                    .replace("${value}", new_value))
                .collect::<Vec<_>>()
                .join(" ")
        ),
        AdapterInvocation::DBus(invocation) => format!(
            "{} {}({})",
            invocation.interface,
            invocation.member,
            invocation
                .arg_types
                .iter()
                .zip(&invocation.args)
                .map(|(kind, value)| format!(
                    "{}={}",
                    kind.as_str(),
                    value
                        .replace("${old_value}", old_value)
                        .replace("${value}", new_value)
                ))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        AdapterInvocation::Socket(invocation) => format!(
            "{} {} {:?}",
            invocation.endpoint.display(),
            invocation.framing.as_str(),
            invocation
                .request
                .replace("${old_value}", old_value)
                .replace("${value}", new_value)
        ),
    };
    text.chars()
        .filter(|ch| !ch.is_control() && *ch != '\u{1b}')
        .take(512)
        .collect()
}

fn validate_transform_edits(
    text: &str,
    source_id: &str,
    edits: &mut [TransformEdit],
) -> Result<()> {
    let digest = sha256_text(text);
    for edit in edits.iter() {
        if edit.source_id != source_id {
            bail!(
                "transform edit references undeclared source {}",
                edit.source_id
            );
        }
        if edit.expected_sha256 != digest {
            bail!("transform edit digest does not match the current source");
        }
        if edit.start > edit.end
            || edit.end > text.len()
            || !text.is_char_boundary(edit.start)
            || !text.is_char_boundary(edit.end)
        {
            bail!("transform edit has an invalid UTF-8 range");
        }
        if edit.replacement.contains('\0') {
            bail!("transform replacement contains a NUL byte");
        }
        if edit.replacement.len() > 1024 * 1024 {
            bail!("transform replacement exceeds the 1 MiB limit");
        }
    }
    edits.sort_by_key(|edit| std::cmp::Reverse(edit.start));
    for pair in edits.windows(2) {
        if pair[1].end > pair[0].start {
            bail!("transform returned overlapping edits");
        }
    }
    Ok(())
}

fn sha256_text(text: &str) -> String {
    format!("sha256:{:x}", Sha256::digest(text.as_bytes()))
}

fn rollback_adapters(applied: &[StagedAdapter]) -> Vec<String> {
    let mut errors = Vec::new();
    for adapter in applied.iter().rev() {
        if let Err(error) =
            compensate_adapter_binding(&adapter.binding, &adapter.old_value, &adapter.new_value)
        {
            errors.push(format!("{}: {error}", adapter.entry_id));
        }
    }
    errors
}

fn is_reginux_config_path(path: &Path) -> bool {
    path.file_name().and_then(|name| name.to_str()) == Some("config.toml")
        && path
            .parent()
            .and_then(Path::file_name)
            .and_then(|name| name.to_str())
            == Some("reginux")
}

fn valid_hostname_label(label: &str) -> bool {
    !label.is_empty()
        && label.len() <= 63
        && !label.starts_with('-')
        && !label.ends_with('-')
        && label
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '-')
}

impl StagedFile {
    fn is_changed(&self) -> bool {
        !self.existed || self.original != self.staged
    }
}

fn capture_original(path: &Path) -> Result<StagedFile> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() {
                bail!(
                    "{} is a symbolic link; edit its resolved target explicitly",
                    path.display()
                );
            }
            if !metadata.is_file() {
                bail!("{} is not a regular file", path.display());
            }
            let limit = if crate::filesystem::is_system_path(path) {
                HELPER_FILE_LIMIT
            } else {
                USER_STAGED_FILE_LIMIT
            };
            Ok(StagedFile {
                original: read_regular_file_limited(path, limit as u64)?,
                staged: Vec::new(),
                mode: file_mode(path),
                existed: true,
                authorization: None,
            })
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(StagedFile {
            original: Vec::new(),
            staged: Vec::new(),
            mode: None,
            existed: false,
            authorization: None,
        }),
        Err(error) => Err(error).with_context(|| format!("inspect {}", path.display())),
    }
}

fn verify_unchanged(path: &Path, staged: &StagedFile) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if !staged.existed {
                bail!(
                    "source conflict: {} was created after staging; reload before applying",
                    path.display()
                );
            }
            if metadata.file_type().is_symlink() || !metadata.is_file() {
                bail!(
                    "source conflict: {} is no longer the same regular file",
                    path.display()
                );
            }
            let limit = if crate::filesystem::is_system_path(path) {
                HELPER_FILE_LIMIT
            } else {
                USER_STAGED_FILE_LIMIT
            };
            let current = read_regular_file_limited(path, limit as u64)?;
            if current != staged.original {
                bail!(
                    "source conflict: {} changed on disk after staging; reload and review the new diff",
                    path.display()
                );
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            if staged.existed {
                bail!(
                    "source conflict: {} was removed after staging; reload before applying",
                    path.display()
                );
            }
        }
        Err(error) => return Err(error).with_context(|| format!("inspect {}", path.display())),
    }
    Ok(())
}

fn rollback_applied(applied: &[PathBuf], files: &BTreeMap<PathBuf, StagedFile>) -> Vec<String> {
    let mut errors = Vec::new();
    for path in applied.iter().rev() {
        let Some(file) = files.get(path) else {
            errors.push(format!("{}: staged state missing", path.display()));
            continue;
        };
        let result = if file.existed {
            atomic_write_checked(path, Some(&file.staged), &file.original, file.mode)
        } else {
            remove_regular_file_checked(path, &file.staged)
                .with_context(|| format!("remove new file {}", path.display()))
        };
        if let Err(error) = result {
            errors.push(format!("{}: {error}", path.display()));
        }
    }
    errors
}

fn system_authorization(entry: &ConfigEntry) -> Result<SystemAuthorization> {
    let metadata = entry.metadata.iter().cloned().collect::<BTreeMap<_, _>>();
    match (
        metadata.get("system_plugin_id"),
        metadata.get("system_plugin_manifest"),
        metadata.get("system_plugin_digest"),
    ) {
        (Some(plugin_id), Some(manifest), Some(digest)) => Ok(SystemAuthorization::Plugin {
            plugin_id: plugin_id.clone(),
            manifest_path: PathBuf::from(manifest),
            plugin_digest: digest.clone(),
        }),
        (None, None, None) if !entry.provider.starts_with("plugin.") => {
            Ok(SystemAuthorization::Builtin)
        }
        _ => bail!(
            "system entry {} has no complete privileged authorization metadata",
            entry.id
        ),
    }
}

fn privileged_request(
    transaction_id: &str,
    backup: bool,
    paths: &[PathBuf],
    files: &BTreeMap<PathBuf, StagedFile>,
    reverse: bool,
) -> Result<PrivilegedRequest> {
    let edits = paths
        .iter()
        .map(|path| {
            let file = files
                .get(path)
                .ok_or_else(|| anyhow!("staged system file disappeared"))?;
            let authorization = file.authorization.clone().ok_or_else(|| {
                anyhow!(
                    "{} was staged without an authorized system entry; raw system writes are forbidden",
                    path.display()
                )
            })?;
            let (expected, replacement) = if reverse {
                (&file.staged, &file.original)
            } else {
                (&file.original, &file.staged)
            };
            Ok(PrivilegedFileEdit::new(
                path.clone(),
                expected,
                replacement,
                file.mode,
                authorization,
                reverse && !file.existed,
            ))
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(PrivilegedRequest {
        protocol: HELPER_PROTOCOL_VERSION,
        transaction_id: transaction_id.to_owned(),
        backup,
        files: edits,
    })
}

fn validate_entry_value(entry: &ConfigEntry, value: &str) -> Result<()> {
    if value.contains('\0') {
        bail!("value contains a NUL byte");
    }
    if value.contains('\n') && entry.value_type != ValueType::Raw {
        bail!("structured values must be a single line");
    }
    if entry.id == "linux.hostname" && value.trim().is_empty() {
        bail!("hostname must not be empty");
    }
    match &entry.value_type {
        ValueType::Boolean => {
            if !matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "true" | "false" | "yes" | "no" | "on" | "off" | "1" | "0"
            ) {
                bail!("expected a boolean");
            }
        }
        ValueType::Integer => {
            value.trim().parse::<i64>().context("expected an integer")?;
        }
        ValueType::Float => {
            value.trim().parse::<f64>().context("expected a number")?;
        }
        _ => {}
    }

    for (key, constraint) in &entry.metadata {
        if key == "min" || key == "max" {
            if let Ok(number) = value.trim().parse::<f64>() {
                let limit = constraint.parse::<f64>().unwrap_or(number);
                if key == "min" && number < limit {
                    bail!("value is below minimum {limit}");
                }
                if key == "max" && number > limit {
                    bail!("value is above maximum {limit}");
                }
            }
        }
        if key == "values" {
            let accepted = constraint.split('|').collect::<Vec<_>>();
            if !accepted.iter().any(|candidate| candidate == &value) {
                bail!("value must be one of {}", accepted.join(", "));
            }
        }
    }
    Ok(())
}

fn find_key_value(text: &str, key: &str, separator: &KeyValueSeparator) -> Option<String> {
    text.lines()
        .filter_map(|line| parse_key_value_line(line, key, separator))
        .next_back()
}

fn replace_key_value(text: &str, key: &str, value: &str, separator: KeyValueSeparator) -> String {
    let target = text
        .lines()
        .enumerate()
        .filter(|(_, line)| key_value_line_matches(line, key, &separator))
        .map(|(index, _)| index)
        .last();
    let mut output = Vec::new();
    for (index, line) in text.lines().enumerate() {
        if Some(index) == target {
            output.push(rewrite_key_value_line(line, key, value, &separator));
        } else {
            output.push(line.to_owned());
        }
    }
    if target.is_none() {
        if !text.is_empty() && !text.ends_with('\n') {
            output.push(String::new());
        }
        output.push(match separator {
            KeyValueSeparator::Equals => format!("{key}={value}"),
            KeyValueSeparator::Whitespace => format!("{key} {value}"),
        });
    }
    let mut result = output.join("\n");
    if text.ends_with('\n') || !result.ends_with('\n') {
        result.push('\n');
    }
    result
}

fn remove_key_value(text: &str, key: &str, separator: &KeyValueSeparator) -> String {
    let mut result = text
        .lines()
        .filter(|line| !key_value_line_matches(line, key, separator))
        .collect::<Vec<_>>()
        .join("\n");
    if text.ends_with('\n') && !result.is_empty() {
        result.push('\n');
    }
    result
}

fn key_value_line_matches(line: &str, key: &str, separator: &KeyValueSeparator) -> bool {
    parse_key_value_line(line, key, separator).is_some()
}

fn parse_key_value_line(line: &str, key: &str, separator: &KeyValueSeparator) -> Option<String> {
    let trimmed = line.trim();
    if trimmed.is_empty() || trimmed.starts_with('#') {
        return None;
    }
    let raw_value = match separator {
        KeyValueSeparator::Equals => {
            let (candidate, value) = trimmed.split_once('=')?;
            (candidate.trim() == key).then_some(value)?
        }
        KeyValueSeparator::Whitespace => {
            let key_end = trimmed.find(char::is_whitespace)?;
            (trimmed[..key_end] == *key).then_some(trimmed[key_end..].trim_start())?
        }
    };
    let (value, _) = split_value_and_comment(raw_value);
    Some(unquote(value.trim()).to_owned())
}

fn rewrite_key_value_line(
    line: &str,
    key: &str,
    value: &str,
    separator: &KeyValueSeparator,
) -> String {
    let (prefix, raw_value) = match separator {
        KeyValueSeparator::Equals => {
            let separator_index = line.find('=').unwrap_or(line.len());
            (
                &line[..separator_index.saturating_add(1)],
                &line[separator_index.saturating_add(1)..],
            )
        }
        KeyValueSeparator::Whitespace => {
            let key_start = line.len() - line.trim_start().len();
            let after_key = key_start + key.len();
            let spacing = line[after_key..]
                .chars()
                .take_while(|ch| ch.is_whitespace())
                .map(char::len_utf8)
                .sum::<usize>();
            (&line[..after_key + spacing], &line[after_key + spacing..])
        }
    };
    let leading_len = raw_value.len() - raw_value.trim_start().len();
    let leading = &raw_value[..leading_len];
    let (old_value, suffix) = split_value_and_comment(&raw_value[leading_len..]);
    let encoded = if old_value.trim().starts_with('"') && old_value.trim().ends_with('"') {
        format!("\"{}\"", escape_quoted(value))
    } else {
        value.to_owned()
    };
    format!("{prefix}{leading}{encoded}{suffix}")
}

fn split_value_and_comment(value: &str) -> (&str, &str) {
    let mut quote = None;
    let mut escaped = false;
    for (index, ch) in value.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        if ch == '\\' && quote.is_some() {
            escaped = true;
            continue;
        }
        if matches!(ch, '\'' | '"') {
            if quote == Some(ch) {
                quote = None;
            } else if quote.is_none() {
                quote = Some(ch);
            }
            continue;
        }
        if ch == '#'
            && quote.is_none()
            && (index == 0 || value[..index].ends_with(char::is_whitespace))
        {
            let value_end = value[..index].trim_end().len();
            return (&value[..value_end], &value[value_end..]);
        }
    }
    (value.trim_end(), &value[value.trim_end().len()..])
}

fn unquote(value: &str) -> &str {
    if value.len() >= 2
        && ((value.starts_with('"') && value.ends_with('"'))
            || (value.starts_with('\'') && value.ends_with('\'')))
    {
        &value[1..value.len() - 1]
    } else {
        value
    }
}

fn escape_quoted(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

fn normalize_boolean(value: &str) -> Result<String> {
    match value.trim().to_ascii_lowercase().as_str() {
        "true" | "yes" | "on" | "1" => Ok("true".to_owned()),
        "false" | "no" | "off" | "0" => Ok("false".to_owned()),
        _ => bail!("expected a boolean"),
    }
}

fn find_toml_field(text: &str, section: Option<&str>, key: &str) -> Option<String> {
    let mut current_section = None;
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            current_section = Some(trimmed[1..trimmed.len() - 1].trim());
            continue;
        }
        if current_section != section {
            continue;
        }
        let Some((candidate, value)) = trimmed.split_once('=') else {
            continue;
        };
        if candidate.trim() == key {
            return Some(value.trim().trim_matches('"').to_owned());
        }
    }
    None
}

fn replace_toml_field(
    text: &str,
    section: Option<&str>,
    key: &str,
    value: &str,
    value_type: &ValueType,
) -> String {
    let encoded = match value_type {
        ValueType::Boolean => value.trim().to_ascii_lowercase(),
        ValueType::Integer | ValueType::Float => value.trim().to_owned(),
        _ => format!("\"{}\"", escape_quoted(value)),
    };
    let mut current_section = None;
    let mut found = false;
    let mut output = Vec::new();
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            current_section = Some(trimmed[1..trimmed.len() - 1].trim().to_owned());
        }
        let matches = current_section.as_deref() == section
            && !trimmed.starts_with('#')
            && trimmed
                .split_once('=')
                .map(|(candidate, _)| candidate.trim() == key)
                .unwrap_or(false);
        if matches {
            found = true;
            let indent = line.len() - line.trim_start().len();
            output.push(format!("{}{} = {}", &line[..indent], key, encoded));
        } else {
            output.push(line.to_owned());
        }
    }
    if !found {
        if let Some(section) = section {
            let section_header = format!("[{section}]");
            if let Some(section_index) =
                output.iter().position(|line| line.trim() == section_header)
            {
                let insert_at = output
                    .iter()
                    .enumerate()
                    .skip(section_index + 1)
                    .find(|(_, line)| {
                        let trimmed = line.trim();
                        trimmed.starts_with('[') && trimmed.ends_with(']')
                    })
                    .map(|(index, _)| index)
                    .unwrap_or(output.len());
                output.insert(insert_at, format!("{key} = {encoded}"));
            } else {
                if !output.is_empty() && !output.last().is_some_and(String::is_empty) {
                    output.push(String::new());
                }
                output.push(format!("[{section}]"));
                output.push(format!("{key} = {encoded}"));
            }
        } else {
            let insert_at = output
                .iter()
                .position(|line| {
                    let trimmed = line.trim();
                    trimmed.starts_with('[') && trimmed.ends_with(']')
                })
                .unwrap_or(output.len());
            output.insert(insert_at, format!("{key} = {encoded}"));
        }
    }
    let mut result = output.join("\n");
    if !result.ends_with('\n') {
        result.push('\n');
    }
    result
}

fn line_diff(before: &str, after: &str) -> String {
    let before = before.lines().collect::<Vec<_>>();
    let after = after.lines().collect::<Vec<_>>();
    let mut output = String::new();
    let count = before.len().max(after.len());
    for index in 0..count {
        match (before.get(index), after.get(index)) {
            (Some(left), Some(right)) if left == right => {
                output.push_str(&format!(" {left}\n"));
            }
            (Some(left), Some(right)) => {
                output.push_str(&format!("-{left}\n+{right}\n"));
            }
            (Some(left), None) => output.push_str(&format!("-{left}\n")),
            (None, Some(right)) => output.push_str(&format!("+{right}\n")),
            (None, None) => {}
        }
    }
    output
}

fn line_change_counts(before: &str, after: &str) -> (usize, usize) {
    let before = before.lines().collect::<Vec<_>>();
    let after = after.lines().collect::<Vec<_>>();
    let count = before.len().max(after.len());
    let mut added = 0;
    let mut removed = 0;
    for index in 0..count {
        match (before.get(index), after.get(index)) {
            (Some(left), Some(right)) if left == right => {}
            (Some(_), Some(_)) => {
                added += 1;
                removed += 1;
            }
            (Some(_), None) => removed += 1,
            (None, Some(_)) => added += 1,
            (None, None) => {}
        }
    }
    (added, removed)
}
