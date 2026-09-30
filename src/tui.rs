//! `smartloop run` as a full-screen app, laid out like Claude Code:
//!
//! ```text
//! > what is the capital of France?
//!
//! ⏺ Paris is the capital of France.
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
//! checklist under the banner before chat opens. Models, projects and
//! downloads open as panels over it.
//!
//! Blocking work (setup, the agent's REST calls, model downloads) runs on
//! plain threads and reports through a channel, so quitting never waits for
//! a download to finish.

use std::time::{Duration, Instant};

use async_openai::{Client as OpenAIClient, config::OpenAIConfig};
use crossterm::event::{
    DisableMouseCapture, EnableMouseCapture, Event, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers,
    MouseButton, MouseEvent, MouseEventKind,
};
use futures_util::StreamExt;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Clear, List, ListItem, ListState, Paragraph, Row, Table, TableState, Wrap};
use ratatui::{DefaultTerminal, Frame};
use reqwest::blocking::Client;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

use crate::chat::{self, ChatEvent, TurnStats};
use crate::progress::{self, Steps, format_duration, format_size};
use crate::{ModelRow, framework};

const LABEL_WIDTH: usize = 28;
const DETAIL_WIDTH: usize = 12;
const BAR_WIDTH: usize = 30;
/// An active step shows its elapsed time once it has run this long.
const SHOW_ELAPSED_AFTER: Duration = Duration::from_secs(2);
const TICK: Duration = Duration::from_millis(100);
/// Lines one wheel notch scrolls, and one page key.
const WHEEL_LINES: u16 = 3;
const PAGE_LINES: u16 = 10;
/// A bar turning in brackets.
const SPINNER: [&str; 4] = ["[-]", "[\\]", "[|]", "[/]"];

const BANNER: [&str; 2] = [
    "█▀ █▀▄▀█ ▄▀█ █▀█ ▀█▀ █   █▀█ █▀█ █▀█",
    "▄█ █ ▀ █ █▀█ █▀▄  █  █▄▄ █▄█ █▄█ █▀▀",
];

/// Run the app until the user quits, then print the session id so the
/// conversation can be resumed with `--session`.
pub fn run(client: &Client, prompt: Option<String>, project: Option<String>, session: Option<String>) {
    let runtime = tokio::runtime::Runtime::new()
        .unwrap_or_else(|e| crate::fail(format!("Failed to start async runtime: {}", e)));
    let mut terminal = ratatui::init();
    // The wheel scrolls the chat. Put mouse reporting back on a panic too,
    // or the shell gets escape codes for every move.
    let _ = crossterm::execute!(std::io::stdout(), EnableMouseCapture);
    let hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = crossterm::execute!(std::io::stdout(), DisableMouseCapture);
        hook(info);
    }));
    let result = runtime.block_on(App::new(client.clone(), prompt, project, session).run(&mut terminal));
    let _ = crossterm::execute!(std::io::stdout(), DisableMouseCapture);
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
    Chat(ChatEvent),
    TurnDone(Result<TurnStats, String>),
    Download(usize, StepEvent),
    DownloadDone(usize),
    Notice(String),
    Error(String),
    /// Who the agent is signed in as, after startup or `/login`/`/logout`.
    Account(Option<String>),
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

    /// The checklist's rows, with the active download's bar under its row.
    fn lines(&self) -> Vec<Line<'static>> {
        let mut lines = Vec::new();
        for s in &self.steps {
            let label = format!("{:<w$}", truncate(&s.label, LABEL_WIDTH), w = LABEL_WIDTH);
            let detail = |text: &str| Span::styled(format!(" {:>w$}", text, w = DETAIL_WIDTH), dim());
            let mut spans = match s.state {
                State::Done => vec![bracket("x", Color::Green), Span::raw(label), detail(&s.detail)],
                State::Failed => vec![bracket("✗", Color::Red), Span::raw(label.trim_end().to_string())],
                State::Pending => vec![Span::styled(format!("[ ] {}", label.trim_end()), dim())],
                State::Active => vec![bracket("•", pink()), Span::raw(label)],
            };
            if s.state == State::Active && s.bytes.is_none() {
                let took = s.started.map(|t| t.elapsed()).filter(|t| *t >= SHOW_ELAPSED_AFTER);
                match (s.detail.is_empty(), took) {
                    (false, Some(t)) => {
                        spans.push(detail(&s.detail));
                        spans.push(Span::styled(format!(" {}", format_duration(t)), dim()));
                    }
                    (false, None) => spans.push(detail(&s.detail)),
                    (true, Some(t)) => spans.push(detail(&format_duration(t))),
                    (true, None) => {}
                }
            }
            lines.push(Line::from(spans));
            if let (State::Active, Some((done, total))) = (s.state, s.bytes) {
                let mut bar = vec![Span::raw("    ")];
                bar.extend(bar_spans(done, total, BAR_WIDTH));
                lines.push(Line::from(bar));
            }
        }
        lines
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
    Projects { rows: Option<Result<Vec<serde_json::Value>, String>>, selected: usize },
    Downloads,
    Help,
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
    chat: OpenAIClient<OpenAIConfig>,
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
    /// Who the agent is signed in as; None until known or when signed out.
    account: Option<String>,
    /// The highlighted slash-command hint while typing a command.
    hint_at: usize,
    quit: bool,
}

impl App {
    fn new(client: Client, prompt: Option<String>, project: Option<String>, session: Option<String>) -> Self {
        let (tx, rx) = unbounded_channel();
        App {
            client,
            chat: chat::openai_client(crate::api_url()),
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
            scroll_back: 0,
            scroll_limit: std::cell::Cell::new(0),
            shown: std::cell::RefCell::new((Rect::default(), Vec::new())),
            selection: None,
            copied: None,
            clipboard: None,
            ticks: 0,
            token_entry: false,
            account: None,
            hint_at: 0,
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

    fn send_turn(&mut self, message: String) {
        let Some((project, _)) = self.project.clone() else { return };
        self.entries.push(Entry::User(message.clone()));
        self.entries.push(Entry::Reply(Reply::default()));
        let entry = self.entries.len() - 1;
        self.scroll_back = 0;
        let (chat, tx, session) = (self.chat.clone(), self.tx.clone(), self.session.clone());
        let task = tokio::spawn(async move {
            let events = tx.clone();
            let mut on = move |event| {
                let _ = events.send(AppEvent::Chat(event));
            };
            let result = chat::stream_turn(&chat, &message, &project, &session, &mut on).await;
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
                self.load_account();
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
            AppEvent::Chat(event) => self.chat_event(event),
            AppEvent::TurnDone(result) => {
                let Some(turn) = self.turn.take() else { return };
                if let Some(Entry::Reply(reply)) = self.entries.get_mut(turn.entry) {
                    match result {
                        Ok(stats) => reply.stats = Some(stats),
                        Err(e) => reply.ended = Some(e),
                    }
                }
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
            AppEvent::Account(account) => self.account = account,
            AppEvent::Error(text) => self.entries.push(Entry::Error(text)),
        }
    }

    /// Pick the project: the one asked for, else the server's current one.
    fn projects_loaded(&mut self, result: Result<Vec<serde_json::Value>, String>) {
        if let Overlay::Projects { rows, .. } = &mut self.overlay {
            *rows = Some(result.clone());
        }
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
                if let Some(prompt) = self.pending_prompt.take() {
                    self.send_turn(prompt);
                }
            }
            None => self.entries.push(Entry::Error(
                "No projects found; create one with `smartloop project create`".to_string(),
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
            (KeyCode::Char('p'), true) => return self.open_projects(),
            (KeyCode::Char('g'), true) => return self.overlay = Overlay::Downloads,
            _ => {}
        }
        match self.overlay {
            Overlay::None => self.key_chat(key),
            Overlay::Models { .. } => self.key_models(key),
            Overlay::Projects { .. } => self.key_projects(key),
            Overlay::Downloads | Overlay::Help => {
                if matches!(key.code, KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q')) {
                    self.overlay = Overlay::None;
                }
            }
        }
    }

    fn open_models(&mut self) {
        if self.project.is_none() {
            return;
        }
        self.overlay = Overlay::Models { rows: None, selected: 0 };
        self.load_models();
    }

    fn open_projects(&mut self) {
        if !matches!(self.phase, Phase::Ready) {
            return;
        }
        self.overlay = Overlay::Projects { rows: None, selected: 0 };
        self.load_projects();
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
        match key.code {
            KeyCode::Esc if self.token_entry => {
                self.token_entry = false;
                self.input.clear();
                self.cursor = 0;
            }
            KeyCode::Esc if self.turn.is_some() => self.stop_turn(),
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
            KeyCode::Up => self.recall(-1),
            KeyCode::Down => self.recall(1),
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

    fn byte_at(&self, chars: usize) -> usize {
        self.input.char_indices().nth(chars).map_or(self.input.len(), |(i, _)| i)
    }

    /// Step through earlier prompts, like a shell.
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
        self.cursor = self.input.chars().count();
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
                Some(token) => self.login(token.to_string()),
                None => self.token_entry = true,
            },
            "/logout" => {
                let project = self.project.clone();
                self.spawn(move |client, tx| {
                    let _ = tx.send(match crate::try_logout(&client) {
                        Ok(()) => AppEvent::Notice("Logged out".to_string()),
                        Err(e) => AppEvent::Error(e),
                    });
                    refresh_account(&client, &tx, project);
                })
            }
            "/quit" | "/exit" | "/q" | "exit" => self.quit = true,
            "/models" => self.open_models(),
            "/projects" => self.open_projects(),
            "/downloads" => self.overlay = Overlay::Downloads,
            "/help" | "/?" => self.overlay = Overlay::Help,
            "/new" => {
                self.session = chat::new_session_id();
                self.entries.push(Entry::Notice("New session".to_string()));
            }
            "/clear" => self.entries.clear(),
            _ if command.starts_with('/') => {
                self.entries.push(Entry::Error(format!("Unknown command {}; try /help", command)));
            }
            _ => {
                // One turn at a time, and only once there's a project.
                if self.turn.is_some() || self.project.is_none() {
                    return;
                }
                self.send_turn(input.clone());
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
            refresh_account(&client, &tx, project);
        });
    }

    fn load_account(&self) {
        self.spawn(|client, tx| refresh_account(&client, &tx, None));
    }

    fn key_models(&mut self, key: KeyEvent) {
        let Overlay::Models { rows, selected } = &mut self.overlay else { return };
        let count = rows.as_ref().and_then(|r| r.as_ref().ok()).map_or(0, Vec::len);
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => self.overlay = Overlay::None,
            KeyCode::Up | KeyCode::Char('k') => *selected = selected.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => *selected = (*selected + 1).min(count.saturating_sub(1)),
            KeyCode::Char('r') => self.load_models(),
            KeyCode::Enter | KeyCode::Char(' ') => {
                let Some(row) = rows.as_ref().and_then(|r| r.as_ref().ok()).and_then(|r| r.get(*selected)) else {
                    return;
                };
                let row = row.clone();
                if !row.accessible {
                    self.entries.push(Entry::Error(format!(
                        "{} needs a sign-in; run `smartloop login` first",
                        row.name
                    )));
                } else if row.enabled {
                    self.disable_model(row.name);
                } else {
                    self.enable_model(row.name);
                }
            }
            _ => {}
        }
    }

    fn key_projects(&mut self, key: KeyEvent) {
        let Overlay::Projects { rows, selected } = &mut self.overlay else { return };
        let list = rows.as_ref().and_then(|r| r.as_ref().ok()).cloned().unwrap_or_default();
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => self.overlay = Overlay::None,
            KeyCode::Up | KeyCode::Char('k') => *selected = selected.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => *selected = (*selected + 1).min(list.len().saturating_sub(1)),
            KeyCode::Char('r') => self.load_projects(),
            KeyCode::Enter => {
                let Some(p) = list.get(*selected) else { return };
                let id = p["id"].as_str().unwrap_or_default().to_string();
                let name = p["name"].as_str().unwrap_or_default().to_string();
                self.overlay = Overlay::None;
                if self.project.as_ref().is_some_and(|(current, _)| *current == id) {
                    return;
                }
                self.stop_turn();
                self.session = chat::new_session_id();
                self.entries.push(Entry::Notice(format!("Switched to {} · new session", name)));
                self.project = Some((id, name));
            }
            _ => {}
        }
    }

    // ----- drawing -------------------------------------------------------

    fn draw(&self, frame: &mut Frame) {
        let status = self.status_lines();
        // Command hints take the footer's place while a command is typed.
        let hints = self.hint_lines();
        // A blank row at the top, and one between the conversation and the
        // status line, so neither sits cramped against its neighbor.
        let [_, body, _, status_area, input, footer] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Min(3),
            Constraint::Length(1),
            Constraint::Length(status.len().max(1) as u16),
            Constraint::Length(3),
            Constraint::Length(hints.len().max(1) as u16),
        ])
        .areas(frame.area());

        self.draw_conversation(frame, body);
        self.keep_shown(frame, body);
        frame.render_widget(Paragraph::new(status), status_area);
        self.draw_input(frame, input);
        if hints.is_empty() {
            self.draw_footer(frame, footer);
        } else {
            frame.render_widget(Paragraph::new(hints), footer);
        }

        match &self.overlay {
            Overlay::None => {}
            Overlay::Models { rows, selected } => self.draw_models(frame, rows, *selected),
            Overlay::Projects { rows, selected } => self.draw_projects(frame, rows, *selected),
            Overlay::Downloads => self.draw_downloads(frame),
            Overlay::Help => draw_help(frame),
        }
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

    /// The welcome card: the logo on the left, and on the right who's
    /// signed in, the project, the agent and the versions.
    ///
    /// ```text
    /// ╭──────────────────────────────────────────────────────────────╮
    /// │                                                              │
    /// │  █▀ █▀▄▀█ ▄▀█ █▀█ ▀█▀ █   █▀█ █▀█ █▀█   account  you@...    │
    /// │  ▄█ █ ▀ █ █▀█ █▀▄  █  █▄▄ █▄█ █▄█ █▀▀   project  general_chat│
    /// │  Local AI assistant                     agent    localhost…  │
    /// │                                         version  cli 1.0.11  │
    /// ╰──────────────────────────────────────────────────────────────╯
    /// ```
    fn welcome(&self, width: u16) -> Vec<Line<'static>> {
        let width = width as usize;
        let border = dim();
        let account = self.account.clone().unwrap_or_else(|| "not signed in · /login".to_string());
        let project = self.project.as_ref().map_or("…".to_string(), |(_, name)| name.clone());
        let right = [
            ("account", account),
            ("project", project),
            ("agent", crate::base_url().trim_start_matches("http://").to_string()),
            ("version", format!("cli {} · SLP {}", env!("CARGO_PKG_VERSION"), framework::VERSION)),
        ];
        // Styled per span: the rows are taken apart into spans below.
        let mut left: Vec<Line<'static>> = BANNER
            .iter()
            .map(|row| Line::from(Span::styled(*row, Style::new().fg(pink()))))
            .collect();
        left.push(Line::from(Span::styled("Local AI assistant · ? for shortcuts", dim())));
        let left_width = BANNER[0].chars().count().max(left[2].width()) + 4;

        let row = |label: &str, value: &str| {
            Line::from(vec![
                Span::styled(format!("{:<9}", label), dim()),
                Span::raw(value.to_string()),
            ])
        };
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

        let mut lines = vec![Line::styled(format!("╭{}╮", "─".repeat(width.saturating_sub(2))), border)];
        let blank = Line::default();
        for line in std::iter::once(&blank).chain(body.iter()).chain(std::iter::once(&blank)) {
            let used = line.width().min(inner);
            let mut spans = vec![Span::styled("│ ", border)];
            spans.extend(line.spans.iter().cloned());
            spans.push(Span::raw(" ".repeat(inner - used)));
            spans.push(Span::styled(" │", border));
            lines.push(Line::from(spans));
        }
        lines.push(Line::styled(format!("╰{}╯", "─".repeat(width.saturating_sub(2))), border));
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
            lines.extend(self.setup.lines());
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
                    Span::styled("⏺ ", Style::new().fg(Color::Green)),
                    Span::raw(text.clone()),
                ])),
                Entry::Warning(text) => lines.push(Line::from(vec![
                    Span::styled("⏺ ", Style::new().fg(Color::Yellow)),
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
            // Keep the cursor in view: show the tail of a long prompt.
            let before: String = input.chars().take(self.cursor).collect();
            let indent = label.len() as u16;
            let room = inner.width.saturating_sub(indent + 1) as usize;
            let offset = Span::raw(before.as_str()).width().saturating_sub(room);
            let visible: String = input.chars().skip(offset).collect();
            let x = inner.x + indent + Span::raw(before.as_str()).width().saturating_sub(offset) as u16;
            if matches!(self.overlay, Overlay::None) {
                frame.set_cursor_position((x.min(inner.right().saturating_sub(1)), inner.y));
            }
            Line::from(vec![prompt, Span::raw(visible)])
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
        } else {
            // The rest live under help, as in Claude Code.
            hints.extend(key_hints(&[("enter", "send"), ("esc", "interrupt"), ("?", "shortcuts")]));
        }
        frame.render_widget(Paragraph::new(Line::from(hints)), area);
    }

    fn draw_models(&self, frame: &mut Frame, rows: &Option<Result<Vec<ModelRow>, String>>, selected: usize) {
        let area = popup(frame.area(), 90, 70);
        let project = self.project.as_ref().map_or(String::new(), |(_, n)| n.clone());
        let block = panel(&format!(" Models · {} ", project), &bar_hints(&[("↑↓", "select"), ("enter", "enable/disable"), ("r", "refresh"), ("esc", "close")]));
        frame.render_widget(Clear, area);
        let list = match rows {
            None => return frame.render_widget(Paragraph::new(format!("Loading {}", self.spinner())).block(block), area),
            Some(Err(e)) => return frame.render_widget(Paragraph::new(e.clone()).block(block).wrap(Wrap { trim: false }), area),
            Some(Ok(list)) => list,
        };
        let flag = |on: bool| if on { Cell::from("✓").green() } else { Cell::from("·").dim() };
        let body = list.iter().map(|m| {
            let downloading = self.downloads.iter().find(|d| d.model == m.name && !d.finished);
            let downloaded = match downloading.and_then(|d| d.list.active()) {
                Some(StepRow { bytes: Some((done, total)), .. }) if *total > 0 => {
                    Cell::from(format!("{}%", done * 100 / total)).fg(pink())
                }
                Some(_) => Cell::from(self.spinner()).fg(pink()),
                None => flag(m.downloaded),
            };
            let access = if m.accessible {
                Cell::from(m.access.clone())
            } else {
                Cell::from(format!("{} (sign in)", m.access)).yellow()
            };
            Row::new(vec![Cell::from(m.name.clone()), Cell::from(m.capabilities.clone()), access, downloaded, flag(m.enabled)])
        });
        let table = Table::new(
            body,
            [Constraint::Fill(2), Constraint::Fill(3), Constraint::Length(16), Constraint::Length(10), Constraint::Length(7)],
        )
        .header(Row::new(["Name", "Capabilities", "Access", "Downloaded", "Enabled"]).bold().bottom_margin(1))
        .row_highlight_style(Style::new().bg(Color::DarkGray))
        .highlight_symbol("▶ ")
        .block(block);
        let mut state = TableState::default().with_selected(Some(selected));
        frame.render_stateful_widget(table, area, &mut state);
    }

    fn draw_projects(&self, frame: &mut Frame, rows: &Option<Result<Vec<serde_json::Value>, String>>, selected: usize) {
        let area = popup(frame.area(), 70, 60);
        let block = panel(" Projects ", &bar_hints(&[("↑↓", "select"), ("enter", "switch"), ("r", "refresh"), ("esc", "close")]));
        frame.render_widget(Clear, area);
        let list = match rows {
            None => return frame.render_widget(Paragraph::new(format!("Loading {}", self.spinner())).block(block), area),
            Some(Err(e)) => return frame.render_widget(Paragraph::new(e.clone()).block(block).wrap(Wrap { trim: false }), area),
            Some(Ok(list)) => list,
        };
        let current = self.project.as_ref().map(|(id, _)| id.as_str());
        let items = list.iter().map(|p| {
            let id = p["id"].as_str().unwrap_or_default();
            let mut spans = vec![Span::raw(p["name"].as_str().unwrap_or_default().to_string())];
            if Some(id) == current {
                spans.push(Span::styled("  (this chat)", Style::new().fg(pink())));
            } else if p["current"].as_bool().unwrap_or_default() {
                spans.push(Span::styled("  (current)", dim()));
            }
            if p["system"].as_bool().unwrap_or_default() {
                spans.push(Span::styled("  system", Style::new().fg(Color::Magenta)));
            }
            spans.push(Span::styled(format!("  {}", id), dim()));
            ListItem::new(Line::from(spans))
        });
        let widget = List::new(items)
            .block(block)
            .highlight_style(Style::new().bg(Color::DarkGray))
            .highlight_symbol("▶ ");
        let mut state = ListState::default().with_selected(Some(selected));
        frame.render_stateful_widget(widget, area, &mut state);
    }

    fn draw_downloads(&self, frame: &mut Frame) {
        let area = popup(frame.area(), 80, 60);
        let block = panel(" Downloads ", &bar_hints(&[("esc", "close")]));
        frame.render_widget(Clear, area);
        let mut lines: Vec<Line> = Vec::new();
        if !self.setup.steps.is_empty() {
            lines.push(Line::styled("Setup", Style::new().bold()));
            lines.extend(self.setup.lines());
        }
        for d in &self.downloads {
            if !lines.is_empty() {
                lines.push(Line::default());
            }
            lines.push(Line::styled(d.model.clone(), Style::new().bold()));
            lines.extend(d.list.lines());
        }
        if lines.is_empty() {
            lines.push(Line::styled("Nothing downloaded yet. Enable a model from ^O models.", dim()));
        }
        frame.render_widget(Paragraph::new(lines).block(block), area);
    }

    fn spinner(&self) -> &'static str {
        SPINNER[self.ticks % SPINNER.len()]
    }
}

/// A reply as Claude Code lays one out: the answer under a `⏺`, its
/// sources, then `⎿` with the model that answered and how it went.
fn reply_lines(reply: &Reply, lines: &mut Vec<Line<'static>>) {
    for warning in &reply.warnings {
        lines.push(Line::from(vec![
            Span::styled("⏺ ", Style::new().fg(Color::Yellow)),
            Span::styled(warning.clone(), dim()),
        ]));
    }
    for (i, line) in reply.text.trim_end().lines().enumerate() {
        let mark = if i == 0 { Span::raw("⏺ ") } else { Span::raw("  ") };
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
        Span::styled("⏺ ", Style::new().fg(Color::Red)),
        Span::styled("Error: ", Style::new().fg(Color::Red).bold()),
        Span::raw(text.to_string()),
    ])
}

/// Re-read who the agent is signed in as and, with a project, its models:
/// signing in or out changes which ones are accessible.
fn refresh_account(client: &Client, tx: &UnboundedSender<AppEvent>, project: Option<(String, String)>) {
    if let Ok(account) = crate::signed_in_as(client) {
        let _ = tx.send(AppEvent::Account(account));
    }
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

/// The slash commands, for hints as they're typed.
/// `/exit` and `/q` also quit, but aren't hinted.
const COMMANDS: [(&str, &str); 9] = [
    ("/help", "shortcuts and commands"),
    ("/login", "sign in with a token"),
    ("/logout", "sign out"),
    ("/models", "enable, disable and download models"),
    ("/projects", "switch project"),
    ("/downloads", "setup and model downloads"),
    ("/new", "start a new session"),
    ("/clear", "clear the screen"),
    ("/quit", "quit"),
];

fn draw_help(frame: &mut Frame) {
    let rows = [
        ("[?] /help", "these shortcuts"),
        ("[enter]", "send the prompt"),
        ("[esc]", "interrupt the reply"),
        ("[↑] [↓]", "earlier prompts"),
        ("wheel [pgup] [pgdn]", "scroll the conversation"),
        ("drag", "select and copy to the clipboard"),
        ("[ctrl+o] /models", "models: enable, disable, download"),
        ("[ctrl+p] /projects", "switch project"),
        ("[ctrl+g] /downloads", "setup and model downloads"),
        ("/login", "sign in with a token"),
        ("/logout", "sign out"),
        ("/new", "start a new session"),
        ("/clear", "clear the screen"),
        ("[ctrl+c] /quit", "quit"),
    ];
    let lines: Vec<Line> = rows
        .iter()
        .map(|(key, what)| Line::from(vec![Span::styled(format!("{:<22}", key), key_style()), Span::raw(*what)]))
        .collect();
    // Just tall enough for the list, centered.
    let screen = frame.area();
    let height = (lines.len() as u16 + 2).min(screen.height);
    let width = 64.min(screen.width);
    let area = Rect::new(
        screen.x + (screen.width - width) / 2,
        screen.y + (screen.height - height) / 2,
        width,
        height,
    );
    frame.render_widget(Clear, area);
    frame.render_widget(Paragraph::new(lines).block(panel(" Help ", &bar_hints(&[("esc", "close")]))), area);
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

fn panel(title: &str, hint: &[Span<'static>]) -> Block<'static> {
    Block::bordered()
        .border_style(Style::new().fg(pink()))
        .title(Line::from(title.to_string()).bold())
        .title_bottom(Line::from(hint.to_vec()).right_aligned())
}

/// A box `w`% by `h`% of `area`, centered.
fn popup(area: Rect, w: u16, h: u16) -> Rect {
    let [_, row, _] = Layout::vertical([
        Constraint::Percentage((100 - h) / 2),
        Constraint::Percentage(h),
        Constraint::Fill(1),
    ])
    .areas(area);
    let [_, cell, _] = Layout::horizontal([
        Constraint::Percentage((100 - w) / 2),
        Constraint::Percentage(w),
        Constraint::Fill(1),
    ])
    .areas(row);
    cell
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
        app.input = "/login".into();
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
        let banner = rows.iter().position(|r| r.contains("█▀ █▀▄▀█")).expect(&text);
        assert!(rows[banner].contains("account  not signed in"), "{}", text);
        assert!(rows[banner + 1].contains("project  Default"), "{}", text);
        assert!(rows[banner + 3].contains(&format!("version  cli {}", env!("CARGO_PKG_VERSION"))), "{}", text);
        assert!(rows[banner - 2].starts_with('╭') && rows[banner + 5].starts_with('╰'), "{}", text);
        for i in 0..40 {
            app.entries.push(Entry::Notice(format!("line {}", i)));
        }
        let text = screen(&app, 80, 24);
        assert!(!text.contains("█▀ █▀▄▀█"), "{}", text);
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
        app.account = Some("you@example.com".into());
        app.entries.push(Entry::User("What are three things to do in Madrid? Keep it short.".into()));
        app.entries.push(Entry::Reply(Reply {
            warnings: Vec::new(),
            model: Some("sl-mini".into()),
            text: "1. Explore the Royal Palace & Prado Museum\n2. Wander Los Rosales\n3. Indulge in Tapas and Food".into(),
            citations: vec![
                "https://www.cntraveler.com/story/three-perfect-days-in-madrid-according-to-our-local-editor".into(),
                "https://www.esmadrid.com/en/whats-on-madrid".into(),
                "https://www.tripadvisor.com/Attractions-g187514-Activities-Madrid.html".into(),
            ],
            stats: Some(TurnStats { tokens: 28, elapsed: Duration::from_secs(25) }),
            ended: None,
        }));
        app.entries.push(Entry::User("Which one is best on a rainy day?".into()));
        app.entries.push(Entry::Reply(Reply::default()));
        let runtime = tokio::runtime::Runtime::new().unwrap();
        app.turn = Some(Turn {
            started: Instant::now() - Duration::from_secs(4),
            task: runtime.spawn(async {}),
            entry: 3,
            tokens: 0,
            activity: Some("Searching the web for the latest information".into()),
        });
        app.input = "".into();

        let (width, height) = (100, 30);
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
        assert!(screen(&app, 80, 24).contains("█▀ █▀▄▀█"), "the top shows the welcome card");
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
        // From "first" (after "⏺ ") to the end of "second".
        app.mouse(mouse(MouseEventKind::Down(MouseButton::Left), 2, y0));
        app.mouse(mouse(MouseEventKind::Drag(MouseButton::Left), 12, y1));
        let selection = app.selection.as_ref().unwrap();
        assert_eq!(app.selected_text(selection), "first line\n\n⏺ second line");
        app.mouse(mouse(MouseEventKind::Up(MouseButton::Left), 12, y1));
        assert_eq!(app.copied.map(|(n, _)| n), Some(25));
        assert!(screen(&app, 80, 24).contains("Copied 25 characters"));
        assert_eq!(base64(b"hi!"), "aGkh");
        assert_eq!(base64(b"hi"), "aGk=");
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
        assert!(!text.contains("/models"), "{}", text);
        app.key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        app.key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert_eq!(app.input, "/logout ");
        assert!(app.command_hints().is_empty());
        app.input = "/mo".into();
        app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(matches!(app.overlay, Overlay::Models { .. }));
    }

    #[test]
    fn setup_shows_the_checklist() {
        let mut app = app();
        for event in [
            StepEvent::Add("SLP framework 1.2.7".into()),
            StepEvent::Done(0),
            StepEvent::Add("Start agent".into()),
            StepEvent::Detail(1, "port 38540".into()),
            StepEvent::Done(1),
            StepEvent::Add("Chat model".into()),
            StepEvent::Add("Default project".into()),
            StepEvent::Name(2, "sl-mini".into()),
            StepEvent::Progress(2, 377 << 20, 769 << 20),
        ] {
            app.apply(AppEvent::Setup(event));
        }
        let text = screen(&app, 80, 24);
        assert!(text.contains("[x] Start agent"), "{}", text);
        assert!(text.contains("port 38540"), "{}", text);
        assert!(text.contains("[•] Chat model sl-mini"), "{}", text);
        assert!(text.contains("49%  377 MB/769 MB"), "{}", text);
        assert!(text.contains("[ ] Default project"), "{}", text);
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
        assert!(text.contains("⏺ Your documents cover the roadmap."), "{}", text);
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
