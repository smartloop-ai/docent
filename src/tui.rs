//! `smartloop run` as a full-screen app, laid out like Claude Code:
//!
//! ```text
//! > what is the capital of France?
//!
//! ■ Paris is the capital of France.
//!
//!   References
//!   [1] https://en.wikipedia.org/wiki/Paris
//!   ⎿  sl-mini · 7 tokens · 0.4 tok/s · 16s
//!
//! [-] Reading en.wikipedia.org… (4s)
//! ╭──────────────────────────────────────────────────────╮
//! │ > _                                                  │
//! ╰──────────────────────────────────────────────────────╯
//!   [enter] send  [esc] interrupt  [?] shortcuts
//! ```
//!
//! The conversation fills the screen. The agent's steps (`chat.status`)
//! show on the live status line above the prompt, with what's running and
//! for how long, and a line per running download; a finished reply keeps
//! only the model that answered.
//! First-run setup (framework, agent, embeddings, chat model) shows as the
//! checklist under the banner before chat opens. Models and status open
//! as panels under the prompt.
//!
//! Blocking work (setup, the agent's REST calls, model downloads) runs on
//! plain threads and reports through a channel, so quitting never waits for
//! a download to finish.

use std::time::{Duration, Instant};

use crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture, Event,
    KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers,
    MouseButton, MouseEvent, MouseEventKind,
};
use futures_util::StreamExt;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Padding, Paragraph, Wrap};
use ratatui::{DefaultTerminal, Frame};
use reqwest::blocking::Client;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

use crate::chat::{self, ChatEvent, TurnStats};
use crate::progress::{self, Steps, format_duration, format_size};
use crate::usage::{SearchUsage, TokenStats};
use crate::{ModelRow, framework, mcp};

const LABEL_WIDTH: usize = 28;
const DETAIL_WIDTH: usize = 12;
/// Blocks in each setup step's bar.
const BLOCKS: usize = 16;
/// An active step shows its elapsed time once it has run this long.
const SHOW_ELAPSED_AFTER: Duration = Duration::from_secs(2);
const TICK: Duration = Duration::from_millis(100);
/// The most lines the prompt grows to before it scrolls.
const PROMPT_LINES: u16 = 6;
/// Lines one wheel notch scrolls, and one page key.
const WHEEL_LINES: u16 = 3;
const PAGE_LINES: u16 = 10;
/// How long after an Esc a second one clears the prompt. Generous, since a
/// quick double press can reach us as one Esc (crossterm reads two ESC bytes
/// that arrive together as a single key); the footer then asks for another.
const DOUBLE_ESC: Duration = Duration::from_secs(2);
/// Cells in `/usage`'s bar.
const USAGE_BAR: usize = 30;
/// A bar turning in brackets.
const SPINNER: [&str; 4] = ["[-]", "[\\]", "[|]", "[/]"];

const BANNER: [&str; 2] = [
    "█▀▄ █▀█ █▀▀ █▀▀ █▄  █ ▀█▀",
    "█▄▀ █▄█ █▄▄ ██▄ █ ▀▄█  █",
];

/// Run the app until the user quits, then print the session id so the
/// conversation can be resumed with `--session`.
pub fn run(client: &Client, prompt: Option<String>, project: Option<String>, session: Option<String>) {
    let runtime = tokio::runtime::Runtime::new()
        .unwrap_or_else(|e| crate::fail(format!("Failed to start async runtime: {}", e)));
    let mut terminal = ratatui::init();
    // The wheel scrolls the chat. Put mouse reporting back on a panic too,
    // or the shell gets escape codes for every move.
    // Dropping a file on the terminal pastes its path; bracketed paste
    // hands the app the whole path at once.
    let _ = crossterm::execute!(std::io::stdout(), EnableMouseCapture, EnableBracketedPaste);
    // Where the terminal supports it, have Shift+Enter reported apart from
    // Enter, for a new line in the prompt.
    let enhanced = crossterm::terminal::supports_keyboard_enhancement().unwrap_or(false);
    if enhanced {
        let _ = crossterm::execute!(
            std::io::stdout(),
            PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
        );
    }
    let hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = crossterm::execute!(std::io::stdout(), DisableMouseCapture, DisableBracketedPaste);
        if enhanced {
            let _ = crossterm::execute!(std::io::stdout(), PopKeyboardEnhancementFlags);
        }
        hook(info);
    }));
    let result = runtime.block_on(App::new(client.clone(), prompt, project, session).run(&mut terminal));
    let _ = crossterm::execute!(std::io::stdout(), DisableMouseCapture, DisableBracketedPaste);
    if enhanced {
        let _ = crossterm::execute!(std::io::stdout(), PopKeyboardEnhancementFlags);
    }
    ratatui::restore();
    // A chat turn may still be streaming; don't wait for it.
    runtime.shutdown_background();
    match result {
        Ok(session) => eprintln!("session: {}", session),
        Err(e) => crate::fail(e),
    }
}

/// What background work reports to the app.
enum AppEvent {
    Setup(StepEvent),
    SetupDone(Result<(), String>),
    Projects(Result<Vec<serde_json::Value>, String>),
    Models(Result<Vec<ModelRow>, String>),
    /// The project's MCP servers, for `/mcp`.
    Mcp(Result<Vec<mcp::Server>, String>),
    Chat(ChatEvent),
    TurnDone(Result<TurnStats, String>),
    Download(usize, StepEvent),
    DownloadDone(usize),
    Notice(String),
    Warning(String),
    Error(String),
    Status(Vec<(String, String)>),
    /// The account's web search usage; `None` when signed out.
    Usage(Option<SearchUsage>),
    /// The project's web search switch, read or just toggled; `toggled`
    /// says whether to announce it.
    WebSearch { enabled: bool, toggled: bool },
}

/// One `Steps` call, carried from a worker thread to the app.
#[derive(Clone)]
enum StepEvent {
    Add(String),
    Name(usize, String),
    Detail(usize, String),
    Start(usize),
    Note(usize, String),
    Progress(usize, u64, u64),
    Warn(String),
    Done(usize),
    Fail(usize, String),
}

/// `Steps` for a worker thread: each call goes to the app as an event.
struct Remote {
    tx: UnboundedSender<AppEvent>,
    wrap: Box<dyn Fn(StepEvent) -> AppEvent + Send>,
    count: usize,
}

impl Remote {
    fn new(tx: UnboundedSender<AppEvent>, wrap: impl Fn(StepEvent) -> AppEvent + Send + 'static) -> Self {
        Remote { tx, wrap: Box::new(wrap), count: 0 }
    }

    fn send(&self, event: StepEvent) {
        let _ = self.tx.send((self.wrap)(event));
    }
}

impl Steps for Remote {
    fn add(&mut self, label: &str) -> usize {
        self.send(StepEvent::Add(label.to_string()));
        self.count += 1;
        self.count - 1
    }
    fn add_name(&mut self, step: usize, name: &str) {
        self.send(StepEvent::Name(step, name.to_string()));
    }
    fn set_detail(&mut self, step: usize, detail: &str) {
        self.send(StepEvent::Detail(step, detail.to_string()));
    }
    fn start(&mut self, step: usize) {
        self.send(StepEvent::Start(step));
    }
    fn note(&mut self, step: usize, detail: &str) {
        self.send(StepEvent::Note(step, detail.to_string()));
    }
    fn progress(&mut self, step: usize, done: u64, total: u64) {
        self.send(StepEvent::Progress(step, done, total));
    }
    fn warn(&mut self, text: &str) {
        self.send(StepEvent::Warn(text.to_string()));
    }
    fn done(&mut self, step: usize) {
        self.send(StepEvent::Done(step));
    }
    /// The app shows the failure; this worker has nothing left to do, so it
    /// waits until the process exits.
    fn fail(&mut self, step: usize, message: String) -> ! {
        self.send(StepEvent::Fail(step, message));
        loop {
            std::thread::park();
        }
    }
    fn len(&self) -> usize {
        self.count
    }
}

#[derive(Clone, Copy, PartialEq)]
enum State {
    Pending,
    Active,
    Done,
    Failed,
}

struct StepRow {
    label: String,
    state: State,
    detail: String,
    /// Bytes so far and in all, while the step's bar shows.
    bytes: Option<(u64, u64)>,
    downloaded: Option<u64>,
    started: Option<Instant>,
}

/// A checklist drawn in the app, fed by `StepEvent`s: the same rows as
/// `progress::Checklist` draws on stderr.
struct StepList {
    steps: Vec<StepRow>,
    started: Instant,
    /// How long it took, once every step is done.
    took: Option<Duration>,
    failure: Option<String>,
}

impl StepList {
    fn new() -> Self {
        StepList { steps: Vec::new(), started: Instant::now(), took: None, failure: None }
    }

    fn apply(&mut self, event: StepEvent) {
        match event {
            StepEvent::Add(label) => self.steps.push(StepRow {
                label,
                state: State::Pending,
                detail: String::new(),
                bytes: None,
                downloaded: None,
                started: None,
            }),
            StepEvent::Name(i, name) => {
                if let Some(label) = progress::with_name(&self.steps[i].label, &name) {
                    self.steps[i].label = label;
                }
            }
            StepEvent::Detail(i, detail) => self.steps[i].detail = detail,
            StepEvent::Start(i) => self.start(i),
            StepEvent::Note(i, detail) => {
                self.start(i);
                self.steps[i].detail = detail;
                self.steps[i].bytes = None;
            }
            StepEvent::Progress(i, done, total) => {
                self.start(i);
                let s = &mut self.steps[i];
                if s.bytes.is_none() {
                    s.detail.clear();
                }
                s.bytes = Some((done, total));
                s.downloaded = Some(total.max(done));
            }
            // Warnings go to the status pane; see `App::apply_setup`.
            StepEvent::Warn(_) => {}
            StepEvent::Done(i) => {
                let s = &mut self.steps[i];
                if s.state != State::Done {
                    s.state = State::Done;
                    s.bytes = None;
                    if s.detail.is_empty()
                        && let Some(size) = s.downloaded
                    {
                        s.detail = format_size(size);
                    }
                }
            }
            StepEvent::Fail(i, message) => {
                self.steps[i].state = State::Failed;
                self.steps[i].bytes = None;
                self.failure = Some(message);
            }
        }
    }

    fn start(&mut self, i: usize) {
        let s = &mut self.steps[i];
        if s.state == State::Pending {
            s.state = State::Active;
            s.started = Some(Instant::now());
        }
    }

    fn finish(&mut self) {
        for s in &mut self.steps {
            if matches!(s.state, State::Pending | State::Active) {
                s.state = State::Done;
                s.bytes = None;
            }
        }
        self.took = Some(self.started.elapsed());
    }

    fn active(&self) -> Option<&StepRow> {
        self.steps.iter().find(|s| s.state == State::Active)
    }

    /// Setup as a box, a row per step, each with a bar of blocks: filling
    /// as a download's bytes arrive, a pulse while any other step runs,
    /// full and green once it's done.
    ///
    /// ```text
    /// ╭ Booting up …           ──────────────────────────────────╮
    /// │                                                          │
    /// │  [x] Start agent          ▰▰▰▰▰▰▰▰▰▰▰▰▰▰▰▰  port 38540   │
    /// │  [•] Base model sl-mini   ▰▰▰▰▰▰▰▰▱▱▱▱▱▱▱▱  49%          │
    /// │  [ ] Default project      ▱▱▱▱▱▱▱▱▱▱▱▱▱▱▱▱               │
    /// │                                                          │
    /// ╰──────────────────────────────── 377 MB of 769 MB · 23s ──╯
    /// ```
    fn card(&self, ticks: usize) -> Vec<Line<'static>> {
        // Labels as wide as the longest, so the bars line up close to them.
        let label_width = self.steps.iter().map(|s| s.label.chars().count()).max().unwrap_or(0).min(LABEL_WIDTH);
        let rows: Vec<Line<'static>> = self.steps.iter().map(|s| self.card_row(s, label_width, ticks)).collect();
        let inner = rows.iter().map(Line::width).max().unwrap_or(0) + 2;
        let border = dim();

        let title = " Booting up… ";
        let mut lines = vec![Line::from(vec![
            Span::styled("╭", border),
            Span::styled(title, Style::new().bold()),
            Span::styled(format!("{}╮", "─".repeat(inner.saturating_sub(title.chars().count()))), border),
        ])];
        let blank = Line::default();
        for row in std::iter::once(&blank).chain(rows.iter()).chain(std::iter::once(&blank)) {
            let mut spans = vec![Span::styled("│", border)];
            spans.extend(row.spans.iter().cloned());
            spans.push(Span::raw(" ".repeat(inner.saturating_sub(row.width()))));
            spans.push(Span::styled("│", border));
            lines.push(Line::from(spans));
        }
        // The running download's bytes and time, or how long it all took.
        let note = match (self.active(), self.took) {
            (_, Some(took)) => format!(" done in {} ", format_duration(took)),
            (Some(StepRow { bytes: Some((done, total)), started, .. }), None) if *total > 0 => format!(
                " {} of {} · {} ",
                format_size(*done),
                format_size(*total),
                format_duration(started.map_or(Duration::ZERO, |t| t.elapsed()))
            ),
            (Some(StepRow { started: Some(t), .. }), None) => format!(" {} ", format_duration(t.elapsed())),
            _ => String::new(),
        };
        let fill = inner.saturating_sub(note.chars().count() + 2);
        lines.push(Line::from(vec![
            Span::styled(format!("╰{}", "─".repeat(fill)), border),
            Span::styled(note, dim()),
            Span::styled("──╯", border),
        ]));
        lines
    }

    fn card_row(&self, s: &StepRow, label_width: usize, ticks: usize) -> Line<'static> {
        let label = format!("{:<w$}", truncate(&s.label, label_width), w = label_width);
        let (mark, label_style) = match s.state {
            State::Done => (bracket("x", Color::Green), Style::new()),
            State::Failed => (bracket("✗", Color::Red), Style::new()),
            State::Active => (bracket("•", pink()), Style::new()),
            State::Pending => (Span::styled("[ ] ", dim()), dim()),
        };
        let lit = |on: bool, color: Color| {
            if on { Span::styled("▰", Style::new().fg(color)) } else { Span::styled("▱", dim()) }
        };
        let blocks: Vec<Span<'static>> = match (s.state, s.bytes) {
            (State::Done, _) => (0..BLOCKS).map(|_| lit(true, Color::Green)).collect(),
            (State::Active, Some((done, total))) if total > 0 => {
                let lit_count = (done as f64 / total as f64 * BLOCKS as f64).round() as usize;
                (0..BLOCKS).map(|i| lit(i < lit_count, pink())).collect()
            }
            // Running with nothing to count: a pulse of three blocks sweeps
            // along the track.
            (State::Active, _) => {
                let at = ticks % (BLOCKS + 3);
                (0..BLOCKS).map(|i| lit(i < at && i + 3 >= at, pink())).collect()
            }
            _ => (0..BLOCKS).map(|_| lit(false, pink())).collect(),
        };
        let detail = match (s.state, s.bytes) {
            (State::Active, Some((done, total))) if total > 0 => format!("{}%", done * 100 / total),
            (State::Active, _) => s
                .started
                .map(|t| t.elapsed())
                .filter(|t| *t >= SHOW_ELAPSED_AFTER)
                .map(format_duration)
                .map_or(s.detail.clone(), |t| if s.detail.is_empty() { t } else { format!("{} {}", s.detail, t) }),
            (State::Done, _) => s.detail.clone(),
            _ => String::new(),
        };
        let mut spans = vec![Span::raw("  "), mark, Span::styled(label, label_style), Span::raw("  ")];
        spans.extend(blocks);
        spans.push(Span::styled(format!("  {:<w$}", detail, w = DETAIL_WIDTH), dim()));
        Line::from(spans)
    }
}

fn bracket(mark: &str, color: Color) -> Span<'static> {
    Span::styled(format!("[{}] ", mark), Style::new().fg(color))
}

/// `██████▋░░░░   49%  377 MB/769 MB`, or the bytes so far when the size
/// isn't known.
fn bar_spans(done: u64, total: u64, width: usize) -> Vec<Span<'static>> {
    if total == 0 {
        return vec![Span::styled(format_size(done), dim())];
    }
    let fraction = (done as f64 / total as f64).clamp(0.0, 1.0);
    vec![
        Span::styled(progress::bar(fraction, width), Style::new().fg(pink())),
        Span::raw(format!(
            "  {:>3}%  {}/{}",
            (fraction * 100.0) as u64,
            format_size(done),
            format_size(total)
        )),
    ]
}

/// A model download started from the models panel.
struct Download {
    model: String,
    list: StepList,
    finished: bool,
}

enum Entry {
    User(String),
    Reply(Reply),
    Notice(String),
    Warning(String),
    Error(String),
}

/// One answer as it streams in. The agent's steps show on the status line
/// while it runs; only the model that answered stays with the reply.
#[derive(Default)]
struct Reply {
    /// Problems worth keeping, e.g. a dropped connection that was retried.
    warnings: Vec<String>,
    model: Option<String>,
    text: String,
    citations: Vec<String>,
    stats: Option<TurnStats>,
    /// Why it ended early: stopped by the user, or an error.
    ended: Option<String>,
}

enum Overlay {
    None,
    Models { rows: Option<Result<Vec<ModelRow>, String>>, selected: usize },
    /// `/mcp`: the project's MCP servers.
    Mcp { rows: Option<Result<Vec<mcp::Server>, String>>, selected: usize },
    Help,
    /// `/status`: rows fill in once the agent answers.
    Status(Option<Vec<(String, String)>>),
    /// `/usage`: this month's web search allowance, re-read on opening
    /// (`loading` until the platform answers), and the week's tokens.
    Usage { loading: bool, tokens: TokenStats },
}

enum Phase {
    /// First-run setup is running (or failed); chat opens when it's done.
    Setup,
    /// Looking up the project to chat in.
    Connecting,
    Ready,
}

struct Turn {
    started: Instant,
    task: tokio::task::JoinHandle<()>,
    /// Index of its `Entry::Reply`.
    entry: usize,
    /// Content chunks so far, for the status line.
    tokens: u64,
    /// The latest `chat.status` message, for the status line.
    activity: Option<String>,
}

struct App {
    client: Client,
    tx: UnboundedSender<AppEvent>,
    rx: UnboundedReceiver<AppEvent>,
    phase: Phase,
    setup: StepList,
    project: Option<(String, String)>,
    wanted_project: Option<String>,
    session: String,
    pending_prompt: Option<String>,
    entries: Vec<Entry>,
    turn: Option<Turn>,
    downloads: Vec<Download>,
    overlay: Overlay,
    input: String,
    /// Cursor position in `input`, in chars.
    cursor: usize,
    history: Vec<String>,
    history_at: Option<usize>,
    /// When Esc was last pressed at the prompt, so a second one clears it.
    last_esc: Option<Instant>,
    /// Lines scrolled up from the bottom of the conversation; 0 follows it.
    scroll_back: u16,
    /// How far up the conversation goes, as of the last draw.
    scroll_limit: std::cell::Cell<u16>,
    /// The conversation pane as last drawn, row by row, for copying a
    /// selection out of it.
    shown: std::cell::RefCell<(Rect, Vec<Vec<String>>)>,
    /// Text being selected with the mouse, in screen cells.
    selection: Option<Selection>,
    /// What the last selection copied, flashed in the footer for a moment.
    copied: Option<(usize, Instant)>,
    /// Kept for the app's life: on X11 the clipboard empties when its owner
    /// goes away.
    clipboard: Option<arboard::Clipboard>,
    ticks: usize,
    /// The prompt takes a token for `/login`, masked, instead of a message.
    token_entry: bool,
    /// Files dropped on the prompt, each shown there as a chip such as
    /// `[Image 1]` until the message is sent.
    attachments: Vec<Attachment>,
    /// The highlighted slash-command hint while typing a command.
    hint_at: usize,
    /// This month's web search usage, shown in the footer once known.
    search: Option<SearchUsage>,
    /// The project's web search switch (ctrl+s), once read.
    web_search: Option<bool>,
    quit: bool,
}

impl App {
    fn new(client: Client, prompt: Option<String>, project: Option<String>, session: Option<String>) -> Self {
        let (tx, rx) = unbounded_channel();
        App {
            client,
            tx,
            rx,
            phase: Phase::Setup,
            setup: StepList::new(),
            project: None,
            wanted_project: project,
            session: session.unwrap_or_else(chat::new_session_id),
            pending_prompt: prompt.filter(|p| !p.trim().is_empty()),
            entries: Vec::new(),
            turn: None,
            downloads: Vec::new(),
            overlay: Overlay::None,
            input: String::new(),
            cursor: 0,
            history: Vec::new(),
            history_at: None,
            last_esc: None,
            scroll_back: 0,
            scroll_limit: std::cell::Cell::new(0),
            shown: std::cell::RefCell::new((Rect::default(), Vec::new())),
            selection: None,
            copied: None,
            clipboard: None,
            ticks: 0,
            token_entry: false,
            attachments: Vec::new(),
            hint_at: 0,
            search: None,
            web_search: None,
            quit: false,
        }
    }

    /// The event loop. Returns the session id, or an error that should
    /// print once the terminal is restored.
    async fn run(mut self, terminal: &mut DefaultTerminal) -> Result<String, String> {
        self.start_setup();
        let mut keys = EventStream::new();
        let mut tick = tokio::time::interval(TICK);
        while !self.quit {
            terminal
                .draw(|frame| self.draw(frame))
                .map_err(|e| format!("Failed to draw: {}", e))?;
            tokio::select! {
                event = keys.next() => match event {
                    Some(Ok(Event::Key(key))) if key.kind == KeyEventKind::Press => self.key(key),
                    Some(Ok(Event::Mouse(mouse))) if matches!(self.overlay, Overlay::None) => self.mouse(mouse),
                    // Pasting is typing too: it closes an open panel.
                    Some(Ok(Event::Paste(text))) => {
                        self.overlay = Overlay::None;
                        self.paste(&text)
                    }
                    Some(Ok(_)) => {}
                    Some(Err(e)) => return Err(format!("Failed to read input: {}", e)),
                    None => break,
                },
                Some(event) = self.rx.recv() => {
                    self.apply(event);
                    // Take everything queued before drawing again, so a fast
                    // download doesn't redraw once per chunk.
                    while let Ok(event) = self.rx.try_recv() {
                        self.apply(event);
                    }
                }
                _ = tick.tick() => self.ticks += 1,
            }
        }
        Ok(self.session)
    }

    // ----- background work -------------------------------------------------

    /// Run `work` on its own thread; it reports through the channel.
    fn spawn(&self, work: impl FnOnce(Client, UnboundedSender<AppEvent>) + Send + 'static) {
        let (client, tx) = (self.client.clone(), self.tx.clone());
        std::thread::spawn(move || work(client, tx));
    }

    fn start_setup(&self) {
        self.spawn(|client, tx| {
            let mut list = Remote::new(tx.clone(), AppEvent::Setup);
            let result = framework::prepare(&mut list, &client, &crate::base_url(), crate::is_local());
            let _ = tx.send(AppEvent::SetupDone(result));
        });
    }

    fn load_projects(&self) {
        self.spawn(|client, tx| {
            let _ = tx.send(AppEvent::Projects(crate::try_fetch_projects(&client)));
        });
    }

    /// Re-read the web search usage from the platform. Also the last step
    /// of each turn, which may have spent a search.
    fn load_usage(&self) {
        self.spawn(|client, tx| {
            let _ = tx.send(AppEvent::Usage(crate::usage::fetch(&client)));
        });
    }

    fn load_models(&self) {
        let Some((id, _)) = self.project.clone() else { return };
        self.spawn(move |client, tx| {
            let _ = tx.send(AppEvent::Models(crate::model_rows(&client, &id)));
        });
    }

    fn enable_model(&mut self, name: String) {
        let Some((project, _)) = self.project.clone() else { return };
        if self.downloads.iter().any(|d| d.model == name && !d.finished) {
            return;
        }
        let id = self.downloads.len();
        self.downloads.push(Download { model: name.clone(), list: StepList::new(), finished: false });
        self.spawn(move |client, tx| {
            let mut list = Remote::new(tx.clone(), move |e| AppEvent::Download(id, e));
            crate::enable_steps(&mut list, &client, &project, &name);
            let _ = tx.send(AppEvent::DownloadDone(id));
            let _ = tx.send(AppEvent::Notice(format!("Model {} enabled", name)));
        });
    }

    fn disable_model(&mut self, name: String) {
        let Some((project, _)) = self.project.clone() else { return };
        self.spawn(move |client, tx| {
            let event = match crate::patch_project_model(&client, &project, &name, false) {
                Ok(()) => AppEvent::Notice(format!("Model {} disabled", name)),
                Err(e) => AppEvent::Error(e),
            };
            let _ = tx.send(event);
            let _ = tx.send(AppEvent::Models(crate::model_rows(&client, &project)));
        });
    }

    /// Send a turn: `shown` is the prompt as typed (chips and all), `message`
    /// what the agent gets. Attached files are uploaded first.
    fn send_turn(&mut self, shown: String, message: String, files: Vec<Attachment>) {
        let Some((project, _)) = self.project.clone() else { return };
        self.entries.push(Entry::User(shown));
        self.entries.push(Entry::Reply(Reply::default()));
        let entry = self.entries.len() - 1;
        self.scroll_back = 0;
        // Made per turn: the agent picks its port once it's up, and a new
        // one each time it restarts.
        let chat = chat::openai_client(crate::api_url());
        let (tx, session, client) = (self.tx.clone(), self.session.clone(), self.client.clone());
        let task = tokio::spawn(async move {
            let events = tx.clone();
            let mut on = move |event| {
                let _ = events.send(AppEvent::Chat(event));
            };
            let mut ids = Vec::new();
            for file in files {
                on(ChatEvent::Status { step: "upload".to_string(), message: format!("Uploading {}", file.name()) });
                let client = client.clone();
                let uploaded = tokio::task::spawn_blocking(move || crate::upload_asset(&client, &file.path)).await;
                match uploaded {
                    Ok(Ok(id)) => ids.push(id),
                    Ok(Err(e)) => return drop(tx.send(AppEvent::TurnDone(Err(e)))),
                    Err(e) => return drop(tx.send(AppEvent::TurnDone(Err(format!("Upload failed: {}", e))))),
                }
            }
            let result = chat::stream_turn(&chat, &message, &ids, &project, &session, &mut on).await;
            let _ = tx.send(AppEvent::TurnDone(result));
        });
        self.turn = Some(Turn { started: Instant::now(), task, entry, tokens: 0, activity: None });
    }

    fn stop_turn(&mut self) {
        if let Some(turn) = self.turn.take() {
            turn.task.abort();
            if let Some(Entry::Reply(reply)) = self.entries.get_mut(turn.entry) {
                reply.ended = Some("Interrupted".to_string());
            }
        }
    }

    // ----- events ------------------------------------------------------------

    fn apply(&mut self, event: AppEvent) {
        match event {
            AppEvent::Setup(StepEvent::Warn(text)) => self.entries.push(Entry::Warning(text)),
            AppEvent::Setup(event) => self.setup.apply(event),
            AppEvent::SetupDone(Ok(())) => {
                self.setup.finish();
                if let Some(took) = self.setup.took.filter(|_| !self.setup.steps.is_empty()) {
                    self.entries.push(Entry::Notice(format!("Setup complete in {}", format_duration(took))));
                }
                self.phase = Phase::Connecting;
                self.load_projects();
                self.load_usage();
            }
            AppEvent::SetupDone(Err(e)) => self.setup.failure = Some(e),
            AppEvent::Projects(result) => self.projects_loaded(result),
            AppEvent::Models(result) => {
                if let Overlay::Models { rows, selected } = &mut self.overlay {
                    if let Ok(list) = &result {
                        *selected = (*selected).min(list.len().saturating_sub(1));
                    }
                    *rows = Some(result);
                }
            }
            AppEvent::Mcp(result) => {
                if let Overlay::Mcp { rows, selected } = &mut self.overlay {
                    if let Ok(list) = &result {
                        *selected = (*selected).min(list.len().saturating_sub(1));
                    }
                    *rows = Some(result);
                }
            }
            AppEvent::Chat(event) => self.chat_event(event),
            AppEvent::TurnDone(result) => {
                let Some(turn) = self.turn.take() else { return };
                if let Some(Entry::Reply(reply)) = self.entries.get_mut(turn.entry) {
                    match result {
                        Ok(stats) => reply.stats = Some(stats),
                        Err(e) => reply.ended = Some(e),
                    }
                }
                self.load_usage();
            }
            AppEvent::Download(id, StepEvent::Warn(text)) => {
                let model = self.downloads[id].model.clone();
                self.entries.push(Entry::Warning(format!("{}: {}", model, text)));
            }
            AppEvent::Download(id, event) => {
                if let StepEvent::Fail(_, message) = &event {
                    self.entries.push(Entry::Error(message.clone()));
                    self.downloads[id].finished = true;
                }
                self.downloads[id].list.apply(event);
            }
            AppEvent::DownloadDone(id) => {
                self.downloads[id].list.finish();
                self.downloads[id].finished = true;
                self.load_models();
            }
            AppEvent::Notice(text) => self.entries.push(Entry::Notice(text)),
            AppEvent::Warning(text) => self.entries.push(Entry::Warning(text)),
            AppEvent::Status(rows) => {
                if let Overlay::Status(shown) = &mut self.overlay {
                    *shown = Some(rows);
                }
            }
            AppEvent::Error(text) => self.entries.push(Entry::Error(text)),
            AppEvent::Usage(usage) => {
                self.search = usage;
                if let Overlay::Usage { loading, .. } = &mut self.overlay {
                    *loading = false;
                }
            }
            AppEvent::WebSearch { enabled, toggled } => {
                self.web_search = Some(enabled);
                if toggled {
                    let text = if enabled { "Web search on" } else { "Web search off; ctrl+s turns it back on" };
                    self.entries.push(Entry::Notice(text.to_string()));
                }
            }
        }
    }

    /// Pick the project: the one asked for, else the server's current one.
    fn projects_loaded(&mut self, result: Result<Vec<serde_json::Value>, String>) {
        if !matches!(self.phase, Phase::Connecting) {
            return;
        }
        let projects = match result {
            Ok(projects) => projects,
            Err(e) => {
                self.entries.push(Entry::Error(e));
                return;
            }
        };
        let field = |p: &serde_json::Value, k: &str| p[k].as_str().unwrap_or_default().to_string();
        let chosen = match &self.wanted_project {
            Some(id) => projects
                .iter()
                .find(|p| field(p, "id") == *id)
                .map(|p| (field(p, "id"), field(p, "name")))
                .or_else(|| Some((id.clone(), id.clone()))),
            None => projects
                .iter()
                .find(|p| p["current"].as_bool().unwrap_or_default())
                .or_else(|| projects.first())
                .map(|p| (field(p, "id"), field(p, "name"))),
        };
        self.phase = Phase::Ready;
        match chosen {
            Some(project) => {
                self.project = Some(project);
                self.load_web_search();
                if let Some(prompt) = self.pending_prompt.take() {
                    self.send_turn(prompt.clone(), prompt, Vec::new());
                }
            }
            None => self.entries.push(Entry::Error(
                "No projects found; create one with `docent project create`".to_string(),
            )),
        }
    }

    fn chat_event(&mut self, event: ChatEvent) {
        let Some(turn) = self.turn.as_mut() else { return };
        let Some(Entry::Reply(reply)) = self.entries.get_mut(turn.entry) else { return };
        match event {
            ChatEvent::Content(content) => {
                turn.tokens += 1;
                reply.text.push_str(&content);
            }
            ChatEvent::Citations(list) => {
                reply.citations = list.iter().filter_map(chat::citation_label).collect();
            }
            ChatEvent::Status { step, message } => {
                if step == "model"
                    && let Some(name) = selected_model(&message)
                {
                    reply.model = Some(name);
                }
                turn.activity = Some(message.trim_end_matches(['.', '…']).to_string());
            }
            ChatEvent::Retrying(e) => reply.warnings.push(format!("Connection dropped, retrying: {}", e)),
        }
    }

    // ----- keys ------------------------------------------------------------

    /// The wheel scrolls; a left-button drag over the conversation selects,
    /// and letting go copies the selection.
    fn mouse(&mut self, mouse: MouseEvent) {
        let at = (mouse.column, mouse.row);
        match mouse.kind {
            MouseEventKind::ScrollUp => self.scroll(WHEEL_LINES as i32),
            MouseEventKind::ScrollDown => self.scroll(-(WHEEL_LINES as i32)),
            MouseEventKind::Down(MouseButton::Left) => {
                let area = self.shown.borrow().0;
                self.selection = area.contains(at.into()).then_some(Selection { anchor: at, head: at });
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                if let Some(selection) = &mut self.selection {
                    let area = self.shown.borrow().0;
                    selection.head = clamp_to(area, at);
                }
            }
            MouseEventKind::Up(MouseButton::Left) => {
                let text = self.selection.as_ref().filter(|s| s.anchor != s.head).map(|s| self.selected_text(s));
                match text.filter(|t| !t.trim().is_empty()) {
                    Some(text) => self.copy(text),
                    None => self.selection = None,
                }
            }
            _ => {}
        }
    }

    /// The selected cells' text, a line per screen row, trailing blanks off.
    fn selected_text(&self, selection: &Selection) -> String {
        let shown = self.shown.borrow();
        let (area, rows) = (&shown.0, &shown.1);
        selection
            .rows(*area)
            .map(|(y, from, to)| {
                let row = &rows[(y - area.y) as usize];
                let line: String = row[(from - area.x) as usize..=(to - area.x) as usize].concat();
                line.trim_end().to_string()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Put `text` on the system clipboard, or where there's none (over SSH),
    /// ask the terminal to with OSC 52.
    fn copy(&mut self, text: String) {
        self.copied = Some((text.chars().count(), Instant::now()));
        if cfg!(test) {
            return;
        }
        if self.clipboard.is_none() {
            self.clipboard = arboard::Clipboard::new().ok();
        }
        let copied = self.clipboard.as_mut().is_some_and(|c| c.set_text(text.clone()).is_ok());
        if !copied {
            use std::io::Write;
            let mut out = std::io::stdout();
            let _ = write!(out, "\x1b]52;c;{}\x07", base64(text.as_bytes()));
            let _ = out.flush();
        }
    }

    fn key(&mut self, key: KeyEvent) {
        // Typing moves on from a selection.
        self.selection = None;
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match (key.code, ctrl) {
            (KeyCode::Char('c'), true) => {
                self.quit = true;
                return;
            }
            (KeyCode::Char('o'), true) => return self.open_models(),
            (KeyCode::Char('s'), true) => return self.toggle_web_search(),
            _ => {}
        }
        let handled = match self.overlay {
            Overlay::None => false,
            Overlay::Models { .. } => self.key_models(key),
            Overlay::Mcp { .. } => self.key_mcp(key),
            Overlay::Help | Overlay::Status(_) | Overlay::Usage { .. } => {
                let close = matches!(key.code, KeyCode::Esc | KeyCode::Enter);
                if close {
                    self.overlay = Overlay::None;
                }
                close
            }
        };
        // A panel keeps only its own keys; anything else is typing, which
        // closes it and goes to the prompt.
        if !handled {
            self.overlay = Overlay::None;
            self.key_chat(key);
        }
    }

    fn open_models(&mut self) {
        if self.project.is_none() {
            return;
        }
        self.overlay = Overlay::Models { rows: None, selected: 0 };
        self.load_models();
    }

    fn open_mcp(&mut self) {
        if self.project.is_none() {
            return;
        }
        self.overlay = Overlay::Mcp { rows: None, selected: 0 };
        self.load_mcp();
    }

    fn load_mcp(&self) {
        let Some((id, _)) = self.project.clone() else { return };
        self.spawn(move |client, tx| {
            let _ = tx.send(AppEvent::Mcp(mcp::list(&client, &id)));
        });
    }

    /// `/mcp add <url>`: register the server, opening the browser when it
    /// wants a sign-in, then say once it's connected (or why not).
    fn add_mcp(&mut self, url: &str) {
        let url = match mcp::parse_url(url) {
            Ok(url) => url,
            Err(e) => return self.entries.push(Entry::Error(e)),
        };
        let Some((project, _)) = self.project.clone() else { return };
        self.entries.push(Entry::Notice(format!("Adding MCP server {}", url)));
        self.spawn(move |client, tx| {
            let id = match mcp::add(&client, &project, &url) {
                Ok(mcp::Added::Server(server)) => Some(server.id),
                Ok(mcp::Added::SignIn(auth_url)) => {
                    crate::open_browser(&auth_url);
                    let _ = tx.send(AppEvent::Notice(format!("Sign in to the MCP server in the browser: {}", auth_url)));
                    None
                }
                Err(e) => return drop(tx.send(AppEvent::Error(e))),
            };
            let _ = tx.send(AppEvent::Mcp(mcp::list(&client, &project)));
            let event = match mcp::watch(&client, &project, &url, id.as_deref()) {
                Some(server) if server.state == "failed" => AppEvent::Error(format!(
                    "MCP server {} failed to connect: {}",
                    server.name,
                    server.error.unwrap_or_else(|| "no reason given".to_string())
                )),
                Some(server) => AppEvent::Notice(format!(
                    "MCP server {} connected with {} tool{}",
                    server.name,
                    server.tools,
                    if server.tools == 1 { "" } else { "s" }
                )),
                None if id.is_none() => AppEvent::Error(format!("MCP server {} wasn't signed in to; /mcp add it again to retry", url)),
                None => AppEvent::Warning(format!("MCP server {} is still connecting; /mcp shows when it's ready", url)),
            };
            let _ = tx.send(event);
            let _ = tx.send(AppEvent::Mcp(mcp::list(&client, &project)));
        });
    }

    fn remove_mcp(&mut self, server: mcp::Server) {
        let Some((project, _)) = self.project.clone() else { return };
        self.spawn(move |client, tx| {
            let _ = tx.send(match mcp::remove(&client, &project, &server.id) {
                Ok(()) => AppEvent::Notice(format!("MCP server {} removed", server.name)),
                Err(e) => AppEvent::Error(e),
            });
            let _ = tx.send(AppEvent::Mcp(mcp::list(&client, &project)));
        });
    }

    fn open_status(&mut self) {
        self.overlay = Overlay::Status(None);
        self.spawn(|client, tx| {
            let _ = tx.send(AppEvent::Status(crate::status_rows(&client)));
        });
    }

    fn open_usage(&mut self) {
        self.overlay = Overlay::Usage { loading: true, tokens: TokenStats::load() };
        self.load_usage();
    }

    /// ctrl+s: flip the project's web search switch, as the studio app's
    /// Cmd/Ctrl+Alt+S does.
    fn toggle_web_search(&mut self) {
        let Some((project, _)) = self.project.clone() else { return };
        let current = self.web_search;
        self.spawn(move |client, tx| {
            let current = match current {
                Some(on) => Ok(on),
                None => crate::web_search_enabled(&client, &project),
            };
            let _ = tx.send(match current.and_then(|on| crate::set_web_search(&client, &project, !on)) {
                Ok(enabled) => AppEvent::WebSearch { enabled, toggled: true },
                Err(e) => AppEvent::Error(e),
            });
        });
    }

    fn load_web_search(&self) {
        let Some((project, _)) = self.project.clone() else { return };
        self.spawn(move |client, tx| {
            if let Ok(enabled) = crate::web_search_enabled(&client, &project) {
                let _ = tx.send(AppEvent::WebSearch { enabled, toggled: false });
            }
        });
    }

    fn key_chat(&mut self, key: KeyEvent) {
        let hints = self.command_hints();
        if !hints.is_empty() {
            let selected = hints[self.hint_at.min(hints.len() - 1)].0;
            match key.code {
                KeyCode::Up => return self.hint_at = self.hint_at.saturating_sub(1),
                KeyCode::Down => return self.hint_at = (self.hint_at + 1).min(hints.len() - 1),
                KeyCode::Tab => {
                    self.input = format!("{} ", selected);
                    self.cursor = self.input.chars().count();
                    return;
                }
                KeyCode::Enter => {
                    self.input = selected.to_string();
                    return self.submit();
                }
                KeyCode::Esc => {
                    self.input.clear();
                    self.cursor = 0;
                    return;
                }
                _ => {}
            }
        }
        let before = self.input.clone();
        self.key_edit(key);
        if self.input != before {
            self.hint_at = 0;
        }
    }

    /// The slash commands matching what's typed so far, while it's still one
    /// word starting with `/`.
    fn command_hints(&self) -> Vec<(&'static str, &'static str)> {
        if self.token_entry || !self.input.starts_with('/') || self.input.contains(' ') {
            return Vec::new();
        }
        COMMANDS.iter().copied().filter(|(name, _)| name.starts_with(self.input.as_str())).collect()
    }

    fn key_edit(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        // Any other key in between makes the next Esc a first one again.
        let last_esc = self.last_esc.take();
        let esc_again = key.code == KeyCode::Esc && last_esc.is_some_and(|t| t.elapsed() < DOUBLE_ESC);
        match key.code {
            KeyCode::Esc if self.token_entry => {
                self.token_entry = false;
                self.input.clear();
                self.cursor = 0;
            }
            KeyCode::Esc if self.turn.is_some() => self.stop_turn(),
            // Esc twice clears the prompt, attachments and all.
            KeyCode::Esc if esc_again => {
                self.input.clear();
                self.attachments.clear();
                self.cursor = 0;
                self.history_at = None;
            }
            KeyCode::Esc if !self.input.is_empty() => self.last_esc = Some(Instant::now()),
            // A new line in the prompt: Shift+Enter where the terminal tells
            // it apart, Alt+Enter and Ctrl+J everywhere.
            KeyCode::Enter if key.modifiers.intersects(KeyModifiers::SHIFT | KeyModifiers::ALT) && !self.token_entry => {
                self.insert("\n")
            }
            KeyCode::Char('j') if ctrl && !self.token_entry => self.insert("\n"),
            KeyCode::Enter => self.submit(),
            KeyCode::Char('u') if ctrl => {
                self.input.clear();
                self.cursor = 0;
            }
            KeyCode::Char('a') if ctrl => self.cursor = 0,
            KeyCode::Char('e') if ctrl => self.cursor = self.input.chars().count(),
            KeyCode::Char('?') if self.input.is_empty() => self.overlay = Overlay::Help,
            KeyCode::Char(c) if !ctrl => {
                let at = self.byte_at(self.cursor);
                self.input.insert(at, c);
                self.cursor += 1;
            }
            KeyCode::Backspace if self.cursor > 0 => {
                // A chip goes as a whole, and its file with it.
                let before: String = self.input.chars().take(self.cursor).collect();
                if let Some(i) = self.attachments.iter().position(|a| before.ends_with(&a.label)) {
                    let label = self.attachments.remove(i).label;
                    let start = self.byte_at(self.cursor - label.chars().count());
                    self.input.replace_range(start..self.byte_at(self.cursor), "");
                    self.cursor -= label.chars().count();
                    return;
                }
                self.cursor -= 1;
                let at = self.byte_at(self.cursor);
                self.input.remove(at);
            }
            KeyCode::Delete if self.cursor < self.input.chars().count() => {
                let at = self.byte_at(self.cursor);
                self.input.remove(at);
            }
            KeyCode::Left => self.cursor = self.cursor.saturating_sub(1),
            KeyCode::Right => self.cursor = (self.cursor + 1).min(self.input.chars().count()),
            KeyCode::Home => self.cursor = 0,
            KeyCode::End => {
                self.cursor = self.input.chars().count();
                self.scroll_back = 0;
            }
            // The arrows move between the prompt's lines, and past its first
            // or last one step through earlier prompts.
            KeyCode::Up => {
                if !self.move_line(-1) {
                    self.recall(-1)
                }
            }
            KeyCode::Down => {
                if !self.move_line(1) {
                    self.recall(1)
                }
            }
            KeyCode::PageUp => self.scroll(PAGE_LINES as i32),
            KeyCode::PageDown => self.scroll(-(PAGE_LINES as i32)),
            _ => {}
        }
    }

    /// Scroll the conversation `lines` up (negative: down), no further than
    /// its first line or its end.
    fn scroll(&mut self, lines: i32) {
        let back = (self.scroll_back as i32 + lines).clamp(0, self.scroll_limit.get() as i32);
        self.scroll_back = back as u16;
    }

    /// A paste: dropped files become chips, anything else is typed in.
    fn paste(&mut self, text: &str) {
        if self.token_entry {
            return self.insert(text.trim());
        }
        match dropped_files(text) {
            Some(paths) => {
                for path in paths {
                    if !is_image(&path) && !is_document(&path) {
                        let name = path.file_name().map_or_else(|| path.display().to_string(), |n| n.to_string_lossy().into_owned());
                        self.entries.push(Entry::Error(format!(
                            "Can't attach {}: only PNG and JPEG images, and PDF, Word, PowerPoint, Excel, CSV, text, HTML and JSON documents",
                            name
                        )));
                        continue;
                    }
                    let kind = if is_image(&path) { "Image" } else { "Doc" };
                    let n = self.attachments.iter().filter(|a| a.label.starts_with(&format!("[{} ", kind))).count() + 1;
                    let label = format!("[{} {}]", kind, n);
                    let before = self.input.chars().take(self.cursor).last();
                    if before.is_some_and(|c| c != ' ') {
                        self.insert(" ");
                    }
                    self.insert(&label);
                    self.insert(" ");
                    self.attachments.push(Attachment { label, path });
                }
            }
            None => self.insert(&text.replace("\r\n", "\n").replace('\r', "\n")),
        }
    }

    /// Move the cursor to the same column one line up or down, if there is
    /// such a line.
    fn move_line(&mut self, step: isize) -> bool {
        let chars: Vec<char> = self.input.chars().collect();
        let starts: Vec<usize> =
            std::iter::once(0).chain(chars.iter().enumerate().filter(|(_, c)| **c == '\n').map(|(i, _)| i + 1)).collect();
        let row = starts.iter().rposition(|&s| s <= self.cursor).unwrap_or(0);
        let col = self.cursor - starts[row];
        let target = row as isize + step;
        if target < 0 || target as usize >= starts.len() {
            return false;
        }
        let start = starts[target as usize];
        let end = starts.get(target as usize + 1).map_or(chars.len(), |s| s - 1);
        self.cursor = (start + col).min(end);
        true
    }

    fn insert(&mut self, text: &str) {
        let at = self.byte_at(self.cursor);
        self.input.insert_str(at, text);
        self.cursor += text.chars().count();
    }

    fn byte_at(&self, chars: usize) -> usize {
        self.input.char_indices().nth(chars).map_or(self.input.len(), |(i, _)| i)
    }

    /// Step through earlier prompts, like a shell. Going back the cursor lands
    /// on a prompt's first line, so the next Up goes on back past a long one.
    fn recall(&mut self, step: isize) {
        if self.history.is_empty() {
            return;
        }
        let last = self.history.len() as isize;
        let at = self.history_at.map_or(last, |i| i as isize) + step;
        if at >= last {
            self.history_at = None;
            self.input.clear();
        } else {
            let at = at.max(0) as usize;
            self.history_at = Some(at);
            self.input = self.history[at].clone();
        }
        self.cursor = match self.input.find('\n') {
            Some(i) if step < 0 => self.input[..i].chars().count(),
            _ => self.input.chars().count(),
        };
    }

    fn submit(&mut self) {
        let input = self.input.trim().to_string();
        if self.token_entry {
            if input.is_empty() {
                return;
            }
            self.token_entry = false;
            self.input.clear();
            self.cursor = 0;
            return self.login(input);
        }
        if input.is_empty() {
            return;
        }
        let mut words = input.split_whitespace();
        let command = words.next().unwrap_or_default();
        match command {
            "/login" => match words.next() {
                None => self.browser_login(),
                Some("--token") => self.token_entry = true,
                Some(token) => self.login(token.to_string()),
            },
            "/logout" => {
                let project = self.project.clone();
                self.spawn(move |client, tx| {
                    let _ = tx.send(match crate::try_logout(&client) {
                        Ok(()) => AppEvent::Notice("Logged out".to_string()),
                        Err(e) => AppEvent::Error(e),
                    });
                    refresh_models(&client, &tx, project);
                    let _ = tx.send(AppEvent::Usage(crate::usage::fetch(&client)));
                })
            }
            "/quit" | "/exit" | "/q" | "exit" => self.quit = true,
            "/models" => self.open_models(),
            "/mcp" => match words.next() {
                None | Some("list") => self.open_mcp(),
                Some("add") => self.add_mcp(&words.collect::<Vec<_>>().join(" ")),
                // A bare URL adds it too.
                Some(url) if url.contains("://") => self.add_mcp(url),
                Some(other) => self.entries.push(Entry::Error(format!("Unknown /mcp {}; try /mcp add <url>", other))),
            },
            "/help" | "/?" => self.overlay = Overlay::Help,
            "/status" => self.open_status(),
            "/usage" => self.open_usage(),
            "/upgrade" => {
                crate::open_browser(crate::UPGRADE_URL);
                self.entries.push(Entry::Notice(format!("Opening {} in the browser", crate::UPGRADE_URL)));
            }
            // A clean slate: the conversation goes, and so does the agent's
            // memory of it, with a new session.
            "/clear" => {
                self.stop_turn();
                self.entries.clear();
                self.attachments.clear();
                self.session = chat::new_session_id();
                self.scroll_back = 0;
            }
            _ if command.starts_with('/') => {
                self.entries.push(Entry::Error(format!("Unknown command {}; try /help", command)));
            }
            _ => {
                // One turn at a time, and only once there's a project.
                if self.turn.is_some() || self.project.is_none() {
                    return;
                }
                // The chips still in the prompt are the files to send; the
                // agent reads each chip as its file's name.
                let files: Vec<Attachment> =
                    std::mem::take(&mut self.attachments).into_iter().filter(|a| input.contains(&a.label)).collect();
                let mut message = input.clone();
                for file in &files {
                    message = message.replace(&file.label, &file.name());
                }
                self.send_turn(input.clone(), message, files);
            }
        }
        // Never keep a pasted token in the history.
        let kept = if command == "/login" { command.to_string() } else { input.clone() };
        self.history.push(kept);
        self.history_at = None;
        self.input.clear();
        self.cursor = 0;
    }

    /// Sign in with `token`, then refresh the models panel: models behind a
    /// sign-in become available.
    fn login(&mut self, token: String) {
        let project = self.project.clone();
        self.spawn(move |client, tx| {
            let _ = tx.send(match crate::try_login(&client, &token) {
                Ok(who) => AppEvent::Notice(who),
                Err(e) => AppEvent::Error(e),
            });
            refresh_models(&client, &tx, project);
            let _ = tx.send(AppEvent::Usage(crate::usage::fetch(&client)));
        });
    }


    /// Sign in on app.smartloop.ai in the browser, then refresh the models
    /// panel as a token sign-in does.
    fn browser_login(&mut self) {
        let project = self.project.clone();
        self.spawn(move |client, tx| {
            let notify = tx.clone();
            let result = crate::browser_login(&client, |url| {
                let _ = notify.send(AppEvent::Notice(format!("Sign in in the browser: {}", url)));
            });
            let _ = tx.send(match result {
                Ok(who) => AppEvent::Notice(who),
                Err(e) => AppEvent::Error(e),
            });
            refresh_models(&client, &tx, project);
            let _ = tx.send(AppEvent::Usage(crate::usage::fetch(&client)));
        });
    }

    /// The models panel's keys; false for any other, which is typing.
    fn key_models(&mut self, key: KeyEvent) -> bool {
        let Overlay::Models { rows, selected } = &mut self.overlay else { return false };
        let count = rows.as_ref().and_then(|r| r.as_ref().ok()).map_or(0, Vec::len);
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Esc => self.overlay = Overlay::None,
            KeyCode::Up => *selected = selected.saturating_sub(1),
            KeyCode::Down => *selected = (*selected + 1).min(count.saturating_sub(1)),
            KeyCode::Char('r') if ctrl => self.load_models(),
            KeyCode::Enter => {
                let Some(row) = rows.as_ref().and_then(|r| r.as_ref().ok()).and_then(|r| r.get(*selected)) else {
                    return true;
                };
                let row = row.clone();
                if !row.accessible {
                    self.entries.push(Entry::Error(format!(
                        "{} needs a sign-in; /login first",
                        row.name
                    )));
                } else if row.enabled {
                    self.disable_model(row.name);
                } else {
                    self.enable_model(row.name);
                }
            }
            _ => return false,
        }
        true
    }

    /// The MCP panel's keys; false for any other, which is typing (a new
    /// server is added from the prompt with `/mcp add <url>`).
    fn key_mcp(&mut self, key: KeyEvent) -> bool {
        let Overlay::Mcp { rows, selected } = &mut self.overlay else { return false };
        let list = rows.as_ref().and_then(|r| r.as_ref().ok());
        let count = list.map_or(0, Vec::len);
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Esc => self.overlay = Overlay::None,
            KeyCode::Up => *selected = selected.saturating_sub(1),
            KeyCode::Down => *selected = (*selected + 1).min(count.saturating_sub(1)),
            KeyCode::Char('r') if ctrl => self.load_mcp(),
            KeyCode::Char('d') if ctrl => {
                if let Some(server) = list.and_then(|l| l.get(*selected)).cloned() {
                    self.remove_mcp(server);
                }
            }
            KeyCode::Delete => {
                if let Some(server) = list.and_then(|l| l.get(*selected)).cloned() {
                    self.remove_mcp(server);
                }
            }
            _ => return false,
        }
        true
    }

    // ----- drawing -------------------------------------------------------

    fn draw(&self, frame: &mut Frame) {
        let status = self.status_lines();
        // Command hints take the footer's place while a command is typed,
        // and an open panel takes it in the same way, under the prompt.
        let hints = self.hint_lines();
        let room = frame.area().height.saturating_sub(12);
        let under = match self.panel_height() {
            Some(height) => height.min(room),
            None => hints.len().max(1) as u16,
        };
        // A blank row at the top, and one between the conversation and the
        // status line, so neither sits cramped against its neighbor.
        let [_, body, _, status_area, input, footer] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Min(3),
            Constraint::Length(1),
            Constraint::Length(status.len().max(1) as u16),
            Constraint::Length(self.input_rows() + 2),
            Constraint::Length(under),
        ])
        .areas(frame.area());

        self.draw_conversation(frame, body);
        self.keep_shown(frame, body);
        frame.render_widget(Paragraph::new(status), status_area);
        self.draw_input(frame, input);
        match &self.overlay {
            Overlay::None if hints.is_empty() => self.draw_footer(frame, footer),
            Overlay::None => frame.render_widget(Paragraph::new(hints), footer),
            Overlay::Models { rows, selected } => self.draw_models(frame, footer, rows, *selected),
            Overlay::Mcp { rows, selected } => self.draw_mcp(frame, footer, rows, *selected),
            Overlay::Help => {
                let block = panel(&bar_hints(&[("esc", "close")]));
                frame.render_widget(Paragraph::new(help_lines()).block(block), footer);
            }
            Overlay::Status(rows) => {
                let block = panel(&bar_hints(&[("esc", "close")]));
                frame.render_widget(Paragraph::new(self.status_panel_lines(rows)).block(block), footer);
            }
            Overlay::Usage { loading, tokens } => {
                let block = panel(&bar_hints(&[("esc", "close")]));
                frame.render_widget(Paragraph::new(self.usage_panel_lines(*loading, tokens)).block(block), footer);
            }
        }
    }

    /// Rows an open panel needs under the prompt, border included.
    fn panel_height(&self) -> Option<u16> {
        let rows = match &self.overlay {
            Overlay::None => return None,
            Overlay::Models { rows: Some(Ok(list)), .. } => list.len(),
            Overlay::Models { .. } => 1,
            Overlay::Mcp { rows: Some(Ok(list)), .. } => list.len().max(1),
            Overlay::Mcp { .. } => 1,
            Overlay::Help => help_lines().len(),
            Overlay::Status(rows) => self.status_panel_lines(rows).len(),
            Overlay::Usage { loading, tokens } => self.usage_panel_lines(*loading, tokens).len(),
        };
        // And the row of keys under them.
        Some(rows as u16 + 1)
    }

    /// The live line above the prompt, like Claude Code's: what's running
    /// and for how long, plus a line per running download.
    fn status_lines(&self) -> Vec<Line<'static>> {
        // Muted, so it doesn't pull the eye from the reply.
        let spinner = Span::styled(format!("{} ", self.spinner()), dim());
        let mut lines = Vec::new();
        let working = |what: String, detail: String| {
            Line::from(vec![
                spinner.clone(),
                Span::styled(format!("{}…", what), dim()),
                Span::styled(format!(" ({})", detail), dim()),
            ])
        };
        match (&self.phase, &self.turn) {
            (Phase::Setup, _) if self.setup.failure.is_none() => {
                let (what, took) = match self.setup.active() {
                    Some(step) => (step.label.clone(), step.started.map_or(Duration::ZERO, |t| t.elapsed())),
                    None => ("Connecting to the agent".to_string(), self.setup.started.elapsed()),
                };
                lines.push(working(what, format_duration(took)));
            }
            (Phase::Connecting, _) => {
                let took = format_duration(self.setup.started.elapsed());
                lines.push(working("Connecting".to_string(), took));
            }
            (_, Some(turn)) => {
                let what = turn.activity.clone().unwrap_or_else(|| "Thinking".to_string());
                let mut detail = format_duration(turn.started.elapsed());
                if turn.tokens > 0 {
                    detail.push_str(&format!(" · {} tokens", turn.tokens));
                }
                lines.push(working(what, detail));
            }
            _ => {}
        }
        for d in self.downloads.iter().filter(|d| !d.finished) {
            let mut spans = vec![
                Span::styled("↓ ", dim()),
                Span::raw(format!("{} ", d.model)),
            ];
            match d.list.active() {
                Some(StepRow { bytes: Some((done, total)), .. }) => spans.extend(bar_spans(*done, *total, 20)),
                Some(step) => spans.push(Span::styled(format!("{}…", step.label), dim())),
                None => spans.push(Span::styled("starting…", dim())),
            }
            lines.push(Line::from(spans));
        }
        lines
    }

    /// The welcome card: the logo on the left, and on the right the
    /// project, the agent and the versions; first commands under them.
    ///
    /// ```text
    /// ╭───────────────────────────────────────────────────────────────────────────╮
    /// │                                                                           │
    /// │ █▀▄ █▀█ █▀▀ █▀▀ █▄  █ ▀█▀         project  general_chat                   │
    /// │ █▄▀ █▄█ █▄▄ ██▄ █ ▀▄█  █          agent    http://localhost:38540/v1      │
    /// │ Your private AI assistant         version  CLI 1.0.14 · agent 1.2.7       │
    /// │ by Smartloop · ? for shortcuts                                            │
    /// │                                                                           │
    /// │ Here are some commands to get you started                                 │
    /// │   /login     sign in for web search and more models                       │
    /// │   /models    enable, download or turn off models                          │
    /// │   /status    account, model, agent and versions                           │
    /// │                                                                           │
    /// ╰───────────────────────────────────────────────────────────────────────────╯
    /// ```
    fn welcome(&self, width: u16) -> Vec<Line<'static>> {
        let width = width as usize;
        let border = dim();
        let project = self.project.as_ref().map_or("…".to_string(), |(_, name)| name.clone());
        let right = [
            ("project", project),
            // The endpoint the chat and every call go to.
            ("agent", crate::api_url()),
            ("version", format!("CLI {} · agent {}", env!("CARGO_PKG_VERSION"), framework::VERSION)),
        ];
        // Styled per span: the rows are taken apart into spans below.
        let mut left: Vec<Line<'static>> = BANNER
            .iter()
            .map(|row| Line::from(Span::styled(*row, Style::new().fg(pink()))))
            .collect();
        left.push(Line::from(Span::styled("Your private AI assistant", dim())));
        left.push(Line::from(Span::styled("by Smartloop · ? for shortcuts", dim())));
        let left_width = left.iter().map(Line::width).max().unwrap_or(0) + 4;

        let row = |label: &str, value: &str| {
            Line::from(vec![
                Span::styled(format!("{:<9}", label), dim()),
                Span::raw(value.to_string()),
            ])
        };
        // Room inside the border at most; the card itself is only as wide as
        // what it holds.
        let inner = width.saturating_sub(4);
        // Two columns when there's room, else the details under the logo.
        let body: Vec<Line<'static>> = if inner >= left_width + 30 {
            (0..left.len().max(right.len()))
                .map(|i| {
                    let mut spans = left.get(i).cloned().unwrap_or_default().spans;
                    let used: usize = spans.iter().map(|s| s.width()).sum();
                    spans.push(Span::raw(" ".repeat(left_width.saturating_sub(used))));
                    if let Some((label, value)) = right.get(i) {
                        let room = inner.saturating_sub(left_width + 9);
                        spans.extend(row(label, &truncate(value, room)).spans);
                    }
                    Line::from(spans)
                })
                .collect()
        } else {
            let mut body = left;
            body.push(Line::default());
            body.extend(right.iter().map(|(l, v)| row(l, &truncate(v, inner.saturating_sub(9)))));
            body
        };

        // First commands, under the logo and details, when there's room.
        let mut body = body;
        let tips = getting_started();
        if tips.iter().all(|t| t.width() <= inner) {
            body.push(Line::default());
            body.extend(tips);
        }

        // Shrunk to its content, with two columns of margin on the right.
        let inner = (body.iter().map(Line::width).max().unwrap_or(0) + 2).min(inner);
        let rule = "─".repeat(inner + 2);
        let mut lines = vec![Line::styled(format!("╭{}╮", rule), border)];
        let blank = Line::default();
        for line in std::iter::once(&blank).chain(body.iter()).chain(std::iter::once(&blank)) {
            let used = line.width().min(inner);
            let mut spans = vec![Span::styled("│ ", border)];
            spans.extend(line.spans.iter().cloned());
            spans.push(Span::raw(" ".repeat(inner - used)));
            spans.push(Span::styled(" │", border));
            lines.push(Line::from(spans));
        }
        lines.push(Line::styled(format!("╰{}╯", rule), border));
        lines
    }

    /// Remember the conversation as drawn, for copying, and highlight the
    /// selection over it.
    fn keep_shown(&self, frame: &mut Frame, area: Rect) {
        let buffer = frame.buffer_mut();
        let rows = (area.top()..area.bottom())
            .map(|y| (area.left()..area.right()).map(|x| buffer[(x, y)].symbol().to_string()).collect())
            .collect();
        *self.shown.borrow_mut() = (area, rows);
        if let Some(selection) = &self.selection {
            for (y, from, to) in selection.rows(area) {
                for x in from..=to {
                    buffer[(x, y)].set_style(Style::new().add_modifier(Modifier::REVERSED));
                }
            }
        }
    }

    fn draw_conversation(&self, frame: &mut Frame, area: Rect) {
        // The welcome card opens the conversation, like a first message, and
        // scrolls away with it.
        let mut lines = self.welcome(area.width);
        if !self.setup.steps.is_empty() {
            lines.push(Line::default());
            lines.extend(self.setup.card(self.ticks));
        }
        if let Some(e) = &self.setup.failure {
            lines.push(Line::default());
            lines.push(error_line(e));
        }
        for entry in &self.entries {
            lines.push(Line::default());
            match entry {
                Entry::User(text) => {
                    for (i, line) in text.lines().enumerate() {
                        let mark = if i == 0 { "> " } else { "  " };
                        lines.push(Line::from(vec![
                            Span::styled(mark, dim()),
                            Span::styled(line.to_string(), dim()),
                        ]));
                    }
                }
                Entry::Reply(reply) => reply_lines(reply, &mut lines),
                Entry::Notice(text) => lines.push(Line::from(vec![
                    Span::styled("■ ", Style::new().fg(Color::Green)),
                    Span::raw(text.clone()),
                ])),
                Entry::Warning(text) => lines.push(Line::from(vec![
                    Span::styled("■ ", Style::new().fg(Color::Yellow)),
                    Span::styled(text.clone(), dim()),
                ])),
                Entry::Error(text) => lines.push(error_line(text)),
            }
        }

        let paragraph = Paragraph::new(lines).wrap(Wrap { trim: false });
        let total = paragraph.line_count(area.width) as u16;
        // Anchored to the bottom, just above the prompt: a short conversation
        // sits low and grows upward, as in Claude Code.
        let area = Rect { y: area.y + area.height.saturating_sub(total), height: area.height.min(total), ..area };
        let bottom = total.saturating_sub(area.height);
        self.scroll_limit.set(bottom);
        let top = bottom.saturating_sub(self.scroll_back);
        frame.render_widget(paragraph.scroll((top, 0)), area);
    }

    /// Lines the prompt shows: one per line typed, up to `PROMPT_LINES`.
    fn input_rows(&self) -> u16 {
        (self.input.split('\n').count() as u16).clamp(1, PROMPT_LINES)
    }

    fn draw_input(&self, frame: &mut Frame, area: Rect) {
        // Between two rules across the screen, as in Claude Code.
        let block = Block::new().borders(Borders::TOP | Borders::BOTTOM).border_style(dim());
        let inner = block.inner(area);
        let ready = matches!(self.phase, Phase::Ready) && self.project.is_some();
        let label = if self.token_entry { "token: " } else { "> " };
        let prompt = Span::styled(label, Style::new().fg(pink()).bold());
        // A token shows as dots.
        let input = if self.token_entry { "•".repeat(self.input.chars().count()) } else { self.input.clone() };
        let line = if self.token_entry && input.is_empty() {
            Line::from(vec![prompt, Span::styled("paste your Smartloop token", dim())])
        } else if input.is_empty() && !ready {
            Line::from(vec![prompt, Span::styled("waiting for setup…", dim())])
        } else {
            // One row per line typed. The cursor's line shows its tail when
            // it's too long, so the cursor stays in view; and when there are
            // more lines than rows, the rows follow the cursor.
            let indent = label.len() as u16;
            let room = inner.width.saturating_sub(indent + 1) as usize;
            let before: String = input.chars().take(self.cursor).collect();
            let row = before.matches('\n').count();
            let col_text = before.rsplit('\n').next().unwrap_or_default();
            let lines: Vec<&str> = input.split('\n').collect();
            let first = (row + 1).saturating_sub(inner.height as usize);
            let mut shown = Vec::new();
            for (i, text) in lines.iter().enumerate().skip(first).take(inner.height as usize) {
                let mark = if i == 0 { prompt.clone() } else { Span::raw(" ".repeat(indent as usize)) };
                let offset = if i == row { Span::raw(col_text).width().saturating_sub(room) } else { 0 };
                let visible: String = text.chars().skip(offset).collect();
                let mut spans = vec![mark, Span::raw(visible)];
                // Until something's typed, say what the prompt takes.
                if input.is_empty() && !self.token_entry {
                    spans.push(Span::styled("Ask anything, or drop a file to attach it", dim()));
                }
                shown.push(Line::from(spans));
                if i == row && matches!(self.overlay, Overlay::None) {
                    let x = inner.x + indent + Span::raw(col_text).width().saturating_sub(offset) as u16;
                    let y = inner.y + (i - first) as u16;
                    frame.set_cursor_position((x.min(inner.right().saturating_sub(1)), y));
                }
            }
            return frame.render_widget(Paragraph::new(shown).block(block), area);
        };
        frame.render_widget(Paragraph::new(line).block(block), area);
    }

    /// `  /models     models: enable, disable, download`, one per matching
    /// command, the highlighted one in pink.
    fn hint_lines(&self) -> Vec<Line<'static>> {
        let hints = self.command_hints();
        let at = self.hint_at.min(hints.len().saturating_sub(1));
        hints
            .iter()
            .enumerate()
            .map(|(i, (name, what))| {
                let (mark, style) = if i == at {
                    ("› ", Style::new().fg(pink()).bold())
                } else {
                    ("  ", dim())
                };
                Line::from(vec![
                    Span::styled(format!("{}{:<14}", mark, name), style),
                    Span::styled(what.to_string(), if i == at { Style::new() } else { dim() }),
                ])
            })
            .collect()
    }

    /// Key hints under the prompt.
    fn draw_footer(&self, frame: &mut Frame, area: Rect) {
        let mut hints = vec![Span::raw("  ")];
        if let Some((chars, _)) = self.copied.filter(|(_, at)| at.elapsed() < Duration::from_secs(2)) {
            hints.push(Span::styled("✓ ", Style::new().fg(Color::Green)));
            hints.push(Span::styled(format!("Copied {} characters", chars), dim()));
        } else if self.scroll_back > 0 {
            hints.push(Span::styled(format!("↓ {} lines below  ", self.scroll_back), dim()));
            hints.extend(key_hints(&[("pgdn", "scroll down"), ("end", "latest")]));
        } else if self.token_entry {
            hints.extend(key_hints(&[("enter", "sign in"), ("esc", "cancel")]));
        } else if self.last_esc.is_some_and(|t| t.elapsed() < DOUBLE_ESC) {
            hints.extend(key_hints(&[("esc", "again to clear")]));
        } else if let Some(warning) = self.search_warning(area.width.saturating_sub(2) as usize) {
            // Takes the key hints' place, so a narrow terminal can't hide it.
            hints.extend(warning);
        } else {
            // The rest live under help, as in Claude Code.
            hints.extend(key_hints(&[("enter", "send"), ("esc", "interrupt"), ("?", "shortcuts")]));
        }
        frame.render_widget(Paragraph::new(Line::from(hints)), area);
    }

    /// From 70% of the month's web searches, the footer says how much of the
    /// budget is gone, and where to get more. It stays one row: the longest
    /// wording that fits in `width` cells, down to a few words.
    fn search_warning(&self, width: usize) -> Option<Vec<Span<'static>>> {
        let usage = self.search.as_ref().filter(|u| u.nearly_exhausted())?;
        let (percent, left) = (usage.percent_used(), usage.remaining().unwrap_or_default());
        let searches = if left == 1 { "search" } else { "searches" };
        let wordings = if usage.exhausted() {
            vec![
                "You are out of AI search credits for this month".to_string(),
                "Out of AI search credits".to_string(),
                "Out of search credits".to_string(),
            ]
        } else {
            vec![
                format!("You have used {}% of your allocated AI search budget · {} {} left", percent, left, searches),
                format!("{}% of AI search budget used · {} left", percent, left),
                format!("AI search {}% used", percent),
            ]
        };
        let upgrade = if usage.plan == "free" { " · /upgrade".chars().count() } else { 0 };
        let text = wordings
            .iter()
            .find(|w| 2 + w.chars().count() + upgrade <= width)
            .unwrap_or(&wordings[wordings.len() - 1])
            .clone();
        let mut spans = vec![Span::styled("⚠ ", Style::new().fg(Color::Yellow)), Span::styled(text, Style::new().fg(Color::Yellow))];
        if upgrade > 0 {
            spans.push(Span::styled(" · ", dim()));
            spans.push(Span::styled("/upgrade", key_style()));
        }
        Some(spans)
    }

    /// The project's models as a list, like Claude Code's model picker:
    /// the selected one marked, each with its state and what it can do.
    fn draw_models(&self, frame: &mut Frame, area: Rect, rows: &Option<Result<Vec<ModelRow>, String>>, selected: usize) {
        let block = panel(&bar_hints(&[("↑↓", "select"), ("enter", "enable/disable"), ("ctrl+r", "refresh"), ("esc", "close")]));
        frame.render_widget(Clear, area);
        let list = match rows {
            None => return frame.render_widget(Paragraph::new(format!(" Loading models {}", self.spinner())).block(block), area),
            Some(Err(e)) => return frame.render_widget(Paragraph::new(e.clone()).block(block).wrap(Wrap { trim: false }), area),
            Some(Ok(list)) => list,
        };
        let name_width = list.iter().map(|m| m.name.chars().count()).max().unwrap_or(0) + 2;
        let lines: Vec<Line> = list
            .iter()
            .enumerate()
            .map(|(i, m)| {
                let chosen = i == selected;
                let mut spans = vec![
                    Span::styled(if chosen { "› " } else { "  " }, Style::new().fg(pink()).bold()),
                    Span::styled(
                        format!("{:<w$}", m.name, w = name_width),
                        if chosen { Style::new().fg(pink()).bold() } else { Style::new() },
                    ),
                ];
                let downloading = self.downloads.iter().find(|d| d.model == m.name && !d.finished);
                let state = match downloading.and_then(|d| d.list.active()) {
                    Some(StepRow { bytes: Some((done, total)), .. }) if *total > 0 => {
                        Span::styled(format!("downloading {}%", done * 100 / total), Style::new().fg(pink()))
                    }
                    Some(_) => Span::styled(format!("downloading {}", self.spinner()), Style::new().fg(pink())),
                    None if m.enabled => Span::styled("✓ enabled", Style::new().fg(Color::Green)),
                    None if m.downloaded => Span::styled("downloaded", dim()),
                    None => Span::styled("not downloaded", dim()),
                };
                let state_width = state.width();
                spans.push(state);
                spans.push(Span::raw(" ".repeat(16usize.saturating_sub(state_width))));
                spans.push(Span::styled(m.capabilities.clone(), dim()));
                if !m.accessible {
                    spans.push(Span::styled("  · sign in to use", Style::new().fg(Color::Yellow)));
                }
                Line::from(spans)
            })
            .collect();
        frame.render_widget(Paragraph::new(lines).block(block), area);
    }

    fn draw_mcp(&self, frame: &mut Frame, area: Rect, rows: &Option<Result<Vec<mcp::Server>, String>>, selected: usize) {
        let block = panel(&bar_hints(&[("↑↓", "select"), ("ctrl+d", "remove"), ("ctrl+r", "refresh"), ("esc", "close")]));
        frame.render_widget(Clear, area);
        let list = match rows {
            None => return frame.render_widget(Paragraph::new(format!(" Loading MCP servers {}", self.spinner())).block(block), area),
            Some(Err(e)) => return frame.render_widget(Paragraph::new(e.clone()).block(block).wrap(Wrap { trim: false }), area),
            Some(Ok(list)) if list.is_empty() => {
                let line = Line::from(vec![
                    Span::styled("No MCP servers yet; add one with ", dim()),
                    Span::styled("/mcp add <url>", key_style()),
                ]);
                return frame.render_widget(Paragraph::new(line).block(block), area);
            }
            Some(Ok(list)) => list,
        };
        let name_width = list.iter().map(|s| s.name.chars().count()).max().unwrap_or(0).min(28) + 2;
        let lines: Vec<Line> = list
            .iter()
            .enumerate()
            .map(|(i, s)| {
                let chosen = i == selected;
                let mut spans = vec![
                    Span::styled(if chosen { "› " } else { "  " }, Style::new().fg(pink()).bold()),
                    Span::styled(
                        format!("{:<w$}", truncate(&s.name, name_width - 2), w = name_width),
                        if chosen { Style::new().fg(pink()).bold() } else { Style::new() },
                    ),
                ];
                let state = match s.state.as_str() {
                    "provisioning" => Span::styled(format!("connecting {}", self.spinner()), Style::new().fg(pink())),
                    "failed" => Span::styled("✗ failed", Style::new().fg(Color::Red)),
                    _ => Span::styled(
                        format!("✓ {} tool{}", s.tools, if s.tools == 1 { "" } else { "s" }),
                        Style::new().fg(Color::Green),
                    ),
                };
                let state_width = state.width();
                spans.push(state);
                spans.push(Span::raw(" ".repeat(16usize.saturating_sub(state_width))));
                match (&s.error, s.state.as_str()) {
                    (Some(error), "failed") => spans.push(Span::styled(error.clone(), Style::new().fg(Color::Red))),
                    _ => spans.push(Span::styled(s.location.clone(), dim())),
                }
                Line::from(spans)
            })
            .collect();
        frame.render_widget(Paragraph::new(lines).block(block), area);
    }

    /// Who's signed in, the project and session, the agent, its model and
    /// process, and the versions.
    fn status_panel_lines(&self, rows: &Option<Vec<(String, String)>>) -> Vec<Line<'static>> {
        let project = self.project.as_ref().map_or("…".to_string(), |(_, name)| name.clone());
        let mut all = vec![("project".to_string(), project), ("session".to_string(), self.session.clone())];
        match rows {
            Some(rows) => all.extend(rows.iter().cloned()),
            None => all.push(("agent".to_string(), format!("asking the agent {}", self.spinner()))),
        }
        all.into_iter()
            .map(|(label, value)| Line::from(vec![Span::styled(format!(" {:<10}", label), dim()), Span::raw(value)]))
            .collect()
    }

    /// `/usage`: how many web searches are left this month, as a bar that
    /// empties as they're spent, then the week's tokens and what they'd
    /// have cost on a hosted model, as on the web app's dashboard.
    ///
    /// ```text
    ///  Web search  ███████████████████████░░░░░░░  38 left of 50
    ///              12 used · free plan · resets 2026-11-01
    ///              /upgrade to Pro for 1,000 searches a month
    ///
    ///  Tokens      45,210 in the last 7 days  ▁▁▃▂█▅▁
    ///              40,120 in · 5,090 out · on this machine
    ///
    ///  Cost saved  $0.1512 in the last 7 days
    ///              vs. a hosted frontier model at $2.50/M in, $10/M out
    /// ```
    fn usage_panel_lines(&self, loading: bool, tokens: &TokenStats) -> Vec<Line<'static>> {
        let mut lines = self.search_usage_lines(loading);
        let label = |text: &str| Span::styled(format!(" {:<12}", text), dim());
        let indent = || Span::raw(format!(" {:<12}", ""));
        let days = format!(" in the last {} days", crate::usage::TOKEN_DAYS);
        // A blank line between sections, so each reads on its own.
        lines.push(Line::default());
        lines.push(Line::from(vec![
            label("Tokens"),
            Span::styled(crate::usage::thousands(tokens.total()), key_style()),
            Span::styled(days.clone(), dim()),
            Span::raw(format!("  {}", tokens.sparkline())),
        ]));
        lines.push(Line::from(vec![
            indent(),
            Span::styled(
                format!(
                    "{} in · {} out · on this machine",
                    crate::usage::thousands(tokens.prompt()),
                    crate::usage::thousands(tokens.completion())
                ),
                dim(),
            ),
        ]));
        lines.push(Line::default());
        lines.push(Line::from(vec![
            label("Cost saved"),
            Span::styled(format!("${:.4}", tokens.saved()), key_style()),
            Span::styled(days, dim()),
        ]));
        lines.push(Line::from(vec![
            indent(),
            Span::styled("vs. a hosted frontier model at $2.50/M in, $10/M out", dim()),
        ]));
        lines
    }

    fn search_usage_lines(&self, loading: bool) -> Vec<Line<'static>> {
        let label = || Span::styled(format!(" {:<12}", "Web search"), dim());
        let indent = || Span::raw(format!(" {:<12}", ""));
        let switch = (self.web_search == Some(false)).then(|| {
            Line::from(vec![indent(), Span::styled("off for this project · ctrl+s turns it on", dim())])
        });
        let Some(usage) = self.search.as_ref() else {
            let text = if loading {
                format!("asking api.smartloop.ai {}", self.spinner())
            } else {
                "sign in with /login to see your usage".to_string()
            };
            return std::iter::once(Line::from(vec![label(), Span::styled(text, dim())])).chain(switch).collect();
        };
        let mut lines = Vec::new();
        match (usage.limit, usage.remaining()) {
            (Some(limit), Some(left)) => {
                // The terminal's own text color, so the bar reads on a light
                // theme as well as a dark one.
                let fraction = if limit == 0 { 0.0 } else { left as f64 / limit as f64 };
                lines.push(Line::from(vec![
                    label(),
                    Span::raw(progress::bar(fraction, USAGE_BAR)),
                    Span::styled(format!("  {} left", left), key_style()),
                    Span::styled(format!(" of {}", limit), dim()),
                ]));
            }
            _ => lines.push(Line::from(vec![label(), Span::styled("unlimited", key_style())])),
        }
        lines.push(Line::from(vec![indent(), Span::styled(usage.detail(), dim())]));
        if usage.plan == "free" {
            lines.push(Line::from(vec![
                indent(),
                Span::styled("/upgrade", key_style()),
                Span::styled(" to Pro for 1,000 searches a month", dim()),
            ]));
        }
        lines.extend(switch);
        lines
    }

    fn spinner(&self) -> &'static str {
        SPINNER[self.ticks % SPINNER.len()]
    }
}

/// A reply as Claude Code lays one out: the answer under a `■` (square, like the setup blocks), its
/// sources, then `⎿` with the model that answered and how it went.
fn reply_lines(reply: &Reply, lines: &mut Vec<Line<'static>>) {
    for warning in &reply.warnings {
        lines.push(Line::from(vec![
            Span::styled("■ ", Style::new().fg(Color::Yellow)),
            Span::styled(warning.clone(), dim()),
        ]));
    }
    for (i, line) in reply.text.trim_end().lines().enumerate() {
        let mark = if i == 0 { Span::raw("■ ") } else { Span::raw("  ") };
        lines.push(Line::from(vec![mark, Span::raw(line.to_string())]));
    }
    if !reply.citations.is_empty() {
        lines.push(Line::default());
        lines.push(Line::styled("  References", dim()));
        for (i, label) in reply.citations.iter().enumerate() {
            lines.push(Line::from(vec![Span::styled(format!("  [{}] ", i + 1), dim()), Span::raw(label.clone())]));
        }
    }
    match (&reply.ended, &reply.stats) {
        (Some(why), _) => lines.push(Line::from(vec![
            Span::styled("  ⎿  ", dim()),
            Span::styled(why.clone(), Style::new().fg(Color::Red)),
        ])),
        (None, Some(stats)) => {
            let mut summary = stats.summary().replace(", ", " · ");
            if let Some(model) = &reply.model {
                summary = format!("{} · {}", model, summary);
            }
            lines.push(Line::styled(format!("  ⎿  {}", summary), dim()));
        }
        (None, None) => {}
    }
}

/// The model in the agent's "Selected sl-mini for the response".
fn selected_model(message: &str) -> Option<String> {
    let name = message.strip_prefix("Selected ")?.split_whitespace().next()?;
    Some(name.to_string())
}

fn error_line(text: &str) -> Line<'static> {
    Line::from(vec![
        Span::styled("■ ", Style::new().fg(Color::Red)),
        Span::styled("Error: ", Style::new().fg(Color::Red).bold()),
        Span::raw(text.to_string()),
    ])
}

/// Re-read the project's models: signing in or out changes which ones are
/// accessible.
fn refresh_models(client: &Client, tx: &UnboundedSender<AppEvent>, project: Option<(String, String)>) {
    if let Some((id, _)) = project {
        let _ = tx.send(AppEvent::Models(crate::model_rows(client, &id)));
    }
}

/// A mouse selection: where the drag started and where it is now.
struct Selection {
    anchor: (u16, u16),
    head: (u16, u16),
}

impl Selection {
    /// Each selected row as (y, first x, last x), the way a terminal selects:
    /// from the start to the row's end, whole rows between, then up to the
    /// end.
    fn rows(&self, area: Rect) -> impl Iterator<Item = (u16, u16, u16)> {
        let (a, b) = (self.anchor, self.head);
        let ((x0, y0), (x1, y1)) = if (a.1, a.0) <= (b.1, b.0) { (a, b) } else { (b, a) };
        let (left, right) = (area.left(), area.right().saturating_sub(1));
        (y0..=y1).filter(move |y| *y >= area.top() && *y < area.bottom()).map(move |y| {
            let from = if y == y0 { x0 } else { left };
            let to = if y == y1 { x1 } else { right };
            (y, from.clamp(left, right), to.clamp(left, right))
        })
    }
}

/// `at`, moved inside `area`.
fn clamp_to(area: Rect, at: (u16, u16)) -> (u16, u16) {
    (
        at.0.clamp(area.left(), area.right().saturating_sub(1)),
        at.1.clamp(area.top(), area.bottom().saturating_sub(1)),
    )
}

/// Standard base64, for OSC 52.
fn base64(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let n = chunk.iter().enumerate().fold(0u32, |n, (i, b)| n | (*b as u32) << (16 - 8 * i));
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(TABLE[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// The welcome card's first steps: the commands to reach for first.
fn getting_started() -> Vec<Line<'static>> {
    let command = |name: &str, what: &str| {
        Line::from(vec![
            Span::raw("  "),
            Span::styled(format!("{:<11}", name), key_style()),
            Span::styled(what.to_string(), dim()),
        ])
    };
    vec![
        Line::from(Span::styled("Here are some commands to get you started", key_style())),
        command("/login", "sign in for web search and more models"),
        command("/models", "enable, download or turn off models"),
        command("/status", "account, model, agent and versions"),
    ]
}

/// A file dropped on the prompt.
#[derive(Debug, Clone)]
struct Attachment {
    /// Its chip in the prompt, e.g. `[Image 1]`.
    label: String,
    path: std::path::PathBuf,
}

impl Attachment {
    fn name(&self) -> String {
        self.path.file_name().map_or_else(|| self.path.display().to_string(), |n| n.to_string_lossy().into_owned())
    }
}

/// The files in a paste, when it's nothing but paths to existing files, as
/// a terminal pastes a dropped file: quoted, or with spaces escaped
/// (`My\ Report.pdf`), or as a `file://` URL.
fn dropped_files(text: &str) -> Option<Vec<std::path::PathBuf>> {
    let mut paths = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    let mut chars = text.trim().chars().peekable();
    while let Some(c) = chars.next() {
        match (quote, c) {
            (None, '\\') => current.extend(chars.next()),
            (None, '\'' | '"') => quote = Some(c),
            (Some(q), c) if c == q => quote = None,
            (None, c) if c.is_whitespace() => {
                if !current.is_empty() {
                    paths.push(std::mem::take(&mut current));
                }
            }
            (_, c) => current.push(c),
        }
    }
    if !current.is_empty() {
        paths.push(current);
    }
    let files: Vec<std::path::PathBuf> = paths
        .into_iter()
        .map(|p| {
            let p = p.strip_prefix("file://").map(|rest| rest.replace("%20", " ")).unwrap_or(p);
            match p.strip_prefix("~/") {
                Some(rest) => std::env::var("HOME").map(|h| format!("{}/{}", h, rest)).unwrap_or(p.clone()),
                None => p,
            }
        })
        .map(std::path::PathBuf::from)
        .collect();
    (!files.is_empty() && files.iter().all(|f| f.is_file())).then_some(files)
}

fn is_image(path: &std::path::Path) -> bool {
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or_default().to_ascii_lowercase();
    matches!(ext.as_str(), "png" | "jpg" | "jpeg")
}

/// The documents the agent reads as text; it has a parser for these and
/// stores anything else as bytes the model never sees.
fn is_document(path: &std::path::Path) -> bool {
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or_default().to_ascii_lowercase();
    matches!(ext.as_str(), "pdf" | "docx" | "pptx" | "xlsx" | "csv" | "txt" | "html" | "htm" | "json")
}

/// The slash commands, for hints as they're typed.
/// `/exit` and `/q` also quit, but aren't hinted.
const COMMANDS: [(&str, &str); 10] = [
    ("/help", "shortcuts and commands"),
    ("/login", "sign in in the browser (--token to paste one)"),
    ("/logout", "log out"),
    ("/models", "enable, disable and download models"),
    ("/mcp", "MCP servers; /mcp add <url> to connect one"),
    ("/status", "account, model, agent and versions"),
    ("/usage", "web searches left, tokens and cost saved"),
    ("/upgrade", "Pro plan: 1,000 web searches a month"),
    ("/clear", "clear the chat and start a new session"),
    ("/quit", "quit"),
];

/// Every key and command, for the help panel.
fn help_lines() -> Vec<Line<'static>> {
    let rows = [
        ("[?] /help", "these shortcuts"),
        ("[enter]", "send the prompt"),
        ("[shift+enter] [alt+enter]", "new line (also [ctrl+j])"),
        ("[esc]", "interrupt the reply"),
        ("[esc] [esc]", "clear the prompt"),
        ("[↑] [↓]", "earlier prompts, past the prompt's first or last line"),
        ("wheel [pgup] [pgdn]", "scroll the conversation"),
        ("drag", "select and copy to the clipboard"),
        ("drop a file", "attach an image or document as [Image 1] or [Doc 1]"),
        ("[ctrl+o] /models", "models: enable, disable, download"),
        ("[ctrl+s]", "turn web search on or off for the project"),
        ("/mcp", "MCP servers: list, remove"),
        ("/mcp add <url>", "connect an MCP server by its URL"),
        ("/status", "account, model, agent and versions"),
        ("/login", "sign in in the browser (--token to paste one)"),
        ("/logout", "log out"),
        ("/usage", "web searches left, tokens and cost saved"),
        ("/upgrade", "Pro plan: 1,000 web searches a month"),
        ("/clear", "clear the chat and start a new session"),
        ("[ctrl+c] /quit", "quit"),
    ];
    rows.iter()
        .map(|(key, what)| Line::from(vec![Span::styled(format!(" {:<27}", key), key_style()), Span::raw(*what)]))
        .collect()
}

/// A key in a hint: the terminal's own text color, bold, so it reads as
/// white on a dark theme and dark on a light one (`Color::White` all but
/// vanishes on a light background).
fn key_style() -> Style {
    Style::new().add_modifier(Modifier::BOLD)
}

/// `[ctrl+o] models  [esc] interrupt`: each key as a bracketed cap, then
/// what it does.
fn key_hints(keys: &[(&str, &str)]) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    for (i, (key, what)) in keys.iter().enumerate() {
        if i > 0 {
            spans.push(Span::raw("  "));
        }
        spans.push(Span::styled(format!("[{}]", key), key_style()));
        spans.push(Span::styled(format!(" {}", what), dim()));
    }
    spans
}

/// Key hints for a panel's bottom border, padded off its corner.
fn bar_hints(keys: &[(&str, &str)]) -> Vec<Span<'static>> {
    let mut spans = vec![Span::raw(" ")];
    spans.extend(key_hints(keys));
    spans.push(Span::raw(" "));
    spans
}

/// A panel in the dropdown under the prompt, as Claude Code draws them: no
/// border or title (the prompt's rule sets it off, and the command just
/// typed names it), the rows indented, and its keys on the last row.
fn panel(hint: &[Span<'static>]) -> Block<'static> {
    Block::new()
        .title_bottom(Line::from(hint.to_vec()).right_aligned())
        .padding(Padding::left(1))
}

fn dim() -> Style {
    Style::new().add_modifier(Modifier::DIM)
}

/// Smartloop brand pink (#e55d9c), in 24-bit color where the terminal
/// supports it and the nearest 256-color shade elsewhere.
fn pink() -> Color {
    let truecolor = std::env::var("COLORTERM")
        .map(|v| v.contains("truecolor") || v.contains("24bit"))
        .unwrap_or(false);
    if truecolor { Color::Rgb(229, 93, 156) } else { Color::Indexed(169) }
}

fn truncate(label: &str, max: usize) -> String {
    if label.chars().count() <= max {
        return label.to_string();
    }
    let kept: String = label.chars().take(max - 1).collect();
    format!("{}…", kept)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn screen(app: &App, width: u16, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|f| app.draw(f)).unwrap();
        let buffer = terminal.backend().buffer();
        (0..height)
            .map(|y| (0..width).map(|x| buffer[(x, y)].symbol()).collect::<String>().trim_end().to_string())
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn app() -> App {
        App::new(Client::new(), None, None, Some("cli-test".to_string()))
    }

    #[test]
    fn login_masks_the_token_and_keeps_it_out_of_history() {
        let mut app = app();
        app.phase = Phase::Ready;
        app.project = Some(("p1".into(), "Default".into()));
        app.input = "/login --token".into();
        app.submit();
        assert!(app.token_entry);
        app.input = "sl_secret".into();
        app.cursor = 9;
        let text = screen(&app, 80, 24);
        assert!(text.contains("token: •••••••••"), "{}", text);
        assert!(!text.contains("sl_secret"), "{}", text);
        assert!(text.contains("[enter] sign in  [esc] cancel"), "{}", text);
        app.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(!app.token_entry && app.input.is_empty());
        assert_eq!(app.history, vec!["/login".to_string()]);
    }

    #[test]
    fn the_banner_opens_the_conversation_and_scrolls_away() {
        let mut app = app();
        app.phase = Phase::Ready;
        app.project = Some(("p1".into(), "Default".into()));
        let text = screen(&app, 80, 24);
        let rows: Vec<&str> = text.lines().collect();
        // Short: the banner sits just above the prompt.
        let banner = rows.iter().position(|r| r.contains("█▀▄ █▀█ █▀▀")).expect(&text);
        assert!(rows[banner].contains("project  Default"), "{}", text);
        assert!(rows[banner + 1].contains("agent    http://localhost:"), "{}", text);
        assert!(rows[banner + 1].contains("/v1"), "{}", text);
        assert!(rows[banner + 2].contains(&format!("version  CLI {} · agent", env!("CARGO_PKG_VERSION"))), "{}", text);
        assert!(rows[banner + 2].contains("Your private AI assistant"), "{}", text);
        assert!(rows[banner + 3].contains("by Smartloop"), "{}", text);
        // No account row (the /status tip mentions the word, not a value).
        assert!(!text.contains("account  "), "{}", text);
        // Only as wide as what it holds, not the screen.
        let wide = screen(&app, 120, 24);
        let top = wide.lines().find(|r| r.starts_with('╭')).expect(&wide);
        assert!(top.ends_with('╮') && top.chars().count() < 100, "{}", wide);
        assert!(rows[banner + 5].contains("Here are some commands to get you started"), "{}", text);
        assert!(rows[banner + 6].contains("/login"), "{}", text);
        assert!(!text.contains("search the web"), "{}", text);
        assert!(rows[banner - 2].starts_with('╭') && rows[banner + 10].starts_with('╰'), "{}", text);
        for i in 0..40 {
            app.entries.push(Entry::Notice(format!("line {}", i)));
        }
        let text = screen(&app, 80, 24);
        assert!(!text.contains("█▀▄ █▀█ █▀▀"), "{}", text);
        assert!(text.contains("line 39"), "{}", text);
    }

    /// Writes the README screenshot's screen, cell by cell, to the JSON file
    /// `SCREENSHOT_JSON` names; `docs/render-screenshot.py` turns it into a
    /// PNG. Run with `cargo test screenshot -- --ignored`.
    #[test]
    #[ignore]
    fn screenshot() {
        let Ok(path) = std::env::var("SCREENSHOT_JSON") else { return };
        let mut app = app();
        app.phase = Phase::Ready;
        app.project = Some(("p1".into(), "general_chat".into()));
        app.entries.push(Entry::User(
            "[Doc 1] Issue a mutual NDA to Northwind Labs from our template: 2-year term, California law".into(),
        ));
        app.entries.push(Entry::Reply(Reply {
            warnings: Vec::new(),
            model: Some("sl-mini".into()),
            text: "Drafted nda-northwind-labs.docx from your template:\n\
                   1. Mutual NDA between Smartloop Inc. and Northwind Labs, Inc.\n\
                   2. 2-year term from signing; confidentiality survives 3 years (section 5)\n\
                   3. Governed by California law, venue in San Francisco (section 9)\n\
                   Signature blocks are left blank for both parties."
                .into(),
            citations: vec!["mutual-nda-template.docx".into()],
            stats: Some(TurnStats { tokens: 96, elapsed: Duration::from_secs(14) }),
            ended: None,
        }));
        app.entries.push(Entry::User("[Doc 2] Compare it with their redlines and flag anything risky".into()));
        app.entries.push(Entry::Reply(Reply::default()));
        let runtime = tokio::runtime::Runtime::new().unwrap();
        app.turn = Some(Turn {
            started: Instant::now() - Duration::from_secs(4),
            task: runtime.spawn(async {}),
            entry: 3,
            tokens: 0,
            activity: Some("Reading northwind-redlines.pdf".into()),
        });
        app.input = "".into();

        let (width, height) = (100, 38);
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|f| app.draw(f)).unwrap();
        let buffer = terminal.backend().buffer();
        let color = |c: Color| match c {
            Color::Rgb(r, g, b) => format!("#{:02x}{:02x}{:02x}", r, g, b),
            Color::Indexed(169) => "#e55d9c".to_string(),
            Color::Reset => String::new(),
            other => format!("{:?}", other),
        };
        let mut rows = Vec::new();
        for y in 0..height {
            let mut cells = Vec::new();
            for x in 0..width {
                let cell = &buffer[(x, y)];
                cells.push(serde_json::json!({
                    "s": cell.symbol(),
                    "fg": color(cell.fg),
                    "dim": cell.modifier.contains(Modifier::DIM),
                    "bold": cell.modifier.contains(Modifier::BOLD),
                }));
            }
            rows.push(cells);
        }
        std::fs::write(path, serde_json::to_string(&rows).unwrap()).unwrap();
    }

    #[test]
    fn a_short_conversation_sits_above_the_prompt() {
        let mut app = app();
        app.phase = Phase::Ready;
        app.project = Some(("p1".into(), "Default".into()));
        app.entries.push(Entry::Notice("hello".into()));
        let text = screen(&app, 80, 24);
        let rows: Vec<&str> = text.lines().collect();
        let at = rows.iter().position(|r| r.contains("hello")).expect(&text);
        let prompt = rows.iter().rposition(|r| r.starts_with('>')).expect(&text) - 1;
        // A blank row and the (empty) status row between them.
        assert_eq!(prompt - at, 3, "{}", text);
    }

    #[test]
    fn scrolling_stops_at_the_top_and_the_end() {
        let mut app = app();
        app.phase = Phase::Ready;
        for i in 0..60 {
            app.entries.push(Entry::Notice(format!("line {}", i)));
        }
        screen(&app, 80, 24);
        let limit = app.scroll_limit.get();
        assert!(limit > 0);
        for _ in 0..100 {
            app.scroll(WHEEL_LINES as i32);
        }
        assert_eq!(app.scroll_back, limit);
        assert!(screen(&app, 80, 24).contains("█▀▄ █▀█ █▀▀"), "the top shows the welcome card");
        app.scroll(-1000);
        assert_eq!(app.scroll_back, 0);
        assert!(screen(&app, 80, 24).contains("line 59"));
    }

    #[test]
    fn dragging_selects_and_copies_the_text() {
        let mut app = app();
        app.phase = Phase::Ready;
        app.entries.push(Entry::Notice("first line".into()));
        app.entries.push(Entry::Notice("second line".into()));
        let text = screen(&app, 80, 24);
        let rows: Vec<&str> = text.lines().collect();
        let y0 = rows.iter().position(|r| r.contains("first line")).unwrap() as u16;
        let y1 = rows.iter().position(|r| r.contains("second line")).unwrap() as u16;
        let mouse = |kind, column, row| MouseEvent { kind, column, row, modifiers: KeyModifiers::NONE };
        // From "first" (after "■ ") to the end of "second".
        app.mouse(mouse(MouseEventKind::Down(MouseButton::Left), 2, y0));
        app.mouse(mouse(MouseEventKind::Drag(MouseButton::Left), 12, y1));
        let selection = app.selection.as_ref().unwrap();
        assert_eq!(app.selected_text(selection), "first line\n\n■ second line");
        app.mouse(mouse(MouseEventKind::Up(MouseButton::Left), 12, y1));
        assert_eq!(app.copied.map(|(n, _)| n), Some(25));
        assert!(screen(&app, 80, 24).contains("Copied 25 characters"));
        assert_eq!(base64(b"hi!"), "aGkh");
        assert_eq!(base64(b"hi"), "aGk=");
    }

    #[test]
    fn footer_warns_from_70_percent_of_searches() {
        let mut app = app();
        app.phase = Phase::Ready;
        app.project = Some(("p1".into(), "Default".into()));
        let usage = |used| SearchUsage { used, limit: Some(50), plan: "free".into(), resets_on: None };
        app.apply(AppEvent::Usage(Some(usage(34))));
        assert!(!screen(&app, 100, 24).contains("AI search budget"));
        app.apply(AppEvent::Usage(Some(usage(35))));
        let text = screen(&app, 100, 24);
        assert!(text.contains("You have used 70% of your allocated AI search budget · 15 searches left · /upgrade"), "{}", text);
        // The free plan's 4 of 5: in full where it fits.
        let five = SearchUsage { used: 4, limit: Some(5), plan: "free".into(), resets_on: None };
        app.apply(AppEvent::Usage(Some(five)));
        let text = screen(&app, 100, 24);
        assert!(text.contains("You have used 80% of your allocated AI search budget · 1 search left · /upgrade"), "{}", text);
        // Narrower windows get shorter wording rather than a cut-off line.
        let text = screen(&app, 60, 24);
        assert!(text.contains("⚠ 80% of AI search budget used · 1 left · /upgrade"), "{}", text);
        let text = screen(&app, 40, 24);
        assert!(text.contains("⚠ AI search 80% used · /upgrade"), "{}", text);
        app.apply(AppEvent::Usage(Some(usage(50))));
        assert!(screen(&app, 120, 24).contains("⚠ You are out of AI search credits for this month · /upgrade"));
        assert!(screen(&app, 40, 24).contains("⚠ Out of AI search credits · /upgrade"));
    }

    #[test]
    fn usage_shows_whats_left_as_a_bar() {
        let mut app = app();
        app.phase = Phase::Ready;
        app.project = Some(("p1".into(), "Default".into()));
        app.overlay = Overlay::Usage { loading: true, tokens: TokenStats { days: vec![(0, 0), (0, 0), (0, 0), (0, 0), (500, 50), (0, 0), (1_000_000, 100_000)] } };
        assert!(screen(&app, 80, 24).contains("asking api.smartloop.ai"));
        app.apply(AppEvent::Usage(Some(SearchUsage {
            used: 12,
            limit: Some(50),
            plan: "free".into(),
            resets_on: Some("2026-11-01".into()),
        })));
        let text = screen(&app, 80, 24);
        for expected in [
            "Web search",
            "38 left of 50",
            "12 used · free plan · resets 2026-11-01",
            "/upgrade to Pro for 1,000 searches a month",
            "Tokens      1,100,550 in the last 7 days  ▁▁▁▁▂▁█",
            "1,000,500 in · 100,050 out · on this machine",
            "Cost saved  $3.5018 in the last 7 days",
        ] {
            assert!(text.contains(expected), "{}\n{}", expected, text);
        }
        // ctrl+s turned search off: say so, and how to turn it back on.
        app.apply(AppEvent::WebSearch { enabled: false, toggled: true });
        let text = screen(&app, 80, 30);
        assert!(text.contains("off for this project · ctrl+s turns it on"), "{}", text);
        // Signed out: nothing to show but how to sign in.
        app.apply(AppEvent::Usage(None));
        assert!(screen(&app, 80, 24).contains("sign in with /login"));
    }

    #[test]
    fn status_lists_the_session_and_what_the_agent_reports() {
        let mut app = app();
        app.phase = Phase::Ready;
        app.project = Some(("p1".into(), "Default".into()));
        app.overlay = Overlay::Status(None);
        app.apply(AppEvent::Status(vec![
            ("account".into(), "you@example.com".into()),
            ("model".into(), "sl-mini (Q4_K_M, 32768 ctx, 769 MB)".into()),
        ]));
        let text = screen(&app, 80, 24);
        for expected in [" project   Default", " session   cli-test", " account   you@example.com", " model     sl-mini (Q4_K_M"] {
            assert!(text.contains(expected), "{}\n{}", expected, text);
        }
        // In the dropdown under the prompt, where the command hints show.
        let rows: Vec<&str> = text.lines().collect();
        let top = rows.iter().position(|r| r.starts_with("  project   Default")).expect(&text);
        assert!(rows[top - 1].starts_with('─') && rows[top - 2].starts_with('>'), "{}", text);
        assert!(!text.contains('┌') && !text.contains("Status"), "{}", text);
        assert!(rows[23].contains("[esc] close"), "{}", text);
    }

    #[test]
    fn clear_empties_the_chat_and_starts_a_new_session() {
        let mut app = app();
        app.phase = Phase::Ready;
        app.project = Some(("p1".into(), "Default".into()));
        app.entries.push(Entry::Notice("earlier".into()));
        app.input = "/clear".into();
        app.submit();
        assert!(app.entries.is_empty());
        assert_ne!(app.session, "cli-test");
        app.input = "/new".into();
        app.submit();
        assert!(matches!(app.entries.last(), Some(Entry::Error(e)) if e.contains("Unknown command /new")));
    }

    #[test]
    fn models_show_as_a_list() {
        let mut app = app();
        app.phase = Phase::Ready;
        app.project = Some(("p1".into(), "Default".into()));
        let model = |name: &str, downloaded, enabled| ModelRow {
            name: name.into(),
            capabilities: "general, vision".into(),
            access: "free".into(),
            accessible: true,
            downloaded,
            enabled,
        };
        app.overlay = Overlay::Models {
            rows: Some(Ok(vec![model("gemma4-e4b", false, false), model("qwen3.5-2b", true, true)])),
            selected: 1,
        };
        let text = screen(&app, 80, 24);
        assert!(text.contains("  gemma4-e4b  not downloaded  general, vision"), "{}", text);
        assert!(text.contains("› qwen3.5-2b  ✓ enabled"), "{}", text);
        assert!(!text.contains("Capabilities") && !text.contains("Models"), "{}", text);
    }

    #[test]
    fn dropped_files_become_chips() {
        let dir = std::env::temp_dir().join(format!("slp-drop-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let photo = dir.join("my photo.png");
        let report = dir.join("report.pdf");
        std::fs::write(&photo, b"png").unwrap();
        std::fs::write(&report, b"pdf").unwrap();

        // How terminals paste a drop: escaped spaces, quotes, file:// URLs.
        let escaped = format!("{} {}", photo.display().to_string().replace(' ', "\\ "), report.display());
        assert_eq!(dropped_files(&escaped), Some(vec![photo.clone(), report.clone()]));
        assert_eq!(dropped_files(&format!("'{}'", photo.display())), Some(vec![photo.clone()]));
        assert_eq!(
            dropped_files(&format!("file://{}", photo.display().to_string().replace(' ', "%20"))),
            Some(vec![photo.clone()])
        );
        assert_eq!(dropped_files("just some text"), None);

        let mut app = app();
        app.phase = Phase::Ready;
        app.input = "compare".into();
        app.cursor = 7;
        app.paste(&escaped);
        assert_eq!(app.input, "compare [Image 1] [Doc 1] ");
        assert_eq!(app.attachments.len(), 2);

        // Backspace takes a chip, and its file, in one go.
        app.key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE));
        app.key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE));
        assert_eq!(app.input, "compare [Image 1] ");
        assert_eq!(app.attachments.len(), 1);

        // A file the agent can't read isn't attached, and says why.
        let archive = dir.join("bundle.zip");
        std::fs::write(&archive, b"zip").unwrap();
        app.paste(&archive.display().to_string());
        assert_eq!(app.input, "compare [Image 1] ");
        assert_eq!(app.attachments.len(), 1);
        assert!(matches!(app.entries.last(), Some(Entry::Error(e)) if e.contains("Can't attach bundle.zip")));
        // Nor an image the vision model can't read.
        let heic = dir.join("IMG_0001.HEIC");
        std::fs::write(&heic, b"heic").unwrap();
        app.paste(&heic.display().to_string());
        assert_eq!(app.attachments.len(), 1);
        assert!(matches!(app.entries.last(), Some(Entry::Error(e)) if e.contains("Can't attach IMG_0001.HEIC")));

        // Plain text pastes as text.
        app.paste("these two");
        assert_eq!(app.input, "compare [Image 1] these two");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn shift_enter_starts_a_new_line_for_the_question() {
        let dir = std::env::temp_dir().join(format!("slp-multi-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let report = dir.join("report.pdf");
        std::fs::write(&report, b"pdf").unwrap();

        let mut app = app();
        app.phase = Phase::Ready;
        app.project = Some(("p1".into(), "Default".into()));
        let press = |app: &mut App, code, modifiers| app.key(KeyEvent::new(code, modifiers));
        app.paste(&report.display().to_string());
        press(&mut app, KeyCode::Enter, KeyModifiers::SHIFT);
        for c in "what does it conclude?".chars() {
            press(&mut app, KeyCode::Char(c), KeyModifiers::NONE);
        }
        press(&mut app, KeyCode::Enter, KeyModifiers::ALT);
        press(&mut app, KeyCode::Char('j'), KeyModifiers::CONTROL);
        assert_eq!(app.input, "[Doc 1] \nwhat does it conclude?\n\n");
        assert_eq!(app.input_rows(), 4);

        let text = screen(&app, 80, 24);
        let rows: Vec<&str> = text.lines().collect();
        let first = rows.iter().position(|r| r.starts_with("> [Doc 1]")).expect(&text);
        assert!(rows[first + 1].starts_with("  what does it conclude?"), "{}", text);

        // Up moves between the prompt's lines before going through history.
        press(&mut app, KeyCode::Up, KeyModifiers::NONE);
        press(&mut app, KeyCode::Up, KeyModifiers::NONE);
        assert_eq!(app.cursor, "[Doc 1] \n".chars().count());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn esc_twice_clears_a_pasted_prompt() {
        let mut app = app();
        app.phase = Phase::Ready;
        app.project = Some(("p1".into(), "Default".into()));
        app.paste("a lot\nof text\npasted by mistake");
        app.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(app.input, "a lot\nof text\npasted by mistake");
        assert!(screen(&app, 80, 24).contains("[esc] again to clear"));
        app.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(app.input.is_empty() && app.cursor == 0);
        // Esc, a key, then Esc again doesn't.
        app.paste("kept");
        app.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        app.key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE));
        app.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(app.input, "kept");
    }

    #[test]
    fn up_goes_through_history_past_the_first_line() {
        let mut app = app();
        app.phase = Phase::Ready;
        app.project = Some(("p1".into(), "Default".into()));
        app.history = vec!["first".into(), "long\npasted\ntext".into()];
        let press = |app: &mut App, code| app.key(KeyEvent::new(code, KeyModifiers::NONE));
        app.paste("draft\nline two");
        press(&mut app, KeyCode::Up);
        assert_eq!(app.input, "draft\nline two");
        press(&mut app, KeyCode::Up);
        assert_eq!(app.input, "long\npasted\ntext");
        press(&mut app, KeyCode::Up);
        assert_eq!(app.input, "first");
        press(&mut app, KeyCode::Down);
        assert_eq!(app.input, "long\npasted\ntext");
        assert_eq!(app.cursor, app.input.chars().count());
        press(&mut app, KeyCode::Down);
        assert!(app.input.is_empty());
    }

    #[test]
    fn slash_shows_matching_commands() {
        let mut app = app();
        app.phase = Phase::Ready;
        app.project = Some(("p1".into(), "Default".into()));
        for c in "/l".chars() {
            app.key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
        }
        let text = screen(&app, 80, 24);
        assert!(text.contains("› /login"), "{}", text);
        assert!(text.contains("  /logout"), "{}", text);
        // The /models hint (not the card's tip) is filtered out.
        assert!(!text.contains("enable, disable and download models"), "{}", text);
        app.key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        app.key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert_eq!(app.input, "/logout ");
        assert!(app.command_hints().is_empty());
        app.input = "/mo".into();
        app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(matches!(app.overlay, Overlay::Models { .. }));
    }

    #[test]
    fn mcp_lists_the_servers_and_checks_the_url() {
        let mut app = app();
        app.phase = Phase::Ready;
        app.project = Some(("p1".into(), "Default".into()));
        app.input = "/mcp".into();
        app.submit();
        let server = |name: &str, state: &str, tools| mcp::Server {
            id: name.into(),
            name: name.into(),
            location: format!("https://{}.example.com/mcp", name),
            state: state.into(),
            tools,
            error: None,
        };
        app.apply(AppEvent::Mcp(Ok(vec![server("linear", "ready", 12), server("notion", "provisioning", 0)])));
        let text = screen(&app, 100, 30);
        assert!(text.contains("› linear"), "{}", text);
        assert!(text.contains("✓ 12 tools"), "{}", text);
        assert!(text.contains("https://linear.example.com/mcp"), "{}", text);
        assert!(text.contains("connecting"), "{}", text);

        // Typing closes the panel and goes to the prompt.
        for c in "/mcp add not-a-url".chars() {
            app.key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
        }
        assert!(matches!(app.overlay, Overlay::None));
        assert_eq!(app.input, "/mcp add not-a-url");
        app.submit();
        assert!(matches!(app.entries.last(), Some(Entry::Error(e)) if e.contains("Not a URL")));
    }

    #[test]
    fn setup_shows_a_box_with_a_bar_per_step() {
        let mut app = app();
        for event in [
            StepEvent::Add("Agent 1.2.7".into()),
            StepEvent::Done(0),
            StepEvent::Add("Start agent".into()),
            StepEvent::Detail(1, "port 38540".into()),
            StepEvent::Done(1),
            StepEvent::Add("Base model".into()),
            StepEvent::Add("Default project".into()),
            StepEvent::Name(2, "sl-mini".into()),
            StepEvent::Progress(2, 377 << 20, 769 << 20),
        ] {
            app.apply(AppEvent::Setup(event));
        }
        let text = screen(&app, 80, 30);
        let rows: Vec<&str> = text.lines().collect();
        let top = rows.iter().position(|r| r.starts_with("╭ Booting up…")).expect(&text);
        assert!(rows[top + 2].contains("[x] Agent 1.2.7") && rows[top + 2].contains("▰▰▰▰▰▰▰▰▰▰▰▰▰▰▰▰"), "{}", text);
        assert!(rows[top + 3].contains("[x] Start agent") && rows[top + 3].contains("port 38540"), "{}", text);
        assert!(rows[top + 4].contains("[•] Base model sl-mini") && rows[top + 4].contains("▰▰▰▰▰▰▰▰▱▱▱▱▱▱▱▱  49%"), "{}", text);
        assert!(rows[top + 5].contains("[ ] Default project") && rows[top + 5].contains("▱▱▱▱▱▱▱▱▱▱▱▱▱▱▱▱"), "{}", text);
        assert!(rows[top + 7].starts_with('╰') && rows[top + 7].contains("377 MB of 769 MB"), "{}", text);
    }

    #[test]
    fn conversation_logs_steps_and_status_sits_above_the_prompt() {
        let mut app = app();
        app.phase = Phase::Ready;
        app.project = Some(("p1".into(), "Default".into()));
        app.entries.push(Entry::User("what is in my docs?".into()));
        app.entries.push(Entry::Reply(Reply {
            warnings: Vec::new(),
            model: Some("sl-mini".into()),
            text: "Your documents cover the roadmap.".into(),
            citations: vec!["roadmap.pdf".into()],
            stats: Some(TurnStats { tokens: 12, elapsed: Duration::from_secs(2) }),
            ended: None,
        }));
        let runtime = tokio::runtime::Runtime::new().unwrap();
        app.turn = Some(Turn {
            started: Instant::now(),
            task: runtime.spawn(async {}),
            entry: 1,
            tokens: 40,
            activity: Some("Reading roadmap.pdf".into()),
        });
        let text = screen(&app, 80, 24);
        let rows: Vec<&str> = text.lines().collect();
        assert!(text.contains("> what is in my docs?"), "{}", text);
        assert!(text.contains("■ Your documents cover the roadmap."), "{}", text);
        assert!(text.contains("[1] roadmap.pdf"), "{}", text);
        assert!(text.contains("⎿  sl-mini · 12 tokens · 6.0 tok/s · 2s"), "{}", text);
        // The status line sits right above the prompt box.
        let status = rows.iter().position(|r| r.contains("Reading roadmap.pdf…")).expect(&text);
        assert!(rows[status].ends_with("(0s · 40 tokens)"), "{}", text);
        assert!(rows[status + 1].starts_with("────"), "{}", text);
        assert!(rows[status + 2].starts_with('>'), "{}", text);
        assert!(rows[23].contains("[enter] send  [esc] interrupt  [?] shortcuts"), "{}", text);
    }
}
