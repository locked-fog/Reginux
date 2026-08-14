use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::io::{Read, Write};
use std::net::Shutdown;
use std::os::fd::AsRawFd;
use std::os::unix::fs::FileTypeExt;
use std::os::unix::net::UnixStream;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use chrono::Utc;
use glob::glob;
use mlua::{HookTriggers, Lua, LuaOptions, LuaSerdeExt, StdLib, Value as LuaValue, VmState};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};

use crate::filesystem::{home_dir, is_system_path, read_regular_file_limited};
use crate::model::{
    AdapterBinding, AdapterInvocation, AdapterVerification, Backend, BusKind, CommandInvocation,
    ConfigEntry, DBusInvocation, EditCapability, NetworkAccess, Privilege, SocketFraming,
    SocketInvocation, SourceRef, SourceScope, TransactionGuarantee, ValueType,
};
use crate::sandbox::SandboxRequest;

const MANIFEST_LIMIT: u64 = 256 * 1024;
const SOURCE_LIMIT_DEFAULT: u64 = 1024 * 1024;
const SOURCE_TOTAL_LIMIT: usize = 8 * 1024 * 1024;
const COMMAND_OUTPUT_LIMIT: usize = 1024 * 1024;
const COMMAND_TIMEOUT_DEFAULT_MS: u64 = 3_000;
const LUA_MEMORY_LIMIT: usize = 8 * 1024 * 1024;
const LUA_INSTRUCTION_LIMIT: u64 = 2_000_000;
const MAX_TEXT_FIELD: usize = 512;
const MAX_PLUGIN_CANDIDATES: usize = 128;
const PLUGIN_DISCOVERY_CONCURRENCY: usize = 8;

#[derive(Clone, Debug, Default)]
pub struct PluginPolicy {
    pub approved_adapters: BTreeMap<String, String>,
    pub approved_transforms: BTreeMap<String, String>,
    /// Development escape hatch. Every ID remains explicit; there is no global allow-all.
    pub temporary_approvals: HashSet<String>,
    pub refresh_runtime: bool,
}

#[derive(Clone, Debug)]
pub struct PluginSummary {
    pub id: String,
    pub name: String,
    pub kind: String,
    pub path: PathBuf,
    pub status: String,
    pub permissions: Vec<String>,
    pub trust: String,
    pub approval: String,
    pub digest: Option<String>,
    pub sources: Vec<String>,
    pub capabilities: Vec<String>,
    pub captured_at: Option<String>,
    pub last_error: Option<String>,
    pub last_error_at: Option<String>,
    pub stale: bool,
}

pub struct PluginDiscovery {
    pub entries: Vec<ConfigEntry>,
    pub summaries: Vec<PluginSummary>,
    pub diagnostics: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum PluginKind {
    Schema,
    Adapter,
    Transform,
}

impl PluginKind {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Schema => "schema",
            Self::Adapter => "adapter",
            Self::Transform => "transform",
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    schema_version: u32,
    plugin: PluginSpec,
    #[serde(default)]
    sources: BTreeMap<String, SourceSpec>,
    #[serde(default)]
    fields: BTreeMap<String, BTreeMap<String, FieldSpec>>,
    #[serde(default)]
    nodes: Vec<NodeSpec>,
    transport: Option<TransportSpec>,
    #[serde(default)]
    operations: BTreeMap<String, OperationSpec>,
    transform: Option<TransformSpec>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PluginSpec {
    id: String,
    name: String,
    version: String,
    kind: PluginKind,
    #[serde(default)]
    description: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SourceSpec {
    path: String,
    format: String,
    #[serde(default)]
    scope: ScopeSpec,
    #[serde(default = "default_source_limit")]
    max_bytes: u64,
    imports: Option<ImportSpec>,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum ScopeSpec {
    #[default]
    User,
    System,
}

impl From<&ScopeSpec> for SourceScope {
    fn from(value: &ScopeSpec) -> Self {
        match value {
            ScopeSpec::User => Self::User,
            ScopeSpec::System => Self::System,
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ImportSpec {
    keyword: String,
    #[serde(default = "default_import_syntax")]
    syntax: String,
    #[serde(default = "default_relative_to")]
    relative_to: String,
    #[serde(default)]
    glob: bool,
    #[serde(default = "default_true")]
    recursive: bool,
    #[serde(default = "default_import_depth")]
    max_depth: usize,
    #[serde(default = "default_import_files")]
    max_files: usize,
    allowed_roots: Vec<String>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FieldSpec {
    #[serde(default)]
    label: Option<String>,
    #[serde(rename = "type")]
    value_type: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    default: Option<toml::Value>,
    #[serde(default)]
    source: Option<String>,
    #[serde(default)]
    key: Option<String>,
    #[serde(default)]
    binding: Option<String>,
    #[serde(default)]
    min: Option<f64>,
    #[serde(default)]
    max: Option<f64>,
    #[serde(default)]
    values: Vec<String>,
    #[serde(default)]
    sensitive: bool,
    #[serde(default = "default_write_target")]
    write_target: String,
    #[serde(default)]
    explicit_source: Option<String>,
    #[serde(default)]
    insert: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct TransportSpec {
    kind: String,
    program: Option<String>,
    #[serde(default)]
    bus: Option<String>,
    #[serde(default)]
    service: Option<String>,
    #[serde(default)]
    object_path: Option<String>,
    #[serde(default)]
    interface: Option<String>,
    #[serde(default)]
    endpoint: Option<String>,
    #[serde(default)]
    peer: Option<String>,
    #[serde(default)]
    read_paths: Vec<String>,
    #[serde(default)]
    network: NetworkSpec,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
enum NetworkSpec {
    #[default]
    None,
    Local,
    Internet,
}

impl From<&NetworkSpec> for NetworkAccess {
    fn from(value: &NetworkSpec) -> Self {
        match value {
            NetworkSpec::None => Self::None,
            NetworkSpec::Local => Self::Local,
            NetworkSpec::Internet => Self::Internet,
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct OperationSpec {
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    decoder: Option<String>,
    #[serde(default)]
    decoder_config: DecoderConfig,
    #[serde(default = "default_command_timeout")]
    timeout_ms: u64,
    #[serde(default)]
    guarantee: GuaranteeSpec,
    #[serde(default)]
    scope: ScopeSpec,
    #[serde(default)]
    compensation: Option<String>,
    #[serde(default)]
    precondition: Option<String>,
    #[serde(default)]
    validate: Option<String>,
    #[serde(default)]
    verify: Option<String>,
    #[serde(default)]
    expected_stdout: Option<String>,
    #[serde(default)]
    member: Option<String>,
    #[serde(default)]
    arg_types: Vec<String>,
    #[serde(default)]
    reply_type: Option<String>,
    #[serde(default)]
    request: Option<String>,
    #[serde(default)]
    framing: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DecoderConfig {
    #[serde(default)]
    delimiter: Option<String>,
    #[serde(default)]
    columns: Vec<String>,
    #[serde(default)]
    collection: Option<String>,
    #[serde(default)]
    pattern: Option<String>,
    #[serde(default = "default_true")]
    headers: bool,
}

impl Default for DecoderConfig {
    fn default() -> Self {
        Self {
            delimiter: None,
            columns: Vec::new(),
            collection: None,
            pattern: None,
            headers: true,
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
enum GuaranteeSpec {
    Atomic,
    Compensatable,
    #[default]
    BestEffort,
    Irreversible,
}

impl From<&GuaranteeSpec> for TransactionGuarantee {
    fn from(value: &GuaranteeSpec) -> Self {
        match value {
            GuaranteeSpec::Atomic => Self::Atomic,
            GuaranteeSpec::Compensatable => Self::Compensatable,
            GuaranteeSpec::BestEffort => Self::BestEffort,
            GuaranteeSpec::Irreversible => Self::Irreversible,
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct NodeSpec {
    id: String,
    kind: String,
    #[serde(default)]
    label: Option<String>,
    #[serde(default)]
    parent: Option<String>,
    #[serde(default)]
    collection: Option<String>,
    #[serde(default)]
    key: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    fields: Vec<NodeFieldSpec>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct NodeFieldSpec {
    id: String,
    label: String,
    #[serde(rename = "type")]
    value_type: String,
    value: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    operation: Option<String>,
    #[serde(default)]
    binding: Option<String>,
    #[serde(default)]
    read_only: bool,
    #[serde(default)]
    sensitive: bool,
    #[serde(default)]
    values: Vec<String>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct TransformSpec {
    script: String,
    #[serde(default = "default_decode_entrypoint")]
    decode_entrypoint: String,
    #[serde(default = "default_plan_entrypoint")]
    plan_entrypoint: String,
}

#[derive(Clone, Debug)]
struct ResolvedDocument {
    source_id: String,
    path: PathBuf,
    parent: Option<PathBuf>,
    contents: String,
    format: String,
    scope: SourceScope,
}

#[derive(Debug, Deserialize)]
struct TransformDocument {
    #[serde(default)]
    bindings: BTreeMap<String, TransformBinding>,
    #[serde(default)]
    records: Vec<TransformRecord>,
    #[serde(default)]
    diagnostics: Vec<String>,
}

#[derive(Clone, Debug, Deserialize)]
struct TransformBinding {
    value: Value,
    source_id: String,
    range: TextRange,
}

#[derive(Debug, Deserialize)]
struct TransformRecord {
    kind: String,
    key: String,
    #[serde(default)]
    properties: Map<String, Value>,
    #[serde(default)]
    bindings: BTreeMap<String, TransformBinding>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct TextRange {
    pub start: usize,
    pub end: usize,
}

#[derive(Clone, Debug, Deserialize)]
pub struct TransformEdit {
    pub source_id: String,
    pub expected_sha256: String,
    pub start: usize,
    pub end: usize,
    pub replacement: String,
}

#[derive(Debug, Deserialize)]
struct TransformPlan {
    edits: Vec<TransformEdit>,
}

pub fn discover_plugins(directories: &[String], policy: &PluginPolicy) -> PluginDiscovery {
    let mut entries: Vec<ConfigEntry> = Vec::new();
    let mut summaries: Vec<PluginSummary> = Vec::new();
    let mut diagnostics = Vec::new();
    let mut candidates = Vec::new();

    for configured in directories {
        let root = expand_configured_root(configured);
        if !root.exists() {
            continue;
        }
        match secure_plugin_directories(&root) {
            Ok(mut directories) => candidates.append(&mut directories),
            Err(error) => diagnostics.push(format!("plugin root {}: {error}", root.display())),
        }
    }
    candidates.sort();
    candidates.dedup();
    if candidates.len() > MAX_PLUGIN_CANDIDATES {
        diagnostics.push(format!(
            "plugin candidate limit reached; only the first {MAX_PLUGIN_CANDIDATES} directories were inspected"
        ));
        candidates.truncate(MAX_PLUGIN_CANDIDATES);
    }

    let mut seen_ids: HashMap<String, PathBuf> = HashMap::new();
    let mut conflicted_ids = HashSet::new();
    for chunk in candidates.chunks(PLUGIN_DISCOVERY_CONCURRENCY) {
        let loaded = thread::scope(|scope| {
            let handles = chunk
                .iter()
                .map(|directory| {
                    scope.spawn(move || {
                        let manifest = find_manifest(directory)
                            .with_context(|| format!("inspect {}", directory.display()))?;
                        Ok::<_, anyhow::Error>(manifest.map(|manifest_path| {
                            let result = load_plugin(directory, &manifest_path, policy);
                            (directory.clone(), manifest_path, result)
                        }))
                    })
                })
                .collect::<Vec<_>>();
            handles
                .into_iter()
                .map(|handle| handle.join())
                .collect::<Vec<_>>()
        });
        for candidate in loaded {
            let candidate = match candidate {
                Ok(Ok(candidate)) => candidate,
                Ok(Err(error)) => {
                    diagnostics.push(format!("plugin discovery: {error}"));
                    continue;
                }
                Err(_) => {
                    diagnostics.push("plugin discovery worker panicked".to_owned());
                    continue;
                }
            };
            let Some((directory, manifest_path, result)) = candidate else {
                continue;
            };
            match result {
                Ok((manifest_id, mut plugin_entries, summary)) => {
                    if let Some(first) = seen_ids.get(&manifest_id) {
                        diagnostics.push(format!(
                            "duplicate plugin id {manifest_id}: {} conflicts with {}; all copies were disabled",
                            directory.display(),
                            first.display()
                        ));
                        conflicted_ids.insert(manifest_id.clone());
                        entries.retain(|entry| entry.provider != format!("plugin.{manifest_id}"));
                        if let Some(previous) =
                            summaries.iter_mut().find(|plugin| plugin.id == manifest_id)
                        {
                            previous.status = "disabled; duplicate plugin id".to_owned();
                        }
                        summaries.push(PluginSummary {
                            status: format!("disabled; duplicate of {}", first.display()),
                            ..summary
                        });
                    } else {
                        seen_ids.insert(manifest_id, directory.clone());
                        if !conflicted_ids.contains(&summary.id) {
                            entries.append(&mut plugin_entries);
                        }
                        summaries.push(summary);
                    }
                }
                Err(error) => summaries.push(error_summary(&manifest_path, error)),
            }
        }
    }

    PluginDiscovery {
        entries,
        summaries,
        diagnostics,
    }
}

fn load_plugin(
    directory: &Path,
    manifest_path: &Path,
    policy: &PluginPolicy,
) -> Result<(String, Vec<ConfigEntry>, PluginSummary)> {
    let bytes = read_limited_regular_file(manifest_path, MANIFEST_LIMIT)?;
    let text = std::str::from_utf8(&bytes).context("manifest is not UTF-8")?;
    let manifest: Manifest = toml::from_str(text).context("parse plugin manifest TOML")?;
    validate_manifest(&manifest)?;
    let digest = plugin_digest(directory, &manifest, &bytes)?;
    let is_system_install = directory.starts_with("/usr/share/reginux/plugins");
    if is_system_install
        || manifest
            .sources
            .values()
            .any(|source| source.scope == ScopeSpec::System)
    {
        validate_system_plugin_location(directory, manifest_path)?;
    }
    let approved = is_approved(&manifest, &digest, policy);

    let (mut entries, status, permissions, sources, capabilities, approval) =
        match manifest.plugin.kind {
            PluginKind::Schema => {
                let entries = load_schema(directory, &manifest)?;
                (
                    entries,
                    format!("loaded ({})", manifest.plugin.version),
                    vec!["declarative; no code execution".to_owned()],
                    source_descriptions(&manifest.sources),
                    vec!["read".to_owned(), "file-plan".to_owned()],
                    "not required".to_owned(),
                )
            }
            PluginKind::Adapter if !approved => (
                Vec::new(),
                "disabled; adapter approval required".to_owned(),
                adapter_permissions(&manifest),
                source_descriptions(&manifest.sources),
                adapter_capabilities(&manifest),
                "required".to_owned(),
            ),
            PluginKind::Adapter => (
                load_adapter(directory, &manifest, policy.refresh_runtime)?,
                format!("loaded ({})", manifest.plugin.version),
                adapter_permissions(&manifest),
                source_descriptions(&manifest.sources),
                adapter_capabilities(&manifest),
                "approved".to_owned(),
            ),
            PluginKind::Transform if !approved => (
                Vec::new(),
                "disabled; transform approval required".to_owned(),
                vec!["sandboxed Lua transform".to_owned()],
                source_descriptions(&manifest.sources),
                vec!["decode".to_owned(), "file-plan".to_owned()],
                "required".to_owned(),
            ),
            PluginKind::Transform => {
                let (entries, transform_diagnostics) = load_transform(directory, &manifest)?;
                let status = if transform_diagnostics.is_empty() {
                    format!("loaded ({})", manifest.plugin.version)
                } else {
                    format!(
                        "loaded ({}) with diagnostics: {}",
                        manifest.plugin.version,
                        transform_diagnostics.join("; ")
                    )
                };
                (
                    entries,
                    status,
                    vec![
                        "sandboxed Lua; no I/O, process, network, package or debug libraries"
                            .to_owned(),
                    ],
                    source_descriptions(&manifest.sources),
                    vec!["decode".to_owned(), "file-plan".to_owned()],
                    "approved".to_owned(),
                )
            }
        };

    for entry in &mut entries {
        if entry.source.is_system() {
            entry.metadata.push((
                "system_plugin_manifest".to_owned(),
                manifest_path.display().to_string(),
            ));
            entry
                .metadata
                .push(("system_plugin_digest".to_owned(), digest.clone()));
            entry
                .metadata
                .push(("system_plugin_id".to_owned(), manifest.plugin.id.clone()));
        }
    }

    let id = manifest.plugin.id.clone();
    Ok((
        id.clone(),
        entries,
        PluginSummary {
            id,
            name: clean_text(&manifest.plugin.name),
            kind: manifest.plugin.kind.as_str().to_owned(),
            path: directory.to_path_buf(),
            status,
            permissions,
            trust: plugin_trust(directory, &manifest.plugin.kind),
            approval,
            digest: Some(digest),
            sources,
            capabilities,
            captured_at: (manifest.plugin.kind == PluginKind::Adapter && approved)
                .then(|| Utc::now().to_rfc3339()),
            last_error: None,
            last_error_at: None,
            stale: false,
        },
    ))
}

/// Revalidate a system Schema plugin at the privileged boundary and confirm
/// that `target` is one of the files reached from its declared source graph.
/// User-installed manifests are deliberately never trusted for system writes.
pub fn authorize_system_schema_target(
    manifest_path: &Path,
    expected_plugin_id: &str,
    expected_digest: &str,
    target: &Path,
) -> Result<()> {
    let system_root = Path::new("/usr/share/reginux/plugins");
    if !manifest_path.starts_with(system_root) {
        bail!("system-writing plugin manifests must be installed under /usr/share/reginux/plugins");
    }
    let directory = manifest_path
        .parent()
        .ok_or_else(|| anyhow!("plugin manifest has no parent directory"))?;
    validate_system_plugin_location(directory, manifest_path)?;
    let bytes = read_limited_regular_file(manifest_path, MANIFEST_LIMIT)?;
    let text = std::str::from_utf8(&bytes).context("manifest is not UTF-8")?;
    let manifest: Manifest = toml::from_str(text).context("parse plugin manifest TOML")?;
    validate_manifest(&manifest)?;
    if manifest.plugin.kind != PluginKind::Schema || manifest.plugin.id != expected_plugin_id {
        bail!("privileged plugin identity or kind does not match the staged plan");
    }
    let digest = plugin_digest(directory, &manifest, &bytes)?;
    if digest != expected_digest {
        bail!("system plugin changed after discovery; privileged write refused");
    }
    let documents = resolve_documents(&manifest.sources)?;
    let declared = documents
        .iter()
        .any(|document| document.scope == SourceScope::System && document.path.as_path() == target);
    if !declared {
        bail!("target is not a declared system source of the approved plugin");
    }
    Ok(())
}

fn load_schema(_directory: &Path, manifest: &Manifest) -> Result<Vec<ConfigEntry>> {
    if manifest.sources.is_empty() || manifest.fields.is_empty() {
        bail!("schema plugin requires sources and fields");
    }
    let documents = resolve_documents(&manifest.sources)?;
    let roots = root_documents(&documents);
    let mut entries = Vec::new();

    for (section_id, fields) in &manifest.fields {
        validate_identifier(section_id, "section id")?;
        for (field_id, field) in fields {
            validate_identifier(field_id, "field id")?;
            let source_id = field
                .source
                .as_deref()
                .ok_or_else(|| anyhow!("field {section_id}.{field_id} has no source"))?;
            let key = field
                .key
                .as_deref()
                .ok_or_else(|| anyhow!("field {section_id}.{field_id} has no key"))?;
            let root = roots
                .get(source_id)
                .ok_or_else(|| anyhow!("field references unknown source {source_id}"))?;
            let value_type = parse_value_type(&field.value_type)?;
            let sensitive = field.sensitive || value_type == ValueType::Secret;
            let source_spec = manifest
                .sources
                .get(source_id)
                .ok_or_else(|| anyhow!("field references unknown source {source_id}"))?;
            let found = find_effective_schema_value(source_id, source_spec, root, &documents, key)?;
            let target = select_schema_target(
                field,
                root,
                found.as_ref().map(|(document, _)| *document),
                &roots,
            )?;
            if found.is_none() && field.insert.is_none() {
                bail!(
                    "field {section_id}.{field_id} is absent and must declare insert=\"end\" or insert=\"section\""
                );
            }
            let value = found
                .as_ref()
                .map(|(_, value)| value.clone())
                .or_else(|| field.default.as_ref().map(toml_value_to_string))
                .unwrap_or_else(|| "<unset>".to_owned());
            let mut metadata = field_metadata(field);
            metadata.push(("plugin_type".to_owned(), "schema".to_owned()));
            metadata.push(("source_id".to_owned(), source_id.to_owned()));
            metadata.push(("format".to_owned(), target.format.clone()));
            if target.parent.is_some() {
                metadata.push(("imported".to_owned(), "true".to_owned()));
            }
            if sensitive && !field.sensitive {
                metadata.push(("sensitive".to_owned(), "true".to_owned()));
            }
            let scope = target.scope.clone();
            let privilege = privilege_for_scope(&scope);
            let (edit_capability, privilege, backend) = if sensitive {
                (
                    EditCapability::None,
                    Privilege::ReadOnly,
                    Backend::ReadOnly {
                        reason: "sensitive fields are masked and read-only".to_owned(),
                    },
                )
            } else {
                (
                    EditCapability::File,
                    privilege,
                    Backend::SchemaField {
                        path: target.path.clone(),
                        source_id: source_id.to_owned(),
                        key: key.to_owned(),
                        format: target.format.clone(),
                        plugin_id: manifest.plugin.id.clone(),
                        insert: field.insert.clone(),
                    },
                )
            };
            entries.push(ConfigEntry {
                id: format!("{}.{}.{}", manifest.plugin.id, section_id, field_id),
                label: clean_text(field.label.as_deref().unwrap_or(field_id)),
                section: format!(
                    "Applications / {} / {}",
                    clean_text(&manifest.plugin.name),
                    clean_text(section_id)
                ),
                description: clean_text(
                    field
                        .description
                        .as_deref()
                        .or(manifest.plugin.description.as_deref())
                        .unwrap_or("Declarative configuration field."),
                ),
                value: display_value(&value, sensitive),
                default_value: field.default.as_ref().map(toml_value_to_string),
                value_type: if sensitive {
                    ValueType::Secret
                } else {
                    value_type.clone()
                },
                source: SourceRef::File {
                    source_id: source_id.to_owned(),
                    path: target.path.clone(),
                    scope,
                    imported_from: target.parent.clone(),
                },
                edit_capability,
                privilege,
                provider: format!("plugin.{}", manifest.plugin.id),
                validation: field_validation(field, &value_type),
                backend,
                metadata,
            });
        }
    }
    Ok(entries)
}

#[derive(Clone, Debug)]
enum LoadedTransport {
    Command {
        program: PathBuf,
        digest: String,
        read_paths: Vec<PathBuf>,
        network: NetworkAccess,
    },
    DBus {
        bus: BusKind,
        service: String,
        object_path: String,
        interface: String,
    },
    Socket {
        endpoint: PathBuf,
        peer_uid: u32,
    },
}

fn load_transport(transport: &TransportSpec) -> Result<LoadedTransport> {
    match transport.kind.as_str() {
        "command" => {
            let program = absolute_program(transport)?;
            let digest = sha256_file(&program, 64 * 1024 * 1024)?;
            let read_paths = transport
                .read_paths
                .iter()
                .map(|path| expand_source_path(path))
                .collect::<Result<Vec<_>>>()?;
            for path in &read_paths {
                reject_symlink_components(path)?;
                if !path.exists() {
                    bail!(
                        "declared command read_path does not exist: {}",
                        path.display()
                    );
                }
            }
            Ok(LoadedTransport::Command {
                program,
                digest,
                read_paths,
                network: (&transport.network).into(),
            })
        }
        "dbus" => {
            if transport.program.is_some()
                || transport.endpoint.is_some()
                || !transport.read_paths.is_empty()
                || !matches!(transport.network, NetworkSpec::None)
            {
                bail!("D-Bus transport cannot declare command/socket permissions");
            }
            let bus = match transport.bus.as_deref() {
                Some("session") => BusKind::Session,
                Some("system") => BusKind::System,
                Some(other) => bail!("unsupported D-Bus bus {other}"),
                None => bail!("D-Bus transport requires bus"),
            };
            let service = transport
                .service
                .clone()
                .ok_or_else(|| anyhow!("D-Bus transport requires service"))?;
            let object_path = transport
                .object_path
                .clone()
                .ok_or_else(|| anyhow!("D-Bus transport requires object_path"))?;
            let interface = transport
                .interface
                .clone()
                .ok_or_else(|| anyhow!("D-Bus transport requires interface"))?;
            // zbus performs the complete protocol validation; constructing names
            // here makes malformed manifests fail before approval/execution.
            zbus::names::BusName::try_from(service.as_str()).context("invalid D-Bus service")?;
            zbus::zvariant::ObjectPath::try_from(object_path.as_str())
                .context("invalid D-Bus object_path")?;
            zbus::names::InterfaceName::try_from(interface.as_str())
                .context("invalid D-Bus interface")?;
            Ok(LoadedTransport::DBus {
                bus,
                service,
                object_path,
                interface,
            })
        }
        "unix_socket" => {
            if transport.program.is_some()
                || transport.bus.is_some()
                || transport.service.is_some()
                || transport.object_path.is_some()
                || transport.interface.is_some()
                || !transport.read_paths.is_empty()
                || !matches!(transport.network, NetworkSpec::None)
            {
                bail!("Unix socket transport has incompatible fields");
            }
            let endpoint = expand_source_path(
                transport
                    .endpoint
                    .as_deref()
                    .ok_or_else(|| anyhow!("Unix socket transport requires endpoint"))?,
            )?;
            reject_symlink_components(&endpoint)?;
            let metadata = fs::symlink_metadata(&endpoint)
                .with_context(|| format!("inspect Unix socket {}", endpoint.display()))?;
            if !metadata.file_type().is_socket() {
                bail!("declared endpoint is not a Unix socket");
            }
            let peer = transport
                .peer
                .as_deref()
                .ok_or_else(|| anyhow!("Unix socket transport requires peer=self|root|UID"))?;
            let peer_uid = match peer {
                "self" => unsafe { libc::geteuid() },
                "root" => 0,
                value => value
                    .parse::<u32>()
                    .with_context(|| format!("invalid Unix socket peer UID {value:?}"))?,
            };
            Ok(LoadedTransport::Socket { endpoint, peer_uid })
        }
        other => bail!("unsupported adapter transport {other}"),
    }
}

fn source_ref_for_transport(
    plugin_id: &str,
    operation_id: &str,
    transport: &LoadedTransport,
) -> SourceRef {
    match transport {
        LoadedTransport::Command { program, .. } => SourceRef::Command {
            plugin_id: plugin_id.to_owned(),
            operation_id: operation_id.to_owned(),
            program: program.clone(),
        },
        LoadedTransport::DBus {
            bus,
            service,
            object_path,
            interface,
        } => SourceRef::DBus {
            plugin_id: plugin_id.to_owned(),
            bus: bus.as_str().to_owned(),
            service: service.clone(),
            object_path: object_path.clone(),
            interface: interface.clone(),
        },
        LoadedTransport::Socket { endpoint, .. } => SourceRef::Socket {
            plugin_id: plugin_id.to_owned(),
            endpoint_id: endpoint.display().to_string(),
        },
    }
}

fn invocation_has_placeholder(invocation: &AdapterInvocation, placeholder: &str) -> bool {
    match invocation {
        AdapterInvocation::Command(invocation) => invocation
            .args
            .iter()
            .any(|argument| argument.contains(placeholder)),
        AdapterInvocation::DBus(invocation) => invocation
            .args
            .iter()
            .any(|argument| argument.contains(placeholder)),
        AdapterInvocation::Socket(invocation) => invocation.request.contains(placeholder),
    }
}

fn load_adapter(
    directory: &Path,
    manifest: &Manifest,
    refresh_runtime: bool,
) -> Result<Vec<ConfigEntry>> {
    let transport = manifest
        .transport
        .as_ref()
        .ok_or_else(|| anyhow!("adapter requires a transport"))?;
    let loaded_transport = load_transport(transport)?;
    let (snapshot_id, snapshot) = if refresh_runtime {
        manifest
            .operations
            .get("refresh")
            .map(|operation| ("refresh", operation))
            .unwrap_or((
                "snapshot",
                manifest
                    .operations
                    .get("snapshot")
                    .ok_or_else(|| anyhow!("adapter requires operations.snapshot"))?,
            ))
    } else {
        (
            "snapshot",
            manifest
                .operations
                .get("snapshot")
                .ok_or_else(|| anyhow!("adapter requires operations.snapshot"))?,
        )
    };
    let invocation = operation_invocation(&loaded_transport, snapshot, &Value::Null)?;
    let output = invoke_adapter(&invocation)?;
    let lua_decoder = if snapshot.decoder.as_deref() == Some("lua") {
        let transform = manifest
            .transform
            .as_ref()
            .ok_or_else(|| anyhow!("adapter Lua decoder requires [transform]"))?;
        let script = secure_relative_plugin_file(directory, &transform.script, MANIFEST_LIMIT)?;
        let digest = sha256_file(&script, MANIFEST_LIMIT)?;
        Some((script, digest, transform.decode_entrypoint.as_str()))
    } else {
        None
    };
    let snapshot_data = decode_output(
        snapshot.decoder.as_deref().unwrap_or("single"),
        &snapshot.decoder_config,
        &output,
        lua_decoder
            .as_ref()
            .map(|(script, digest, entrypoint)| (script.as_path(), digest.as_str(), *entrypoint)),
    )?;
    project_adapter_entries(
        manifest,
        &loaded_transport,
        snapshot_id,
        &snapshot_data,
        &output,
        &Utc::now().to_rfc3339(),
    )
}

fn project_adapter_entries(
    manifest: &Manifest,
    transport: &LoadedTransport,
    snapshot_operation_id: &str,
    snapshot: &Value,
    raw_response: &[u8],
    captured_at: &str,
) -> Result<Vec<ConfigEntry>> {
    let mut result = Vec::new();
    for node in &manifest.nodes {
        validate_identifier(&node.id, "node id")?;
        if node.kind == "group" {
            continue;
        }
        if node.kind != "resource" {
            bail!("node {} has unsupported kind {}", node.id, node.kind);
        }
        let collection_path = node
            .collection
            .as_deref()
            .ok_or_else(|| anyhow!("resource node {} has no collection", node.id))?;
        let records = snapshot
            .pointer(collection_path)
            .and_then(Value::as_array)
            .ok_or_else(|| anyhow!("{} is not an array in adapter snapshot", collection_path))?;
        for record in records {
            let key_pointer = node
                .key
                .as_deref()
                .ok_or_else(|| anyhow!("resource node {} has no stable key", node.id))?;
            let resource_key = pointer_string(record, key_pointer)?;
            validate_resource_key(&resource_key)?;
            let resource_label = node
                .label
                .as_deref()
                .map(|pointer| pointer_string(record, pointer))
                .transpose()?
                .unwrap_or_else(|| resource_key.clone());
            for field in &node.fields {
                validate_identifier(&field.id, "field id")?;
                let raw_value = record.pointer(&field.value).ok_or_else(|| {
                    anyhow!(
                        "resource field {} has no value at {}",
                        field.id,
                        field.value
                    )
                })?;
                let value = json_value_to_string(raw_value);
                let value_type = parse_value_type(&field.value_type)?;
                let sensitive = field.sensitive || value_type == ValueType::Secret;
                let operation = field
                    .operation
                    .as_deref()
                    .map(|id| adapter_binding(manifest, transport, id, record))
                    .transpose()?;
                if field.read_only && operation.is_some() {
                    bail!(
                        "field {} is both read_only and mapped to an operation",
                        field.id
                    );
                }
                let source =
                    source_ref_for_transport(&manifest.plugin.id, snapshot_operation_id, transport);
                let (edit_capability, privilege, backend) = if sensitive {
                    (
                        EditCapability::None,
                        Privilege::ReadOnly,
                        Backend::ReadOnly {
                            reason: "sensitive fields are masked and read-only".to_owned(),
                        },
                    )
                } else if let Some(binding) = operation {
                    if binding.guarantee == TransactionGuarantee::Irreversible {
                        (
                            EditCapability::None,
                            Privilege::ReadOnly,
                            Backend::ReadOnly {
                                reason: "irreversible adapter operations are not editable"
                                    .to_owned(),
                            },
                        )
                    } else {
                        let privilege = privilege_for_scope(&binding.scope);
                        (
                            EditCapability::Adapter,
                            privilege,
                            Backend::AdapterField {
                                binding: Box::new(binding),
                            },
                        )
                    }
                } else {
                    (
                        EditCapability::None,
                        Privilege::ReadOnly,
                        Backend::ReadOnly {
                            reason: "runtime observation".to_owned(),
                        },
                    )
                };
                let mut metadata = vec![
                    ("plugin_type".to_owned(), "adapter".to_owned()),
                    ("resource_key".to_owned(), resource_key.clone()),
                    ("runtime_state".to_owned(), "true".to_owned()),
                    ("captured_at".to_owned(), captured_at.to_owned()),
                ];
                if let Some(raw_response) = adapter_raw_response_metadata(manifest, raw_response) {
                    metadata.push(("raw_response".to_owned(), raw_response));
                }
                if !field.values.is_empty() {
                    metadata.push(("values".to_owned(), field.values.join("|")));
                }
                if sensitive {
                    metadata.push(("sensitive".to_owned(), "true".to_owned()));
                }
                result.push(ConfigEntry {
                    id: format!(
                        "{}.{}.{}.{}",
                        manifest.plugin.id, node.id, resource_key, field.id
                    ),
                    label: clean_text(&field.label),
                    section: format!(
                        "Applications / {} / {} / {}",
                        clean_text(&manifest.plugin.name),
                        clean_text(node.parent.as_deref().unwrap_or(&node.id)),
                        clean_text(&resource_label)
                    ),
                    description: clean_text(
                        field
                            .description
                            .as_deref()
                            .or(node.description.as_deref())
                            .unwrap_or("Runtime state supplied by a declared adapter."),
                    ),
                    value: display_value(&value, sensitive),
                    default_value: None,
                    value_type: if sensitive {
                        ValueType::Secret
                    } else {
                        value_type.clone()
                    },
                    source,
                    edit_capability,
                    privilege,
                    provider: format!("plugin.{}", manifest.plugin.id),
                    validation: if field.values.is_empty() {
                        value_type.as_str().to_owned()
                    } else {
                        format!("enum; values={}", field.values.join(","))
                    },
                    backend,
                    metadata,
                });
            }
        }
    }
    Ok(result)
}

fn adapter_raw_response_metadata(manifest: &Manifest, raw_response: &[u8]) -> Option<String> {
    let has_sensitive_field = manifest.nodes.iter().any(|node| {
        node.fields
            .iter()
            .any(|field| field.sensitive || field.value_type == "secret")
    });
    (!has_sensitive_field).then(|| clean_text(&String::from_utf8_lossy(raw_response)))
}

fn adapter_binding(
    manifest: &Manifest,
    transport: &LoadedTransport,
    operation_id: &str,
    record: &Value,
) -> Result<AdapterBinding> {
    let operation = manifest
        .operations
        .get(operation_id)
        .ok_or_else(|| anyhow!("field references unknown operation {operation_id}"))?;
    let precondition = operation
        .precondition
        .as_deref()
        .map(|id| {
            let spec = manifest
                .operations
                .get(id)
                .ok_or_else(|| anyhow!("unknown precondition operation {id}"))?;
            Ok::<AdapterVerification, anyhow::Error>(AdapterVerification {
                invocation: operation_invocation(transport, spec, record)?,
                expected_stdout: spec
                    .expected_stdout
                    .clone()
                    .unwrap_or_else(|| "${old_value}".to_owned()),
            })
        })
        .transpose()?;
    if precondition.is_none() {
        bail!("editable adapter operation {operation_id} requires a precondition operation");
    }
    let validation = operation
        .validate
        .as_deref()
        .map(|id| {
            let spec = manifest
                .operations
                .get(id)
                .ok_or_else(|| anyhow!("unknown validation operation {id}"))?;
            Ok::<AdapterVerification, anyhow::Error>(AdapterVerification {
                invocation: operation_invocation(transport, spec, record)?,
                expected_stdout: spec
                    .expected_stdout
                    .clone()
                    .unwrap_or_else(|| "ok".to_owned()),
            })
        })
        .transpose()?;
    let invocation = operation_invocation(transport, operation, record)?;
    if !invocation_has_placeholder(&invocation, "${value}") {
        bail!("edit operation {operation_id} has no typed ${{value}} placeholder");
    }
    let compensation = operation
        .compensation
        .as_deref()
        .map(|id| {
            let spec = manifest
                .operations
                .get(id)
                .ok_or_else(|| anyhow!("unknown compensation operation {id}"))?;
            operation_invocation(transport, spec, record)
        })
        .transpose()?;
    let verification = operation
        .verify
        .as_deref()
        .map(|id| {
            let spec = manifest
                .operations
                .get(id)
                .ok_or_else(|| anyhow!("unknown verification operation {id}"))?;
            Ok::<AdapterVerification, anyhow::Error>(AdapterVerification {
                invocation: operation_invocation(transport, spec, record)?,
                expected_stdout: operation
                    .expected_stdout
                    .clone()
                    .unwrap_or_else(|| "${value}".to_owned()),
            })
        })
        .transpose()?;
    Ok(AdapterBinding {
        plugin_id: manifest.plugin.id.clone(),
        operation_id: operation_id.to_owned(),
        invocation,
        precondition,
        validation,
        compensation,
        verification,
        guarantee: (&operation.guarantee).into(),
        scope: (&operation.scope).into(),
    })
}

fn load_transform(
    directory: &Path,
    manifest: &Manifest,
) -> Result<(Vec<ConfigEntry>, Vec<String>)> {
    let transform = manifest
        .transform
        .as_ref()
        .ok_or_else(|| anyhow!("transform plugin requires [transform]"))?;
    let script = secure_relative_plugin_file(directory, &transform.script, MANIFEST_LIMIT)?;
    let script_digest = sha256_file(&script, MANIFEST_LIMIT)?;
    let documents = resolve_documents(&manifest.sources)?;
    let source_map = documents
        .iter()
        .filter(|document| document.parent.is_none())
        .map(|document| (document.source_id.clone(), document.contents.clone()))
        .collect::<BTreeMap<_, _>>();
    let output = run_lua_function(
        &script,
        Some(&script_digest),
        &transform.decode_entrypoint,
        &json!({ "sources": source_map }),
    )?;
    let decoded: TransformDocument =
        serde_json::from_value(output).context("decode transform Document Model")?;
    let transform_diagnostics = decoded
        .diagnostics
        .iter()
        .map(|diagnostic| clean_text(diagnostic))
        .collect::<Vec<_>>();
    let documents_by_id = documents
        .iter()
        .filter(|document| document.parent.is_none())
        .map(|document| (document.source_id.clone(), document))
        .collect::<HashMap<_, _>>();
    let mut entries = Vec::new();
    for (section_id, fields) in &manifest.fields {
        for (field_id, field) in fields {
            let binding_id = field
                .binding
                .as_deref()
                .ok_or_else(|| anyhow!("transform field {section_id}.{field_id} has no binding"))?;
            let value_type = parse_value_type(&field.value_type)?;
            let Some(binding) = decoded.bindings.get(binding_id) else {
                let source_id = field.source.as_deref().ok_or_else(|| {
                    anyhow!("unresolved transform field {section_id}.{field_id} has no source")
                })?;
                let document = documents_by_id.get(source_id).ok_or_else(|| {
                    anyhow!("transform field references unknown source {source_id}")
                })?;
                let mut metadata = field_metadata(field);
                metadata.push(("plugin_type".to_owned(), "transform".to_owned()));
                metadata.push(("binding".to_owned(), binding_id.to_owned()));
                metadata.push(("unresolved".to_owned(), "true".to_owned()));
                entries.push(ConfigEntry {
                    id: format!("{}.{}.{}", manifest.plugin.id, section_id, field_id),
                    label: clean_text(field.label.as_deref().unwrap_or(field_id)),
                    section: format!(
                        "Applications / {} / {}",
                        clean_text(&manifest.plugin.name),
                        clean_text(section_id)
                    ),
                    description: clean_text(
                        field
                            .description
                            .as_deref()
                            .unwrap_or("Dynamic expression could not be safely resolved."),
                    ),
                    value: "<dynamic/unavailable>".to_owned(),
                    default_value: field.default.as_ref().map(toml_value_to_string),
                    value_type,
                    source: SourceRef::file(
                        source_id.to_owned(),
                        document.path.clone(),
                        document.scope.clone(),
                    ),
                    edit_capability: EditCapability::None,
                    privilege: Privilege::ReadOnly,
                    provider: format!("plugin.{}", manifest.plugin.id),
                    validation: "unresolved dynamic expression".to_owned(),
                    backend: Backend::ReadOnly {
                        reason: "transform did not return a safe static binding".to_owned(),
                    },
                    metadata,
                });
                continue;
            };
            let document = documents_by_id
                .get(&binding.source_id)
                .ok_or_else(|| anyhow!("binding {binding_id} references unknown source"))?;
            validate_range(&document.contents, binding.range.start, binding.range.end)?;
            let value = json_value_to_string(&binding.value);
            let sensitive = field.sensitive || value_type == ValueType::Secret;
            let mut metadata = field_metadata(field);
            metadata.push(("plugin_type".to_owned(), "transform".to_owned()));
            metadata.push(("binding".to_owned(), binding_id.to_owned()));
            if sensitive && !field.sensitive {
                metadata.push(("sensitive".to_owned(), "true".to_owned()));
            }
            let (edit_capability, privilege, backend) = if sensitive {
                (
                    EditCapability::None,
                    Privilege::ReadOnly,
                    Backend::ReadOnly {
                        reason: "sensitive fields are masked and read-only".to_owned(),
                    },
                )
            } else {
                (
                    EditCapability::Transform,
                    privilege_for_scope(&document.scope),
                    Backend::TransformField {
                        path: document.path.clone(),
                        source_id: binding.source_id.clone(),
                        plugin_id: manifest.plugin.id.clone(),
                        script: script.clone(),
                        expected_script_digest: script_digest.clone(),
                        plan_entrypoint: transform.plan_entrypoint.clone(),
                        binding: binding_id.to_owned(),
                        start: binding.range.start,
                        end: binding.range.end,
                        expected_digest: sha256_bytes(document.contents.as_bytes()),
                    },
                )
            };
            entries.push(ConfigEntry {
                id: format!("{}.{}.{}", manifest.plugin.id, section_id, field_id),
                label: clean_text(field.label.as_deref().unwrap_or(field_id)),
                section: format!(
                    "Applications / {} / {}",
                    clean_text(&manifest.plugin.name),
                    clean_text(section_id)
                ),
                description: clean_text(
                    field
                        .description
                        .as_deref()
                        .or(manifest.plugin.description.as_deref())
                        .unwrap_or("Field decoded by a sandboxed Lua transform."),
                ),
                value: display_value(&value, sensitive),
                default_value: field.default.as_ref().map(toml_value_to_string),
                value_type: if sensitive {
                    ValueType::Secret
                } else {
                    value_type.clone()
                },
                source: SourceRef::file(
                    binding.source_id.clone(),
                    document.path.clone(),
                    document.scope.clone(),
                ),
                edit_capability,
                privilege,
                provider: format!("plugin.{}", manifest.plugin.id),
                validation: field_validation(field, &value_type),
                backend,
                metadata,
            });
        }
    }
    for record in decoded.records {
        let node = manifest
            .nodes
            .iter()
            .find(|node| node.id == record.kind && node.kind == "resource")
            .ok_or_else(|| anyhow!("transform returned undeclared record kind {}", record.kind))?;
        validate_resource_key(&record.key)?;
        let properties = Value::Object(record.properties);
        let resource_label = node
            .label
            .as_deref()
            .map(|pointer| pointer_string(&properties, pointer))
            .transpose()?
            .unwrap_or_else(|| record.key.clone());
        for field in &node.fields {
            if field.operation.is_some() {
                bail!("transform node fields use binding, not adapter operation");
            }
            let value = properties
                .pointer(&field.value)
                .ok_or_else(|| anyhow!("transform record field {} has no value", field.id))?;
            let value_type = parse_value_type(&field.value_type)?;
            let sensitive = field.sensitive || value_type == ValueType::Secret;
            let binding = field
                .binding
                .as_deref()
                .and_then(|binding| record.bindings.get(binding));
            let (source, edit_capability, privilege, backend) = if let Some(binding) = binding {
                let document = documents_by_id
                    .get(&binding.source_id)
                    .ok_or_else(|| anyhow!("record binding references unknown source"))?;
                validate_range(&document.contents, binding.range.start, binding.range.end)?;
                let source = SourceRef::file(
                    binding.source_id.clone(),
                    document.path.clone(),
                    document.scope.clone(),
                );
                if field.read_only || sensitive {
                    (
                        source,
                        EditCapability::None,
                        Privilege::ReadOnly,
                        Backend::ReadOnly {
                            reason: if sensitive {
                                "sensitive fields are masked and read-only".to_owned()
                            } else {
                                "transform record is declared read-only".to_owned()
                            },
                        },
                    )
                } else {
                    (
                        source,
                        EditCapability::Transform,
                        privilege_for_scope(&document.scope),
                        Backend::TransformField {
                            path: document.path.clone(),
                            source_id: binding.source_id.clone(),
                            plugin_id: manifest.plugin.id.clone(),
                            script: script.clone(),
                            expected_script_digest: script_digest.clone(),
                            plan_entrypoint: transform.plan_entrypoint.clone(),
                            binding: format!("{}.{}.{}", node.id, record.key, field.id),
                            start: binding.range.start,
                            end: binding.range.end,
                            expected_digest: sha256_bytes(document.contents.as_bytes()),
                        },
                    )
                }
            } else {
                let source_id = manifest
                    .sources
                    .keys()
                    .next()
                    .ok_or_else(|| anyhow!("transform record has no declared source"))?;
                let document = documents_by_id
                    .get(source_id)
                    .ok_or_else(|| anyhow!("transform source is unavailable"))?;
                (
                    SourceRef::file(
                        source_id.clone(),
                        document.path.clone(),
                        document.scope.clone(),
                    ),
                    EditCapability::None,
                    Privilege::ReadOnly,
                    Backend::ReadOnly {
                        reason: "transform record has no editable binding".to_owned(),
                    },
                )
            };
            let mut metadata = vec![
                ("plugin_type".to_owned(), "transform".to_owned()),
                ("resource_key".to_owned(), record.key.clone()),
                ("record_kind".to_owned(), record.kind.clone()),
            ];
            if !field.values.is_empty() {
                metadata.push(("values".to_owned(), field.values.join("|")));
            }
            if sensitive {
                metadata.push(("sensitive".to_owned(), "true".to_owned()));
            }
            entries.push(ConfigEntry {
                id: format!(
                    "{}.{}.{}.{}",
                    manifest.plugin.id, node.id, record.key, field.id
                ),
                label: clean_text(&field.label),
                section: format!(
                    "Applications / {} / {} / {}",
                    clean_text(&manifest.plugin.name),
                    clean_text(node.parent.as_deref().unwrap_or(&node.id)),
                    clean_text(&resource_label)
                ),
                description: clean_text(
                    field
                        .description
                        .as_deref()
                        .or(node.description.as_deref())
                        .unwrap_or("Record decoded by a sandboxed Lua transform."),
                ),
                value: display_value(&json_value_to_string(value), sensitive),
                default_value: None,
                value_type: if sensitive {
                    ValueType::Secret
                } else {
                    value_type.clone()
                },
                source,
                edit_capability,
                privilege,
                provider: format!("plugin.{}", manifest.plugin.id),
                validation: if field.values.is_empty() {
                    value_type.as_str().to_owned()
                } else {
                    format!("enum; values={}", field.values.join(","))
                },
                backend,
                metadata,
            });
        }
    }
    Ok((entries, transform_diagnostics))
}

pub fn plan_transform_edit(
    script: &Path,
    expected_script_digest: &str,
    entrypoint: &str,
    source_id: &str,
    source_text: &str,
    binding: &str,
    value: &str,
) -> Result<Vec<TransformEdit>> {
    let expected_sha256 = sha256_bytes(source_text.as_bytes());
    let output = run_lua_function(
        script,
        Some(expected_script_digest),
        entrypoint,
        &json!({
            "binding": binding,
            "value": value,
            "expected_sha256": expected_sha256,
            "sources": { source_id: source_text }
        }),
    )?;
    let plan: TransformPlan =
        serde_json::from_value(output).context("decode transform edit plan")?;
    if plan.edits.is_empty() {
        bail!("transform returned an empty edit plan");
    }
    Ok(plan.edits)
}

pub fn execute_adapter_binding(
    binding: &AdapterBinding,
    old_value: &str,
    new_value: &str,
) -> Result<()> {
    if let Some(precondition) = &binding.precondition {
        verify_adapter_observation(precondition, old_value, new_value).context(
            "adapter precondition failed; runtime state changed, refresh before applying",
        )?;
    }
    let invocation = substitute_invocation(&binding.invocation, old_value, new_value);
    if let Err(invocation_error) = invoke_adapter(&invocation) {
        if binding.compensation.is_some() {
            return match compensate_adapter_binding(binding, old_value, new_value) {
                Ok(()) => Err(anyhow!(
                    "adapter operation {} failed during invocation and was compensated: {}",
                    binding.operation_id,
                    invocation_error
                )),
                Err(compensation_error) => Err(anyhow!(
                    "adapter operation {} failed during invocation and compensation failed: {}; {}",
                    binding.operation_id,
                    invocation_error,
                    compensation_error
                )),
            };
        }
        return Err(invocation_error).with_context(|| {
            format!(
                "adapter operation {} failed during invocation",
                binding.operation_id
            )
        });
    }
    if let Some(verification) = &binding.verification {
        let verification_result = verify_adapter_observation(verification, old_value, new_value);
        if let Err(verification_error) = verification_result {
            if binding.compensation.is_some() {
                return match compensate_adapter_binding(binding, old_value, new_value) {
                    Ok(()) => Err(anyhow!(
                        "adapter operation {} failed verification and was compensated: {}",
                        binding.operation_id,
                        verification_error
                    )),
                    Err(compensation_error) => Err(anyhow!(
                        "adapter operation {} failed verification and compensation failed: {}; {}",
                        binding.operation_id,
                        verification_error,
                        compensation_error
                    )),
                };
            }
            bail!(
                "adapter operation {} executed but verification failed: {}",
                binding.operation_id,
                verification_error
            );
        }
    }
    Ok(())
}

pub fn validate_adapter_binding(
    binding: &AdapterBinding,
    old_value: &str,
    new_value: &str,
) -> Result<()> {
    if let Some(validation) = &binding.validation {
        verify_adapter_observation(validation, old_value, new_value)
            .context("adapter rejected the staged value")?;
    }
    Ok(())
}

fn verify_adapter_observation(
    verification: &AdapterVerification,
    old_value: &str,
    new_value: &str,
) -> Result<()> {
    let invocation = substitute_invocation(&verification.invocation, old_value, new_value);
    let stdout = invoke_adapter(&invocation)?;
    let actual = std::str::from_utf8(&stdout)
        .context("adapter verification output is not UTF-8")?
        .trim();
    let expected =
        substitute_value_placeholders(&verification.expected_stdout, old_value, new_value);
    if actual != expected {
        bail!(
            "expected {:?}, got {:?}",
            clean_text(&expected),
            clean_text(actual)
        );
    }
    Ok(())
}

pub fn compensate_adapter_binding(
    binding: &AdapterBinding,
    old_value: &str,
    new_value: &str,
) -> Result<()> {
    let compensation = binding
        .compensation
        .as_ref()
        .ok_or_else(|| anyhow!("adapter operation has no compensation"))?;
    let invocation = substitute_invocation(compensation, old_value, new_value);
    let _ = invoke_adapter(&invocation)?;
    Ok(())
}

fn substitute_invocation(
    invocation: &AdapterInvocation,
    old_value: &str,
    new_value: &str,
) -> AdapterInvocation {
    match invocation {
        AdapterInvocation::Command(invocation) => AdapterInvocation::Command(CommandInvocation {
            program: invocation.program.clone(),
            expected_digest: invocation.expected_digest.clone(),
            args: invocation
                .args
                .iter()
                .map(|argument| substitute_value_placeholders(argument, old_value, new_value))
                .collect(),
            timeout_ms: invocation.timeout_ms,
            read_paths: invocation.read_paths.clone(),
            network: invocation.network.clone(),
        }),
        AdapterInvocation::DBus(invocation) => AdapterInvocation::DBus(DBusInvocation {
            bus: invocation.bus.clone(),
            service: invocation.service.clone(),
            object_path: invocation.object_path.clone(),
            interface: invocation.interface.clone(),
            member: invocation.member.clone(),
            args: invocation
                .args
                .iter()
                .map(|argument| substitute_value_placeholders(argument, old_value, new_value))
                .collect(),
            arg_types: invocation.arg_types.clone(),
            reply_type: invocation.reply_type.clone(),
            timeout_ms: invocation.timeout_ms,
        }),
        AdapterInvocation::Socket(invocation) => AdapterInvocation::Socket(SocketInvocation {
            endpoint: invocation.endpoint.clone(),
            request: substitute_value_placeholders(&invocation.request, old_value, new_value),
            framing: invocation.framing.clone(),
            expected_peer_uid: invocation.expected_peer_uid,
            timeout_ms: invocation.timeout_ms,
        }),
    }
}

fn substitute_value_placeholders(template: &str, old_value: &str, new_value: &str) -> String {
    template
        .replace("${old_value}", old_value)
        .replace("${value}", new_value)
}

fn run_lua_function(
    script: &Path,
    expected_digest: Option<&str>,
    entrypoint: &str,
    input: &Value,
) -> Result<Value> {
    validate_identifier(entrypoint, "Lua entrypoint")?;
    let script_text = String::from_utf8(read_limited_regular_file(script, MANIFEST_LIMIT)?)
        .context("transform script is not UTF-8")?;
    if expected_digest.is_some_and(|expected| sha256_bytes(script_text.as_bytes()) != expected) {
        bail!("transform script changed after approval; reload and approve it again");
    }
    let libraries = StdLib::TABLE | StdLib::STRING | StdLib::MATH | StdLib::UTF8;
    let lua = Lua::new_with(libraries, LuaOptions::default()).context("create Lua sandbox")?;
    lua.set_memory_limit(LUA_MEMORY_LIMIT)
        .context("set Lua memory limit")?;
    let instructions = Arc::new(AtomicU64::new(0));
    let hook_counter = Arc::clone(&instructions);
    lua.set_hook(
        HookTriggers {
            every_nth_instruction: Some(10_000),
            ..HookTriggers::default()
        },
        move |_, _| {
            let current = hook_counter.fetch_add(10_000, Ordering::Relaxed) + 10_000;
            if current > LUA_INSTRUCTION_LIMIT {
                return Err(mlua::Error::runtime("transform instruction limit exceeded"));
            }
            Ok(VmState::Continue)
        },
    )?;
    lua.load(&script_text)
        .set_name(script.display().to_string())
        .exec()
        .with_context(|| format!("load transform {}", script.display()))?;
    let function: mlua::Function = lua
        .globals()
        .get(entrypoint)
        .with_context(|| format!("transform has no {entrypoint} function"))?;
    let argument = lua.to_value(input).context("encode transform input")?;
    let returned: LuaValue = function
        .call(argument)
        .with_context(|| format!("run transform {entrypoint}"))?;
    let output = lua
        .from_value(returned)
        .context("decode transform output")?;
    validate_data_value(&output)?;
    Ok(output)
}

fn validate_data_value(root: &Value) -> Result<()> {
    let mut stack = vec![(root, 0usize)];
    let mut nodes = 0usize;
    let mut encoded_bytes = 0usize;
    while let Some((value, depth)) = stack.pop() {
        nodes += 1;
        if nodes > 100_000 || depth > 64 {
            bail!("decoded data exceeds the node or nesting limit");
        }
        match value {
            Value::String(value) => encoded_bytes = encoded_bytes.saturating_add(value.len()),
            Value::Array(values) => {
                stack.extend(values.iter().map(|value| (value, depth + 1)));
            }
            Value::Object(values) => {
                encoded_bytes =
                    encoded_bytes.saturating_add(values.keys().map(String::len).sum::<usize>());
                stack.extend(values.values().map(|value| (value, depth + 1)));
            }
            _ => {}
        }
        if encoded_bytes > SOURCE_TOTAL_LIMIT {
            bail!("decoded data exceeds the 8 MiB result limit");
        }
    }
    Ok(())
}

fn resolve_documents(sources: &BTreeMap<String, SourceSpec>) -> Result<Vec<ResolvedDocument>> {
    let mut documents = Vec::new();
    let mut seen = HashSet::new();
    let mut active = HashSet::new();
    let mut total = 0usize;
    for (source_id, spec) in sources {
        validate_identifier(source_id, "source id")?;
        validate_source_spec(spec)?;
        let root = expand_source_path(&spec.path)?;
        resolve_document_recursive(
            source_id,
            spec,
            root,
            None,
            0,
            &mut seen,
            &mut active,
            &mut total,
            &mut documents,
        )?;
    }
    Ok(documents)
}

#[allow(clippy::too_many_arguments)]
fn resolve_document_recursive(
    source_id: &str,
    spec: &SourceSpec,
    path: PathBuf,
    parent: Option<PathBuf>,
    depth: usize,
    seen: &mut HashSet<PathBuf>,
    active: &mut HashSet<PathBuf>,
    total: &mut usize,
    documents: &mut Vec<ResolvedDocument>,
) -> Result<()> {
    let normalized = normalize_absolute(&path)?;
    enforce_source_scope(&normalized, &spec.scope)?;
    reject_symlink_components(&normalized)?;
    if active.contains(&normalized) {
        bail!("import cycle detected at {}", normalized.display());
    }
    if !seen.insert(normalized.clone()) {
        return Ok(());
    }
    active.insert(normalized.clone());
    let contents = match fs::symlink_metadata(&normalized) {
        Ok(_) => String::from_utf8(read_limited_regular_file(&normalized, spec.max_bytes)?)
            .with_context(|| format!("{} is not UTF-8", normalized.display()))?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound && parent.is_none() => {
            String::new()
        }
        Err(error) => {
            return Err(error).with_context(|| format!("inspect {}", normalized.display()))
        }
    };
    *total = total.saturating_add(contents.len());
    if *total > SOURCE_TOTAL_LIMIT {
        bail!("plugin source graph exceeds {SOURCE_TOTAL_LIMIT} bytes");
    }
    documents.push(ResolvedDocument {
        source_id: source_id.to_owned(),
        path: normalized.clone(),
        parent: parent.clone(),
        contents: contents.clone(),
        format: spec.format.clone(),
        scope: (&spec.scope).into(),
    });
    let Some(imports) = &spec.imports else {
        active.remove(&normalized);
        return Ok(());
    };
    if !imports.recursive && parent.is_some() {
        active.remove(&normalized);
        return Ok(());
    }
    if depth >= imports.max_depth {
        bail!("import depth exceeded at {}", normalized.display());
    }
    let allowed_roots = imports
        .allowed_roots
        .iter()
        .map(|root| expand_source_path(root).and_then(|path| normalize_absolute(&path)))
        .collect::<Result<Vec<_>>>()?;
    let discovered = parse_imports(&contents, &normalized, imports, &allowed_roots)?;
    if discovered.len() + documents.len() > imports.max_files {
        bail!("import graph exceeds {} files", imports.max_files);
    }
    for imported in discovered {
        resolve_document_recursive(
            source_id,
            spec,
            imported,
            Some(normalized.clone()),
            depth + 1,
            seen,
            active,
            total,
            documents,
        )?;
    }
    active.remove(&normalized);
    Ok(())
}

fn parse_imports(
    contents: &str,
    including_file: &Path,
    spec: &ImportSpec,
    allowed_roots: &[PathBuf],
) -> Result<Vec<PathBuf>> {
    if spec.syntax != "shell_words" || spec.relative_to != "including_file" {
        bail!("v1 imports require syntax=shell_words and relative_to=including_file");
    }
    if allowed_roots.is_empty() {
        bail!("import rules require at least one allowed_root");
    }
    let mut result = Vec::new();
    for line in contents.lines() {
        let trimmed = line.trim();
        let Some(rest) = trimmed.strip_prefix(&spec.keyword) else {
            continue;
        };
        if !rest.starts_with(char::is_whitespace) {
            continue;
        }
        for token in shell_words::split(rest.trim()).context("parse import arguments")? {
            let expanded = expand_template(&token)?;
            let candidate = if Path::new(&expanded).is_absolute() {
                PathBuf::from(expanded)
            } else {
                including_file
                    .parent()
                    .ok_or_else(|| anyhow!("including file has no parent"))?
                    .join(expanded)
            };
            let paths = if spec.glob && has_glob_meta(&candidate) {
                glob(candidate.to_string_lossy().as_ref())
                    .context("invalid import glob")?
                    .collect::<std::result::Result<Vec<_>, _>>()?
            } else {
                vec![candidate]
            };
            for path in paths {
                let normalized = normalize_absolute(&path)?;
                if !allowed_roots
                    .iter()
                    .any(|root| normalized.starts_with(root))
                {
                    bail!("import {} escapes allowed_roots", normalized.display());
                }
                result.push(normalized);
            }
        }
    }
    result.sort();
    result.dedup();
    Ok(result)
}

fn decode_output(
    decoder: &str,
    config: &DecoderConfig,
    output: &[u8],
    lua: Option<(&Path, &str, &str)>,
) -> Result<Value> {
    let text = std::str::from_utf8(output).context("adapter output is not UTF-8")?;
    let data = match decoder {
        "json" => serde_json::from_str(text).context("decode adapter JSON")?,
        "json_lines" => Value::Array(
            text.lines()
                .filter(|line| !line.trim().is_empty())
                .map(serde_json::from_str)
                .collect::<std::result::Result<Vec<_>, _>>()
                .context("decode adapter JSON Lines")?,
        ),
        "key_value" => {
            let mut object = Map::new();
            for line in text.lines().filter(|line| !line.trim().is_empty()) {
                let (key, value) = line
                    .split_once('=')
                    .ok_or_else(|| anyhow!("adapter key_value line has no '='"))?;
                object.insert(
                    key.trim().to_owned(),
                    Value::String(value.trim().to_owned()),
                );
            }
            Value::Object(object)
        }
        "ini" => decode_ini_snapshot(text)?,
        "csv" => decode_csv(text, config)?,
        "fixed_status" => decode_fixed_status(text, config)?,
        "delimited" => decode_delimited(text, config)?,
        "single" => Value::String(text.trim().to_owned()),
        "single_record" => Value::Array(vec![json!({
            "id": "singleton",
            "value": text.trim()
        })]),
        "lua" => {
            let (script, digest, entrypoint) =
                lua.ok_or_else(|| anyhow!("Lua decoder has no transform"))?;
            run_lua_function(
                script,
                Some(digest),
                entrypoint,
                &json!({ "stdout": text, "stderr": "", "status": 0 }),
            )?
        }
        other => bail!("unsupported decoder {other}"),
    };
    let snapshot = if let Some(collection) = &config.collection {
        let mut root = Map::new();
        root.insert(collection.clone(), data);
        Value::Object(root)
    } else {
        data
    };
    validate_data_value(&snapshot)?;
    Ok(snapshot)
}

fn decode_delimited(text: &str, config: &DecoderConfig) -> Result<Value> {
    let delimiter = config
        .delimiter
        .as_deref()
        .ok_or_else(|| anyhow!("delimited decoder requires delimiter"))?;
    if delimiter.len() != 1 || config.columns.is_empty() {
        bail!("delimited decoder requires a delimiter and columns");
    }
    decode_csv_records(text, delimiter.as_bytes()[0], false, &config.columns)
}

fn decode_csv(text: &str, config: &DecoderConfig) -> Result<Value> {
    let delimiter = config.delimiter.as_deref().unwrap_or(",");
    if delimiter.len() != 1 {
        bail!("CSV delimiter must be exactly one byte");
    }
    decode_csv_records(
        text,
        delimiter.as_bytes()[0],
        config.headers,
        &config.columns,
    )
}

fn decode_csv_records(
    text: &str,
    delimiter: u8,
    has_headers: bool,
    declared_columns: &[String],
) -> Result<Value> {
    let mut reader = csv::ReaderBuilder::new()
        .delimiter(delimiter)
        .has_headers(has_headers)
        .flexible(false)
        .from_reader(text.as_bytes());
    let columns = if has_headers {
        let headers = reader.headers().context("decode CSV header")?;
        if headers.is_empty() {
            bail!("CSV header is empty");
        }
        headers.iter().map(str::to_owned).collect::<Vec<_>>()
    } else {
        if declared_columns.is_empty() {
            bail!("CSV without headers requires decoder_config.columns");
        }
        declared_columns.to_vec()
    };
    let mut rows = Vec::new();
    for record in reader.records() {
        let record = record.context("decode CSV row")?;
        if record.len() != columns.len() {
            bail!(
                "CSV row has {} columns; expected {}",
                record.len(),
                columns.len()
            );
        }
        let object = columns
            .iter()
            .zip(record.iter())
            .map(|(key, value)| (key.clone(), Value::String(value.trim().to_owned())))
            .collect::<Map<_, _>>();
        rows.push(Value::Object(object));
    }
    Ok(Value::Array(rows))
}

fn decode_ini_snapshot(text: &str) -> Result<Value> {
    let mut root = Map::new();
    let mut section: Option<String> = None;
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with(['#', ';']) {
            continue;
        }
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            let name = trimmed[1..trimmed.len() - 1].trim();
            validate_identifier(name, "INI section")?;
            section = Some(name.to_owned());
            root.entry(name.to_owned())
                .or_insert_with(|| Value::Object(Map::new()));
            continue;
        }
        let (key, value) = trimmed
            .split_once('=')
            .ok_or_else(|| anyhow!("INI line has no '='"))?;
        let key = key.trim();
        validate_identifier(key, "INI key")?;
        let decoded = Value::String(value.trim().trim_matches('"').to_owned());
        if let Some(section) = &section {
            root.get_mut(section)
                .and_then(Value::as_object_mut)
                .ok_or_else(|| anyhow!("INI section is not an object"))?
                .insert(key.to_owned(), decoded);
        } else {
            root.insert(key.to_owned(), decoded);
        }
    }
    Ok(Value::Object(root))
}

fn decode_fixed_status(text: &str, config: &DecoderConfig) -> Result<Value> {
    let pattern = config
        .pattern
        .as_deref()
        .ok_or_else(|| anyhow!("fixed_status decoder requires decoder_config.pattern"))?;
    if pattern.len() > 4096 {
        bail!("fixed_status regex exceeds 4096 bytes");
    }
    let regex = regex::RegexBuilder::new(pattern)
        .size_limit(1024 * 1024)
        .dfa_size_limit(1024 * 1024)
        .build()
        .context("compile fixed_status regex")?;
    let names = regex
        .capture_names()
        .flatten()
        .map(str::to_owned)
        .collect::<Vec<_>>();
    if names.is_empty() {
        bail!("fixed_status regex requires named capture groups");
    }
    let mut records = Vec::new();
    for line in text.lines().filter(|line| !line.trim().is_empty()) {
        let captures = regex
            .captures(line)
            .ok_or_else(|| anyhow!("fixed_status line did not match the declared pattern"))?;
        let object = names
            .iter()
            .map(|name| {
                (
                    name.clone(),
                    captures
                        .name(name)
                        .map(|capture| Value::String(capture.as_str().to_owned()))
                        .unwrap_or(Value::Null),
                )
            })
            .collect::<Map<_, _>>();
        records.push(Value::Object(object));
    }
    Ok(Value::Array(records))
}

fn invoke_command(invocation: &CommandInvocation) -> Result<Vec<u8>> {
    if !invocation.program.is_absolute() {
        bail!("adapter command must be an absolute path");
    }
    reject_symlink_components(&invocation.program)?;
    if sha256_file(&invocation.program, 64 * 1024 * 1024)? != invocation.expected_digest {
        bail!("adapter executable changed after approval; reload and approve the new digest");
    }
    let launcher = sandbox_launcher()?;
    let sandbox_request = SandboxRequest {
        program: invocation.program.clone(),
        expected_digest: invocation.expected_digest.clone(),
        args: invocation.args.clone(),
        read_paths: invocation.read_paths.clone(),
        network: invocation.network.clone(),
    };
    let request_bytes = serde_json::to_vec(&sandbox_request).context("encode sandbox request")?;
    let mut command = Command::new(&launcher);
    command
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("LANG", "C.UTF-8")
        .env("LC_ALL", "C.UTF-8")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let mut child = command
        .spawn()
        .with_context(|| format!("start mandatory sandbox {}", launcher.display()))?;
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| anyhow!("sandbox stdin unavailable"))?;
    stdin
        .write_all(&request_bytes)
        .context("send sandbox request")?;
    drop(stdin);
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow!("adapter stdout unavailable"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| anyhow!("adapter stderr unavailable"))?;
    let stdout_reader = thread::spawn(move || read_capped(stdout));
    let stderr_reader = thread::spawn(move || read_capped(stderr));
    let started = Instant::now();
    let timeout = Duration::from_millis(invocation.timeout_ms);
    let status = loop {
        if let Some(status) = child.try_wait().context("poll adapter command")? {
            break status;
        }
        if started.elapsed() >= timeout {
            terminate_process_group(&mut child);
            let _ = child.wait();
            return Err(anyhow!(
                "adapter command exceeded {} ms",
                invocation.timeout_ms
            ));
        }
        thread::sleep(Duration::from_millis(10));
    };
    // A successful direct child may leave descendants holding inherited pipe
    // descriptors. Clear the remaining process group before joining readers.
    terminate_process_group(&mut child);
    let stdout = join_capped(stdout_reader, "stdout")?;
    let stderr = join_capped(stderr_reader, "stderr")?;
    if !status.success() {
        bail!(
            "adapter command exited with {status}: {}",
            clean_text(String::from_utf8_lossy(&stderr).trim())
        );
    }
    Ok(stdout)
}

fn sandbox_launcher() -> Result<PathBuf> {
    let current = std::env::current_exe().context("locate Reginux executable")?;
    let mut candidates = Vec::new();
    if let Some(parent) = current.parent() {
        candidates.push(parent.join("reginux-sandbox"));
        if parent.file_name().and_then(|name| name.to_str()) == Some("deps") {
            if let Some(profile) = parent.parent() {
                candidates.push(profile.join("reginux-sandbox"));
            }
        }
    }
    for candidate in candidates {
        if candidate.exists() {
            reject_symlink_components(&candidate)?;
            return Ok(candidate);
        }
    }
    bail!(
        "mandatory reginux-sandbox launcher is not installed beside {}",
        current.display()
    )
}

fn invoke_adapter(invocation: &AdapterInvocation) -> Result<Vec<u8>> {
    match invocation {
        AdapterInvocation::Command(invocation) => invoke_command(invocation),
        AdapterInvocation::DBus(invocation) => invoke_dbus(invocation),
        AdapterInvocation::Socket(invocation) => invoke_socket(invocation),
    }
}

fn invoke_dbus(invocation: &DBusInvocation) -> Result<Vec<u8>> {
    use zbus::blocking::{connection, Proxy};
    use zbus::zvariant::{StructureBuilder, Value as ZValue};

    let timeout = Duration::from_millis(invocation.timeout_ms);
    let builder = match invocation.bus {
        BusKind::Session => connection::Builder::session(),
        BusKind::System => connection::Builder::system(),
    }
    .context("open declared D-Bus")?
    .method_timeout(timeout);
    let connection = builder.build().context("connect to declared D-Bus")?;
    let proxy = Proxy::new(
        &connection,
        invocation.service.as_str(),
        invocation.object_path.as_str(),
        invocation.interface.as_str(),
    )
    .context("create declared D-Bus proxy")?;

    let message = if invocation.args.is_empty() {
        proxy.call_method(invocation.member.as_str(), &())
    } else {
        let mut body = StructureBuilder::new();
        for (argument, value_type) in invocation.args.iter().zip(&invocation.arg_types) {
            let value = match value_type {
                ValueType::Boolean => ZValue::from(
                    argument
                        .parse::<bool>()
                        .with_context(|| format!("parse D-Bus boolean {argument:?}"))?,
                ),
                ValueType::Integer => ZValue::from(
                    argument
                        .parse::<i64>()
                        .with_context(|| format!("parse D-Bus integer {argument:?}"))?,
                ),
                ValueType::Float => ZValue::from(
                    argument
                        .parse::<f64>()
                        .with_context(|| format!("parse D-Bus float {argument:?}"))?,
                ),
                ValueType::String | ValueType::Enum | ValueType::Path | ValueType::Secret => {
                    ZValue::from(argument.clone())
                }
                ValueType::List | ValueType::Raw => {
                    bail!("D-Bus arguments do not support list/raw without a typed manifest ABI")
                }
            };
            body.push_value(value);
        }
        proxy.call_method(invocation.member.as_str(), &body.build()?)
    }
    .with_context(|| format!("call D-Bus member {}", invocation.member))?;
    if message.primary_header().body_len() as usize > COMMAND_OUTPUT_LIMIT {
        bail!("D-Bus response body exceeds the 1 MiB limit");
    }
    let body = message.body();
    match invocation.reply_type.as_str() {
        "unit" => {
            body.deserialize::<()>()
                .context("decode empty D-Bus reply")?;
            Ok(Vec::new())
        }
        "string" | "json" => bounded_adapter_output(
            body.deserialize::<String>()
                .context("decode D-Bus string reply")?
                .into_bytes(),
            "D-Bus string reply",
        ),
        "bytes" => bounded_adapter_output(
            body.deserialize::<Vec<u8>>()
                .context("decode D-Bus byte-array reply")?,
            "D-Bus byte-array reply",
        ),
        "boolean" => bounded_adapter_output(
            body.deserialize::<bool>()
                .context("decode D-Bus boolean reply")?
                .to_string()
                .into_bytes(),
            "D-Bus boolean reply",
        ),
        "integer" => bounded_adapter_output(
            body.deserialize::<i64>()
                .context("decode D-Bus integer reply")?
                .to_string()
                .into_bytes(),
            "D-Bus integer reply",
        ),
        "float" => bounded_adapter_output(
            body.deserialize::<f64>()
                .context("decode D-Bus float reply")?
                .to_string()
                .into_bytes(),
            "D-Bus float reply",
        ),
        "string_array" => bounded_adapter_output(
            serde_json::to_vec(
                &body
                    .deserialize::<Vec<String>>()
                    .context("decode D-Bus string-array reply")?,
            )
            .context("encode D-Bus string-array reply")?,
            "D-Bus string-array reply",
        ),
        "string_map" => bounded_adapter_output(
            serde_json::to_vec(
                &body
                    .deserialize::<BTreeMap<String, String>>()
                    .context("decode D-Bus string-map reply")?,
            )
            .context("encode D-Bus string-map reply")?,
            "D-Bus string-map reply",
        ),
        other => bail!("unsupported D-Bus reply_type {other}"),
    }
}

fn invoke_socket(invocation: &SocketInvocation) -> Result<Vec<u8>> {
    if invocation.request.len() > COMMAND_OUTPUT_LIMIT {
        bail!("Unix socket request exceeds the 1 MiB limit");
    }
    reject_symlink_components(&invocation.endpoint)?;
    let metadata = fs::symlink_metadata(&invocation.endpoint)
        .with_context(|| format!("inspect Unix socket {}", invocation.endpoint.display()))?;
    if !metadata.file_type().is_socket() {
        bail!("declared endpoint is not a Unix socket");
    }
    let mut stream = UnixStream::connect(&invocation.endpoint)
        .with_context(|| format!("connect Unix socket {}", invocation.endpoint.display()))?;
    let timeout = Some(Duration::from_millis(invocation.timeout_ms));
    stream.set_read_timeout(timeout)?;
    stream.set_write_timeout(timeout)?;
    verify_peer_uid(&stream, invocation.expected_peer_uid)?;

    match invocation.framing {
        SocketFraming::Eof => {
            stream.write_all(invocation.request.as_bytes())?;
            stream.shutdown(Shutdown::Write)?;
            read_capped(stream).context("read Unix socket response")
        }
        SocketFraming::Line => {
            stream.write_all(invocation.request.as_bytes())?;
            stream.write_all(b"\n")?;
            stream.flush()?;
            let mut result = Vec::new();
            let mut byte = [0_u8; 1];
            while result.len() <= COMMAND_OUTPUT_LIMIT {
                let count = stream.read(&mut byte)?;
                if count == 0 || byte[0] == b'\n' {
                    break;
                }
                result.push(byte[0]);
            }
            if result.len() > COMMAND_OUTPUT_LIMIT {
                bail!("Unix socket response exceeds the 1 MiB limit");
            }
            Ok(result)
        }
        SocketFraming::LengthPrefixed => {
            let request = invocation.request.as_bytes();
            let length = u32::try_from(request.len()).context("socket request is too large")?;
            stream.write_all(&length.to_be_bytes())?;
            stream.write_all(request)?;
            stream.flush()?;
            let mut header = [0_u8; 4];
            stream.read_exact(&mut header)?;
            let response_length = u32::from_be_bytes(header) as usize;
            if response_length > COMMAND_OUTPUT_LIMIT {
                bail!("Unix socket response exceeds the 1 MiB limit");
            }
            let mut response = vec![0_u8; response_length];
            stream.read_exact(&mut response)?;
            Ok(response)
        }
    }
}

fn bounded_adapter_output(output: Vec<u8>, description: &str) -> Result<Vec<u8>> {
    if output.len() > COMMAND_OUTPUT_LIMIT {
        bail!("{description} exceeds the 1 MiB limit");
    }
    Ok(output)
}

fn verify_peer_uid(stream: &UnixStream, expected_uid: u32) -> Result<()> {
    let mut credentials = libc::ucred {
        pid: 0,
        uid: 0,
        gid: 0,
    };
    let mut length = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    let result = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut credentials as *mut libc::ucred).cast(),
            &mut length,
        )
    };
    if result == -1 {
        return Err(std::io::Error::last_os_error()).context("read Unix socket peer credentials");
    }
    if credentials.uid != expected_uid {
        bail!(
            "Unix socket peer UID {} does not match declared UID {}",
            credentials.uid,
            expected_uid
        );
    }
    Ok(())
}

fn operation_invocation(
    transport: &LoadedTransport,
    operation: &OperationSpec,
    record: &Value,
) -> Result<AdapterInvocation> {
    if operation.timeout_ms == 0 || operation.timeout_ms > 30_000 {
        bail!("adapter timeout must be between 1 and 30000 ms");
    }
    let args = operation
        .args
        .iter()
        .map(|argument| substitute_resource_placeholders(argument, record))
        .collect::<Result<Vec<_>>>()?;
    match transport {
        LoadedTransport::Command {
            program,
            digest,
            read_paths,
            network,
        } => Ok(AdapterInvocation::Command(CommandInvocation {
            program: program.clone(),
            expected_digest: digest.clone(),
            args,
            timeout_ms: operation.timeout_ms,
            read_paths: read_paths.clone(),
            network: network.clone(),
        })),
        LoadedTransport::DBus {
            bus,
            service,
            object_path,
            interface,
        } => {
            let member = operation
                .member
                .clone()
                .ok_or_else(|| anyhow!("D-Bus operation requires member"))?;
            if operation.arg_types.len() != args.len() {
                bail!("D-Bus arg_types count must match args count");
            }
            let arg_types = operation
                .arg_types
                .iter()
                .map(|value| parse_value_type(value))
                .collect::<Result<Vec<_>>>()?;
            Ok(AdapterInvocation::DBus(DBusInvocation {
                bus: bus.clone(),
                service: service.clone(),
                object_path: object_path.clone(),
                interface: interface.clone(),
                member,
                args,
                arg_types,
                reply_type: operation
                    .reply_type
                    .clone()
                    .unwrap_or_else(|| "string".to_owned()),
                timeout_ms: operation.timeout_ms,
            }))
        }
        LoadedTransport::Socket { endpoint, peer_uid } => {
            if !args.is_empty() {
                bail!("Unix socket operations use request, not args");
            }
            let request = substitute_resource_placeholders(
                operation
                    .request
                    .as_deref()
                    .ok_or_else(|| anyhow!("Unix socket operation requires request"))?,
                record,
            )?;
            let framing = match operation.framing.as_deref().unwrap_or("line") {
                "eof" => SocketFraming::Eof,
                "line" => SocketFraming::Line,
                "length_prefixed" => SocketFraming::LengthPrefixed,
                other => bail!("unsupported Unix socket framing {other}"),
            };
            Ok(AdapterInvocation::Socket(SocketInvocation {
                endpoint: endpoint.clone(),
                request,
                framing,
                expected_peer_uid: *peer_uid,
                timeout_ms: operation.timeout_ms,
            }))
        }
    }
}

fn substitute_resource_placeholders(argument: &str, record: &Value) -> Result<String> {
    let mut result = argument.to_owned();
    while let Some(start) = result.find("${resource.") {
        let tail = &result[start + 11..];
        let end = tail
            .find('}')
            .ok_or_else(|| anyhow!("unterminated resource placeholder"))?;
        let property = &tail[..end];
        validate_identifier(property, "resource property")?;
        let replacement = record
            .get(property)
            .map(json_value_to_string)
            .ok_or_else(|| anyhow!("resource has no property {property}"))?;
        result.replace_range(start..start + 11 + end + 1, &replacement);
    }
    let without_value_placeholders = result.replace("${value}", "").replace("${old_value}", "");
    if without_value_placeholders.contains("${") {
        bail!("unknown adapter placeholder in {argument}");
    }
    Ok(result)
}

fn root_documents(documents: &[ResolvedDocument]) -> HashMap<String, &ResolvedDocument> {
    documents
        .iter()
        .filter(|document| document.parent.is_none())
        .map(|document| (document.source_id.clone(), document))
        .collect()
}

fn select_schema_target<'a>(
    field: &FieldSpec,
    root: &'a ResolvedDocument,
    origin: Option<&'a ResolvedDocument>,
    roots: &HashMap<String, &'a ResolvedDocument>,
) -> Result<&'a ResolvedDocument> {
    match field.write_target.as_str() {
        "origin" => Ok(origin.unwrap_or(root)),
        "root" => Ok(root),
        "explicit_source" => {
            let source_id = field
                .explicit_source
                .as_deref()
                .ok_or_else(|| anyhow!("write_target=explicit_source requires explicit_source"))?;
            roots
                .get(source_id)
                .copied()
                .ok_or_else(|| anyhow!("explicit write source {source_id} does not exist"))
        }
        other => bail!("unsupported write_target {other}; use origin, root, or explicit_source"),
    }
}

fn find_effective_schema_value<'a>(
    source_id: &str,
    spec: &SourceSpec,
    root: &'a ResolvedDocument,
    documents: &'a [ResolvedDocument],
    key: &str,
) -> Result<Option<(&'a ResolvedDocument, String)>> {
    let by_path = documents
        .iter()
        .filter(|document| document.source_id == source_id)
        .map(|document| (document.path.clone(), document))
        .collect::<HashMap<_, _>>();
    let allowed_roots = spec
        .imports
        .as_ref()
        .map(|imports| {
            imports
                .allowed_roots
                .iter()
                .map(|path| expand_source_path(path).and_then(|path| normalize_absolute(&path)))
                .collect::<Result<Vec<_>>>()
        })
        .transpose()?
        .unwrap_or_default();

    #[allow(clippy::too_many_arguments)]
    fn visit<'a>(
        document: &'a ResolvedDocument,
        spec: &SourceSpec,
        by_path: &HashMap<PathBuf, &'a ResolvedDocument>,
        allowed_roots: &[PathBuf],
        key: &str,
        depth: usize,
        active: &mut HashSet<PathBuf>,
        found: &mut Option<(&'a ResolvedDocument, String)>,
    ) -> Result<()> {
        if !active.insert(document.path.clone()) {
            bail!("import cycle detected at {}", document.path.display());
        }
        if spec.imports.is_none() {
            if let Some(value) =
                crate::structured::find_value(&document.contents, key, &document.format)?
            {
                *found = Some((document, value));
            }
            active.remove(&document.path);
            return Ok(());
        }
        for line in document.contents.lines() {
            if let Some(imports) = &spec.imports {
                let can_recurse = imports.recursive || depth == 0;
                if can_recurse {
                    let imported = parse_imports(line, &document.path, imports, allowed_roots)?;
                    if !imported.is_empty() {
                        for path in imported {
                            let imported_document = by_path.get(&path).ok_or_else(|| {
                                anyhow!("resolved import {} is unavailable", path.display())
                            })?;
                            visit(
                                imported_document,
                                spec,
                                by_path,
                                allowed_roots,
                                key,
                                depth + 1,
                                active,
                                found,
                            )?;
                        }
                        continue;
                    }
                }
            }
            if let Some(value) = crate::structured::find_value(line, key, &document.format)? {
                *found = Some((document, value));
            }
        }
        active.remove(&document.path);
        Ok(())
    }

    let mut found = None;
    visit(
        root,
        spec,
        &by_path,
        &allowed_roots,
        key,
        0,
        &mut HashSet::new(),
        &mut found,
    )?;
    Ok(found)
}

fn validate_manifest(manifest: &Manifest) -> Result<()> {
    if manifest.schema_version != 1 {
        bail!(
            "unsupported schema_version {}; expected 1",
            manifest.schema_version
        );
    }
    validate_plugin_id(&manifest.plugin.id)?;
    validate_text(&manifest.plugin.name, "plugin name")?;
    validate_text(&manifest.plugin.version, "plugin version")?;
    if let Some(description) = &manifest.plugin.description {
        validate_text(description, "plugin description")?;
    }
    match manifest.plugin.kind {
        PluginKind::Schema if manifest.transport.is_some() || manifest.transform.is_some() => {
            bail!("schema plugins cannot declare transport or transform")
        }
        PluginKind::Adapter if manifest.transport.is_none() => bail!("adapter requires transport"),
        PluginKind::Transform if manifest.transform.is_none() => {
            bail!("transform requires [transform]")
        }
        PluginKind::Transform if manifest.transport.is_some() => {
            bail!("transform plugins cannot declare transport")
        }
        _ => {}
    }
    for (source_id, source) in &manifest.sources {
        validate_identifier(source_id, "source id")?;
        validate_source_spec(source)?;
    }
    for (section_id, fields) in &manifest.fields {
        validate_identifier(section_id, "section id")?;
        for (field_id, field) in fields {
            validate_identifier(field_id, "field id")?;
            parse_value_type(&field.value_type)?;
            if let Some(source) = &field.source {
                if !manifest.sources.contains_key(source) {
                    bail!("field {section_id}.{field_id} references unknown source {source}");
                }
            }
            if !matches!(
                field.write_target.as_str(),
                "origin" | "root" | "explicit_source"
            ) {
                bail!("field {section_id}.{field_id} has invalid write_target");
            }
            if field.write_target == "explicit_source" {
                let source = field.explicit_source.as_deref().ok_or_else(|| {
                    anyhow!("field {section_id}.{field_id} requires explicit_source")
                })?;
                if !manifest.sources.contains_key(source) {
                    bail!("field {section_id}.{field_id} has unknown explicit_source {source}");
                }
            } else if field.explicit_source.is_some() {
                bail!("explicit_source is only valid with write_target=explicit_source");
            }
            if !matches!(field.insert.as_deref(), None | Some("end" | "section")) {
                bail!("field {section_id}.{field_id} has invalid insert strategy");
            }
        }
    }
    if manifest.plugin.kind == PluginKind::Adapter {
        validate_adapter_manifest(manifest)?;
    }
    Ok(())
}

fn validate_adapter_manifest(manifest: &Manifest) -> Result<()> {
    let transport = manifest
        .transport
        .as_ref()
        .ok_or_else(|| anyhow!("adapter requires transport"))?;
    validate_transport_declaration(transport)?;
    if !manifest.operations.contains_key("snapshot") {
        bail!("adapter requires operations.snapshot");
    }
    if manifest.nodes.is_empty() {
        bail!("adapter requires at least one presentation node");
    }
    for (id, operation) in &manifest.operations {
        validate_identifier(id, "operation id")?;
        if operation.timeout_ms == 0 || operation.timeout_ms > 30_000 {
            bail!("operation {id} timeout must be between 1 and 30000 ms");
        }
        if let Some(decoder) = operation.decoder.as_deref() {
            if !matches!(
                decoder,
                "json"
                    | "json_lines"
                    | "key_value"
                    | "ini"
                    | "csv"
                    | "fixed_status"
                    | "delimited"
                    | "single"
                    | "single_record"
                    | "lua"
            ) {
                bail!("operation {id} has unsupported decoder {decoder}");
            }
            if decoder == "lua" && manifest.transform.is_none() {
                bail!("operation {id} uses Lua decoding without [transform]");
            }
            if decoder == "fixed_status" && operation.decoder_config.pattern.is_none() {
                bail!("operation {id} fixed_status decoder requires pattern");
            }
        }
        match operation.guarantee {
            GuaranteeSpec::Compensatable
                if operation.compensation.is_none() || operation.verify.is_none() =>
            {
                bail!("compensatable operation {id} requires compensation and verify")
            }
            _ => {}
        }
        for reference in [
            operation.compensation.as_deref(),
            operation.precondition.as_deref(),
            operation.validate.as_deref(),
            operation.verify.as_deref(),
        ]
        .into_iter()
        .flatten()
        {
            if !manifest.operations.contains_key(reference) {
                bail!("operation {id} references unknown operation {reference}");
            }
        }
        match transport.kind.as_str() {
            "command" => {
                if operation.member.is_some()
                    || operation.request.is_some()
                    || operation.framing.is_some()
                    || !operation.arg_types.is_empty()
                {
                    bail!("command operation {id} contains IPC-only fields");
                }
            }
            "dbus" => {
                if operation.member.is_none()
                    || operation.request.is_some()
                    || operation.framing.is_some()
                    || operation.arg_types.len() != operation.args.len()
                {
                    bail!("D-Bus operation {id} requires member and one arg_type per arg");
                }
                for value_type in &operation.arg_types {
                    parse_value_type(value_type)?;
                }
            }
            "unix_socket" => {
                if operation.request.is_none()
                    || !operation.args.is_empty()
                    || operation.member.is_some()
                    || !operation.arg_types.is_empty()
                {
                    bail!(
                        "Unix socket operation {id} requires request and no command/D-Bus fields"
                    );
                }
                if !matches!(
                    operation.framing.as_deref(),
                    None | Some("line" | "eof" | "length_prefixed")
                ) {
                    bail!("Unix socket operation {id} has invalid framing");
                }
            }
            _ => unreachable!("transport validated below"),
        }
    }
    for node in &manifest.nodes {
        validate_identifier(&node.id, "node id")?;
        if node.kind == "resource" && (node.collection.is_none() || node.key.is_none()) {
            bail!(
                "resource node {} requires collection and stable key",
                node.id
            );
        }
        if !matches!(node.kind.as_str(), "group" | "resource") {
            bail!("node {} has unsupported kind", node.id);
        }
        for field in &node.fields {
            validate_identifier(&field.id, "node field id")?;
            parse_value_type(&field.value_type)?;
            if let Some(operation) = &field.operation {
                let spec = manifest.operations.get(operation).ok_or_else(|| {
                    anyhow!(
                        "node field {} references unknown operation {operation}",
                        field.id
                    )
                })?;
                if field.read_only {
                    bail!("node field {} is read_only and has an operation", field.id);
                }
                if spec.precondition.is_none() {
                    bail!("editable operation {operation} requires precondition");
                }
                if spec.verify.is_none() {
                    bail!("editable operation {operation} requires verify");
                }
            }
        }
    }
    Ok(())
}

fn validate_transport_declaration(transport: &TransportSpec) -> Result<()> {
    match transport.kind.as_str() {
        "command" => {
            let program = transport
                .program
                .as_deref()
                .ok_or_else(|| anyhow!("command transport requires program"))?;
            if !Path::new(program).is_absolute() {
                bail!("command transport program must be absolute");
            }
            if transport.bus.is_some()
                || transport.service.is_some()
                || transport.object_path.is_some()
                || transport.interface.is_some()
                || transport.endpoint.is_some()
                || transport.peer.is_some()
            {
                bail!("command transport contains incompatible fields");
            }
        }
        "dbus" => {
            if !matches!(transport.bus.as_deref(), Some("session" | "system")) {
                bail!("D-Bus transport requires bus=session|system");
            }
            let service = transport
                .service
                .as_deref()
                .ok_or_else(|| anyhow!("D-Bus service is required"))?;
            let object_path = transport
                .object_path
                .as_deref()
                .ok_or_else(|| anyhow!("D-Bus object_path is required"))?;
            let interface = transport
                .interface
                .as_deref()
                .ok_or_else(|| anyhow!("D-Bus interface is required"))?;
            zbus::names::BusName::try_from(service).context("invalid D-Bus service")?;
            zbus::zvariant::ObjectPath::try_from(object_path)
                .context("invalid D-Bus object_path")?;
            zbus::names::InterfaceName::try_from(interface).context("invalid D-Bus interface")?;
            if transport.program.is_some()
                || transport.endpoint.is_some()
                || transport.peer.is_some()
                || !transport.read_paths.is_empty()
                || !matches!(transport.network, NetworkSpec::None)
            {
                bail!("D-Bus transport contains incompatible fields");
            }
        }
        "unix_socket" => {
            let endpoint = transport
                .endpoint
                .as_deref()
                .ok_or_else(|| anyhow!("Unix socket endpoint is required"))?;
            let endpoint = expand_source_path(endpoint)?;
            if !endpoint.is_absolute() {
                bail!("Unix socket endpoint must be absolute");
            }
            let peer = transport
                .peer
                .as_deref()
                .ok_or_else(|| anyhow!("Unix socket peer is required"))?;
            if !matches!(peer, "self" | "root") {
                peer.parse::<u32>()
                    .context("invalid Unix socket peer UID")?;
            }
            if transport.program.is_some()
                || transport.bus.is_some()
                || transport.service.is_some()
                || transport.object_path.is_some()
                || transport.interface.is_some()
                || !transport.read_paths.is_empty()
                || !matches!(transport.network, NetworkSpec::None)
            {
                bail!("Unix socket transport contains incompatible fields");
            }
        }
        other => bail!("unsupported adapter transport {other}"),
    }
    Ok(())
}

fn validate_source_spec(spec: &SourceSpec) -> Result<()> {
    if !matches!(
        spec.format.as_str(),
        "kitty" | "whitespace" | "key_value" | "equals" | "toml" | "ini" | "kdl" | "lua"
    ) {
        bail!("unsupported source format {}", spec.format);
    }
    if spec.max_bytes == 0 || spec.max_bytes > SOURCE_TOTAL_LIMIT as u64 {
        bail!("source max_bytes must be between 1 and {SOURCE_TOTAL_LIMIT}");
    }
    if let Some(imports) = &spec.imports {
        if crate::structured::is_structured(&spec.format) {
            bail!("TOML/INI/KDL sources cannot declare line-oriented imports");
        }
        validate_text(&imports.keyword, "import keyword")?;
        if imports.max_depth == 0 || imports.max_depth > 64 {
            bail!("import max_depth must be between 1 and 64");
        }
        if imports.max_files == 0 || imports.max_files > 1024 {
            bail!("import max_files must be between 1 and 1024");
        }
    }
    Ok(())
}

fn parse_value_type(value: &str) -> Result<ValueType> {
    match value {
        "boolean" | "bool" => Ok(ValueType::Boolean),
        "integer" | "int" => Ok(ValueType::Integer),
        "float" | "number" => Ok(ValueType::Float),
        "string" => Ok(ValueType::String),
        "enum" => Ok(ValueType::Enum),
        "path" => Ok(ValueType::Path),
        "list" => Ok(ValueType::List),
        "raw" => Ok(ValueType::Raw),
        "secret" => Ok(ValueType::Secret),
        other => bail!("unsupported field type {other}"),
    }
}

fn field_metadata(field: &FieldSpec) -> Vec<(String, String)> {
    let mut metadata = Vec::new();
    if let Some(min) = field.min {
        metadata.push(("min".to_owned(), min.to_string()));
    }
    if let Some(max) = field.max {
        metadata.push(("max".to_owned(), max.to_string()));
    }
    if !field.values.is_empty() {
        metadata.push(("values".to_owned(), field.values.join("|")));
    }
    if field.sensitive {
        metadata.push(("sensitive".to_owned(), "true".to_owned()));
    }
    metadata
}

fn field_validation(field: &FieldSpec, value_type: &ValueType) -> String {
    let mut validation = value_type.as_str().to_owned();
    if let Some(min) = field.min {
        validation.push_str(&format!("; min={min}"));
    }
    if let Some(max) = field.max {
        validation.push_str(&format!("; max={max}"));
    }
    if !field.values.is_empty() {
        validation.push_str(&format!("; values={}", field.values.join(",")));
    }
    validation
}

fn plugin_digest(directory: &Path, manifest: &Manifest, manifest_bytes: &[u8]) -> Result<String> {
    let mut hasher = Sha256::new();
    hasher.update(manifest_bytes);
    if let Some(transport) = &manifest.transport {
        if let Some(program) = &transport.program {
            let program = PathBuf::from(program);
            if program.is_absolute() {
                hasher.update(program.as_os_str().as_encoded_bytes());
                hash_file_into(&program, &mut hasher, 64 * 1024 * 1024)?;
            }
        }
    }
    if let Some(transform) = &manifest.transform {
        let script = secure_relative_plugin_file(directory, &transform.script, MANIFEST_LIMIT)?;
        hasher.update(script.as_os_str().as_encoded_bytes());
        hash_file_into(&script, &mut hasher, MANIFEST_LIMIT)?;
    }
    Ok(format!("sha256:{:x}", hasher.finalize()))
}

fn is_approved(manifest: &Manifest, digest: &str, policy: &PluginPolicy) -> bool {
    if policy.temporary_approvals.contains(&manifest.plugin.id) {
        return true;
    }
    let approved = match manifest.plugin.kind {
        PluginKind::Adapter => policy.approved_adapters.get(&manifest.plugin.id),
        PluginKind::Transform => policy.approved_transforms.get(&manifest.plugin.id),
        PluginKind::Schema => return true,
    };
    approved.is_some_and(|approved| approved == digest)
}

fn adapter_permissions(manifest: &Manifest) -> Vec<String> {
    let mut permissions = Vec::new();
    if let Some(transport) = &manifest.transport {
        match transport.kind.as_str() {
            "command" => {
                permissions.push(clean_text(&format!(
                    "command: {}",
                    transport.program.as_deref().unwrap_or("<missing>")
                )));
                permissions.push("kernel sandbox: Landlock + seccomp + rlimits".to_owned());
                permissions.push(format!("network: {:?}", transport.network));
                for path in &transport.read_paths {
                    permissions.push(clean_text(&format!("read path: {path}")));
                }
            }
            "dbus" => permissions.push(clean_text(&format!(
                "D-Bus: {} {} {} {}",
                transport.bus.as_deref().unwrap_or("?"),
                transport.service.as_deref().unwrap_or("?"),
                transport.object_path.as_deref().unwrap_or("?"),
                transport.interface.as_deref().unwrap_or("?")
            ))),
            "unix_socket" => permissions.push(clean_text(&format!(
                "Unix socket: {} (peer {})",
                transport.endpoint.as_deref().unwrap_or("?"),
                transport.peer.as_deref().unwrap_or("?")
            ))),
            other => permissions.push(clean_text(&format!("unknown transport: {other}"))),
        }
    }
    if manifest
        .operations
        .values()
        .any(|operation| operation.scope == ScopeSpec::System)
    {
        permissions.push("contains system-scope operations".to_owned());
    }
    permissions
}

fn adapter_capabilities(manifest: &Manifest) -> Vec<String> {
    manifest
        .operations
        .keys()
        .map(|id| clean_text(id))
        .collect()
}

fn source_descriptions(sources: &BTreeMap<String, SourceSpec>) -> Vec<String> {
    sources
        .iter()
        .map(|(id, source)| clean_text(&format!("{id}: {} ({:?})", source.path, source.scope)))
        .collect()
}

fn plugin_trust(directory: &Path, kind: &PluginKind) -> String {
    if matches!(kind, PluginKind::Schema) && directory.starts_with("/usr/share/reginux/plugins") {
        "system declarative".to_owned()
    } else if matches!(kind, PluginKind::Schema) {
        "user declarative".to_owned()
    } else {
        "external code".to_owned()
    }
}

fn error_summary(path: &Path, error: anyhow::Error) -> PluginSummary {
    let identity = manifest_identity(path);
    PluginSummary {
        id: identity
            .as_ref()
            .map(|(id, _)| id.clone())
            .unwrap_or_else(|| path.display().to_string()),
        name: identity
            .map(|(_, name)| name)
            .unwrap_or_else(|| path.display().to_string()),
        kind: "invalid".to_owned(),
        path: path.to_path_buf(),
        status: format!("error: {}", clean_text(&error.to_string())),
        permissions: Vec::new(),
        trust: "unknown".to_owned(),
        approval: "not evaluated".to_owned(),
        digest: None,
        sources: Vec::new(),
        capabilities: Vec::new(),
        captured_at: None,
        last_error: Some(clean_text(&error.to_string())),
        last_error_at: Some(Utc::now().to_rfc3339()),
        stale: false,
    }
}

fn manifest_identity(path: &Path) -> Option<(String, String)> {
    let bytes = read_limited_regular_file(path, MANIFEST_LIMIT).ok()?;
    let text = std::str::from_utf8(&bytes).ok()?;
    let value = toml::from_str::<toml::Value>(text).ok()?;
    let plugin = value.get("plugin")?.as_table()?;
    let id = plugin.get("id")?.as_str()?;
    let name = plugin
        .get("name")
        .and_then(toml::Value::as_str)
        .unwrap_or(id);
    validate_plugin_id(id).ok()?;
    Some((id.to_owned(), clean_text(name)))
}

fn secure_plugin_directories(root: &Path) -> Result<Vec<PathBuf>> {
    let root = normalize_absolute(root)?;
    reject_symlink_components(&root)?;
    let mut result = vec![root.clone()];
    for item in fs::read_dir(&root).with_context(|| format!("read {}", root.display()))? {
        let path = item?.path();
        let metadata = fs::symlink_metadata(&path)?;
        if metadata.file_type().is_symlink() {
            continue;
        }
        if metadata.is_dir() {
            result.push(path);
        }
    }
    result.sort();
    Ok(result)
}

fn find_manifest(directory: &Path) -> Result<Option<PathBuf>> {
    let manifests = ["manifest.toml", "plugin.toml"]
        .iter()
        .map(|name| directory.join(name))
        .filter(|path| path.exists())
        .collect::<Vec<_>>();
    match manifests.as_slice() {
        [] => {
            let legacy = ["manifest.yaml", "manifest.yml", "plugin.yaml", "plugin.yml"]
                .iter()
                .any(|name| directory.join(name).exists());
            if legacy {
                bail!("legacy YAML manifest is unsupported; migrate to schema_version=1 TOML")
            }
            Ok(None)
        }
        [manifest] => {
            reject_symlink_components(manifest)?;
            Ok(Some(manifest.clone()))
        }
        _ => bail!("ambiguous plugin directory: both manifest.toml and plugin.toml exist"),
    }
}

fn secure_relative_plugin_file(directory: &Path, relative: &str, limit: u64) -> Result<PathBuf> {
    let path = Path::new(relative);
    if path.is_absolute()
        || path
            .components()
            .any(|component| component == Component::ParentDir)
    {
        bail!("plugin-local file must be relative and may not contain '..'");
    }
    let full = normalize_absolute(&directory.join(path))?;
    let root = normalize_absolute(directory)?;
    if !full.starts_with(&root) {
        bail!("plugin-local file escapes plugin directory");
    }
    let _ = read_limited_regular_file(&full, limit)?;
    Ok(full)
}

fn expand_configured_root(value: &str) -> PathBuf {
    if value == "~" {
        home_dir()
    } else if let Some(rest) = value.strip_prefix("~/") {
        home_dir().join(rest)
    } else {
        PathBuf::from(value)
    }
}

fn expand_source_path(value: &str) -> Result<PathBuf> {
    let expanded = expand_template(value)?;
    let path = PathBuf::from(expanded);
    if !path.is_absolute() {
        bail!("source path must expand to an absolute path");
    }
    normalize_absolute(&path)
}

fn expand_template(value: &str) -> Result<String> {
    let variables = BTreeMap::from([
        ("HOME", home_dir()),
        (
            "XDG_CONFIG_HOME",
            std::env::var_os("XDG_CONFIG_HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| home_dir().join(".config")),
        ),
        (
            "XDG_STATE_HOME",
            std::env::var_os("XDG_STATE_HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| home_dir().join(".local/state")),
        ),
        (
            "XDG_DATA_HOME",
            std::env::var_os("XDG_DATA_HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| home_dir().join(".local/share")),
        ),
        (
            "XDG_RUNTIME_DIR",
            std::env::var_os("XDG_RUNTIME_DIR")
                .map(PathBuf::from)
                .unwrap_or_else(|| {
                    PathBuf::from(format!("/run/user/{}", unsafe { libc::geteuid() }))
                }),
        ),
    ]);
    let mut output = String::new();
    let mut rest = value;
    while let Some(start) = rest.find("${") {
        output.push_str(&rest[..start]);
        let tail = &rest[start + 2..];
        let end = tail
            .find('}')
            .ok_or_else(|| anyhow!("unterminated environment placeholder"))?;
        let name = &tail[..end];
        let replacement = variables
            .get(name)
            .ok_or_else(|| anyhow!("environment variable {name} is not allowed"))?;
        output.push_str(&replacement.to_string_lossy());
        rest = &tail[end + 1..];
    }
    output.push_str(rest);
    if output.contains('$') {
        bail!("only ${{NAME}} placeholders are supported");
    }
    Ok(output)
}

fn normalize_absolute(path: &Path) -> Result<PathBuf> {
    if !path.is_absolute() {
        bail!("path is not absolute: {}", path.display());
    }
    let mut normalized = PathBuf::from("/");
    for component in path.components() {
        match component {
            Component::RootDir => {}
            Component::Normal(value) => normalized.push(value),
            Component::CurDir => {}
            Component::ParentDir => {
                if !normalized.pop() {
                    bail!("path escapes filesystem root");
                }
            }
            Component::Prefix(_) => bail!("unsupported path prefix"),
        }
    }
    Ok(normalized)
}

fn reject_symlink_components(path: &Path) -> Result<()> {
    let mut current = PathBuf::new();
    for component in path.components() {
        current.push(component.as_os_str());
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                bail!(
                    "symbolic link component is not allowed: {}",
                    current.display()
                )
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => break,
            Err(error) => {
                return Err(error).with_context(|| format!("inspect {}", current.display()))
            }
        }
    }
    Ok(())
}

fn enforce_source_scope(path: &Path, scope: &ScopeSpec) -> Result<()> {
    match scope {
        ScopeSpec::User if !path.starts_with(home_dir()) => {
            bail!("user source {} is outside HOME", path.display())
        }
        ScopeSpec::System if !is_system_path(path) => {
            bail!(
                "system source {} is outside supported system roots",
                path.display()
            )
        }
        _ => Ok(()),
    }
}

fn absolute_program(transport: &TransportSpec) -> Result<PathBuf> {
    let path = PathBuf::from(
        transport
            .program
            .as_deref()
            .ok_or_else(|| anyhow!("command transport has no program"))?,
    );
    if !path.is_absolute() {
        bail!("adapter program must be absolute");
    }
    reject_symlink_components(&path)?;
    Ok(path)
}

fn read_limited_regular_file(path: &Path, limit: u64) -> Result<Vec<u8>> {
    read_regular_file_limited(path, limit)
}

fn hash_file_into(path: &Path, hasher: &mut Sha256, limit: u64) -> Result<()> {
    let bytes = read_limited_regular_file(path, limit)?;
    hasher.update(bytes);
    Ok(())
}

fn sha256_file(path: &Path, limit: u64) -> Result<String> {
    Ok(sha256_bytes(&read_limited_regular_file(path, limit)?))
}

fn validate_system_plugin_location(directory: &Path, manifest_path: &Path) -> Result<()> {
    let trusted_root = Path::new("/usr/share/reginux/plugins");
    if !directory.starts_with(trusted_root) {
        bail!(
            "system sources require a plugin installed under {}",
            trusted_root.display()
        );
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let mut checked = vec![trusted_root.to_path_buf()];
        let relative = directory
            .strip_prefix(trusted_root)
            .context("system plugin escaped the trusted root")?;
        let mut current = trusted_root.to_path_buf();
        for component in relative.components() {
            current.push(component.as_os_str());
            checked.push(current.clone());
        }
        checked.push(manifest_path.to_path_buf());
        checked.sort();
        checked.dedup();
        for path in checked {
            let metadata = fs::symlink_metadata(&path)
                .with_context(|| format!("inspect trusted plugin path {}", path.display()))?;
            let mode = metadata.permissions().mode();
            if metadata.uid() != 0 || mode & 0o022 != 0 {
                bail!(
                    "system plugin path {} must be root-owned and not group/world writable",
                    path.display()
                );
            }
        }
    }
    Ok(())
}

fn sha256_bytes(bytes: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(bytes))
}

fn read_capped(mut reader: impl Read) -> std::io::Result<Vec<u8>> {
    let mut result = Vec::new();
    let mut buffer = [0_u8; 8192];
    loop {
        let count = reader.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        if result.len() + count > COMMAND_OUTPUT_LIMIT {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "adapter output limit exceeded",
            ));
        }
        result.extend_from_slice(&buffer[..count]);
    }
    Ok(result)
}

fn join_capped(
    handle: thread::JoinHandle<std::io::Result<Vec<u8>>>,
    stream: &str,
) -> Result<Vec<u8>> {
    handle
        .join()
        .map_err(|_| anyhow!("adapter {stream} reader panicked"))?
        .with_context(|| format!("read adapter {stream}"))
}

fn terminate_process_group(child: &mut std::process::Child) {
    #[cfg(unix)]
    unsafe {
        libc::kill(-(child.id() as i32), libc::SIGKILL);
    }
    #[cfg(not(unix))]
    {
        let _ = child.kill();
    }
}

fn validate_plugin_id(value: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > 128
        || !value
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-'))
    {
        bail!("invalid plugin id {value:?}");
    }
    Ok(())
}

fn validate_identifier(value: &str, label: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > 128
        || !value
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-'))
    {
        bail!("invalid {label} {value:?}");
    }
    Ok(())
}

fn validate_resource_key(value: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > 256
        || value
            .chars()
            .any(|ch| ch.is_control() || matches!(ch, '/' | '\\' | '.'))
    {
        bail!("invalid dynamic resource key {value:?}");
    }
    Ok(())
}

fn validate_text(value: &str, label: &str) -> Result<()> {
    if value.len() > MAX_TEXT_FIELD || value.chars().any(|ch| ch.is_control() || ch == '\u{1b}') {
        bail!("{label} contains disallowed text");
    }
    Ok(())
}

fn clean_text(value: &str) -> String {
    value
        .chars()
        .filter(|ch| !ch.is_control() && *ch != '\u{1b}')
        .take(MAX_TEXT_FIELD)
        .collect()
}

fn validate_range(text: &str, start: usize, end: usize) -> Result<()> {
    if start > end
        || end > text.len()
        || !text.is_char_boundary(start)
        || !text.is_char_boundary(end)
    {
        bail!("transform returned an invalid UTF-8 byte range {start}..{end}");
    }
    Ok(())
}

fn pointer_string(value: &Value, pointer: &str) -> Result<String> {
    value
        .pointer(pointer)
        .map(json_value_to_string)
        .ok_or_else(|| anyhow!("snapshot has no value at {pointer}"))
}

fn json_value_to_string(value: &Value) -> String {
    match value {
        Value::Null => "<unset>".to_owned(),
        Value::Bool(value) => value.to_string(),
        Value::Number(value) => value.to_string(),
        Value::String(value) => clean_text(value),
        other => serde_json::to_string(other).unwrap_or_else(|_| "<invalid>".to_owned()),
    }
}

fn display_value(value: &str, sensitive: bool) -> String {
    if sensitive && value != "<unset>" {
        "••••••".to_owned()
    } else {
        clean_text(value)
    }
}

fn toml_value_to_string(value: &toml::Value) -> String {
    match value {
        toml::Value::String(value) => value.clone(),
        other => other.to_string(),
    }
}

fn privilege_for_scope(scope: &SourceScope) -> Privilege {
    match scope {
        SourceScope::User => Privilege::User,
        SourceScope::System => Privilege::System,
    }
}

fn has_glob_meta(path: &Path) -> bool {
    path.to_string_lossy()
        .chars()
        .any(|ch| matches!(ch, '*' | '?' | '['))
}

fn default_source_limit() -> u64 {
    SOURCE_LIMIT_DEFAULT
}

fn default_import_syntax() -> String {
    "shell_words".to_owned()
}

fn default_relative_to() -> String {
    "including_file".to_owned()
}

fn default_import_depth() -> usize {
    16
}

fn default_import_files() -> usize {
    128
}

fn default_command_timeout() -> u64 {
    COMMAND_TIMEOUT_DEFAULT_MS
}

fn default_write_target() -> String {
    "origin".to_owned()
}

fn default_decode_entrypoint() -> String {
    "decode".to_owned()
}

fn default_plan_entrypoint() -> String {
    "plan".to_owned()
}

fn default_true() -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use std::time::{SystemTime, UNIX_EPOCH};

    use crate::model::{EditCapability, SourceScope};
    use crate::transaction::Transaction;

    static ENVIRONMENT_LOCK: Mutex<()> = Mutex::new(());

    fn fixture(name: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "reginux-plugin-test-{}-{nonce}-{name}",
            std::process::id()
        ))
    }

    fn command_sandbox_available() -> bool {
        let program = PathBuf::from("/usr/bin/true");
        let invocation = CommandInvocation {
            expected_digest: sha256_file(&program, 64 * 1024 * 1024).unwrap(),
            program,
            args: Vec::new(),
            timeout_ms: 1_000,
            read_paths: Vec::new(),
            network: NetworkAccess::None,
        };
        match invoke_command(&invocation) {
            Ok(_) => true,
            Err(error)
                if error.to_string().contains("incompatible access-rights")
                    || error
                        .to_string()
                        .contains("Landlock policy was not fully enforced") =>
            {
                false
            }
            Err(error) => panic!("unexpected sandbox probe failure: {error}"),
        }
    }

    #[test]
    fn environment_expansion_is_whitelisted_and_absolute() {
        assert!(expand_source_path("${HOME}/.config/app.conf").is_ok());
        assert!(expand_source_path("${PATH}/app.conf").is_err());
        assert!(expand_source_path("relative.conf").is_err());
    }

    #[test]
    fn schema_parser_uses_last_assignment_and_strips_comments() {
        let text = "font_size 10\nfont_size 12 # effective\n";
        let parsed = crate::structured::find_value(text, "font_size", "kitty").unwrap();
        assert_eq!(parsed.as_deref(), Some("12"));
    }

    #[test]
    fn delimited_decoder_requires_exact_columns() {
        let config = DecoderConfig {
            delimiter: Some(":".to_owned()),
            columns: vec!["name".to_owned(), "id".to_owned()],
            collection: Some("items".to_owned()),
            ..DecoderConfig::default()
        };
        let decoded = decode_output("delimited", &config, b"alpha:1\nbeta:2\n", None).unwrap();
        assert_eq!(
            decoded.pointer("/items/1/id").and_then(Value::as_str),
            Some("2")
        );
        assert!(decode_output("delimited", &config, b"broken\n", None).is_err());
    }

    #[test]
    fn lua_sandbox_has_no_os_or_io_libraries() {
        let root = std::env::temp_dir().join(format!("reginux-lua-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        let script = root.join("transform.lua");
        fs::write(
            &script,
            "function decode(input) return { has_os = os ~= nil, has_io = io ~= nil, value = input.value } end",
        )
        .unwrap();
        let output = run_lua_function(&script, None, "decode", &json!({"value": 7})).unwrap();
        assert_eq!(output.get("has_os"), Some(&Value::Bool(false)));
        assert_eq!(output.get("has_io"), Some(&Value::Bool(false)));
        assert_eq!(output.get("value"), Some(&Value::Number(7.into())));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn lua_execution_rechecks_the_approved_script_digest() {
        let root = fixture("lua-digest");
        fs::create_dir_all(&root).unwrap();
        let script = root.join("transform.lua");
        fs::write(&script, "function decode() return {} end").unwrap();
        let error = run_lua_function(
            &script,
            Some("sha256:not-the-current-script"),
            "decode",
            &Value::Null,
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("changed after approval"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn builtin_decoders_cover_quoted_csv_ini_and_fixed_status() {
        let csv = decode_output(
            "csv",
            &DecoderConfig {
                headers: true,
                ..DecoderConfig::default()
            },
            b"id,name\n1,\"alpha,beta\"\n",
            None,
        )
        .unwrap();
        assert_eq!(
            csv.pointer("/0/name").and_then(Value::as_str),
            Some("alpha,beta")
        );

        let ini = decode_output(
            "ini",
            &DecoderConfig::default(),
            b"[service]\nstate=ready\n",
            None,
        )
        .unwrap();
        assert_eq!(
            ini.pointer("/service/state").and_then(Value::as_str),
            Some("ready")
        );

        let fixed = decode_output(
            "fixed_status",
            &DecoderConfig {
                pattern: Some(r"^(?P<name>[a-z]+):(?P<state>[a-z]+)$".to_owned()),
                ..DecoderConfig::default()
            },
            b"daemon:ready\n",
            None,
        )
        .unwrap();
        assert_eq!(
            fixed.pointer("/0/state").and_then(Value::as_str),
            Some("ready")
        );
    }

    #[test]
    fn schema_import_graph_writes_the_effective_origin() {
        let _environment = ENVIRONMENT_LOCK.lock().unwrap();
        let root = fixture("schema-import");
        let config = root.join("config");
        let application = config.join("example");
        let plugin = root.join("plugin");
        fs::create_dir_all(&application).unwrap();
        fs::create_dir_all(&plugin).unwrap();
        fs::write(
            application.join("main.conf"),
            "font_size 10\ninclude extra.conf\n",
        )
        .unwrap();
        fs::write(application.join("extra.conf"), "font_size 12 # effective\n").unwrap();
        fs::write(
            plugin.join("manifest.toml"),
            r#"
schema_version = 1
[plugin]
id = "org.reginux.test.schema"
name = "Schema Test"
version = "1.0.0"
kind = "schema"
[sources.main]
path = "${XDG_CONFIG_HOME}/example/main.conf"
format = "kitty"
scope = "user"
[sources.main.imports]
keyword = "include"
allowed_roots = ["${XDG_CONFIG_HOME}/example"]
[fields.appearance.font_size]
source = "main"
key = "font_size"
type = "integer"
write_target = "origin"
"#,
        )
        .unwrap();
        let old_home = std::env::var_os("HOME");
        let old_config = std::env::var_os("XDG_CONFIG_HOME");
        std::env::set_var("HOME", &root);
        std::env::set_var("XDG_CONFIG_HOME", &config);
        let loaded = load_plugin(
            &plugin,
            &plugin.join("manifest.toml"),
            &PluginPolicy::default(),
        );
        fs::write(
            application.join("extra.conf"),
            "include main.conf\nfont_size 12\n",
        )
        .unwrap();
        let cycle_error = load_plugin(
            &plugin,
            &plugin.join("manifest.toml"),
            &PluginPolicy::default(),
        )
        .unwrap_err()
        .to_string();
        assert!(cycle_error.contains("import cycle"));
        fs::write(application.join("extra.conf"), "font_size 12 # effective\n").unwrap();
        if let Some(value) = old_home {
            std::env::set_var("HOME", value);
        } else {
            std::env::remove_var("HOME");
        }
        if let Some(value) = old_config {
            std::env::set_var("XDG_CONFIG_HOME", value);
        } else {
            std::env::remove_var("XDG_CONFIG_HOME");
        }
        let (_, entries, _) = loaded.unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].value, "12");
        assert_eq!(
            entries[0].source_path(),
            Some(application.join("extra.conf").as_path())
        );
        let mut transaction = Transaction::default();
        transaction.stage_entry(&entries[0], "14").unwrap();
        let staged = String::from_utf8(
            transaction
                .content_for(&application.join("extra.conf"))
                .unwrap(),
        )
        .unwrap();
        assert!(staged.contains("font_size 14 # effective"));
        assert_eq!(
            fs::read_to_string(application.join("main.conf")).unwrap(),
            "font_size 10\ninclude extra.conf\n"
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn secret_type_is_masked_and_read_only_without_extra_flag() {
        let _environment = ENVIRONMENT_LOCK.lock().unwrap();
        let root = fixture("schema-secret");
        let plugin = root.join("plugin");
        fs::create_dir_all(&plugin).unwrap();
        fs::write(root.join("credentials.conf"), "token visible-secret\n").unwrap();
        fs::write(
            plugin.join("manifest.toml"),
            r#"
schema_version = 1
[plugin]
id = "org.reginux.test.secret"
name = "Secret Test"
version = "1.0.0"
kind = "schema"
[sources.main]
path = "${HOME}/credentials.conf"
format = "whitespace"
scope = "user"
[fields.credentials.token]
source = "main"
key = "token"
type = "secret"
"#,
        )
        .unwrap();
        let old_home = std::env::var_os("HOME");
        std::env::set_var("HOME", &root);
        let loaded = load_plugin(
            &plugin,
            &plugin.join("manifest.toml"),
            &PluginPolicy::default(),
        );
        if let Some(value) = old_home {
            std::env::set_var("HOME", value);
        } else {
            std::env::remove_var("HOME");
        }
        let (_, entries, _) = loaded.unwrap();
        assert_eq!(entries[0].value, "••••••");
        assert_eq!(entries[0].value_type, ValueType::Secret);
        assert_eq!(entries[0].edit_capability, EditCapability::None);
        assert!(!entries[0].searchable_text().contains("visible-secret"));
        assert!(Transaction::default()
            .stage_entry(&entries[0], "replacement")
            .is_err());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn adapter_requires_approval_and_projects_stable_resource_ids() {
        let root = fixture("adapter");
        fs::create_dir_all(&root).unwrap();
        let manifest = root.join("manifest.toml");
        fs::write(
            &manifest,
            r#"
schema_version = 1
[plugin]
id = "org.reginux.test.adapter"
name = "Adapter Test"
version = "1.0.0"
kind = "adapter"
[transport]
kind = "command"
program = "/usr/bin/printf"
[operations.snapshot]
args = ['[{"id":"stable","name":"Visible","enabled":true}]']
decoder = "json"
[operations.snapshot.decoder_config]
collection = "items"
[[nodes]]
id = "item"
kind = "resource"
label = "/name"
collection = "/items"
key = "/id"
[[nodes.fields]]
id = "enabled"
label = "Enabled"
type = "boolean"
value = "/enabled"
read_only = true
"#,
        )
        .unwrap();
        let (_, disabled_entries, summary) =
            load_plugin(&root, &manifest, &PluginPolicy::default()).unwrap();
        assert!(disabled_entries.is_empty());
        assert_eq!(summary.approval, "required");

        if !command_sandbox_available() {
            fs::remove_dir_all(root).unwrap();
            return;
        }

        let mut policy = PluginPolicy::default();
        policy
            .temporary_approvals
            .insert("org.reginux.test.adapter".to_owned());
        let (_, entries, summary) = load_plugin(&root, &manifest, &policy).unwrap();
        assert_eq!(summary.approval, "approved");
        assert_eq!(entries.len(), 1);
        assert_eq!(
            entries[0].id,
            "org.reginux.test.adapter.item.stable.enabled"
        );
        assert_eq!(entries[0].value, "true");
        assert!(entries[0].source_path().is_none());
        assert_eq!(entries[0].edit_capability, EditCapability::None);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn editable_adapter_manifest_requires_precondition_and_verification() {
        let text = r#"
schema_version = 1
[plugin]
id = "org.reginux.test.plan"
name = "Plan Test"
version = "1.0.0"
kind = "adapter"
[transport]
kind = "command"
program = "/usr/bin/true"
[operations.snapshot]
args = []
decoder = "json"
[operations.set]
args = ["${value}"]
[[nodes]]
id = "item"
kind = "resource"
collection = "/items"
key = "/id"
[[nodes.fields]]
id = "enabled"
label = "Enabled"
type = "boolean"
value = "/enabled"
operation = "set"
"#;
        let manifest: Manifest = toml::from_str(text).unwrap();
        let error = validate_manifest(&manifest).unwrap_err().to_string();
        assert!(error.contains("requires precondition"));
    }

    #[test]
    fn adapter_commands_receive_only_the_safe_environment() {
        if !command_sandbox_available() {
            return;
        }
        let program = PathBuf::from("/usr/bin/env");
        let invocation = CommandInvocation {
            expected_digest: sha256_file(&program, 64 * 1024 * 1024).unwrap(),
            program,
            args: Vec::new(),
            timeout_ms: 1_000,
            read_paths: Vec::new(),
            network: NetworkAccess::None,
        };
        let output = String::from_utf8(invoke_command(&invocation).unwrap()).unwrap();
        assert!(output.contains("PATH=/usr/bin:/bin"));
        assert!(output.contains("LANG=C.UTF-8"));
        assert!(!output.contains("HOME="));
        assert!(!output.contains("TOKEN="));
    }

    #[test]
    fn transform_plan_is_revalidated_before_staging() {
        let root = fixture("transform-plan");
        fs::create_dir_all(&root).unwrap();
        let source = root.join("init.lua");
        let script = root.join("transform.lua");
        fs::write(&source, "vim.o.tabstop = 4\n").unwrap();
        fs::write(
            &script,
            r#"
function plan(input)
  local text = input.sources.init_lua
  local start_pos, end_pos = text:find("%d+")
  return { edits = {{
    source_id = "init_lua",
    expected_sha256 = input.expected_sha256,
    start = start_pos - 1,
    ["end"] = end_pos,
    replacement = tostring(input.value),
  }} }
end
"#,
        )
        .unwrap();
        let entry = ConfigEntry {
            id: "org.reginux.test.transform.editor.tabstop".to_owned(),
            label: "Tab width".to_owned(),
            section: "Applications / Test".to_owned(),
            description: String::new(),
            value: "4".to_owned(),
            default_value: None,
            value_type: ValueType::Integer,
            source: SourceRef::file("init_lua", source.clone(), SourceScope::User),
            edit_capability: EditCapability::Transform,
            privilege: Privilege::User,
            provider: "plugin.test".to_owned(),
            validation: "integer".to_owned(),
            backend: Backend::TransformField {
                path: source.clone(),
                source_id: "init_lua".to_owned(),
                plugin_id: "org.reginux.test.transform".to_owned(),
                expected_script_digest: sha256_file(&script, MANIFEST_LIMIT).unwrap(),
                script,
                plan_entrypoint: "plan".to_owned(),
                binding: "editor.tabstop".to_owned(),
                start: 16,
                end: 17,
                expected_digest: sha256_file(&source, SOURCE_LIMIT_DEFAULT).unwrap(),
            },
            metadata: Vec::new(),
        };
        let mut transaction = Transaction::default();
        transaction.stage_entry(&entry, "2").unwrap();
        assert_eq!(
            String::from_utf8(transaction.content_for(&source).unwrap()).unwrap(),
            "vim.o.tabstop = 2\n"
        );
        assert!(transaction.diff().contains("+vim.o.tabstop = 2"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn adapter_changes_use_the_shared_transaction_and_diff() {
        let program = PathBuf::from("/usr/bin/true");
        let binding = AdapterBinding {
            plugin_id: "org.reginux.test.adapter".to_owned(),
            operation_id: "enable".to_owned(),
            invocation: AdapterInvocation::Command(CommandInvocation {
                expected_digest: sha256_file(&program, 64 * 1024 * 1024).unwrap(),
                program: program.clone(),
                args: vec!["${value}".to_owned()],
                timeout_ms: 1_000,
                read_paths: Vec::new(),
                network: NetworkAccess::None,
            }),
            precondition: Some(AdapterVerification {
                invocation: AdapterInvocation::Command(CommandInvocation {
                    expected_digest: sha256_file(&program, 64 * 1024 * 1024).unwrap(),
                    program: program.clone(),
                    args: Vec::new(),
                    timeout_ms: 1_000,
                    read_paths: Vec::new(),
                    network: NetworkAccess::None,
                }),
                expected_stdout: String::new(),
            }),
            validation: None,
            compensation: None,
            verification: Some(AdapterVerification {
                invocation: AdapterInvocation::Command(CommandInvocation {
                    expected_digest: sha256_file(&program, 64 * 1024 * 1024).unwrap(),
                    program: program.clone(),
                    args: Vec::new(),
                    timeout_ms: 1_000,
                    read_paths: Vec::new(),
                    network: NetworkAccess::None,
                }),
                expected_stdout: String::new(),
            }),
            guarantee: TransactionGuarantee::BestEffort,
            scope: SourceScope::User,
        };
        let entry = ConfigEntry {
            id: "org.reginux.test.adapter.resource.enabled".to_owned(),
            label: "Enabled".to_owned(),
            section: "Applications / Test".to_owned(),
            description: String::new(),
            value: "false".to_owned(),
            default_value: None,
            value_type: ValueType::Boolean,
            source: SourceRef::Command {
                plugin_id: binding.plugin_id.clone(),
                operation_id: "snapshot".to_owned(),
                program,
            },
            edit_capability: EditCapability::Adapter,
            privilege: Privilege::User,
            provider: "plugin.test".to_owned(),
            validation: "boolean".to_owned(),
            backend: Backend::AdapterField {
                binding: Box::new(binding),
            },
            metadata: Vec::new(),
        };
        let mut transaction = Transaction::default();
        transaction.stage_entry(&entry, "true").unwrap();
        assert!(transaction.diff().contains("best-effort; user scope"));
        if !command_sandbox_available() {
            return;
        }
        let report = transaction.apply(false).unwrap();
        assert_eq!(report.adapter_operations, vec![entry.id]);
        assert_eq!(transaction.changed_count(), 0);
    }

    #[cfg(unix)]
    #[test]
    fn adapter_executable_digest_is_checked_again_at_execution() {
        use std::os::unix::fs::PermissionsExt;

        let root = fixture("adapter-digest");
        fs::create_dir_all(&root).unwrap();
        let program = root.join("adapter.sh");
        fs::write(&program, "#!/bin/sh\nprintf first\n").unwrap();
        fs::set_permissions(&program, fs::Permissions::from_mode(0o700)).unwrap();
        let invocation = CommandInvocation {
            expected_digest: sha256_file(&program, MANIFEST_LIMIT).unwrap(),
            program: program.clone(),
            args: Vec::new(),
            timeout_ms: 1_000,
            read_paths: Vec::new(),
            network: NetworkAccess::None,
        };
        let sandbox_available = command_sandbox_available();
        if sandbox_available {
            assert_eq!(invoke_command(&invocation).unwrap(), b"first");
        }
        fs::write(&program, "#!/bin/sh\nprintf changed\n").unwrap();
        let error = invoke_command(&invocation).unwrap_err().to_string();
        assert!(error.contains("changed after approval"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn unix_socket_transport_checks_peer_and_framing() {
        use std::os::unix::net::UnixListener;

        let root = fixture("socket");
        fs::create_dir_all(&root).unwrap();
        let endpoint = root.join("adapter.sock");
        let listener = match UnixListener::bind(&endpoint) {
            Ok(listener) => listener,
            Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
                fs::remove_dir_all(root).unwrap();
                return;
            }
            Err(error) => panic!("bind test Unix socket: {error}"),
        };
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = Vec::new();
            loop {
                let mut byte = [0_u8; 1];
                stream.read_exact(&mut byte).unwrap();
                if byte[0] == b'\n' {
                    break;
                }
                request.push(byte[0]);
            }
            assert_eq!(request, b"status");
            stream.write_all(b"{\"ok\":true}\n").unwrap();
        });
        let invocation = SocketInvocation {
            endpoint: endpoint.clone(),
            request: "status".to_owned(),
            framing: SocketFraming::Line,
            expected_peer_uid: unsafe { libc::geteuid() },
            timeout_ms: 1_000,
        };
        assert_eq!(invoke_socket(&invocation).unwrap(), b"{\"ok\":true}");
        server.join().unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn adapter_ipc_limits_requests_and_decoded_outputs() {
        let oversized_request = SocketInvocation {
            endpoint: PathBuf::from("/does/not/connect.sock"),
            request: "x".repeat(COMMAND_OUTPUT_LIMIT + 1),
            framing: SocketFraming::Line,
            expected_peer_uid: unsafe { libc::geteuid() },
            timeout_ms: 100,
        };
        let request_error = invoke_socket(&oversized_request).unwrap_err();
        assert!(request_error
            .to_string()
            .contains("Unix socket request exceeds the 1 MiB limit"));

        assert_eq!(
            bounded_adapter_output(vec![0_u8; COMMAND_OUTPUT_LIMIT], "test output").unwrap(),
            vec![0_u8; COMMAND_OUTPUT_LIMIT]
        );
        let output_error =
            bounded_adapter_output(vec![0_u8; COMMAND_OUTPUT_LIMIT + 1], "test output")
                .unwrap_err();
        assert!(output_error
            .to_string()
            .contains("test output exceeds the 1 MiB limit"));
    }

    #[test]
    fn sensitive_adapter_fields_omit_raw_response_metadata() {
        let manifest = Manifest {
            schema_version: 1,
            plugin: PluginSpec {
                id: "org.reginux.test.adapter".to_owned(),
                name: "Adapter".to_owned(),
                version: "1".to_owned(),
                kind: PluginKind::Adapter,
                description: None,
            },
            sources: BTreeMap::new(),
            fields: BTreeMap::new(),
            nodes: vec![NodeSpec {
                id: "resource".to_owned(),
                kind: "resource".to_owned(),
                label: None,
                parent: None,
                collection: None,
                key: Some("/key".to_owned()),
                description: None,
                fields: vec![NodeFieldSpec {
                    id: "token".to_owned(),
                    label: "Token".to_owned(),
                    value_type: "secret".to_owned(),
                    value: "/token".to_owned(),
                    description: None,
                    operation: None,
                    binding: None,
                    read_only: true,
                    sensitive: false,
                    values: Vec::new(),
                }],
            }],
            transport: None,
            operations: BTreeMap::new(),
            transform: None,
        };
        assert!(adapter_raw_response_metadata(&manifest, b"visible-secret").is_none());
    }

    #[test]
    fn compensatable_socket_operation_compensates_after_invocation_failure() {
        use std::os::unix::net::UnixListener;
        use std::sync::mpsc;

        let root = fixture("socket-compensation");
        fs::create_dir_all(&root).unwrap();
        let endpoint = root.join("adapter.sock");
        let listener = match UnixListener::bind(&endpoint) {
            Ok(listener) => listener,
            Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
                fs::remove_dir_all(root).unwrap();
                return;
            }
            Err(error) => panic!("bind compensation test Unix socket: {error}"),
        };
        let (requests_tx, requests_rx) = mpsc::channel();
        let server = thread::spawn(move || {
            let (mut first, _) = listener.accept().unwrap();
            let mut first_request = Vec::new();
            let mut byte = [0_u8; 1];
            loop {
                first.read_exact(&mut byte).unwrap();
                if byte[0] == b'\n' {
                    break;
                }
                first_request.push(byte[0]);
            }
            requests_tx.send(first_request).unwrap();
            thread::sleep(Duration::from_millis(150));
            drop(first);

            let (mut second, _) = listener.accept().unwrap();
            let mut second_request = Vec::new();
            loop {
                second.read_exact(&mut byte).unwrap();
                if byte[0] == b'\n' {
                    break;
                }
                second_request.push(byte[0]);
            }
            requests_tx.send(second_request).unwrap();
            second.write_all(b"restored\n").unwrap();
        });
        let socket = |request: &str| {
            AdapterInvocation::Socket(SocketInvocation {
                endpoint: endpoint.clone(),
                request: request.to_owned(),
                framing: SocketFraming::Line,
                expected_peer_uid: unsafe { libc::geteuid() },
                timeout_ms: 50,
            })
        };
        let binding = AdapterBinding {
            plugin_id: "test".to_owned(),
            operation_id: "apply".to_owned(),
            invocation: socket("apply ${value}"),
            precondition: None,
            validation: None,
            compensation: Some(socket("restore ${old_value}")),
            verification: None,
            guarantee: TransactionGuarantee::Compensatable,
            scope: SourceScope::User,
        };

        let error = execute_adapter_binding(&binding, "old", "new")
            .expect_err("primary invocation should time out");
        assert!(error.to_string().contains("failed during invocation"));
        assert_eq!(requests_rx.recv().unwrap(), b"apply new");
        assert_eq!(requests_rx.recv().unwrap(), b"restore old");
        server.join().unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn dbus_transport_calls_typed_method() {
        let _environment = ENVIRONMENT_LOCK.lock().unwrap();
        let output = Command::new("/usr/bin/dbus-daemon")
            .args(["--session", "--fork", "--print-address=1", "--print-pid=1"])
            .output()
            .unwrap();
        if !output.status.success()
            && String::from_utf8_lossy(&output.stderr).contains("Operation not permitted")
        {
            return;
        }
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let details = String::from_utf8(output.stdout).unwrap();
        let mut lines = details.lines();
        let address = lines.next().unwrap().to_owned();
        let pid = lines.next().unwrap().parse::<i32>().unwrap();
        let old = std::env::var_os("DBUS_SESSION_BUS_ADDRESS");
        std::env::set_var("DBUS_SESSION_BUS_ADDRESS", &address);
        let invocation = DBusInvocation {
            bus: BusKind::Session,
            service: "org.freedesktop.DBus".to_owned(),
            object_path: "/org/freedesktop/DBus".to_owned(),
            interface: "org.freedesktop.DBus".to_owned(),
            member: "ListNames".to_owned(),
            args: Vec::new(),
            arg_types: Vec::new(),
            reply_type: "string_array".to_owned(),
            timeout_ms: 1_000,
        };
        let result = invoke_dbus(&invocation);
        unsafe { libc::kill(pid, libc::SIGTERM) };
        if let Some(old) = old {
            std::env::set_var("DBUS_SESSION_BUS_ADDRESS", old);
        } else {
            std::env::remove_var("DBUS_SESSION_BUS_ADDRESS");
        }
        let decoded: Value = serde_json::from_slice(&result.unwrap()).unwrap();
        assert!(decoded
            .as_array()
            .unwrap()
            .iter()
            .any(|name| name == "org.freedesktop.DBus"));
    }

    #[test]
    fn bundled_examples_are_valid_v1_plugins() {
        let _environment = ENVIRONMENT_LOCK.lock().unwrap();
        let examples = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../plugins/examples");
        let discovery =
            discover_plugins(&[examples.display().to_string()], &PluginPolicy::default());
        let ids = discovery
            .summaries
            .iter()
            .map(|plugin| plugin.id.as_str())
            .collect::<HashSet<_>>();
        assert!(ids.contains("org.reginux.example.kitty"));
        assert!(ids.contains("org.reginux.example.clock"));
        assert!(ids.contains("org.reginux.example.neovim"));
        assert!(ids.contains("org.reginux.example.dbus"));
        assert!(ids.contains("org.reginux.example.socket"));
        assert!(
            discovery
                .summaries
                .iter()
                .filter(|plugin| plugin.kind != "schema")
                .all(|plugin| plugin.approval == "required"),
            "{:#?}",
            discovery.summaries
        );
    }
}
