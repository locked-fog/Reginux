use std::collections::HashSet;
use std::io::stdout;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use clap::Parser;
use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers,
};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::{Frame, Terminal};
use reginux_core::config::{
    load_app_config, reset_keybindings, save_app_config, AppConfig, ConfigLoad,
};
use reginux_core::filesystem::{is_system_path, read_regular_file, run_editor};
use reginux_core::keybindings::{Action, KeyStroke, Keymap};
use reginux_core::plugin::PluginPolicy;
use reginux_core::{
    clean_display_text, Catalog, ConfigEntry, DiscoverOptions, Transaction, ValueType,
};

#[derive(Debug, Parser)]
#[command(
    name = "reginux",
    version,
    about = "A safe, extensible Linux configuration layer"
)]
struct Args {
    /// Ignore user configuration and start with defaults.
    #[arg(long)]
    safe: bool,

    /// Restore the default keymap in ~/.config/reginux/config.toml.
    #[arg(long)]
    reset_keybindings: bool,

    /// Add one plugin directory for this invocation.
    #[arg(long)]
    plugin_dir: Option<PathBuf>,

    /// Temporarily approve one Adapter or Transform plugin ID for this process.
    #[arg(long = "allow-plugin", value_name = "ID")]
    allow_plugins: Vec<String>,

    /// Skip the apply confirmation dialog for this invocation.
    #[arg(long)]
    no_confirm: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum View {
    Form,
    Raw,
    Diff,
    Info,
    Help,
    Plugins,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Scope {
    Overview,
    System,
    Applications,
    Reginux,
    ConfigFiles,
    Search,
}

impl Scope {
    fn label(self) -> &'static str {
        match self {
            Self::Overview => "Overview",
            Self::System => "System",
            Self::Applications => "Applications",
            Self::Reginux => "Reginux",
            Self::ConfigFiles => "Config files",
            Self::Search => "Search results",
        }
    }

    fn includes(self, entry: &ConfigEntry) -> bool {
        match self {
            Self::Overview => {
                entry.provider != "generic.files" && entry.provider != "linux.environment"
            }
            Self::System => entry.section.starts_with("System /"),
            Self::Applications => entry.section.starts_with("Applications /"),
            Self::Reginux => entry.section.starts_with("Reginux /"),
            Self::ConfigFiles => entry.section.starts_with("Config Files /"),
            Self::Search => true,
        }
    }

    fn shifted(self, direction: isize) -> Self {
        const SCOPES: [Scope; 5] = [
            Scope::Overview,
            Scope::System,
            Scope::Applications,
            Scope::Reginux,
            Scope::ConfigFiles,
        ];
        if self == Scope::Search {
            return Scope::Overview;
        }
        let current = SCOPES.iter().position(|scope| *scope == self).unwrap_or(0) as isize;
        SCOPES[(current + direction).rem_euclid(SCOPES.len() as isize) as usize]
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Normal,
    Search,
    Edit,
    ConfirmApply,
    ConfirmDiscard,
    ConfirmQuit,
    ConfirmPluginApprove,
    ConfirmPluginRevoke,
}

enum Effect {
    None,
    OpenEditor { source: PathBuf, working: PathBuf },
}

struct App {
    catalog: Catalog,
    config: AppConfig,
    keymap: Keymap,
    transaction: Transaction,
    visible: Vec<usize>,
    selected: usize,
    plugin_selected: usize,
    scope: Scope,
    view: View,
    mode: Mode,
    edit_input: String,
    edit_cursor: usize,
    search_input: String,
    search_cursor: usize,
    search_original_visible: Vec<usize>,
    search_original_selected: usize,
    status: String,
    warning: Option<String>,
    pending_keys: Vec<KeyStroke>,
    sequence_started: Option<Instant>,
    content_scroll: u16,
    should_quit: bool,
    no_confirm: bool,
    safe_mode: bool,
    extra_plugin_directory: Option<PathBuf>,
    temporary_plugin_approvals: HashSet<String>,
}

impl App {
    fn new(
        load: ConfigLoad,
        catalog: Catalog,
        no_confirm: bool,
        safe_mode: bool,
        extra_plugin_directory: Option<PathBuf>,
        temporary_plugin_approvals: HashSet<String>,
    ) -> Self {
        let (keymap, keymap_warning) = match Keymap::from_config(&load.config.keybindings) {
            Ok(keymap) => (keymap, None),
            Err(error) => (
                Keymap::default(),
                Some(format!("Keymap error: {error}. Defaults are active.")),
            ),
        };
        let initial_view = parse_view(&load.config.interface.default_view).unwrap_or(View::Form);
        let mut app = Self {
            visible: Vec::new(),
            catalog,
            config: load.config,
            keymap,
            transaction: Transaction::default(),
            selected: 0,
            plugin_selected: 0,
            scope: Scope::Overview,
            view: initial_view,
            mode: Mode::Normal,
            edit_input: String::new(),
            edit_cursor: 0,
            search_input: String::new(),
            search_cursor: 0,
            search_original_visible: Vec::new(),
            search_original_selected: 0,
            status: String::new(),
            warning: load.warning,
            pending_keys: Vec::new(),
            sequence_started: None,
            content_scroll: 0,
            should_quit: false,
            no_confirm,
            safe_mode,
            extra_plugin_directory,
            temporary_plugin_approvals,
        };
        app.rebuild_visible_for_scope();
        if let Some(warning) = app.warning.clone().or(keymap_warning) {
            app.status = warning;
        } else if !app.catalog.diagnostics.is_empty() {
            app.status = app.catalog.diagnostics.join(" | ");
        } else {
            app.status = "Ready. Files remain the source of truth.".to_owned();
        }
        app
    }

    fn selected_entry(&self) -> Option<&ConfigEntry> {
        self.visible
            .get(self.selected)
            .and_then(|index| self.catalog.entries.get(*index))
    }

    fn selected_index(&self) -> Option<usize> {
        self.visible.get(self.selected).copied()
    }

    fn active_contexts(&self) -> &'static [&'static str] {
        match self.mode {
            Mode::Search => &["search"],
            Mode::Edit
            | Mode::ConfirmApply
            | Mode::ConfirmDiscard
            | Mode::ConfirmQuit
            | Mode::ConfirmPluginApprove
            | Mode::ConfirmPluginRevoke => &["dialog"],
            Mode::Normal => match self.view {
                View::Form => &["form", "browser"],
                View::Raw => &["raw"],
                View::Diff => &["diff"],
                View::Info => &["info"],
                View::Help => &["help"],
                View::Plugins => &["plugin_manager"],
            },
        }
    }

    fn handle_key(&mut self, event: KeyEvent) -> Result<Effect> {
        if !matches!(event.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            return Ok(Effect::None);
        }
        match self.mode {
            Mode::Search => return self.handle_search_key(event),
            Mode::Edit => return self.handle_edit_key(event),
            Mode::ConfirmApply
            | Mode::ConfirmDiscard
            | Mode::ConfirmQuit
            | Mode::ConfirmPluginApprove
            | Mode::ConfirmPluginRevoke => {
                return self.handle_confirm_key(event);
            }
            Mode::Normal => {}
        }

        if self.sequence_started.is_some_and(|started| {
            started.elapsed() > Duration::from_millis(self.config.interface.key_sequence_timeout_ms)
        }) {
            self.pending_keys.clear();
            self.sequence_started = None;
        }
        let stroke = KeyStroke::from_event(event);
        if self.pending_keys.is_empty() {
            self.sequence_started = Some(Instant::now());
        }
        self.pending_keys.push(stroke);
        if let Some(action) = self
            .keymap
            .resolve_contexts(self.active_contexts(), &self.pending_keys)
        {
            self.pending_keys.clear();
            self.sequence_started = None;
            return self.perform_action(action);
        }
        if self
            .keymap
            .is_prefix_contexts(self.active_contexts(), &self.pending_keys)
        {
            self.status = format!(
                "Pending key sequence: {}",
                display_pending(&self.pending_keys)
            );
            return Ok(Effect::None);
        }
        self.pending_keys.clear();
        self.sequence_started = None;
        Ok(Effect::None)
    }

    fn expire_pending_sequence(&mut self) -> bool {
        if self.sequence_started.is_some_and(|started| {
            started.elapsed() > Duration::from_millis(self.config.interface.key_sequence_timeout_ms)
        }) {
            self.pending_keys.clear();
            self.sequence_started = None;
            if self.status.starts_with("Pending key sequence:") {
                self.status = "Key sequence timed out.".to_owned();
            }
            return true;
        }
        false
    }

    fn perform_action(&mut self, action: Action) -> Result<Effect> {
        match action.0.as_str() {
            "navigation.down" => self.navigate_or_scroll(1),
            "navigation.up" => self.navigate_or_scroll(-1),
            "navigation.top" => {
                if self.view == View::Form {
                    self.selected = 0;
                } else if self.view == View::Plugins {
                    self.plugin_selected = 0;
                } else {
                    self.content_scroll = 0;
                }
            }
            "navigation.bottom" => {
                if self.view == View::Form {
                    self.selected = self.visible.len().saturating_sub(1);
                } else if self.view == View::Plugins {
                    self.plugin_selected = self.catalog.plugins.len().saturating_sub(1);
                } else {
                    self.content_scroll = u16::MAX;
                }
            }
            "navigation.page_up" => self.navigate_or_scroll(-10),
            "navigation.page_down" => self.navigate_or_scroll(10),
            "navigation.left" => {
                self.view = View::Form;
                self.content_scroll = 0;
                self.status = "Returned to Form view.".to_owned();
            }
            "navigation.right" => {
                self.view = if matches!(self.view, View::Form) {
                    View::Raw
                } else {
                    View::Form
                };
                self.content_scroll = 0;
            }
            "navigation.activate" => {
                if matches!(self.view, View::Help) {
                    self.view = View::Form;
                } else if let Some(entry) = self.selected_entry().cloned() {
                    if entry.value_type == ValueType::Raw {
                        self.view = View::Raw;
                    } else {
                        self.view = View::Form;
                    }
                }
            }
            "view.next" => self.next_view(),
            "view.form" => self.set_view(View::Form),
            "view.raw" => self.set_view(View::Raw),
            "view.diff" => self.set_view(View::Diff),
            "view.info" => self.set_view(View::Info),
            "view.plugins" => self.set_view(View::Plugins),
            "scope.next" => self.change_scope(1),
            "scope.previous" => self.change_scope(-1),
            "config.edit" => return self.start_edit(),
            "config.reload" => self.request_reload_selected(),
            "plugin.approve" => self.request_plugin_approval(),
            "plugin.revoke" => self.request_plugin_revocation(),
            "changes.undo" => {
                if self.transaction.undo() {
                    self.refresh_values();
                    self.status = "Undid the most recent staged operation.".to_owned();
                } else {
                    self.status = "Nothing to undo.".to_owned();
                }
            }
            "changes.apply" => {
                if self.transaction.changed_count() == 0 {
                    self.status = "No staged changes.".to_owned();
                } else if self.no_confirm || !self.config.interface.confirm_before_apply {
                    self.perform_apply()?;
                } else {
                    self.mode = Mode::ConfirmApply;
                }
            }
            "search.open" => {
                self.search_original_visible = self.visible.clone();
                self.search_original_selected = self.selected;
                self.search_input.clear();
                self.search_cursor = 0;
                self.mode = Mode::Search;
            }
            "search.next" => self.move_search_result(1),
            "search.previous" => self.move_search_result(-1),
            "help.keybindings" => self.set_view(View::Help),
            "application.back" => self.go_back(),
            "application.quit" => self.request_quit(),
            _ => {}
        }
        Ok(Effect::None)
    }

    fn handle_search_key(&mut self, event: KeyEvent) -> Result<Effect> {
        match event.code {
            KeyCode::Esc => {
                self.mode = Mode::Normal;
                self.search_input.clear();
                self.search_cursor = 0;
                self.visible = std::mem::take(&mut self.search_original_visible);
                self.selected = self
                    .search_original_selected
                    .min(self.visible.len().saturating_sub(1));
                self.status = "Search cancelled.".to_owned();
            }
            KeyCode::Enter => {
                self.mode = Mode::Normal;
                if self.search_input.is_empty() {
                    self.visible = std::mem::take(&mut self.search_original_visible);
                    self.selected = self
                        .search_original_selected
                        .min(self.visible.len().saturating_sub(1));
                    self.status = "Search closed without a query.".to_owned();
                } else {
                    self.scope = Scope::Search;
                    self.status = format!("Search: {} result(s)", self.visible.len());
                }
            }
            KeyCode::Backspace => {
                delete_previous_char(&mut self.search_input, &mut self.search_cursor);
                self.refresh_search();
            }
            KeyCode::Delete => {
                delete_next_char(&mut self.search_input, self.search_cursor);
                self.refresh_search();
            }
            KeyCode::Left => move_cursor_left(&self.search_input, &mut self.search_cursor),
            KeyCode::Right => move_cursor_right(&self.search_input, &mut self.search_cursor),
            KeyCode::Home => self.search_cursor = 0,
            KeyCode::End => self.search_cursor = self.search_input.len(),
            KeyCode::Char(ch)
                if event.modifiers.is_empty() || event.modifiers == KeyModifiers::SHIFT =>
            {
                insert_char(&mut self.search_input, &mut self.search_cursor, ch);
                self.refresh_search();
            }
            _ => {}
        }
        Ok(Effect::None)
    }

    fn handle_edit_key(&mut self, event: KeyEvent) -> Result<Effect> {
        match event.code {
            KeyCode::Esc => {
                self.mode = Mode::Normal;
                self.edit_input.clear();
                self.edit_cursor = 0;
                self.status = "Edit cancelled.".to_owned();
            }
            KeyCode::Enter => self.commit_inline_edit()?,
            KeyCode::Backspace => {
                delete_previous_char(&mut self.edit_input, &mut self.edit_cursor);
            }
            KeyCode::Delete => delete_next_char(&mut self.edit_input, self.edit_cursor),
            KeyCode::Left => move_cursor_left(&self.edit_input, &mut self.edit_cursor),
            KeyCode::Right => move_cursor_right(&self.edit_input, &mut self.edit_cursor),
            KeyCode::Home => self.edit_cursor = 0,
            KeyCode::End => self.edit_cursor = self.edit_input.len(),
            KeyCode::Char('w') if event.modifiers.contains(KeyModifiers::CONTROL) => {
                delete_previous_word(&mut self.edit_input, &mut self.edit_cursor);
            }
            KeyCode::Char('u') if event.modifiers.contains(KeyModifiers::CONTROL) => {
                self.edit_input.drain(..self.edit_cursor);
                self.edit_cursor = 0;
            }
            KeyCode::Char(ch)
                if event.modifiers.is_empty() || event.modifiers == KeyModifiers::SHIFT =>
            {
                insert_char(&mut self.edit_input, &mut self.edit_cursor, ch);
            }
            _ => {}
        }
        Ok(Effect::None)
    }

    fn handle_confirm_key(&mut self, event: KeyEvent) -> Result<Effect> {
        match event.code {
            KeyCode::Enter | KeyCode::Char('y') => {
                let mode = self.mode;
                self.mode = Mode::Normal;
                match mode {
                    Mode::ConfirmApply => self.perform_apply()?,
                    Mode::ConfirmDiscard => self.confirm_reload_discard(),
                    Mode::ConfirmQuit => self.should_quit = true,
                    Mode::ConfirmPluginApprove => self.persist_plugin_approval(true)?,
                    Mode::ConfirmPluginRevoke => self.persist_plugin_approval(false)?,
                    _ => {}
                }
            }
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('n') => {
                self.mode = Mode::Normal;
                self.status = "Confirmation cancelled.".to_owned();
            }
            _ => {}
        }
        Ok(Effect::None)
    }

    fn start_edit(&mut self) -> Result<Effect> {
        let Some(index) = self.selected_index() else {
            return Ok(Effect::None);
        };
        let entry = self.catalog.entries[index].clone();
        if !entry.is_editable() {
            self.status = "This entry is read-only.".to_owned();
            return Ok(Effect::None);
        }
        if matches!(entry.value_type, ValueType::Raw) || matches!(self.view, View::Raw) {
            if entry.source.is_system() {
                self.status = "Raw editing of system files is disabled; use an authorized built-in or trusted Schema field."
                    .to_owned();
                return Ok(Effect::None);
            }
            let Some(source) = entry.source_path().map(PathBuf::from) else {
                self.status = "Runtime sources do not have a Raw file editor.".to_owned();
                return Ok(Effect::None);
            };
            let working = match self.transaction.working_copy(&source) {
                Ok(working) => working,
                Err(error) => {
                    self.status = format!("Cannot prepare external edit: {error}");
                    return Ok(Effect::None);
                }
            };
            self.status = format!(
                "Editing staged copy with external editor: {}",
                source.display()
            );
            return Ok(Effect::OpenEditor { source, working });
        }
        self.edit_input = entry.value;
        self.edit_cursor = self.edit_input.len();
        self.mode = Mode::Edit;
        Ok(Effect::None)
    }

    fn commit_inline_edit(&mut self) -> Result<()> {
        let Some(index) = self.selected_index() else {
            self.mode = Mode::Normal;
            return Ok(());
        };
        let entry = self.catalog.entries[index].clone();
        match self.transaction.stage_entry(&entry, &self.edit_input) {
            Ok(()) => {
                self.catalog.entries[index].value = self.edit_input.clone();
                self.status = format!("Staged change for {}.", entry.id);
                self.mode = Mode::Normal;
                self.edit_cursor = 0;
            }
            Err(error) => {
                self.status = format!("Cannot stage: {error}");
            }
        }
        Ok(())
    }

    fn complete_external_edit(&mut self, source: PathBuf, working: PathBuf) {
        let result = (|| -> Result<()> {
            let bytes = read_regular_file(&working)
                .context("the editor working copy was removed or became invalid")?;
            self.transaction.stage_raw(&source, bytes)?;
            let _ = std::fs::remove_file(&working);
            self.refresh_values();
            Ok(())
        })();
        match result {
            Ok(()) => {
                self.status = if self.transaction.has_changes_for(&source) {
                    format!("Staged external edit for {}.", source.display())
                } else {
                    format!("Editor made no changes to {}.", source.display())
                };
            }
            Err(error) => {
                self.status = format!("External edit was not staged: {error}");
                let _ = std::fs::remove_file(&working);
            }
        }
    }

    fn perform_apply(&mut self) -> Result<()> {
        if !self.config.safety.allow_system_writes && self.transaction.has_system_changes() {
            self.status = "System writes are disabled by Reginux safety settings.".to_owned();
            return Ok(());
        }
        for plugin_id in self.transaction.staged_adapter_plugin_ids() {
            let usable = self.catalog.plugins.iter().any(|plugin| {
                plugin.id == plugin_id
                    && plugin.kind == "adapter"
                    && plugin.approval == "approved"
                    && !plugin.stale
                    && plugin.last_error.is_none()
            });
            if !usable {
                self.status = format!(
                    "Adapter plugin {plugin_id} is no longer approved or has a stale snapshot; discard and refresh its staged changes."
                );
                return Ok(());
            }
        }
        match self
            .transaction
            .apply(self.config.safety.backup_before_apply)
        {
            Ok(report) if report.files.is_empty() && report.adapter_operations.is_empty() => {
                self.status = "No staged changes.".to_owned();
            }
            Ok(report) => {
                self.status = format!(
                    "Applied {} file(s) and {} adapter operation(s) in transaction {}. Backups: {}.",
                    report.files.len(),
                    report.adapter_operations.len(),
                    report.transaction_id,
                    report.backups.len()
                );
                self.reload_after_apply();
            }
            Err(error) => {
                self.status = format!("Apply failed; staged changes were retained: {error}");
            }
        }
        Ok(())
    }

    fn reload_after_apply(&mut self) {
        let selected_id = self.selected_entry().map(|entry| entry.id.clone());
        let load = load_app_config(self.safe_mode);
        self.config = load.config;
        self.warning = load.warning;
        if let Ok(keymap) = Keymap::from_config(&self.config.keybindings) {
            self.keymap = keymap;
        } else {
            self.keymap = Keymap::default();
            self.status.push_str(" Keymap invalid; defaults active.");
        }
        let mut plugin_directories = self.config.plugins.directories.clone();
        if let Some(directory) = &self.extra_plugin_directory {
            plugin_directories.push(directory.display().to_string());
        }
        let mut policy = self.plugin_policy();
        policy.refresh_runtime = true;
        let mut refreshed = Catalog::discover(DiscoverOptions {
            app_config: self.config.clone(),
            plugin_directories,
            plugin_policy: policy,
            include_generic_files: true,
        });
        refreshed.retain_stale_from(&self.catalog);
        self.catalog = refreshed;
        if self.scope == Scope::Search {
            self.scope = Scope::Overview;
        }
        self.rebuild_visible_for_scope();
        self.selected = selected_id
            .and_then(|id| {
                self.visible
                    .iter()
                    .position(|index| self.catalog.entries[*index].id == id)
            })
            .unwrap_or(0);
        self.refresh_values();
        self.content_scroll = 0;
    }

    fn request_reload_selected(&mut self) {
        if self.view == View::Plugins {
            let selected = self.selected_plugin().map(|plugin| plugin.id.clone());
            self.reload_after_apply();
            self.plugin_selected = selected
                .and_then(|id| {
                    self.catalog
                        .plugins
                        .iter()
                        .position(|plugin| plugin.id == id)
                })
                .unwrap_or(0);
            self.status = "Refreshed plugin sources and runtime snapshots.".to_owned();
            return;
        }
        if let Some(entry) = self.selected_entry().cloned() {
            if self.transaction.has_changes_for_entry(&entry) {
                self.mode = Mode::ConfirmDiscard;
            } else if entry.provider.starts_with("plugin.") {
                self.reload_after_apply();
                self.status = format!("Refreshed plugin source for {}.", entry.id);
            } else if let Some(path) = entry.source_path() {
                self.refresh_values();
                self.status = format!("Reloaded {} from disk.", path.display());
            } else {
                self.reload_after_apply();
                self.status = format!("Refreshed runtime source for {}.", entry.id);
            }
        }
    }

    fn discard_selected(&mut self) {
        let Some(entry) = self.selected_entry().cloned() else {
            return;
        };
        if self.transaction.discard_entry(&entry) {
            self.refresh_values();
            self.status = format!("Discarded staged changes for {}.", entry.source_display());
        } else {
            self.status = "No staged changes for the selected source.".to_owned();
        }
    }

    fn confirm_reload_discard(&mut self) {
        let plugin_backed = self
            .selected_entry()
            .is_some_and(|entry| entry.provider.starts_with("plugin."));
        let runtime = self
            .selected_entry()
            .is_some_and(|entry| entry.source_path().is_none());
        self.discard_selected();
        if plugin_backed || runtime {
            self.reload_after_apply();
            self.status = "Discarded staged state and refreshed the selected source.".to_owned();
        }
    }

    fn plugin_policy(&self) -> PluginPolicy {
        PluginPolicy {
            approved_adapters: self.config.plugins.approved_adapters.clone(),
            approved_transforms: self.config.plugins.approved_transforms.clone(),
            temporary_approvals: self.temporary_plugin_approvals.clone(),
            refresh_runtime: false,
        }
    }

    fn selected_plugin(&self) -> Option<&reginux_core::PluginSummary> {
        self.catalog.plugins.get(self.plugin_selected)
    }

    fn request_plugin_approval(&mut self) {
        if self.view != View::Plugins {
            self.status = "Open the Plugins view before approving a plugin.".to_owned();
            return;
        }
        if self.safe_mode {
            self.status = "Safe mode does not persist plugin approvals.".to_owned();
            return;
        }
        let Some(plugin) = self.selected_plugin() else {
            self.status = "No plugin selected.".to_owned();
            return;
        };
        if !matches!(plugin.kind.as_str(), "adapter" | "transform") || plugin.digest.is_none() {
            self.status = "This plugin does not require executable-code approval.".to_owned();
            return;
        }
        self.mode = Mode::ConfirmPluginApprove;
    }

    fn request_plugin_revocation(&mut self) {
        if self.view != View::Plugins {
            self.status = "Open the Plugins view before revoking a plugin.".to_owned();
            return;
        }
        if self.safe_mode {
            self.status = "Safe mode does not persist plugin approvals.".to_owned();
            return;
        }
        if self.selected_plugin().is_none() {
            self.status = "No plugin selected.".to_owned();
            return;
        }
        self.mode = Mode::ConfirmPluginRevoke;
    }

    fn persist_plugin_approval(&mut self, approve: bool) -> Result<()> {
        let Some(plugin) = self.selected_plugin().cloned() else {
            self.status = "Plugin disappeared before confirmation.".to_owned();
            return Ok(());
        };
        if approve {
            let digest = plugin
                .digest
                .clone()
                .ok_or_else(|| anyhow::anyhow!("plugin has no approval digest"))?;
            match plugin.kind.as_str() {
                "adapter" => {
                    self.config
                        .plugins
                        .approved_adapters
                        .insert(plugin.id.clone(), digest);
                }
                "transform" => {
                    self.config
                        .plugins
                        .approved_transforms
                        .insert(plugin.id.clone(), digest);
                }
                _ => {
                    self.status = "Plugin does not require approval.".to_owned();
                    return Ok(());
                }
            }
        } else {
            self.config.plugins.approved_adapters.remove(&plugin.id);
            self.config.plugins.approved_transforms.remove(&plugin.id);
            self.temporary_plugin_approvals.remove(&plugin.id);
        }
        save_app_config(&self.config)?;
        self.reload_after_apply();
        self.plugin_selected = self
            .catalog
            .plugins
            .iter()
            .position(|candidate| candidate.id == plugin.id)
            .unwrap_or(0);
        self.status = if approve {
            format!("Approved {} at its current digest.", plugin.id)
        } else {
            format!("Revoked approval for {}.", plugin.id)
        };
        Ok(())
    }

    fn request_quit(&mut self) {
        if self.transaction.changed_count() == 0 {
            self.should_quit = true;
        } else {
            self.mode = Mode::ConfirmQuit;
        }
    }

    fn refresh_values(&mut self) {
        for entry in &mut self.catalog.entries {
            if let Ok(value) = self.transaction.value_for_entry(entry) {
                if entry.value_type != ValueType::Raw {
                    entry.value = value;
                }
            }
        }
    }

    fn rebuild_visible_for_scope(&mut self) {
        self.visible = self
            .catalog
            .entries
            .iter()
            .enumerate()
            .filter_map(|(index, entry)| self.scope.includes(entry).then_some(index))
            .collect();
        self.selected = self.selected.min(self.visible.len().saturating_sub(1));
        self.content_scroll = 0;
    }

    fn change_scope(&mut self, direction: isize) {
        self.scope = self.scope.shifted(direction);
        self.selected = 0;
        self.rebuild_visible_for_scope();
        self.status = format!(
            "Scope: {} ({} visible entries).",
            self.scope.label(),
            self.visible.len()
        );
    }

    fn refresh_search(&mut self) {
        self.visible = if self.search_input.is_empty() {
            (0..self.catalog.entries.len()).collect()
        } else {
            self.catalog.search_indices(&self.search_input)
        };
        self.selected = self.selected.min(self.visible.len().saturating_sub(1));
        self.content_scroll = 0;
    }

    fn search_next(&mut self, direction: isize) {
        if self.visible.is_empty() {
            return;
        }
        let len = self.visible.len() as isize;
        self.selected = ((self.selected as isize + direction).rem_euclid(len)) as usize;
        self.content_scroll = 0;
    }

    fn move_search_result(&mut self, direction: isize) {
        if self.scope == Scope::Search {
            self.search_next(direction);
        } else {
            self.status = "No active search results; press / to search.".to_owned();
        }
    }

    fn move_selection(&mut self, amount: isize) {
        if self.visible.is_empty() {
            return;
        }
        let len = self.visible.len() as isize;
        self.selected = ((self.selected as isize + amount).rem_euclid(len)) as usize;
        self.content_scroll = 0;
    }

    fn navigate_or_scroll(&mut self, amount: isize) {
        if self.view == View::Form {
            self.move_selection(amount);
        } else if self.view == View::Plugins {
            if self.catalog.plugins.is_empty() {
                return;
            }
            let len = self.catalog.plugins.len() as isize;
            self.plugin_selected =
                ((self.plugin_selected as isize + amount).rem_euclid(len)) as usize;
            self.content_scroll = 0;
        } else if amount.is_negative() {
            self.content_scroll = self
                .content_scroll
                .saturating_sub(amount.unsigned_abs() as u16);
        } else {
            self.content_scroll = self.content_scroll.saturating_add(amount as u16);
        }
    }

    fn set_view(&mut self, view: View) {
        self.view = view;
        self.content_scroll = 0;
    }

    fn next_view(&mut self) {
        self.set_view(match self.view {
            View::Form => View::Raw,
            View::Raw => View::Diff,
            View::Diff => View::Info,
            View::Info => View::Plugins,
            View::Plugins | View::Help => View::Form,
        });
    }

    fn go_back(&mut self) {
        match self.view {
            View::Form => {
                self.status = "At the root view; use Q to quit.".to_owned();
            }
            _ => self.set_view(View::Form),
        }
    }

    fn ui(&self, frame: &mut Frame) {
        let outer = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(2),
                Constraint::Min(5),
                Constraint::Length(2),
            ])
            .split(frame.area());
        self.render_header(frame, outer[0]);
        self.render_body(frame, outer[1]);
        self.render_footer(frame, outer[2]);
    }

    fn render_header(&self, frame: &mut Frame, area: Rect) {
        let title = format!(
            " Reginux · {} · {}/{} entries · {} plugins · [s/]s scope ",
            self.scope.label(),
            self.visible.len(),
            self.catalog.entries.len(),
            self.catalog.plugins.len()
        );
        frame.render_widget(
            Paragraph::new(title)
                .style(
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                )
                .block(Block::default().borders(Borders::BOTTOM)),
            area,
        );
    }

    fn render_body(&self, frame: &mut Frame, area: Rect) {
        let columns = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(38), Constraint::Percentage(62)])
            .split(area);
        let items = self
            .visible
            .iter()
            .map(|index| {
                let entry = &self.catalog.entries[*index];
                let marker = if self.transaction.has_changes_for_entry(entry) {
                    "*"
                } else {
                    " "
                };
                let access = if !entry.is_editable() {
                    "R"
                } else if entry.source.is_system() {
                    "S"
                } else if entry.source_path().is_none() {
                    "A"
                } else {
                    "U"
                };
                ListItem::new(format!(
                    "{marker} [{access}] {}  ·  {}",
                    clean_display_text(&entry.label),
                    clean_display_text(&entry.section)
                ))
            })
            .collect::<Vec<_>>();
        let list = List::new(items)
            .block(
                Block::default()
                    .title("Configuration")
                    .borders(Borders::ALL),
            )
            .highlight_style(Style::default().bg(Color::Blue).fg(Color::White))
            .highlight_symbol("› ");
        let mut state = ListState::default();
        state.select(Some(self.selected));
        frame.render_stateful_widget(list, columns[0], &mut state);

        if matches!(
            self.mode,
            Mode::ConfirmApply
                | Mode::ConfirmDiscard
                | Mode::ConfirmQuit
                | Mode::ConfirmPluginApprove
                | Mode::ConfirmPluginRevoke
        ) {
            self.render_confirmation(frame, columns[1]);
        } else {
            match self.view {
                View::Form => self.render_form(frame, columns[1]),
                View::Raw => self.render_raw(frame, columns[1]),
                View::Diff => self.render_diff(frame, columns[1]),
                View::Info => self.render_info(frame, columns[1]),
                View::Help => self.render_help(frame, columns[1]),
                View::Plugins => self.render_plugins(frame, columns[1]),
            }
        }
    }

    fn render_form(&self, frame: &mut Frame, area: Rect) {
        let Some(entry) = self.selected_entry() else {
            frame.render_widget(Paragraph::new("No configuration entries detected."), area);
            return;
        };
        let mut text = format!(
            "{}\n\n{}\n\nValue\n{}\n\nType: {}\nEdit capability: {}\nPrivilege: {}\n\nSource\n{}\n\nProvider\n{}\n\nValidation\n{}",
            clean_display_text(&entry.label),
            clean_display_text(&entry.description),
            clean_terminal_text(&entry.value),
            entry.value_type.as_str(),
            entry.edit_capability.as_str(),
            entry.privilege.as_str(),
            clean_display_text(&entry.source_display()),
            clean_display_text(&entry.provider),
            clean_display_text(&entry.validation),
        );
        if let Mode::Edit = self.mode {
            let edit_display = display_with_cursor(&self.edit_input, self.edit_cursor);
            text = format!(
                "{}\n\nEdit value\n{}\n\n←/→ move · Home/End · Ctrl+W delete word\nEnter stage · Esc cancel",
                clean_display_text(&entry.label),
                clean_terminal_text(&edit_display)
            );
        }
        frame.render_widget(
            Paragraph::new(text)
                .block(Block::default().title("Form").borders(Borders::ALL))
                .wrap(Wrap { trim: false }),
            area,
        );
    }

    fn render_raw(&self, frame: &mut Frame, area: Rect) {
        let Some(entry) = self.selected_entry() else {
            return;
        };
        let Some(path) = entry.source_path() else {
            frame.render_widget(
                Paragraph::new(format!(
                    "Runtime source\n\n{}\n\nRaw file editing is unavailable. Press r to refresh the declared provider.",
                    clean_display_text(&entry.source_display())
                ))
                .block(Block::default().title("Raw · runtime source").borders(Borders::ALL))
                .wrap(Wrap { trim: false }),
                area,
            );
            return;
        };
        let content = self
            .transaction
            .content_for(path)
            .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
            .unwrap_or_else(|error| {
                format!(
                    "Cannot read {}: {}",
                    clean_display_text(&path.display().to_string()),
                    clean_display_text(&error.to_string())
                )
            });
        let content = clean_terminal_text(&content);
        let scroll = clamped_scroll(&content, area, self.content_scroll);
        frame.render_widget(
            Paragraph::new(content)
                .block(
                    Block::default()
                        .title(format!(
                            "Raw: {}",
                            clean_display_text(&path.display().to_string())
                        ))
                        .borders(Borders::ALL),
                )
                .wrap(Wrap { trim: false })
                .scroll((scroll, 0)),
            area,
        );
    }

    fn render_diff(&self, frame: &mut Frame, area: Rect) {
        let text = clean_terminal_text(&self.transaction.diff());
        let scroll = clamped_scroll(&text, area, self.content_scroll);
        frame.render_widget(
            Paragraph::new(text)
                .block(
                    Block::default()
                        .title("Diff · staged changes")
                        .borders(Borders::ALL),
                )
                .wrap(Wrap { trim: false })
                .scroll((scroll, 0)),
            area,
        );
    }

    fn render_info(&self, frame: &mut Frame, area: Rect) {
        let Some(entry) = self.selected_entry() else {
            return;
        };
        let mut text = format!(
            "ID\n{}\n\nLabel\n{}\n\nSection\n{}\n\nSource\n{}\n\nProvider\n{}\n\nType\n{}\n\nPrivilege\n{}\n\nEdit capability\n{}\n\nValidation\n{}",
            clean_display_text(&entry.id),
            clean_display_text(&entry.label),
            clean_display_text(&entry.section),
            clean_display_text(&entry.source_display()),
            clean_display_text(&entry.provider),
            entry.value_type.as_str(),
            entry.privilege.as_str(),
            entry.edit_capability.as_str(),
            clean_display_text(&entry.validation),
        );
        if !entry.metadata.is_empty() {
            text.push_str("\n\nMetadata\n");
            for (key, value) in &entry.metadata {
                text.push_str(&format!(
                    "{}: {}\n",
                    clean_display_text(key),
                    clean_display_text(value)
                ));
            }
        }
        let scroll = clamped_scroll(&text, area, self.content_scroll);
        frame.render_widget(
            Paragraph::new(text)
                .block(Block::default().title("Info").borders(Borders::ALL))
                .wrap(Wrap { trim: false })
                .scroll((scroll, 0)),
            area,
        );
    }

    fn render_help(&self, frame: &mut Frame, area: Rect) {
        let text = self.keymap.help_text();
        let scroll = clamped_scroll(&text, area, self.content_scroll);
        frame.render_widget(
            Paragraph::new(text)
                .block(Block::default().title("Keybindings").borders(Borders::ALL))
                .wrap(Wrap { trim: false })
                .scroll((scroll, 0)),
            area,
        );
    }

    fn render_plugins(&self, frame: &mut Frame, area: Rect) {
        let mut text = String::from(
            "Plugin manager · j/k select · a approve exact digest · x revoke · r refresh\n\n",
        );
        if let Some(plugin) = self.selected_plugin() {
            text.push_str(&format!(
                "Plugin {}/{}\n\n{} [{}]\nID: {}\nStatus: {}\nSnapshot: {}{}\nTrust: {}\nApproval: {}\nDigest: {}\nPath: {}\n",
                self.plugin_selected + 1,
                self.catalog.plugins.len(),
                clean_display_text(&plugin.name),
                clean_display_text(&plugin.kind),
                clean_display_text(&plugin.id),
                clean_display_text(&plugin.status),
                clean_display_text(plugin.captured_at.as_deref().unwrap_or("not captured")),
                if plugin.stale { " (STALE, read-only)" } else { "" },
                clean_display_text(&plugin.trust),
                clean_display_text(&plugin.approval),
                clean_display_text(plugin.digest.as_deref().unwrap_or("-")),
                clean_display_text(&plugin.path.display().to_string())
            ));
            if let Some(error) = &plugin.last_error {
                text.push_str(&format!(
                    "Last error: {} at {}\n",
                    clean_display_text(error),
                    clean_display_text(plugin.last_error_at.as_deref().unwrap_or("unknown time"))
                ));
            }
            if !plugin.sources.is_empty() {
                text.push_str("\nSources\n");
                for source in &plugin.sources {
                    text.push_str(&format!("• {}\n", clean_display_text(source)));
                }
            }
            if !plugin.capabilities.is_empty() {
                text.push_str("\nCapabilities\n");
                for capability in &plugin.capabilities {
                    text.push_str(&format!("• {}\n", clean_display_text(capability)));
                }
            }
            if !plugin.permissions.is_empty() {
                text.push_str("\nPermissions\n");
                for permission in &plugin.permissions {
                    text.push_str(&format!("• {}\n", clean_display_text(permission)));
                }
            }
        } else {
            text.push_str("No plugins discovered. Add a directory in Reginux / Plugins.\n");
        }
        let scroll = clamped_scroll(&text, area, self.content_scroll);
        frame.render_widget(
            Paragraph::new(text)
                .block(Block::default().title("Plugins").borders(Borders::ALL))
                .wrap(Wrap { trim: false })
                .scroll((scroll, 0)),
            area,
        );
    }

    fn render_confirmation(&self, frame: &mut Frame, area: Rect) {
        let text = match self.mode {
            Mode::ConfirmApply => {
                let summaries = self.transaction.change_summaries();
                let mut text = format!(
                    "Apply {} staged change(s)?\n\n",
                    self.transaction.changed_count()
                );
                for summary in summaries {
                    let scope = if is_system_path(&summary.path) {
                        "SYSTEM"
                    } else {
                        "USER"
                    };
                    let operation = if summary.creates_file { "create" } else { "replace" };
                    text.push_str(&format!(
                        "[{scope}] {}\n  {operation}; +{} -{} line(s)\n",
                        clean_display_text(&summary.path.display().to_string()),
                        summary.added_lines,
                        summary.removed_lines
                    ));
                }
                for adapter in self.transaction.adapter_change_summaries() {
                    text.push_str(&format!(
                        "[{} ADAPTER] {}\n  operation: {} via {}\n  arguments: {}\n  guarantee: {}; precondition={}; validation={}; verification={}; compensation={}\n",
                        adapter.scope.to_ascii_uppercase(),
                        clean_display_text(&adapter.entry_id),
                        clean_display_text(&adapter.operation_id),
                        clean_display_text(&adapter.transport),
                        clean_display_text(&adapter.typed_arguments),
                        clean_display_text(&adapter.guarantee),
                        adapter.has_precondition,
                        adapter.has_validation,
                        adapter.has_verification,
                        adapter.has_compensation
                    ));
                }
                text.push_str(
                    "\nUser files use atomic replacement after backups. System files are compare-and-replaced by the polkit helper. Reginux executes only the reviewed plan and then runs declared verification.\n\nEnter/y apply · Esc/n cancel",
                );
                text
            }
            Mode::ConfirmDiscard => {
                let path = self
                    .selected_entry()
                    .map(|entry| clean_display_text(&entry.source_display()))
                    .unwrap_or_else(|| "selected file".to_owned());
                format!(
                    "Discard staged changes for\n{path}?\n\nThis removes every staged field backed by that file. The source file is not changed.\n\nEnter/y discard · Esc/n cancel"
                )
            }
            Mode::ConfirmQuit => format!(
                "Quit with {} staged change(s)?\n\nUnapplied changes will be lost. Sources have not been modified.\n\nEnter/y quit · Esc/n cancel",
                self.transaction.changed_count()
            ),
            Mode::ConfirmPluginApprove => self
                .selected_plugin()
                .map(|plugin| {
                    format!(
                        "Approve executable plugin?\n\nName: {}\nID: {}\nKind: {}\nDigest: {}\n\nPermissions:\n{}\n\nThis approval is invalidated whenever the manifest, script, or executable digest changes.\n\nEnter/y approve · Esc/n cancel",
                        clean_display_text(&plugin.name),
                        clean_display_text(&plugin.id),
                        clean_display_text(&plugin.kind),
                        clean_display_text(plugin.digest.as_deref().unwrap_or("-")),
                        plugin
                            .permissions
                            .iter()
                            .map(|item| format!("• {}", clean_display_text(item)))
                            .collect::<Vec<_>>()
                            .join("\n")
                    )
                })
                .unwrap_or_else(|| "Plugin is no longer available.".to_owned()),
            Mode::ConfirmPluginRevoke => self
                .selected_plugin()
                .map(|plugin| {
                    format!(
                        "Revoke approval for {} ({})?\n\nThe plugin will be disabled after reload.\n\nEnter/y revoke · Esc/n cancel",
                        clean_display_text(&plugin.name),
                        clean_display_text(&plugin.id)
                    )
                })
                .unwrap_or_else(|| "Plugin is no longer available.".to_owned()),
            _ => String::new(),
        };
        let scroll = clamped_scroll(&text, area, self.content_scroll);
        frame.render_widget(
            Paragraph::new(text)
                .block(
                    Block::default()
                        .title("Confirmation")
                        .borders(Borders::ALL)
                        .border_style(Style::default().fg(Color::Yellow)),
                )
                .wrap(Wrap { trim: false })
                .scroll((scroll, 0)),
            area,
        );
    }

    fn render_footer(&self, frame: &mut Frame, area: Rect) {
        let mode = match self.mode {
            Mode::Normal => "NORMAL",
            Mode::Search => "SEARCH",
            Mode::Edit => "EDIT",
            Mode::ConfirmApply
            | Mode::ConfirmDiscard
            | Mode::ConfirmQuit
            | Mode::ConfirmPluginApprove
            | Mode::ConfirmPluginRevoke => "CONFIRM",
        };
        let view = match self.view {
            View::Form => "Form",
            View::Raw => "Raw",
            View::Diff => "Diff",
            View::Info => "Info",
            View::Help => "Help",
            View::Plugins => "Plugins",
        };
        let source = self
            .selected_entry()
            .map(|entry| clean_display_text(&entry.source_display()))
            .unwrap_or_else(|| "-".to_owned());
        let status = match self.mode {
            Mode::Search => format!(
                "Search: {}",
                display_with_cursor(&self.search_input, self.search_cursor)
            ),
            Mode::Edit => format!(
                "Edit: {}",
                display_with_cursor(&self.edit_input, self.edit_cursor)
            ),
            Mode::ConfirmApply => "Review impact · Enter/y Apply · Esc/n Cancel".to_owned(),
            Mode::ConfirmDiscard => "Enter/y Discard · Esc/n Cancel".to_owned(),
            Mode::ConfirmQuit => "Enter/y Quit · Esc/n Cancel".to_owned(),
            Mode::ConfirmPluginApprove => "Enter/y Approve exact digest · Esc/n Cancel".to_owned(),
            Mode::ConfirmPluginRevoke => "Enter/y Revoke approval · Esc/n Cancel".to_owned(),
            Mode::Normal => clean_terminal_text(&self.status),
        };
        let hint = if self.config.interface.show_key_hints && matches!(self.mode, Mode::Normal) {
            self.keymap.hints_contexts(self.active_contexts())
        } else {
            String::new()
        };
        let first = format!(
            " {mode} │ {view} │ {} staged │ {status} │ {source}",
            self.transaction.changed_count(),
        );
        let second = if hint.is_empty() {
            String::new()
        } else {
            format!(" {hint}")
        };
        let lines = vec![
            Line::from(Span::styled(first, Style::default().fg(Color::Yellow))),
            Line::from(Span::styled(second, Style::default().fg(Color::DarkGray))),
        ];
        frame.render_widget(Paragraph::new(lines), area);
    }
}

fn clean_terminal_text(value: &str) -> String {
    value
        .chars()
        .filter(|ch| *ch == '\n' || *ch == '\t' || (!ch.is_control() && *ch != '\u{1b}'))
        .collect()
}

fn display_pending(keys: &[KeyStroke]) -> String {
    keys.iter()
        .map(|key| match key.code {
            KeyCode::Char(ch) => {
                if key.modifiers.contains(KeyModifiers::CONTROL) {
                    format!("Ctrl+{ch}")
                } else {
                    ch.to_string()
                }
            }
            KeyCode::Enter => "Enter".to_owned(),
            KeyCode::Esc => "Esc".to_owned(),
            KeyCode::Up => "Up".to_owned(),
            KeyCode::Down => "Down".to_owned(),
            KeyCode::Left => "Left".to_owned(),
            KeyCode::Right => "Right".to_owned(),
            other => format!("{other:?}"),
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn parse_view(value: &str) -> Option<View> {
    match value.trim().to_ascii_lowercase().as_str() {
        "form" => Some(View::Form),
        "raw" => Some(View::Raw),
        "diff" => Some(View::Diff),
        "info" => Some(View::Info),
        "help" => Some(View::Help),
        "plugins" => Some(View::Plugins),
        _ => None,
    }
}

fn display_with_cursor(value: &str, cursor: usize) -> String {
    let cursor = cursor.min(value.len());
    let mut output = String::with_capacity(value.len() + 3);
    output.push_str(&value[..cursor]);
    output.push('│');
    output.push_str(&value[cursor..]);
    output
}

fn previous_char_boundary(value: &str, cursor: usize) -> usize {
    value[..cursor]
        .char_indices()
        .next_back()
        .map(|(index, _)| index)
        .unwrap_or(0)
}

fn next_char_boundary(value: &str, cursor: usize) -> usize {
    value[cursor..]
        .char_indices()
        .nth(1)
        .map(|(index, _)| cursor + index)
        .unwrap_or(value.len())
}

fn move_cursor_left(value: &str, cursor: &mut usize) {
    *cursor = previous_char_boundary(value, (*cursor).min(value.len()));
}

fn move_cursor_right(value: &str, cursor: &mut usize) {
    *cursor = next_char_boundary(value, (*cursor).min(value.len()));
}

fn insert_char(value: &mut String, cursor: &mut usize, ch: char) {
    value.insert(*cursor, ch);
    *cursor += ch.len_utf8();
}

fn delete_previous_char(value: &mut String, cursor: &mut usize) {
    if *cursor == 0 {
        return;
    }
    let previous = previous_char_boundary(value, *cursor);
    value.drain(previous..*cursor);
    *cursor = previous;
}

fn delete_next_char(value: &mut String, cursor: usize) {
    if cursor >= value.len() {
        return;
    }
    let next = next_char_boundary(value, cursor);
    value.drain(cursor..next);
}

fn delete_previous_word(value: &mut String, cursor: &mut usize) {
    let mut start = *cursor;
    while start > 0 {
        let previous = previous_char_boundary(value, start);
        let ch = value[previous..start].chars().next().unwrap_or(' ');
        if !ch.is_whitespace() {
            break;
        }
        start = previous;
    }
    while start > 0 {
        let previous = previous_char_boundary(value, start);
        let ch = value[previous..start].chars().next().unwrap_or(' ');
        if ch.is_whitespace() {
            break;
        }
        start = previous;
    }
    value.drain(start..*cursor);
    *cursor = start;
}

fn clamped_scroll(text: &str, area: Rect, requested: u16) -> u16 {
    let viewport = area.height.saturating_sub(2) as usize;
    let line_count = text.lines().count().max(1);
    requested.min(line_count.saturating_sub(viewport) as u16)
}

fn main() -> Result<()> {
    let args = Args::parse();
    if args.reset_keybindings {
        let path = reset_keybindings()?;
        println!("Default keybindings written to {}", path.display());
        return Ok(());
    }

    let mut load = load_app_config(args.safe);
    if args.no_confirm {
        load.config.interface.confirm_before_apply = false;
    }
    let extra_plugin_directory = args.plugin_dir.clone();
    let mut plugin_directories = load.config.plugins.directories.clone();
    if let Some(directory) = &extra_plugin_directory {
        plugin_directories.push(directory.display().to_string());
    }
    let temporary_plugin_approvals = args.allow_plugins.into_iter().collect::<HashSet<_>>();
    let plugin_policy = PluginPolicy {
        approved_adapters: load.config.plugins.approved_adapters.clone(),
        approved_transforms: load.config.plugins.approved_transforms.clone(),
        temporary_approvals: temporary_plugin_approvals.clone(),
        refresh_runtime: false,
    };
    let catalog = Catalog::discover(DiscoverOptions {
        app_config: load.config.clone(),
        plugin_directories,
        plugin_policy,
        include_generic_files: true,
    });
    let mut app = App::new(
        load,
        catalog,
        args.no_confirm,
        args.safe,
        extra_plugin_directory,
        temporary_plugin_approvals,
    );
    run(&mut app)
}

fn run(app: &mut App) -> Result<()> {
    enable_raw_mode().context("enable terminal raw mode")?;
    let mut output = stdout();
    if let Err(error) = execute!(output, EnterAlternateScreen, EnableMouseCapture) {
        let _ = disable_raw_mode();
        return Err(error).context("enter terminal alternate screen");
    }
    let backend = CrosstermBackend::new(output);
    let mut terminal = match Terminal::new(backend) {
        Ok(terminal) => terminal,
        Err(error) => {
            let _ = disable_raw_mode();
            let mut output = stdout();
            let _ = execute!(output, LeaveAlternateScreen, DisableMouseCapture);
            return Err(error).context("initialize terminal backend");
        }
    };
    if let Err(error) = terminal.clear() {
        let _ = disable_raw_mode();
        let _ = execute!(
            terminal.backend_mut(),
            LeaveAlternateScreen,
            DisableMouseCapture
        );
        return Err(error).context("clear terminal");
    }

    let result = (|| -> Result<()> {
        terminal.draw(|frame| app.ui(frame))?;
        loop {
            if app.should_quit {
                break Ok(());
            }
            if !event::poll(Duration::from_millis(100))? {
                if app.expire_pending_sequence() {
                    terminal.draw(|frame| app.ui(frame))?;
                }
                continue;
            }
            match event::read()? {
                Event::Key(key) => match app.handle_key(key)? {
                    Effect::None => {}
                    Effect::OpenEditor { source, working } => {
                        disable_raw_mode()?;
                        execute!(
                            terminal.backend_mut(),
                            LeaveAlternateScreen,
                            DisableMouseCapture
                        )?;
                        let editor_result = run_editor(&app.config.editor, &working);
                        execute!(
                            terminal.backend_mut(),
                            EnterAlternateScreen,
                            EnableMouseCapture
                        )?;
                        enable_raw_mode()?;
                        terminal.clear()?;
                        if let Err(error) = editor_result {
                            app.status = format!("Editor failed: {error}");
                            let _ = std::fs::remove_file(&working);
                        } else {
                            app.complete_external_edit(source, working);
                        }
                    }
                },
                Event::Resize(_, _) => {}
                _ => continue,
            }
            terminal.draw(|frame| app.ui(frame))?;
        }
    })();

    let cleanup = (|| -> Result<()> {
        disable_raw_mode()?;
        execute!(
            terminal.backend_mut(),
            LeaveAlternateScreen,
            DisableMouseCapture
        )?;
        terminal.show_cursor()?;
        Ok(())
    })();
    result.and(cleanup)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unicode_edit_cursor_moves_and_deletes_on_boundaries() {
        let mut value = "ab雾".to_owned();
        let mut cursor = value.len();
        move_cursor_left(&value, &mut cursor);
        assert_eq!(cursor, 2);
        delete_next_char(&mut value, cursor);
        assert_eq!(value, "ab");
        insert_char(&mut value, &mut cursor, '猫');
        assert_eq!(value, "ab猫");
    }

    #[test]
    fn default_view_parser_rejects_unknown_values() {
        assert_eq!(parse_view("plugins"), Some(View::Plugins));
        assert_eq!(parse_view("surprise"), None);
    }

    #[test]
    fn scope_navigation_skips_the_transient_search_scope() {
        assert_eq!(Scope::Overview.shifted(1), Scope::System);
        assert_eq!(Scope::Overview.shifted(-1), Scope::ConfigFiles);
        assert_eq!(Scope::Search.shifted(1), Scope::Overview);
    }

    #[test]
    fn terminal_display_removes_escape_and_carriage_return_but_keeps_layout() {
        assert_eq!(
            clean_terminal_text("\u{1b}[31mvalue\r\n\tsecond"),
            "[31mvalue\n\tsecond"
        );
    }
}
