//! Reginux's frontend-independent core.
//!
//! The core deliberately contains no terminal UI code.  It owns discovery,
//! configuration models, plugins, staged transactions, validation and safe
//! filesystem writes so that a future CLI or GUI can reuse the same behavior.

pub mod catalog;
pub mod config;
pub mod filesystem;
pub mod keybindings;
pub mod model;
pub mod plugin;
pub mod privileged;
pub mod provider;
pub mod sandbox;
pub mod structured;
pub mod transaction;

pub use catalog::{Catalog, DiscoverOptions, PluginSummary};
pub use config::{load_app_config, reset_keybindings, AppConfig, ConfigLoad};
pub use keybindings::{Action, KeySequence, KeyStroke, Keymap};
pub use model::{
    clean_display_text, Backend, ConfigEntry, ConfigFile, EditCapability, Privilege, SourceRef,
    SourceScope, TransactionGuarantee, ValueType,
};
pub use transaction::{
    AdapterChangeSummary, ApplyReport, ChangeSummary, Transaction, ValidationIssue,
};
