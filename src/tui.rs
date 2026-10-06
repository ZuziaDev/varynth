use anyhow::Result;
use crossterm::event::{
    self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind,
};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph, Wrap};
use ratatui::Terminal;
use std::io::{self, Stdout};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, Mutex};

use crate::activity::{ActivityKind, ActivityLog, ActivityState};
use crate::automation::TaskStore;
use crate::clipboard;
use crate::compact;
use crate::composer::{self, Composer, PickItem, Picker, PickerKind};
use crate::config::{self, Config};
use crate::mascot;
use crate::permissions::PermissionMode;
use crate::runtime::{Approval, ApprovalRequest, GoalState, GoalStatus, Runtime};
use crate::session::Session;
use crate::skills_search::{self, SkillEntry};
use crate::theme::Palette;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

const HELP: &str = "/goal <condition>  keep working until done  ·  /goal clear\n\
/agents <name>: <task>  run a one-shot subagent in the background\n\
/collaborate <term:N>  pair-program with a live terminal on the bus\n\
/compact  summarize earlier history into this session (auto when over budget)\n\
/cost     tokens and session time  ·  /diff  git changes\n\
/review   model review of the working-copy diff\n\
/send <id|latest> <text>  message another session on the bus\n\
/inbox    read messages from other sessions\n\
/undo     restore the last checkpointed write\n\
/doctor   check config, provider and proxy\n\
/resume   reopen a previous session (same as ←)\n\
/effort   pick reasoning effort  ·  /effort <level>\n\
/permission  pick the permission mode (acceptEdits / prompt / bypass)\n\
/status   runtime and session info\n\
/model    pick the model  ·  /model <id>\n\
/skills   list local skills  ·  /<skill> runs one\n\
/skills-search  find and install skills from skills.sh\n\
/sandbox  pick the sandbox mode\n\
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
                "yüklü beceriler listelendi · arama için yaz ve Enter'a bas".into(),
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
        self.note(StatusKind::Info, format!("aranıyor: {query} (skills.sh)"));
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
                    format!("{n} sonuç bulundu ({} yeni)", self.results.len()),
                );
                self.queue_preview();
            }
            Err(e) => self.note(StatusKind::Error, format!("arama başarısız: {e}")),
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
                self.note(StatusKind::Ok, format!("yüklendi: {name}"));
                for var in &missing_env {
                    self.note(StatusKind::Error, format!("eksik ortam değişkeni: {var}"));
                }
                if missing_env.is_empty() {
                    self.note(StatusKind::Info, format!("/{name} kullanıma hazır"));
                }
                self.installed = installed;
                self.results.retain(|e| e.source != source);
                self.clamp_selection();
            }
            Err(e) => self.note(StatusKind::Error, format!("kurulum başarısız: {e}")),
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
        Self {
            draft: cfg,
            tab: 0,
            selected: 0,
            edit: None,
            key_status: config::keyring_status(),
            error: String::new(),
        }
    }
    fn rows(&self) -> Vec<(String, String)> {
        match self.tab {
            0 => crate::theme::Theme::ALL
                .iter()
                .map(|theme| {
                    (
                        theme.name().into(),
                        if theme.name() == self.draft.theme {
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
                ("proxy URL".into(), safe_url(&self.draft.proxy_url)),
                (
                    "OpenAI URL".into(),
                    self.draft
                        .openai_base_url
                        .as_deref()
                        .map(safe_url)
                        .unwrap_or_else(|| "default".into()),
                ),
                (
                    "Anthropic URL".into(),
                    self.draft
                        .anthropic_base_url
                        .as_deref()
                        .map(safe_url)
                        .unwrap_or_else(|| "default".into()),
                ),
                ("effort".into(), self.draft.effort.clone()),
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
            0 => self.draft.theme = crate::theme::Theme::ALL[selected].name().into(),
            1 => match selected {
                0 => {
                    self.draft.provider =
                        cycle(&self.draft.provider, &["proxy", "openai", "anthropic"], 1)
                }
                5 => self.draft.effort = cycle(&self.draft.effort, &Runtime::EFFORT_LEVELS, 1),
                _ => {
                    let value = match selected {
                        1 => self.draft.model.clone(),
                        2 => self.draft.proxy_url.clone(),
                        3 => self.draft.openai_base_url.clone().unwrap_or_default(),
                        _ => self.draft.anthropic_base_url.clone().unwrap_or_default(),
                    };
                    let mut editor = composer::TextEditor::new(value);
                    editor.cursor = editor.text.chars().count();
                    self.edit = Some(editor);
                }
            },
            2 => self.draft.permission_mode = permission_picker("").items[selected].id.clone(),
            3 => self.draft.sandbox = sandbox_picker("").items[selected].id.clone(),
            _ => return Some(config::KEYRING_FIELDS[selected].into()),
        }
        None
    }
    fn finish_edit(&mut self) {
        let Some(edit) = self.edit.take() else {
            return;
        };
        let value = edit.text.trim().to_string();
        match self.selected {
            1 => self.draft.model = value,
            2 => self.draft.proxy_url = value,
            3 => self.draft.openai_base_url = (!value.is_empty()).then_some(value),
            4 => self.draft.anthropic_base_url = (!value.is_empty()).then_some(value),
            _ => {}
        }
    }
}

fn cycle(current: &str, choices: &[&str], delta: isize) -> String {
    let index = choices
        .iter()
        .position(|value| *value == current)
        .unwrap_or(0) as isize;
    choices[(index + delta).rem_euclid(choices.len() as isize) as usize].into()
}

fn safe_url(raw: &str) -> String {
    match reqwest::Url::parse(raw) {
        Ok(mut url) => {
            let _ = url.set_username("");
            let _ = url.set_password(None);
            url.set_query(None);
            url.set_fragment(None);
            url.to_string()
        }
        Err(_) => "<invalid URL>".into(),
    }
}

fn set_secret(cfg: &mut Config, field: &str, value: String) {
    match field {
        "proxy-token" => cfg.proxy_token = Some(value),
        "openai-api-key" => cfg.openai_api_key = Some(value),
        "anthropic-api-key" => cfg.anthropic_api_key = Some(value),
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
    anyhow::ensure!(
        crate::theme::Theme::parse(&cfg.theme).is_some(),
        "invalid theme"
    );
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
        "model" => cfg.model = value.into(),
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
    skills: &[crate::skills::Skill],
    installed: &[SkillEntry],
) {
    composer.commands = composer::builtin_commands();
    composer.commands.extend(
        skills
            .iter()
            .filter(|skill| {
                !composer::builtin_commands()
                    .iter()
                    .any(|command| command.name == skill.name)
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

pub async fn run(rt: Runtime, session: Session) -> Result<()> {
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

    let boot = std::time::Instant::now();
    while boot.elapsed() < Duration::from_millis(1100) {
        let tick = (boot.elapsed().as_millis() / 80) as usize;
        terminal.draw(|f| {
            let splash = mascot::splash(tick);
            f.render_widget(
                Paragraph::new(splash).style(Style::default().bg(Palette::bg())),
                f.area(),
            );
        })?;
        if event::poll(Duration::from_millis(16))? {
            if let Event::Key(k) = event::read()? {
                if k.kind == KeyEventKind::Press {
                    break;
                }
            }
        }
    }

    let result = run_loop(&mut terminal, rt, session).await;

    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        DisableBracketedPaste,
        DisableMouseCapture,
        LeaveAlternateScreen
    )?;
    terminal.show_cursor()?;
    result
}

async fn run_loop(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    rt: Runtime,
    session: Session,
) -> Result<()> {
    let cwd = rt.jail.cwd.display().to_string();
    let agents = rt.agents_md_line();
    let mut log = chat_log(&session);

    // Prompt-mode approvals: the runtime hands requests over this channel
    // and the modal dialog answers them. The receiver is shared with the
    // turn task so the UI event envelope can carry the oneshot along.
    let (approval_tx, approval_rx) = mpsc::unbounded_channel::<ApprovalRequest>();
    let approval_rx = Arc::new(std::sync::Mutex::new(approval_rx));
    let mut rt = rt;
    rt.approver = Some(approval_tx);

    let registry = crate::terminals::Registry::default();
    let registration = registry.register(session.id(), &cwd, &session.meta.title)?;
    let mut heartbeat = Heartbeat::new();
    let personas = crate::agents::personas();
    let mut peer_terms = registry.live();
    let mut session_rows = Session::list().unwrap_or_default();
    let mut installed = installed_skills(&rt);
    let mut composer = Composer::default();
    composer.workspace_files = composer::scan_jail_files(&rt.jail);
    composer.mentions = mention_catalog(&personas, &peer_terms, &session_rows, &registration.id);
    refresh_skill_commands(&mut composer, &rt.skills, &installed);
    let shared = Arc::new(Mutex::new((rt, session)));
    let (tx, mut rx) = mpsc::unbounded_channel::<UiEvent>();
    let mut picker: Option<Picker> = None;
    let mut notice: Option<(String, Instant)> = None;
    let mut busy = false;
    // Pending prompt-mode permission request, shown as a modal dialog.
    let mut dialog: Option<ApprovalDialog> = None;
    // Full-screen preview of a paste card, opened with Ctrl+O.
    let mut paste_preview: Option<PastePreview> = None;
    // Full-width `/skills-search` explorer: search, preview and install
    // skills from skills.sh. Network work runs in background tasks that
    // report back through the UiEvent channel.
    let mut skills: Option<SkillsBrowser> = None;
    let mut settings: Option<SettingsDialog> = None;
    let mut context: Option<ContextView> = None;
    let mut mcp: Option<McpBrowser> = None;
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
    // Click on the octopus: one short dance, then the still pose.
    let mut dance: Option<(usize, usize)> = None;
    // Rows above the bottom of the chat. 0 sticks to the latest line.
    let mut scroll: usize = 0;
    let frames = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
    let modes = [
        PermissionMode::AcceptEdits,
        PermissionMode::Prompt,
        PermissionMode::Bypass,
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
            if reload_skills {
                g.0.skills = crate::skills::discover(&g.0.jail.cwd);
                installed = installed_skills(&g.0);
                refresh_skill_commands(&mut composer, &g.0.skills, &installed);
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
            ));
        }
        composer.mentions =
            mention_catalog(&personas, &peer_terms, &session_rows, &registration.id);
        if let Some(browser) = skills.as_mut() {
            if let Some(source) = browser.take_pending_preview() {
                spawn_skill_preview(tx.clone(), source);
            }
        }
        let Some((model, sandbox, perm, provider, effort, goal, session_title, memory, theme)) =
            snapshot.clone()
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
        if let Some((_, step)) = dance.as_mut() {
            *step += 1;
            if *step >= mascot::DANCE_LEN * 3 {
                dance = None;
            }
        }
        let mut max_scroll = 0;
        terminal.draw(|f| {
            crate::theme::with_theme(&theme, || {
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
                    dance,
                    scroll,
                    &cwd,
                    &model,
                    &sandbox,
                    perm,
                    &provider,
                    &effort,
                    goal.as_ref(),
                    &agents,
                    &session_title,
                    &memory,
                    skills.as_ref(),
                    None,
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
        let next_event = if composer.queued() > 0
            && !busy
            && skills.is_none()
            && picker.is_none()
            && dialog.is_none()
            && paste_preview.is_none()
            && settings.is_none()
            && context.is_none()
            && mcp.is_none()
            && secret_form.is_none()
            && composer.prefix_kind().is_none()
            && composer.slash_items().is_empty()
        {
            Some(Event::Key(KeyEvent::new(
                KeyCode::Enter,
                KeyModifiers::NONE,
            )))
        } else if event::poll(Duration::from_millis(40))? {
            Some(event::read()?)
        } else {
            None
        };
        if let Some(ev) = next_event {
            if let Event::Mouse(mouse) = &ev {
                if skills.is_some()
                    || paste_preview.is_some()
                    || dialog.is_some()
                    || settings.is_some()
                    || context.is_some()
                    || mcp.is_some()
                    || secret_form.is_some()
                {
                    continue;
                }
                match mouse.kind {
                    MouseEventKind::ScrollUp => scroll = scroll.saturating_add(3),
                    MouseEventKind::ScrollDown => scroll = scroll.saturating_sub(3),
                    MouseEventKind::Down(MouseButton::Left)
                        if mouse.column < mascot::GLYPH_WIDTH as u16
                            && mouse.row < mascot::GLYPH_HEIGHT as u16
                            && dance.is_none() =>
                    {
                        dance = Some(((spinner as usize) % 4, 0));
                    }
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
                    match (d.request, key.code) {
                        (Some(req), KeyCode::Char('o') | KeyCode::Char('O')) => {
                            let _ = req.respond.send(Approval::AllowOnce);
                        }
                        (Some(req), KeyCode::Char('a') | KeyCode::Char('A')) => {
                            let _ = req.respond.send(Approval::AllowAlways);
                        }
                        (Some(req), KeyCode::Char('d') | KeyCode::Char('D') | KeyCode::Esc) => {
                            let _ = req.respond.send(Approval::Deny);
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
                                            dialog.key_status = config::keyring_status();
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
                                    cfg_dialog.selected = cfg_dialog.selected.saturating_sub(1)
                                }
                                KeyCode::Down => {
                                    cfg_dialog.selected = (cfg_dialog.selected + 1)
                                        .min(cfg_dialog.rows().len().saturating_sub(1))
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
                                                format!("yükleniyor: {source}"),
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
                    // instead of quitting; Ctrl+C always quits.
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
                                        .unwrap_or_else(|| "token yok".into());
                                    notice = Some((format!("{name} · {label}"), Instant::now()));
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
                    KeyCode::Enter if busy => match composer.enqueue() {
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
                    },
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
                                | "/config"
                                | "/context"
                                | "/mcp"
                                | "/new"
                                | "/clear"
                                | "/rename"
                                | "/sandbox"
                        ) || line.starts_with("/agents")
                            || line.starts_with("/rename ")
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
                        if line == "/config" {
                            settings = Some(SettingsDialog::new(shared.lock().await.0.cfg.clone()));
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
                        if line == "/sandbox" {
                            let cur = shared.lock().await.0.cfg.sandbox.clone();
                            picker = Some(sandbox_picker(&cur));
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
                        if line == "/plugins" {
                            picker = Some(plugins_picker());
                            continue;
                        }
                        if line == "/models" || line == "/model" {
                            let g = shared.lock().await;
                            match g.0.list_models().await {
                                Ok(ms) => picker = Some(models_picker(ms, &g.0.cfg.model)),
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
                        picker = Some(sessions_picker());
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
    Ok(())
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
    let rect = modal_rect(f.area(), 100, 18);
    let mut rows = vec![
        Line::from(
            SettingsDialog::TABS
                .iter()
                .enumerate()
                .map(|(index, name)| {
                    Span::styled(
                        format!(" {name} "),
                        if index == dialog.tab {
                            Style::default()
                                .fg(Palette::bg())
                                .bg(Palette::accent_neon())
                        } else {
                            Style::default().fg(Palette::dim())
                        },
                    )
                })
                .collect::<Vec<_>>(),
        ),
        Line::from(""),
    ];
    rows.extend(
        dialog
            .rows()
            .into_iter()
            .enumerate()
            .map(|(index, (name, value))| {
                Line::from(vec![
                    Span::styled(
                        format!(
                            "{} {name:<20} ",
                            if index == dialog.selected { ">" } else { " " }
                        ),
                        Style::default().fg(Palette::accent_neon()),
                    ),
                    Span::raw(value),
                ])
            }),
    );
    if let Some(editor) = &dialog.edit {
        rows.push(Line::from(format!("edit: {}", editor.text)));
    }
    rows.push(Line::from(""));
    rows.push(Line::from(dialog.error.clone()));
    f.render_widget(Clear, rect);
    f.render_widget(
        Paragraph::new(rows).block(
            modal_block(" Config ")
                .title_bottom(" Tab / Shift+Tab   Enter select   Ctrl+S save   Esc cancel "),
        ),
        rect,
    );
}

fn draw_context(f: &mut ratatui::Frame, view: &ContextView) {
    let rect = modal_rect(f.area(), 72, 14);
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
        let note = match g.0.mcp.probe_server(&name) {
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
    dance: Option<(usize, usize)>,
    scroll: usize,
    cwd: &str,
    model: &str,
    sandbox: &str,
    perm: PermissionMode,
    provider: &str,
    effort: &str,
    goal: Option<&GoalState>,
    agents: &str,
    session_title: &str,
    memory: &str,
    skills: Option<&SkillsBrowser>,
    dialog: Option<&ApprovalDialog>,
) -> DrawOut {
    let header_h = mascot::GLYPH_HEIGHT as u16 + if goal.is_some() { 2 } else { 0 };
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(header_h),
            Constraint::Min(6),
            Constraint::Length(composer.height(f.area().width)),
        ])
        .split(f.area());

    draw_header(
        f,
        chunks[0],
        tick,
        busy,
        cwd,
        model,
        sandbox,
        provider,
        effort,
        goal,
        agents,
        session_title,
        dance,
    );
    let clamp_scroll = draw_chat(
        f, chunks[1], log, streaming, busy, spin, tick, memory, scroll,
    );
    composer::render(
        f,
        chunks[2],
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
        },
    );
    match picker {
        Some(p) => composer::render_picker(f, chunks[1], p),
        None if composer.prefix_kind().is_some() => composer::render_prefix(f, chunks[1], composer),
        None => composer::render_slash(f, chunks[1], composer),
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
            let mark = if *installed { "[yüklü] " } else { "" };
            let installs = e
                .installs
                .map(|n| format!(" · {n} kurulum"))
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
        .unwrap_or_else(|| "önizleme".into());
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
        format!("[PANO METNİ #{} / {} · {}]", pv.card + 1, count, card.lang)
    } else {
        format!("[PANO METNİ · {}]", card.lang)
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
                " Ctrl+D: bu yapıştırmayı iptal · tab: sonraki kart · esc/ctrl+o kapat ",
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
        ("read-only", "Dosyalar salt okunur"),
        ("workspace-write", "Çalışma dizininde yazma"),
        ("danger-full-access", "Kısıtlama yok — dikkat"),
        (
            "docker-isolated",
            "Komutlar Docker konteynerinde çalışır (docker gerektirir)",
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
    let width = ((area.width as u32 * 3 / 5) as u16).max(44).min(area.width);
    let height = 7.min(area.height);
    let rect = Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    };
    f.render_widget(Clear, rect);
    let body = vec![
        Line::from(""),
        Line::from(Span::styled(
            format!("  {}", dialog.text),
            Style::default().fg(Palette::fg()),
        )),
        Line::from(""),
        Line::from(Span::styled(
            "  o allow once · a allow always · d deny (esc)",
            Style::default().fg(Palette::dim()),
        )),
    ];
    f.render_widget(
        Paragraph::new(body)
            .block(
                Block::default()
                    .title(" permission ")
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Palette::warn()))
                    .style(Style::default().bg(Palette::bg())),
            )
            .wrap(Wrap { trim: false }),
        rect,
    );
}

fn draw_header(
    f: &mut ratatui::Frame,
    area: Rect,
    tick: usize,
    busy: bool,
    cwd: &str,
    model: &str,
    sandbox: &str,
    provider: &str,
    effort: &str,
    goal: Option<&GoalState>,
    agents: &str,
    session_title: &str,
    dance: Option<(usize, usize)>,
) {
    let g = match dance {
        Some((kind, step)) => mascot::dance(kind, step / 3),
        None => mascot::glyph(None, busy),
    };
    let shown_model = display_model(model);
    let session = session_label(session_title);
    let hit = hit_line(tick, agents);
    let mut header = vec![
        Line::from({
            let mut s = g[0].spans.clone();
            s.push(Span::raw("  "));
            s.push(Span::styled(
                "Varynth",
                Style::default()
                    .fg(Palette::fg())
                    .add_modifier(Modifier::BOLD),
            ));
            s.push(Span::styled(
                "  v0.1.0",
                Style::default().fg(Palette::dim()),
            ));
            s
        }),
        Line::from({
            let mut s = g[1].spans.clone();
            s.push(Span::raw("  "));
            s.push(Span::styled(
                shown_model,
                Style::default().fg(Palette::accent_neon()),
            ));
            s.push(Span::styled(
                format!("  {effort}"),
                Style::default().fg(Palette::fg()),
            ));
            s.push(Span::styled("  ·  ", Style::default().fg(Palette::dim())));
            s.push(Span::styled(sandbox, Style::default().fg(Palette::fg())));
            s.push(Span::styled("  ·  ", Style::default().fg(Palette::dim())));
            s.push(Span::styled(provider, Style::default().fg(Palette::dim())));
            s
        }),
        Line::from({
            let mut s = g[2].spans.clone();
            s.push(Span::raw("  "));
            s.push(Span::styled(cwd, Style::default().fg(Palette::dim())));
            s
        }),
        Line::from(g[3].spans.clone()),
        Line::from(g[4].spans.clone()),
        Line::from({
            let mut s = g[5].spans.clone();
            s.push(Span::raw("  "));
            s.push(Span::styled(
                "session  ",
                Style::default().fg(Palette::dim()),
            ));
            s.push(Span::styled(
                session,
                Style::default()
                    .fg(Palette::fg())
                    .add_modifier(Modifier::BOLD),
            ));
            s
        }),
        Line::from({
            let mut s = g[6].spans.clone();
            s.push(Span::raw("  "));
            s.push(Span::styled(
                "hit  ",
                Style::default()
                    .fg(Palette::ok())
                    .add_modifier(Modifier::BOLD),
            ));
            s.push(Span::styled(hit, Style::default().fg(Palette::fg())));
            s
        }),
    ];
    if let Some(goal) = goal {
        let short: String = goal.condition.chars().take(88).collect();
        let badge = match goal.status {
            GoalStatus::Active => format!("goal {}/{}", goal.rounds, goal.max_rounds),
            GoalStatus::Met => "goal met".into(),
            GoalStatus::Stopped => "goal stopped".into(),
        };
        header.push(Line::from(""));
        header.push(Line::from(vec![
            Span::styled(
                format!("  {badge}  "),
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
        Paragraph::new(header).style(Style::default().bg(Palette::bg())),
        area,
    );
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
) -> usize {
    let mut lines: Vec<Line> = Vec::new();
    for msg in log {
        let (who, color) = match msg.role.as_str() {
            "user" => (">", Palette::warn()),
            "assistant" => ("●", Palette::info()),
            "tool" => ("▸", Palette::ok()),
            "error" => ("!", Color::Rgb(255, 92, 92)),
            _ => ("·", Palette::dim()),
        };
        lines.push(Line::from(""));
        let colored = msg.role == "user";
        for (i, part) in wrap_text(&msg.text, area.width.saturating_sub(6) as usize)
            .into_iter()
            .enumerate()
        {
            let body = if colored {
                paint_slash(&part, i == 0)
            } else {
                vec![Span::styled(part, Style::default().fg(Palette::fg()))]
            };
            if i == 0 {
                let mut spans = vec![Span::styled(
                    format!("  {who}  "),
                    Style::default().fg(color),
                )];
                spans.extend(body);
                lines.push(Line::from(spans));
            } else {
                let mut spans = vec![Span::raw("     ")];
                spans.extend(body);
                lines.push(Line::from(spans));
            }
        }
    }
    if let Some(text) = streaming {
        // Live reply in progress: an assistant-style bubble that the final
        // message replaces wholesale once the turn completes.
        lines.push(Line::from(""));
        for (i, part) in wrap_text(text, area.width.saturating_sub(6) as usize)
            .into_iter()
            .enumerate()
        {
            let mut spans = if i == 0 {
                vec![Span::styled("  ●  ", Style::default().fg(Palette::info()))]
            } else {
                vec![Span::raw("     ")]
            };
            spans.push(Span::styled(part, Style::default().fg(Palette::fg())));
            lines.push(Line::from(spans));
        }
        lines.push(Line::from(vec![
            Span::styled("  ◌  ", Style::default().fg(Palette::accent_neon())),
            Span::styled("streaming…", Style::default().fg(Palette::dim())),
        ]));
    }
    if busy {
        lines.push(Line::from(""));
        lines.push(Line::from(vec![
            Span::styled(
                format!("  {spin}  "),
                Style::default().fg(Palette::accent_neon()),
            ),
            Span::styled(
                "working on this machine…",
                Style::default().fg(Palette::dim()),
            ),
        ]));
    }
    if !busy {
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
    let skip = chat_skip(lines.len(), visible, scroll);
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
            .style(Style::default().bg(Color::Rgb(7, 8, 12))),
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
fn paint_slash(part: &str, first: bool) -> Vec<Span<'static>> {
    let plain = Span::styled(part.to_string(), Style::default().fg(Palette::fg()));
    if !first {
        return vec![plain];
    }
    let Some(rest) = part.strip_prefix('/') else {
        return vec![plain];
    };
    if rest.is_empty() || !rest.chars().next().is_some_and(|c| c.is_ascii_alphabetic()) {
        return vec![plain];
    }
    let split = rest
        .find(|c: char| c.is_whitespace() || c == ':')
        .map(|n| n + 1)
        .unwrap_or(part.len());
    let (name, body) = part.split_at(split);
    let mut spans = vec![Span::styled(
        name.to_string(),
        Style::default()
            .fg(Palette::accent_neon())
            .add_modifier(Modifier::BOLD),
    )];
    if !body.is_empty() {
        spans.push(Span::styled(
            body.to_string(),
            Style::default().fg(Palette::warn()),
        ));
    }
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
    use super::paint_slash;

    fn texts(part: &str) -> Vec<String> {
        paint_slash(part, true)
            .into_iter()
            .map(|span| span.content.into_owned())
            .collect()
    }

    #[test]
    fn slash_name_and_body_split() {
        assert_eq!(texts("/goal keep going"), vec!["/goal", " keep going"]);
        assert_eq!(texts("/skill:args"), vec!["/skill", ":args"]);
    }

    #[test]
    fn plain_text_stays_one_span() {
        assert_eq!(texts("just text").len(), 1);
        assert_eq!(texts("/").len(), 1);
        assert_eq!(paint_slash("/goal later", false).len(), 1);
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

fn hit_line(tick: usize, agents: &str) -> &'static str {
    let _ = agents;
    const HITS: [&str; 6] = [
        "/skills  ·  call a local skill by name",
        "Alt+V  ·  paste an image into this turn",
        "←  ·  previous sessions",
        "↓  ·  subagents and shell",
        "/goal  ·  lock a long run until done",
        "Shift+Tab  ·  cycle permission mode",
    ];
    HITS[(tick / 45) % HITS.len()]
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
        ("low", "Minimum reasoning, hızlı yanıtlar"),
        ("medium", "Dengeli düşünme"),
        ("high", "Derin analiz, genişletilmiş token bütçesi"),
        ("xhigh", "Geniş mimari planlama"),
        ("max", "Maksimum düşünme pencereleri"),
        ("ultra", "Multi-pass reasoning ve kendi kendini doğrulama"),
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
    const MODES: [(&str, &str); 3] = [
        ("acceptEdits", "Yazmaları otomatik onayla"),
        ("prompt", "Her yazma/kabuk için sor"),
        ("bypass", "Onay sorma — dikkat"),
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
    use super::{doctor_lines, git_diff};

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
    fn hits_rotate_and_stay_brandless() {
        let a = hit_line(0, "");
        let b = hit_line(45, "");
        assert_ne!(a, b);
        for tick in 0..300 {
            let h = hit_line(tick, "");
            let lower = h.to_ascii_lowercase();
            assert!(!lower.contains("openclaw"));
            assert!(!lower.contains("claude code"));
            assert!(!lower.contains("codex"));
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
    fn permission_picker_lists_the_three_modes() {
        let p = permission_picker("prompt");
        let ids: Vec<&str> = p.items.iter().map(|it| it.id.as_str()).collect();
        assert_eq!(ids, vec!["acceptEdits", "prompt", "bypass"]);
        assert!(p.items[0].detail.contains("Yazmaları otomatik onayla"));
        assert!(p.items[2].detail.contains("dikkat"));
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
        cfg.telegram_bot_token = Some("private-telegram".into());
        cfg.dashboard_token = Some("private-dashboard".into());
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
}
