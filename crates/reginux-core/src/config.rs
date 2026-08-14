use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::filesystem::{atomic_write, config_dir};

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct AppConfig {
    pub interface: InterfaceConfig,
    pub editor: EditorConfig,
    pub safety: SafetyConfig,
    pub plugins: PluginConfig,
    pub keybindings: KeybindingsConfig,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct InterfaceConfig {
    pub show_key_hints: bool,
    pub key_sequence_timeout_ms: u64,
    pub confirm_before_apply: bool,
    pub default_view: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct EditorConfig {
    /// A shell-like argument string. `{file}` is replaced without invoking a shell.
    pub command: String,
    pub use_environment_editor: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct SafetyConfig {
    pub backup_before_apply: bool,
    pub allow_system_writes: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct PluginConfig {
    pub directories: Vec<String>,
    /// Approval is bound to the digest displayed by the plugin manager.
    pub approved_adapters: BTreeMap<String, String>,
    pub approved_transforms: BTreeMap<String, String>,
}

/// Kept as explicit contexts so the TOML file is pleasant to hand-edit:
/// `[keybindings.browser]`, `[keybindings.global]`, and so on.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct KeybindingsConfig {
    pub global: BTreeMap<String, String>,
    pub browser: BTreeMap<String, String>,
    pub form: BTreeMap<String, String>,
    pub raw: BTreeMap<String, String>,
    pub diff: BTreeMap<String, String>,
    pub info: BTreeMap<String, String>,
    pub help: BTreeMap<String, String>,
    pub search: BTreeMap<String, String>,
    pub dialog: BTreeMap<String, String>,
    pub plugin_manager: BTreeMap<String, String>,
}

impl Default for InterfaceConfig {
    fn default() -> Self {
        Self {
            show_key_hints: true,
            key_sequence_timeout_ms: 500,
            confirm_before_apply: true,
            default_view: "form".to_owned(),
        }
    }
}

impl Default for EditorConfig {
    fn default() -> Self {
        Self {
            command: "vim {file}".to_owned(),
            use_environment_editor: false,
        }
    }
}

impl Default for SafetyConfig {
    fn default() -> Self {
        Self {
            backup_before_apply: true,
            allow_system_writes: false,
        }
    }
}

impl Default for PluginConfig {
    fn default() -> Self {
        Self {
            directories: vec![
                "~/.local/share/reginux/plugins".to_owned(),
                "/usr/share/reginux/plugins".to_owned(),
            ],
            approved_adapters: BTreeMap::new(),
            approved_transforms: BTreeMap::new(),
        }
    }
}

impl Default for KeybindingsConfig {
    fn default() -> Self {
        let mut global = BTreeMap::new();
        global.insert("Esc".to_owned(), "application.back".to_owned());
        global.insert("/".to_owned(), "search.open".to_owned());
        global.insert("n".to_owned(), "search.next".to_owned());
        global.insert("N".to_owned(), "search.previous".to_owned());
        global.insert("Tab".to_owned(), "view.next".to_owned());
        global.insert("Ctrl+s".to_owned(), "changes.apply".to_owned());
        global.insert("u".to_owned(), "changes.undo".to_owned());
        global.insert("?".to_owned(), "help.keybindings".to_owned());
        global.insert("]s".to_owned(), "scope.next".to_owned());
        global.insert("[s".to_owned(), "scope.previous".to_owned());
        global.insert("q".to_owned(), "application.back".to_owned());
        global.insert("Q".to_owned(), "application.quit".to_owned());

        let mut browser = BTreeMap::new();
        browser.insert("j".to_owned(), "navigation.down".to_owned());
        browser.insert("Down".to_owned(), "navigation.down".to_owned());
        browser.insert("k".to_owned(), "navigation.up".to_owned());
        browser.insert("Up".to_owned(), "navigation.up".to_owned());
        browser.insert("h".to_owned(), "navigation.left".to_owned());
        browser.insert("Left".to_owned(), "navigation.left".to_owned());
        browser.insert("l".to_owned(), "navigation.right".to_owned());
        browser.insert("Right".to_owned(), "navigation.right".to_owned());
        browser.insert("gg".to_owned(), "navigation.top".to_owned());
        browser.insert("G".to_owned(), "navigation.bottom".to_owned());
        browser.insert("Ctrl+u".to_owned(), "navigation.page_up".to_owned());
        browser.insert("Ctrl+d".to_owned(), "navigation.page_down".to_owned());
        browser.insert("Enter".to_owned(), "navigation.activate".to_owned());
        browser.insert("e".to_owned(), "config.edit".to_owned());
        browser.insert("r".to_owned(), "config.reload".to_owned());
        browser.insert("d".to_owned(), "view.diff".to_owned());
        browser.insert("i".to_owned(), "view.info".to_owned());
        browser.insert("p".to_owned(), "view.plugins".to_owned());

        let mut form = BTreeMap::new();
        form.insert("e".to_owned(), "config.edit".to_owned());
        form.insert("r".to_owned(), "config.reload".to_owned());
        form.insert("d".to_owned(), "view.diff".to_owned());
        form.insert("i".to_owned(), "view.info".to_owned());
        form.insert("Enter".to_owned(), "navigation.activate".to_owned());

        let mut raw = BTreeMap::new();
        raw.insert("e".to_owned(), "config.edit".to_owned());
        raw.insert("r".to_owned(), "config.reload".to_owned());
        raw.insert("d".to_owned(), "view.diff".to_owned());
        add_scroll_bindings(&mut raw);

        let mut diff = BTreeMap::new();
        diff.insert("]c".to_owned(), "navigation.down".to_owned());
        diff.insert("[c".to_owned(), "navigation.up".to_owned());
        add_scroll_bindings(&mut diff);

        let mut info = BTreeMap::new();
        add_scroll_bindings(&mut info);

        let mut help = BTreeMap::new();
        add_scroll_bindings(&mut help);

        let mut search = BTreeMap::new();
        search.insert("Enter".to_owned(), "navigation.activate".to_owned());

        let mut dialog = BTreeMap::new();
        dialog.insert("Enter".to_owned(), "navigation.activate".to_owned());

        let mut plugin_manager = BTreeMap::new();
        add_scroll_bindings(&mut plugin_manager);
        plugin_manager.insert("a".to_owned(), "plugin.approve".to_owned());
        plugin_manager.insert("x".to_owned(), "plugin.revoke".to_owned());
        plugin_manager.insert("r".to_owned(), "config.reload".to_owned());

        Self {
            global,
            browser,
            form,
            raw,
            diff,
            info,
            help,
            search,
            dialog,
            plugin_manager,
        }
    }
}

impl KeybindingsConfig {
    pub fn contexts(&self) -> Vec<(&'static str, &BTreeMap<String, String>)> {
        vec![
            ("global", &self.global),
            ("browser", &self.browser),
            ("form", &self.form),
            ("raw", &self.raw),
            ("diff", &self.diff),
            ("info", &self.info),
            ("help", &self.help),
            ("search", &self.search),
            ("dialog", &self.dialog),
            ("plugin_manager", &self.plugin_manager),
        ]
    }
}

fn add_scroll_bindings(map: &mut BTreeMap<String, String>) {
    map.insert("j".to_owned(), "navigation.down".to_owned());
    map.insert("Down".to_owned(), "navigation.down".to_owned());
    map.insert("k".to_owned(), "navigation.up".to_owned());
    map.insert("Up".to_owned(), "navigation.up".to_owned());
    map.insert("Ctrl+d".to_owned(), "navigation.page_down".to_owned());
    map.insert("Ctrl+u".to_owned(), "navigation.page_up".to_owned());
}

pub struct ConfigLoad {
    pub config: AppConfig,
    pub path: PathBuf,
    pub warning: Option<String>,
}

pub fn load_app_config(safe: bool) -> ConfigLoad {
    let path = config_dir().join("config.toml");
    if safe || !path.exists() {
        return ConfigLoad {
            config: AppConfig::default(),
            path,
            warning: None,
        };
    }

    match fs::read_to_string(&path)
        .with_context(|| format!("read {}", path.display()))
        .and_then(|text| toml::from_str::<AppConfig>(&text).context("parse TOML"))
        .and_then(|config| {
            validate_app_config(&config)?;
            Ok(config)
        }) {
        Ok(config) => ConfigLoad {
            config,
            path,
            warning: None,
        },
        Err(error) => ConfigLoad {
            config: AppConfig::default(),
            path: path.clone(),
            warning: Some(format!(
                "Configuration error in {}: {error}. Started with defaults.",
                path.display()
            )),
        },
    }
}

pub fn save_app_config(config: &AppConfig) -> Result<PathBuf> {
    validate_app_config(config)?;
    let path = config_dir().join("config.toml");
    let contents = toml::to_string_pretty(config).context("serialize Reginux configuration")?;
    atomic_write(&path, contents.as_bytes(), None)?;
    Ok(path)
}

pub fn validate_app_config(config: &AppConfig) -> Result<()> {
    if !(50..=5000).contains(&config.interface.key_sequence_timeout_ms) {
        bail!("interface.key_sequence_timeout_ms must be between 50 and 5000");
    }
    if !matches!(
        config
            .interface
            .default_view
            .trim()
            .to_ascii_lowercase()
            .as_str(),
        "form" | "raw" | "diff" | "info" | "plugins"
    ) {
        bail!("interface.default_view must be form, raw, diff, info, or plugins");
    }
    crate::keybindings::Keymap::from_config(&config.keybindings)
        .context("validate configured keybindings")?;
    Ok(())
}

pub fn reset_keybindings() -> Result<PathBuf> {
    let path = config_dir().join("config.toml");
    let mut config = if path.exists() {
        fs::read_to_string(&path)
            .ok()
            .and_then(|text| toml::from_str::<AppConfig>(&text).ok())
            .unwrap_or_default()
    } else {
        AppConfig::default()
    };
    config.keybindings = KeybindingsConfig::default();
    let contents = toml::to_string_pretty(&config).context("serialize reset configuration")?;
    atomic_write(&path, contents.as_bytes(), None)?;
    Ok(path)
}
