use anyhow::Result;
use crossterm::event::{
    self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEventKind,
};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph, Wrap};
use ratatui::Terminal;
use std::io::{self, Stdout};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, Mutex};

use crate::activity::{ActivityKind, ActivityLog, ActivityState};
use crate::automation::TaskStore;
use crate::checkpoint;
use crate::clipboard;
use crate::compact;
use crate::composer::{self, Composer, PickItem, Picker, PickerKind};
use crate::config::{self, Config};
use crate::permissions::PermissionMode;
use crate::runtime::{Approval, ApprovalRequest, GoalState, GoalStatus, Runtime};
use crate::session::Session;
use crate::skills_search::{self, SkillEntry};
use crate::theme::Palette;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

const HELP: &str = "/goal <condition>  keep working until done  ·  /goal clear\n\
/btw <note>   send side note or question without interrupting the agent\n\
/seance <q>   summon insights & patterns from past sessions\n\
/todo [cmd]   track tasks & live progress  ·  /todo add|done|rm|clear\n\
/swarm <task> run 5-model multi-agent council in parallel\n\
/agents <name>: <task>  run a one-shot subagent in the background\n\
/collaborate <term:N>  pair-program with a live terminal on the bus\n\
/compact  summarize earlier history into this session\n\
/autocompact  on at 76% of the context budget  ·  /autocompact off|on|<1-99>\n\
/cost     tokens and session time  ·  /diff  git changes\n\
/review   model review of the working-copy diff\n\
/send <id|latest> <text>  message another session on the bus\n\
/inbox    read messages from other sessions\n\
/undo [files|chat|all|n]  granular restore: files, chat or both\n\
/redo     reapply the writes the last /undo reverted\n\
/checkpoints  list and restore this session's file checkpoints\n\
/fork [n]  branch this conversation at message count n\n\
/export   write a transcript  ·  /export [--format md|json] [--out path] [--force]\n\
/doctor   check config, provider and proxy\n\
/resume   reopen a previous session (same as ←)\n\
/effort   pick reasoning effort  ·  /effort <level>\n\
/permission  pick the permission mode (acceptEdits / prompt / bypass / plan)\n\
/plan <task>  propose a plan; writes and mutating commands are refused\n\
/permissions  show the active mode and loaded permission rules\n\
/status   runtime and session info\n\
/theme    live theme picker
/skills   list local skills  ·  /<skill> runs one\n\
/skills-search  find and install skills from skills.sh\n\
/sandbox  pick the sandbox mode\n\
/computer-use  toggle desktop operator & visual overlay (on/off/status)\n\
/quit     exit\n\
\n\
← sessions  ·  ↓ tasks  ·  ↑ history  ·  alt+v image  ·  shift+enter newline  ·  shift+tab permissions\n\
ctrl+o paste preview  ·  ctrl+d drop last paste";

#[derive(Clone)]
struct ChatLine {
    role: String,
    text: String,
}

enum UiEvent {
    Assistant(String),
    /// One streamed text token of the reply currently being generated.
    Delta(String),
    Tool(String),
    System(String),
    Error(String),
    Agent(String, String),
    /// A prompt-mode permission request: the text for the dialog plus, once
    /// captured from the approver channel, the request with the oneshot the
    /// dialog answers.
    Approval {
        text: String,
        request: Option<ApprovalRequest>,
    },
    McpUpdated {
        statuses: Vec<crate::mcp::McpServerStatus>,
        schemas: Vec<serde_json::Value>,
        note: String,
    },
    /// `/skills-search` results came back from the background task. `seq`
    /// matches the request the browser is still waiting for; stale replies
    /// are dropped.
    SkillsResults {
        seq: u64,
        res: Result<Vec<SkillEntry>>,
    },
    /// A SKILL.md preview fetch for one browser row finished.
    SkillsPreview {
        source: String,
        res: Result<String>,
    },
    /// A skill install finished: the result line, the skill's unset env
    /// requirements and the refreshed installed list.
    SkillsInstalled {
        source: String,
        res: Result<String>,
        missing_env: Vec<String>,
        installed: Vec<SkillEntry>,
    },
}

/// A permission request raised into a modal dialog. `request` is filled in
/// from the approver channel as soon as it lands there.
struct ApprovalDialog {
    text: String,
    request: Option<ApprovalRequest>,
}

/// Maps one keypress in the approval dialog to a vote. `y`/`n` are accepted
/// beside the historical `o`/`a`/`s`/`d` so the dialog matches the yes/no
/// wording operators expect; `None` means "keep the dialog open". Modified
/// chords (Ctrl+S, Alt+A, …) never vote: only bare characters and Esc do.
fn approval_for_key(code: KeyCode, modifiers: KeyModifiers) -> Option<Approval> {
    if modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) {
        return None;
    }
    match code {
        KeyCode::Char('o') | KeyCode::Char('O') | KeyCode::Char('y') | KeyCode::Char('Y') => {
            Some(Approval::AllowOnce)
        }
        KeyCode::Char('a') | KeyCode::Char('A') => Some(Approval::AllowAlways),
        KeyCode::Char('s') | KeyCode::Char('S') => Some(Approval::AllowAlwaysSaveRule),
        KeyCode::Char('d')
        | KeyCode::Char('D')
        | KeyCode::Char('n')
        | KeyCode::Char('N')
        | KeyCode::Esc => Some(Approval::Deny),
        _ => None,
    }
}

/// What Enter means inside the skills browser, depending on what is selected.
enum BrowserAction {
    /// Install this source (skills.sh slug, GitHub shorthand or raw URL).
    Install(String),
    /// Run a network search for the typed query.
    Search(String),
    /// The selected row is an installed skill: insert `/name` into the draft.
    Insert(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StatusKind {
    Info,
    Ok,
    Error,
}

/// Two-pane `/skills-search` explorer: left = search + skill rows, right =
/// preview and status lines. Network work runs on background threads that
/// report back through `UiEvent::Skills*`; the browser matches replies to
/// requests via a monotonically increasing `seq`.
struct SkillsBrowser {
    query: String,
    installed: Vec<SkillEntry>,
    results: Vec<SkillEntry>,
    selected: usize,
    preview_source: Option<String>,
    preview_text: String,
    preview_scroll: u16,
    status: Vec<(StatusKind, String)>,
    search_seq: u64,
    waiting_search: Option<u64>,
    pending_preview: Option<String>,
    preview_cache: HashMap<String, String>,
    preview_focus: bool,
    installing: HashSet<String>,
}

impl SkillsBrowser {
    fn new(installed: Vec<SkillEntry>, prefill: &str) -> Self {
        let mut b = Self {
            query: prefill.to_string(),
            installed,
            results: Vec::new(),
            selected: 0,
            preview_source: None,
            preview_text: String::new(),
            preview_scroll: 0,
            status: vec![(
                StatusKind::Info,
                "installed skills listed · type to search and press Enter".into(),
            )],
            search_seq: 0,
            waiting_search: None,
            pending_preview: None,
            preview_cache: HashMap::new(),
            preview_focus: false,
            installing: HashSet::new(),
        };
        b.clamp_selection();
        b.queue_preview();
        b
    }

    /// Rows shown in the left pane: installed first (marked `[yüklü]`), then
    /// search results. The query filters what is already known client-side;
    /// Enter runs the network search.
    fn rows(&self) -> Vec<(bool, &SkillEntry)> {
        let rows: Vec<_> = self
            .installed
            .iter()
            .map(|entry| (true, entry))
            .chain(
                self.results
                    .iter()
                    .filter(|entry| !self.installed.iter().any(|local| local.name == entry.name))
                    .map(|entry| (false, entry)),
            )
            .collect();
        let items: Vec<_> = rows
            .iter()
            .map(|(_, entry)| PickItem {
                id: entry.source.clone(),
                label: format!("{} {} {}", entry.name, entry.description, entry.source),
                detail: String::new(),
            })
            .collect();
        composer::fuzzy_indices(&self.query, &items)
            .into_iter()
            .map(|index| rows[index])
            .collect()
    }

    fn clamp_selection(&mut self) {
        let len = self.rows().len();
        if len == 0 {
            self.selected = 0;
        } else {
            self.selected = self.selected.min(len - 1);
        }
    }

    fn selected_source(&self) -> Option<String> {
        self.rows()
            .get(self.selected)
            .map(|(_, entry)| entry.source.clone())
    }

    fn note(&mut self, kind: StatusKind, text: String) {
        self.status.push((kind, text));
        if self.status.len() > 40 {
            self.status.remove(0);
        }
    }

    fn up(&mut self) {
        if self.selected > 0 {
            self.selected -= 1;
        }
        self.queue_preview();
    }

    fn down(&mut self) {
        self.selected = (self.selected + 1).min(self.rows().len().saturating_sub(1));
        self.queue_preview();
    }

    fn preview_scroll_up(&mut self) {
        self.preview_scroll = self.preview_scroll.saturating_sub(8);
    }

    fn preview_scroll_down(&mut self) {
        self.preview_scroll = self.preview_scroll.saturating_add(8);
    }

    fn type_char(&mut self, c: char) {
        self.query.push(c);
        self.selected = 0;
        self.queue_preview();
    }

    fn backspace(&mut self) {
        self.query.pop();
        self.selected = 0;
        self.queue_preview();
    }

    fn queue_preview(&mut self) {
        let Some(src) = self.selected_source() else {
            self.preview_source = None;
            self.preview_text.clear();
            self.pending_preview = None;
            return;
        };
        if self.preview_source.as_deref() != Some(src.as_str()) {
            self.preview_source = Some(src.clone());
            self.preview_scroll = 0;
            self.preview_text = self.preview_cache.get(&src).cloned().unwrap_or_default();
            self.pending_preview = (!self.preview_cache.contains_key(&src)).then_some(src);
        }
    }

    /// The preview fetch queued by the last navigation, if any.
    fn take_pending_preview(&mut self) -> Option<String> {
        self.pending_preview.take()
    }

    /// Enter: act on the selection, or the typed query when nothing matches.
    fn enter_action(&mut self) -> Option<BrowserAction> {
        let rows = self.rows();
        if let Some((installed, e)) = rows.get(self.selected) {
            if *installed {
                return Some(BrowserAction::Insert(format!("/{}", e.name)));
            }
            if self.query.trim().is_empty() || !rows.is_empty() && self.selected < rows.len() {
                return Some(BrowserAction::Install(e.source.clone()));
            }
        }
        let q = self.query.trim().to_string();
        if q.is_empty() {
            return None;
        }
        // A query that looks like a source installs directly; anything else
        // runs a network search.
        if q.contains('/') || q.starts_with("http") {
            Some(BrowserAction::Install(q))
        } else {
            Some(BrowserAction::Search(q))
        }
    }

    /// Start a search; returns the seq the reply must carry.
    fn request_search(&mut self, query: &str) -> u64 {
        self.search_seq += 1;
        self.waiting_search = Some(self.search_seq);
        self.note(StatusKind::Info, format!("searching: {query} (skills.sh)"));
        self.search_seq
    }

    fn search_done(&mut self, seq: u64, res: Result<Vec<SkillEntry>>) {
        if self.waiting_search != Some(seq) {
            return; // stale reply
        }
        self.waiting_search = None;
        match res {
            Ok(mut found) => {
                let n = found.len();
                found.retain(|e| !self.installed.iter().any(|i| i.name == e.name));
                self.results = found;
                self.selected = 0;
                self.clamp_selection();
                self.note(
                    StatusKind::Ok,
                    format!("{n} results found ({} new)", self.results.len()),
                );
                self.queue_preview();
            }
            Err(e) => self.note(StatusKind::Error, format!("search failed: {e}")),
        }
    }

    fn preview_done(&mut self, source: &str, res: Result<String>) {
        match res {
            Ok(text) => {
                self.preview_cache.insert(source.into(), text.clone());
                if self.preview_source.as_deref() == Some(source) {
                    self.preview_text = text;
                    self.preview_scroll = 0;
                }
            }
            Err(e) if self.preview_source.as_deref() == Some(source) => {
                self.preview_text = format!("preview failed: {e}");
                self.note(StatusKind::Error, self.preview_text.clone());
            }
            Err(_) => {}
        }
    }

    fn install_done(
        &mut self,
        source: &str,
        res: Result<String>,
        missing_env: Vec<String>,
        installed: Vec<SkillEntry>,
    ) {
        self.installing.remove(source);
        match res {
            Ok(name) => {
                self.note(StatusKind::Ok, format!("installed: {name}"));
                for var in &missing_env {
                    self.note(StatusKind::Error, format!("missing environment variable: {var}"));
                }
                if missing_env.is_empty() {
                    self.note(StatusKind::Info, format!("/{name} ready to use"));
                }
                self.installed = installed;
                self.results.retain(|e| e.source != source);
                self.clamp_selection();
            }
            Err(e) => self.note(StatusKind::Error, format!("install failed: {e}")),
        }
    }
}

/// Full-screen preview of one paste card: which card and the scroll offset.
struct PastePreview {
    card: usize,
    editor: composer::TextEditor,
}

impl PastePreview {
    fn open(composer: &Composer, card: usize) -> Option<Self> {
        Some(Self {
            card,
            editor: composer::TextEditor::new(composer.pastes().get(card)?.text.clone()),
        })
    }

    fn save(self, composer: &mut Composer) {
        composer.replace_paste(self.card, self.editor.text);
    }
}

struct SecretForm {
    fields: Vec<String>,
    values: Vec<String>,
    selected: usize,
    keyring: bool,
    error: String,
}

impl SecretForm {
    fn environment(fields: Vec<String>) -> Self {
        let values = vec![String::new(); fields.len()];
        Self {
            fields,
            values,
            selected: 0,
            keyring: false,
            error: String::new(),
        }
    }
    fn keyring(field: String) -> Self {
        let mut form = Self::environment(vec![field]);
        form.keyring = true;
        form
    }
    fn masked(&self, index: usize) -> String {
        "*".repeat(self.values[index].chars().count().min(40))
    }
    fn key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Tab | KeyCode::Down => {
                self.selected = (self.selected + 1) % self.fields.len().max(1)
            }
            KeyCode::BackTab | KeyCode::Up => self.selected = self.selected.saturating_sub(1),
            KeyCode::Backspace => {
                self.values[self.selected].pop();
            }
            KeyCode::Char(c)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                self.values[self.selected].push(c)
            }
            _ => {}
        }
    }
    fn submit_environment(&self) -> Result<()> {
        anyhow::ensure!(
            self.values.iter().all(|value| !value.trim().is_empty()),
            "all required values must be set"
        );
        for (field, value) in self.fields.iter().zip(&self.values) {
            anyhow::ensure!(
                !field.is_empty()
                    && field
                        .chars()
                        .all(|ch| ch.is_ascii_alphanumeric() || ch == '_'),
                "invalid environment variable name"
            );
            anyhow::ensure!(
                !value.contains('\0'),
                "environment values must not contain NUL"
            );
        }
        for (field, value) in self.fields.iter().zip(&self.values) {
            std::env::set_var(field, value);
        }
        Ok(())
    }
}

struct SettingsDialog {
    draft: Config,
    tab: usize,
    selected: usize,
    edit: Option<composer::TextEditor>,
    key_status: Vec<(String, bool)>,
    error: String,
}

impl SettingsDialog {
    const TABS: [&'static str; 5] = [
        "Theme",
        "Model / Provider",
        "Permissions",
        "Sandbox",
        "API Keys",
    ];
    fn new(cfg: Config) -> Self {
        let key_status = cfg.direct_secret_fields();
        Self {
            draft: cfg,
            tab: 0,
            selected: 0,
            edit: None,
            key_status,
            error: String::new(),
        }
    }
    fn rows(&self) -> Vec<(String, String)> {
        match self.tab {
            0 => crate::theme::choices()
                .into_iter()
                .map(|name| {
                    let selected = crate::theme::canonical(&self.draft.theme) == name;
                    (
                        name,
                        if selected {
                            "selected".into()
                        } else {
                            String::new()
                        },
                    )
                })
                .collect(),
            1 => vec![
                ("provider".into(), self.draft.provider.clone()),
                ("model".into(), self.draft.model.clone()),
                ("effort".into(), self.draft.effort.clone()),
                (
                    "auth".into(),
                    if crate::varynth_auth::signed_in() {
                        "signed-in".into()
                    } else {
                        "signed-out".into()
                    },
                ),
            ],
            2 => permission_picker(&self.draft.permission_mode)
                .items
                .into_iter()
                .map(|item| (item.id, item.detail))
                .collect(),
            3 => sandbox_picker(&self.draft.sandbox)
                .items
                .into_iter()
                .map(|item| (item.id, item.detail))
                .collect(),
            _ => self
                .key_status
                .iter()
                .map(|(name, present)| {
                    (
                        name.clone(),
                        if *present {
                            "******** (keyring)".into()
                        } else {
                            "not set".into()
                        },
                    )
                })
                .collect(),
        }
    }
    fn accept(&mut self) -> Option<String> {
        let selected = self.selected;
        match self.tab {
            0 => {
                if let Some(name) = crate::theme::choices().get(selected) {
                    self.draft.theme = name.clone();
                }
            }
            1 => match selected {
                0 => {
                    let ids = provider_ids(&self.draft);
                    let refs: Vec<&str> = ids.iter().map(String::as_str).collect();
                    let next = cycle(&self.draft.provider, &refs, 1);
                    self.draft.provider = next.clone();
                    if let Some(profile) = self
                        .draft
                        .provider_profiles
                        .iter()
                        .find(|profile| profile.id == next)
                    {
                        if let Some(model) = profile.models.first() {
                            self.draft.model = format!("{next}/{model}");
                        }
                    } else if next == "varynth" {
                        self.draft.model = "varynth/gpt-oss-120b".into();
                    }
                }
                1 => {
                    let mut editor = composer::TextEditor::new(self.draft.model.clone());
                    editor.cursor = editor.text.chars().count();
                    self.edit = Some(editor);
                }
                2 => self.draft.effort = cycle(&self.draft.effort, &Runtime::EFFORT_LEVELS, 1),
                _ => {}
            },
            2 => self.draft.permission_mode = permission_picker("").items[selected].id.clone(),
            3 => self.draft.sandbox = sandbox_picker("").items[selected].id.clone(),
            _ => return self.key_status.get(selected).map(|(name, _)| name.clone()),
        }
        None
    }
    fn sync_theme_row(&mut self) {
        if self.tab != 0 {
            return;
        }
        if let Some(name) = crate::theme::choices().get(self.selected) {
            self.draft.theme = name.clone();
        }
    }
    fn finish_edit(&mut self) {
        let Some(edit) = self.edit.take() else {
            return;
        };
        let value = edit.text.trim().to_string();
        if self.selected == 1 {
            self.draft.model = value;
        }
    }
}

fn provider_ids(cfg: &Config) -> Vec<String> {
    let mut ids: Vec<String> = cfg
        .provider_profiles
        .iter()
        .filter(|profile| profile.enabled)
        .map(|profile| profile.id.clone())
        .collect();
    if !ids.iter().any(|id| id == "varynth") {
        ids.insert(0, "varynth".into());
    }
    if !ids.iter().any(|id| id == &cfg.provider) {
        ids.push(cfg.provider.clone());
    }
    ids
}

fn cycle(current: &str, choices: &[&str], delta: isize) -> String {
    let index = choices
        .iter()
        .position(|value| *value == current)
        .unwrap_or(0) as isize;
    choices[(index + delta).rem_euclid(choices.len() as isize) as usize].into()
}

fn set_secret(cfg: &mut Config, field: &str, value: String) {
    match field {
        "proxy-token" => cfg.proxy_token = Some(value),
        "openai-api-key" => cfg.openai_api_key = Some(value),
        "anthropic-api-key" => cfg.anthropic_api_key = Some(value),
        "google-api-key" => cfg.google_api_key = Some(value),
        "telegram-bot-token" => cfg.telegram_bot_token = Some(value),
        "dashboard-token" => cfg.dashboard_token = Some(value),
        _ => {}
    }
}

fn persistence_config(cfg: &Config) -> Config {
    let mut safe = cfg.clone();
    safe.proxy_token = None;
    safe.openai_api_key = None;
    safe.anthropic_api_key = None;
    safe.google_api_key = None;
    for profile in &mut safe.provider_profiles {
        profile.api_key = None;
    }
    safe.telegram_bot_token = None;
    safe.dashboard_token = None;
    safe
}

fn apply_settings_with(
    rt: &mut Runtime,
    cfg: Config,
    persist: impl FnOnce(&Config) -> Result<()>,
) -> Result<()> {
    cfg.validate()?;
    crate::theme::resolve(&cfg.theme)?;
    anyhow::ensure!(!cfg.model.trim().is_empty(), "model must not be empty");
    cfg.require_provider_credentials()?;
    let _ = crate::providers::build(&cfg)?;
    persist(&persistence_config(&cfg))?;
    rt.reconfigure(cfg)
}

fn apply_settings(rt: &mut Runtime, cfg: Config) -> Result<()> {
    apply_settings_with(rt, cfg, Config::save)
}

fn change_setting(rt: &mut Runtime, field: &str, value: &str) -> Result<()> {
    let mut cfg = rt.cfg.clone();
    match field {
        "model" => cfg.select_model(value)?,
        "effort" => cfg.effort = value.into(),
        "permission" => cfg.permission_mode = value.into(),
        "sandbox" => cfg.sandbox = value.into(),
        _ => anyhow::bail!("unknown setting"),
    }
    apply_settings(rt, cfg)
}

struct ContextView {
    rows: Vec<(String, usize)>,
    used: usize,
    budget: usize,
}
impl ContextView {
    fn new(rt: &Runtime, session: &Session) -> Self {
        let mut rows = vec![
            (
                "System / instructions / memory".into(),
                rt.system_prompt().chars().count().div_ceil(4),
            ),
            (
                "MCP tool schemas".into(),
                serde_json::to_string(rt.mcp.schemas())
                    .unwrap_or_default()
                    .chars()
                    .count()
                    .div_ceil(4),
            ),
        ];
        for role in ["user", "assistant", "tool", "system"] {
            let messages: Vec<_> = session
                .messages
                .iter()
                .filter(|message| message.role == role)
                .cloned()
                .collect();
            rows.push((
                format!("{role} messages"),
                compact::estimate_messages(&messages).div_ceil(4),
            ));
        }
        let estimated: usize = rows.iter().map(|(_, tokens)| tokens).sum();
        rows.push((
            format!(
                "autocompact {}",
                if rt.cfg.auto_compact { "on" } else { "off" }
            ),
            if rt.cfg.auto_compact {
                compact::auto_compact_threshold(
                    compact::DEFAULT_MESSAGE_BUDGET,
                    rt.cfg.auto_compact_percent,
                )
            } else {
                0
            },
        ));
        Self {
            rows,
            used: estimated.max(rt.context_tokens(session)),
            budget: compact::DEFAULT_MESSAGE_BUDGET,
        }
    }
}

struct McpAdd {
    name: String,
    entry: composer::TextEditor,
    field: usize,
}
struct McpBrowser {
    statuses: Vec<crate::mcp::McpServerStatus>,
    schemas: Vec<serde_json::Value>,
    selected: usize,
    query: String,
    scroll: u16,
    add: Option<McpAdd>,
    note: String,
    waiting: bool,
}
impl McpBrowser {
    fn new(rt: &Runtime) -> Self {
        Self {
            statuses: rt.mcp.status(),
            schemas: rt.mcp.schemas().to_vec(),
            selected: 0,
            query: String::new(),
            scroll: 0,
            add: None,
            note: String::new(),
            waiting: false,
        }
    }
    fn rows(&self) -> Vec<PickItem> {
        self.statuses
            .iter()
            .map(|status| PickItem {
                id: status.name.clone(),
                label: status.name.clone(),
                detail: format!(
                    "{} {:?} {} tools",
                    status.transport,
                    status.status,
                    status.tool_count.unwrap_or(0)
                ),
            })
            .collect()
    }
    fn selected_name(&self) -> Option<String> {
        let rows = self.rows();
        let indices = composer::fuzzy_indices(&self.query, &rows);
        indices
            .get(self.selected)
            .map(|&index| rows[index].id.clone())
    }
    fn inspect(&self) -> String {
        let Some(name) = self.selected_name() else {
            return "No configured server matches".into();
        };
        let status = self
            .statuses
            .iter()
            .find(|status| status.name == name)
            .unwrap();
        let mut text = format!(
            "{}\n{}\nstatus: {:?}\ntools: {}\n",
            name,
            status.target,
            status.status,
            status.tool_count.unwrap_or(0)
        );
        if let Some(error) = &status.error {
            text.push_str(&format!("\n{error}\n"));
        }
        for schema in &self.schemas {
            let tool_name = schema
                .pointer("/function/name")
                .and_then(|value| value.as_str())
                .unwrap_or("");
            if tool_name.starts_with(&crate::mcp::mcp_tool_name(&name, "")) {
                text.push_str(&format!(
                    "\n{}\n",
                    serde_json::to_string_pretty(schema).unwrap_or_default()
                ));
            }
        }
        text
    }
}

fn installed_skills(rt: &Runtime) -> Vec<SkillEntry> {
    rt.skills
        .iter()
        .map(|skill| SkillEntry {
            name: skill.name.clone(),
            description: skill.description.clone(),
            source: skill.path.display().to_string(),
            installs: None,
        })
        .collect()
}

fn refresh_skill_commands(
    composer: &mut Composer,
    commands: &[crate::commands::Command],
    skills: &[crate::skills::Skill],
    installed: &[SkillEntry],
) {
    composer.commands = composer::builtin_commands();
    // User-defined commands sit after the built-ins; skills keep their old
    // place after them, and a skill never masks a command of the same name.
    composer.commands.extend(composer::command_items(commands));
    let taken: Vec<String> = composer
        .commands
        .iter()
        .map(|command| command.name.to_lowercase())
        .collect();
    composer.commands.extend(
        skills
            .iter()
            .filter(|skill| {
                let name = skill.name.to_lowercase();
                !taken.iter().any(|existing| existing == &name)
            })
            .map(|skill| composer::SlashItem::new(&skill.name, &skill.description)),
    );
    composer.skills = composer::skill_rows(installed, skills);
}

fn mention_catalog(
    personas: &[crate::agents::PersonaSpec],
    terminals: &[crate::terminals::TerminalInfo],
    sessions: &[crate::session::SessionMeta],
    self_id: &str,
) -> Vec<PickItem> {
    let mut rows = composer::mention_rows(personas, terminals, Some(self_id));
    rows.extend(sessions.iter().map(|session| PickItem {
        id: format!("session:{}", session.id),
        label: format!("@session:{}", session.id),
        detail: session_label(&session.title),
    }));
    rows
}

fn resolve_peer(
    handle: &str,
    terminals: &[crate::terminals::TerminalInfo],
    sessions: &[crate::session::SessionMeta],
) -> Result<String> {
    let handle = handle.trim_start_matches('@');
    if let Some(term) = handle.strip_prefix("term:") {
        let found = if let Ok(number) = term.parse::<usize>() {
            terminals
                .get(number.saturating_sub(1))
                .filter(|_| number > 0)
        } else {
            let mut found = terminals.iter().filter(|entry| {
                entry.id == term
                    || entry
                        .label
                        .as_deref()
                        .is_some_and(|label| label.eq_ignore_ascii_case(term))
            });
            let entry = found.next();
            anyhow::ensure!(found.next().is_none(), "terminal name is ambiguous");
            entry
        }
        .ok_or_else(|| anyhow::anyhow!("terminal not found: {handle}"))?;
        return found
            .session_id
            .clone()
            .ok_or_else(|| anyhow::anyhow!("terminal has no session address"));
    }
    let session = handle.strip_prefix("session:").unwrap_or(handle);
    if let Some(found) = sessions.iter().find(|entry| entry.id == session) {
        return Ok(found.id.clone());
    }
    let mut matches = sessions
        .iter()
        .filter(|entry| entry.id.starts_with(session) || entry.title.eq_ignore_ascii_case(session));
    let found = matches
        .next()
        .ok_or_else(|| anyhow::anyhow!("session not found: {session}"))?;
    anyhow::ensure!(matches.next().is_none(), "session name is ambiguous");
    Ok(found.id.clone())
}

fn spawn_agent(
    tx: mpsc::UnboundedSender<UiEvent>,
    cfg: Config,
    root: PathBuf,
    name: String,
    prompt: String,
) {
    tokio::spawn(async move {
        let event = match crate::agents::run(&cfg, &root, &name, &prompt).await {
            Ok(result) => UiEvent::Agent(result.name, result.reply),
            Err(error) => UiEvent::System(format!("agent {name} failed: {error}")),
        };
        let _ = tx.send(event);
    });
}

/// Heartbeat bookkeeping for this terminal's registry entry: re-announce
/// when more than [`HEARTBEAT_EVERY`] has passed since the last beat so
/// peers keep seeing this TUI as live.
struct Heartbeat {
    last: Instant,
}

/// Registered terminals go stale after 5 minutes; 60 s beats keep us fresh.
const HEARTBEAT_EVERY: Duration = Duration::from_secs(60);

impl Heartbeat {
    fn new() -> Self {
        Self {
            last: Instant::now(),
        }
    }

    /// True when a re-announce is due at `now`.
    fn due(&self, now: Instant) -> bool {
        now.duration_since(self.last) > HEARTBEAT_EVERY
    }

    fn beat(&mut self, now: Instant) {
        self.last = now;
    }
}

/// Live text of the reply being streamed into the chat. Deltas append; the
/// final assistant message, an error or an interrupt drops the buffer so the
/// full reply replaces it without duplicated text on screen.
struct StreamBuffer {
    text: String,
    live: bool,
}

impl StreamBuffer {
    fn new() -> Self {
        Self {
            text: String::new(),
            live: false,
        }
    }

    fn push(&mut self, delta: &str) {
        self.live = true;
        self.text.push_str(delta);
    }

    fn reset(&mut self) {
        self.live = false;
        self.text.clear();
    }

    /// The live streamed text, when a stream is (or was) in progress.
    fn live_text(&self) -> Option<&str> {
        self.live.then_some(self.text.as_str())
    }
}

fn unknown_slash(line: &str, commands: &[composer::SlashItem]) -> Option<String> {
    let rest = line.strip_prefix('/')?;
    let name = rest.split([' ', ':']).next().unwrap_or("");
    if name.is_empty() || name == "exit" {
        return None;
    }
    // A path or dotted token is prose (`/etc/hosts …`), not a command typo.
    if name.contains(['/', '\\', '.']) {
        return None;
    }
    let known = composer::builtin_commands()
        .iter()
        .any(|item| item.name == name)
        || commands.iter().any(|item| item.name == name);
    if known {
        None
    } else {
        Some(name.to_string())
    }
}

/// Nearest built-in or user command name within edit distance 2.
fn slash_suggestion(name: &str, commands: &[composer::SlashItem]) -> Option<String> {
    let mut best: Option<(usize, &str)> = None;
    let builtins = composer::builtin_commands();
    for item in builtins.iter().chain(commands.iter()) {
        let distance = edit_distance(name, &item.name);
        if distance <= 2 && best.is_none_or(|(best_distance, _)| distance < best_distance) {
            best = Some((distance, item.name.as_str()));
        }
    }
    best.map(|(_, name)| name.to_string())
}

/// Levenshtein distance over chars; both strings here are short command names.
fn edit_distance(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut previous: Vec<usize> = (0..=b.len()).collect();
    let mut current = vec![0usize; b.len() + 1];
    for (i, ca) in a.iter().enumerate() {
        current[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let substitution = previous[j] + usize::from(ca != cb);
            current[j + 1] = substitution.min(previous[j + 1] + 1).min(current[j] + 1);
        }
        std::mem::swap(&mut previous, &mut current);
    }
    previous[b.len()]
}

/// The reply for an unknown slash command: the nearest command when one is
/// close, otherwise the usage line.
fn unknown_slash_reply(name: &str, commands: &[composer::SlashItem]) -> String {
    let shown: String = name
        .chars()
        .filter(|ch| !ch.is_control())
        .take(64)
        .collect();
    match slash_suggestion(name, commands) {
        Some(suggestion) => format!("!! unknown command /{shown}\ndid you mean /{suggestion}?"),
        None => format!("!! unknown command /{shown}\ntry: /help"),
    }
}

fn show_brand_splash(terminal: &mut Terminal<CrosstermBackend<Stdout>>, theme: &str) -> Result<()> {
    if !crate::brand::splash_allowed() {
        return Ok(());
    }
    let boot = std::time::Instant::now();
    while boot.elapsed() < std::time::Duration::from_millis(1400) {
        terminal.draw(|frame| {
            crate::theme::with_theme(theme, || {
                let area = frame.area();
                let painted = crate::gradient::fill(
                    frame,
                    area,
                    crate::theme::active_gradient(),
                    crate::theme::active_truecolor(),
                    Palette::bg(),
                );
                if !painted {
                    frame.render_widget(
                        Paragraph::new(Span::styled(
                            "─".repeat(area.width as usize),
                            Style::default().fg(Palette::accent()),
                        )),
                        Rect {
                            x: 0,
                            y: 1,
                            width: area.width,
                            height: 1,
                        },
                    );
                }
                let width = crate::brand::MARK_W.min(area.width);
                let height = crate::brand::MARK_H;
                let x = area.width.saturating_sub(width) / 2;
                let y = area.height.saturating_sub(height) / 2;
                crate::brand::paint_mark(frame, x, y, crate::brand::color_allowed());
            });
        })?;
        if event::poll(std::time::Duration::from_millis(16))? {
            if let Event::Key(key) = event::read()? {
                if key.kind == KeyEventKind::Press {
                    break;
                }
            }
        }
    }
    Ok(())
}

pub async fn run(rt: Runtime, session: Session) -> Result<()> {
    let theme_name = rt.cfg.theme.clone();
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(
        stdout,
        EnterAlternateScreen,
        EnableBracketedPaste,
        EnableMouseCapture
    )?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    terminal.clear()?;

    show_brand_splash(&mut terminal, &theme_name)?;

    let result = run_loop(&mut terminal, rt, session).await;

    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        DisableBracketedPaste,
        DisableMouseCapture,
        LeaveAlternateScreen
    )?;
    terminal.show_cursor()?;
    if let Ok(Some(id)) = &result {
        println!("\nsession {id}\n  continue: varynth resume {id}\n");
    }
    result.map(|_| ())
}

/// Whether `ev` can take part in a paste burst: a bracketed-paste event,
/// or a plain editing key press — a character without Ctrl/Alt, an
/// unmodified Enter, or Backspace.
fn is_editing_event(ev: &Event) -> bool {
    match ev {
        Event::Paste(_) => true,
        Event::Key(key) if key.kind == KeyEventKind::Press => match key.code {
            KeyCode::Char(_) => !key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT),
            KeyCode::Enter => key.modifiers.is_empty(),
            KeyCode::Backspace => true,
            _ => false,
        },
        _ => false,
    }
}

fn is_key_release(ev: &Event) -> bool {
    matches!(ev, Event::Key(key) if key.kind == KeyEventKind::Release)
}

/// Fold one editing event into the burst `text`, returning whether it was
/// a bracketed-paste event. Backspace removes one character from the burst
/// text (not from the draft); anything else returns `None`.
fn push_editing_event(ev: &Event, text: &mut String) -> Option<bool> {
    if !is_editing_event(ev) {
        return None;
    }
    match ev {
        Event::Paste(pasted) => {
            text.push_str(pasted);
            Some(true)
        }
        Event::Key(key) => match key.code {
            KeyCode::Char(c) => {
                text.push(c);
                Some(false)
            }
            KeyCode::Enter => {
                text.push('\n');
                Some(false)
            }
            KeyCode::Backspace => {
                text.pop();
                Some(false)
            }
            _ => None,
        },
        _ => None,
    }
}

/// Outcome of folding a possible paste burst that starts at one event.
enum PasteBurst {
    /// More than one editing event, any bracketed paste, or a newline was
    /// folded: the whole `text` goes to `Composer::paste` as one unit.
    /// `pending` holds a non-editing event that had to be read to classify
    /// it; it is replayed on the next loop iteration instead of dropped.
    Burst {
        text: String,
        pending: Option<Event>,
    },
    /// Not a burst: `first` still needs its normal handling, and `pending`
    /// (when set) is replayed on the next loop iteration.
    Single {
        first: Event,
        pending: Option<Event>,
    },
}

/// Drain already-buffered editing keys and paste events behind `first`
/// into one string. The caller has checked via `event::poll` that more
/// input is already waiting; this only ever reads events `poll` reports
/// as waiting, so it never blocks. Draining stops before the first
/// non-editing event: that event cannot be unread, so it is returned as
/// `pending` for the caller to replay rather than being consumed here.
fn drain_paste_burst(first: Event) -> Result<PasteBurst> {
    let mut text = String::new();
    let Some(first_was_paste) = push_editing_event(&first, &mut text) else {
        return Ok(PasteBurst::Single {
            first,
            pending: None,
        });
    };
    let mut saw_paste = first_was_paste;
    let mut folded = 1usize;
    let mut pending = None;
    while event::poll(Duration::ZERO)? {
        let next = event::read()?;
        match push_editing_event(&next, &mut text) {
            Some(was_paste) => {
                saw_paste |= was_paste;
                folded += 1;
            }
            None if is_key_release(&next) => {}
            None => {
                pending = Some(next);
                break;
            }
        }
    }
    if folded > 1 || saw_paste {
        Ok(PasteBurst::Burst { text, pending })
    } else {
        Ok(PasteBurst::Single { first, pending })
    }
}

/// After a plain Enter, wait briefly for more typing. A paste that missed
/// bracketed mode arrives as keys a few milliseconds apart; a real Enter
/// is followed by silence. Returns the extra text (without the Enter) and
/// a non-editing event that had to be read, if any.
fn collect_paste_tail(text: &mut String) -> Result<Option<Event>> {
    let idle = Duration::from_millis(60);
    let deadline = Instant::now() + Duration::from_millis(700);
    let mut pending = None;
    loop {
        if Instant::now() >= deadline {
            break;
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if !event::poll(idle.min(left))? {
            break;
        }
        let next = event::read()?;
        match push_editing_event(&next, text) {
            Some(_) => {}
            None if is_key_release(&next) => {}
            None => {
                pending = Some(next);
                break;
            }
        }
    }
    Ok(pending)
}

/// True when this Enter belongs to a paste. The newline stays in the draft
/// and nothing is submitted or queued.
fn absorb_paste_continuation(
    composer: &mut Composer,
    burst_pending: &mut Option<Event>,
) -> Result<bool> {
    let mut extra = String::new();
    let pending = collect_paste_tail(&mut extra)?;
    if extra.is_empty() {
        if let Some(event) = pending {
            *burst_pending = Some(event);
        }
        return Ok(false);
    }
    *burst_pending = pending;
    composer.insert_char('\n');
    composer.paste(&extra);
    Ok(true)
}

async fn run_loop(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    rt: Runtime,
    session: Session,
) -> Result<Option<String>> {
    let cwd = rt.jail.cwd.display().to_string();
    let _agents = rt.agents_md_line();
    let mut log = chat_log(&session);

    // Prompt-mode approvals: the runtime hands requests over this channel
    // and the modal dialog answers them. The receiver is shared with the
    // turn task so the UI event envelope can carry the oneshot along.
    let (approval_tx, approval_rx) = mpsc::unbounded_channel::<ApprovalRequest>();
    let approval_rx = Arc::new(std::sync::Mutex::new(approval_rx));
    let mut rt = rt;
    rt.approver = Some(approval_tx);
    let side_notes = rt.side_notes.clone();

    let registry = crate::terminals::Registry::default();
    let mut current_session_id = session.id().to_string();
    let registration = registry.register(session.id(), &cwd, &session.meta.title)?;
    let mut heartbeat = Heartbeat::new();
    let personas = crate::agents::personas();
    let mut peer_terms = registry.live();
    let mut session_rows = Session::list().unwrap_or_default();
    let mut installed = installed_skills(&rt);
    let mut composer = Composer::default();
    composer.workspace_files = composer::scan_jail_files(&rt.jail);
    composer.mentions = mention_catalog(&personas, &peer_terms, &session_rows, &registration.id);
    refresh_skill_commands(&mut composer, &rt.commands, &rt.skills, &installed);
    let shared = Arc::new(Mutex::new((rt, session)));
    let (tx, mut rx) = mpsc::unbounded_channel::<UiEvent>();
    let mut picker: Option<Picker> = None;
    let mut notice: Option<(String, Instant)> = None;
    let mut busy = false;
    // Pending prompt-mode permission request, shown as a modal dialog.
    let mut dialog: Option<ApprovalDialog> = None;
    // Full-screen preview of a paste card, opened with Ctrl+O.
    let mut paste_preview: Option<PastePreview> = None;
    // Non-editing event read while classifying a paste burst; crossterm
    // cannot unread it, so it is replayed as the next loop's event.
    let mut burst_pending: Option<Event> = None;
    // Full-width `/skills-search` explorer: search, preview and install
    // skills from skills.sh. Network work runs in background tasks that
    // report back through the UiEvent channel.
    let mut skills: Option<SkillsBrowser> = None;
    let mut settings: Option<SettingsDialog> = None;
    let mut context: Option<ContextView> = None;
    let mut mcp: Option<McpBrowser> = None;
    let mut model_browser: Option<ModelBrowser> = None;
    let mut secret_form: Option<SecretForm> = None;
    let mut reload_skills = false;
    let mut pending_agent_results: Vec<(String, String)> = Vec::new();
    let mut snapshot = None;
    // Text streamed so far for the reply currently being generated; dropped
    // when the full assistant message lands (or the turn errors/stops).
    let mut stream = StreamBuffer::new();
    // The running turn, so Esc / Ctrl+Enter can stop it. Messages sent while
    // it runs wait in the composer's queue and go out one by one.
    let mut turn_task: Option<tokio::task::JoinHandle<()>> = None;
    let mut spinner = 0usize;
    // Rows above the bottom of the chat. 0 sticks to the latest line.
    let mut scroll: usize = 0;
    // Double-press state: Ctrl+C twice quits, ← twice walks back through
    // sessions while a single ← still opens the session picker.
    let mut last_ctrl_c: Option<Instant> = None;
    let mut pending_left: Option<Instant> = None;
    let mut quit_session: Option<String> = None;
    let frames = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
    let modes = [
        PermissionMode::AcceptEdits,
        PermissionMode::Prompt,
        PermissionMode::Bypass,
        PermissionMode::Plan,
    ];

    loop {
        while let Ok(ev) = rx.try_recv() {
            match ev {
                UiEvent::Assistant(t) => {
                    stream.reset();
                    log.push(ChatLine {
                        role: "assistant".into(),
                        text: t,
                    });
                    busy = false;
                    scroll = 0;
                    // The turn is over; a leftover dialog is dead weight.
                    dialog = None;
                }
                UiEvent::Delta(t) => {
                    stream.push(&t);
                    scroll = 0;
                }
                UiEvent::Tool(t) => log.push(ChatLine {
                    role: "tool".into(),
                    text: t,
                }),
                UiEvent::System(t) => log.push(ChatLine {
                    role: "system".into(),
                    text: t,
                }),
                UiEvent::Agent(name, t) => {
                    pending_agent_results.push((name.clone(), t.clone()));
                    log.push(ChatLine {
                        role: "assistant".into(),
                        text: format!("[agent {name}]  {t}"),
                    });
                }
                UiEvent::Error(t) => {
                    stream.reset();
                    log.push(ChatLine {
                        role: "error".into(),
                        text: t,
                    });
                    busy = false;
                    dialog = None;
                }
                UiEvent::Approval { text, request } => {
                    if dialog.is_none() {
                        dialog = Some(ApprovalDialog { text, request });
                    } else if let Some(d) = dialog.as_mut() {
                        // The poll may have opened this dialog from the
                        // channel already; only fill in the missing oneshot.
                        if d.request.is_none() {
                            d.request = request;
                        } else if let Some(req) = request {
                            let _ = req.respond.send(Approval::Deny);
                        }
                    }
                }
                UiEvent::McpUpdated {
                    statuses,
                    schemas,
                    note: message,
                } => {
                    if let Some(browser) = mcp.as_mut() {
                        browser.statuses = statuses;
                        browser.schemas = schemas;
                        browser.note = message;
                        browser.waiting = false;
                    }
                }
                UiEvent::SkillsResults { seq, res } => {
                    if let Some(b) = skills.as_mut() {
                        b.search_done(seq, res);
                    }
                }
                UiEvent::SkillsPreview { source, res } => {
                    if let Some(b) = skills.as_mut() {
                        b.preview_done(&source, res);
                    }
                }
                UiEvent::SkillsInstalled {
                    source,
                    res,
                    missing_env,
                    installed,
                } => {
                    let successful = res.is_ok();
                    if successful {
                        reload_skills = true;
                        if !missing_env.is_empty() {
                            secret_form = Some(SecretForm::environment(missing_env.clone()));
                        }
                    }
                    if let Some(b) = skills.as_mut() {
                        b.install_done(&source, res, missing_env, installed);
                    }
                }
            }
        }
        // Complete a dialog still missing its oneshot, or open one for a
        // request whose pairing event never landed (aborted turn task).
        if let Ok(mut rx) = approval_rx.lock() {
            while let Ok(req) = rx.try_recv() {
                if dialog.is_none() {
                    dialog = Some(ApprovalDialog {
                        text: format!("{} {}", req.tool, req.detail),
                        request: Some(req),
                    });
                } else if let Some(d) = dialog.as_mut() {
                    match d.request {
                        None => d.request = Some(req),
                        // Only one request is ever outstanding; a second is a
                        // leftover from an aborted turn.
                        Some(_) => {
                            let _ = req.respond.send(Approval::Deny);
                        }
                    }
                }
            }
        }

        // A running turn holds the lock until the model replies; never wait on
        // it here or the UI freezes. Reuse the last snapshot while it's held.
        if let Ok(mut g) = shared.try_lock() {
            current_session_id = g.1.id().to_string();
            if reload_skills {
                g.0.refresh_skills();
                installed = installed_skills(&g.0);
                refresh_skill_commands(&mut composer, &g.0.commands, &g.0.skills, &installed);
                if let Some(browser) = skills.as_mut() {
                    browser.installed = installed.clone();
                    browser.clamp_selection();
                    browser.queue_preview();
                }
                reload_skills = false;
            }
            for (name, text) in pending_agent_results.drain(..) {
                let _ = g.1.append(crate::session::ChatMessage {
                    role: "system".into(),
                    content: format!("[agent {name}] {text}"),
                    ..Default::default()
                });
            }
            if heartbeat.due(Instant::now()) {
                if let Err(error) = registration.refresh(g.1.id(), &cwd, &g.1.meta.title) {
                    notice = Some((format!("terminal heartbeat: {error}"), Instant::now()));
                }
                heartbeat.beat(Instant::now());
                peer_terms = registry.live();
                session_rows = Session::list().unwrap_or_default();
            }
            snapshot = Some((
                g.0.cfg.model.clone(),
                g.0.cfg.sandbox.clone(),
                g.0.permission,
                g.0.cfg.provider.clone(),
                g.0.cfg.effort.clone(),
                g.0.goal.clone(),
                g.1.meta.title.clone(),
                g.0.agent_files.memory(),
                g.0.cfg.theme.clone(),
                g.0.cfg.auto_compact,
                g.0.cfg.auto_compact_percent,
                {
                    let tokens = g.0.context_tokens(&g.1);
                    let percent =
                        tokens.saturating_mul(100) / crate::compact::DEFAULT_MESSAGE_BUDGET.max(1);
                    u8::try_from(percent).ok()
                },
                {
                    let servers = g.0.mcp.servers.len();
                    let tools = g.0.mcp.tool_schemas.len();
                    format!("{servers} servers · {tools} tools")
                },
                {
                    let mut names: Vec<String> = Vec::new();
                    for skill in &g.0.skills {
                        if !skill.name.eq_ignore_ascii_case("motion-frames") && !skill.name.eq_ignore_ascii_case("motion-fream") && !skill.name.eq_ignore_ascii_case("motion-fream-pro") {
                            names.push(skill.name.clone());
                        }
                    }
                    names
                },
            ));
        }
        composer.mentions =
            mention_catalog(&personas, &peer_terms, &session_rows, &registration.id);
        if let Some(browser) = skills.as_mut() {
            if let Some(source) = browser.take_pending_preview() {
                spawn_skill_preview(tx.clone(), source);
            }
        }
        let Some((
            model,
            sandbox,
            perm,
            provider,
            effort,
            goal,
            session_title,
            memory,
            theme,
            auto_compact,
            auto_percent,
            context_percent,
            mcp_summary,
            skills_list,
        )) = snapshot.clone()
        else {
            tokio::time::sleep(Duration::from_millis(10)).await;
            continue;
        };

        // The goal badge reads its own elapsed time; nothing cached here.

        if notice
            .as_ref()
            .is_some_and(|(_, at)| at.elapsed() > Duration::from_secs(4))
        {
            notice = None;
        }
        spinner = spinner.wrapping_add(1);
        // A single ← opens the session picker, but only after the double-press
        // window closes, so ← ← can mean "previous session" instead.
        if pending_left.is_some_and(|at| at.elapsed() > Duration::from_millis(400)) {
            pending_left = None;
            if picker.is_none() && skills.is_none() && dialog.is_none() && composer.is_empty() {
                picker = Some(sessions_picker());
            }
        }
        let mut max_scroll = 0;
        let preview_theme = settings
            .as_ref()
            .map(|dialog| dialog.draft.theme.clone())
            .unwrap_or_else(|| theme.clone());
        let show_hints = picker.is_none()
            && skills.is_none()
            && dialog.is_none()
            && paste_preview.is_none()
            && settings.is_none()
            && context.is_none()
            && mcp.is_none()
            && model_browser.is_none()
            && secret_form.is_none();
        terminal.draw(|f| {
            crate::theme::with_theme(&preview_theme, || {
                max_scroll = draw(
                    f,
                    &log,
                    stream.live_text(),
                    &composer,
                    picker.as_ref(),
                    notice.as_ref().map(|(n, _)| n.as_str()),
                    busy,
                    frames[spinner % frames.len()],
                    spinner,
                    scroll,
                    &cwd,
                    &model,
                    &sandbox,
                    perm,
                    &provider,
                    &effort,
                    goal.as_ref(),
                    &session_title,
                    &memory,
                    skills.as_ref(),
                    None,
                    auto_compact,
                    auto_percent,
                    context_percent,
                    &mcp_summary,
                    &skills_list,
                    show_hints,
                )
                .clamp_scroll;
                if let Some(pv) = &paste_preview {
                    draw_paste_preview(f, pv, &composer);
                }
                if let Some(view) = &context {
                    draw_context(f, view);
                }
                if let Some(browser) = &mcp {
                    draw_mcp(f, browser);
                }
                if let Some(mb) = &model_browser {
                    draw_model_browser(f, mb);
                }
                if let Some(cfg) = &settings {
                    draw_settings(f, cfg);
                }
                if let Some(form) = &secret_form {
                    draw_secret_form(f, form);
                }
                if let Some(d) = &dialog {
                    draw_approval(f, d);
                }
            })
        })?;
        scroll = scroll.min(max_scroll);

        // The browser modal owns Enter; don't drain the queue underneath it.
        // The fabricated Enter is flagged so the paste-burst hook below
        // never mistakes it for a real key press.
        let mut synthetic_enter = false;
        let next_event = if composer.queued() > 0
            && !busy
            && skills.is_none()
            && picker.is_none()
            && dialog.is_none()
            && paste_preview.is_none()
            && settings.is_none()
            && context.is_none()
            && mcp.is_none()
            && model_browser.is_none()
            && secret_form.is_none()
            && composer.prefix_kind().is_none()
            && composer.slash_items().is_empty()
        {
            synthetic_enter = true;
            Some(Event::Key(KeyEvent::new(
                KeyCode::Enter,
                KeyModifiers::NONE,
            )))
        } else if let Some(pending) = burst_pending.take() {
            Some(pending)
        } else if event::poll(Duration::from_millis(40))? {
            Some(event::read()?)
        } else {
            None
        };
        if let Some(mut ev) = next_event {
            if let Event::Mouse(mouse) = &ev {
                if skills.is_some()
                    || paste_preview.is_some()
                    || dialog.is_some()
                    || settings.is_some()
                    || context.is_some()
                    || mcp.is_some()
                    || model_browser.is_some()
                    || secret_form.is_some()
                {
                    continue;
                }
                match mouse.kind {
                    MouseEventKind::ScrollUp => scroll = scroll.saturating_add(3),
                    MouseEventKind::ScrollDown => scroll = scroll.saturating_sub(3),
                    _ => {}
                }
                continue;
            }
            // Modal approval: while a request is up, every key routes to it
            // and composer input (paste included) is suppressed.
            if let Some(d) = dialog.take() {
                if let Event::Key(key) = ev {
                    if key.kind != KeyEventKind::Press {
                        dialog = Some(d);
                        continue;
                    }
                    if key.modifiers.contains(KeyModifiers::CONTROL)
                        && matches!(key.code, KeyCode::Char('c') | KeyCode::Char('d'))
                    {
                        break;
                    }
                    match (d.request, approval_for_key(key.code, key.modifiers)) {
                        (Some(req), Some(approval)) => {
                            let _ = req.respond.send(approval);
                        }
                        (maybe, _) => {
                            dialog = Some(ApprovalDialog {
                                text: d.text,
                                request: maybe,
                            });
                        }
                    }
                }
                continue;
            }
            if let Some(mut form) = secret_form.take() {
                match ev {
                    Event::Key(key) if key.kind == KeyEventKind::Press => {
                        if key.code == KeyCode::Esc
                            || (key.modifiers.contains(KeyModifiers::CONTROL)
                                && key.code == KeyCode::Char('d'))
                        {
                            continue;
                        }
                        if key.code == KeyCode::Enter {
                            let result = if form.keyring {
                                let field = form.fields[0].clone();
                                let value = form.values[0].clone();
                                if value.trim().is_empty() {
                                    Err(anyhow::anyhow!("key must not be empty"))
                                } else {
                                    let mut g = shared.lock().await;
                                    let mut cfg = settings
                                        .as_ref()
                                        .map(|dialog| dialog.draft.clone())
                                        .unwrap_or_else(|| g.0.cfg.clone());
                                    set_secret(&mut cfg, &field, value.clone());
                                    cfg.use_keyring = Some(true);
                                    let result = config::keyring_set(&field, &value)
                                        .and_then(|_| apply_settings(&mut g.0, cfg.clone()));
                                    if result.is_ok() {
                                        if let Some(dialog) = settings.as_mut() {
                                            dialog.draft = cfg;
                                            dialog.key_status = dialog.draft.direct_secret_fields();
                                        }
                                    }
                                    result
                                }
                            } else {
                                form.submit_environment()
                            };
                            match result {
                                Ok(()) => {
                                    notice = Some(("credentials updated".into(), Instant::now()));
                                    reload_skills = true;
                                }
                                Err(error) => {
                                    form.error = error.to_string();
                                    secret_form = Some(form);
                                }
                            }
                        } else {
                            form.key(key);
                            secret_form = Some(form);
                        }
                    }
                    Event::Paste(text) => {
                        if let Some(value) = form.values.get_mut(form.selected) {
                            value.push_str(text.trim());
                        }
                        secret_form = Some(form);
                    }
                    _ => secret_form = Some(form),
                }
                continue;
            }
            if let Some(mut cfg_dialog) = settings.take() {
                match ev {
                    Event::Key(key) if key.kind == KeyEventKind::Press => {
                        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
                        if key.code == KeyCode::Esc && cfg_dialog.edit.is_none() {
                            continue;
                        }
                        if ctrl && key.code == KeyCode::Char('s') {
                            cfg_dialog.finish_edit();
                            let mut g = shared.lock().await;
                            match apply_settings(&mut g.0, cfg_dialog.draft.clone()) {
                                Ok(()) => {
                                    reload_skills = true;
                                    notice =
                                        Some(("settings saved and applied".into(), Instant::now()));
                                }
                                Err(error) => {
                                    cfg_dialog.error = error.to_string();
                                    settings = Some(cfg_dialog);
                                }
                            }
                            continue;
                        }
                        if cfg_dialog.edit.is_some() {
                            if key.code == KeyCode::Enter {
                                cfg_dialog.finish_edit();
                            } else if key.code == KeyCode::Esc {
                                cfg_dialog.edit = None;
                            } else {
                                cfg_dialog.edit.as_mut().unwrap().key(key);
                            }
                        } else {
                            match key.code {
                                KeyCode::Tab => {
                                    cfg_dialog.tab =
                                        (cfg_dialog.tab + 1) % SettingsDialog::TABS.len();
                                    cfg_dialog.selected = 0;
                                }
                                KeyCode::BackTab => {
                                    cfg_dialog.tab = (cfg_dialog.tab + SettingsDialog::TABS.len()
                                        - 1)
                                        % SettingsDialog::TABS.len();
                                    cfg_dialog.selected = 0;
                                }
                                KeyCode::Up => {
                                    cfg_dialog.selected = cfg_dialog.selected.saturating_sub(1);
                                    cfg_dialog.sync_theme_row();
                                }
                                KeyCode::Down => {
                                    cfg_dialog.selected = (cfg_dialog.selected + 1)
                                        .min(cfg_dialog.rows().len().saturating_sub(1));
                                    cfg_dialog.sync_theme_row();
                                }
                                KeyCode::Enter | KeyCode::Right if cfg_dialog.tab == 0 => {
                                    let _ = cfg_dialog.accept();
                                    let mut g = shared.lock().await;
                                    match apply_settings(&mut g.0, cfg_dialog.draft.clone()) {
                                        Ok(()) => {
                                            notice = Some(("ok theme".into(), Instant::now()));
                                        }
                                        Err(error) => {
                                            cfg_dialog.error = error.to_string();
                                            settings = Some(cfg_dialog);
                                        }
                                    }
                                    continue;
                                }
                                KeyCode::Enter | KeyCode::Right => {
                                    if let Some(field) = cfg_dialog.accept() {
                                        secret_form = Some(SecretForm::keyring(field));
                                    }
                                }
                                _ => {}
                            }
                        }
                    }
                    Event::Paste(text) => {
                        if let Some(editor) = cfg_dialog.edit.as_mut() {
                            editor.insert(&text);
                        }
                    }
                    _ => {}
                }
                settings = Some(cfg_dialog);
                continue;
            }
            if context.is_some() {
                if matches!(&ev, Event::Key(key) if key.kind == KeyEventKind::Press && matches!(key.code, KeyCode::Esc | KeyCode::Enter))
                {
                    context = None;
                }
                continue;
            }
            if let Some(mut mb) = model_browser.take() {
                if let Event::Key(key) = ev {
                    if key.kind != KeyEventKind::Press {
                        model_browser = Some(mb);
                        continue;
                    }
                    let alt = key.modifiers.contains(KeyModifiers::ALT);
                    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
                    if key.code == KeyCode::Esc {
                        continue;
                    }
                    match key.code {
                        KeyCode::Tab | KeyCode::BackTab => mb.toggle_pane(),
                        KeyCode::Left if alt => mb.prev_kind(),
                        KeyCode::Right if alt => mb.next_kind(),
                        KeyCode::Up => mb.up(),
                        KeyCode::Down => mb.down(),
                        KeyCode::Backspace => mb.backspace(),
                        KeyCode::Enter => {
                            if let Some(m) = mb.selected_model_id() {
                                let mut g = shared.lock().await;
                                let res = change_setting(&mut g.0, "model", &m);
                                if res.is_ok() {
                                    g.0.cfg.set_role_model(
                                        crate::config::ModelRole::Default,
                                        Some(m.clone()),
                                    );
                                    let _ = persistence_config(&g.0.cfg).save();
                                }
                                mb.roles = g.0.cfg.roles.clone();
                                mb.active_model = g.0.cfg.model.clone();
                                notice =
                                    Some((setting_notice(&format!("model → {m}"), res), Instant::now()));
                            }
                            continue;
                        }
                        KeyCode::Char(c) if !ctrl && !alt => {
                            if let Some(role) = mb.role_for_shortcut(c) {
                                if let Some(m) = mb.selected_model_id() {
                                    let mut g = shared.lock().await;
                                    g.0.cfg.set_role_model(role, Some(m.clone()));
                                    if role == crate::config::ModelRole::Default {
                                        let _ = change_setting(&mut g.0, "model", &m);
                                    }
                                    let save_res = persistence_config(&g.0.cfg).save();
                                    mb.roles = g.0.cfg.roles.clone();
                                    mb.active_model = g.0.cfg.model.clone();
                                    mb.note = format!("bound [{}] → {m}", role.as_str());
                                    notice = Some((
                                        setting_notice(
                                            &format!("role {} → {m}", role.as_str()),
                                            save_res,
                                        ),
                                        Instant::now(),
                                    ));
                                }
                            } else {
                                mb.type_char(c);
                            }
                        }
                        _ => {}
                    }
                }
                model_browser = Some(mb);
                continue;
            }
            if let Some(mut browser) = mcp.take() {
                if let Event::Key(key) = ev {
                    if key.kind != KeyEventKind::Press {
                        mcp = Some(browser);
                        continue;
                    }
                    if let Some(form) = browser.add.as_mut() {
                        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
                        if key.code == KeyCode::Esc {
                            browser.add = None;
                        } else if ctrl && key.code == KeyCode::Char('s') {
                            let outcome = (|| -> Result<()> {
                                let value: serde_json::Value =
                                    serde_json::from_str(&form.entry.text)?;
                                let mut g = shared
                                    .try_lock()
                                    .map_err(|_| anyhow::anyhow!("runtime is busy"))?;
                                let path =
                                    g.0.cfg
                                        .mcp_config
                                        .clone()
                                        .unwrap_or_else(|| Config::home_dir().join("mcp.json"));
                                crate::mcp::add_server_entry(&path, &form.name, &value, false)?;
                                let manager = match crate::mcp::McpManager::from_config(Some(&path))
                                {
                                    Ok(manager) => manager,
                                    Err(error) => {
                                        let _ = crate::mcp::remove_server_entry(
                                            &path, &form.name, &value,
                                        );
                                        return Err(error);
                                    }
                                };
                                let mut cfg = g.0.cfg.clone();
                                cfg.mcp_config = Some(path.clone());
                                if let Err(error) = apply_settings(&mut g.0, cfg) {
                                    let _ =
                                        crate::mcp::remove_server_entry(&path, &form.name, &value);
                                    return Err(error);
                                }
                                g.0.mcp = manager;
                                browser.statuses = g.0.mcp.status();
                                browser.schemas = g.0.mcp.schemas().to_vec();
                                Ok(())
                            })();
                            browser.note = match outcome {
                                Ok(()) => "server added".into(),
                                Err(error) => error.to_string(),
                            };
                            if browser.note == "server added" {
                                browser.add = None;
                            }
                        } else if key.code == KeyCode::Tab {
                            form.field = (form.field + 1) % 2;
                        } else if form.field == 0 {
                            match key.code {
                                KeyCode::Backspace => {
                                    form.name.pop();
                                }
                                KeyCode::Char(c) if !ctrl => form.name.push(c),
                                _ => {}
                            }
                        } else {
                            form.entry.key(key);
                        }
                    } else {
                        match key.code {
                            KeyCode::Esc => continue,
                            KeyCode::Up => browser.selected = browser.selected.saturating_sub(1),
                            KeyCode::Down => {
                                browser.selected = (browser.selected + 1).min(
                                    composer::fuzzy_indices(&browser.query, &browser.rows())
                                        .len()
                                        .saturating_sub(1),
                                )
                            }
                            KeyCode::PageUp => browser.scroll = browser.scroll.saturating_sub(8),
                            KeyCode::PageDown => browser.scroll = browser.scroll.saturating_add(8),
                            KeyCode::Char('a') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                                browser.add = Some(McpAdd {
                                    name: String::new(),
                                    field: 0,
                                    entry: composer::TextEditor::new(
                                        "{\"type\":\"http\",\"url\":\"http://127.0.0.1:3000/mcp\"}"
                                            .into(),
                                    ),
                                })
                            }
                            KeyCode::Enter if !browser.waiting => {
                                if let Some(name) = browser.selected_name() {
                                    browser.waiting = true;
                                    browser.note = format!("testing {name}");
                                    spawn_mcp_probe(shared.clone(), tx.clone(), name);
                                }
                            }
                            KeyCode::Backspace => {
                                browser.query.pop();
                                browser.selected = 0;
                            }
                            KeyCode::Char(c)
                                if !key
                                    .modifiers
                                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
                            {
                                browser.query.push(c);
                                browser.selected = 0;
                            }
                            _ => {}
                        }
                    }
                } else if let Event::Paste(text) = ev {
                    if let Some(form) = browser.add.as_mut() {
                        if form.field == 0 {
                            form.name.push_str(text.trim());
                        } else {
                            form.entry.insert(&text);
                        }
                    }
                }
                mcp = Some(browser);
                continue;
            }
            // Skills browser modal: while it is up every key routes here and
            // the draft is untouched. Approval dialogs keep priority above.
            if skills.is_some() {
                if let Event::Key(key) = ev {
                    if key.kind != KeyEventKind::Press {
                        continue;
                    }
                    let mut close = false;
                    {
                        let b = skills.as_mut().unwrap();
                        match key.code {
                            KeyCode::Esc => close = true,
                            KeyCode::Tab => b.preview_focus = !b.preview_focus,
                            KeyCode::Up if b.preview_focus => b.preview_scroll_up(),
                            KeyCode::Down if b.preview_focus => b.preview_scroll_down(),
                            KeyCode::Up => b.up(),
                            KeyCode::Down => b.down(),
                            KeyCode::PageUp => b.preview_scroll_up(),
                            KeyCode::PageDown => b.preview_scroll_down(),
                            KeyCode::Backspace => b.backspace(),
                            KeyCode::Char(c)
                                if !key
                                    .modifiers
                                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
                            {
                                b.type_char(c);
                            }
                            KeyCode::Enter
                                if !key
                                    .modifiers
                                    .intersects(KeyModifiers::SHIFT | KeyModifiers::ALT) =>
                            {
                                match b.enter_action() {
                                    Some(BrowserAction::Install(source)) => {
                                        if b.installing.insert(source.clone()) {
                                            b.note(
                                                StatusKind::Info,
                                                format!("installing: {source}"),
                                            );
                                            spawn_skill_install(
                                                tx.clone(),
                                                source.clone(),
                                                b.preview_cache.get(&source).cloned(),
                                            );
                                        }
                                    }
                                    Some(BrowserAction::Search(query)) => {
                                        let seq = b.request_search(&query);
                                        spawn_skill_search(tx.clone(), query, seq);
                                    }
                                    Some(BrowserAction::Insert(line)) => {
                                        composer.insert_str(&line);
                                        close = true;
                                    }
                                    None => {}
                                }
                            }
                            _ => {}
                        }
                    }
                    // Navigation may have queued a preview fetch for the
                    // newly highlighted row.
                    if let Some(b) = skills.as_mut() {
                        if let Some(src) = b.take_pending_preview() {
                            spawn_skill_preview(tx.clone(), src);
                        }
                    }
                    if close {
                        skills = None;
                    }
                }
                continue;
            }
            if let Some(mut pv) = paste_preview.take() {
                match ev {
                    Event::Paste(text) => {
                        pv.editor.insert(&text);
                        paste_preview = Some(pv);
                    }
                    Event::Key(key) if key.kind == KeyEventKind::Press => {
                        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
                        if ctrl && key.code == KeyCode::Char('d') {
                            composer.remove_paste(pv.card);
                        } else if key.code == KeyCode::Esc
                            || (ctrl && key.code == KeyCode::Char('o'))
                        {
                            pv.save(&mut composer);
                        } else if key.code == KeyCode::Tab {
                            let next = (pv.card + 1) % composer.pastes().len().max(1);
                            pv.save(&mut composer);
                            paste_preview = PastePreview::open(&composer, next);
                        } else {
                            pv.editor.key(key);
                            let area = terminal.size()?;
                            pv.editor.reveal(
                                area.height.saturating_sub(6) as usize,
                                area.width.saturating_sub(18) as usize,
                            );
                            paste_preview = Some(pv);
                        }
                    }
                    _ => paste_preview = Some(pv),
                }
                continue;
            }
            // A paste that misses bracketed paste arrives as a burst of
            // plain keys: folding it after the fact is impossible, because
            // the first Enter would already have submitted the draft and
            // later Enters would enqueue the rest line by line. When more
            // input is already buffered behind an editing key or a paste
            // event, drain that burst into one composer paste instead and
            // never reach the Enter submit/enqueue arms below. A lone key
            // (nothing buffered) keeps its normal handling, and the
            // fabricated queue-drain Enter is never treated as a paste.
            if !synthetic_enter
                && picker.is_none()
                && is_editing_event(&ev)
                && event::poll(Duration::ZERO)?
            {
                match drain_paste_burst(ev)? {
                    PasteBurst::Burst { text, pending } => {
                        burst_pending = pending;
                        composer.paste(&text);
                        continue;
                    }
                    PasteBurst::Single { first, pending } => {
                        burst_pending = pending;
                        ev = first;
                    }
                }
            }
            if let Event::Paste(text) = &ev {
                // While the paste preview is up, pastes are ignored along
                // with every other editing key.
                if paste_preview.is_some() {
                    continue;
                }
                match clipboard::image_path(text) {
                    Some(path) => match clipboard::image_from_path(std::path::Path::new(&path)) {
                        Ok(img) => {
                            composer.attachments.push(img);
                            notice = Some((
                                format!("Image #{} attached", composer.attachments.len()),
                                Instant::now(),
                            ));
                        }
                        Err(e) => notice = Some((e.to_string(), Instant::now())),
                    },
                    // Long pastes become a badge card under the draft instead
                    // of flooding it.
                    None => {
                        composer.paste(text);
                    }
                }
                continue;
            }
            if let Event::Key(key) = ev {
                if key.kind != KeyEventKind::Press {
                    continue;
                }
                if key.modifiers.contains(KeyModifiers::CONTROL)
                    && matches!(key.code, KeyCode::Char('c') | KeyCode::Char('d'))
                {
                    // Ctrl+D with paste cards attached drops the last card
                    // instead of quitting.
                    if key.code == KeyCode::Char('d') && composer.has_pastes() {
                        composer.drop_last_paste();
                        if composer.pastes().is_empty() {
                            paste_preview = None;
                        } else if let Some(pv) = paste_preview.as_mut() {
                            pv.card = pv.card.min(composer.pastes().len() - 1);
                            pv.editor.scroll = 0;
                        }
                        continue;
                    }
                    if key.code == KeyCode::Char('c') {
                        let now = Instant::now();
                        let repeated = last_ctrl_c
                            .is_some_and(|at| now.duration_since(at) < Duration::from_secs(2));
                        last_ctrl_c = Some(now);
                        if !repeated {
                            // The first press never loses work: stop the turn
                            // and print the id needed to come back to it.
                            if busy {
                                interrupt(&mut turn_task, &mut busy, &mut stream);
                            }
                            if let Ok(mut g) = shared.try_lock() {
                                let _ = g.1.close_interrupted_tools();
                                let _ = g.1.persist_meta();
                                current_session_id = g.1.id().to_string();
                            }
                            notice = Some((
                                format!("ctrl+c again to quit · session {current_session_id}"),
                                Instant::now(),
                            ));
                            continue;
                        }
                        if busy {
                            interrupt(&mut turn_task, &mut busy, &mut stream);
                        }
                        if let Ok(mut g) = shared.try_lock() {
                            let _ = g.1.close_interrupted_tools();
                            let _ = g.1.persist_meta();
                            current_session_id = g.1.id().to_string();
                        }
                        quit_session = Some(current_session_id.clone());
                    }
                    break;
                }
                if let Some(p) = picker.as_mut() {
                    match key.code {
                        KeyCode::Up => p.up(),
                        KeyCode::Down => p.down(),
                        KeyCode::Esc | KeyCode::Left => picker = None,
                        KeyCode::Backspace if p.kind == PickerKind::Models => p.backspace(),
                        KeyCode::Char(c)
                            if p.kind == PickerKind::Models
                                && !key
                                    .modifiers
                                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
                        {
                            p.type_char(c);
                        }
                        KeyCode::Enter => {
                            let kind = p.kind;
                            let picked = p.selected().map(|it| it.id.clone());
                            picker = None;
                            match (kind, picked.clone()) {
                                (PickerKind::Models, Some(m)) => {
                                    let mut g = shared.lock().await;
                                    let msg = setting_notice(
                                        &format!("model → {m}"),
                                        change_setting(&mut g.0, "model", &m),
                                    );
                                    notice = Some((msg, Instant::now()));
                                    continue;
                                }
                                (PickerKind::Effort, Some(e)) => {
                                    let mut g = shared.lock().await;
                                    let msg = setting_notice(
                                        &format!("effort → {e}"),
                                        change_setting(&mut g.0, "effort", &e),
                                    );
                                    notice = Some((msg, Instant::now()));
                                    continue;
                                }
                                (PickerKind::Permissions, Some(m)) => {
                                    let mut g = shared.lock().await;
                                    let msg = setting_notice(
                                        &format!("permission → {m}"),
                                        change_setting(&mut g.0, "permission", &m),
                                    );
                                    notice = Some((msg, Instant::now()));
                                    continue;
                                }
                                (PickerKind::Sandbox, Some(m)) => {
                                    let mut g = shared.lock().await;
                                    let msg = setting_notice(
                                        &format!("sandbox → {m}"),
                                        change_setting(&mut g.0, "sandbox", &m),
                                    );
                                    notice = Some((msg, Instant::now()));
                                    continue;
                                }
                                (PickerKind::Plugins, Some(name)) => {
                                    let label = crate::plugins::catalog()
                                        .iter()
                                        .find(|plugin| plugin.name == name)
                                        .map(|plugin| {
                                            crate::plugins::status_label(&crate::plugins::status(
                                                plugin,
                                            ))
                                            .to_string()
                                        })
                                        .unwrap_or_else(|| "no token".into());
                                    notice = Some((format!("{name} · {label}"), Instant::now()));
                                    continue;
                                }
                                (PickerKind::Checkpoints, Some(seq)) => {
                                    let g = shared.lock().await;
                                    let session_id = g.1.id().to_string();
                                    // Undo everything from the selected
                                    // snapshot forward, newest first.
                                    let steps = match g.0.checkpoints.list(&session_id) {
                                        Ok(list) => seq
                                            .parse::<u32>()
                                            .ok()
                                            .and_then(|seq| list.iter().position(|c| c.seq == seq))
                                            .map(|position| list.len() - position)
                                            .unwrap_or(1),
                                        Err(_) => 1,
                                    };
                                    let text = match g.0.checkpoints.undo_n(&session_id, steps) {
                                        Ok(outcomes) => checkpoint::summarize(&outcomes),
                                        Err(error) => error.to_string(),
                                    };
                                    log.push(ChatLine {
                                        role: "system".into(),
                                        text,
                                    });
                                    continue;
                                }
                                _ => {}
                            }
                            let id = picked.filter(|_| kind == PickerKind::Sessions);
                            if let Some(id) = id {
                                match Session::load(&id) {
                                    Ok(s) => {
                                        log = chat_log(&s);
                                        notice = Some((
                                            format!("resumed: {}", s.meta.title),
                                            Instant::now(),
                                        ));
                                        let mut g = shared.lock().await;
                                        let _ = g.1.close_interrupted_tools();
                                        g.0.reset_session_state();
                                        let _ = registration.refresh(s.id(), &cwd, &s.meta.title);
                                        g.1 = s;
                                        composer.clear();
                                        stream.reset();
                                        scroll = 0;
                                    }
                                    Err(e) => log.push(ChatLine {
                                        role: "error".into(),
                                        text: format!("resume failed: {e}"),
                                    }),
                                }
                            }
                        }
                        _ => {}
                    }
                    continue;
                }
                if key.modifiers.contains(KeyModifiers::CONTROL)
                    && !key.modifiers.contains(KeyModifiers::ALT)
                    && matches!(key.code, KeyCode::Char('v') | KeyCode::Char('V'))
                {
                    if let Ok(text) = clipboard::text_from_clipboard() {
                        composer.paste(&text);
                        continue;
                    }
                }
                if key.modifiers.contains(KeyModifiers::ALT)
                    && matches!(key.code, KeyCode::Char('v') | KeyCode::Char('V'))
                {
                    match clipboard::image_from_clipboard() {
                        Ok(img) => {
                            composer.attachments.push(img);
                            notice = Some((
                                format!("Image #{} attached", composer.attachments.len()),
                                Instant::now(),
                            ));
                        }
                        Err(e) => notice = Some((e.to_string(), Instant::now())),
                    }
                    continue;
                }
                if key.modifiers.is_empty() {
                    if key.code == KeyCode::Enter {
                        if let Some(picked) = composer.prefix_selected() {
                            match composer.prefix_kind() {
                                Some(composer::PrefixKind::Files) => {
                                    let outcome = shared
                                        .try_lock()
                                        .map_err(|_| {
                                            anyhow::anyhow!("files can be pinned after this turn")
                                        })
                                        .and_then(|g| {
                                            composer::read_jail_pin(&g.0.jail, &picked.id)
                                        });
                                    match outcome {
                                        Ok(content) => {
                                            composer.accept_file_pin(&picked.id, &content)
                                        }
                                        Err(error) => {
                                            notice = Some((error.to_string(), Instant::now()))
                                        }
                                    }
                                }
                                Some(composer::PrefixKind::Mentions) => {
                                    if picked.id.starts_with("session:") {
                                        composer.accept_mention(&format!("@{}", picked.id));
                                    } else {
                                        composer.accept_mention(&picked.label);
                                    }
                                }
                                Some(composer::PrefixKind::Skills) => {
                                    composer.accept_skill(&picked.id)
                                }
                                None => {}
                            }
                            continue;
                        }
                    }
                    if composer.prefix_key(key.code) || composer.slash_key(key.code) {
                        continue;
                    }
                }
                if key.code == KeyCode::Enter && key.modifiers.is_empty() && !synthetic_enter {
                    if absorb_paste_continuation(&mut composer, &mut burst_pending)? {
                        continue;
                    }
                }
                match key.code {
                    KeyCode::Esc if busy => {
                        if interrupt(&mut turn_task, &mut busy, &mut stream) {
                            log.push(ChatLine {
                                role: "system".into(),
                                text: "stopped  ·  esc".into(),
                            });
                        }
                    }
                    KeyCode::Esc => {
                        if composer.is_empty() {
                            notice = Some(("ctrl+c quits".into(), Instant::now()));
                        } else {
                            composer.submit();
                        }
                    }
                    KeyCode::BackTab => {
                        let Ok(mut g) = shared.try_lock() else {
                            notice = Some((
                                "permission mode changes after this turn".into(),
                                Instant::now(),
                            ));
                            continue;
                        };
                        let cur = g.0.permission;
                        let i = modes.iter().position(|m| *m == cur).unwrap_or(0);
                        let next = modes[(i + 1) % modes.len()];
                        notice = Some((
                            setting_notice(
                                &format!("permission → {}", next.as_str()),
                                change_setting(&mut g.0, "permission", next.as_str()),
                            ),
                            Instant::now(),
                        ));
                    }
                    KeyCode::Enter
                        if key
                            .modifiers
                            .intersects(KeyModifiers::SHIFT | KeyModifiers::ALT) =>
                    {
                        composer.insert_char('\n');
                    }
                    KeyCode::Enter | KeyCode::Char('j')
                        if busy && key.modifiers.contains(KeyModifiers::CONTROL) =>
                    {
                        if composer.is_empty() {
                            continue;
                        }
                        let now = composer.submit();
                        interrupt(&mut turn_task, &mut busy, &mut stream);
                        log.push(ChatLine {
                            role: "system".into(),
                            text: "stopped  ·  sending now".into(),
                        });
                        composer.enqueue_front(now);
                    }
                    KeyCode::Char('j') if key.modifiers.contains(KeyModifiers::CONTROL) => {}
                    KeyCode::Enter if busy => {
                        let text = composer.text().trim().to_string();
                        if let Some(note) = text.strip_prefix("/btw").map(|s| s.trim()) {
                            if !note.is_empty() {
                                let (line, _) = composer.submit();
                                let note = line
                                    .trim()
                                    .strip_prefix("/btw")
                                    .unwrap_or("")
                                    .trim()
                                    .to_string();
                                if let Ok(mut notes) = side_notes.lock() {
                                    notes.push(note.clone());
                                }
                                log.push(ChatLine {
                                    role: "user".into(),
                                    text: format!("/btw {note}"),
                                });
                                log.push(ChatLine {
                                    role: "system".into(),
                                    text: "💡 Side note attached — running agent will read it on its next step.".into(),
                                });
                            }
                        } else {
                            match composer.enqueue() {
                                Some(n) => {
                                    notice = Some((
                                        format!("queued #{n}  ·  ctrl+enter sends now"),
                                        Instant::now(),
                                    ));
                                }
                                None if composer.queued() >= composer::MAX_QUEUE => {
                                    notice = Some(("queue is full".into(), Instant::now()));
                                }
                                None => {}
                            }
                        }
                    }
                    KeyCode::Enter => {
                        let (line, mut images) = if let Some(pending) = composer.next_queued() {
                            pending
                        } else {
                            if composer.is_empty() {
                                continue;
                            }
                            composer.submit()
                        };
                        let local = matches!(
                            line.as_str(),
                            "/quit"
                                | "/exit"
                                | "/update"
                                | "/effort"
                                | "/status"
                                | "/skills"
                                | "/skills-search"
                                | "/models"
                                | "/model"
                                | "/plugins"
                                | "/help"
                                | "/commands"
                                | "/cost"
                                | "/diff"
                                | "/doctor"
                                | "/resume"
                                | "/compact"
                                | "/review"
                                | "/permission"
                                | "/permissions"
                                | "/config"
                                | "/theme"
                                | "/context"
                                | "/mcp"
                                | "/new"
                                | "/clear"
                                | "/rename"
                                | "/sandbox"
                                | "/computer"
                                | "/computer-use"
                                | "/checkpoints"
                                | "/fork"
                                | "/export"
                        ) || line.starts_with("/agents")
                            || line.starts_with("/btw")
                            || line.starts_with("/computer ")
                            || line.starts_with("/computer-use ")
                            || line.starts_with("/swarm")
                            || line.starts_with("/todo")
                            || line.starts_with("/seance")
                            || line.starts_with("/rename ")
                            || line.starts_with("/fork ")
                            || line.starts_with("/export ")
                            || line.starts_with("/collaborate")
                            || line.starts_with("/mcp ")
                            || line.starts_with("/permission ")
                            || line.starts_with("/sandbox ")
                            || line.starts_with("/model ")
                            || line.starts_with("/effort ")
                            || line.starts_with("/skills-search ");
                        if local {
                            // Local commands don't send a turn; keep the images queued.
                            composer.attachments = std::mem::take(&mut images);
                        }
                        let line = if line.is_empty() {
                            "Describe the attached image.".to_string()
                        } else {
                            line
                        };
                        if line == "/quit" || line == "/exit" {
                            // Let a `session_stop` hook run before the
                            // process exits; skip it if a turn still holds
                            // the lock rather than stalling the exit.
                            if let Ok(mut g) = shared.try_lock() {
                                g.0.end_session().await;
                            }
                            break;
                        }
                        if line == "/new" || line == "/clear" {
                            let mut g = shared.lock().await;
                            let outcome = if line == "/new" {
                                g.1.close_interrupted_tools()
                                    .and_then(|_| Session::new(&cwd, &g.0.cfg.model))
                                    .map(|session| {
                                        g.1 = session;
                                    })
                            } else {
                                g.1.clear_messages()
                            };
                            match outcome {
                                Ok(()) => {
                                    g.0.reset_session_state();
                                    composer.clear();
                                    log.clear();
                                    stream.reset();
                                    scroll = 0;
                                    let _ = registration.refresh(g.1.id(), &cwd, &g.1.meta.title);
                                    notice = Some((
                                        if line == "/new" {
                                            "new session"
                                        } else {
                                            "context cleared"
                                        }
                                        .into(),
                                        Instant::now(),
                                    ));
                                }
                                Err(error) => log.push(ChatLine {
                                    role: "error".into(),
                                    text: error.to_string(),
                                }),
                            }
                            continue;
                        }
                        if line == "/rename" || line.starts_with("/rename ") {
                            let title = line.strip_prefix("/rename ").unwrap_or("").trim();
                            let mut g = shared.lock().await;
                            let result = if title.is_empty() {
                                Err(anyhow::anyhow!("usage: /rename <title>"))
                            } else {
                                g.1.set_title(title)
                            };
                            notice =
                                Some((setting_notice("session renamed", result), Instant::now()));
                            let _ = registration.refresh(g.1.id(), &cwd, &g.1.meta.title);
                            continue;
                        }
                        if line == "/config" || line == "/theme" {
                            let mut dialog = SettingsDialog::new(shared.lock().await.0.cfg.clone());
                            if line == "/theme" {
                                dialog.tab = 0;
                            }
                            settings = Some(dialog);
                            continue;
                        }
                        if line == "/context" {
                            let g = shared.lock().await;
                            context = Some(ContextView::new(&g.0, &g.1));
                            continue;
                        }
                        if line == "/mcp" || line.starts_with("/mcp ") {
                            let g = shared.lock().await;
                            let mut browser = McpBrowser::new(&g.0);
                            if line.trim() == "/mcp add" {
                                browser.add = Some(McpAdd {
                                    name: String::new(),
                                    field: 0,
                                    entry: composer::TextEditor::new(
                                        "{\"type\":\"http\",\"url\":\"http://127.0.0.1:3000/mcp\"}"
                                            .into(),
                                    ),
                                });
                            } else if let Some(name) = line.strip_prefix("/mcp test ") {
                                browser.query = name.trim().into();
                                browser.waiting = true;
                                spawn_mcp_probe(shared.clone(), tx.clone(), name.trim().into());
                            }
                            mcp = Some(browser);
                            continue;
                        }
                        if line == "/collaborate" || line.starts_with("/collaborate ") {
                            peer_terms = registry.live();
                            session_rows = Session::list().unwrap_or_default();
                            let handle = line.strip_prefix("/collaborate ").unwrap_or("").trim();
                            let mut g = shared.lock().await;
                            let result = resolve_peer(handle, &peer_terms, &session_rows).and_then(|target| {
                                anyhow::ensure!(target != g.1.id(), "cannot collaborate with this session");
                                let handshake = format!("[collaborate request] session {} ({}) wants to pair-program in {}. Reply with /send {} <message>.", g.1.id(), g.1.meta.title, cwd, g.1.id());
                                g.0.bus.send(g.1.id(), &target, &handshake)?;
                                g.1.append(crate::session::ChatMessage { role: "system".into(), content: format!("[collaborate requested with {target}]"), ..Default::default() })?;
                                Ok(())
                            });
                            notice = Some((
                                setting_notice("collaboration request sent", result),
                                Instant::now(),
                            ));
                            continue;
                        }
                        if line == "/resume" {
                            picker = Some(sessions_picker());
                            continue;
                        }
                        if line == "/cost" {
                            let g = shared.lock().await;
                            log.push(ChatLine {
                                role: "system".into(),
                                text: cost_line(&g.0, &g.1),
                            });
                            continue;
                        }
                        if line == "/diff" {
                            log.push(ChatLine {
                                role: "system".into(),
                                text: git_diff(&cwd),
                            });
                            continue;
                        }
                        if line == "/checkpoints" || line.starts_with("/checkpoints ") {
                            let g = shared.lock().await;
                            let items = match g.0.checkpoints.list(g.1.id()) {
                                Ok(list) => list
                                    .iter()
                                    .map(|checkpoint| PickItem {
                                        id: checkpoint.seq.to_string(),
                                        label: checkpoint
                                            .path
                                            .file_name()
                                            .map(|name| name.to_string_lossy().into_owned())
                                            .unwrap_or_else(|| {
                                                checkpoint.path.display().to_string()
                                            }),
                                        detail: format!(
                                            "seq {} · {} B{}",
                                            checkpoint.seq,
                                            checkpoint.bytes,
                                            if checkpoint.skipped {
                                                " · skipped"
                                            } else {
                                                ""
                                            }
                                        ),
                                    })
                                    .collect(),
                                Err(error) => {
                                    log.push(ChatLine {
                                        role: "error".into(),
                                        text: format!("checkpoints failed: {error}"),
                                    });
                                    continue;
                                }
                            };
                            picker = Some(checkpoints_picker(items));
                            continue;
                        }
                        if line == "/fork" || line.starts_with("/fork ") {
                            let at = line.strip_prefix("/fork").unwrap_or("").trim();
                            let at = if at.is_empty() {
                                None
                            } else {
                                match at.parse::<usize>() {
                                    Ok(n) if n >= 1 => Some(n),
                                    _ => {
                                        log.push(ChatLine {
                                            role: "system".into(),
                                            text: "usage: /fork [message-count]  (n >= 1)".into(),
                                        });
                                        continue;
                                    }
                                }
                            };
                            let mut g = shared.lock().await;
                            let result =
                                g.1.close_interrupted_tools()
                                    .and_then(|_| Session::fork_from(g.1.id(), at));
                            match result {
                                Ok(fork) => {
                                    g.0.reset_session_state();
                                    let _ = registration.refresh(fork.id(), &cwd, &fork.meta.title);
                                    g.1 = fork;
                                    composer.clear();
                                    stream.reset();
                                    scroll = 0;
                                    log = chat_log(&g.1);
                                    notice = Some((
                                        format!("forked: {}", g.1.meta.title),
                                        Instant::now(),
                                    ));
                                }
                                Err(error) => log.push(ChatLine {
                                    role: "error".into(),
                                    text: format!("fork failed: {error}"),
                                }),
                            }
                            continue;
                        }
                        if line == "/export" || line.starts_with("/export ") {
                            let mut format = crate::export::ExportFormat::Markdown;
                            let mut out: Option<PathBuf> = None;
                            let mut force = false;
                            let mut args = line.split_whitespace().skip(1);
                            let mut bad = None;
                            while let Some(arg) = args.next() {
                                match arg {
                                    "--force" => force = true,
                                    "--format" => match args.next() {
                                        Some(value) => {
                                            match crate::export::ExportFormat::parse(value) {
                                                Ok(parsed) => format = parsed,
                                                Err(error) => bad = Some(error),
                                            }
                                        }
                                        None => {
                                            bad = Some(anyhow::anyhow!("--format needs md or json"))
                                        }
                                    },
                                    "--out" => match args.next() {
                                        Some(value) => out = Some(PathBuf::from(value)),
                                        None => bad = Some(anyhow::anyhow!("--out needs a path")),
                                    },
                                    other => {
                                        bad = Some(anyhow::anyhow!("unknown option `{other}`"))
                                    }
                                }
                            }
                            let result = match bad {
                                Some(error) => Err(error),
                                None => {
                                    let g = shared.lock().await;
                                    crate::export::export_session(
                                        &g.1,
                                        format,
                                        out.as_deref(),
                                        force,
                                    )
                                }
                            };
                            match result {
                                Ok(path) => log.push(ChatLine {
                                    role: "system".into(),
                                    text: format!("exported {}", path.display()),
                                }),
                                Err(error) => log.push(ChatLine {
                                    role: "error".into(),
                                    text: format!("export failed: {error}"),
                                }),
                            }
                            continue;
                        }
                        if line == "/doctor" {
                            let cfg = shared.lock().await.0.cfg.clone();
                            let tx2 = tx.clone();
                            notice = Some(("running doctor…".into(), Instant::now()));
                            tokio::spawn(async move {
                                let text = match crate::doctor::run(&cfg).await {
                                    Ok(v) => doctor_lines(&v),
                                    Err(e) => format!("doctor failed: {e}"),
                                };
                                let _ = tx2.send(UiEvent::System(text));
                            });
                            continue;
                        }
                        if let Some(rest) = line.strip_prefix("/btw") {
                            let note = rest.trim();
                            if note.is_empty() {
                                log.push(ChatLine {
                                    role: "system".into(),
                                    text: "Usage: /btw <note or question>\nSends a side note without interrupting the current workflow.".into(),
                                });
                            } else {
                                if let Ok(mut notes) = side_notes.lock() {
                                    notes.push(note.to_string());
                                }
                                log.push(ChatLine {
                                    role: "user".into(),
                                    text: format!("/btw {note}"),
                                });
                                log.push(ChatLine {
                                    role: "system".into(),
                                    text: format!("💡 Side note registered: \"{note}\" (will be injected into the next agent step)"),
                                });
                            }
                            continue;
                        }
                        if let Some(task) = line.strip_prefix("/swarm") {
                            let task = task.trim();
                            if task.is_empty() {
                                log.push(ChatLine {
                                    role: "system".into(),
                                    text: "Usage: /swarm <task>\nSpawns a 5-model multi-agent council (Architect, Researcher, Coder, Reviewer, Synthesizer) debating and collaborating concurrently for lightning-fast, high-quality results.".into(),
                                });
                                continue;
                            }
                            let (cfg, root) = {
                                let g = shared.lock().await;
                                (g.0.cfg.clone(), g.0.jail.cwd.clone())
                            };
                            log.push(ChatLine {
                                role: "user".into(),
                                text: format!("/swarm {task}"),
                            });
                            log.push(ChatLine {
                                role: "system".into(),
                                text: "⚡ Multi-Model Swarm Council started (5 agents debating in parallel)...".into(),
                            });
                            let tx2 = tx.clone();
                            let task = task.to_string();
                            tokio::spawn(async move {
                                let tx_ev = tx2.clone();
                                let res = crate::swarm::run_swarm(&cfg, &root, &task, move |ev| {
                                    let _ = tx_ev.send(UiEvent::System(format!(
                                        "🤖 [{} | {}]: {}",
                                        ev.agent, ev.phase, ev.text
                                    )));
                                })
                                .await;
                                match res {
                                    Ok(res) => {
                                        let _ = tx2.send(UiEvent::System(format!(
                                            "✨ Council Synthesis Complete:\n\n{}",
                                            res.summary
                                        )));
                                    }
                                    Err(e) => {
                                        let _ = tx2.send(UiEvent::System(format!(
                                            "❌ Swarm Council failed: {e}"
                                        )));
                                    }
                                }
                            });
                            continue;
                        }
                        if let Some(cmd) = line.strip_prefix("/todo") {
                            let cmd = cmd.trim();
                            let root = shared.lock().await.0.jail.cwd.clone();
                            let path = root.join(".varynth").join("todos.json");
                            let mut store = crate::todo::TodoStore::load_or_new(&path);
                            if cmd.is_empty() || cmd == "list" {
                                log.push(ChatLine {
                                    role: "system".into(),
                                    text: store.format_list(),
                                });
                            } else if let Some(rest) = cmd.strip_prefix("add ") {
                                let id = store.add(rest);
                                let _ = store.save(&path);
                                log.push(ChatLine {
                                    role: "system".into(),
                                    text: format!("Added task #{id}: {rest}"),
                                });
                            } else if let Some(rest) = cmd.strip_prefix("done ") {
                                if let Ok(id) = rest.trim().parse::<usize>() {
                                    if store.set_done(id, true) {
                                        let _ = store.save(&path);
                                        log.push(ChatLine {
                                            role: "system".into(),
                                            text: format!("Completed task #{id}"),
                                        });
                                    } else {
                                        log.push(ChatLine {
                                            role: "error".into(),
                                            text: format!("Task #{id} not found"),
                                        });
                                    }
                                } else {
                                    log.push(ChatLine {
                                        role: "error".into(),
                                        text: "Usage: /todo done <id>".into(),
                                    });
                                }
                            } else if let Some(rest) = cmd.strip_prefix("rm ") {
                                if let Ok(id) = rest.trim().parse::<usize>() {
                                    if store.remove(id) {
                                        let _ = store.save(&path);
                                        log.push(ChatLine {
                                            role: "system".into(),
                                            text: format!("Removed task #{id}"),
                                        });
                                    } else {
                                        log.push(ChatLine {
                                            role: "error".into(),
                                            text: format!("Task #{id} not found"),
                                        });
                                    }
                                }
                            } else if cmd == "clear" {
                                store.clear();
                                let _ = store.save(&path);
                                log.push(ChatLine {
                                    role: "system".into(),
                                    text: "All tasks cleared.".into(),
                                });
                            } else {
                                log.push(ChatLine {
                                    role: "system".into(),
                                    text: "Usage: /todo [list] | /todo add <text> | /todo done <id> | /todo rm <id> | /todo clear".into(),
                                });
                            }
                            continue;
                        }
                        if let Some(query) = line.strip_prefix("/seance") {
                            let query = query.trim();
                            if query.is_empty() {
                                log.push(ChatLine {
                                    role: "system".into(),
                                    text: "Usage: /seance <topic or keyword>\nSummons knowledge, code patterns and architectural decisions from past sessions.".into(),
                                });
                                continue;
                            }
                            log.push(ChatLine {
                                role: "user".into(),
                                text: format!("/seance {query}"),
                            });
                            match crate::seance::search_past_sessions(query, 3) {
                                Ok(results) if results.is_empty() => {
                                    log.push(ChatLine {
                                        role: "system".into(),
                                        text: format!(
                                            "No previous sessions found matching \"{query}\"."
                                        ),
                                    });
                                }
                                Ok(results) => {
                                    let mut out = format!(
                                        "🔮 Summoned insights from {} past session(s):\n\n",
                                        results.len()
                                    );
                                    for (idx, r) in results.iter().enumerate() {
                                        out.push_str(&format!(
                                            "{}. **{}** (`{}`)\n   > {}\n\n",
                                            idx + 1,
                                            r.title,
                                            r.session_id,
                                            r.snippet.replace('\n', " ")
                                        ));
                                    }
                                    out.push_str(
                                        "💡 Insights injected into session context for reference.",
                                    );
                                    let mut g = shared.lock().await;
                                    let _ = g.1.append(crate::session::ChatMessage {
                                        role: "user".into(),
                                        content: format!(
                                            "[Seance Memory Consultant for \"{query}\"]:\n{out}"
                                        ),
                                        ..Default::default()
                                    });
                                    log.push(ChatLine {
                                        role: "system".into(),
                                        text: out,
                                    });
                                }
                                Err(e) => {
                                    log.push(ChatLine {
                                        role: "error".into(),
                                        text: format!("Seance search failed: {e}"),
                                    });
                                }
                            }
                            continue;
                        }
                        if let Some(spec) = line.strip_prefix("/agents") {
                            let (name, prompt) = match crate::agents::parse_spec(spec) {
                                Ok(v) => v,
                                Err(e) => {
                                    log.push(ChatLine {
                                        role: "system".into(),
                                        text: e.to_string(),
                                    });
                                    continue;
                                }
                            };
                            let (cfg, root) = {
                                let g = shared.lock().await;
                                (g.0.cfg.clone(), g.0.jail.cwd.clone())
                            };
                            log.push(ChatLine {
                                role: "system".into(),
                                text: format!("agent {name} started  ·  ↓ shows it"),
                            });
                            let tx2 = tx.clone();
                            tokio::spawn(async move {
                                let ev = match crate::agents::run(&cfg, &root, &name, &prompt).await
                                {
                                    Ok(r) => UiEvent::Agent(r.name, r.reply),
                                    Err(e) => UiEvent::System(format!("agent {name} failed: {e}")),
                                };
                                let _ = tx2.send(ev);
                            });
                            continue;
                        }
                        if line == "/compact" {
                            log.push(ChatLine {
                                role: "user".into(),
                                text: "/compact".into(),
                            });
                            busy = true;
                            let sh = shared.clone();
                            let tx2 = tx.clone();
                            turn_task = Some(tokio::spawn(async move {
                                let mut g = sh.lock().await;
                                let (rt, sess) = &mut *g;
                                let _ = sess.close_interrupted_tools();
                                let before_msgs = sess.messages.len();
                                let before = compact::estimate_messages(&sess.messages);
                                match rt.compact_session(sess).await {
                                    Ok(summary) => {
                                        let after = compact::estimate_messages(&sess.messages);
                                        let _ = tx2.send(UiEvent::Assistant(format!(
                                            "compacted {before_msgs} → {} messages in place  ·  ≈{} → ≈{} tokens\n\n{}",
                                            sess.messages.len(),
                                            before.div_ceil(4),
                                            after.div_ceil(4),
                                            summary
                                        )));
                                    }
                                    Err(e) => {
                                        let _ = tx2
                                            .send(UiEvent::Error(format!("compact failed: {e}")));
                                    }
                                }
                            }));
                            continue;
                        }
                        if line == "/review" {
                            log.push(ChatLine {
                                role: "user".into(),
                                text: "/review".into(),
                            });
                            busy = true;
                            scroll = 0;
                            let sh = shared.clone();
                            let tx2 = tx.clone();
                            let cwd2 = cwd.clone();
                            turn_task = Some(tokio::spawn(async move {
                                let mut g = sh.lock().await;
                                let (rt, sess) = &mut *g;
                                if let Err(e) = sess.close_interrupted_tools() {
                                    let _ = tx2.send(UiEvent::Error(e.to_string()));
                                    return;
                                }
                                match rt.review(sess, std::path::Path::new(&cwd2)).await {
                                    Ok(text) => {
                                        let _ = tx2.send(UiEvent::Assistant(text));
                                    }
                                    Err(e) => {
                                        let _ =
                                            tx2.send(UiEvent::Error(format!("review failed: {e}")));
                                    }
                                }
                            }));
                            continue;
                        }
                        if line == "/help" || line == "/commands" {
                            log.push(ChatLine {
                                role: "system".into(),
                                text: HELP.into(),
                            });
                            continue;
                        }
                        if line == "/effort" {
                            let cur = shared.lock().await.0.cfg.effort.clone();
                            picker = Some(effort_picker(&cur));
                            continue;
                        }
                        if let Some(arg) = line.strip_prefix("/effort ") {
                            let e = arg.trim();
                            let mut g = shared.lock().await;
                            notice = Some((
                                setting_notice(
                                    &format!("effort → {e}"),
                                    change_setting(&mut g.0, "effort", e),
                                ),
                                Instant::now(),
                            ));
                            continue;
                        }
                        for (command, field) in
                            [("/permission ", "permission"), ("/sandbox ", "sandbox")]
                        {
                            if let Some(value) = line.strip_prefix(command) {
                                let mut g = shared.lock().await;
                                notice = Some((
                                    setting_notice(
                                        &format!("{field} → {}", value.trim()),
                                        change_setting(&mut g.0, field, value.trim()),
                                    ),
                                    Instant::now(),
                                ));
                            }
                        }
                        if line.starts_with("/permission ") || line.starts_with("/sandbox ") {
                            continue;
                        }
                        if line == "/permission" {
                            let cur = shared.lock().await.0.cfg.permission_mode.clone();
                            picker = Some(permission_picker(&cur));
                            continue;
                        }
                        if line == "/permissions" {
                            let g = shared.lock().await;
                            log.push(ChatLine {
                                role: "system".into(),
                                text: g.0.permissions_report(),
                            });
                            continue;
                        }
                        if line == "/sandbox" {
                            let cur = shared.lock().await.0.cfg.sandbox.clone();
                            picker = Some(sandbox_picker(&cur));
                            continue;
                        }
                        if line == "/computer-use"
                            || line == "/computer"
                            || line.starts_with("/computer-use ")
                            || line.starts_with("/computer ")
                        {
                            let arg = line
                                .strip_prefix("/computer-use ")
                                .or_else(|| line.strip_prefix("/computer "))
                                .unwrap_or("")
                                .trim();
                            let mut g = shared.lock().await;
                            match arg {
                                "on" | "enable" | "true" => {
                                    g.0.cfg.computer_use = true;
                                    let _ = g.0.cfg.save();
                                    std::env::set_var("VARYNTH_COMPUTER_USE", "1");
                                    log.push(ChatLine {
                                        role: "system".into(),
                                        text: "✨ Desktop operator enabled: VARYNTH_COMPUTER_USE=1. Visual overlay & safety kill-switch (Esc) active.".into(),
                                    });
                                }
                                "off" | "disable" | "false" => {
                                    g.0.cfg.computer_use = false;
                                    let _ = g.0.cfg.save();
                                    std::env::remove_var("VARYNTH_COMPUTER_USE");
                                    log.push(ChatLine {
                                        role: "system".into(),
                                        text: "Desktop operator disabled.".into(),
                                    });
                                }
                                "status" | "doctor" => {
                                    let is_on = g.0.cfg.computer_use || std::env::var("VARYNTH_COMPUTER_USE").is_ok();
                                    let status_msg = format!(
                                        "Desktop Operator Status:\n• State: {}\n• Safety Kill-Switch: Active (Esc / physical mouse override)\n• DPI Normalization: Per-Monitor V2 enabled",
                                        if is_on { "ACTIVE (Enabled)" } else { "INACTIVE (Use /computer-use on)" }
                                    );
                                    log.push(ChatLine {
                                        role: "system".into(),
                                        text: status_msg,
                                    });
                                }
                                _ => {
                                    let next = !g.0.cfg.computer_use;
                                    g.0.cfg.computer_use = next;
                                    let _ = g.0.cfg.save();
                                    if next {
                                        std::env::set_var("VARYNTH_COMPUTER_USE", "1");
                                    } else {
                                        std::env::remove_var("VARYNTH_COMPUTER_USE");
                                    }
                                    log.push(ChatLine {
                                        role: "system".into(),
                                        text: format!(
                                            "Desktop operator toggled: {} (VARYNTH_COMPUTER_USE={})",
                                            if next { "ENABLED" } else { "DISABLED" },
                                            if next { "1" } else { "0" }
                                        ),
                                    });
                                }
                            }
                            continue;
                        }
                        if line == "/skills-search" || line.starts_with("/skills-search ") {
                            let prefill = line
                                .strip_prefix("/skills-search ")
                                .unwrap_or("")
                                .trim()
                                .to_string();
                            let installed = installed.clone();
                            let mut b = SkillsBrowser::new(installed, &prefill);
                            if !prefill.is_empty() {
                                let seq = b.request_search(&prefill);
                                spawn_skill_search(tx.clone(), prefill, seq);
                            }
                            skills = Some(b);
                            continue;
                        }
                        if let Some(arg) = line.strip_prefix("/model ") {
                            let mut g = shared.lock().await;
                            notice = Some((
                                setting_notice(
                                    &format!("model → {}", arg.trim()),
                                    change_setting(&mut g.0, "model", arg.trim()),
                                ),
                                Instant::now(),
                            ));
                            continue;
                        }
                        if line == "/status" {
                            let g = shared.lock().await;
                            log.push(ChatLine {
                                role: "system".into(),
                                text: serde_json::to_string_pretty(&g.0.status_json(Some(&g.1)))
                                    .unwrap_or_default(),
                            });
                            continue;
                        }
                        if line == "/skills" {
                            let g = shared.lock().await;
                            let t =
                                g.0.skills
                                    .iter()
                                    .map(|s| format!("/{}  {}", s.name, s.description))
                                    .collect::<Vec<_>>()
                                    .join("\n");
                            log.push(ChatLine {
                                role: "system".into(),
                                text: if t.is_empty() {
                                    "(no skills)".into()
                                } else {
                                    t
                                },
                            });
                            continue;
                        }
                        if line == "/update" {
                            match crate::update::fetch_latest(std::time::Duration::from_secs(5)).await {
                                Ok(latest) => {
                                    if crate::update::is_newer(&latest, crate::update::CURRENT) {
                                        log.push(ChatLine {
                                            role: "assistant".into(),
                                            text: format!("↑ Yeni bir sürüm var: v{latest} (mevcut: v{}). Güncellemek için `varynth update` yazabilirsiniz.", crate::update::CURRENT),
                                        });
                                    } else {
                                        log.push(ChatLine {
                                            role: "assistant".into(),
                                            text: format!("✓ En son sürümü kullanıyorsunuz (v{}).", crate::update::CURRENT),
                                        });
                                    }
                                }
                                Err(e) => {
                                    log.push(ChatLine {
                                        role: "error".into(),
                                        text: format!("Güncelleme kontrolü başarısız: {e}"),
                                    });
                                }
                            }
                            continue;
                        }
                        if line == "/plugins" {
                            picker = Some(plugins_picker());
                            continue;
                        }
                        if line == "/models" || line == "/model" {
                            let g = shared.lock().await;
                            match g.0.list_models().await {
                                Ok(ms) => {
                                    model_browser = Some(ModelBrowser::new(ms, &g.0.cfg));
                                }
                                Err(e) => log.push(ChatLine {
                                    role: "error".into(),
                                    text: e.to_string(),
                                }),
                            }
                            continue;
                        }
                        let (names, mut addresses) = composer::extract_mentions(&line, &personas);
                        addresses.extend(
                            line.split_whitespace()
                                .filter_map(|word| word.strip_prefix('@'))
                                .filter(|word| word.starts_with("session:"))
                                .map(str::to_string),
                        );
                        if !names.is_empty() || !addresses.is_empty() {
                            peer_terms = registry.live();
                            session_rows = Session::list().unwrap_or_default();
                            let g = shared.lock().await;
                            for name in names {
                                let brief = personas
                                    .iter()
                                    .find(|persona| persona.name == name)
                                    .map(|persona| persona.prompt.as_str())
                                    .unwrap_or("");
                                spawn_agent(
                                    tx.clone(),
                                    g.0.cfg.clone(),
                                    g.0.jail.cwd.clone(),
                                    name.clone(),
                                    format!("{brief}\n\n{line}"),
                                );
                                log.push(ChatLine {
                                    role: "system".into(),
                                    text: format!("agent {name} started"),
                                });
                            }
                            let mut sent = HashSet::new();
                            for address in addresses {
                                let outcome = resolve_peer(&address, &peer_terms, &session_rows)
                                    .and_then(|target| {
                                        anyhow::ensure!(
                                            target != g.1.id(),
                                            "cannot send to this session"
                                        );
                                        if sent.insert(target.clone()) {
                                            g.0.bus.send(g.1.id(), &target, &line)?;
                                        }
                                        Ok(())
                                    });
                                if let Err(error) = outcome {
                                    log.push(ChatLine {
                                        role: "error".into(),
                                        text: error.to_string(),
                                    });
                                }
                            }
                        }
                        if let Some(name) = unknown_slash(&line, &composer.commands) {
                            log.push(ChatLine {
                                role: "error".into(),
                                text: unknown_slash_reply(&name, &composer.commands),
                            });
                            continue;
                        }
                        let tags: String = (1..=images.len())
                            .map(|i| format!("  [Image #{i}]"))
                            .collect();
                        log.push(ChatLine {
                            role: "user".into(),
                            text: format!("{line}{tags}"),
                        });
                        busy = true;
                        scroll = 0;
                        let sh = shared.clone();
                        let tx2 = tx.clone();
                        let approvals = Arc::clone(&approval_rx);
                        turn_task = Some(tokio::spawn(async move {
                            let mut next = line;
                            loop {
                                let mut g = sh.lock().await;
                                let (rt, sess) = &mut *g;
                                if let Err(e) = sess.close_interrupted_tools() {
                                    let _ = tx2.send(UiEvent::Error(e.to_string()));
                                    break;
                                }
                                rt.pending_images = std::mem::take(&mut images);
                                let turn = rt
                                    .turn(sess, &next, |ev| match ev.kind.as_str() {
                                        "delta" => {
                                            let _ = tx2.send(UiEvent::Delta(ev.text));
                                        }
                                        "tool" => {
                                            let _ = tx2.send(UiEvent::Tool(ev.text));
                                        }
                                        "system" => {
                                            let _ = tx2.send(UiEvent::System(ev.text));
                                        }
                                        "approval_request" => {
                                            // The runtime sent the request on
                                            // the approver channel right before
                                            // this event; hand the oneshot
                                            // straight to the dialog.
                                            let request = approvals
                                                .lock()
                                                .ok()
                                                .and_then(|mut rx| rx.try_recv().ok());
                                            let _ = tx2.send(UiEvent::Approval {
                                                text: ev.text,
                                                request,
                                            });
                                        }
                                        _ => {}
                                    })
                                    .await;
                                let cont = rt.wants_goal_continue();
                                drop(g);
                                match turn {
                                    Ok(reply) => {
                                        let _ = tx2.send(UiEvent::Assistant(reply));
                                        if !cont {
                                            break;
                                        }
                                        let _ = tx2.send(UiEvent::System(
                                            "goal still active — continuing".into(),
                                        ));
                                        next = "Goal still active. Continue uninterrupted. Do not ask what to do. End with a line that is exactly GOAL_COMPLETE only when the condition is fully met.".into();
                                    }
                                    Err(e) => {
                                        let _ = tx2.send(UiEvent::Error(e.to_string()));
                                        break;
                                    }
                                }
                            }
                        }));
                    }
                    KeyCode::Backspace => composer.backspace(),
                    KeyCode::Delete => composer.delete(),
                    KeyCode::Left if composer.is_empty() && !busy => {
                        let now = Instant::now();
                        let repeated = pending_left
                            .is_some_and(|at| now.duration_since(at) < Duration::from_millis(700));
                        pending_left = Some(now);
                        if !repeated {
                            notice =
                                Some(("← again for the previous session".into(), Instant::now()));
                            continue;
                        }
                        pending_left = None;
                        let current = current_session_id.clone();
                        let rows = Session::list().unwrap_or_default();
                        let older = rows
                            .iter()
                            .position(|row| row.id == current)
                            .and_then(|index| rows.get(index + 1))
                            .map(|row| row.id.clone());
                        let Some(id) = older else {
                            notice = Some(("no older session".into(), Instant::now()));
                            continue;
                        };
                        match Session::load(&id) {
                            Ok(s) => {
                                log = chat_log(&s);
                                notice =
                                    Some((format!("session ← {}", s.meta.title), Instant::now()));
                                let mut g = shared.lock().await;
                                let _ = g.1.close_interrupted_tools();
                                g.0.reset_session_state();
                                let _ = registration.refresh(s.id(), &cwd, &s.meta.title);
                                g.1 = s;
                                composer.clear();
                                stream.reset();
                                scroll = 0;
                            }
                            Err(error) => log.push(ChatLine {
                                role: "error".into(),
                                text: format!("session load failed: {error}"),
                            }),
                        }
                    }
                    KeyCode::Left => composer.left(),
                    KeyCode::Right => composer.right(),
                    KeyCode::Home => composer.home(),
                    KeyCode::End => composer.end(),
                    KeyCode::PageUp => scroll = scroll.saturating_add(8),
                    KeyCode::PageDown => scroll = scroll.saturating_sub(8),
                    KeyCode::Up => {
                        composer.history_prev();
                    }
                    KeyCode::Down => {
                        if !composer.history_next() && composer.is_empty() {
                            picker = Some(activity_picker());
                        }
                    }
                    KeyCode::Char('o')
                        if key.modifiers.contains(KeyModifiers::CONTROL)
                            && composer.has_pastes() =>
                    {
                        // Open the paste preview on the newest card.
                        paste_preview = PastePreview::open(&composer, composer.pastes().len() - 1);
                    }
                    KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                        composer.insert_char(c);
                    }
                    _ => {}
                }
            }
        }
    }
    Ok(quit_session)
}

fn modal_rect(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    Rect::new(
        area.x + area.width.saturating_sub(width) / 2,
        area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    )
}

fn modal_block(title: &str) -> Block<'_> {
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .title(title)
        .border_style(Style::default().fg(Palette::accent()))
        .style(Style::default().bg(Palette::bg()).fg(Palette::fg()))
}

fn draw_secret_form(f: &mut ratatui::Frame, form: &SecretForm) {
    let rect = modal_rect(
        f.area(),
        64,
        (form.fields.len() + 5).min(u16::MAX as usize) as u16,
    );
    let mut rows: Vec<Line> = form
        .fields
        .iter()
        .enumerate()
        .map(|(index, field)| {
            Line::from(vec![
                Span::styled(
                    format!(
                        "{} {field}: ",
                        if index == form.selected { ">" } else { " " }
                    ),
                    Style::default().fg(Palette::accent_neon()),
                ),
                Span::raw(form.masked(index)),
            ])
        })
        .collect();
    rows.push(Line::from(form.error.clone()));
    f.render_widget(Clear, rect);
    f.render_widget(
        Paragraph::new(rows).block(modal_block(if form.keyring {
            " API Key "
        } else {
            " Required Environment "
        })),
        rect,
    );
}

fn draw_settings(f: &mut ratatui::Frame, dialog: &SettingsDialog) {
    let screen = f.area();
    let width = 104.min(screen.width.saturating_sub(4)).max(60);
    let height = 22.min(screen.height.saturating_sub(4)).max(14);
    let rect = modal_rect(screen, width, height);

    // Active tab bar with stylish badges
    let tabs_line = Line::from(
        SettingsDialog::TABS
            .iter()
            .enumerate()
            .flat_map(|(index, name)| {
                let is_active = index == dialog.tab;
                let style = if is_active {
                    Style::default()
                        .fg(Palette::bg())
                        .bg(Palette::accent_neon())
                        .add_modifier(ratatui::style::Modifier::BOLD)
                } else {
                    Style::default().fg(Palette::dim())
                };
                vec![Span::styled(format!("  {name}  "), style), Span::raw(" ")]
            })
            .collect::<Vec<_>>(),
    );

    let all_rows = dialog.rows();
    let total = all_rows.len();
    // Reserve space for: tabs (1), blank (1), editor/error (2), footer hint (1)
    let available_rows = (height as usize).saturating_sub(6).max(4);

    let scroll_offset = if dialog.selected >= available_rows {
        dialog.selected + 1 - available_rows
    } else {
        0
    };
    let visible_end = (scroll_offset + available_rows).min(total);

    let mut rows = vec![
        tabs_line,
        Line::from(Span::styled(
            "─".repeat(width.saturating_sub(2) as usize),
            Style::default().fg(Palette::border()),
        )),
    ];

    for index in scroll_offset..visible_end {
        if let Some((name, value)) = all_rows.get(index) {
            let is_sel = index == dialog.selected;
            let (prefix, row_style, val_style) = if is_sel {
                (
                    " ❯ ",
                    Style::default()
                        .fg(Palette::accent_neon())
                        .add_modifier(ratatui::style::Modifier::BOLD),
                    Style::default()
                        .fg(Palette::fg())
                        .add_modifier(ratatui::style::Modifier::BOLD),
                )
            } else {
                (
                    "   ",
                    Style::default().fg(Palette::fg()),
                    Style::default().fg(Palette::dim()),
                )
            };

            let name_col = format!("{name:<26}");
            rows.push(Line::from(vec![
                Span::styled(prefix, Style::default().fg(Palette::accent_neon())),
                Span::styled(name_col, row_style),
                Span::styled(value.clone(), val_style),
            ]));
        }
    }

    if let Some(editor) = &dialog.edit {
        rows.push(Line::from(""));
        rows.push(Line::from(vec![
            Span::styled(" ✎ Edit: ", Style::default().fg(Palette::accent_neon())),
            Span::styled(
                &editor.text,
                Style::default()
                    .fg(Palette::fg())
                    .add_modifier(ratatui::style::Modifier::UNDERLINED),
            ),
        ]));
    } else if !dialog.error.is_empty() {
        rows.push(Line::from(""));
        rows.push(Line::from(Span::styled(
            format!(" ✖ {}", dialog.error),
            Style::default().fg(Palette::error()),
        )));
    }

    // Scroll status indicator
    let scroll_hint = if total > available_rows {
        format!(
            " (showing {}-{} of {}) ",
            scroll_offset + 1,
            visible_end,
            total
        )
    } else {
        String::new()
    };

    f.render_widget(Clear, rect);
    f.render_widget(
        Paragraph::new(rows).block(
            modal_block(&format!(" ⚙ Varynth Settings{scroll_hint}"))
                .title_bottom(" ⇥ Tab / ⇤ Shift+Tab: Switch Tab │ ↑/↓: Navigate │ Enter: Select/Edit │ Ctrl+S: Save │ Esc: Close "),
        ),
        rect,
    );
}

fn draw_context(f: &mut ratatui::Frame, view: &ContextView) {
    let rect = modal_rect(f.area(), 72, 16);
    let bar_width = rect.width.saturating_sub(8) as usize;
    let filled = view
        .used
        .saturating_mul(bar_width)
        .checked_div(view.budget)
        .unwrap_or(0)
        .min(bar_width);
    let mut rows = vec![
        Line::from(format!(
            "{} / {} tokens ({:.1}%)",
            view.used,
            view.budget,
            view.used as f64 / view.budget as f64 * 100.0
        )),
        Line::from(vec![
            Span::styled(
                "#".repeat(filled),
                Style::default().fg(Palette::accent_neon()),
            ),
            Span::styled(
                "-".repeat(bar_width.saturating_sub(filled)),
                Style::default().fg(Palette::dim()),
            ),
        ]),
        Line::from(""),
    ];
    rows.extend(
        view.rows
            .iter()
            .map(|(name, tokens)| Line::from(format!("{name:<35} {tokens:>10}"))),
    );
    rows.push(Line::from(
        "Estimates include loaded instructions and tool schemas.",
    ));
    f.render_widget(Clear, rect);
    f.render_widget(
        Paragraph::new(rows).block(modal_block(" Context Budget ")),
        rect,
    );
}

fn draw_mcp(f: &mut ratatui::Frame, browser: &McpBrowser) {
    let rect = f.area();
    f.render_widget(Clear, rect);
    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(35), Constraint::Percentage(65)])
        .split(rect);
    let rows = browser.rows();
    let indices = composer::fuzzy_indices(&browser.query, &rows);
    let mut list = vec![Line::from(format!("> {}", browser.query)), Line::from("")];
    list.extend(indices.iter().enumerate().map(|(position, &index)| {
        Line::from(format!(
            "{} {} {}",
            if position == browser.selected {
                ">"
            } else {
                " "
            },
            rows[index].label,
            rows[index].detail
        ))
    }));
    list.push(Line::from(browser.note.clone()));
    f.render_widget(
        Paragraph::new(list)
            .block(modal_block(" MCP ").title_bottom(" Enter test  Ctrl+A add  Esc close ")),
        columns[0],
    );
    f.render_widget(
        Paragraph::new(browser.inspect())
            .wrap(Wrap { trim: false })
            .scroll((browser.scroll, 0))
            .block(modal_block(" Schema Inspector ")),
        columns[1],
    );
    if let Some(form) = &browser.add {
        let rect = modal_rect(rect, 90, 16);
        let mut rows = vec![
            Line::from(format!(
                "{}name: {}",
                if form.field == 0 { "> " } else { "  " },
                form.name
            )),
            Line::from(""),
            Line::from("Server JSON"),
        ];
        rows.extend(
            form.entry
                .text
                .lines()
                .map(|line| Line::from(composer::highlighted_line(line))),
        );
        rows.push(Line::from(browser.note.clone()));
        f.render_widget(Clear, rect);
        f.render_widget(
            Paragraph::new(rows).block(
                modal_block(" Add MCP Server ").title_bottom(" Tab field  Ctrl+S add  Esc cancel "),
            ),
            rect,
        );
    }
}

fn spawn_mcp_probe(
    shared: Arc<Mutex<(Runtime, Session)>>,
    tx: mpsc::UnboundedSender<UiEvent>,
    name: String,
) {
    tokio::task::spawn_blocking(move || {
        let mut g = shared.blocking_lock();
        let note = match g.0.mcp.probe_server_with_tools(&name) {
            Ok(status) => format!("{}: {} tools", status.name, status.tool_count.unwrap_or(0)),
            Err(error) => error.to_string(),
        };
        let _ = g.0.mcp.load_tools();
        let _ = tx.send(UiEvent::McpUpdated {
            statuses: g.0.mcp.status(),
            schemas: g.0.mcp.schemas().to_vec(),
            note,
        });
    });
}

struct DrawOut {
    clamp_scroll: usize,
}

fn draw(
    f: &mut ratatui::Frame,
    log: &[ChatLine],
    streaming: Option<&str>,
    composer: &Composer,
    picker: Option<&Picker>,
    notice: Option<&str>,
    busy: bool,
    spin: &str,
    tick: usize,
    scroll: usize,
    cwd: &str,
    model: &str,
    sandbox: &str,
    perm: PermissionMode,
    provider: &str,
    effort: &str,
    goal: Option<&GoalState>,
    session_title: &str,
    memory: &str,
    skills: Option<&SkillsBrowser>,
    dialog: Option<&ApprovalDialog>,
    auto_compact: bool,
    auto_percent: u8,
    context_percent: Option<u8>,
    mcp_summary: &str,
    skill_names: &[String],
    hints: bool,
) -> DrawOut {
    let _home = log.is_empty() && streaming.is_none() && !busy;
    let mark_h = 0u16;
    let header_h = 2 + u16::from(goal.is_some());
    let chat_floor = 6u16;
    let max_composer = f
        .area()
        .height
        .saturating_sub(mark_h + header_h + chat_floor)
        .max(4);
    let composer_h = composer.height(f.area().width).min(max_composer);
    let constraints = vec![
        Constraint::Length(header_h),
        Constraint::Min(chat_floor),
        Constraint::Length(composer_h),
    ];
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(f.area());
    let header_area = chunks[0];
    let chat_area = chunks[1];
    let composer_area = chunks[2];
    draw_header(
        f,
        header_area,
        busy,
        cwd,
        model,
        sandbox,
        provider,
        effort,
        goal,
        session_title,
        perm,
        context_percent,
    );
    let meta = ChatMeta {
        model,
        effort,
        perm,
        sandbox,
        cwd,
        session_title,
        mcp: mcp_summary,
        skills: skill_names,
        context_percent,
    };
    let clamp_scroll = draw_chat(
        f, chat_area, log, streaming, busy, spin, tick, memory, scroll, &meta,
    );
    composer::render(
        f,
        composer_area,
        composer,
        &composer::Status {
            busy,
            spin,
            perm,
            effort,
            goal: goal.is_some(),
            goal_for: goal.map(|g| g.started_at.elapsed()),
            notice,
            focused: picker.is_none() && skills.is_none(),
            hints,
            auto_compact,
            auto_percent,
        },
    );
    match picker {
        Some(p) => composer::render_picker(f, chat_area, p),
        None if composer.prefix_kind().is_some() => composer::render_prefix(f, chat_area, composer),
        None => composer::render_slash(f, chat_area, composer),
    }
    // The skills browser paints above the chat; the approval modal above it.
    if let Some(b) = skills {
        draw_skills_browser(f, b);
    }
    // The permission modal paints above everything else.
    if let Some(d) = dialog {
        draw_approval(f, d);
    }
    DrawOut { clamp_scroll }
}

/// Two-pane `/skills-search` explorer: left = search + rows, right = preview
/// and status lines. Full-screen modal; Esc closes (handled in the event loop).
fn draw_skills_browser(f: &mut ratatui::Frame, b: &SkillsBrowser) {
    use ratatui::widgets::{Block, Borders, List, ListItem, Paragraph, Wrap};
    let area = f.area();
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(45), Constraint::Percentage(55)])
        .split(area);
    let waiting = if b.waiting_search.is_some() {
        " …"
    } else {
        ""
    };

    // Left column: search input row on top, list below.
    let left = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(3)])
        .split(cols[0]);
    f.render_widget(Paragraph::new(format!("› {}", b.query)), left[0]);
    let rows: Vec<ListItem> = b
        .rows()
        .iter()
        .enumerate()
        .map(|(i, (installed, e))| {
            let mark = if *installed { "[installed] " } else { "" };
            let installs = e
                .installs
                .map(|n| format!(" · {n} installs"))
                .unwrap_or_default();
            let sel = if i == b.selected { "▸ " } else { "  " };
            ListItem::new(format!("{sel}{mark}{}{installs}", e.name))
        })
        .collect();
    let mut state = ratatui::widgets::ListState::default();
    state.select(Some(b.selected));
    f.render_stateful_widget(
        List::new(rows)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(format!(" skills-search{waiting} "))
                    .border_style(Style::default().fg(crate::theme::ACCENT)),
            )
            .highlight_style(Style::default().fg(crate::theme::ACCENT_NEON)),
        left[1],
        &mut state,
    );

    // Right column: description, preview text, status tail.
    let preview_title = b
        .preview_source
        .clone()
        .unwrap_or_else(|| "preview".into());
    let mut right_text = String::new();
    if let Some((_, e)) = b.rows().get(b.selected) {
        if !e.description.is_empty() {
            right_text.push_str(&e.description);
            right_text.push_str("\n\n");
        }
    }
    right_text.push_str(&b.preview_text);
    let status_tail: Vec<String> = b
        .status
        .iter()
        .rev()
        .take(4)
        .rev()
        .map(|(k, s)| {
            let tag = match k {
                StatusKind::Info => "·",
                StatusKind::Ok => "✓",
                StatusKind::Error => "✗",
            };
            format!("{tag} {s}")
        })
        .collect();
    if !status_tail.is_empty() {
        right_text.push_str("\n\n──────────\n");
        right_text.push_str(&status_tail.join("\n"));
    }
    let right = Paragraph::new(right_text)
        .wrap(Wrap { trim: false })
        .scroll((b.preview_scroll, 0))
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(format!(" {preview_title} "))
                .border_style(Style::default().fg(crate::theme::ACCENT)),
        );
    f.render_widget(right, cols[1]);
}

/// Centered modal for a pending prompt-mode permission request. The runtime
/// is blocked on the oneshot until the user answers here.
/// Full-screen preview of one paste card: numbered lines, scrollable. Tab
/// cycles between cards when several are attached; Ctrl+D drops the last one.
fn draw_paste_preview(f: &mut ratatui::Frame, pv: &PastePreview, composer: &Composer) {
    let Some(card) = composer.pastes().get(pv.card) else {
        return;
    };
    let area = f.area();
    let width = ((area.width as u32 * 4 / 5) as u16).max(44).min(area.width);
    let height = area.height.saturating_sub(4).max(7);
    let rect = Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.y + 2,
        width,
        height,
    };
    let viewport = rect.height.saturating_sub(2) as usize;
    let total = pv.editor.text.lines().count();
    let scroll = pv.editor.scroll.min(total.saturating_sub(viewport));
    let count = composer.pastes().len();
    let title = if count > 1 {
        format!("[PASTED TEXT #{} / {} · {}]", pv.card + 1, count, card.lang)
    } else {
        format!("[PASTED TEXT · {}]", card.lang)
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(Palette::accent_neon()))
        .title(Span::styled(
            format!(" {title} "),
            Style::default()
                .fg(Palette::accent_neon())
                .add_modifier(Modifier::BOLD),
        ))
        .title_bottom(
            Line::from(Span::styled(
                " Ctrl+D: drop this paste · tab: next card · esc/ctrl+o close ",
                Style::default().fg(Palette::dim()),
            ))
            .right_aligned(),
        )
        .style(Style::default().bg(Palette::bg()));
    let lines: Vec<Line> = pv
        .editor
        .text
        .lines()
        .enumerate()
        .map(|(i, l)| {
            let mut spans = vec![Span::styled(
                format!("{:>4} │ ", i + 1),
                Style::default().fg(Palette::dim()),
            )];
            spans.extend(composer::highlighted_line(l));
            Line::from(spans)
        })
        .collect();
    f.render_widget(Clear, rect);
    f.render_widget(
        Paragraph::new(lines)
            .block(block)
            .scroll((scroll as u16, 0)),
        rect,
    );
}

/// A failed settings transaction leaves the active runtime unchanged.
fn setting_notice(base: &str, result: Result<()>) -> String {
    match result {
        Ok(()) => base.to_string(),
        Err(error) => format!("{base} failed: {error}"),
    }
}

/// `/sandbox` selector. Enter applies the mode to the running session and
/// persists it to config.toml.
fn sandbox_picker(current: &str) -> Picker {
    const MODES: [(&str, &str); 4] = [
        ("read-only", "Read-only files"),
        ("workspace-write", "Write access in workspace"),
        ("danger-full-access", "No restrictions — caution"),
        (
            "docker-isolated",
            "Commands run in a Docker container (requires docker)",
        ),
    ];
    let items: Vec<PickItem> = MODES
        .iter()
        .map(|(m, desc)| PickItem {
            id: (*m).to_string(),
            label: (*m).to_string(),
            detail: if *m == current {
                format!("● current  ·  {desc}")
            } else {
                (*desc).to_string()
            },
        })
        .collect();
    let sel = items.iter().position(|it| it.id == current).unwrap_or(0);
    Picker {
        kind: PickerKind::Sandbox,
        items,
        sel,
        query: String::new(),
    }
}

/// Network search on a background thread; the reply carries the request's
/// `seq` so stale results can be dropped by the browser.
fn spawn_skill_search(tx: mpsc::UnboundedSender<UiEvent>, query: String, seq: u64) {
    std::thread::spawn(move || {
        let res = skills_search::search(&query);
        let _ = tx.send(UiEvent::SkillsResults { seq, res });
    });
}

/// SKILL.md preview fetch for the highlighted row.
fn spawn_skill_preview(tx: mpsc::UnboundedSender<UiEvent>, source: String) {
    std::thread::spawn(move || {
        let res = if Path::new(&source).is_file() {
            std::fs::read_to_string(&source).map_err(Into::into)
        } else {
            skills_search::fetch_skill_str(&source).map(|(_fm, raw)| raw)
        };
        let _ = tx.send(UiEvent::SkillsPreview { source, res });
    });
}

/// Install in the background: fetch, write to `~/.varynth/skills`, check env
/// requirements and refresh the installed list.
fn spawn_skill_install(tx: mpsc::UnboundedSender<UiEvent>, source: String, cached: Option<String>) {
    std::thread::spawn(move || {
        let skills_dir = crate::config::Config::home_dir().join("skills");
        let outcome = (|| -> Result<(String, Vec<String>, Vec<SkillEntry>)> {
            let raw = match cached {
                Some(raw) => raw,
                None => skills_search::fetch_skill_str(&source)?.1,
            };
            let report = skills_search::install_raw(&raw, &skills_dir, false)?;
            let missing_env = skills_search::check_requirements(&raw);
            let installed = skills_search::list_installed(&skills_dir);
            Ok((report.name, missing_env, installed))
        })();
        let ev = match outcome {
            Ok((name, missing_env, installed)) => UiEvent::SkillsInstalled {
                source,
                res: Ok(name),
                missing_env,
                installed,
            },
            Err(e) => UiEvent::SkillsInstalled {
                source,
                res: Err(e),
                missing_env: Vec::new(),
                installed: Vec::new(),
            },
        };
        let _ = tx.send(ev);
    });
}

fn draw_approval(f: &mut ratatui::Frame, dialog: &ApprovalDialog) {
    let area = f.area();
    let ascii = crate::brand::ascii_only();
    let width = ((area.width as u32 * 3 / 5) as u16).max(48).min(area.width);
    let height = 7.min(area.height);
    let rect = Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    };
    f.render_widget(Clear, rect);
    let footer = if ascii {
        Line::from(vec![
            Span::styled(
                "  [v Allow Once (y)]",
                Style::default()
                    .fg(Palette::ok_light())
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                "  [! Always (a)]",
                Style::default().fg(Palette::accent_neon()),
            ),
            Span::styled(
                "  [* Save Rule (s)]",
                Style::default().fg(Palette::info_light()),
            ),
            Span::styled(
                "  [x Deny (n/Esc)]",
                Style::default().fg(Palette::error_light()),
            ),
        ])
    } else {
        Line::from(vec![
            Span::styled(
                "  [✓ Allow Once (y)]",
                Style::default()
                    .fg(Palette::ok_light())
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                "  [⚡ Always (a)]",
                Style::default().fg(Palette::accent_neon()),
            ),
            Span::styled(
                "  [💾 Save Rule (s)]",
                Style::default().fg(Palette::info_light()),
            ),
            Span::styled(
                "  [✕ Deny (n/Esc)]",
                Style::default().fg(Palette::error_light()),
            ),
        ])
    };
    let body = vec![
        Line::from(""),
        Line::from(Span::styled(
            format!("  {}", dialog.text),
            Style::default().fg(Palette::fg()),
        )),
        Line::from(""),
        footer,
    ];
    let modal_title = if ascii {
        " ! Varynth Action & Patch Review "
    } else {
        " ⚡ Varynth Action & Patch Review "
    };
    f.render_widget(
        Paragraph::new(body)
            .block(
                Block::default()
                    .title(Span::styled(
                        modal_title,
                        Style::default()
                            .fg(Palette::warn_light())
                            .add_modifier(Modifier::BOLD),
                    ))
                    .borders(Borders::ALL)
                    .border_type(crate::brand::border_type())
                    .border_style(Style::default().fg(Palette::accent()))
                    .style(Style::default().bg(Palette::bg_panel())),
            )
            .wrap(Wrap { trim: false }),
        rect,
    );
}

fn draw_header(
    f: &mut ratatui::Frame,
    area: Rect,
    busy: bool,
    cwd: &str,
    model: &str,
    sandbox: &str,
    provider: &str,
    effort: &str,
    goal: Option<&GoalState>,
    session_title: &str,
    perm: PermissionMode,
    context_percent: Option<u8>,
) {
    let shown_model = display_model(model);
    let session = session_label(session_title);
    let mark = crate::brand::mark_char();
    let ascii = crate::brand::ascii_only();
    let perm_label = match perm {
        PermissionMode::Bypass => "bypass",
        PermissionMode::AcceptEdits => "accept-edits",
        PermissionMode::Prompt => "prompt",
        PermissionMode::Plan => "plan",
    };
    // Narrow terminals get the two facts that change how the agent behaves;
    // the rest of the header would be clipped, not read.
    if area.width < 60 {
        f.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(
                    format!(" {mark} "),
                    Style::default()
                        .fg(Palette::accent())
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(shown_model, Style::default().fg(Palette::accent_neon())),
                Span::styled("  ·  ", Style::default().fg(Palette::dim())),
                Span::styled(perm_label, Style::default().fg(Palette::warn())),
            ]))
            .style(Style::default().bg(Palette::bg_panel())),
            area,
        );
        return;
    }
    let spine = Span::styled(
        if ascii { "|" } else { "▌" },
        Style::default().fg(Palette::accent()),
    );
    let dot = Span::styled(" · ", Style::default().fg(Palette::dim()));
    let (mode_badge, mode_style) = match perm {
        PermissionMode::AcceptEdits => (
            if ascii {
                " >> Auto-Edit "
            } else {
                " ⏵⏵ Auto-Edit "
            },
            Style::default()
                .fg(Palette::ok_light())
                .bg(Palette::pill_cmd_bg())
                .add_modifier(Modifier::BOLD),
        ),
        PermissionMode::Plan => (
            if ascii { " || Plan " } else { " ⏸ Plan " },
            Style::default()
                .fg(Palette::info_light())
                .bg(Palette::pill_file_bg())
                .add_modifier(Modifier::BOLD),
        ),
        PermissionMode::Prompt => (
            if ascii {
                " [?] Ask First "
            } else {
                " 🛡 Ask First "
            },
            Style::default()
                .fg(Palette::warn_light())
                .bg(Palette::pill_skill_bg())
                .add_modifier(Modifier::BOLD),
        ),
        PermissionMode::Bypass => (
            if ascii { " ! Bypass " } else { " ⚡ Bypass " },
            Style::default()
                .fg(Palette::accent_neon())
                .bg(Palette::pill_agent_bg())
                .add_modifier(Modifier::BOLD),
        ),
    };
    let row1 = Line::from(vec![
        spine.clone(),
        Span::styled(
            if ascii { " * " } else { " ● " },
            Style::default()
                .fg(Palette::accent())
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            if ascii {
                "PS: Varynth CLI - "
            } else {
                "PS: Varynth CLI — "
            },
            Style::default()
                .fg(Palette::fg())
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(cwd.to_string(), Style::default().fg(Palette::info_light())),
        Span::styled(
            if ascii { "  |  " } else { "  │  " },
            Style::default().fg(Palette::border()),
        ),
        Span::styled(
            if ascii { "* " } else { "◆ " },
            Style::default().fg(Palette::accent_neon()),
        ),
        Span::styled(
            shown_model.to_string(),
            Style::default()
                .fg(Palette::accent_neon())
                .add_modifier(Modifier::BOLD),
        ),
        dot.clone(),
        Span::styled(
            effort.to_string(),
            Style::default().fg(Palette::warn_light()),
        ),
        dot.clone(),
        Span::styled(sandbox.to_string(), Style::default().fg(Palette::dim())),
        Span::raw("  "),
        Span::styled(mode_badge, mode_style),
    ]);

    let sep = if ascii { "  |  " } else { "  •  " };
    let ver_label = match crate::update::available() {
        Some(latest) => format!(" {mark} Varynth v{} (↑ v{latest}: varynth update)", env!("CARGO_PKG_VERSION")),
        None => format!(" {mark} Varynth v{}", env!("CARGO_PKG_VERSION")),
    };
    let ver_style = if crate::update::available().is_some() {
        Style::default()
            .fg(Palette::warn_light())
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Palette::dim())
    };
    let mut row2_spans = vec![
        spine.clone(),
        Span::styled(ver_label, ver_style),
        Span::styled(
            format!("{sep}session: "),
            Style::default().fg(Palette::dim()),
        ),
        Span::styled(
            session,
            Style::default()
                .fg(Palette::fg())
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("{sep}provider: "),
            Style::default().fg(Palette::dim()),
        ),
        Span::styled(
            provider.to_string(),
            Style::default().fg(Palette::info_light()),
        ),
    ];
    if let Some(p) = context_percent {
        let bar_w = 8usize;
        let filled = (p as usize * bar_w / 100).min(bar_w);
        let fill_ch = if ascii { "#" } else { "█" };
        let empty_ch = if ascii { "-" } else { "░" };
        row2_spans.push(Span::styled(
            format!("{sep}Context: "),
            Style::default().fg(Palette::dim()),
        ));
        row2_spans.push(Span::styled(
            fill_ch.repeat(filled),
            Style::default().fg(Palette::accent()),
        ));
        row2_spans.push(Span::styled(
            empty_ch.repeat(bar_w - filled),
            Style::default().fg(Palette::border()),
        ));
        row2_spans.push(Span::styled(
            format!(" {p}%"),
            Style::default().fg(if p >= 90 {
                Palette::warn()
            } else {
                Palette::ok_light()
            }),
        ));
    }
    let todo_path = std::path::Path::new(cwd)
        .join(".varynth")
        .join("todos.json");
    if let Ok(bytes) = std::fs::read(&todo_path) {
        if let Ok(store) = serde_json::from_slice::<crate::todo::TodoStore>(&bytes) {
            if let Some(badge) = store.summary_badge() {
                row2_spans.push(Span::styled(
                    format!("{sep}📋 {badge}"),
                    Style::default()
                        .fg(Palette::warn_light())
                        .add_modifier(Modifier::BOLD),
                ));
            }
        }
    }
    if busy {
        row2_spans.push(Span::styled(
            if ascii {
                format!("{sep}* working...")
            } else {
                format!("{sep}✻ working…")
            },
            Style::default().fg(Palette::accent_neon()),
        ));
    }
    let mut header = vec![row1, Line::from(row2_spans)];
    if let Some(goal) = goal {
        let short: String = goal.condition.chars().take(88).collect();
        let badge = match goal.status {
            GoalStatus::Active => format!("goal {}/{}", goal.rounds, goal.max_rounds),
            GoalStatus::Met => "goal met".into(),
            GoalStatus::Stopped => "goal stopped".into(),
        };
        header.push(Line::from(vec![
            spine.clone(),
            Span::styled(
                format!(" {badge}  "),
                Style::default()
                    .fg(Palette::ok())
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(short, Style::default().fg(Palette::ok())),
            Span::styled(
                format!("  ·  {}", fmt_elapsed(goal.started_at.elapsed())),
                Style::default().fg(Palette::dim()),
            ),
            Span::styled("  ·  /goal clear", Style::default().fg(Palette::dim())),
        ]));
    }
    f.render_widget(
        Paragraph::new(header).style(Style::default().bg(Palette::bg_panel())),
        area,
    );
}

fn empty_session_lines(meta: &ChatMeta<'_>, width: usize) -> Vec<Line<'static>> {
    let ascii = crate::brand::ascii_only();
    let dim = Style::default().fg(Palette::dim());
    let fg = Style::default().fg(Palette::fg());
    let border = Style::default().fg(Palette::border_purple());
    let inner = width.saturating_sub(4).max(8);

    let (mode_text, mode_color) = match meta.perm {
        PermissionMode::AcceptEdits => (
            if ascii {
                ">> Auto-Edit"
            } else {
                "⏵⏵ Auto-Edit"
            },
            Palette::ok_light(),
        ),
        PermissionMode::Plan => (
            if ascii {
                "|| Plan Mode"
            } else {
                "⏸ Plan Mode"
            },
            Palette::info_light(),
        ),
        PermissionMode::Prompt => (
            if ascii {
                "[?] Ask First"
            } else {
                "🛡 Ask First"
            },
            Palette::warn_light(),
        ),
        PermissionMode::Bypass => (
            if ascii { "! Bypass" } else { "⚡ Bypass" },
            Palette::accent_neon(),
        ),
    };
    let branch_prefix = if ascii { "|- " } else { "├─ " };
    let last_prefix = if ascii { "+- " } else { "╰─ " };
    let effort_suffix = if ascii {
        " (Deep Think)"
    } else {
        " ⚡ (Deep Think)"
    };

    let tree_model = vec![
        Span::styled(format!("{branch_prefix}Model:     "), dim),
        Span::styled(
            meta.model.to_string(),
            Style::default()
                .fg(Palette::accent_neon())
                .add_modifier(Modifier::BOLD),
        ),
    ];
    let tree_effort = vec![
        Span::styled(format!("{branch_prefix}Effort:    "), dim),
        Span::styled(
            format!("{}{effort_suffix}", meta.effort.to_uppercase()),
            Style::default()
                .fg(Palette::warn_light())
                .add_modifier(Modifier::BOLD),
        ),
    ];
    let tree_mode = vec![
        Span::styled(format!("{branch_prefix}Mode:      "), dim),
        Span::styled(
            mode_text.to_string(),
            Style::default().fg(mode_color).add_modifier(Modifier::BOLD),
        ),
    ];
    let tree_sandbox = vec![
        Span::styled(format!("{branch_prefix}Sandbox:   "), dim),
        Span::styled(
            meta.sandbox.to_string(),
            Style::default().fg(Palette::info_light()),
        ),
    ];
    let tree_workspace = vec![
        Span::styled(format!("{branch_prefix}Workspace: "), dim),
        Span::styled(meta.cwd.to_string(), fg),
    ];
    let tree_mcp = vec![
        Span::styled(format!("{branch_prefix}MCP:       "), dim),
        Span::styled(
            if ascii { "* " } else { "● " },
            Style::default().fg(Palette::ok()),
        ),
        Span::styled(
            meta.mcp.to_string(),
            Style::default().fg(Palette::ok_light()),
        ),
    ];

    let percent = meta.context_percent.unwrap_or(0);
    let bar_width = 16usize;
    let filled = (percent as usize * bar_width / 100).min(bar_width);
    let fill_ch = if ascii { "#" } else { "█" };
    let empty_ch = if ascii { "-" } else { "░" };
    let tree_context = vec![
        Span::styled(format!("{branch_prefix}Context:   "), dim),
        Span::styled(
            fill_ch.repeat(filled),
            Style::default().fg(Palette::accent()),
        ),
        Span::styled(
            empty_ch.repeat(bar_width - filled),
            Style::default().fg(Palette::border()),
        ),
        Span::styled(format!("  {percent}%"), dim),
    ];

    let mut skill_spans = vec![Span::styled(format!("{last_prefix}Skills:    "), dim)];
    if meta.skills.is_empty() {
        skill_spans.push(Span::styled("$create-skill", Palette::pill_skill_style()));
        skill_spans.push(Span::raw("  "));
        skill_spans.push(Span::styled("$refactor", Palette::pill_skill_style()));
        skill_spans.push(Span::raw("  "));
        skill_spans.push(Span::styled("$security-audit", Palette::pill_skill_style()));
        skill_spans.push(Span::styled("  (none installed)", dim));
    } else {
        let mut ordered: Vec<&String> = Vec::with_capacity(meta.skills.len());
        for s in meta.skills {
            let n = s.trim_start_matches('$');
            if !n.eq_ignore_ascii_case("motion-frames")
                && !n.eq_ignore_ascii_case("motion-fream")
                && !n.eq_ignore_ascii_case("motion-fream-pro")
            {
                ordered.push(s);
            }
        }
        for (idx, s) in ordered.into_iter().take(4).enumerate() {
            if idx > 0 {
                skill_spans.push(Span::raw("  "));
            }
            let tag = if s.starts_with('$') {
                s.clone()
            } else {
                format!("${s}")
            };
            skill_spans.push(Span::styled(tag, Palette::pill_skill_style()));
        }
        if meta.skills.len() > 4 {
            skill_spans.push(Span::styled("  …", dim));
        }
    }

    let div_ch = if ascii { "-" } else { "─" };
    let divider = vec![Span::styled(
        div_ch.repeat(inner),
        Style::default().fg(Palette::border_subtle()),
    )];

    let mut rows: Vec<Vec<Span<'static>>> = Vec::new();
    let wordmark = crate::brand::paint_wordmark();

    if width >= 100 {
        let right_rows = [
            tree_model,
            tree_effort,
            tree_mode,
            tree_sandbox,
            tree_workspace,
            tree_mcp,
        ];
        for (wm_line, right) in wordmark.into_iter().zip(right_rows) {
            let mut combined = wm_line.spans;
            combined.push(Span::raw("   "));
            combined.extend(right);
            rows.push(combined);
        }
        rows.push(vec![Span::styled(
            crate::brand::tagline_subtitle().to_string(),
            dim,
        )]);
        rows.push(divider.clone());
        rows.push(tree_context);
        rows.push(skill_spans);
    } else {
        if width >= 68 {
            for wm_line in wordmark {
                rows.push(wm_line.spans);
            }
        } else {
            rows.push(vec![Span::styled(
                crate::brand::banner_header().to_string(),
                Style::default()
                    .fg(Palette::accent_neon())
                    .add_modifier(Modifier::BOLD),
            )]);
        }
        rows.push(vec![Span::styled(
            crate::brand::tagline_subtitle().to_string(),
            dim,
        )]);
        rows.push(divider.clone());
        rows.push(tree_model);
        rows.push(tree_effort);
        rows.push(tree_mode);
        rows.push(tree_sandbox);
        rows.push(tree_workspace);
        rows.push(tree_mcp);
        rows.push(tree_context);
        rows.push(skill_spans);
    }

    rows.push(divider);
    rows.push(vec![
        Span::styled("Shortcuts: ", dim),
        Span::styled("[/] Commands", Palette::pill_cmd_style()),
        Span::raw("  "),
        Span::styled("[@] Subagents", Palette::pill_agent_style()),
        Span::raw("  "),
        Span::styled("[#] Files", Palette::pill_file_style()),
        Span::raw("  "),
        Span::styled("[$] Skills", Palette::pill_skill_style()),
        Span::raw("  "),
        Span::styled("[Shift+Tab] Mode", dim),
    ]);
    rows.push(vec![Span::styled(
        if ascii {
            "Memory: VARYNTH.md loaded | Autonomous subagent swarm ready"
        } else {
            "Memory: VARYNTH.md loaded • Autonomous subagent swarm ready"
        },
        Style::default().fg(Palette::ok_light()),
    )]);
    if let Some(latest) = crate::update::available() {
        rows.push(vec![
            Span::styled(
                if ascii { "▲ UPDATE: " } else { "↑ GÜNCELLEME: " },
                Style::default()
                    .fg(Palette::warn_light())
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("Yeni bir sürüm var (v{latest})! "),
                Style::default()
                    .fg(Palette::fg())
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                "`varynth update`",
                Palette::pill_cmd_style().add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                " yazarak güncelleyebilirsiniz.",
                Style::default().fg(Palette::dim()),
            ),
        ]);
    }

    let title = vec![
        Span::styled(
            if ascii { "* " } else { "✦ " },
            Style::default()
                .fg(Palette::accent_neon())
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            "VARYNTH AUTONOMOUS MULTIMODAL CLI AGENT",
            Style::default()
                .fg(Palette::fg())
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" "),
        Span::styled(
            format!(" v{} ", env!("CARGO_PKG_VERSION")),
            Palette::pill_agent_style().add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("  {}", meta.session_title),
            Style::default().fg(Palette::dim()),
        ),
    ];
    let mut out: Vec<Line<'static>> = Vec::new();
    out.extend(card_lines(title, rows, width, border));
    out
}

/// Everything the chat surface needs to describe the current session.
struct ChatMeta<'a> {
    model: &'a str,
    effort: &'a str,
    perm: PermissionMode,
    sandbox: &'a str,
    cwd: &'a str,
    session_title: &'a str,
    mcp: &'a str,
    skills: &'a [String],
    context_percent: Option<u8>,
}

/// Card chrome: `╭─ title ─────╮`, padded body rows, `╰──────╯`.
fn card_lines(
    title: Vec<Span<'static>>,
    body: Vec<Vec<Span<'static>>>,
    width: usize,
    border: Style,
) -> Vec<Line<'static>> {
    let ascii = crate::brand::ascii_only();
    let (tl, tr, bl, br, hbar, vbar) = if ascii {
        ("+- ", "+", "+", "+", "-", "|")
    } else {
        ("╭─ ", "╮", "╰", "╯", "─", "│")
    };
    let inner = width.saturating_sub(4).max(8);
    let mut out = Vec::with_capacity(body.len() + 2);
    let used = title
        .iter()
        .map(|span| span.content.chars().count())
        .sum::<usize>()
        .min(inner);
    let mut head = vec![Span::styled(tl, border)];
    head.extend(title);
    head.push(Span::styled(
        format!(" {}{tr}", hbar.repeat(inner.saturating_sub(used + 1))),
        border,
    ));
    out.push(Line::from(head));
    for row in body {
        // Clip to the inner width so a long row can never push the border.
        let mut spans: Vec<Span<'static>> = Vec::new();
        let mut used = 0usize;
        let mut clipped = false;
        for span in row {
            if used >= inner {
                clipped = true;
                break;
            }
            let take = (inner - used).min(span.content.chars().count());
            if take < span.content.chars().count() {
                clipped = true;
            }
            let text: String = span.content.chars().take(take).collect();
            used += take;
            spans.push(Span::styled(text, span.style));
        }
        let mut line = vec![Span::styled(format!("{vbar} "), border)];
        line.append(&mut spans);
        let pad = inner.saturating_sub(used + usize::from(clipped));
        let ell = if clipped {
            if ascii {
                "."
            } else {
                "…"
            }
        } else {
            ""
        };
        line.push(Span::styled(
            format!("{}{ell}{vbar}", " ".repeat(pad)),
            border,
        ));
        out.push(Line::from(line));
    }
    out.push(Line::from(Span::styled(
        format!("{bl}{}{br}", hbar.repeat(inner + 1)),
        border,
    )));
    out
}

fn draw_chat(
    f: &mut ratatui::Frame,
    area: Rect,
    log: &[ChatLine],
    streaming: Option<&str>,
    busy: bool,
    spin: &str,
    tick: usize,
    memory: &str,
    scroll: usize,
    meta: &ChatMeta<'_>,
) -> usize {
    let mut lines: Vec<Line> = Vec::new();
    let width = area.width as usize;
    let body_width = width.saturating_sub(8).max(12);
    let ascii = crate::brand::ascii_only();
    for msg in log {
        let kind = match msg.role.as_str() {
            "user" => "user",
            "assistant" => "assistant",
            "tool" => "tool",
            "error" => "error",
            _ => "system",
        };
        if kind == "tool" {
            let (name, detail) = msg
                .text
                .split_once("  ")
                .map(|(name, detail)| (name, detail))
                .unwrap_or((msg.text.as_str(), ""));
            let lower_name = name.to_ascii_lowercase();
            let dot_icon = if ascii { "*" } else { "●" };
            let tree_icon = if ascii { "|-" } else { "├─" };
            let mut spans = if lower_name.starts_with('@')
                || lower_name.contains("agent")
                || lower_name.contains("swarm")
            {
                let badge = if lower_name.starts_with('@') {
                    name.to_string()
                } else {
                    format!("@{name}")
                };
                vec![
                    Span::styled(
                        format!("  {tree_icon} {dot_icon} "),
                        Style::default().fg(Palette::accent()),
                    ),
                    Span::styled(
                        format!("[{badge}]"),
                        Palette::pill_agent_style().add_modifier(Modifier::BOLD),
                    ),
                ]
            } else if lower_name.contains("mcp") {
                vec![
                    Span::styled(
                        format!("  {dot_icon} MCP:"),
                        Style::default()
                            .fg(Palette::ok())
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        name.to_string(),
                        Style::default()
                            .fg(Palette::ok_light())
                            .add_modifier(Modifier::BOLD),
                    ),
                ]
            } else if matches!(
                lower_name.as_str(),
                "edit" | "write" | "patch" | "apply_patch"
            ) {
                vec![Span::styled(
                    format!("  {dot_icon} Edit "),
                    Style::default()
                        .fg(Palette::accent_neon())
                        .add_modifier(Modifier::BOLD),
                )]
            } else {
                vec![
                    Span::styled(format!("  {dot_icon} "), Style::default().fg(Palette::ok())),
                    Span::styled(
                        name.to_string(),
                        Style::default()
                            .fg(Palette::accent())
                            .add_modifier(Modifier::BOLD),
                    ),
                ]
            };
            if !detail.is_empty() {
                let clean_detail = detail.replace(['{', '}'], "");
                let detail_style = if lower_name.starts_with('@')
                    || lower_name.contains("agent")
                    || lower_name.contains("swarm")
                {
                    Style::default().fg(Palette::fg())
                } else if lower_name.contains("mcp") {
                    Style::default().fg(Palette::dim())
                } else if matches!(
                    lower_name.as_str(),
                    "edit" | "write" | "patch" | "apply_patch"
                ) {
                    Style::default().fg(Palette::info_light())
                } else {
                    Style::default().fg(Palette::accent_neon())
                };
                let sep = if matches!(
                    lower_name.as_str(),
                    "edit" | "write" | "patch" | "apply_patch"
                ) {
                    ""
                } else {
                    "  "
                };
                spans.push(Span::styled(format!("{sep}{clean_detail}"), detail_style));
            }
            lines.push(Line::from(spans));
            continue;
        }
        let body: Vec<Vec<Span<'static>>> = if kind == "user" {
            wrap_text(&msg.text, body_width)
                .into_iter()
                .map(|part| paint_user_line(&part))
                .collect()
        } else {
            markdown_lines(&msg.text, body_width)
        };
        let sep = if ascii { " | " } else { " • " };
        let (title, card_border) = match kind {
            "user" => (
                vec![
                    Span::styled(
                        if ascii { "> You" } else { "❯ You" },
                        Style::default()
                            .fg(Palette::accent_neon())
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        format!("{sep}{}", meta.cwd),
                        Style::default().fg(Palette::dim()),
                    ),
                ],
                Style::default().fg(Palette::border_purple()),
            ),
            "assistant" => (
                vec![
                    Span::styled(
                        if ascii { "* Varynth" } else { "◆ Varynth" },
                        Style::default()
                            .fg(Palette::accent_neon())
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        format!("{sep}{}", display_model(meta.model)),
                        Style::default().fg(Palette::dim()),
                    ),
                ],
                Style::default().fg(Palette::accent()),
            ),
            "error" => (
                vec![Span::styled(
                    if ascii { "x Error" } else { "✕ Error" },
                    Style::default()
                        .fg(Palette::error())
                        .add_modifier(Modifier::BOLD),
                )],
                Style::default().fg(Palette::error()),
            ),
            _ => (
                vec![Span::styled(
                    "· system",
                    Style::default()
                        .fg(Palette::dim())
                        .add_modifier(Modifier::BOLD),
                )],
                Style::default().fg(Palette::border()),
            ),
        };
        lines.extend(card_lines(title, body, width, card_border));
        lines.push(Line::from(""));
    }
    if let Some(text) = streaming {
        // Live reply in progress: the same card, replaced wholesale when the
        // final assistant message lands.
        let mut body = markdown_lines(text, body_width);
        body.push(vec![Span::styled(
            format!("{spin} streaming…"),
            Style::default().fg(Palette::accent_neon()),
        )]);
        let sep = if ascii { " | " } else { " • " };
        let title = vec![
            Span::styled(
                if ascii { "* Varynth" } else { "◆ Varynth" },
                Style::default()
                    .fg(Palette::accent_neon())
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("{sep}{}", display_model(meta.model)),
                Style::default().fg(Palette::dim()),
            ),
        ];
        lines.extend(card_lines(
            title,
            body,
            width,
            Style::default().fg(Palette::accent()),
        ));
        lines.push(Line::from(""));
    }
    if busy {
        let stage = match (tick / 12) % 3 {
            0 => {
                if ascii {
                    "* Varynth Thinking & Synthesizing AST Plan..."
                } else {
                    "✻ Varynth Thinking & Synthesizing AST Plan…"
                }
            }
            1 => {
                if ascii {
                    "! Running Subagent & Tool Pipeline..."
                } else {
                    "⚡ Running Subagent & Tool Pipeline…"
                }
            }
            _ => {
                if ascii {
                    "+ Verifying Output & Context..."
                } else {
                    "🔌 Verifying Output & Context…"
                }
            }
        };
        lines.push(Line::from(vec![
            Span::styled(
                format!("  {spin}  "),
                Style::default()
                    .fg(Palette::accent_neon())
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(stage, Style::default().fg(Palette::dim())),
        ]));
    }
    if log.is_empty() && streaming.is_none() && !busy {
        lines.extend(empty_session_lines(meta, width));
    } else if !busy {
        let (label, hint) = context_hint(tick, memory);
        lines.push(Line::from(""));
        lines.push(Line::from(vec![
            Span::styled("  ·  ", Style::default().fg(Palette::dim())),
            Span::styled(format!("{label}  "), Style::default().fg(Palette::dim())),
            Span::styled(hint, Style::default().fg(Palette::dim())),
        ]));
    }
    let visible = area.height as usize;
    let max_scroll = chat_max_scroll(lines.len(), visible);
    let scroll = scroll.min(max_scroll);
    // The empty/landing card is top-anchored: clipping it from the top would
    // hide the model and mode rows, which are the point of the panel.
    let skip = if log.is_empty() && streaming.is_none() && !busy {
        0
    } else {
        chat_skip(lines.len(), visible, scroll)
    };
    let mut shown: Vec<Line> = lines.into_iter().skip(skip).take(visible).collect();
    if scroll > 0 {
        if let Some(last) = shown.last_mut() {
            *last = Line::from(Span::styled(
                format!("  ↓ {scroll} more · PgDn"),
                Style::default().fg(Palette::dim()),
            ));
        }
    }
    f.render_widget(
        Paragraph::new(shown)
            .wrap(Wrap { trim: false })
            .style(Style::default().bg(Palette::bg())),
        area,
    );
    max_scroll
}

/// Aborts the running turn. Dropping its future releases the runtime lock;
/// tool calls it left open are closed before the next turn starts. The live
/// streaming buffer dies with the turn.
fn interrupt(
    task: &mut Option<tokio::task::JoinHandle<()>>,
    busy: &mut bool,
    stream: &mut StreamBuffer,
) -> bool {
    let Some(handle) = task.take() else {
        return false;
    };
    let was_running = !handle.is_finished();
    handle.abort();
    *busy = false;
    stream.reset();
    was_running
}

/// A slash command paints its name teal and the rest coral, like a skill chip.

/// Colours a user message body: `/command`, `@agent`, `#file` and `$skill`
/// tokens become pills so the composer vocabulary is visible in the log.
fn paint_user_line(line: &str) -> Vec<Span<'static>> {
    let base = Style::default().fg(Palette::fg());
    // Pills keep the token's own bytes so wrapping and copy/paste stay exact;
    // the emphasis is colour, not padding.
    let pill = |text: &str, style: Style| Span::styled(text.to_string(), style);
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut buf = String::new();
    let flush = |buf: &mut String, spans: &mut Vec<Span<'static>>| {
        if !buf.is_empty() {
            spans.push(Span::styled(std::mem::take(buf), base));
        }
    };
    for token in line.split_inclusive(' ') {
        let bare = token.trim_end();
        let trailing = &token[bare.len()..];
        let mut pill_style = None;
        if let Some(rest) = bare.strip_prefix('@') {
            if !rest.is_empty() {
                pill_style = Some(Palette::pill_agent_style().add_modifier(Modifier::BOLD));
            }
        } else if let Some(rest) = bare.strip_prefix('#') {
            if !rest.is_empty() {
                pill_style = Some(Palette::pill_file_style().add_modifier(Modifier::BOLD));
            }
        } else if let Some(rest) = bare.strip_prefix('$') {
            if !rest.is_empty() {
                pill_style = Some(Palette::pill_skill_style().add_modifier(Modifier::BOLD));
            }
        } else if bare.starts_with('/') && bare.len() > 1 {
            pill_style = Some(Palette::pill_cmd_style().add_modifier(Modifier::BOLD));
        }
        match pill_style {
            Some(style) => {
                flush(&mut buf, &mut spans);
                spans.push(pill(bare, style));
                if !trailing.is_empty() {
                    buf.push_str(trailing);
                }
            }
            None => buf.push_str(token),
        }
    }
    flush(&mut buf, &mut spans);
    spans
}

fn chat_max_scroll(len: usize, visible: usize) -> usize {
    len.saturating_sub(visible)
}

fn chat_skip(len: usize, visible: usize, scroll: usize) -> usize {
    let scroll = scroll.min(chat_max_scroll(len, visible));
    len.saturating_sub(visible + scroll)
}

#[cfg(test)]
mod paint_tests {
    use super::paint_user_line;

    fn texts(line: &str) -> Vec<String> {
        paint_user_line(line)
            .into_iter()
            .map(|span| span.content.into_owned())
            .collect()
    }

    #[test]
    fn composer_tokens_become_pills() {
        let spans = texts("@architect refactor #src/a.ts with $optimize");
        assert!(spans.iter().any(|s| s.contains("@architect")), "{spans:?}");
        assert!(spans.iter().any(|s| s.contains("#src/a.ts")), "{spans:?}");
        assert!(spans.iter().any(|s| s.contains("$optimize")), "{spans:?}");
        assert!(spans.iter().any(|s| s.trim() == "refactor"), "{spans:?}");
    }

    #[test]
    fn slash_commands_and_plain_text_stay_intact() {
        let spans = texts("/goal keep going");
        let joined: String = spans.concat();
        assert_eq!(joined, "/goal keep going");
        assert!(spans.iter().any(|s| s.trim() == "/goal"));
        let plain = texts("just text");
        assert_eq!(plain.concat(), "just text");
        assert_eq!(texts("/").concat(), "/");
    }
}

/// Token cost of the session: provider-reported totals when any completion
/// reported usage, chars÷4 estimate otherwise, plus wall-clock session time.
fn cost_line(rt: &Runtime, session: &Session) -> String {
    let chars = compact::estimate_messages(&session.messages);
    let mut s = format!(
        "≈{} tokens of context  ·  {} messages",
        chars.div_ceil(4),
        session.messages.len()
    );
    let images: usize = session.messages.iter().map(|m| m.images.len()).sum();
    if images > 0 {
        s.push_str(&format!("  ·  {images} images"));
    }
    if rt.usage_count > 0 {
        s.push_str(&format!(
            "\n{} in · {} out · {} total tokens (reported across {} turns)",
            rt.input_tokens,
            rt.output_tokens,
            rt.input_tokens + rt.output_tokens,
            rt.turns
        ));
    } else {
        s.push_str(
            "\nestimate only — the provider has not reported usage. /compact shrinks the context.",
        );
    }
    s.push_str(&format!(
        "\nsession {}  ·  model time {}",
        fmt_elapsed(rt.started_at.elapsed()),
        fmt_elapsed(rt.turn_time)
    ));
    s
}

/// "12m 34s" style duration, compact and human.
fn fmt_elapsed(d: Duration) -> String {
    let secs = d.as_secs();
    if secs >= 3600 {
        format!(
            "{}h {:02}m {:02}s",
            secs / 3600,
            (secs % 3600) / 60,
            secs % 60
        )
    } else if secs >= 60 {
        format!("{}m {:02}s", secs / 60, secs % 60)
    } else {
        format!("{secs}s")
    }
}

fn git_diff(cwd: &str) -> String {
    let run = |args: &[&str]| {
        std::process::Command::new("git")
            .args(args)
            .current_dir(cwd)
            .output()
    };
    match run(&["status", "--short"]) {
        Ok(o) if o.status.success() => {
            let status = String::from_utf8_lossy(&o.stdout).trim_end().to_string();
            if status.is_empty() {
                return "no changes".into();
            }
            let stat = run(&["diff", "--stat", "HEAD"])
                .ok()
                .filter(|o| o.status.success())
                .map(|o| String::from_utf8_lossy(&o.stdout).trim_end().to_string())
                .unwrap_or_default();
            if stat.is_empty() {
                status
            } else {
                format!("{status}\n\n{stat}")
            }
        }
        Ok(_) => format!("{cwd} is not a git repository"),
        Err(e) => format!("git is not available: {e}"),
    }
}

fn doctor_lines(v: &serde_json::Value) -> String {
    let Some(checks) = v.get("checks").and_then(|c| c.as_array()) else {
        return serde_json::to_string_pretty(v).unwrap_or_default();
    };
    checks
        .iter()
        .map(|c| {
            let ok = c.get("ok").and_then(|x| x.as_bool()).unwrap_or(false);
            format!(
                "{}  {:<14} {}",
                if ok { "✓" } else { "✗" },
                c.get("name").and_then(|x| x.as_str()).unwrap_or("?"),
                c.get("detail").and_then(|x| x.as_str()).unwrap_or("")
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn chat_log(session: &Session) -> Vec<ChatLine> {
    session
        .messages
        .iter()
        .filter(|m| m.role == "user" || m.role == "assistant")
        .map(|m| ChatLine {
            role: m.role.clone(),
            text: m.content.clone(),
        })
        .collect()
}

fn sessions_picker() -> Picker {
    let items = Session::list()
        .unwrap_or_default()
        .into_iter()
        .take(50)
        .map(|m| PickItem {
            label: m.title.chars().take(60).collect(),
            detail: format!(
                "{}  ·  {}",
                m.updated_at
                    .with_timezone(&chrono::Local)
                    .format("%d %b %H:%M"),
                m.model
            ),
            id: m.id,
        })
        .collect();
    Picker {
        kind: PickerKind::Sessions,
        items,
        sel: 0,
        query: String::new(),
    }
}

fn checkpoints_picker(items: Vec<PickItem>) -> Picker {
    // Newest snapshot selected by default: Enter restores the last write.
    let sel = items.len().saturating_sub(1);
    Picker {
        kind: PickerKind::Checkpoints,
        items,
        sel,
        query: String::new(),
    }
}

fn activity_picker() -> Picker {
    let mut items = Vec::new();
    if let Ok(log) = ActivityLog::load() {
        for row in log.visible() {
            let kind = match row.kind {
                ActivityKind::Shell => "shell",
                ActivityKind::Subagent => "agent",
            };
            let state = match row.state {
                ActivityState::Running => "running",
                ActivityState::Done => "done",
                ActivityState::Failed => "failed",
            };
            items.push(PickItem {
                id: row.id,
                label: format!("{kind}  {state}  {}", row.label),
                detail: row.detail,
            });
        }
    }
    if let Ok(store) = TaskStore::load() {
        for task in store.list() {
            items.push(PickItem {
                id: task.id.clone(),
                label: format!(
                    "task  {}  {}",
                    if task.enabled { "on" } else { "off" },
                    task.name
                ),
                detail: format!(
                    "next {}",
                    task.next_run_at
                        .with_timezone(&chrono::Local)
                        .format("%d %b %H:%M")
                ),
            });
        }
    }
    Picker {
        kind: PickerKind::Activity,
        items,
        sel: 0,
        query: String::new(),
    }
}

/// Inline markdown for one line: `code` becomes the theme's accent, **bold**
/// and *italic* map to terminal attributes. Unmatched delimiters stay literal.
fn inline_spans(text: &str) -> Vec<Span<'static>> {
    let body = Style::default().fg(Palette::fg());
    let code = Style::default().fg(Palette::accent_neon());
    let bold = Style::default()
        .fg(Palette::fg())
        .add_modifier(Modifier::BOLD);
    let italic = Style::default()
        .fg(Palette::fg())
        .add_modifier(Modifier::ITALIC);
    let mut out: Vec<Span<'static>> = Vec::new();
    let mut buf = String::new();
    let mut rest = text;
    let flush = |buf: &mut String, out: &mut Vec<Span<'static>>| {
        if !buf.is_empty() {
            out.push(Span::styled(std::mem::take(buf), body));
        }
    };
    while let Some(ch) = rest.chars().next() {
        match ch {
            '`' => {
                if let Some(end) = rest[1..].find('`') {
                    flush(&mut buf, &mut out);
                    out.push(Span::styled(format!(" {} ", &rest[1..1 + end]), code));
                    rest = &rest[1 + end + 1..];
                    continue;
                }
            }
            '*' if rest.starts_with("**") => {
                if let Some(end) = rest[2..].find("**") {
                    flush(&mut buf, &mut out);
                    out.push(Span::styled(rest[2..2 + end].to_string(), bold));
                    rest = &rest[2 + end + 2..];
                    continue;
                }
            }
            '*' => {
                if let Some(end) = rest[1..].find('*') {
                    flush(&mut buf, &mut out);
                    out.push(Span::styled(rest[1..1 + end].to_string(), italic));
                    rest = &rest[1 + end + 1..];
                    continue;
                }
            }
            _ => {}
        }
        buf.push(ch);
        rest = &rest[ch.len_utf8()..];
    }
    flush(&mut buf, &mut out);
    out
}

/// Greedy character wrap that keeps each span's style. Breaks prefer the last
/// space on the line; a long unbroken token is split at the width.
fn wrap_styled(chars: &[(char, Style)], width: usize) -> Vec<Vec<Span<'static>>> {
    let width = width.max(8);
    let mut lines: Vec<Vec<Span<'static>>> = Vec::new();
    let mut current: Vec<(char, Style)> = Vec::new();
    let mut break_at: Option<usize> = None;
    for (ch, style) in chars.iter().copied() {
        if ch == '\n' {
            lines.push(take_spans(&mut current));
            break_at = None;
            continue;
        }
        current.push((ch, style));
        if ch == ' ' {
            break_at = Some(current.len());
        }
        if current.len() > width {
            let split = break_at.unwrap_or(current.len());
            let tail = current.split_off(split);
            if current.last().is_some_and(|(c, _)| *c == ' ') {
                current.pop();
            }
            lines.push(take_spans(&mut current));
            current = tail;
            break_at = None;
        }
    }
    if !current.is_empty() {
        lines.push(take_spans(&mut current));
    }
    if lines.is_empty() {
        lines.push(Vec::new());
    }
    lines
}

/// Collapse a run of styled characters into spans, merging equal styles.
fn take_spans(chars: &mut Vec<(char, Style)>) -> Vec<Span<'static>> {
    let mut out: Vec<Span<'static>> = Vec::new();
    let mut text = String::new();
    let mut style: Option<Style> = None;
    for (ch, next) in chars.drain(..) {
        match style {
            Some(current) if current == next => text.push(ch),
            Some(current) => {
                out.push(Span::styled(std::mem::take(&mut text), current));
                text.push(ch);
                style = Some(next);
            }
            None => {
                style = Some(next);
                text.push(ch);
            }
        }
    }
    if let (Some(style), false) = (style, text.is_empty()) {
        out.push(Span::styled(text, style));
    }
    out
}

/// Renders a message body: fenced code blocks, headings, bullets and inline
/// markdown, already wrapped to `width`.
fn markdown_lines(text: &str, width: usize) -> Vec<Vec<Span<'static>>> {
    let mut out: Vec<Vec<Span<'static>>> = Vec::new();
    let mut fence: Option<&str> = None;
    let ascii = crate::brand::ascii_only();
    let vbar = if ascii { '|' } else { '│' };
    for raw in text.lines() {
        let trimmed = raw.trim_start();
        if trimmed.starts_with("```") {
            fence = match fence {
                Some(_) => None,
                None => Some(trimmed.trim_start_matches("```")),
            };
            out.push(vec![Span::styled(
                "  ".to_string(),
                Style::default().fg(Palette::dim()),
            )]);
            continue;
        }
        if fence.is_some() {
            let is_diff_add = trimmed.starts_with('+') && !trimmed.starts_with("+++");
            let is_diff_del = trimmed.starts_with('-') && !trimmed.starts_with("---");
            let (line_style, bar_color) = if is_diff_add {
                (Palette::diff_add_style(), Palette::diff_add_bar())
            } else if is_diff_del {
                (Palette::diff_del_style(), Palette::diff_del_bar())
            } else {
                (
                    Style::default()
                        .fg(Palette::accent_neon())
                        .bg(Palette::bg()),
                    Palette::border(),
                )
            };
            let styled: Vec<(char, Style)> = raw.chars().map(|c| (c, line_style)).collect();
            let mut prefix = vec![(vbar, Style::default().fg(bar_color)), (' ', line_style)];
            prefix.extend(styled);
            out.extend(wrap_styled(&prefix, width));
            continue;
        }
        let heading = trimmed.chars().take_while(|c| *c == '#').count();
        if heading > 0 && trimmed.chars().nth(heading) == Some(' ') {
            let body = trimmed[heading..].trim_start();
            let style = Style::default()
                .fg(Palette::accent())
                .add_modifier(Modifier::BOLD);
            out.push(vec![Span::styled(body.to_string(), style)]);
            continue;
        }
        let bullet = trimmed
            .strip_prefix("- ")
            .or_else(|| trimmed.strip_prefix("* "))
            .or_else(|| trimmed.strip_prefix("+ "));
        if let Some(body) = bullet {
            let mut spans = vec![Span::styled("• ", Style::default().fg(Palette::accent()))];
            spans.extend(inline_spans(body));
            out.extend(wrap_styled(&to_chars(&spans), width));
            continue;
        }
        if raw.is_empty() {
            out.push(Vec::new());
            continue;
        }
        out.extend(wrap_styled(&to_chars(&inline_spans(raw)), width));
    }
    out
}

/// Flatten spans back to styled characters for the wrapper.
fn to_chars(spans: &[Span<'static>]) -> Vec<(char, Style)> {
    spans
        .iter()
        .flat_map(|span| span.content.chars().map(move |ch| (ch, span.style)))
        .collect()
}

fn wrap_text(s: &str, width: usize) -> Vec<String> {
    if width < 12 {
        return vec![s.to_string()];
    }
    let mut out = Vec::new();
    for raw in s.lines() {
        if raw.is_empty() {
            out.push(String::new());
            continue;
        }
        let mut cur = String::new();
        for word in raw.split(' ') {
            if cur.is_empty() {
                cur = word.to_string();
            } else if cur.len() + 1 + word.len() <= width {
                cur.push(' ');
                cur.push_str(word);
            } else {
                out.push(cur);
                cur = word.to_string();
            }
        }
        if !cur.is_empty() {
            out.push(cur);
        }
    }
    if out.is_empty() {
        out.push(String::new());
    }
    out
}

fn display_model(model: &str) -> &str {
    model.rsplit(':').next().unwrap_or(model)
}

fn session_label(title: &str) -> String {
    let t = title.trim();
    if t.is_empty() || t == "new session" {
        "untitled".into()
    } else {
        t.chars().take(48).collect()
    }
}

fn context_hint(tick: usize, memory: &str) -> (&'static str, String) {
    if let Some(summary) = memory_hint(memory) {
        return ("context", summary);
    }
    const HINTS: [&str; 4] = [
        "Ask for a file diff, a test, or a quick status",
        "Alt+V attaches an image to the next turn",
        "← opens previous sessions · ↓ opens shells and subagents",
        "/skills lists the local skills available here",
    ];
    ("hint", HINTS[(tick / 45) % HINTS.len()].into())
}

fn memory_hint(memory: &str) -> Option<String> {
    memory
        .lines()
        .rev()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .filter(|line| !line.eq_ignore_ascii_case("durable facts and decisions are appended here."))
        .map(|line| {
            let clean = line.trim_start_matches(['-', '*', '•']).trim();
            clean.chars().take(96).collect::<String>()
        })
        .find(|line| !line.is_empty())
}

/// `/model` picker, opened on the current model. A model the provider doesn't
/// list stays in the menu so enter can't silently switch away from it.
#[allow(dead_code)]
fn models_picker(models: Vec<crate::providers::CatalogModel>, current: &str) -> Picker {
    let mut items: Vec<PickItem> = models
        .into_iter()
        .map(|m| PickItem {
            label: m.id.clone(),
            detail: m.display_name.or(m.owned_by).unwrap_or_default(),
            id: m.id,
        })
        .collect();
    if !current.is_empty() && !items.iter().any(|it| it.id == current) {
        items.insert(
            0,
            PickItem {
                id: current.to_string(),
                label: current.to_string(),
                detail: String::new(),
            },
        );
    }
    for it in &mut items {
        if it.id == current {
            it.detail = format!("● current  {}", it.detail).trim_end().to_string();
        }
    }
    let sel = items.iter().position(|it| it.id == current).unwrap_or(0);
    Picker {
        kind: PickerKind::Models,
        items,
        sel,
        query: String::new(),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ModelPane {
    Providers,
    Models,
}

#[derive(Debug, Clone)]
struct ModelRow {
    id: String,
    provider: String,
    kind: &'static str,
    context: &'static str,
    pricing: &'static str,
    connected: bool,
}

/// `oh-my-pi` style full-screen 2-pane `/model` selector with provider
/// filtering, model kind tabs (`all`, `chat`, `tiny`, `image`, `tts`, `stt`,
/// `judge`, `rerank`), fuzzy search, and multi-role bindings (`default`,
/// `smol`, `plan`, `memory`, `advisor`, `widget`, `fallback`).
struct ModelBrowser {
    all_models: Vec<ModelRow>,
    providers: Vec<(String, usize, bool)>,
    left_sel: usize,
    right_sel: usize,
    pane: ModelPane,
    kind_idx: usize,
    query: String,
    active_model: String,
    roles: crate::config::ModelRoles,
    note: String,
}

impl ModelBrowser {
    const KINDS: [&'static str; 8] = [
        "all", "chat", "tiny", "image", "tts", "stt", "judge", "rerank",
    ];

    fn new(catalog: Vec<crate::providers::CatalogModel>, cfg: &Config) -> Self {
        let mut seen = HashSet::new();
        let mut all_models = Vec::new();
        let connected_providers: HashSet<String> = {
            let mut set = HashSet::new();
            set.insert(cfg.provider.to_ascii_lowercase());
            set.insert("varynth".into());
            if cfg.openai_api_key.is_some() {
                set.insert("openai".into());
            }
            if cfg.anthropic_api_key.is_some() {
                set.insert("anthropic".into());
            }
            if cfg.google_api_key.is_some() {
                set.insert("google".into());
            }
            for p in &cfg.provider_profiles {
                if p.enabled && (p.api_key_env.as_deref() != Some("VARYNTH_ACCESS_TOKEN") || p.id == "varynth") {
                    set.insert(p.id.to_ascii_lowercase());
                }
            }
            set
        };

        let mut push_model = |id: String, owned_by: Option<String>| {
            if id.trim().is_empty() || !seen.insert(id.clone()) {
                return;
            }
            let provider = infer_provider(&id, owned_by.as_deref(), &cfg.provider, &connected_providers);
            let kind = infer_model_kind(&id);
            let (context, pricing) = infer_model_specs(&id, kind);
            let connected = connected_providers.contains(&provider.to_ascii_lowercase());
            all_models.push(ModelRow {
                id,
                provider,
                kind,
                context,
                pricing,
                connected,
            });
        };

        for m in catalog {
            if m.id.contains("public-") {
                continue;
            }
            push_model(m.id, m.owned_by);
        }
        // Built-in Varynth production models from the official API
        for m in crate::varynth_catalog::DEFAULT_PUBLIC_MODELS {
            push_model(format!("varynth/{m}"), Some("varynth".into()));
        }
        push_model("varynth/flux-1-schnell".into(), Some("varynth".into()));

        if !cfg.model.trim().is_empty() && !cfg.model.contains("public-") {
            push_model(cfg.model.clone(), None);
        }

        let role_models = [
            cfg.roles.default.as_deref(),
            cfg.roles.plan.as_deref(),
            cfg.roles.smol.as_deref(),
            cfg.roles.advisor.as_deref(),
            cfg.roles.widget.as_deref(),
            cfg.roles.memory.as_deref(),
            cfg.roles.fallback.as_deref(),
        ];
        for rm in role_models.into_iter().flatten() {
            if !rm.trim().is_empty() {
                push_model(rm.to_string(), None);
            }
        }

        let mut prov_counts: HashMap<String, (usize, bool)> = HashMap::new();
        for row in &all_models {
            if connected_providers.contains(&row.provider.to_ascii_lowercase()) {
                let entry = prov_counts
                    .entry(row.provider.clone())
                    .or_insert((0, row.connected));
                entry.0 += 1;
                entry.1 |= row.connected;
            }
        }
        let mut providers: Vec<(String, usize, bool)> = prov_counts
            .into_iter()
            .map(|(k, (c, conn))| (k, c, conn))
            .collect();
        let active_lower = cfg.provider.to_ascii_lowercase();
        providers.sort_by(|a, b| {
            let a_is_active = a.0.to_ascii_lowercase() == active_lower;
            let b_is_active = b.0.to_ascii_lowercase() == active_lower;
            b_is_active
                .cmp(&a_is_active)
                .then_with(|| (b.0.to_ascii_lowercase() == "varynth").cmp(&(a.0.to_ascii_lowercase() == "varynth")))
                .then_with(|| b.2.cmp(&a.2))
                .then_with(|| a.0.cmp(&b.0))
        });

        let mut b = Self {
            all_models,
            providers,
            left_sel: 1, // "All models" selected by default
            right_sel: 0,
            pane: ModelPane::Models,
            kind_idx: 0,
            query: String::new(),
            active_model: cfg.model.clone(),
            roles: cfg.roles.clone(),
            note: String::new(),
        };
        if let Some(pos) = b
            .filtered_models()
            .iter()
            .position(|m| m.id == cfg.model)
        {
            b.right_sel = pos;
        }
        b
    }

    fn left_len(&self) -> usize {
        // 0 = Roles, 1 = All models, 2.. = providers
        2 + self.providers.len()
    }

    fn current_kind(&self) -> &'static str {
        Self::KINDS[self.kind_idx % Self::KINDS.len()]
    }

    fn next_kind(&mut self) {
        self.kind_idx = (self.kind_idx + 1) % Self::KINDS.len();
        self.right_sel = 0;
    }

    fn prev_kind(&mut self) {
        self.kind_idx = (self.kind_idx + Self::KINDS.len() - 1) % Self::KINDS.len();
        self.right_sel = 0;
    }

    fn toggle_pane(&mut self) {
        self.pane = match self.pane {
            ModelPane::Providers => ModelPane::Models,
            ModelPane::Models => ModelPane::Providers,
        };
    }

    fn up(&mut self) {
        match self.pane {
            ModelPane::Providers => {
                self.left_sel = self.left_sel.saturating_sub(1);
                self.right_sel = 0;
            }
            ModelPane::Models => {
                self.right_sel = self.right_sel.saturating_sub(1);
            }
        }
    }

    fn down(&mut self) {
        match self.pane {
            ModelPane::Providers => {
                if self.left_sel + 1 < self.left_len() {
                    self.left_sel += 1;
                    self.right_sel = 0;
                }
            }
            ModelPane::Models => {
                let len = self.filtered_models().len();
                if self.right_sel + 1 < len {
                    self.right_sel += 1;
                }
            }
        }
    }

    fn type_char(&mut self, c: char) {
        self.query.push(c);
        self.right_sel = 0;
    }

    fn backspace(&mut self) {
        self.query.pop();
        self.right_sel = 0;
    }

    fn role_for_shortcut(&self, c: char) -> Option<crate::config::ModelRole> {
        use crate::config::ModelRole;
        // Number keys 1..=7 always bind roles; letter shortcuts work when search is empty or pane is Providers
        match c {
            '1' => Some(ModelRole::Default),
            '2' => Some(ModelRole::Smol),
            '3' => Some(ModelRole::Plan),
            '4' => Some(ModelRole::Memory),
            '5' => Some(ModelRole::Advisor),
            '6' => Some(ModelRole::Widget),
            '7' => Some(ModelRole::Fallback),
            _ if self.query.is_empty() && self.pane == ModelPane::Providers => match c {
                'd' => Some(ModelRole::Default),
                's' => Some(ModelRole::Smol),
                'p' => Some(ModelRole::Plan),
                'm' => Some(ModelRole::Memory),
                'a' => Some(ModelRole::Advisor),
                'w' => Some(ModelRole::Widget),
                'f' => Some(ModelRole::Fallback),
                _ => None,
            },
            _ => None,
        }
    }

    fn role_model(&self, role: crate::config::ModelRole) -> Option<&str> {
        use crate::config::ModelRole;
        match role {
            ModelRole::Default => self
                .roles
                .default
                .as_deref()
                .filter(|s| !s.is_empty())
                .or(Some(self.active_model.as_str())),
            ModelRole::Plan => self.roles.plan.as_deref().filter(|s| !s.is_empty()),
            ModelRole::Smol => self.roles.smol.as_deref().filter(|s| !s.is_empty()),
            ModelRole::Advisor => self.roles.advisor.as_deref().filter(|s| !s.is_empty()),
            ModelRole::Widget => self.roles.widget.as_deref().filter(|s| !s.is_empty()),
            ModelRole::Memory => self.roles.memory.as_deref().filter(|s| !s.is_empty()),
            ModelRole::Fallback => self.roles.fallback.as_deref().filter(|s| !s.is_empty()),
        }
    }

    fn roles_for_model(&self, id: &str) -> Vec<&'static str> {
        crate::config::ModelRole::ALL
            .iter()
            .copied()
            .filter(|&role| self.role_model(role) == Some(id))
            .map(|r| r.as_str())
            .collect()
    }

    fn filtered_models(&self) -> Vec<&ModelRow> {
        let kind = self.current_kind();
        let base: Vec<&ModelRow> = self
            .all_models
            .iter()
            .filter(|row| {
                if self.left_sel == 0 {
                    // Roles view: show only models bound to at least one role
                    !self.roles_for_model(&row.id).is_empty()
                } else if self.left_sel == 1 {
                    true
                } else if let Some((prov, _, _)) = self.providers.get(self.left_sel - 2) {
                    &row.provider == prov
                } else {
                    true
                }
            })
            .filter(|row| kind == "all" || row.kind == kind)
            .collect();

        if self.query.trim().is_empty() {
            return base;
        }
        let items: Vec<PickItem> = base
            .iter()
            .map(|r| PickItem {
                id: r.id.clone(),
                label: format!("{} {} {}", r.id, r.provider, r.kind),
                detail: String::new(),
            })
            .collect();
        composer::fuzzy_indices(&self.query, &items)
            .into_iter()
            .filter_map(|idx| base.get(idx).copied())
            .collect()
    }

    fn selected_model(&self) -> Option<&ModelRow> {
        let list = self.filtered_models();
        list.get(self.right_sel.min(list.len().saturating_sub(1)))
            .copied()
    }

    fn selected_model_id(&self) -> Option<String> {
        self.selected_model().map(|m| m.id.clone())
    }
}

fn infer_provider(
    id: &str,
    owned_by: Option<&str>,
    fallback: &str,
    valid_providers: &HashSet<String>,
) -> String {
    if id.starts_with("varynth/")
        || crate::varynth_catalog::DEFAULT_PUBLIC_MODELS.contains(&id)
    {
        return "varynth".into();
    }
    if let Some(rest) = id.strip_prefix("@cf/") {
        if let Some((vendor, _)) = rest.split_once('/') {
            let cf_vendor = format!("cf/{vendor}");
            if valid_providers.contains(&cf_vendor.to_ascii_lowercase()) {
                return cf_vendor;
            }
        }
        if valid_providers.contains("cloudflare") {
            return "cloudflare".into();
        }
        return fallback.to_string();
    }
    if let Some((prefix, _)) = id.split_once('/') {
        let prefix_lower = prefix.to_ascii_lowercase();
        if valid_providers.contains(&prefix_lower) {
            return prefix.to_string();
        }
        // If the prefix is an unconfigured vendor (e.g. microsoft/phi-3, meta-llama/Llama-3),
        // do NOT treat it as a separate provider! It belongs to the added provider (fallback).
        return fallback.to_string();
    }
    if let Some((prefix, _)) = id.split_once(':') {
        let prefix_lower = prefix.to_ascii_lowercase();
        if valid_providers.contains(&prefix_lower) {
            return prefix.to_string();
        }
        return fallback.to_string();
    }
    if let Some(owner) = owned_by.filter(|s| !s.trim().is_empty()) {
        let owner_lower = owner.to_ascii_lowercase();
        if valid_providers.contains(&owner_lower) {
            return owner.to_string();
        }
    }
    let lower = id.to_ascii_lowercase();
    if lower.contains("claude") && valid_providers.contains("anthropic") {
        "anthropic".into()
    } else if (lower.contains("gpt") || lower.starts_with("o1") || lower.starts_with("o3"))
        && valid_providers.contains("openai")
    {
        "openai".into()
    } else if lower.contains("gemini") && valid_providers.contains("google") {
        "google".into()
    } else if lower.contains("grok") && valid_providers.contains("xai") {
        "xai".into()
    } else if lower.contains("deepseek") && valid_providers.contains("deepseek") {
        "deepseek".into()
    } else {
        fallback.to_string()
    }
}

fn infer_model_kind(id: &str) -> &'static str {
    let lower = id.to_ascii_lowercase();
    if lower.contains("flux")
        || lower.contains("imagen")
        || lower.contains("dall-e")
        || lower.contains("sdxl")
    {
        "image"
    } else if lower.contains("aura") || lower.contains("tts") || lower.contains("eleven") {
        "tts"
    } else if lower.contains("whisper") || lower.contains("stt") || lower.contains("nova-2") {
        "stt"
    } else if lower.contains("rerank") || lower.contains("bge-") {
        "rerank"
    } else if lower.contains("judge") || lower.contains("guard") || lower.contains("audit") {
        "judge"
    } else if lower.contains("mini")
        || lower.contains("flash")
        || lower.contains("haiku")
        || lower.contains("8b")
        || lower.contains("3b")
        || lower.contains("2b")
        || lower.contains("1b")
        || lower.contains("micro")
        || lower.contains("tiny")
        || lower.contains("nano")
    {
        "tiny"
    } else {
        "chat"
    }
}

fn infer_model_specs(id: &str, kind: &str) -> (&'static str, &'static str) {
    let lower = id.to_ascii_lowercase();
    if lower.starts_with("varynth/") {
        match kind {
            "image" => return ("1024px", "included"),
            "tiny" => return ("128K", "free"),
            _ => return ("128K", "included"),
        }
    }
    match kind {
        "image" => ("4K / 1024px", "$0.003/img"),
        "tts" => ("streaming", "$0.015/1k"),
        "stt" => ("realtime", "$0.006/min"),
        "rerank" => ("8K", "$0.02/1M"),
        "tiny" => ("128K", "$0.15/1M"),
        _ if lower.contains("gemini") => ("1M", "$1.25/1M"),
        _ if lower.contains("opus") => ("200K", "$15.0/1M"),
        _ if lower.contains("sonnet") => ("200K", "$3.00/1M"),
        _ if lower.contains("gpt-5") => ("256K", "$2.50/1M"),
        _ => ("128K", "$1.00/1M"),
    }
}

fn draw_model_browser(f: &mut ratatui::Frame, mb: &ModelBrowser) {
    let area = f.area();
    let width = area.width.saturating_sub(4).max(40).min(area.width);
    let height = area.height.saturating_sub(2).max(10).min(area.height);
    let rect = Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    };
    f.render_widget(Clear, rect);

    let outer = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(Palette::accent_neon()))
        .title(Span::styled(
            " ◆ Models & Role Routing (oh-my-pi) ",
            Style::default()
                .fg(Palette::accent_neon())
                .add_modifier(Modifier::BOLD),
        ))
        .style(Style::default().bg(Palette::bg_panel()));
    let inner = outer.inner(rect);
    f.render_widget(outer, rect);

    if inner.height < 4 || inner.width < 20 {
        return;
    }

    let vchunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(3), Constraint::Length(3)])
        .split(inner);

    let left_w = (vchunks[0].width * 30 / 100).clamp(18, 34);
    let hchunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(left_w), Constraint::Min(10)])
        .split(vchunks[0]);

    // Left Pane: Providers & Roles
    let left_border = if mb.pane == ModelPane::Providers {
        Palette::accent_neon()
    } else {
        Palette::border()
    };
    let left_block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(left_border))
        .title(Span::styled(
            " Providers ",
            Style::default()
                .fg(Palette::fg())
                .add_modifier(Modifier::BOLD),
        ));
    let left_inner = left_block.inner(hchunks[0]);
    f.render_widget(left_block, hchunks[0]);

    let mut left_lines: Vec<Line> = Vec::new();
    let roles_count = mb
        .all_models
        .iter()
        .filter(|m| !mb.roles_for_model(&m.id).is_empty())
        .count();
    let fixed_rows = [
        (0usize, format!("◆ Roles ({roles_count})"), true),
        (1usize, format!("▸ All models ({})", mb.all_models.len()), true),
    ];
    for (idx, label, _) in fixed_rows {
        let selected = mb.left_sel == idx;
        let style = if selected {
            Style::default()
                .fg(Palette::accent_neon())
                .bg(Palette::pill_agent_bg())
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Palette::fg())
        };
        left_lines.push(Line::from(Span::styled(format!(" {label} "), style)));
    }
    for (i, (prov, count, connected)) in mb.providers.iter().enumerate() {
        let idx = i + 2;
        let selected = mb.left_sel == idx;
        let dot = if *connected { "●" } else { "○" };
        let dot_color = if *connected {
            Palette::ok()
        } else {
            Palette::dim()
        };
        let row_style = if selected {
            Style::default()
                .fg(Palette::accent_neon())
                .bg(Palette::pill_agent_bg())
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Palette::fg())
        };
        left_lines.push(Line::from(vec![
            Span::styled(format!(" {dot} "), Style::default().fg(dot_color)),
            Span::styled(format!("{prov} ({count}) "), row_style),
        ]));
    }
    f.render_widget(Paragraph::new(left_lines), left_inner);

    // Right Pane: Kind filter tabs, search bar, and model rows
    let right_border = if mb.pane == ModelPane::Models {
        Palette::accent_neon()
    } else {
        Palette::border()
    };
    let right_block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(right_border))
        .title(Span::styled(
            " Model Catalog ",
            Style::default()
                .fg(Palette::fg())
                .add_modifier(Modifier::BOLD),
        ));
    let right_inner = right_block.inner(hchunks[1]);
    f.render_widget(right_block, hchunks[1]);

    let rchunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(1),
        ])
        .split(right_inner);

    // Kind tabs bar
    let mut kind_spans = vec![Span::styled(" Kind: ", Style::default().fg(Palette::dim()))];
    for (idx, &k) in ModelBrowser::KINDS.iter().enumerate() {
        if idx > 0 {
            kind_spans.push(Span::styled(" · ", Style::default().fg(Palette::dim())));
        }
        if idx == mb.kind_idx {
            kind_spans.push(Span::styled(
                format!("◀ {k} ▶"),
                Style::default()
                    .fg(Palette::accent_neon())
                    .add_modifier(Modifier::BOLD),
            ));
        } else {
            kind_spans.push(Span::styled(k, Style::default().fg(Palette::dim())));
        }
    }
    f.render_widget(Paragraph::new(Line::from(kind_spans)), rchunks[0]);

    // Search row
    let search_line = Line::from(vec![
        Span::styled(
            " 🔍 > ",
            Style::default()
                .fg(Palette::accent_neon())
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            if mb.query.is_empty() {
                "type to fuzzy-filter models…".to_string()
            } else {
                mb.query.clone()
            },
            Style::default().fg(if mb.query.is_empty() {
                Palette::dim()
            } else {
                Palette::fg()
            }),
        ),
    ]);
    f.render_widget(Paragraph::new(search_line), rchunks[1]);

    // Models table
    let filtered = mb.filtered_models();
    let visible_h = rchunks[2].height as usize;
    let sel = mb.right_sel.min(filtered.len().saturating_sub(1));
    let skip = (sel + 1).saturating_sub(visible_h.max(1));
    let mut model_lines: Vec<Line> = Vec::new();
    for (i, row) in filtered.iter().enumerate().skip(skip).take(visible_h) {
        let is_sel = i == sel;
        let roles = mb.roles_for_model(&row.id);
        let badge = if roles.is_empty() {
            String::new()
        } else {
            format!(" [{}]", roles.join(","))
        };
        let marker = if is_sel { "▸ " } else { "  " };
        let row_bg = if is_sel {
            Palette::pill_agent_bg()
        } else {
            Palette::bg_panel()
        };
        let id_style = if is_sel {
            Style::default()
                .fg(Palette::accent_neon())
                .bg(row_bg)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Palette::fg())
        };
        let meta_style = Style::default().fg(Palette::dim()).bg(row_bg);
        let badge_style = Style::default()
            .fg(Palette::ok_light())
            .bg(row_bg)
            .add_modifier(Modifier::BOLD);
        model_lines.push(Line::from(vec![
            Span::styled(marker, id_style),
            Span::styled(format!("{:<36}", row.id), id_style),
            Span::styled(
                format!(" {:<6} · {:<6} · {:<10}", row.kind, row.context, row.pricing),
                meta_style,
            ),
            Span::styled(badge, badge_style),
        ]));
    }
    if model_lines.is_empty() {
        model_lines.push(Line::from(Span::styled(
            "  (no models match current filter)",
            Style::default().fg(Palette::dim()),
        )));
    }
    f.render_widget(Paragraph::new(model_lines), rchunks[2]);

    // Footer: Role bindings + Key hints
    let mut role_spans = vec![Span::styled(" Roles: ", Style::default().fg(Palette::dim()))];
    for (idx, role) in crate::config::ModelRole::ALL.iter().copied().enumerate() {
        if idx > 0 {
            role_spans.push(Span::styled(" · ", Style::default().fg(Palette::dim())));
        }
        let bound = mb.role_model(role);
        let dot = if bound.is_some() { "●" } else { "○" };
        let short_model = bound
            .map(|m| m.rsplit('/').next().unwrap_or(m))
            .unwrap_or("unset");
        let style = if bound.is_some() {
            Style::default().fg(Palette::ok_light())
        } else {
            Style::default().fg(Palette::dim())
        };
        role_spans.push(Span::styled(
            format!("{dot} {}:{}({})", idx + 1, role.as_str(), short_model),
            style,
        ));
    }
    let hint_text = if mb.note.is_empty() {
        " ⮐ select default · ↑/↓ navigate · Alt+←/→ kinds · Tab switch pane · 1-7 bind role · Esc close ".to_string()
    } else {
        format!(" ✓ {}  ·  ⮐ select · Tab pane · 1-7 bind role · Esc close ", mb.note)
    };
    let footer_lines = vec![
        Line::from(role_spans),
        Line::from(Span::styled(hint_text, Style::default().fg(Palette::dim()))),
    ];
    f.render_widget(Paragraph::new(footer_lines), vchunks[1]);
}

fn plugins_picker() -> Picker {
    let items = crate::plugins::rows()
        .into_iter()
        .map(|(name, hint, status)| PickItem {
            id: name.to_string(),
            label: name.to_string(),
            detail: format!("{hint}  ·  {status}"),
        })
        .collect();
    Picker {
        kind: PickerKind::Plugins,
        items,
        sel: 0,
        query: String::new(),
    }
}

/// `/effort` selector. Enter applies the level to the running session and
/// persists it to config.toml.
fn effort_picker(current: &str) -> Picker {
    const LEVELS: [(&str, &str); 6] = [
        ("low", "Minimal reasoning, fast replies"),
        ("medium", "Balanced thinking"),
        ("high", "Deep analysis, extended token budget"),
        ("xhigh", "Broad architectural planning"),
        ("max", "Maximum thinking windows"),
        ("ultra", "Multi-pass reasoning and self-verification"),
    ];
    let items: Vec<PickItem> = LEVELS
        .iter()
        .map(|(l, desc)| PickItem {
            id: (*l).to_string(),
            label: (*l).to_string(),
            detail: if *l == current {
                format!("● current  ·  {desc}")
            } else {
                (*desc).to_string()
            },
        })
        .collect();
    let sel = items.iter().position(|it| it.id == current).unwrap_or(0);
    Picker {
        kind: PickerKind::Effort,
        items,
        sel,
        query: String::new(),
    }
}

/// `/permission` selector. Enter applies the mode to the running session and
/// persists it to config.toml.
fn permission_picker(current: &str) -> Picker {
    const MODES: [(&str, &str); 4] = [
        ("acceptEdits", "Auto-approve file edits"),
        ("prompt", "Ask before every write or shell command"),
        ("bypass", "Never ask for confirmation — caution"),
        ("plan", "Plan first: writes and mutating commands refused"),
    ];
    let items: Vec<PickItem> = MODES
        .iter()
        .map(|(m, desc)| PickItem {
            id: (*m).to_string(),
            label: (*m).to_string(),
            detail: if *m == current {
                format!("● current  ·  {desc}")
            } else {
                (*desc).to_string()
            },
        })
        .collect();
    let sel = items.iter().position(|it| it.id == current).unwrap_or(0);
    Picker {
        kind: PickerKind::Permissions,
        items,
        sel,
        query: String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::chat_skip;
    use super::{
        approval_for_key, doctor_lines, git_diff, inline_spans, markdown_lines, slash_suggestion,
        unknown_slash_reply,
    };
    use crate::runtime::Approval;
    use crossterm::event::KeyCode;

    /// End-to-end render: a chat line with inline code must reach the frame
    /// with the accent foreground, and a tool line must not contain JSON.
    #[test]
    fn draw_chat_paints_code_and_keeps_tool_rows_readable() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;
        let log = vec![
            super::ChatLine {
                role: "assistant".into(),
                text: "calistir `cargo test` simdi".into(),
            },
            super::ChatLine {
                role: "tool".into(),
                text: "bash  cargo test".into(),
            },
        ];
        let backend = TestBackend::new(60, 12);
        let mut terminal = Terminal::new(backend).unwrap();
        let meta = super::ChatMeta {
            model: "evren/x",
            effort: "high",
            perm: crate::permissions::PermissionMode::AcceptEdits,
            sandbox: "workspace-write",
            cwd: "C:/ws",
            session_title: "test",
            mcp: "0 servers · 0 tools",
            skills: &[],
            context_percent: Some(10),
        };
        terminal
            .draw(|frame| {
                super::draw_chat(frame, frame.area(), &log, None, false, "⠋", 0, "", 0, &meta);
            })
            .unwrap();
        let buffer = terminal.backend().buffer().clone();
        let text: String = buffer
            .content
            .iter()
            .map(|cell| cell.symbol().to_string())
            .collect();
        assert!(text.contains("cargo test"), "{text}");
        assert!(!text.contains('{'), "tool row leaked JSON: {text}");
        let accent = crate::theme::Palette::accent_neon();
        let painted = buffer.content.iter().any(|cell| cell.fg == accent);
        assert!(painted, "inline code was not painted in the accent colour");
    }

    #[test]
    fn approval_accepts_both_vote_alphabets() {
        use crossterm::event::KeyModifiers;
        let plain = KeyModifiers::NONE;
        assert_eq!(
            approval_for_key(KeyCode::Char('y'), plain),
            Some(Approval::AllowOnce)
        );
        assert_eq!(
            approval_for_key(KeyCode::Char('O'), plain),
            Some(Approval::AllowOnce)
        );
        assert_eq!(
            approval_for_key(KeyCode::Char('a'), plain),
            Some(Approval::AllowAlways)
        );
        assert_eq!(
            approval_for_key(KeyCode::Char('s'), plain),
            Some(Approval::AllowAlwaysSaveRule)
        );
        assert_eq!(
            approval_for_key(KeyCode::Char('n'), plain),
            Some(Approval::Deny)
        );
        assert_eq!(approval_for_key(KeyCode::Esc, plain), Some(Approval::Deny));
        assert_eq!(approval_for_key(KeyCode::Char('q'), plain), None);
        // Chords must never approve or persist a rule.
        for modifier in [KeyModifiers::CONTROL, KeyModifiers::ALT] {
            for key in ['y', 'a', 's', 'n'] {
                assert_eq!(
                    approval_for_key(KeyCode::Char(key), modifier),
                    None,
                    "{modifier:?}+{key}"
                );
            }
        }
    }

    #[test]
    fn slash_typos_get_the_nearest_command() {
        assert_eq!(slash_suggestion("mode", &[]), Some("model".into()));
        assert_eq!(slash_suggestion("compactt", &[]), Some("compact".into()));
        assert_eq!(slash_suggestion("zzzzzz", &[]), None);
        assert!(unknown_slash_reply("mode", &[]).contains("/model"));
        assert!(unknown_slash_reply("zzzzzz", &[]).contains("/help"));
        assert!(unknown_slash_reply("mode\u{202E}", &[]).contains("/model"));
        assert!(!unknown_slash_reply("a\u{001b}[2J", &[]).contains('\u{001b}'));
        assert_eq!(super::unknown_slash("/mode", &[]), Some("mode".into()));
        assert_eq!(super::unknown_slash("/etc/hosts is odd", &[]), None);
    }

    #[test]
    fn inline_code_and_bold_get_their_own_style() {
        let spans = inline_spans("run `cargo test` now **hard**");
        assert!(spans.iter().any(|span| span.content.contains("cargo test")));
        let code = spans
            .iter()
            .find(|span| span.content.contains("cargo test"))
            .unwrap();
        assert_eq!(code.style.fg, Some(crate::theme::Palette::accent_neon()));
        let bold = spans
            .iter()
            .find(|span| span.content.contains("hard"))
            .unwrap();
        assert!(bold.style.add_modifier.contains(Modifier::BOLD));
        // Unmatched delimiters stay literal.
        let literal = inline_spans("a ` b");
        assert_eq!(
            literal
                .iter()
                .map(|s| s.content.as_ref())
                .collect::<String>(),
            "a ` b"
        );
    }

    #[test]
    fn markdown_renders_headings_bullets_and_fences() {
        let text = "# Başlık\n- ilk `kod`\n```\nlet x = 1;\n```";
        let lines = markdown_lines(text, 40);
        let joined: Vec<String> = lines
            .iter()
            .map(|spans| spans.iter().map(|s| s.content.as_ref()).collect())
            .collect();
        assert!(joined[0].contains("Başlık") && !joined[0].contains('#'));
        assert!(joined.iter().any(|line| line.contains("• ilk")));
        assert!(joined.iter().any(|line| line.contains("let x = 1;")));
        assert!(lines
            .iter()
            .all(|spans| spans.iter().all(|s| s.content.chars().count() <= 40)));
    }

    #[test]
    fn wrapped_markdown_never_exceeds_the_width() {
        let long = "kelime ".repeat(40);
        let lines = markdown_lines(&long, 24);
        assert!(lines.len() > 3);
        for line in &lines {
            let width: usize = line.iter().map(|s| s.content.chars().count()).sum();
            assert!(width <= 24, "line was {width} chars");
        }
    }

    #[test]
    fn doctor_lines_mark_each_check() {
        let v = serde_json::json!({"checks": [
            {"name": "config", "ok": true, "detail": "a"},
            {"name": "rustc", "ok": false, "detail": "missing"}
        ]});
        let t = doctor_lines(&v);
        assert!(t.contains("✓  config"));
        assert!(t.contains("✗  rustc"));
    }

    #[test]
    fn git_diff_reports_non_repo() {
        let dir = tempfile::tempdir().unwrap();
        let out = git_diff(&dir.path().display().to_string());
        assert!(out.contains("not a git repository") || out.contains("git is not available"));
    }

    #[test]
    fn chat_scroll_starts_at_the_bottom_and_clamps() {
        assert_eq!(chat_skip(40, 10, 0), 30);
        assert_eq!(chat_skip(40, 10, 5), 25);
        assert_eq!(chat_skip(40, 10, 100), 0);
        assert_eq!(chat_skip(4, 10, 3), 0);
    }

    use super::*;

    #[test]
    fn stream_buffer_drops_when_the_reply_lands_and_restarts_per_turn() {
        let mut sb = StreamBuffer::new();
        assert_eq!(sb.live_text(), None);
        sb.push("Hel");
        sb.push("lo");
        assert_eq!(sb.live_text(), Some("Hello"));
        // The final assistant message (or error/interrupt) replaces the
        // streamed text wholesale.
        sb.reset();
        assert_eq!(sb.live_text(), None);
        // The next turn streams into a fresh buffer.
        sb.push("next");
        assert_eq!(sb.live_text(), Some("next"));
    }

    #[test]
    fn session_label_untitled_when_empty() {
        assert_eq!(session_label("new session"), "untitled");
        assert_eq!(session_label("  "), "untitled");
        assert_eq!(session_label("fix the tui hint"), "fix the tui hint");
    }

    #[test]
    fn context_hints_stay_brandless() {
        for tick in 0..200 {
            let (_, hint) = context_hint(tick, "");
            let lower = hint.to_ascii_lowercase();
            assert!(!lower.contains("openclaw"), "{hint}");
            assert!(!lower.contains("claude code"), "{hint}");
            assert!(!lower.contains("codex"), "{hint}");
        }
    }

    #[test]
    fn display_model_strips_provider_prefix() {
        assert_eq!(display_model("grok-4.7-build"), "grok-4.7-build");
        assert_eq!(display_model("proxy:grok-4.7-build"), "grok-4.7-build");
    }

    #[test]
    fn fmt_elapsed_reads_like_a_clock() {
        assert_eq!(fmt_elapsed(Duration::from_secs(45)), "45s");
        assert_eq!(fmt_elapsed(Duration::from_secs(754)), "12m 34s");
        assert_eq!(fmt_elapsed(Duration::from_secs(3723)), "1h 02m 03s");
    }

    #[test]
    fn memory_hint_uses_last_meaningful_line() {
        let memory = "# Memory\n\nDurable facts and decisions are appended here.\n\n- Last task: fixed the session picker";
        assert_eq!(
            memory_hint(memory).as_deref(),
            Some("Last task: fixed the session picker")
        );
    }

    #[test]
    fn context_hint_falls_back_to_short_usage_hint() {
        let (_, hint) = context_hint(
            0,
            "# Memory\n\nDurable facts and decisions are appended here.",
        );
        assert!(hint.contains("file diff") || hint.contains("Alt+V"));
    }

    #[test]
    fn effort_picker_lists_all_levels_with_descriptions() {
        let p = effort_picker("xhigh");
        assert_eq!(p.items.len(), Runtime::EFFORT_LEVELS.len());
        assert_eq!(p.kind, PickerKind::Effort);
        let ultra = p.items.iter().find(|it| it.id == "ultra").unwrap();
        assert!(
            ultra.detail.contains("Multi-pass reasoning"),
            "{}",
            ultra.detail
        );
        let cur = p.items.iter().find(|it| it.id == "xhigh").unwrap();
        assert!(cur.detail.contains("● current"));
        assert_eq!(p.selected().map(|it| it.id.as_str()), Some("xhigh"));
    }

    #[test]
    fn permission_picker_lists_the_modes() {
        let p = permission_picker("prompt");
        let ids: Vec<&str> = p.items.iter().map(|it| it.id.as_str()).collect();
        assert_eq!(ids, vec!["acceptEdits", "prompt", "bypass", "plan"]);
        assert!(p.items[0].detail.contains("Auto-approve file edits"));
        assert!(p.items[2].detail.contains("caution"));
        assert_eq!(p.selected().map(|it| it.id.as_str()), Some("prompt"));
    }

    #[test]
    fn setting_notice_keeps_success_and_annotates_failures() {
        assert_eq!(setting_notice("effort → low", Ok(())), "effort → low");
        let failed = setting_notice("effort → low", Err(anyhow::anyhow!("disk full")));
        assert!(failed.starts_with("effort → low"));
        assert!(failed.contains("disk full"));
    }

    fn entry(name: &str, source: &str) -> SkillEntry {
        SkillEntry {
            name: name.into(),
            description: format!("{name} description"),
            source: source.into(),
            installs: None,
        }
    }

    #[test]
    fn skills_first_selection_previews_and_navigation_uses_cache() {
        let mut browser = SkillsBrowser::new(
            vec![entry("alpha", "alpha.md"), entry("beta", "beta.md")],
            "",
        );
        assert_eq!(browser.take_pending_preview().as_deref(), Some("alpha.md"));
        browser.preview_done("alpha.md", Ok("first preview".into()));
        browser.down();
        assert_eq!(browser.take_pending_preview().as_deref(), Some("beta.md"));
        browser.preview_done("beta.md", Ok("second preview".into()));
        browser.up();
        assert_eq!(browser.preview_text, "first preview");
        assert!(browser.take_pending_preview().is_none());
        browser.type_char('b');
        assert_eq!(browser.rows()[0].1.name, "beta");
        browser.preview_focus = true;
        browser.preview_scroll_down();
        assert_eq!(browser.selected, 0);
        assert!(browser.preview_scroll > 0);
    }

    #[test]
    fn skills_stale_search_results_are_not_published() {
        let mut browser = SkillsBrowser::new(Vec::new(), "");
        let old = browser.request_search("old");
        let current = browser.request_search("current");
        browser.search_done(old, Ok(vec![entry("old", "o/r/old")]));
        assert!(browser.results.is_empty());
        browser.search_done(current, Ok(vec![entry("current", "o/r/current")]));
        assert_eq!(browser.results.len(), 1);
    }

    #[test]
    fn offline_install_refreshes_slash_and_skill_catalog_without_overwrite() {
        let dir = tempfile::tempdir().unwrap();
        let raw = "---\nname: fresh-skill\ndescription: offline install\n---\nUse this skill.\n";
        let report = skills_search::install_raw(raw, dir.path(), false).unwrap();
        assert!(skills_search::install_raw(raw, dir.path(), false).is_err());
        assert_eq!(std::fs::read_to_string(&report.path).unwrap(), raw);
        let skill = crate::skills::parse_skill_file(&report.path).unwrap();
        let mut composer = Composer::default();
        refresh_skill_commands(
            &mut composer,
            &[],
            &[skill],
            &skills_search::list_installed(dir.path()),
        );
        assert!(composer
            .commands
            .iter()
            .any(|command| command.name == "fresh-skill"));
        assert!(composer.skills.iter().any(|row| row.id == "fresh-skill"));
    }

    #[test]
    fn modal_renderers_mask_secrets_and_work_in_small_terminals() {
        use ratatui::{backend::TestBackend, Terminal};
        let mut form = SecretForm::environment(vec!["TOKEN".into()]);
        form.values[0] = "sensitive-value".into();
        let dialog = SettingsDialog {
            draft: Config::default(),
            tab: 4,
            selected: 0,
            edit: None,
            key_status: vec![("proxy-token".into(), true)],
            error: String::new(),
        };
        let context = ContextView {
            rows: vec![("user messages".into(), 300)],
            used: 300,
            budget: 1000,
        };
        for (width, height) in [(30, 10), (100, 30)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal
                .draw(|frame| draw_secret_form(frame, &form))
                .unwrap();
            assert!(!terminal.backend().to_string().contains("sensitive-value"));
            assert!(terminal.backend().to_string().contains("****"));
            terminal
                .draw(|frame| draw_settings(frame, &dialog))
                .unwrap();
            terminal
                .draw(|frame| draw_context(frame, &context))
                .unwrap();
            assert!(terminal.backend().to_string().contains("300"));
        }
    }

    #[test]
    fn persisted_tui_config_never_contains_runtime_env_secrets() {
        let mut cfg = Config::default();
        cfg.use_keyring = Some(false);
        cfg.proxy_token = Some("private-proxy".into());
        cfg.openai_api_key = Some("private-openai".into());
        cfg.anthropic_api_key = Some("private-anthropic".into());
        cfg.google_api_key = Some("private-google".into());
        cfg.telegram_bot_token = Some("private-telegram".into());
        cfg.dashboard_token = Some("private-dashboard".into());
        cfg.provider_profiles = vec![crate::config::ProviderProfile {
            api_key: Some("private-profile".into()),
            ..Default::default()
        }];
        let raw = toml::to_string(&persistence_config(&cfg)).unwrap();
        assert!(!raw.contains("private-"));
        assert!(raw.contains("model"));
    }

    #[test]
    fn peer_resolution_uses_session_ids_and_rejects_legacy_registration() {
        let dir = tempfile::tempdir().unwrap();
        let registry = crate::terminals::Registry::at(dir.path().join("registry.json"));
        registry
            .heartbeat_session(
                "terminal-uuid",
                1,
                ".",
                Some("peer"),
                Some("actual-session"),
            )
            .unwrap();
        assert_eq!(
            resolve_peer("term:peer", &registry.live(), &[]).unwrap(),
            "actual-session"
        );
        registry
            .heartbeat("legacy-terminal", 2, ".", Some("legacy"))
            .unwrap();
        assert!(resolve_peer("term:legacy", &registry.live(), &[]).is_err());
    }

    #[test]
    fn welcome_banner_renders_wordmark_tree_grid_and_shortcuts() {
        let skills = vec!["refactor".to_string(), "security-audit".to_string()];
        let meta = ChatMeta {
            model: "varynth-omni-2.5",
            effort: "max",
            perm: PermissionMode::AcceptEdits,
            sandbox: "workspace-write",
            cwd: "C:/workspace/varynth",
            session_title: "untitled",
            mcp: "4 servers · 53 tools",
            skills: &skills,
            context_percent: Some(19),
        };
        for width in [120usize, 72usize] {
            let lines = empty_session_lines(&meta, width);
            for line in &lines {
                let line_width: usize = line.spans.iter().map(|s| s.content.chars().count()).sum();
                assert!(
                    line_width <= width,
                    "line width {line_width} exceeded requested width {width}"
                );
            }
            let joined: String = lines
                .iter()
                .map(|l| {
                    l.spans
                        .iter()
                        .map(|s| s.content.as_ref())
                        .collect::<String>()
                })
                .collect::<Vec<_>>()
                .join("\n");
            assert!(
                joined.contains("VARYNTH AUTONOMOUS MULTIMODAL CLI AGENT"),
                "missing VARYNTH AUTONOMOUS MULTIMODAL CLI AGENT at width {width}: {joined}"
            );
            for expected in [
                "Model:",
                "Effort:",
                "Mode:",
                "Workspace:",
                "MCP:",
                "Skills:",
                "Shortcuts:",
                "Memory:",
            ] {
                assert!(
                    joined.contains(expected),
                    "missing {expected} at width {width}: {joined}"
                );
            }
        }
    }

    #[test]
    fn welcome_banner_renders_wordmark_tree_grid_and_shortcuts_at_wide_and_narrow_widths() {
        let skills = vec!["refactor".to_string(), "security-audit".to_string()];
        let meta = ChatMeta {
            model: "varynth-omni-2.5",
            effort: "max",
            perm: PermissionMode::AcceptEdits,
            sandbox: "workspace-write",
            cwd: "C:/workspace/varynth",
            session_title: "untitled",
            mcp: "4 servers · 53 tools",
            skills: &skills,
            context_percent: Some(19),
        };
        for width in [120usize, 80usize, 52usize] {
            let lines = empty_session_lines(&meta, width);
            let joined: String = lines
                .iter()
                .map(|l| {
                    l.spans
                        .iter()
                        .map(|s| s.content.as_ref())
                        .collect::<String>()
                })
                .collect::<Vec<_>>()
                .join("\n");
            assert!(
                joined.contains("VARYNTH"),
                "missing VARYNTH header at width {width}: {joined}"
            );
            assert!(
                joined.contains("Model:"),
                "missing Model row at width {width}: {joined}"
            );
            assert!(
                joined.contains("Effort:"),
                "missing Effort row at width {width}: {joined}"
            );
            assert!(
                joined.contains("Mode:"),
                "missing Mode row at width {width}: {joined}"
            );
            assert!(
                joined.contains("Shortcuts:"),
                "missing Shortcuts row at width {width}: {joined}"
            );
            assert!(
                joined.contains("Memory: VARYNTH.md loaded"),
                "missing Memory row at width {width}: {joined}"
            );
        }
    }

    #[test]
    fn draw_header_renders_tab_pill_telemetry_and_context_bar() {
        use ratatui::{backend::TestBackend, Terminal};
        let mut terminal = Terminal::new(TestBackend::new(120, 4)).unwrap();
        terminal
            .draw(|f| {
                draw_header(
                    f,
                    Rect::new(0, 0, 120, 2),
                    true,
                    "C:/workspace/varynth",
                    "varynth-omni-2.5",
                    "workspace-write",
                    "openai",
                    "max",
                    None,
                    "swarm-router",
                    PermissionMode::AcceptEdits,
                    Some(25),
                );
            })
            .unwrap();
        let text = terminal.backend().to_string();
        assert!(text.contains("PS: Varynth CLI"), "{text}");
        assert!(text.contains("varynth-omni-2.5"), "{text}");
        assert!(text.contains("Auto-Edit"), "{text}");
        assert!(text.contains("session: swarm-router"), "{text}");
        assert!(text.contains("provider: openai"), "{text}");
        assert!(text.contains("Context: "), "{text}");
        assert!(text.contains("25%"), "{text}");
    }

    #[test]
    fn markdown_fenced_diff_lines_use_semantic_add_and_del_styles() {
        let md = "```diff\n+ added line\n- removed line\n  context line\n```";
        let lines = markdown_lines(md, 60);
        let has_add = lines.iter().any(|row| {
            row.iter()
                .any(|s| s.content.contains("+ added line") && s.style == Palette::diff_add_style())
        });
        let has_del = lines.iter().any(|row| {
            row.iter().any(|s| {
                s.content.contains("- removed line") && s.style == Palette::diff_del_style()
            })
        });
        assert!(has_add, "expected diff_add_style on + line");
        assert!(has_del, "expected diff_del_style on - line");
    }

    #[test]
    fn draw_approval_renders_review_title_and_action_badges() {
        use ratatui::{backend::TestBackend, Terminal};
        let dialog = ApprovalDialog {
            text: "edit  src/main.rs".into(),
            request: None,
        };
        let mut terminal = Terminal::new(TestBackend::new(100, 14)).unwrap();
        terminal.draw(|f| draw_approval(f, &dialog)).unwrap();
        let text = terminal.backend().to_string();
        assert!(text.contains("Varynth Action & Patch Review"), "{text}");
        assert!(text.contains("Allow Once (y)"), "{text}");
        assert!(text.contains("Always (a)"), "{text}");
        assert!(text.contains("Save Rule (s)"), "{text}");
        assert!(text.contains("Deny (n/Esc)"), "{text}");
    }

    #[test]
    fn model_browser_supports_kind_filtering_role_binding_and_2pane_render() {
        use ratatui::{backend::TestBackend, Terminal};
        let mut cfg = Config::default();
        cfg.set_role_model(
            crate::config::ModelRole::Plan,
            Some("anthropic/claude-opus-4.6".into()),
        );
        cfg.set_role_model(
            crate::config::ModelRole::Smol,
            Some("openai/gpt-5.4-mini".into()),
        );
        let mut mb = ModelBrowser::new(Vec::new(), &cfg);
        assert_eq!(mb.current_kind(), "all");
        assert!(!mb.filtered_models().is_empty());

        // Filter by image kind
        mb.next_kind(); // chat
        mb.next_kind(); // tiny
        mb.next_kind(); // image
        assert_eq!(mb.current_kind(), "image");
        let img_models = mb.filtered_models();
        assert!(img_models.iter().any(|m| m.id.contains("flux-1-schnell")));

        // Reset kind to all and switch to Roles view on left pane
        mb.kind_idx = 0;
        mb.toggle_pane();
        assert_eq!(mb.pane, ModelPane::Providers);
        mb.up(); // move from 1 (All models) to 0 (Roles)
        assert_eq!(mb.left_sel, 0);
        let role_models = mb.filtered_models();
        assert!(role_models
            .iter()
            .any(|m| m.id == "anthropic/claude-opus-4.6"));
        assert!(role_models.iter().any(|m| m.id == "openai/gpt-5.4-mini"));

        // Verify role shortcut mapping
        assert_eq!(
            mb.role_for_shortcut('1'),
            Some(crate::config::ModelRole::Default)
        );
        assert_eq!(
            mb.role_for_shortcut('3'),
            Some(crate::config::ModelRole::Plan)
        );
        assert_eq!(
            mb.role_for_shortcut('7'),
            Some(crate::config::ModelRole::Fallback)
        );

        let mut terminal = Terminal::new(TestBackend::new(120, 28)).unwrap();
        terminal.draw(|f| draw_model_browser(f, &mb)).unwrap();
        let rendered = terminal.backend().to_string();
        assert!(rendered.contains("Models & Role Routing"), "{rendered}");
        assert!(rendered.contains("Providers"), "{rendered}");
        assert!(rendered.contains("Model Catalog"), "{rendered}");
        assert!(rendered.contains("Roles:"), "{rendered}");
    }
}
