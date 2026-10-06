//! The TUI prompt box: editable multi-line input with cursor, history and
//! image attachments, plus the pickers opened from it (← sessions, ↓ tasks)
//! and the prefix popups typed into the draft (`#` file pins, `@` mentions,
//! `$` skills).

use std::collections::VecDeque;

use fuzzy_matcher::skim::SkimMatcherV2;
use fuzzy_matcher::FuzzyMatcher;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph};

use crate::agents::PersonaSpec;
use crate::permissions::PermissionMode;
use crate::session::ImageAttachment;
use crate::skills_search::SkillEntry;
use crate::terminals::TerminalInfo;

use crate::theme::Palette;

/// Most text rows the box grows to before it scrolls.
const MAX_ROWS: usize = 8;
/// Border (2) + prompt marker (2).
const CHROME_COLS: u16 = 4;
/// Queued messages listed above the input before collapsing to "+N more".
const QUEUE_ROWS: usize = 3;
/// Messages that can wait for the running turn.
pub const MAX_QUEUE: usize = 10;
/// A clipboard paste longer than this many lines (or this many chars) becomes
/// a paste card instead of flooding the draft.
const PASTE_MAX_LINES: usize = 5;
const PASTE_MAX_CHARS: usize = 700;
/// Paste cards drawn under the draft before a "+N more paste" summary row.
const MAX_CARDS: usize = 3;
/// A `#`-pinned file rides on submit with at most this many chars of content.
pub const PIN_MAX_CHARS: usize = 8_000;
/// Workspace paths cached for the `#` popup at TUI startup.
pub const FILE_CACHE_CAP: usize = 5_000;
/// Rows kept from one `#` fuzzy pass (the popup scrolls through them).
const FILE_MATCH_CAP: usize = 50;
/// Rows a prefix popup shows before scrolling.
const PREFIX_ROWS: usize = 8;

/// A submitted message: text plus its images.
pub type Message = (String, Vec<ImageAttachment>);
/// Rows the slash menu shows before scrolling.
const SLASH_ROWS: usize = 8;

/// One entry of the `/` autocomplete menu.
#[derive(Clone, Debug, PartialEq)]
pub struct SlashItem {
    /// Command name without the slash.
    pub name: String,
    pub desc: String,
}

impl SlashItem {
    pub fn new(name: &str, desc: &str) -> Self {
        Self {
            name: name.into(),
            desc: desc.into(),
        }
    }
}

/// Built-in commands, in the order the menu lists them.
pub fn builtin_commands() -> Vec<SlashItem> {
    [
        (
            "goal",
            "keep working until a condition is met  ·  /goal clear",
        ),
        ("agents", "run a one-shot subagent  ·  /agents name: task"),
        (
            "collaborate",
            "pair-program with a live terminal  ·  /collaborate <term:N>",
        ),
        ("compact", "summarize earlier history in this session"),
        ("new", "save this session and start a fresh conversation"),
        ("clear", "clear this conversation and screen"),
        ("rename", "set this session title  ·  /rename <title>"),
        ("context", "inspect the context token budget"),
        ("config", "settings and secure API keys"),
        ("mcp", "server status, tests and tool schemas"),
        ("model", "fuzzy model finder  ·  /model <id>"),
        ("review", "model review of the working-copy diff"),
        (
            "send",
            "message another session  ·  /send <id|latest> <text>",
        ),
        ("inbox", "read messages from other sessions"),
        ("cost", "context size estimate for this session"),
        ("diff", "git changes in the working directory"),
        ("doctor", "check config, provider and proxy"),
        ("effort", "pick reasoning effort"),
        ("permission", "pick the permission mode"),
        ("resume", "reopen a previous session"),
        ("status", "runtime and session info"),
        ("models", "pick the model"),
        ("plugins", "GitHub and Google account links"),
        ("skills", "list local skills"),
        (
            "skills-search",
            "find and install skills from skills.sh  ·  /skills-search <query>",
        ),
        ("sandbox", "pick the sandbox mode"),
        ("help", "commands and shortcuts"),
        ("quit", "exit Varynth"),
    ]
    .into_iter()
    .map(|(n, d)| SlashItem::new(n, d))
    .collect()
}

/// Commands matching `query` (the text after `/`): prefix matches first,
/// then names containing it, each group in catalog order.
pub fn slash_matches<'a>(query: &str, all: &'a [SlashItem]) -> Vec<&'a SlashItem> {
    let q = query.to_lowercase();
    let mut prefix = Vec::new();
    let mut inner = Vec::new();
    for it in all {
        let name = it.name.to_lowercase();
        if name.starts_with(&q) {
            prefix.push(it);
        } else if name.contains(&q) {
            inner.push(it);
        }
    }
    prefix.extend(inner);
    prefix
}

/// Draft first, then every card as a fenced block the model receives
/// verbatim: clipboard pastes as `[Pasted text #N]`, `#`-pinned files as
/// `[#pin <rel>]`, each with a ```lang fence.
fn compose_pastes(draft: String, cards: &[PasteCard]) -> String {
    if cards.is_empty() {
        return draft;
    }
    let mut out = draft;
    let mut paste_no = 0;
    for card in cards {
        if !out.is_empty() {
            out.push_str("\n\n");
        }
        match &card.pin {
            Some(rel) => out.push_str(&format!(
                "[#pin {rel}]\n```{}\n{}\n```",
                card.lang, card.text
            )),
            None => {
                paste_no += 1;
                out.push_str(&format!(
                    "[Pasted text #{paste_no}]\n```{}\n{}\n```",
                    card.lang, card.text
                ));
            }
        }
    }
    out
}

/// Best-effort language label for a pasted snippet: shown on the badge card
/// and used as the fence tag when the paste is sent. Pure heuristics —
/// shebangs, ``` fences, keywords and line shapes.
pub fn sniff_language(text: &str) -> &'static str {
    let t = text.trim_start();

    // A leading ``` fence names the language outright.
    if let Some(rest) = t.strip_prefix("```") {
        let tag = rest
            .lines()
            .next()
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase();
        return match tag.as_str() {
            "rust" | "rs" => "Rust",
            "toml" => "TOML",
            "json" => "JSON",
            "python" | "py" => "Python",
            "ts" | "typescript" => "TypeScript",
            "js" | "javascript" | "jsx" | "tsx" => "JavaScript",
            "md" | "markdown" => "Markdown",
            "yaml" | "yml" => "YAML",
            "sql" => "SQL",
            "sh" | "bash" | "shell" | "zsh" | "console" => "Shell",
            _ => sniff_language(rest), // bare fence: inspect the body
        };
    }

    // Shebangs.
    let first = t.lines().next().unwrap_or("");
    if let Some(interpreter) = first.strip_prefix("#!") {
        let b = interpreter.to_ascii_lowercase();
        if b.contains("python") {
            return "Python";
        }
        if b.contains("node") {
            return "JavaScript";
        }
        return "Shell";
    }

    // JSON: brace-wrapped and actually parseable.
    let trimmed = t.trim();
    if (trimmed.starts_with('{') || trimmed.starts_with('['))
        && serde_json::from_str::<serde_json::Value>(trimmed).is_ok()
    {
        return "JSON";
    }

    let up = text.to_ascii_lowercase().replace('\n', " ");
    if (up.contains("select ") && up.contains(" from "))
        || up.contains("insert into")
        || up.contains("create table")
        || (up.contains("update ") && up.contains(" set "))
    {
        return "SQL";
    }

    if (text.contains("fn ") && (text.contains("let ") || text.contains("impl ")))
        || text.contains("use std::")
    {
        return "Rust";
    }

    let def_colon = t.lines().any(|l| {
        let s = l.trim_start();
        (s.starts_with("def ") || s.starts_with("class ")) && s.trim_end().ends_with(':')
    });
    if def_colon || (text.contains("import ") && text.contains("print(")) {
        return "Python";
    }

    let ts_annotated =
        (text.contains(": string") || text.contains(": number") || text.contains(": boolean"))
            && (text.contains("const ") || text.contains("let ") || text.contains("function "));
    if text.contains("interface ") || text.contains("enum ") || ts_annotated {
        return "TypeScript";
    }

    let js_assign = t.lines().any(|l| {
        let s = l.trim_start();
        (s.starts_with("const ") || s.starts_with("let ") || s.starts_with("var "))
            && s.contains('=')
    });
    if js_assign || text.contains("function ") || text.contains("console.log") {
        return "JavaScript";
    }

    let shell_pref = t.lines().any(|l| {
        let s = l.trim_start();
        s.starts_with("echo ")
            || s.starts_with("cd ")
            || s.starts_with("export ")
            || s.starts_with("sudo ")
            || s.starts_with("./")
    });
    if shell_pref {
        return "Shell";
    }

    let toml_section = t.lines().any(|l| {
        let s = l.trim();
        s.len() > 2 && s.starts_with('[') && s.ends_with(']')
    });
    let toml_kv = t.lines().any(|l| {
        let s = l.trim_start();
        if s.starts_with('#') || s.starts_with('[') {
            return false;
        }
        match s.split_once('=') {
            Some((k, v)) => {
                let v = v.trim();
                (!k.contains(' ')
                    && matches!(
                        v.chars().next(),
                        Some('"') | Some('\'') | Some('0'..='9') | Some('-')
                    ))
                    || v == "true"
                    || v == "false"
            }
            None => false,
        }
    });
    if toml_section || toml_kv {
        return "TOML";
    }

    let yaml_kv = t.lines().any(|l| match l.trim_start().split_once(':') {
        Some((k, v)) => !k.is_empty() && !k.contains(' ') && (v.is_empty() || v.starts_with(' ')),
        None => false,
    });
    let yaml_list = t.lines().any(|l| l.trim_start().starts_with("- "));
    if t.starts_with("---") || (yaml_kv && yaml_list) {
        return "YAML";
    }

    let heading = t
        .lines()
        .any(|l| l.starts_with("# ") || l.starts_with("## ") || l.starts_with("### "));
    if heading || text.contains("](") {
        return "Markdown";
    }

    "Metin"
}

/// `~2.4k` style token estimate (chars ÷ 4, the same heuristic as /cost).
fn fmt_tokens(tokens: usize) -> String {
    if tokens >= 1000 {
        format!("{:.1}k", tokens as f64 / 1000.0)
    } else {
        tokens.to_string()
    }
}

/// The four rows of one paste card: framed header, stats row, shortcut hint
/// and the bottom edge. `width` is the full card width including borders.
fn card_rows(index: usize, card: &PasteCard, width: usize) -> Vec<String> {
    let width = width.clamp(24, 52);
    let inner = width - 2;
    let title = match &card.pin {
        Some(rel) => format!("[#PİN {rel}]"),
        None => format!("[PANO METNİ EKLENDİ #{index}]"),
    };
    // Long file paths clip inside the card frame.
    let max_title = inner.saturating_sub(6);
    let title = if title.chars().count() > max_title {
        let keep = max_title.saturating_sub(1);
        format!("{}…", title.chars().take(keep).collect::<String>())
    } else {
        title
    };
    let head = format!("┌─ {title} ");
    let fill = width.saturating_sub(head.chars().count() + 1);
    let top = format!("{head}{}┐", "─".repeat(fill));
    let tokens = card.text.chars().count() / 4;
    let stats = format!(
        "📄 {} satır · ~{} token · {}",
        card.text.lines().count(),
        fmt_tokens(tokens),
        card.lang
    );
    let hint = "Ctrl+O önizle · Ctrl+D iptal";
    // The 📄 emoji paints two cells but is one char.
    let stats_cells = stats.chars().count() + 1;
    vec![
        top,
        card_body_row(&stats, stats_cells, inner),
        card_body_row(hint, hint.chars().count(), inner),
        format!("└{}┘", "─".repeat(inner)),
    ]
}

/// One card body row: `│ ` + text padded to the inner width + `│`. `cells`
/// is the text's display width; text that does not fit is clipped on the
/// right (narrow terminals only — cards clamp to at least 24 columns).
fn card_body_row(text: &str, mut cells: usize, inner: usize) -> String {
    let mut shown = text.to_string();
    while cells + 1 > inner && shown.chars().count() > 1 {
        shown.pop();
        cells -= 1;
    }
    let pad = inner.saturating_sub(cells + 1);
    format!("│ {shown}{}│", " ".repeat(pad))
}

/// One large clipboard paste or `#`-pinned file kept out of the draft: the
/// full text plus the [`sniff_language`]-style label shown on its card. A
/// pin is a card with a different header: it also carries the workspace
/// path it was pinned from.
#[derive(Clone, Debug, PartialEq)]
pub struct PasteCard {
    pub text: String,
    pub lang: &'static str,
    /// `Some(rel)` for a `#`-pinned workspace file, None for a paste.
    pub pin: Option<String>,
}

#[derive(Default, Clone, Debug)]
pub struct TextEditor {
    pub text: String,
    pub cursor: usize,
    pub scroll: usize,
    pub horizontal: usize,
}

impl TextEditor {
    pub fn new(text: String) -> Self {
        Self {
            text,
            ..Self::default()
        }
    }

    fn byte_at(&self, index: usize) -> usize {
        self.text
            .char_indices()
            .nth(index)
            .map_or(self.text.len(), |(at, _)| at)
    }

    pub fn position(&self) -> (usize, usize) {
        let before = &self.text[..self.byte_at(self.cursor)];
        (
            before.chars().filter(|&c| c == '\n').count(),
            before.rsplit('\n').next().unwrap_or("").chars().count(),
        )
    }

    pub fn insert(&mut self, text: &str) {
        let text = text.replace("\r\n", "\n").replace('\r', "\n");
        self.text.insert_str(self.byte_at(self.cursor), &text);
        self.cursor += text.chars().count();
    }

    pub fn move_row(&mut self, delta: isize) {
        let (row, column) = self.position();
        let lines: Vec<_> = self.text.split('\n').collect();
        let target = row
            .saturating_add_signed(delta)
            .min(lines.len().saturating_sub(1));
        self.cursor = lines
            .iter()
            .take(target)
            .map(|line| line.chars().count() + 1)
            .sum::<usize>()
            + column.min(lines[target].chars().count());
    }

    pub fn key(&mut self, key: crossterm::event::KeyEvent) -> bool {
        use crossterm::event::{KeyCode, KeyModifiers};
        match key.code {
            KeyCode::Char(c)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                self.insert(c.encode_utf8(&mut [0; 4]));
            }
            KeyCode::Enter => self.insert("\n"),
            KeyCode::Backspace if self.cursor > 0 => {
                self.cursor -= 1;
                self.text.remove(self.byte_at(self.cursor));
            }
            KeyCode::Delete if self.cursor < self.text.chars().count() => {
                self.text.remove(self.byte_at(self.cursor));
            }
            KeyCode::Left => self.cursor = self.cursor.saturating_sub(1),
            KeyCode::Right => self.cursor = (self.cursor + 1).min(self.text.chars().count()),
            KeyCode::Home => self.cursor -= self.position().1,
            KeyCode::End => {
                let after = &self.text[self.byte_at(self.cursor)..];
                self.cursor += after.split('\n').next().unwrap_or("").chars().count();
            }
            KeyCode::Up => self.move_row(-1),
            KeyCode::Down => self.move_row(1),
            KeyCode::PageUp => self.move_row(-12),
            KeyCode::PageDown => self.move_row(12),
            _ => return false,
        }
        true
    }

    pub fn reveal(&mut self, height: usize, width: usize) {
        let (row, column) = self.position();
        if row < self.scroll {
            self.scroll = row;
        }
        if row >= self.scroll + height.max(1) {
            self.scroll = row + 1 - height.max(1);
        }
        if column < self.horizontal {
            self.horizontal = column;
        }
        if column >= self.horizontal + width.max(1) {
            self.horizontal = column + 1 - width.max(1);
        }
    }
}

pub fn highlighted_line(text: &str) -> Vec<Span<'static>> {
    use crate::theme::Palette;
    let mut spans = Vec::new();
    let chars: Vec<_> = text.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let start = i;
        let color;
        if (chars[i] == '/' && chars.get(i + 1) == Some(&'/')) || chars[i] == '#' {
            spans.push(Span::styled(
                chars[i..].iter().collect::<String>(),
                Style::default().fg(Palette::dim()),
            ));
            break;
        } else if matches!(chars[i], '"' | '\'') {
            let quote = chars[i];
            i += 1;
            while i < chars.len() {
                if chars[i] == '\\' {
                    i = (i + 2).min(chars.len());
                } else if chars[i] == quote {
                    i += 1;
                    break;
                } else {
                    i += 1;
                }
            }
            color = Palette::ok();
        } else if chars[i].is_ascii_digit() {
            i += 1;
            while i < chars.len() && (chars[i].is_ascii_digit() || chars[i] == '.') {
                i += 1;
            }
            color = Palette::warn();
        } else if chars[i].is_alphabetic() || chars[i] == '_' {
            i += 1;
            while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '_') {
                i += 1;
            }
            let word: String = chars[start..i].iter().collect();
            color = if matches!(
                word.as_str(),
                "fn" | "let"
                    | "mut"
                    | "pub"
                    | "use"
                    | "impl"
                    | "struct"
                    | "enum"
                    | "return"
                    | "if"
                    | "else"
                    | "for"
                    | "while"
                    | "class"
                    | "def"
                    | "import"
                    | "const"
                    | "function"
                    | "async"
                    | "await"
                    | "true"
                    | "false"
                    | "null"
            ) {
                Palette::accent_neon()
            } else {
                Palette::fg()
            };
        } else {
            i += 1;
            color = Palette::fg();
        }
        spans.push(Span::styled(
            chars[start..i].iter().collect::<String>(),
            Style::default().fg(color),
        ));
    }
    spans
}

#[derive(Default)]
pub struct Composer {
    text: String,
    /// Cursor as a char index into `text`.
    cursor: usize,
    pub attachments: Vec<ImageAttachment>,
    /// Large pastes attached below the draft as cards, sent as fenced blocks.
    pastes: Vec<PasteCard>,
    history: Vec<String>,
    hist_pos: Option<usize>,
    draft: String,
    /// Messages sent while a turn was running, oldest first.
    queue: VecDeque<Message>,
    /// Commands offered by the `/` menu (built-ins + skills).
    pub commands: Vec<SlashItem>,
    slash_sel: usize,
    /// Esc closed the menu; it reopens when the text changes.
    slash_hidden: bool,
    /// Workspace-relative paths cached at startup for the `#` popup.
    pub workspace_files: Vec<String>,
    /// `@` popup rows: personas and live peer terminals; the TUI refreshes
    /// them while the popup is open.
    pub mentions: Vec<PickItem>,
    /// `$` popup rows: installed + runtime skills; refreshed on open.
    pub skills: Vec<PickItem>,
    prefix_sel: usize,
    /// Esc closed the popup; it reopens when the text changes.
    prefix_hidden: bool,
}

impl Composer {
    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn is_empty(&self) -> bool {
        self.text.is_empty() && self.attachments.is_empty() && self.pastes.is_empty()
    }

    /// A clipboard paste: text over [`PASTE_MAX_LINES`] lines or
    /// [`PASTE_MAX_CHARS`] chars becomes a card (returns true), anything
    /// shorter lands in the draft (returns false).
    pub fn paste(&mut self, text: &str) -> bool {
        let text = text.replace("\r\n", "\n").replace('\r', "\n");
        let text = text.trim_matches('\n');
        if text.lines().count() > PASTE_MAX_LINES || text.chars().count() > PASTE_MAX_CHARS {
            let lang = sniff_language(text);
            self.pastes.push(PasteCard {
                text: text.to_string(),
                lang,
                pin: None,
            });
            true
        } else {
            self.insert_str(text);
            false
        }
    }

    pub fn has_pastes(&self) -> bool {
        !self.pastes.is_empty()
    }

    pub fn pastes(&self) -> &[PasteCard] {
        &self.pastes
    }

    /// Ctrl+D: drop the newest card. Returns true when one was removed.
    pub fn drop_last_paste(&mut self) -> bool {
        self.pastes.pop().is_some()
    }

    pub fn remove_paste(&mut self, index: usize) -> bool {
        if index >= self.pastes.len() {
            return false;
        }
        self.pastes.remove(index);
        true
    }

    pub fn replace_paste(&mut self, index: usize, text: String) -> bool {
        let Some(card) = self.pastes.get_mut(index) else {
            return false;
        };
        card.lang = card
            .pin
            .as_deref()
            .map_or_else(|| sniff_language(&text), |path| lang_for_path(path, &text));
        card.text = if card.pin.is_some() {
            text.chars().take(PIN_MAX_CHARS).collect()
        } else {
            text
        };
        true
    }

    pub fn clear(&mut self) {
        self.text.clear();
        self.cursor = 0;
        self.attachments.clear();
        self.pastes.clear();
        self.queue.clear();
        self.history.clear();
        self.hist_pos = None;
        self.draft.clear();
        self.popups_reset();
    }

    pub fn insert_str(&mut self, s: &str) {
        let s = s.replace("\r\n", "\n").replace('\r', "\n");
        let at = self.byte_at(self.cursor);
        self.text.insert_str(at, &s);
        self.cursor += s.chars().count();
        self.hist_pos = None;
        self.popups_reset();
    }

    pub fn insert_char(&mut self, c: char) {
        self.insert_str(c.encode_utf8(&mut [0; 4]));
    }

    /// Deletes the char before the cursor; with no text left, drops the
    /// last attachment instead.
    pub fn backspace(&mut self) {
        if self.cursor > 0 {
            self.cursor -= 1;
            let at = self.byte_at(self.cursor);
            self.text.remove(at);
            self.popups_reset();
        } else if self.text.is_empty() {
            self.attachments.pop();
        }
    }

    pub fn delete(&mut self) {
        if self.cursor < self.text.chars().count() {
            let at = self.byte_at(self.cursor);
            self.text.remove(at);
        }
    }

    pub fn left(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    pub fn right(&mut self) {
        self.cursor = (self.cursor + 1).min(self.text.chars().count());
    }

    pub fn home(&mut self) {
        self.cursor = 0;
    }

    pub fn end(&mut self) {
        self.cursor = self.text.chars().count();
    }

    /// ↑: recall an older prompt. Returns false when there is nothing to recall.
    pub fn history_prev(&mut self) -> bool {
        let next = match self.hist_pos {
            None if self.history.is_empty() => return false,
            None => {
                self.draft = self.text.clone();
                self.history.len() - 1
            }
            Some(0) => return true,
            Some(i) => i - 1,
        };
        self.hist_pos = Some(next);
        self.set_text(self.history[next].clone());
        true
    }

    /// ↓: move back toward the draft. Returns false when not browsing history.
    pub fn history_next(&mut self) -> bool {
        let Some(i) = self.hist_pos else {
            return false;
        };
        if i + 1 < self.history.len() {
            self.hist_pos = Some(i + 1);
            self.set_text(self.history[i + 1].clone());
        } else {
            self.hist_pos = None;
            let draft = std::mem::take(&mut self.draft);
            self.set_text(draft);
        }
        true
    }

    /// Clears the box, returning the trimmed text and attachments; the text
    /// is remembered for ↑. Every paste card is appended to the text as a
    /// fenced block (draft first), then the cards clear.
    pub fn submit(&mut self) -> (String, Vec<ImageAttachment>) {
        let line = self.text.trim().to_string();
        if !line.is_empty() && self.history.last() != Some(&line) {
            self.history.push(line.clone());
        }
        self.text.clear();
        self.cursor = 0;
        self.hist_pos = None;
        self.draft.clear();
        self.popups_reset();
        let cards = std::mem::take(&mut self.pastes);
        let line = compose_pastes(line, &cards);
        (line, std::mem::take(&mut self.attachments))
    }

    /// Submits the box into the queue. Returns the queue length, or None
    /// when the box is empty or the queue is full.
    pub fn enqueue(&mut self) -> Option<usize> {
        if self.is_empty() || self.queue.len() >= MAX_QUEUE {
            return None;
        }
        let msg = self.submit();
        self.queue.push_back(msg);
        Some(self.queue.len())
    }

    /// Puts a message at the head of the queue so it is sent next.
    pub fn enqueue_front(&mut self, msg: Message) {
        self.queue.push_front(msg);
    }

    pub fn next_queued(&mut self) -> Option<Message> {
        self.queue.pop_front()
    }

    pub fn queued(&self) -> usize {
        self.queue.len()
    }

    fn queue_rows(&self) -> usize {
        match self.queue.len() {
            n if n > QUEUE_ROWS => QUEUE_ROWS + 1,
            n => n,
        }
    }

    fn set_text(&mut self, t: String) {
        self.cursor = t.chars().count();
        self.text = t;
        self.popups_reset();
    }

    /// Any draft edit re-opens every popup and rewinds their selection.
    fn popups_reset(&mut self) {
        self.slash_sel = 0;
        self.slash_hidden = false;
        self.prefix_sel = 0;
        self.prefix_hidden = false;
    }

    /// The command being typed: the text after a leading `/`, while it is a
    /// single word with the cursor at its end.
    pub fn slash_query(&self) -> Option<&str> {
        let q = self.text.strip_prefix('/')?;
        let at_end = self.cursor == self.text.chars().count();
        (at_end && !q.contains(char::is_whitespace)).then_some(q)
    }

    /// Matches to show, or empty when the menu is closed.
    pub fn slash_items(&self) -> Vec<&SlashItem> {
        if self.slash_hidden {
            return Vec::new();
        }
        match self.slash_query() {
            Some(q) => slash_matches(q, &self.commands),
            None => Vec::new(),
        }
    }

    /// Handles a key while the menu is open. Returns true when it consumed
    /// the key. Enter on a fully typed command falls through and submits.
    pub fn slash_key(&mut self, code: crossterm::event::KeyCode) -> bool {
        use crossterm::event::KeyCode;
        let items = self.slash_items();
        if items.is_empty() {
            return false;
        }
        let n = items.len();
        let sel = self.slash_sel.min(n - 1);
        let picked = items[sel].name.clone();
        let typed = self.slash_query().unwrap_or("").to_string();
        match code {
            KeyCode::Up => self.slash_sel = (sel + n - 1) % n,
            KeyCode::Down => self.slash_sel = (sel + 1) % n,
            KeyCode::Esc => self.slash_hidden = true,
            KeyCode::Tab => self.set_text(format!("/{picked} ")),
            KeyCode::Enter if typed != picked => self.set_text(format!("/{picked} ")),
            _ => return false,
        }
        true
    }

    /// Which prefix popup the token before the cursor drives, honoring Esc:
    /// `#` workspace files, `@` mentions, `$` skills.
    pub fn prefix_kind(&self) -> Option<PrefixKind> {
        if self.prefix_hidden {
            return None;
        }
        self.token_query().map(|(kind, _)| kind)
    }

    /// The text typed after the popup's trigger char, while it is open.
    pub fn prefix_query(&self) -> Option<&str> {
        self.prefix_kind()
            .and_then(|_| self.token_query())
            .map(|(_, q)| q)
    }

    /// Rows the open prefix popup shows (fuzzy-filtered by the token typed
    /// after the trigger), or empty when no popup is open. Only one popup
    /// can ever be open: the kind comes from the single token the cursor
    /// sits at the end of.
    pub fn prefix_items(&self) -> Vec<PickItem> {
        let Some((kind, q)) = self.token_query() else {
            return Vec::new();
        };
        if self.prefix_hidden {
            return Vec::new();
        }
        match kind {
            PrefixKind::Files => filter_files(q, &self.workspace_files),
            PrefixKind::Mentions => fuzzy_rows(q, &self.mentions),
            PrefixKind::Skills => fuzzy_rows(q, &self.skills),
        }
    }

    /// Whether a prefix popup is showing rows right now (the caller keeps
    /// its cursor out of the queue-drain shortcut while this is true).
    pub fn prefix_open(&self) -> bool {
        !self.prefix_items().is_empty()
    }

    /// The row Enter would act on, or None when the popup is closed or has
    /// no matches (Enter then falls through and submits).
    pub fn prefix_selected(&self) -> Option<PickItem> {
        let items = self.prefix_items();
        let sel = self.prefix_sel.min(items.len().checked_sub(1)?);
        items.get(sel).cloned()
    }

    /// Handles a key while a prefix popup is open. Returns true when it
    /// consumed the key. Enter is left to the caller, which acts on
    /// [`Composer::prefix_selected`] (pin, insert or nothing).
    pub fn prefix_key(&mut self, code: crossterm::event::KeyCode) -> bool {
        use crossterm::event::KeyCode;
        // Esc closes the popup even before anything matches, so it never
        // doubles as the clear-the-draft Esc.
        if code == KeyCode::Esc && self.prefix_kind().is_some() {
            self.prefix_hidden = true;
            return true;
        }
        let items = self.prefix_items();
        if items.is_empty() {
            return false;
        }
        let n = items.len();
        let sel = self.prefix_sel.min(n - 1);
        let picked = items[sel].clone();
        match code {
            KeyCode::Up => self.prefix_sel = (sel + n - 1) % n,
            KeyCode::Down => self.prefix_sel = (sel + 1) % n,
            KeyCode::Tab if self.prefix_kind() == Some(PrefixKind::Files) => {
                // Swap the query for the full path and keep filtering.
                self.replace_trigger_token(&format!("#{}", picked.id));
            }
            KeyCode::Tab => match self.prefix_kind() {
                Some(PrefixKind::Mentions) if picked.id.starts_with("session:") => {
                    self.accept_mention(&format!("@{}", picked.id))
                }
                Some(PrefixKind::Mentions) => self.accept_mention(&picked.label),
                _ => self.accept_skill(&picked.id),
            },
            _ => return false,
        }
        true
    }

    /// Enter on the `#` popup: the token becomes a `[file: <rel>]` marker
    /// and the content rides on submit as a fenced `[#pin <rel>]` block.
    /// The caller reads the file; the composer caps and stores it.
    pub fn accept_file_pin(&mut self, rel: &str, content: &str) {
        if self.token_query().is_none() {
            return;
        }
        let text: String = content.chars().take(PIN_MAX_CHARS).collect();
        let lang = lang_for_path(rel, content);
        self.replace_trigger_token(&format!("[file: {rel}] "));
        self.pastes.push(PasteCard {
            text,
            lang,
            pin: Some(rel.to_string()),
        });
    }

    /// Enter on the `@` popup: insert `@name ` into the draft. On submit
    /// the names in the line are extracted and dispatched by the caller.
    pub fn accept_mention(&mut self, label: &str) {
        if self.token_query().is_none() {
            return;
        }
        self.replace_trigger_token(&format!("{label} "));
    }

    /// Enter on the `$` popup: swap the token for the skill's `/name `.
    pub fn accept_skill(&mut self, name: &str) {
        if self.token_query().is_none() {
            return;
        }
        self.replace_trigger_token(&format!("/{name} "));
    }

    /// The token the cursor sits at the end of, and what its leading
    /// trigger char would open. The trigger must start the token (preceded
    /// by whitespace or nothing) and the cursor must be at the very end —
    /// the trigger char stays in the draft while the popup filters.
    fn token_query(&self) -> Option<(PrefixKind, &str)> {
        if self.cursor != self.text.chars().count() {
            return None; // only the token being typed at the end pops up
        }
        let up_to = self.byte_at(self.cursor);
        let start = self.text[..up_to]
            .char_indices()
            .rev()
            .find(|(_, c)| c.is_whitespace())
            .map_or(0, |(b, c)| b + c.len_utf8());
        let token = &self.text[start..up_to];
        let kind = match token.chars().next()? {
            '#' => PrefixKind::Files,
            '@' => PrefixKind::Mentions,
            '$' => PrefixKind::Skills,
            _ => return None,
        };
        Some((kind, &token[1..]))
    }

    /// Swaps the trigger token before the cursor (`#query`, `@query`,
    /// `$query`) for `into` and parks the cursor after it. No-op when no
    /// trigger token is under the cursor.
    fn replace_trigger_token(&mut self, into: &str) {
        if self.token_query().is_none() {
            return;
        }
        let up_to = self.byte_at(self.cursor);
        let start = self.text[..up_to]
            .char_indices()
            .rev()
            .find(|(_, c)| c.is_whitespace())
            .map_or(0, |(b, c)| b + c.len_utf8());
        let tail = self.text.split_off(up_to);
        self.text.truncate(start);
        self.text.push_str(into);
        self.cursor = self.text.chars().count();
        self.text.push_str(&tail);
        self.popups_reset();
    }

    fn byte_at(&self, char_idx: usize) -> usize {
        self.text
            .char_indices()
            .nth(char_idx)
            .map_or(self.text.len(), |(b, _)| b)
    }

    /// Rows the whole composer (box + cards + footer) needs at this width.
    pub fn height(&self, width: u16) -> u16 {
        let (lines, _) = wrap(&self.text, self.cursor, text_cols(width));
        let attach = u16::from(!self.attachments.is_empty());
        2 + self.queue_rows() as u16
            + attach
            + lines.len().min(MAX_ROWS) as u16
            + self.paste_rows() as u16
            + 1
    }

    /// Rows the paste cards need under the draft: four per card, at most
    /// [`MAX_CARDS`] cards drawn plus a "+N more paste" summary row.
    fn paste_rows(&self) -> usize {
        match self.pastes.len() {
            0 => 0,
            n if n > MAX_CARDS => MAX_CARDS * 4 + 1,
            n => n * 4,
        }
    }
}

fn text_cols(width: u16) -> usize {
    width.saturating_sub(CHROME_COLS).max(1) as usize
}

/// Hard-wraps `text` at `cols` (and at newlines). Returns the rows and the
/// cursor's (row, col).
pub fn wrap(text: &str, cursor: usize, cols: usize) -> (Vec<String>, (usize, usize)) {
    let cols = cols.max(1);
    let mut lines = vec![String::new()];
    let mut col = 0;
    let mut pos = None;
    for (i, ch) in text.chars().enumerate() {
        if ch != '\n' && col == cols {
            lines.push(String::new());
            col = 0;
        }
        if i == cursor {
            pos = Some((lines.len() - 1, col));
        }
        if ch == '\n' {
            lines.push(String::new());
            col = 0;
        } else {
            lines.last_mut().unwrap().push(ch);
            col += 1;
        }
    }
    let pos = pos.unwrap_or_else(|| {
        if col == cols {
            lines.push(String::new());
            col = 0;
        }
        (lines.len() - 1, col)
    });
    (lines, pos)
}

pub struct Status<'a> {
    pub busy: bool,
    pub spin: &'a str,
    pub perm: PermissionMode,
    pub effort: &'a str,
    pub goal: bool,
    /// How long the active goal has been running; shown on the box's top edge.
    pub goal_for: Option<std::time::Duration>,
    pub notice: Option<&'a str>,
    /// Whether the terminal cursor should be placed in the box.
    pub focused: bool,
}

/// `42s`, `1m 16s`, `2h 05m`.
pub fn short_elapsed(d: std::time::Duration) -> String {
    let s = d.as_secs();
    match s {
        0..=59 => format!("{s}s"),
        60..=3599 => format!("{}m {}s", s / 60, s % 60),
        _ => format!("{}h {:02}m", s / 3600, s % 3600 / 60),
    }
}

pub fn render(f: &mut ratatui::Frame, area: Rect, c: &Composer, st: &Status) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(3), Constraint::Length(1)])
        .split(area);

    let border = if st.busy {
        Color::Rgb(36, 40, 48)
    } else {
        Palette::dim()
    };
    let mut block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(border));
    if let Some(d) = st.goal_for {
        block = block.title_top(
            Line::from(Span::styled(
                format!(" ◎ /goal active ({}) ", short_elapsed(d)),
                Style::default().fg(Color::Rgb(167, 160, 245)),
            ))
            .right_aligned(),
        );
    }
    let inner = block.inner(chunks[0]);
    f.render_widget(block, chunks[0]);

    let mut rows: Vec<Line> = Vec::new();
    let qcols = (inner.width as usize).saturating_sub(12).max(8);
    for (i, (text, imgs)) in c.queue.iter().take(QUEUE_ROWS).enumerate() {
        let mut one: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
        if one.is_empty() {
            one = "(image)".into();
        }
        if one.chars().count() > qcols {
            one = one.chars().take(qcols - 1).collect::<String>() + "…";
        }
        let tag = if i == 0 { "next  " } else { "      " };
        let mut spans = vec![
            Span::styled("↳ ", Style::default().fg(Palette::dim())),
            Span::styled(tag, Style::default().fg(Palette::ok())),
            Span::styled(
                one,
                Style::default()
                    .fg(Palette::dim())
                    .add_modifier(Modifier::ITALIC),
            ),
        ];
        if !imgs.is_empty() {
            spans.push(Span::styled(
                format!("  [{} image]", imgs.len()),
                Style::default().fg(Palette::dim()),
            ));
        }
        rows.push(Line::from(spans));
    }
    if c.queue.len() > QUEUE_ROWS {
        rows.push(Line::from(Span::styled(
            format!("        +{} more queued", c.queue.len() - QUEUE_ROWS),
            Style::default().fg(Palette::dim()),
        )));
    }
    if !c.attachments.is_empty() {
        let mut spans = Vec::new();
        for i in 1..=c.attachments.len() {
            spans.push(Span::styled(
                format!(" Image #{i} "),
                Style::default()
                    .fg(Palette::bg())
                    .bg(Palette::accent_neon())
                    .add_modifier(Modifier::BOLD),
            ));
            spans.push(Span::raw(" "));
        }
        spans.push(Span::styled(
            " backspace removes",
            Style::default().fg(Palette::dim()),
        ));
        rows.push(Line::from(spans));
    }
    let text_top = rows.len() as u16;

    let (lines, (crow, ccol)) = wrap(&c.text, c.cursor, text_cols(area.width));
    let skip = (crow + 1).saturating_sub(MAX_ROWS);
    let marker = if st.busy {
        Span::styled(
            format!("{} ", st.spin),
            Style::default().fg(Palette::accent_neon()),
        )
    } else {
        Span::styled(
            "› ",
            Style::default()
                .fg(Palette::warn())
                .add_modifier(Modifier::BOLD),
        )
    };
    if c.text.is_empty() {
        let hint = if st.busy {
            "working…  enter queues  ·  ctrl+enter sends now  ·  esc stops"
        } else {
            "Ask Varynth anything  ·  / commands  ·  alt+v paste image"
        };
        rows.push(Line::from(vec![
            marker,
            Span::styled(hint, Style::default().fg(Palette::dim())),
        ]));
    } else {
        for (i, l) in lines.iter().enumerate().skip(skip).take(MAX_ROWS) {
            let lead = if i == 0 {
                marker.clone()
            } else {
                Span::raw("  ")
            };
            rows.push(Line::from(vec![
                lead,
                Span::styled(l.clone(), Style::default().fg(Palette::fg())),
            ]));
        }
    }
    // Paste cards stack under the draft, newest last.
    let card_w = (inner.width as usize).clamp(24, 52);
    for (n, card) in c.pastes.iter().enumerate().take(MAX_CARDS) {
        for (i, row) in card_rows(n + 1, card, card_w).into_iter().enumerate() {
            let color = if i == 1 {
                Palette::fg()
            } else {
                Palette::dim()
            };
            rows.push(Line::from(Span::styled(row, Style::default().fg(color))));
        }
    }
    if c.pastes.len() > MAX_CARDS {
        rows.push(Line::from(Span::styled(
            format!(
                "  +{} more paste  ·  ctrl+o preview",
                c.pastes.len() - MAX_CARDS
            ),
            Style::default().fg(Palette::dim()),
        )));
    }
    f.render_widget(Paragraph::new(rows), inner);

    if st.focused {
        let y = inner.y + text_top + (crow - skip) as u16;
        let x = inner.x + 2 + ccol as u16;
        if y < inner.bottom() && x < inner.right() {
            f.set_cursor_position(Position::new(x, y));
        }
    }

    let (perm_label, perm_color) = match st.perm {
        PermissionMode::Bypass => ("bypass permissions on", Palette::warn()),
        PermissionMode::AcceptEdits => ("accept edits on", Palette::ok()),
        PermissionMode::Prompt => ("prompt on writes", Palette::accent_neon()),
    };
    let left = vec![
        Span::styled(" ▸ ", Style::default().fg(perm_color)),
        Span::styled(perm_label, Style::default().fg(perm_color)),
        Span::styled(" (shift+tab)  ·  ", Style::default().fg(Palette::dim())),
        Span::styled(
            format!("{} ", st.effort),
            Style::default().fg(Palette::fg()),
        ),
        Span::styled("/effort", Style::default().fg(Palette::dim())),
    ];
    let right = match st.notice {
        Some(n) => Span::styled(format!("{n} "), Style::default().fg(Palette::ok())),
        None if st.busy => Span::styled(
            "esc stop  ctrl+enter send now  enter queue ",
            Style::default().fg(Palette::dim()),
        ),
        None => Span::styled(
            "← sessions  ↓ tasks  ↑ history  alt+v image ",
            Style::default().fg(Palette::dim()),
        ),
    };
    let left_w: usize = left.iter().map(|s| s.content.chars().count()).sum();
    f.render_widget(Paragraph::new(Line::from(left)), chunks[1]);
    if left_w + right.content.chars().count() + 2 <= chunks[1].width as usize {
        f.render_widget(
            Paragraph::new(Line::from(right)).alignment(Alignment::Right),
            chunks[1],
        );
    }
}

pub fn render_slash(f: &mut ratatui::Frame, area: Rect, c: &Composer) {
    let items = c.slash_items();
    if items.is_empty() || area.height < 3 {
        return;
    }
    let sel = c.slash_sel.min(items.len() - 1);
    let visible = SLASH_ROWS.min(items.len()).min(area.height as usize - 2);
    let skip = (sel + 1).saturating_sub(visible);
    let h = visible as u16 + 2;
    let rect = Rect {
        x: area.x + 1,
        y: area.bottom().saturating_sub(h),
        width: area.width.saturating_sub(2),
        height: h,
    };
    let name_w = items
        .iter()
        .map(|it| it.name.chars().count())
        .max()
        .unwrap_or(0)
        + 4;
    let more = if items.len() > visible {
        format!(" {}/{} ", sel + 1, items.len())
    } else {
        String::new()
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(Color::Rgb(36, 40, 48)))
        .title_bottom(
            Line::from(vec![
                Span::styled(more, Style::default().fg(Palette::dim())),
                Span::styled(
                    " ↑↓ · tab complete · esc ",
                    Style::default().fg(Palette::dim()),
                ),
            ])
            .right_aligned(),
        )
        .style(Style::default().bg(Palette::bg()));
    let inner = block.inner(rect);
    f.render_widget(Clear, rect);
    f.render_widget(block, rect);
    let desc_w = (inner.width as usize).saturating_sub(name_w + 2);
    let rows: Vec<Line> = items
        .iter()
        .enumerate()
        .skip(skip)
        .take(visible)
        .map(|(i, it)| {
            let on = i == sel;
            let mut desc: String = it.desc.chars().take(desc_w).collect();
            if it.desc.chars().count() > desc_w && desc_w > 1 {
                desc.pop();
                desc.push('…');
            }
            let name = format!(" /{:<w$}", it.name, w = name_w - 2);
            if on {
                let hl = Style::default()
                    .fg(Palette::bg())
                    .bg(Palette::ok())
                    .add_modifier(Modifier::BOLD);
                Line::from(vec![
                    Span::styled(name, hl),
                    Span::styled(
                        format!("{desc:<desc_w$} "),
                        Style::default().fg(Palette::bg()).bg(Palette::ok()),
                    ),
                ])
            } else {
                Line::from(vec![
                    Span::styled(name, Style::default().fg(Palette::fg())),
                    Span::styled(desc, Style::default().fg(Palette::dim())),
                ])
            }
        })
        .collect();
    f.render_widget(Paragraph::new(rows), inner);
}

/// Which prefix popup the token before the cursor drives: `#` workspace
/// files, `@` mentions, `$` skills.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PrefixKind {
    Files,
    Mentions,
    Skills,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PickItem {
    pub id: String,
    pub label: String,
    pub detail: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PickerKind {
    Sessions,
    Tasks,
    /// Background shells and subagents. Read-only.
    Activity,
    /// Provider models; enter switches the session model. Fuzzy-filtered by
    /// the search row.
    Models,
    /// Reasoning effort levels; enter sets the level.
    Effort,
    /// Approval behavior (acceptEdits / prompt / bypass); enter applies and
    /// persists it.
    Permissions,
    /// Sandbox mode (read-only / workspace-write / danger-full-access /
    /// docker-isolated); enter applies and persists it.
    Sandbox,
    /// GitHub and Google links. Enter shows status, it never starts login.
    Plugins,
}

/// A list popup anchored to the bottom of the chat area.
pub struct Picker {
    pub kind: PickerKind,
    pub items: Vec<PickItem>,
    pub sel: usize,
    /// Fuzzy query typed into the search row (the model finder). Empty shows
    /// the unfiltered list.
    pub query: String,
}

impl Picker {
    /// Indices of the items matching the query; every item when it is empty.
    pub fn visible(&self) -> Vec<usize> {
        if self.query.is_empty() {
            return (0..self.items.len()).collect();
        }
        fuzzy_indices(&self.query, &self.items)
    }

    pub fn up(&mut self) {
        self.sel = self.sel.saturating_sub(1);
    }

    pub fn down(&mut self) {
        if self.sel + 1 < self.visible().len() {
            self.sel += 1;
        }
    }

    /// Typing into the search row resets the selection to the best match.
    pub fn type_char(&mut self, c: char) {
        self.query.push(c);
        self.sel = 0;
    }

    pub fn backspace(&mut self) {
        self.query.pop();
        self.sel = 0;
    }

    pub fn selected(&self) -> Option<&PickItem> {
        let vis = self.visible();
        vis.get(self.sel).and_then(|&i| self.items.get(i))
    }
}

/// Fuzzy-match `query` against item labels, best score first (ties keep the
/// catalog order). Non-matching items are dropped; an empty query keeps every
/// item in order.
pub fn fuzzy_indices(query: &str, items: &[PickItem]) -> Vec<usize> {
    let q = query.trim();
    if q.is_empty() {
        return (0..items.len()).collect();
    }
    let matcher = SkimMatcherV2::default();
    let mut scored: Vec<(i64, usize)> = items
        .iter()
        .enumerate()
        .filter_map(|(i, it)| matcher.fuzzy_match(&it.label, q).map(|s| (s, i)))
        .collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    scored.into_iter().map(|(_, i)| i).collect()
}

/// The `@`/`$` rows matching a query, best first.
fn fuzzy_rows(query: &str, rows: &[PickItem]) -> Vec<PickItem> {
    fuzzy_indices(query, rows)
        .into_iter()
        .map(|i| rows[i].clone())
        .collect()
}

/// Fuzzy-match a `#` query against the cached workspace paths, best first,
/// capped at [`FILE_MATCH_CAP`] rows. An empty query keeps the catalog
/// order (the scan's sorted order).
pub fn filter_files(query: &str, files: &[String]) -> Vec<PickItem> {
    let items: Vec<PickItem> = files
        .iter()
        .map(|f| PickItem {
            id: f.clone(),
            label: f.clone(),
            detail: String::new(),
        })
        .collect();
    fuzzy_indices(query, &items)
        .into_iter()
        .take(FILE_MATCH_CAP)
        .map(|i| items[i].clone())
        .collect()
}

/// Cached workspace file list for the `#` popup: relative paths (forward
/// slashes) under `root`, capped at [`FILE_CACHE_CAP`], skipping `target`,
/// `.git` and `node_modules` plus any path with whitespace (the trigger
/// token model can't address those).
pub fn scan_workspace_files(root: &std::path::Path) -> Vec<String> {
    let skip = |name: &str| {
        matches!(
            name.to_ascii_lowercase().as_str(),
            "target" | ".git" | "node_modules"
        )
    };
    let mut out: Vec<String> = walkdir::WalkDir::new(root)
        .into_iter()
        .filter_entry(|e| e.depth() == 0 || !skip(e.file_name().to_string_lossy().as_ref()))
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_file())
        .filter_map(|e| {
            let rel = e.path().strip_prefix(root).ok()?;
            let s = rel.to_string_lossy().replace('\\', "/");
            (!s.is_empty() && !s.contains(char::is_whitespace)).then_some(s)
        })
        .take(FILE_CACHE_CAP)
        .collect();
    out.sort();
    out
}

/// Validate every cached candidate before exposing it to the `#` picker.
/// Workspace pins never escape the workspace, even in full-access mode.
pub fn scan_jail_files(jail: &crate::sandbox::Jail) -> Vec<String> {
    let root = match jail.cwd.canonicalize() {
        Ok(root) => root,
        Err(_) => return Vec::new(),
    };
    let mut files = scan_workspace_files(&jail.cwd);
    files.retain(|rel| {
        jail.resolve(rel)
            .ok()
            .and_then(|path| path.canonicalize().ok())
            .is_some_and(|path| path.starts_with(&root))
    });
    let folders: std::collections::BTreeSet<String> = files
        .iter()
        .flat_map(|file| {
            let mut parts = Vec::new();
            let mut path = std::path::Path::new(file).parent();
            while let Some(parent) = path {
                if parent.as_os_str().is_empty() {
                    break;
                }
                let rel = format!("{}/", parent.to_string_lossy().replace('\\', "/"));
                if jail
                    .resolve(&rel)
                    .ok()
                    .and_then(|p| p.canonicalize().ok())
                    .is_some_and(|p| p.starts_with(&root))
                {
                    parts.push(rel);
                }
                path = parent.parent();
            }
            parts
        })
        .collect();
    files.extend(folders);
    files.sort();
    files.truncate(FILE_CACHE_CAP);
    files
}

pub fn read_jail_pin(jail: &crate::sandbox::Jail, rel: &str) -> anyhow::Result<String> {
    use std::io::Read;
    let root = jail.cwd.canonicalize()?;
    let path = jail.resolve(rel)?;
    let real = path.canonicalize()?;
    anyhow::ensure!(real.starts_with(root), "pin is outside the workspace");
    if real.is_dir() {
        let files = scan_jail_files(jail);
        let prefix = format!("{}/", rel.trim_end_matches(['/', '\\']).replace('\\', "/"));
        return Ok(files
            .into_iter()
            .filter(|file| file.starts_with(&prefix) && !file.ends_with('/'))
            .collect::<Vec<_>>()
            .join("\n")
            .chars()
            .take(PIN_MAX_CHARS)
            .collect());
    }
    let mut bytes = Vec::new();
    std::fs::File::open(&real)?
        .take((PIN_MAX_CHARS * 4) as u64)
        .read_to_end(&mut bytes)?;
    let text = String::from_utf8_lossy(&bytes);
    anyhow::ensure!(
        !text.contains('\0'),
        "binary files cannot be pinned as text"
    );
    Ok(text.chars().take(PIN_MAX_CHARS).collect())
}

/// `@` popup rows: personas first, then live peer terminals numbered the
/// way `Registry::resolve_handle` resolves them. `self_id` marks the
/// caller's own terminal row.
pub fn mention_rows(
    personas: &[PersonaSpec],
    terminals: &[TerminalInfo],
    self_id: Option<&str>,
) -> Vec<PickItem> {
    let mut rows: Vec<PickItem> = personas.iter().map(persona_row).collect();
    rows.extend(
        terminals
            .iter()
            .enumerate()
            .map(|(i, t)| terminal_row(i, t, self_id)),
    );
    rows
}

/// One persona row: `@name` with a `[persona]` tag and its brief.
pub fn persona_row(p: &PersonaSpec) -> PickItem {
    let brief: String = p.prompt.lines().next().unwrap_or("").to_string();
    PickItem {
        id: format!("persona:{}", p.name),
        label: format!("@{}", p.name),
        detail: format!("[persona] {brief}"),
    }
}

/// One terminal row: `@term:N` with a `[term]` tag and the cwd tail.
pub fn terminal_row(idx: usize, t: &TerminalInfo, self_id: Option<&str>) -> PickItem {
    let mut detail = format!("[term] {}", cwd_tail(&t.cwd));
    if Some(t.id.as_str()) == self_id {
        detail.push_str("  ·  bu terminal");
    }
    // Insert a stable session address so heartbeat ordering cannot retarget
    // an accepted mention between composing and submitting it.
    PickItem {
        id: t
            .session_id
            .as_ref()
            .map_or_else(|| format!("term:{}", idx + 1), |id| format!("session:{id}")),
        label: format!("@term:{}", idx + 1),
        detail: format!("{detail}  {}", t.label.as_deref().unwrap_or("")),
    }
}

/// The last two path segments of a cwd, for narrow picker rows.
fn cwd_tail(cwd: &str) -> String {
    let trimmed = cwd.trim_end_matches(['/', '\\']);
    let tail: Vec<&str> = trimmed.rsplit(['/', '\\']).take(2).collect();
    let tail: Vec<String> = tail.into_iter().rev().map(str::to_string).collect();
    if tail.iter().all(|s| s.is_empty()) {
        cwd.to_string()
    } else {
        tail.join("/")
    }
}

/// `$` popup rows: installed skills first, then the runtime catalog,
/// deduped by name.
pub fn skill_rows(installed: &[SkillEntry], runtime: &[crate::skills::Skill]) -> Vec<PickItem> {
    let mut rows: Vec<PickItem> = Vec::new();
    for e in installed {
        push_skill(&mut rows, &e.name, &e.description);
    }
    for s in runtime {
        push_skill(&mut rows, &s.name, &s.description);
    }
    rows
}

fn push_skill(rows: &mut Vec<PickItem>, name: &str, desc: &str) {
    if !name.is_empty() && !rows.iter().any(|r| r.id == name) {
        rows.push(PickItem {
            id: name.to_string(),
            label: format!("${name}"),
            detail: desc.to_string(),
        });
    }
}

/// Mentions in a submitted line: known persona names and `term:N` handles,
/// in order of appearance, deduped. `@unknown` and malformed handles are
/// left alone.
pub fn extract_mentions(line: &str, personas: &[PersonaSpec]) -> (Vec<String>, Vec<String>) {
    let mut names: Vec<String> = Vec::new();
    let mut terms: Vec<String> = Vec::new();
    for tok in line.split_whitespace() {
        let Some(word) = tok.strip_prefix('@') else {
            continue;
        };
        if let Some(n) = word.strip_prefix("term:") {
            let handle = format!("term:{n}");
            if !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()) && !terms.contains(&handle) {
                terms.push(handle);
            }
            continue;
        }
        if personas.iter().any(|p| p.name == word) && !names.iter().any(|n| n == word) {
            names.push(word.to_string());
        }
    }
    (names, terms)
}

/// Fence tag for a pinned file: from the extension, falling back to
/// [`sniff_language`] on the content when the extension is unknown.
pub fn lang_for_path(path: &str, content: &str) -> &'static str {
    let ext = path
        .rsplit_once('.')
        .map(|(_, e)| e.to_ascii_lowercase())
        .unwrap_or_default();
    match ext.as_str() {
        "rs" => "Rust",
        "toml" => "TOML",
        "json" => "JSON",
        "py" => "Python",
        "ts" | "tsx" => "TypeScript",
        "js" | "jsx" | "mjs" | "cjs" => "JavaScript",
        "md" | "markdown" => "Markdown",
        "yaml" | "yml" => "YAML",
        "sql" => "SQL",
        "sh" | "bash" | "zsh" | "fish" => "Shell",
        "html" | "htm" => "HTML",
        "css" => "CSS",
        "go" => "Go",
        "c" | "h" => "C",
        "cpp" | "hpp" | "cc" => "C++",
        "java" => "Java",
        "rb" => "Ruby",
        _ => sniff_language(content),
    }
}

pub fn render_picker(f: &mut ratatui::Frame, area: Rect, p: &Picker) {
    let (title, help) = match p.kind {
        PickerKind::Sessions => (" sessions ", " ↑↓ move · enter resume · esc close "),
        PickerKind::Tasks => (" scheduled tasks ", " ↑↓ move · esc close "),
        PickerKind::Activity => (" shell · subagents ", " ↑↓ move · esc close "),
        PickerKind::Models => (
            " model ",
            " type filters · ↑↓ move · enter select · esc close ",
        ),
        PickerKind::Effort => (" effort ", " ↑↓ move · enter select · esc close "),
        PickerKind::Permissions => (" permission ", " ↑↓ move · enter select · esc close "),
        PickerKind::Sandbox => (" sandbox ", " ↑↓ move · enter select · esc close "),
        PickerKind::Plugins => (" plugins ", " ↑↓ move · enter status · esc close "),
    };
    let visible_idx = p.visible();
    let search = p.kind == PickerKind::Models;
    let visible = area.height.saturating_sub(4).clamp(1, 12) as usize;
    let mut h = (visible_idx.len().max(1).min(visible) as u16) + 2;
    if search {
        h += 1;
    }
    let rect = Rect {
        x: area.x + 1,
        y: area.bottom().saturating_sub(h),
        width: area.width.saturating_sub(2),
        height: h.min(area.height),
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(Palette::accent_neon()))
        .title(Span::styled(
            title,
            Style::default()
                .fg(Palette::accent_neon())
                .add_modifier(Modifier::BOLD),
        ))
        .title_bottom(
            Line::from(Span::styled(help, Style::default().fg(Palette::dim()))).right_aligned(),
        )
        .style(Style::default().bg(Palette::bg()));
    let inner = block.inner(rect);
    f.render_widget(Clear, rect);
    f.render_widget(block, rect);

    if p.items.is_empty() {
        let empty = match p.kind {
            PickerKind::Sessions => "no saved sessions yet",
            PickerKind::Tasks => "no scheduled tasks  ·  varynth task add",
            PickerKind::Activity => "no background shells or subagents yet",
            PickerKind::Models => "provider returned no models",
            PickerKind::Effort | PickerKind::Permissions | PickerKind::Sandbox => "",
            PickerKind::Plugins => "no plugins",
        };
        f.render_widget(
            Paragraph::new(Span::styled(empty, Style::default().fg(Palette::dim()))),
            inner,
        );
        return;
    }
    let mut rows: Vec<Line> = Vec::new();
    if search {
        rows.push(Line::from(vec![
            Span::styled(" › ", Style::default().fg(Palette::warn())),
            Span::styled(format!("{}▌", p.query), Style::default().fg(Palette::fg())),
        ]));
    }
    if visible_idx.is_empty() {
        rows.push(Line::from(Span::styled(
            format!("no match for \"{}\"", p.query),
            Style::default().fg(Palette::dim()),
        )));
        f.render_widget(Paragraph::new(rows), inner);
        return;
    }
    let sel = p.sel.min(visible_idx.len() - 1);
    let skip = (sel + 1).saturating_sub(visible);
    for (pos, &idx) in visible_idx.iter().enumerate().skip(skip).take(visible) {
        let it = &p.items[idx];
        let on = pos == sel;
        let style = if on {
            Style::default()
                .fg(Palette::bg())
                .bg(Palette::accent_neon())
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Palette::fg())
        };
        rows.push(Line::from(vec![
            Span::styled(if on { " ▸ " } else { "   " }, style),
            Span::styled(it.label.clone(), style),
            Span::styled(
                format!("  {}", it.detail),
                Style::default().fg(Palette::dim()),
            ),
        ]));
    }
    f.render_widget(Paragraph::new(rows), inner);
}

/// The `#` / `@` / `$` popup, anchored like the slash menu and styled from
/// the theme so the three prefix popups read as one family. The trigger
/// char and query stay visible in the draft, so there is no search row.
pub fn render_prefix(f: &mut ratatui::Frame, area: Rect, c: &Composer) {
    let Some(kind) = c.prefix_kind() else {
        return;
    };
    let items = c.prefix_items();
    if area.height < 3 {
        return;
    }
    let (title, help) = match kind {
        PrefixKind::Files => (
            " files ",
            " ↑↓ move · tab complete · enter pin · esc close ",
        ),
        PrefixKind::Mentions => (" mention ", " ↑↓ move · enter select · esc close "),
        PrefixKind::Skills => (" skills ", " ↑↓ move · enter select · esc close "),
    };
    let query = c.prefix_query().unwrap_or("").to_string();
    let visible = PREFIX_ROWS
        .min(items.len().max(1))
        .min(area.height as usize - 2);
    let h = visible as u16 + 2;
    let rect = Rect {
        x: area.x + 1,
        y: area.bottom().saturating_sub(h),
        width: area.width.saturating_sub(2),
        height: h,
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(crate::theme::ACCENT))
        .title(Span::styled(
            title,
            Style::default()
                .fg(crate::theme::ACCENT_NEON)
                .add_modifier(Modifier::BOLD),
        ))
        .title_bottom(
            Line::from(Span::styled(
                help,
                Style::default().fg(crate::theme::Palette::dim()),
            ))
            .right_aligned(),
        )
        .style(Style::default().bg(crate::theme::BG));
    let inner = block.inner(rect);
    f.render_widget(Clear, rect);
    f.render_widget(block, rect);

    if items.is_empty() {
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                format!("no match for \"{query}\""),
                Style::default().fg(crate::theme::Palette::dim()),
            ))),
            inner,
        );
        return;
    }
    let sel = c.prefix_sel.min(items.len() - 1);
    let skip = (sel + 1).saturating_sub(visible);
    let label_w = items
        .iter()
        .map(|it| it.label.chars().count())
        .max()
        .unwrap_or(0)
        + 2;
    let desc_w = (inner.width as usize).saturating_sub(label_w + 5);
    let rows: Vec<Line> = items
        .iter()
        .enumerate()
        .skip(skip)
        .take(visible)
        .map(|(i, it)| {
            let on = i == sel;
            let style = if on {
                Style::default()
                    .fg(crate::theme::BG)
                    .bg(crate::theme::ACCENT_NEON)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(crate::theme::FG)
            };
            let mut desc: String = it.detail.chars().take(desc_w).collect();
            if it.detail.chars().count() > desc_w && desc_w > 1 {
                desc.pop();
                desc.push('…');
            }
            let label = format!("{:<w$}", it.label, w = label_w - 2);
            Line::from(vec![
                Span::styled(if on { " ▸ " } else { "   " }, style),
                Span::styled(label, style),
                Span::styled(desc, Style::default().fg(crate::theme::Palette::dim())),
            ])
        })
        .collect();
    f.render_widget(Paragraph::new(rows), inner);
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrap_breaks_at_width_and_newlines() {
        let (lines, pos) = wrap("abcdef\ngh", 9, 4);
        assert_eq!(lines, vec!["abcd", "ef", "gh"]);
        assert_eq!(pos, (2, 2));
    }

    #[test]
    fn wrap_puts_cursor_on_next_row_when_line_is_full() {
        let (lines, pos) = wrap("abcd", 4, 4);
        assert_eq!(lines, vec!["abcd", ""]);
        assert_eq!(pos, (1, 0));
        let (_, mid) = wrap("abcdef", 4, 4);
        assert_eq!(mid, (1, 0));
    }

    #[test]
    fn editing_respects_cursor_and_multibyte_chars() {
        let mut c = Composer::default();
        c.insert_str("şğü");
        c.left();
        c.insert_char('x');
        assert_eq!(c.text(), "şğxü");
        c.home();
        c.delete();
        assert_eq!(c.text(), "ğxü");
        c.end();
        c.backspace();
        assert_eq!(c.text(), "ğx");
    }

    #[test]
    fn backspace_on_empty_text_drops_last_attachment() {
        let mut c = Composer::default();
        c.attachments.push(ImageAttachment {
            media_type: "image/png".into(),
            data: "QQ==".into(),
        });
        assert!(!c.is_empty());
        c.backspace();
        assert!(c.is_empty());
    }

    #[test]
    fn history_recalls_and_restores_draft() {
        let mut c = Composer::default();
        assert!(!c.history_prev());
        c.insert_str("first");
        c.submit();
        c.insert_str("second");
        c.submit();
        c.insert_str("dra");
        assert!(c.history_prev());
        assert_eq!(c.text(), "second");
        assert!(c.history_prev());
        assert_eq!(c.text(), "first");
        assert!(c.history_next());
        assert!(c.history_next());
        assert_eq!(c.text(), "dra");
        assert!(!c.history_next());
    }

    #[test]
    fn submit_returns_trimmed_text_and_attachments() {
        let mut c = Composer::default();
        c.insert_str("  hi \r\n");
        c.attachments.push(ImageAttachment {
            media_type: "image/png".into(),
            data: "QQ==".into(),
        });
        let (line, imgs) = c.submit();
        assert_eq!(line, "hi");
        assert_eq!(imgs.len(), 1);
        assert!(c.is_empty());
    }

    fn cmds() -> Vec<SlashItem> {
        let mut c = builtin_commands();
        c.push(SlashItem::new("xlsx", "spreadsheets"));
        c
    }

    #[test]
    fn slash_matches_prefix_before_substring() {
        let all = cmds();
        let names = |q| -> Vec<String> {
            slash_matches(q, &all)
                .iter()
                .map(|i| i.name.clone())
                .collect()
        };
        assert_eq!(names("").len(), all.len());
        assert_eq!(
            names("s"),
            vec![
                "send",
                "status",
                "skills",
                "skills-search",
                "sandbox",
                "agents",
                "cost",
                "permission",
                "resume",
                "models",
                "plugins",
                "xlsx"
            ]
        );
        assert_eq!(names("SK"), vec!["skills", "skills-search"]);
        assert!(names("zzz").is_empty());
    }

    #[test]
    fn slash_menu_only_while_typing_a_single_command_word() {
        let mut c = Composer::default();
        c.commands = cmds();
        assert!(c.slash_items().is_empty());
        c.insert_str("/");
        assert_eq!(c.slash_items().len(), c.commands.len());
        c.insert_str("go");
        assert_eq!(c.slash_query(), Some("go"));
        c.insert_str(" fix tests");
        assert!(c.slash_items().is_empty());
        let mut c2 = Composer::default();
        c2.commands = cmds();
        c2.insert_str("hello /sk");
        assert!(c2.slash_items().is_empty());
    }

    #[test]
    fn slash_keys_navigate_complete_and_close() {
        use crossterm::event::KeyCode;
        let mut c = Composer::default();
        c.commands = cmds();
        c.insert_str("/s");
        assert!(c.slash_key(KeyCode::Down));
        assert!(c.slash_key(KeyCode::Tab));
        assert_eq!(c.text(), "/status ");
        assert!(c.slash_items().is_empty());

        let mut c = Composer::default();
        c.commands = cmds();
        c.insert_str("/stat");
        assert!(c.slash_key(KeyCode::Enter));
        assert_eq!(c.text(), "/status ");

        let mut c = Composer::default();
        c.commands = cmds();
        c.insert_str("/status");
        assert!(!c.slash_key(KeyCode::Enter), "exact command submits");

        c.backspace();
        assert!(c.slash_key(KeyCode::Esc));
        assert!(c.slash_items().is_empty());
        assert!(!c.slash_key(KeyCode::Up));
        c.insert_str("s");
        assert!(!c.slash_items().is_empty(), "typing reopens");
    }

    #[test]
    fn queue_is_fifo_and_bounded() {
        let mut c = Composer::default();
        assert_eq!(c.enqueue(), None);
        c.insert_str("one");
        assert_eq!(c.enqueue(), Some(1));
        assert!(c.is_empty());
        c.insert_str("two");
        assert_eq!(c.enqueue(), Some(2));
        c.enqueue_front(("urgent".into(), Vec::new()));
        assert_eq!(c.next_queued().unwrap().0, "urgent");
        assert_eq!(c.next_queued().unwrap().0, "one");
        assert_eq!(c.queued(), 1);
        for i in 1..MAX_QUEUE {
            c.insert_str(&format!("m{i}"));
            assert!(c.enqueue().is_some());
        }
        assert_eq!(c.queued(), MAX_QUEUE);
        c.insert_str("overflow");
        assert_eq!(c.enqueue(), None);
        assert_eq!(c.text(), "overflow");
    }

    #[test]
    fn queued_messages_add_rows_up_to_a_cap() {
        let mut c = Composer::default();
        let base = c.height(40);
        c.insert_str("a");
        c.enqueue();
        assert_eq!(c.height(40), base + 1);
        for _ in 0..5 {
            c.insert_str("b");
            c.enqueue();
        }
        assert_eq!(c.height(40), base + QUEUE_ROWS as u16 + 1);
    }

    #[test]
    fn height_grows_with_text_and_attachments() {
        let mut c = Composer::default();
        assert_eq!(c.height(40), 4);
        c.insert_str(&"x".repeat(36 * 3));
        assert_eq!(c.height(40), 2 + 4 + 1);
        c.attachments.push(ImageAttachment {
            media_type: "image/png".into(),
            data: "QQ==".into(),
        });
        assert_eq!(c.height(40), 2 + 1 + 4 + 1);
        c.insert_str(&"x".repeat(1000));
        assert_eq!(c.height(40), 2 + 1 + MAX_ROWS as u16 + 1);
    }

    #[test]
    fn paste_over_threshold_becomes_card_and_short_text_inserts() {
        let mut c = Composer::default();
        assert!(!c.paste("short line"));
        assert_eq!(c.text(), "short line");
        let long = (0..6)
            .map(|i| format!("line{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(c.paste(&long));
        assert_eq!(c.text(), "short line", "card text must not enter the draft");
        assert_eq!(c.pastes().len(), 1);
        assert!(c.paste(&"x".repeat(701)));
        assert_eq!(c.pastes().len(), 2);
        assert!(c.has_pastes());
        assert!(!c.is_empty());
        assert!(c.drop_last_paste());
        assert_eq!(c.pastes().len(), 1);
        assert!(c.drop_last_paste());
        assert!(!c.has_pastes());
        assert!(!c.drop_last_paste());
    }

    #[test]
    fn paste_stays_in_draft_until_six_lines_or_701_chars() {
        let mut c = Composer::default();
        assert!(!c.paste("a\nb\nc\nd\ne"));
        assert!(!c.paste(&"x".repeat(700)));
        assert_eq!(c.pastes().len(), 0);
        assert!(c.paste("a\nb\nc\nd\ne\nf"));
        assert!(c.paste(&"x".repeat(701)));
        assert_eq!(c.pastes().len(), 2);
    }

    #[test]
    fn height_accounts_for_paste_cards() {
        let mut c = Composer::default();
        let base = c.height(80);
        c.paste(&"line\n".repeat(7));
        assert_eq!(c.height(80), base + 4);
        c.paste(&"more\n".repeat(7));
        assert_eq!(c.height(80), base + 8);
    }

    #[test]
    fn submit_appends_fenced_paste_blocks_after_the_draft() {
        let mut c = Composer::default();
        c.insert_str("review this");
        let rust = "fn main() {\n    let x = 1;\n    let y = 2;\n    let z = x + y;\n    println!(\"{z}\");\n}";
        assert!(c.paste(rust));
        assert!(c.paste(
            "{\n  \"a\": 1,\n  \"b\": 2,\n  \"c\": 3,\n  \"d\": 4,\n  \"e\": 5,\n  \"f\": 6\n}"
        ));
        let (line, imgs) = c.submit();
        assert!(imgs.is_empty());
        assert!(line.starts_with("review this\n\n"), "draft first: {line}");
        assert!(line.contains("[Pasted text #1]\n```Rust\nfn main()"));
        assert!(line.contains("[Pasted text #2]\n```JSON"));
        assert!(line.ends_with("```"));
        assert!(!c.has_pastes(), "cards clear after send");
        assert!(c.is_empty());
    }

    #[test]
    fn paste_composes_alone_when_the_draft_is_empty() {
        let mut c = Composer::default();
        assert!(c.paste("a\nb\nc\nd\ne\nf"));
        let (line, _) = c.submit();
        assert!(line.starts_with("[Pasted text #1]\n"), "got: {line}");
    }

    #[test]
    fn sniff_language_detects_common_snippets() {
        assert_eq!(sniff_language("```rust\nfn main() {}\n```"), "Rust");
        assert_eq!(sniff_language("```yaml\na: 1\n```"), "YAML");
        assert_eq!(sniff_language("#!/usr/bin/env python3\nprint(1)"), "Python");
        assert_eq!(sniff_language("#!/bin/bash\necho hi"), "Shell");
        assert_eq!(sniff_language(r#"{"a": 1, "b": [2, 3]}"#), "JSON");
        assert_eq!(
            sniff_language("[package]\nname = \"varynth\"\nversion = \"0.1.0\""),
            "TOML"
        );
        assert_eq!(
            sniff_language("fn main() {\n    let x = 1;\n    println!(\"{x}\");\n}"),
            "Rust"
        );
        assert_eq!(
            sniff_language("def add(a, b):\n    return a + b\n\nprint(add(1, 2))"),
            "Python"
        );
        assert_eq!(
            sniff_language("interface User {\n  id: number;\n  name: string;\n}"),
            "TypeScript"
        );
        assert_eq!(
            sniff_language("const add = (a, b) => {\n  return a + b;\n};"),
            "JavaScript"
        );
        assert_eq!(
            sniff_language("SELECT id, name\nFROM users\nWHERE id = 1"),
            "SQL"
        );
        assert_eq!(
            sniff_language("# Title\n\nSome **bold** text and a [link](https://x.y)."),
            "Markdown"
        );
        assert_eq!(
            sniff_language("services:\n  web:\n    image: nginx\n    ports:\n      - 8080:80"),
            "YAML"
        );
        assert_eq!(
            sniff_language("just some plain words about nothing"),
            "Metin"
        );
    }

    #[test]
    fn card_rows_show_header_stats_and_hints() {
        let card = PasteCard {
            text: "fn main() {}\n".repeat(40),
            lang: "Rust",
            pin: None,
        };
        let rows = card_rows(1, &card, 50);
        assert_eq!(rows.len(), 4);
        assert!(
            rows[0].contains("[PANO METNİ EKLENDİ #1]"),
            "got: {}",
            rows[0]
        );
        assert!(rows[0].starts_with('┌') && rows[0].ends_with('┐'));
        assert!(rows[1].contains("40 satır"), "got: {}", rows[1]);
        assert!(rows[1].contains("~130 token · Rust"), "got: {}", rows[1]);
        assert!(rows[1].contains('📄'));
        assert!(rows[2].contains("Ctrl+O önizle · Ctrl+D iptal"));
        assert!(rows[3].starts_with('└') && rows[3].ends_with('┘'));
        assert_eq!(rows[0].chars().count(), 50);
        assert_eq!(rows[1].chars().count(), 49, "the 📄 emoji paints two cells");
        assert_eq!(rows[3].chars().count(), 50);
    }

    #[test]
    fn tokens_format_as_k_beyond_a_thousand() {
        assert_eq!(fmt_tokens(500), "500");
        assert_eq!(fmt_tokens(1000), "1.0k");
        assert_eq!(fmt_tokens(2400), "2.4k");
        assert_eq!(fmt_tokens(12_345), "12.3k");
    }

    fn pick_items(labels: &[&str]) -> Vec<PickItem> {
        labels
            .iter()
            .map(|l| PickItem {
                id: (*l).into(),
                label: (*l).into(),
                detail: String::new(),
            })
            .collect()
    }

    #[test]
    fn fuzzy_filter_ranks_matches_and_keeps_all_on_empty_query() {
        let items = pick_items(&["gpt-5", "claude-opus", "gemini"]);
        assert_eq!(fuzzy_indices("", &items), vec![0, 1, 2]);
        assert_eq!(fuzzy_indices("gpt", &items), vec![0]);
        assert_eq!(fuzzy_indices("op", &items), vec![1]);
        assert!(fuzzy_indices("zzz", &items).is_empty());
    }

    #[test]
    fn picker_query_filters_navigation_and_selection() {
        let mut p = Picker {
            kind: PickerKind::Models,
            items: pick_items(&["gpt-5", "gpt-4o", "claude-opus"]),
            sel: 0,
            query: String::new(),
        };
        p.down();
        assert_eq!(p.selected().map(|it| it.id.as_str()), Some("gpt-4o"));
        p.type_char('o');
        p.type_char('p');
        assert_eq!(p.query, "op");
        assert_eq!(p.visible(), vec![2], "only claude-opus matches 'op'");
        assert_eq!(p.selected().map(|it| it.id.as_str()), Some("claude-opus"));
        p.backspace();
        p.backspace();
        assert_eq!(p.visible().len(), 3, "empty query restores the full list");
        p.query = "5".into();
        assert_eq!(p.selected().map(|it| it.id.as_str()), Some("gpt-5"));
    }

    #[test]
    fn render_shows_paste_cards_under_the_draft() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;
        let mut c = Composer::default();
        c.insert_str("look at this");
        c.paste("fn main() {}\nlet a = 1;\nlet b = 2;\nlet c = 3;\nlet d = 4;\nlet e = 5;\n");
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal
            .draw(|f| {
                render(
                    f,
                    Rect::new(0, 0, 80, 20),
                    &c,
                    &Status {
                        busy: false,
                        spin: "⠋",
                        perm: PermissionMode::Prompt,
                        effort: "xhigh",
                        goal: false,
                        goal_for: None,
                        notice: None,
                        focused: true,
                    },
                )
            })
            .unwrap();
        let text = terminal.backend().to_string();
        assert!(text.contains("look at this"), "draft is rendered: {text}");
        assert!(
            text.contains("PANO METNİ EKLENDİ #1"),
            "card header is rendered: {text}"
        );
        assert!(text.contains("Ctrl+O önizle"), "card hint is rendered");
    }

    fn files(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn trigger_chars_open_one_popup_at_a_time() {
        let mut c = Composer::default();
        c.workspace_files = files(&["src/main.rs", "docs/notes.md"]);
        assert_eq!(c.prefix_kind(), None, "nothing typed yet");
        c.insert_str("#");
        assert_eq!(c.prefix_kind(), Some(PrefixKind::Files));
        assert_eq!(c.prefix_query(), Some(""));
        c.insert_str("src/ma");
        assert_eq!(c.prefix_query(), Some("src/ma"));
        c.insert_char(' ');
        assert_eq!(c.prefix_kind(), None, "a space ends the token");
        c.insert_char('x');
        c.insert_str("abc#d");
        assert_eq!(c.prefix_kind(), None, "# mid-token does not trigger");
        let mut m = Composer::default();
        m.insert_str("hey @co");
        assert_eq!(m.prefix_kind(), Some(PrefixKind::Mentions));
        let mut s = Composer::default();
        s.insert_str("$fi");
        assert_eq!(s.prefix_kind(), Some(PrefixKind::Skills));
        // Cursor off the end: no popup.
        let mut mid = Composer::default();
        mid.insert_str("#src fix");
        mid.left();
        mid.left();
        mid.left();
        mid.left();
        assert_eq!(mid.prefix_kind(), None);
    }

    #[test]
    fn prefix_popup_hidden_by_esc_and_reopened_by_typing() {
        let mut c = Composer::default();
        c.workspace_files = files(&["src/main.rs"]);
        c.insert_str("#");
        assert!(!c.prefix_items().is_empty());
        assert!(c.prefix_key(crossterm::event::KeyCode::Esc));
        assert!(c.prefix_items().is_empty(), "esc closes");
        assert_eq!(c.prefix_kind(), None);
        c.insert_str("m");
        assert!(!c.prefix_items().is_empty(), "typing reopens");
        assert!(c.prefix_key(crossterm::event::KeyCode::Down));
        // No matches: navigation falls through to the normal handlers.
        let mut c2 = Composer::default();
        c2.workspace_files = files(&["src/main.rs"]);
        c2.insert_str("#zzz");
        assert!(c2.prefix_items().is_empty());
        assert!(!c2.prefix_key(crossterm::event::KeyCode::Up));
        assert!(
            c2.prefix_key(crossterm::event::KeyCode::Esc),
            "esc still closes"
        );
        assert_eq!(c2.prefix_kind(), None);
    }

    #[test]
    fn accept_file_pin_marks_draft_and_rides_on_submit() {
        let mut c = Composer::default();
        c.workspace_files = files(&["src/main.rs", "docs/notes.md"]);
        c.insert_str("#ma");
        let pick = c.prefix_selected().unwrap();
        assert_eq!(pick.id, "src/main.rs");
        let content = "fn main() {}\n".repeat(700); // > 8 000 chars
        c.accept_file_pin(&pick.id, &content);
        assert_eq!(c.text(), "[file: src/main.rs] ");
        assert_eq!(c.pastes().len(), 1);
        assert_eq!(c.pastes()[0].pin.as_deref(), Some("src/main.rs"));
        assert_eq!(c.pastes()[0].lang, "Rust");
        assert_eq!(c.pastes()[0].text.chars().count(), PIN_MAX_CHARS);
        assert!(!c.is_empty());
        c.insert_str("review it");
        let (line, _) = c.submit();
        assert!(line.starts_with("[file: src/main.rs] review it"), "{line}");
        assert!(
            line.contains("[#pin src/main.rs]\n```Rust\nfn main()"),
            "{line}"
        );
        assert!(line.ends_with("```"));
        assert!(c.is_empty(), "pins clear after send");
    }

    #[test]
    fn multiple_pins_and_pastes_compose_in_order() {
        let mut c = Composer::default();
        c.workspace_files = files(&["a.toml", "b.json"]);
        c.insert_str("#a");
        c.accept_file_pin("a.toml", "[package]\nname = \"a\"");
        c.insert_str("and #b");
        c.accept_file_pin("b.json", "{\"a\": 1}");
        assert!(c.paste("p1\np2\np3\np4\np5\np6"));
        let (line, _) = c.submit();
        assert!(line.contains("[#pin a.toml]\n```TOML\n[package]"), "{line}");
        assert!(line.contains("[#pin b.json]\n```JSON"));
        assert!(
            line.contains("[Pasted text #1]\n```"),
            "pastes number among themselves: {line}"
        );
        let pin_a = line.find("[#pin a.toml]").unwrap();
        let pin_b = line.find("[#pin b.json]").unwrap();
        let paste = line.find("[Pasted text #1]").unwrap();
        assert!(pin_a < pin_b && pin_b < paste);
    }

    #[test]
    fn accept_mention_and_skill_replace_the_token() {
        let mut c = Composer::default();
        c.mentions = vec![PickItem {
            id: "persona:coder".into(),
            label: "@coder".into(),
            detail: String::new(),
        }];
        c.insert_str("hey @co");
        let pick = c.prefix_selected().unwrap();
        assert_eq!(pick.label, "@coder");
        c.accept_mention(&pick.label);
        assert_eq!(c.text(), "hey @coder ");
        assert_eq!(c.prefix_kind(), None, "trailing space closes the popup");

        let mut s = Composer::default();
        s.skills = vec![PickItem {
            id: "fix".into(),
            label: "$fix".into(),
            detail: String::new(),
        }];
        s.insert_str("$fi");
        assert!(s.prefix_key(crossterm::event::KeyCode::Tab));
        assert_eq!(s.text(), "/fix ");
    }

    #[test]
    fn tab_on_files_completes_the_path_and_keeps_filtering() {
        let mut c = Composer::default();
        c.workspace_files = files(&["src/main.rs", "src/memory.rs"]);
        c.insert_str("#main");
        assert!(c.prefix_key(crossterm::event::KeyCode::Tab));
        assert_eq!(c.text(), "#src/main.rs");
        assert_eq!(c.prefix_kind(), Some(PrefixKind::Files));
        assert_eq!(c.prefix_selected().unwrap().id, "src/main.rs");
    }

    #[test]
    fn filter_files_fuzzy_ranks_and_caps() {
        let all = files(&[
            "src/composer.rs",
            "src/compose.rs",
            "docs/composition.md",
            "README.md",
        ]);
        let top = filter_files("compose", &all);
        assert_eq!(
            top.len(),
            2,
            "only paths containing the compose subsequence match"
        );
        assert!(top.iter().any(|it| it.id == "src/composer.rs"));
        assert!(top.iter().any(|it| it.id == "src/compose.rs"));
        assert!(!top.iter().any(|it| it.id == "docs/composition.md"));
        assert_eq!(filter_files("", &all).len(), all.len());
        assert!(filter_files("zzzz", &all).is_empty());
        let many: Vec<String> = (0..60).map(|i| format!("f{i}.rs")).collect();
        assert_eq!(filter_files("", &many).len(), FILE_MATCH_CAP);
    }

    #[test]
    fn scan_skips_heavy_dirs_and_normalizes_separators() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("target/debug")).unwrap();
        std::fs::create_dir_all(root.join(".git/objects")).unwrap();
        std::fs::create_dir_all(root.join("node_modules/left-pad")).unwrap();
        std::fs::create_dir_all(root.join("src/deep")).unwrap();
        for p in [
            "target/debug/a.rs",
            ".git/config",
            "node_modules/left-pad/i.js",
            "src/lib.rs",
            "src/deep/nest.rs",
            "README.md",
        ] {
            std::fs::write(root.join(p), "//").unwrap();
        }
        std::fs::create_dir_all(root.join("with space")).unwrap();
        std::fs::write(root.join("with space/x.rs"), "//").unwrap();
        let got = scan_workspace_files(root);
        assert_eq!(
            got,
            vec![
                "README.md".to_string(),
                "src/deep/nest.rs".to_string(),
                "src/lib.rs".to_string(),
            ]
        );
    }

    #[test]
    fn scan_caps_the_file_list() {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..FILE_CACHE_CAP + 10 {
            std::fs::write(dir.path().join(format!("f{i:05}.txt")), "").unwrap();
        }
        assert_eq!(scan_workspace_files(dir.path()).len(), FILE_CACHE_CAP);
    }

    fn term(id: &str, cwd: &str) -> TerminalInfo {
        TerminalInfo {
            id: id.to_string(),
            pid: 1,
            cwd: cwd.to_string(),
            label: None,
            session_id: None,
            last_seen: chrono::Utc::now(),
        }
    }

    #[test]
    fn mention_rows_list_personas_then_numbered_terminals() {
        let personas = crate::agents::personas();
        let terminals = vec![term("self", "/home/me/proj/cli"), term("peer", "/other")];
        let rows = mention_rows(&personas, &terminals, Some("self"));
        assert_eq!(rows.len(), personas.len() + 2);
        assert_eq!(rows[0].label, "@coder");
        assert!(rows[0].detail.starts_with("[persona] "));
        let t1 = rows.iter().find(|r| r.label == "@term:1").unwrap();
        assert_eq!(t1.id, "term:1");
        assert!(t1.detail.starts_with("[term] "));
        assert!(
            t1.detail.contains("bu terminal"),
            "self is marked: {}",
            t1.detail
        );
        assert!(
            t1.detail.contains("proj/cli"),
            "cwd tail shown: {}",
            t1.detail
        );
        let t2 = rows.iter().find(|r| r.label == "@term:2").unwrap();
        assert!(!t2.detail.contains("bu terminal"));
        assert_eq!(cwd_tail("/a/b/c"), "b/c");
        assert_eq!(cwd_tail("only"), "only");
    }

    #[test]
    fn skill_rows_union_installed_and_runtime_deduped() {
        let installed = vec![
            SkillEntry {
                name: "fix".into(),
                description: "installed fixer".into(),
                source: "x".into(),
                installs: None,
            },
            SkillEntry {
                name: "only-local".into(),
                description: "local".into(),
                source: "y".into(),
                installs: None,
            },
        ];
        let runtime = vec![crate::skills::Skill {
            name: "fix".into(),
            description: "runtime fixer".into(),
            path: std::path::PathBuf::new(),
            body: String::new(),
        }];
        let rows = skill_rows(&installed, &runtime);
        assert_eq!(rows.len(), 2, "fix appears once");
        assert_eq!(rows[0].id, "fix");
        assert_eq!(rows[0].label, "$fix");
        assert_eq!(rows[0].detail, "installed fixer", "installed wins");
        assert_eq!(rows[1].id, "only-local");
    }

    #[test]
    fn extract_mentions_finds_personas_and_term_handles() {
        let personas = crate::agents::personas();
        let (names, terms) = extract_mentions(
            "@coder please @reviewer look, cc @unknown and @term:2 then @coder again",
            &personas,
        );
        assert_eq!(names, vec!["coder".to_string(), "reviewer".to_string()]);
        assert_eq!(terms, vec!["term:2".to_string()]);
        let (names, terms) = extract_mentions("no mentions here @term:x @term: @", &personas);
        assert!(names.is_empty());
        assert!(terms.is_empty(), "malformed handles are ignored");
    }

    #[test]
    fn lang_for_path_prefers_extension_then_sniffs() {
        assert_eq!(lang_for_path("src/main.rs", "anything"), "Rust");
        assert_eq!(lang_for_path("Cargo.toml", ""), "TOML");
        assert_eq!(lang_for_path("data.yml", ""), "YAML");
        assert_eq!(
            lang_for_path("Makefile", "fn main() {}\nlet x = 1;"),
            "Rust",
            "unknown extension falls back to sniffing"
        );
        assert_eq!(lang_for_path("notes.txt", "just some plain words"), "Metin");
    }

    #[test]
    fn card_rows_render_pin_header_and_keep_width() {
        let card = PasteCard {
            text: "fn main() {}\n".repeat(40),
            lang: "Rust",
            pin: Some("src/main.rs".into()),
        };
        let rows = card_rows(1, &card, 50);
        assert_eq!(rows.len(), 4);
        assert!(rows[0].contains("[#PİN src/main.rs]"), "got: {}", rows[0]);
        assert!(rows[0].starts_with('┌') && rows[0].ends_with('┐'));
        assert_eq!(rows[0].chars().count(), 50);
        // A very long path clips inside the frame.
        let long = PasteCard {
            text: "x".repeat(10),
            lang: "Metin",
            pin: Some("a/very/deeply/nested/directory/structure/with/a/long/file/name.rs".into()),
        };
        let rows = card_rows(1, &long, 40);
        assert_eq!(rows[0].chars().count(), 40);
        assert!(rows[0].contains('…'), "clipped with an ellipsis");
    }

    #[test]
    fn render_prefix_shows_file_rows_for_the_hash_popup() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;
        let mut c = Composer::default();
        c.insert_str("#ma");
        c.workspace_files = files(&["src/main.rs", "src/memory.rs", "docs/notes.md"]);
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal
            .draw(|f| render_prefix(f, Rect::new(0, 0, 80, 24), &c))
            .unwrap();
        let text = terminal.backend().to_string();
        assert!(text.contains("files"), "popup title: {text}");
        assert!(text.contains("src/main.rs"), "matched row: {text}");
        assert!(!text.contains("docs/notes.md"), "query filters rows");
    }

    #[test]
    fn render_prefix_shows_no_match_box() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;
        let mut c = Composer::default();
        c.insert_str("$zzz");
        c.skills = skill_rows(&[], &[]);
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal
            .draw(|f| render_prefix(f, Rect::new(0, 0, 80, 24), &c))
            .unwrap();
        let text = terminal.backend().to_string();
        assert!(text.contains("no match for \"zzz\""), "{text}");
    }
}
