use std::collections::HashSet;
use std::io::stdout;
use std::path::{Path, PathBuf};
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
use ratatui::style::{Color, Style};
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
    clean_display_text, Catalog, ConfigEntry, ConfigFile, DiscoverOptions, Transaction, ValueType,
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
    Plugins,
    Files,
    Search,
}

impl Scope {
    fn label(self) -> &'static str {
        match self {
            Self::Plugins => "插件",
            Self::Files => "文件",
            Self::Search => "搜索结果",
        }
    }

    fn shifted(self, direction: isize) -> Self {
        const SCOPES: [Scope; 2] = [Scope::Plugins, Scope::Files];
        if self == Scope::Search {
            return Scope::Plugins;
        }
        let current = SCOPES.iter().position(|scope| *scope == self).unwrap_or(0) as isize;
        SCOPES[(current + direction).rem_euclid(SCOPES.len() as isize) as usize]
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Focus {
    Left,
    Right,
}

#[derive(Clone, Debug)]
struct PluginGroup {
    provider: String,
    representative: Option<usize>,
    label: String,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Normal,
    Search,
    Edit,
    Select,
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
    plugin_groups: Vec<PluginGroup>,
    file_visible: Vec<usize>,
    selected: usize,
    selected_setting: usize,
    focus: Focus,
    plugin_selected: usize,
    scope: Scope,
    view: View,
    mode: Mode,
    edit_input: String,
    edit_cursor: usize,
    edit_options: Vec<String>,
    edit_option_selected: usize,
    search_input: String,
    search_cursor: usize,
    search_original_visible: Vec<usize>,
    search_original_selected: usize,
    search_original_scope: Scope,
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
            file_visible: Vec::new(),
            catalog,
            config: load.config,
            keymap,
            transaction: Transaction::default(),
            selected: 0,
            selected_setting: 0,
            focus: Focus::Left,
            plugin_groups: Vec::new(),
            plugin_selected: 0,
            scope: Scope::Plugins,
            view: initial_view,
            mode: Mode::Normal,
            edit_input: String::new(),
            edit_cursor: 0,
            edit_options: Vec::new(),
            edit_option_selected: 0,
            search_input: String::new(),
            search_cursor: 0,
            search_original_visible: Vec::new(),
            search_original_selected: 0,
            search_original_scope: Scope::Plugins,
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
            app.status = "就绪。真实配置文件仍是唯一事实来源。".to_owned();
        }
        app
    }

    fn selected_entry(&self) -> Option<&ConfigEntry> {
        self.setting_indices()
            .get(self.selected_setting)
            .and_then(|index| self.catalog.entries.get(*index))
    }

    fn selected_index(&self) -> Option<usize> {
        self.setting_indices().get(self.selected_setting).copied()
    }

    fn selected_group_entry(&self) -> Option<&ConfigEntry> {
        match self.scope {
            Scope::Files => self.current_file().and_then(|file| {
                self.catalog
                    .entries
                    .iter()
                    .find(|entry| entry.source_path().is_some_and(|path| path == file.path))
            }),
            Scope::Plugins => self
                .plugin_groups
                .get(self.selected)
                .and_then(|group| group.representative)
                .and_then(|index| self.catalog.entries.get(index)),
            Scope::Search => self
                .visible
                .get(self.selected)
                .and_then(|index| self.catalog.entries.get(*index)),
        }
    }

    fn current_file(&self) -> Option<&ConfigFile> {
        self.file_visible
            .get(self.selected)
            .and_then(|index| self.catalog.files.get(*index))
    }

    fn setting_indices(&self) -> Vec<usize> {
        match self.scope {
            Scope::Files => {
                let Some(file) = self.current_file() else {
                    return Vec::new();
                };
                self.catalog
                    .entries
                    .iter()
                    .enumerate()
                    .filter_map(|(index, entry)| {
                        entry
                            .source_path()
                            .is_some_and(|path| path == file.path)
                            .then_some(index)
                    })
                    .collect()
            }
            Scope::Plugins => {
                let Some(group) = self.selected_group_entry() else {
                    let Some(group) = self.plugin_groups.get(self.selected) else {
                        return Vec::new();
                    };
                    return self
                        .catalog
                        .entries
                        .iter()
                        .enumerate()
                        .filter_map(|(index, entry)| {
                            (entry.provider == group.provider).then_some(index)
                        })
                        .collect();
                };
                self.catalog
                    .entries
                    .iter()
                    .enumerate()
                    .filter_map(|(index, entry)| {
                        (entry.provider == group.provider).then_some(index)
                    })
                    .collect()
            }
            Scope::Search => self
                .visible
                .get(self.selected)
                .copied()
                .into_iter()
                .collect(),
        }
    }

    fn active_contexts(&self) -> &'static [&'static str] {
        match self.mode {
            Mode::Search => &["search"],
            Mode::Edit
            | Mode::Select
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
            Mode::Select => return self.handle_select_key(event),
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
                    if self.focus == Focus::Left {
                        self.selected = 0;
                        self.selected_setting = 0;
                    } else {
                        self.selected_setting = 0;
                    }
                } else if self.view == View::Plugins {
                    self.plugin_selected = 0;
                } else {
                    self.content_scroll = 0;
                }
            }
            "navigation.bottom" => {
                if self.view == View::Form {
                    if self.focus == Focus::Left {
                        self.selected = self.group_count().saturating_sub(1);
                        self.selected_setting = 0;
                    } else {
                        self.selected_setting = self.setting_indices().len().saturating_sub(1);
                    }
                } else if self.view == View::Plugins {
                    self.plugin_selected = self.catalog.plugins.len().saturating_sub(1);
                } else {
                    self.content_scroll = u16::MAX;
                }
            }
            "navigation.page_up" => self.navigate_or_scroll(-10),
            "navigation.page_down" => self.navigate_or_scroll(10),
            "navigation.left" => {
                if self.view == View::Form {
                    self.focus = Focus::Left;
                    self.status = "焦点：左侧插件/文件列表".to_owned();
                } else {
                    self.view = View::Form;
                    self.focus = Focus::Left;
                    self.content_scroll = 0;
                    self.status = "已返回设置视图；焦点在左侧列表".to_owned();
                }
            }
            "navigation.right" => {
                if self.view == View::Form {
                    self.focus = Focus::Right;
                    self.status = "焦点：右侧设置；e 编辑，Enter 激活".to_owned();
                } else {
                    self.view = View::Form;
                    self.focus = Focus::Right;
                    self.content_scroll = 0;
                    self.status = "已返回设置视图；焦点在右侧设置".to_owned();
                }
            }
            "navigation.activate" => {
                if matches!(self.view, View::Help) {
                    self.view = View::Form;
                    self.focus = Focus::Left;
                } else if self.view == View::Form && self.focus == Focus::Left {
                    self.focus = Focus::Right;
                    self.selected_setting = 0;
                    self.status = "已选择；焦点移到右侧设置".to_owned();
                } else if self.view == View::Form {
                    return self.start_edit();
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
                self.search_original_scope = self.scope;
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
                self.scope = self.search_original_scope;
                self.rebuild_visible_for_scope();
                self.status = "Search cancelled.".to_owned();
            }
            KeyCode::Enter => {
                self.mode = Mode::Normal;
                if self.search_input.is_empty() {
                    self.visible = std::mem::take(&mut self.search_original_visible);
                    self.selected = self
                        .search_original_selected
                        .min(self.visible.len().saturating_sub(1));
                    self.scope = self.search_original_scope;
                    self.rebuild_visible_for_scope();
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

    fn handle_select_key(&mut self, event: KeyEvent) -> Result<Effect> {
        if self.edit_options.is_empty() {
            self.mode = Mode::Normal;
            return Ok(Effect::None);
        }
        match event.code {
            KeyCode::Esc => {
                self.mode = Mode::Normal;
                self.edit_options.clear();
                self.status = "已取消选择".to_owned();
            }
            KeyCode::Enter => self.commit_option_edit()?,
            KeyCode::Up | KeyCode::Char('k') | KeyCode::Left | KeyCode::Char('h') => {
                self.edit_option_selected = (self.edit_option_selected + self.edit_options.len()
                    - 1)
                    % self.edit_options.len();
            }
            KeyCode::Down | KeyCode::Char('j') | KeyCode::Right | KeyCode::Char('l') => {
                self.edit_option_selected =
                    (self.edit_option_selected + 1) % self.edit_options.len();
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
        if self.focus == Focus::Left && self.scope == Scope::Files {
            let Some(file) = self.current_file().cloned() else {
                self.status = "没有可编辑的声明文件".to_owned();
                return Ok(Effect::None);
            };
            if !file.editable {
                self.status = "该声明文件只有只读设置，不能进行 Raw 编辑".to_owned();
                return Ok(Effect::None);
            }
            return self.start_raw_edit(file.path);
        }
        if self.focus == Focus::Left {
            self.focus = Focus::Right;
            self.selected_setting = 0;
            self.status = "已选择来源；焦点移到右侧设置".to_owned();
            return Ok(Effect::None);
        }
        let Some(index) = self.selected_index() else {
            if self.focus == Focus::Left {
                self.status = "请先选择右侧设置；文件 scope 可直接编辑整份文件".to_owned();
            }
            return Ok(Effect::None);
        };
        let entry = self.catalog.entries[index].clone();
        if !entry.is_editable() {
            self.status = "This entry is read-only.".to_owned();
            return Ok(Effect::None);
        }
        if matches!(entry.value_type, ValueType::Raw) || matches!(self.view, View::Raw) {
            let Some(source) = entry.source_path().map(PathBuf::from) else {
                self.status = "运行时来源没有可打开的 Raw 文件".to_owned();
                return Ok(Effect::None);
            };
            return self.start_raw_edit(source);
        }
        if matches!(entry.value_type, ValueType::Boolean | ValueType::Enum) {
            self.edit_options = if entry.value_type == ValueType::Boolean {
                vec!["true".to_owned(), "false".to_owned()]
            } else {
                entry
                    .metadata
                    .iter()
                    .find(|(key, _)| key == "values")
                    .map(|(_, values)| values.split('|').map(str::to_owned).collect())
                    .unwrap_or_default()
            };
            if self.edit_options.is_empty() {
                self.status = "该选项没有可用值，无法打开选项编辑".to_owned();
                return Ok(Effect::None);
            }
            self.edit_option_selected = self
                .edit_options
                .iter()
                .position(|option| option == &entry.value)
                .unwrap_or(0);
            self.mode = Mode::Select;
            self.focus = Focus::Right;
            return Ok(Effect::None);
        }
        self.edit_input = entry.value;
        self.edit_cursor = self.edit_input.len();
        self.mode = Mode::Edit;
        Ok(Effect::None)
    }

    fn start_raw_edit(&mut self, source: PathBuf) -> Result<Effect> {
        if is_system_path(&source) {
            self.status = "系统文件的 Raw 编辑需要授权；请使用受信任的结构化字段".to_owned();
            return Ok(Effect::None);
        }
        let working = match self.transaction.working_copy(&source) {
            Ok(working) => working,
            Err(error) => {
                self.status = format!("无法准备外部编辑副本：{error}");
                return Ok(Effect::None);
            }
        };
        self.status = format!("正在用外部编辑器打开完整文件：{}", source.display());
        Ok(Effect::OpenEditor { source, working })
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

    fn commit_option_edit(&mut self) -> Result<()> {
        let value = self
            .edit_options
            .get(self.edit_option_selected)
            .cloned()
            .unwrap_or_default();
        let Some(index) = self.selected_index() else {
            self.mode = Mode::Normal;
            return Ok(());
        };
        let entry = self.catalog.entries[index].clone();
        match self.transaction.stage_entry(&entry, &value) {
            Ok(()) => {
                self.catalog.entries[index].value = value.clone();
                self.status = format!("已暂存 {} = {}", entry.label, value);
                self.mode = Mode::Normal;
                self.edit_options.clear();
            }
            Err(error) => self.status = format!("无法暂存：{error}"),
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
        let selected_provider = self
            .selected_group_entry()
            .map(|entry| entry.provider.clone());
        let selected_file = self.current_file().map(|file| file.path.clone());
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
            self.scope = Scope::Plugins;
        }
        self.rebuild_visible_for_scope();
        self.selected = match self.scope {
            Scope::Files => selected_file
                .and_then(|path| self.catalog.files.iter().position(|file| file.path == path))
                .unwrap_or(0),
            Scope::Plugins => selected_provider
                .and_then(|provider| {
                    self.plugin_groups
                        .iter()
                        .position(|group| group.provider == provider)
                })
                .unwrap_or(0),
            Scope::Search => 0,
        };
        self.selected_setting = selected_id
            .and_then(|id| {
                self.setting_indices()
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
        let mut plugin_representatives = std::collections::BTreeMap::<String, usize>::new();
        for (index, entry) in self.catalog.entries.iter().enumerate() {
            plugin_representatives
                .entry(entry.provider.clone())
                .or_insert(index);
        }
        let mut plugin_providers = plugin_representatives
            .keys()
            .cloned()
            .collect::<std::collections::BTreeSet<_>>();
        for plugin in &self.catalog.plugins {
            plugin_providers.insert(format!("plugin.{}", plugin.id));
        }
        self.plugin_groups = plugin_providers
            .into_iter()
            .map(|provider| PluginGroup {
                label: plugin_representatives
                    .get(&provider)
                    .map(|index| self.group_label_for_entry(&self.catalog.entries[*index]))
                    .unwrap_or_else(|| self.group_label_for_provider(&provider)),
                representative: plugin_representatives.get(&provider).copied(),
                provider,
            })
            .collect();
        self.plugin_groups
            .sort_by(|left, right| left.label.cmp(&right.label));
        self.visible = match self.scope {
            Scope::Plugins => self
                .plugin_groups
                .iter()
                .filter_map(|group| group.representative)
                .collect(),
            Scope::Files => Vec::new(),
            Scope::Search => self.visible.clone(),
        };
        self.file_visible = if self.scope == Scope::Files {
            (0..self.catalog.files.len()).collect()
        } else {
            Vec::new()
        };
        self.selected = self.selected.min(self.group_count().saturating_sub(1));
        self.selected_setting = self
            .selected_setting
            .min(self.setting_indices().len().saturating_sub(1));
        self.content_scroll = 0;
    }

    fn change_scope(&mut self, direction: isize) {
        self.scope = self.scope.shifted(direction);
        self.selected = 0;
        self.selected_setting = 0;
        self.focus = Focus::Left;
        self.rebuild_visible_for_scope();
        self.status = format!(
            "范围：{}（{} 个来源）",
            self.scope.label(),
            self.group_count()
        );
    }

    fn refresh_search(&mut self) {
        self.visible = if self.search_input.is_empty() {
            (0..self.catalog.entries.len()).collect()
        } else {
            self.catalog.search_indices(&self.search_input)
        };
        self.scope = Scope::Search;
        self.file_visible.clear();
        self.selected = self.selected.min(self.visible.len().saturating_sub(1));
        self.selected_setting = 0;
        self.focus = Focus::Right;
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
        if self.focus == Focus::Left {
            let len = self.group_count();
            if len == 0 {
                return;
            }
            self.selected = ((self.selected as isize + amount).rem_euclid(len as isize)) as usize;
            self.selected_setting = 0;
            self.focus = Focus::Right;
            self.status = "已选中来源；焦点移到右侧设置".to_owned();
        } else {
            let len = self.setting_indices().len();
            if len == 0 {
                return;
            }
            self.selected_setting =
                ((self.selected_setting as isize + amount).rem_euclid(len as isize)) as usize;
        }
        self.content_scroll = 0;
    }

    fn group_count(&self) -> usize {
        match self.scope {
            Scope::Files => self.file_visible.len(),
            Scope::Plugins => self.plugin_groups.len(),
            Scope::Search => self.visible.len(),
        }
    }

    fn group_label_for_entry(&self, entry: &ConfigEntry) -> String {
        self.group_label_for_provider(&entry.provider)
    }

    fn group_label_for_provider(&self, provider: &str) -> String {
        if provider == "reginux.self" {
            return "Reginux · 内置".to_owned();
        }
        if provider.starts_with("linux.") {
            return format!("Linux · {}", provider.trim_start_matches("linux."));
        }
        if let Some(id) = provider.strip_prefix("plugin.") {
            if let Some(plugin) = self.catalog.plugins.iter().find(|plugin| plugin.id == id) {
                return format!("{} · {}", plugin.name, plugin.kind);
            }
            return id.to_owned();
        }
        provider.to_owned()
    }

    fn group_label(&self) -> String {
        match self.scope {
            Scope::Files => self
                .current_file()
                .map(ConfigFile::display_path)
                .unwrap_or_else(|| "没有声明文件".to_owned()),
            Scope::Plugins => self
                .plugin_groups
                .get(self.selected)
                .map(|group| group.label.clone())
                .unwrap_or_else(|| "没有来源".to_owned()),
            Scope::Search => self
                .selected_group_entry()
                .map(|entry| self.group_label_for_entry(entry))
                .unwrap_or_else(|| "没有来源".to_owned()),
        }
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
            " Reginux · {} · {} 个来源 / {} 个设置 · {} 个插件 · [s]/[s] 切换范围 ",
            self.scope.label(),
            self.group_count(),
            self.catalog.entries.len(),
            self.catalog.plugins.len()
        );
        frame.render_widget(
            Paragraph::new(title).style(palette::HEADER).block(
                Block::default()
                    .borders(Borders::BOTTOM)
                    .border_style(palette::PANEL),
            ),
            area,
        );
    }

    fn render_body(&self, frame: &mut Frame, area: Rect) {
        let columns = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(38), Constraint::Percentage(62)])
            .split(area);
        self.render_scope_list(frame, columns[0]);

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

    fn render_scope_list(&self, frame: &mut Frame, area: Rect) {
        let items = match self.scope {
            Scope::Plugins => self
                .plugin_groups
                .iter()
                .map(|group| {
                    let count = self
                        .catalog
                        .entries
                        .iter()
                        .filter(|candidate| candidate.provider == group.provider)
                        .count();
                    let changed = self
                        .catalog
                        .entries
                        .iter()
                        .filter(|candidate| candidate.provider == group.provider)
                        .any(|candidate| self.transaction.has_changes_for_entry(candidate));
                    ListItem::new(Line::from(vec![
                        Span::styled(
                            if changed { "● " } else { "  " },
                            if changed {
                                palette::STAGED
                            } else {
                                palette::MUTED
                            },
                        ),
                        Span::styled(clean_display_text(&group.label), palette::PRIMARY),
                        Span::styled(format!("  ({count})"), palette::MUTED),
                    ]))
                })
                .collect::<Vec<_>>(),
            Scope::Search => self
                .visible
                .iter()
                .map(|index| {
                    let entry = &self.catalog.entries[*index];
                    let count = self
                        .catalog
                        .entries
                        .iter()
                        .filter(|candidate| candidate.provider == entry.provider)
                        .count();
                    let changed = self
                        .catalog
                        .entries
                        .iter()
                        .filter(|candidate| candidate.provider == entry.provider)
                        .any(|candidate| self.transaction.has_changes_for_entry(candidate));
                    ListItem::new(Line::from(vec![
                        Span::styled(
                            if changed { "● " } else { "  " },
                            if changed {
                                palette::STAGED
                            } else {
                                palette::MUTED
                            },
                        ),
                        Span::styled(
                            clean_display_text(&self.group_label_for_entry(entry)),
                            palette::PRIMARY,
                        ),
                        Span::styled(format!("  ({count})"), palette::MUTED),
                    ]))
                })
                .collect::<Vec<_>>(),
            Scope::Files => self
                .file_visible
                .iter()
                .filter_map(|index| self.catalog.files.get(*index))
                .map(|file| {
                    let changed = self
                        .catalog
                        .entries
                        .iter()
                        .filter(|entry| entry.source_path().is_some_and(|path| path == file.path))
                        .any(|entry| self.transaction.has_changes_for_entry(entry));
                    let state = if file.exists { "● " } else { "◇ " };
                    let state_style = if file.exists {
                        palette::MUTED
                    } else {
                        palette::MISSING
                    };
                    ListItem::new(Line::from(vec![
                        Span::styled(
                            if changed { "● " } else { state },
                            if changed {
                                palette::STAGED
                            } else {
                                state_style
                            },
                        ),
                        Span::styled(compact_path(&file.path, 25), palette::PRIMARY),
                    ]))
                })
                .collect::<Vec<_>>(),
        };
        let title = match self.scope {
            Scope::Plugins => "插件 · 内置与已发现插件",
            Scope::Files => "文件 · 仅声明来源",
            Scope::Search => "搜索结果",
        };
        let list = List::new(items)
            .block(
                Block::default()
                    .title(title)
                    .borders(Borders::ALL)
                    .border_style(if self.focus == Focus::Left {
                        palette::FOCUS_LEFT
                    } else {
                        palette::PANEL
                    }),
            )
            .highlight_style(if self.focus == Focus::Left {
                palette::ACTIVE
            } else {
                palette::INACTIVE_SELECTED
            })
            .highlight_symbol("▸ ");
        let mut state = ListState::default();
        state.select((self.group_count() > 0).then_some(self.selected));
        frame.render_stateful_widget(list, area, &mut state);
    }

    fn render_form(&self, frame: &mut Frame, area: Rect) {
        let indices = self.setting_indices();
        if indices.is_empty() {
            let message = if self.scope == Scope::Files {
                "该文件目前没有结构化设置；按 e 在左侧打开完整文件".to_owned()
            } else if self.scope == Scope::Plugins {
                self.plugin_groups
                    .get(self.selected)
                    .and_then(|group| {
                        self.catalog
                            .plugins
                            .iter()
                            .find(|plugin| format!("plugin.{}", plugin.id) == group.provider)
                    })
                    .map(|plugin| {
                        format!(
                            "{}\n\n状态：{}\n批准：{}\n信任：{}\n路径：{}\n\n该插件当前没有可编辑设置；如需运行时设置，请先在插件管理视图审阅其权限。",
                            clean_display_text(&plugin.name),
                            clean_display_text(&plugin.status),
                            clean_display_text(&plugin.approval),
                            clean_display_text(&plugin.trust),
                            clean_display_text(&plugin.path.display().to_string())
                        )
                    })
                    .unwrap_or_else(|| "没有可显示的设置".to_owned())
            } else {
                "没有可显示的设置".to_owned()
            };
            frame.render_widget(
                Paragraph::new(message)
                    .block(
                        Block::default()
                            .title(format!("设置 · {}", self.group_label()))
                            .borders(Borders::ALL)
                            .border_style(palette::PANEL),
                    )
                    .style(palette::PRIMARY),
                area,
            );
            return;
        }
        let sections = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(5), Constraint::Length(7)])
            .split(area);
        let items = indices
            .iter()
            .map(|index| {
                let entry = &self.catalog.entries[*index];
                let indent = "  ".repeat(setting_depth(entry));
                let tree_path = entry
                    .section
                    .split(" / ")
                    .skip(2)
                    .collect::<Vec<_>>()
                    .join(" / ");
                let value = if entry.value == "<unset>" {
                    entry
                        .default_value
                        .as_deref()
                        .map(|default| format!("<默认: {default}>"))
                        .unwrap_or_else(|| "<未设置>".to_owned())
                } else {
                    clean_terminal_text(&entry.value)
                };
                let marker = if self.transaction.has_changes_for_entry(entry) {
                    "● "
                } else {
                    "  "
                };
                ListItem::new(Line::from(vec![
                    Span::styled(
                        format!(
                            "{}{} ",
                            indent,
                            if setting_depth(entry) > 0 {
                                "└─"
                            } else {
                                ""
                            }
                        ),
                        palette::MUTED,
                    ),
                    Span::styled(marker, palette::STAGED),
                    Span::styled(
                        if tree_path.is_empty() {
                            String::new()
                        } else {
                            format!("{tree_path} / ")
                        },
                        palette::MUTED,
                    ),
                    Span::styled(clean_display_text(&entry.label), palette::PRIMARY),
                    Span::styled(format!("  =  {value}"), value_style(entry)),
                    Span::styled(format!("  [{}]", entry.value_type.as_str()), palette::MUTED),
                ]))
            })
            .collect::<Vec<_>>();
        let list = List::new(items)
            .block(
                Block::default()
                    .title(format!("设置 · {}", self.group_label()))
                    .borders(Borders::ALL)
                    .border_style(if self.focus == Focus::Right {
                        palette::FOCUS_RIGHT
                    } else {
                        palette::PANEL
                    }),
            )
            .highlight_style(if self.focus == Focus::Right {
                palette::ACTIVE
            } else {
                palette::INACTIVE_SELECTED
            })
            .highlight_symbol("▸ ");
        let mut state = ListState::default();
        state.select((!indices.is_empty()).then_some(self.selected_setting));
        frame.render_stateful_widget(list, sections[0], &mut state);

        let entry = self.selected_entry();
        let detail = entry
            .map(|entry| {
                let mut text = format!(
                    "{}\n{}\n类型：{}  编辑：{}  权限：{}\n来源：{}",
                    clean_display_text(&entry.label),
                    clean_display_text(&entry.description),
                    entry.value_type.as_str(),
                    if entry.is_editable() {
                        "可编辑"
                    } else {
                        "只读"
                    },
                    entry.privilege.as_str(),
                    clean_display_text(&entry.source_display()),
                );
                if let Mode::Edit = self.mode {
                    text = format!(
                        "{}\n编辑：{}\nEnter 暂存 · Esc 取消",
                        clean_display_text(&entry.label),
                        clean_terminal_text(&display_with_cursor(
                            &self.edit_input,
                            self.edit_cursor
                        )),
                    );
                }
                if let Mode::Select = self.mode {
                    let options = self
                        .edit_options
                        .iter()
                        .enumerate()
                        .map(|(index, option)| {
                            if index == self.edit_option_selected {
                                format!("[● {option}]")
                            } else {
                                format!("[○ {option}]")
                            }
                        })
                        .collect::<Vec<_>>()
                        .join("  ");
                    text = format!(
                        "{}\n选择：{}\n↑/↓ 或 h/l 移动 · Enter 暂存 · Esc 取消",
                        clean_display_text(&entry.label),
                        options
                    );
                }
                text
            })
            .unwrap_or_else(|| "选择一个设置查看说明".to_owned());
        frame.render_widget(
            Paragraph::new(detail)
                .block(Block::default().title("说明").borders(Borders::ALL))
                .style(palette::SECONDARY)
                .wrap(Wrap { trim: false }),
            sections[1],
        );
    }

    fn render_raw(&self, frame: &mut Frame, area: Rect) {
        let path = self
            .current_file()
            .map(|file| file.path.clone())
            .or_else(|| {
                self.selected_entry()
                    .and_then(|entry| entry.source_path().map(PathBuf::from))
            });
        let Some(path) = path else {
            frame.render_widget(
                Paragraph::new(format!(
                    "运行时来源\n\n{}\n\n没有可打开的 Raw 文件；按 r 刷新已声明来源。",
                    clean_display_text(&self.group_label())
                ))
                .block(
                    Block::default()
                        .title("Raw · 运行时来源")
                        .borders(Borders::ALL),
                )
                .wrap(Wrap { trim: false }),
                area,
            );
            return;
        };
        let content = self
            .transaction
            .content_for(&path)
            .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
            .unwrap_or_else(|error| {
                format!(
                    "Cannot read {}: {}",
                    clean_display_text(&path.display().to_string()),
                    clean_display_text(&error.to_string())
                )
            });
        let missing = !path.exists();
        let content = clean_terminal_text(&content);
        let scroll = clamped_scroll(&content, area, self.content_scroll);
        frame.render_widget(
            Paragraph::new(content)
                .block(
                    Block::default()
                        .title(format!(
                            "Raw{}: {}",
                            if missing {
                                " · 默认路径，不存在"
                            } else {
                                ""
                            },
                            clean_display_text(&path.display().to_string())
                        ))
                        .borders(Borders::ALL)
                        .border_style(if missing {
                            palette::MISSING
                        } else {
                            palette::FOCUS_RIGHT
                        }),
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
            Mode::Select => "选择",
            Mode::ConfirmApply
            | Mode::ConfirmDiscard
            | Mode::ConfirmQuit
            | Mode::ConfirmPluginApprove
            | Mode::ConfirmPluginRevoke => "CONFIRM",
        };
        let view = match self.view {
            View::Form => "设置",
            View::Raw => "Raw",
            View::Diff => "差异",
            View::Info => "详情",
            View::Help => "帮助",
            View::Plugins => "插件管理",
        };
        let source = self
            .selected_entry()
            .map(|entry| clean_display_text(&entry.source_display()))
            .or_else(|| self.current_file().map(ConfigFile::display_path))
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
            Mode::Select => "↑/↓ 选择 · Enter 暂存 · Esc 取消".to_owned(),
            Mode::ConfirmApply => "Review impact · Enter/y Apply · Esc/n Cancel".to_owned(),
            Mode::ConfirmDiscard => "Enter/y Discard · Esc/n Cancel".to_owned(),
            Mode::ConfirmQuit => "Enter/y Quit · Esc/n Cancel".to_owned(),
            Mode::ConfirmPluginApprove => "Enter/y Approve exact digest · Esc/n Cancel".to_owned(),
            Mode::ConfirmPluginRevoke => "Enter/y Revoke approval · Esc/n Cancel".to_owned(),
            Mode::Normal => clean_terminal_text(&self.status),
        };
        let hint = if self.config.interface.show_key_hints && matches!(self.mode, Mode::Normal) {
            if self.view == View::Form {
                "h/l 焦点   j/k 导航   Enter 激活   e 编辑   d 差异   i 详情   [s/]s 范围"
                    .to_owned()
            } else {
                self.keymap.hints_contexts(self.active_contexts())
            }
        } else {
            String::new()
        };
        let first = format!(
            " {mode} │ {view} │ 焦点:{} │ {} staged │ {status} │ {source}",
            match self.focus {
                Focus::Left => "左侧",
                Focus::Right => "右侧",
            },
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

mod palette {
    use ratatui::style::{Color, Modifier, Style};

    pub const ACTIVE: Style = Style::new()
        .fg(Color::Black)
        .bg(Color::Cyan)
        .add_modifier(Modifier::BOLD);
    pub const INACTIVE_SELECTED: Style = Style::new()
        .fg(Color::White)
        .bg(Color::DarkGray)
        .add_modifier(Modifier::BOLD);
    pub const HEADER: Style = Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD);
    pub const PRIMARY: Style = Style::new().fg(Color::White);
    pub const SECONDARY: Style = Style::new().fg(Color::Gray);
    pub const MUTED: Style = Style::new().fg(Color::DarkGray);
    pub const STAGED: Style = Style::new().fg(Color::Yellow);
    pub const MISSING: Style = Style::new().fg(Color::Magenta).add_modifier(Modifier::BOLD);
    pub const PANEL: Style = Style::new().fg(Color::DarkGray);
    pub const FOCUS_LEFT: Style = Style::new().fg(Color::Yellow);
    pub const FOCUS_RIGHT: Style = Style::new().fg(Color::Cyan);
}

fn setting_depth(entry: &ConfigEntry) -> usize {
    entry.section.split(" / ").count().saturating_sub(2).min(4)
}

fn value_style(entry: &ConfigEntry) -> Style {
    if entry.value == "<unset>" {
        palette::MISSING
    } else if entry.value_type == ValueType::Secret {
        Style::default().fg(Color::Red)
    } else {
        Style::default().fg(Color::Green)
    }
}

fn compact_path(path: &Path, width: usize) -> String {
    let display = clean_display_text(&path.display().to_string());
    if display.chars().count() <= width {
        return display;
    }
    let mut suffix = String::new();
    for component in path.components().rev() {
        let part = component.as_os_str().to_string_lossy();
        let candidate = if suffix.is_empty() {
            part.to_string()
        } else {
            format!("{part}/{suffix}")
        };
        if candidate.chars().count() + 2 > width {
            break;
        }
        suffix = candidate;
    }
    if suffix.is_empty() {
        let tail = display
            .chars()
            .rev()
            .take(width.saturating_sub(2))
            .collect::<String>()
            .chars()
            .rev()
            .collect::<String>();
        format!("…{tail}")
    } else {
        format!("…/{suffix}")
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
    fn scope_navigation_switches_between_plugins_and_declared_files() {
        assert_eq!(Scope::Plugins.shifted(1), Scope::Files);
        assert_eq!(Scope::Plugins.shifted(-1), Scope::Files);
        assert_eq!(Scope::Search.shifted(1), Scope::Plugins);
    }

    #[test]
    fn terminal_display_removes_escape_and_carriage_return_but_keeps_layout() {
        assert_eq!(
            clean_terminal_text("\u{1b}[31mvalue\r\n\tsecond"),
            "[31mvalue\n\tsecond"
        );
    }

    #[test]
    fn compact_path_keeps_the_distinguishing_tail() {
        let path = Path::new("/home/example/.config/reginux/config.toml");
        assert_eq!(compact_path(path, 25), "…/reginux/config.toml");
    }
}
