use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

const MAX_DISPLAY_TEXT: usize = 512;

/// Remove terminal control characters from text before it crosses into a UI.
/// Newlines are intentionally removed too: callers rendering multi-line
/// content should use a format-aware sanitizer rather than treating arbitrary
/// plugin text as terminal markup.
pub fn clean_display_text(value: &str) -> String {
    value
        .chars()
        .filter(|ch| !ch.is_control() && *ch != '\u{1b}')
        .take(MAX_DISPLAY_TEXT)
        .collect()
}

/// The semantic type presented by a provider or schema plugin.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum ValueType {
    Boolean,
    Integer,
    Float,
    String,
    Enum,
    Path,
    List,
    Raw,
    Secret,
}

impl ValueType {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Boolean => "boolean",
            Self::Integer => "integer",
            Self::Float => "float",
            Self::String => "string",
            Self::Enum => "enum",
            Self::Path => "path",
            Self::List => "list",
            Self::Raw => "raw",
            Self::Secret => "secret",
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum SourceScope {
    User,
    System,
}

/// A declared configuration source shown by the file-oriented browser.
///
/// This is intentionally derived only from built-in providers and plugin
/// manifests.  It is not a directory scan result, so a missing declared file
/// can still be selected and created through the normal transaction flow.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConfigFile {
    pub path: PathBuf,
    pub scope: SourceScope,
    pub providers: Vec<String>,
    pub entry_count: usize,
    pub exists: bool,
    pub editable: bool,
}

impl ConfigFile {
    pub fn display_path(&self) -> String {
        clean_display_text(&self.path.display().to_string())
    }
}

impl SourceScope {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::User => "user",
            Self::System => "system",
        }
    }
}

/// A typed reference to the real source of a value.
///
/// Runtime observations deliberately do not masquerade as filesystem paths.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum SourceRef {
    File {
        source_id: String,
        path: PathBuf,
        scope: SourceScope,
        imported_from: Option<PathBuf>,
    },
    Command {
        plugin_id: String,
        operation_id: String,
        program: PathBuf,
    },
    DBus {
        plugin_id: String,
        bus: String,
        service: String,
        object_path: String,
        interface: String,
    },
    Socket {
        plugin_id: String,
        endpoint_id: String,
    },
}

impl SourceRef {
    pub fn file(source_id: impl Into<String>, path: PathBuf, scope: SourceScope) -> Self {
        Self::File {
            source_id: source_id.into(),
            path,
            scope,
            imported_from: None,
        }
    }

    pub fn path(&self) -> Option<&PathBuf> {
        match self {
            Self::File { path, .. } => Some(path),
            _ => None,
        }
    }

    pub fn display(&self) -> String {
        let display = match self {
            Self::File {
                source_id,
                path,
                imported_from,
                ..
            } => imported_from.as_ref().map_or_else(
                || format!("{source_id}:{}", path.display()),
                |parent| {
                    format!(
                        "{source_id}:{} (imported by {})",
                        path.display(),
                        parent.display()
                    )
                },
            ),
            Self::Command {
                operation_id,
                program,
                ..
            } => format!("command:{operation_id} ({})", program.display()),
            Self::DBus {
                service,
                object_path,
                interface,
                ..
            } => format!("dbus:{service}{object_path}#{interface}"),
            Self::Socket { endpoint_id, .. } => format!("socket:{endpoint_id}"),
        };
        clean_display_text(&display)
    }

    pub fn is_system(&self) -> bool {
        matches!(
            self,
            Self::File {
                scope: SourceScope::System,
                ..
            }
        )
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum EditCapability {
    None,
    File,
    Adapter,
    Transform,
}

impl EditCapability {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::None => "none",
            Self::File => "file",
            Self::Adapter => "adapter",
            Self::Transform => "transform",
        }
    }

    pub fn is_editable(&self) -> bool {
        !matches!(self, Self::None)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum TransactionGuarantee {
    Atomic,
    Compensatable,
    BestEffort,
    Irreversible,
}

impl TransactionGuarantee {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Atomic => "atomic",
            Self::Compensatable => "compensatable",
            Self::BestEffort => "best-effort",
            Self::Irreversible => "irreversible",
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum NetworkAccess {
    None,
    Local,
    Internet,
}

impl NetworkAccess {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Local => "local",
            Self::Internet => "internet",
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CommandInvocation {
    pub program: PathBuf,
    pub expected_digest: String,
    pub args: Vec<String>,
    pub timeout_ms: u64,
    pub read_paths: Vec<PathBuf>,
    pub network: NetworkAccess,
}

#[derive(Clone, Debug)]
pub enum BusKind {
    Session,
    System,
}

impl BusKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Session => "session",
            Self::System => "system",
        }
    }
}

#[derive(Clone, Debug)]
pub struct DBusInvocation {
    pub bus: BusKind,
    pub service: String,
    pub object_path: String,
    pub interface: String,
    pub member: String,
    pub args: Vec<String>,
    pub arg_types: Vec<ValueType>,
    pub reply_type: String,
    pub timeout_ms: u64,
}

#[derive(Clone, Debug)]
pub enum SocketFraming {
    Eof,
    Line,
    LengthPrefixed,
}

impl SocketFraming {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Eof => "eof",
            Self::Line => "line",
            Self::LengthPrefixed => "length_prefixed",
        }
    }
}

#[derive(Clone, Debug)]
pub struct SocketInvocation {
    pub endpoint: PathBuf,
    pub request: String,
    pub framing: SocketFraming,
    pub expected_peer_uid: u32,
    pub timeout_ms: u64,
}

#[derive(Clone, Debug)]
pub enum AdapterInvocation {
    Command(CommandInvocation),
    DBus(DBusInvocation),
    Socket(SocketInvocation),
}

impl AdapterInvocation {
    pub fn transport_name(&self) -> &'static str {
        match self {
            Self::Command(_) => "command",
            Self::DBus(_) => "dbus",
            Self::Socket(_) => "unix_socket",
        }
    }
}

#[derive(Clone, Debug)]
pub struct AdapterVerification {
    pub invocation: AdapterInvocation,
    pub expected_stdout: String,
}

#[derive(Clone, Debug)]
pub struct AdapterBinding {
    pub plugin_id: String,
    pub operation_id: String,
    pub invocation: AdapterInvocation,
    pub precondition: Option<AdapterVerification>,
    pub validation: Option<AdapterVerification>,
    pub compensation: Option<AdapterInvocation>,
    pub verification: Option<AdapterVerification>,
    pub guarantee: TransactionGuarantee,
    pub scope: SourceScope,
}

/// The privilege boundary that applies to an entry.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum Privilege {
    User,
    System,
    ReadOnly,
}

impl Privilege {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::User => "user",
            Self::System => "system",
            Self::ReadOnly => "read-only",
        }
    }
}

/// How an entry maps back to a source or a declared operation.
///
/// This is intentionally a model-level type.  A frontend never needs to know
/// whether an entry came from `/etc/locale.conf`, a schema plugin, or a
/// Adapter or Transform plugin in order to render it.
#[derive(Clone, Debug)]
pub enum Backend {
    KeyValue {
        path: PathBuf,
        key: String,
        separator: KeyValueSeparator,
    },
    SchemaField {
        path: PathBuf,
        source_id: String,
        key: String,
        format: String,
        plugin_id: String,
        insert: Option<String>,
    },
    TomlField {
        path: PathBuf,
        section: Option<String>,
        key: String,
        value_type: ValueType,
    },
    WholeFile {
        path: PathBuf,
    },
    TransformField {
        path: PathBuf,
        source_id: String,
        plugin_id: String,
        script: PathBuf,
        expected_script_digest: String,
        plan_entrypoint: String,
        binding: String,
        start: usize,
        end: usize,
        expected_digest: String,
    },
    AdapterField {
        binding: Box<AdapterBinding>,
    },
    ReadOnly {
        reason: String,
    },
}

impl Backend {
    pub fn path(&self) -> Option<&PathBuf> {
        match self {
            Self::KeyValue { path, .. }
            | Self::SchemaField { path, .. }
            | Self::TransformField { path, .. }
            | Self::TomlField { path, .. }
            | Self::WholeFile { path } => Some(path),
            Self::AdapterField { .. } | Self::ReadOnly { .. } => None,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum KeyValueSeparator {
    Equals,
    Whitespace,
}

/// A single logical configuration item.
#[derive(Clone, Debug)]
pub struct ConfigEntry {
    pub id: String,
    pub label: String,
    pub section: String,
    pub description: String,
    pub value: String,
    pub default_value: Option<String>,
    pub value_type: ValueType,
    pub source: SourceRef,
    pub edit_capability: EditCapability,
    pub privilege: Privilege,
    pub provider: String,
    pub validation: String,
    pub backend: Backend,
    pub metadata: Vec<(String, String)>,
}

impl ConfigEntry {
    pub fn source_display(&self) -> String {
        self.source.display()
    }

    pub fn source_path(&self) -> Option<&Path> {
        self.source.path().map(PathBuf::as_path)
    }

    pub fn is_editable(&self) -> bool {
        self.edit_capability.is_editable()
    }

    pub fn searchable_text(&self) -> String {
        format!(
            "{} {} {} {} {} {}",
            self.id,
            self.label,
            self.description,
            self.source.display(),
            self.provider,
            self.section
        )
        .to_lowercase()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_text_removes_terminal_controls_and_applies_limit() {
        let value = format!("safe\u{1b}[31m\n{}", "x".repeat(MAX_DISPLAY_TEXT));
        let cleaned = clean_display_text(&value);
        assert!(!cleaned.contains('\u{1b}'));
        assert!(!cleaned.contains('\n'));
        assert_eq!(cleaned.len(), MAX_DISPLAY_TEXT);
    }

    #[test]
    fn source_display_cleans_manifest_control_characters() {
        let source = SourceRef::DBus {
            plugin_id: "org.example".to_owned(),
            bus: "session".to_owned(),
            service: "svc\u{1b}[2J".to_owned(),
            object_path: "/object".to_owned(),
            interface: "iface".to_owned(),
        };
        assert!(!source.display().contains('\u{1b}'));
    }
}
