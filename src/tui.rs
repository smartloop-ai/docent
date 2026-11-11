//! `docent run` as a full-screen app, laid out like Claude Code:
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

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
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
use crate::usage::{CreditUsage, SearchUsage};
use crate::{ModelRow, docent, files, framework, lsp, mcp};

const TICK: Duration = Duration::from_millis(100);
/// How often the welcome card's agent state is re-read.
const AGENT_EVERY: Duration = Duration::from_secs(3);
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
/// Seconds between checks of the working directory's `.docent/` sync.
const SYNC_INTERVAL: u64 = 5;
/// A bar turning in brackets.
const SPINNER: [&str; 4] = ["[-]", "[\\]", "[|]", "[/]"];

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
    // An agent this session started goes with it, unless another session
    // still uses it.
    if crate::is_local() && framework::stop_launched() {
        eprintln!("Stopped the agent this session started");
    }
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
    /// Chat in this project from now on: one just created, imported, or
    /// named with `/project <name>`.
    SwitchProject(Project),
    Models(Result<Vec<ModelRow>, String>),
    /// A newer docent release than this one, for the footer.
    Update(String),
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
    /// `/usage`: this month's credits, read with the searches.
    Credits(Option<CreditUsage>),
    /// The project's web search switch, read or just toggled; `toggled`
    /// says whether to announce it.
    WebSearch { enabled: bool, toggled: bool },
    /// A sync started pushing this many changes, or finished at 0.
    Syncing(usize),
    /// `/sync` checked the folder: link it to this project and follow it.
    SyncLink(std::path::PathBuf, (String, String)),
    /// The sync that `stop` stops has ended on its own.
    SyncEnded(Arc<AtomicBool>),
    /// What the agent is doing for this project, for the welcome card;
    /// `None` when it doesn't answer.
    Agent(Option<String>, Option<crate::AgentState>),
    /// Who's signed in, for the welcome card; `None` when no one is.
    Account(Option<String>),
    /// The MCP servers of this project, by id, for the welcome card.
    Connections(String, Vec<mcp::Server>),
    /// The skills of this project, by id, for the slash-command hints.
    Skills(String, Vec<(String, String)>),
    /// A shell run's process is up: its run's id and process id, for
    /// `/stop`.
    Started(usize, u32),
    /// A shell run printed more: its run's id, and all it printed so far.
    Printed(usize, String),
    /// A `!command` finished: its run's id, its output and how it ended.
    /// `None` when it goes on in the background, told once; its end comes
    /// later as another `Shell`.
    Shell(usize, String, Option<Ended>),
    /// A turn's files were uploaded: their ids, for its tool rounds.
    Uploaded(Vec<String>),
    /// A reply's script finished, or went on in the background: as
    /// `Shell`, with the project it ran for.
    Ran(usize, String, String, Option<Ended>),
    /// A reply's scripts left unrun, as one before them failed or was
    /// stopped: off the running list, and out of the chat.
    Unrun(Vec<usize>),
    /// A check of the project after files were written: its shell entry's
    /// id, output and end.
    Check(usize, String, Ended),
    /// The language servers' errors in the files written: the check's id,
    /// the servers asked, and what they report.
    Diagnostics(usize, Vec<&'static str>, Vec<lsp::Problem>),
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

/// A project as `/project` lists it.
#[derive(Clone, Debug, PartialEq)]
struct Project {
    id: String,
    name: String,
    description: String,
    /// Made by the agent itself, e.g. the default one.
    system: bool,
}

impl Project {
    fn from_json(project: &serde_json::Value) -> Self {
        let field = |k: &str| project[k].as_str().unwrap_or_default().to_string();
        Project {
            id: field("id"),
            name: field("name"),
            description: field("description"),
            system: project["system"].as_bool().unwrap_or_default(),
        }
    }

    /// Whether `/project <wanted>` means this one: its id, or its name in
    /// any case.
    fn named(&self, wanted: &str) -> bool {
        self.id == wanted || self.name.eq_ignore_ascii_case(wanted)
    }
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
    Shell(Shell),
    /// The last reply's shell commands, waiting to be allowed or skipped.
    Ask(Vec<String>),
    Notice(String),
    Warning(String),
    Error(String),
}

/// A file a reply wrote as it streamed: what it was before (`None`, new)
/// and what the reply made it, reported once the reply's in.
struct Early {
    path: String,
    old: Option<String>,
    new: String,
}

/// A `!command`, a reply's script or a check run in the working directory,
/// with what it printed so far and how it ended.
struct Shell {
    id: usize,
    command: String,
    /// What it printed so far, as a terminal would show it.
    output: String,
    /// How it ended; `None` while it runs.
    ended: Option<Ended>,
    /// Given with `&` at its end, as `npm run dev &`: started in the
    /// background, nothing waits for it to end.
    background: bool,
    /// Told to the agent as running in the background, so its end is told
    /// as that one's.
    detached: bool,
}

/// How a shell run ended.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Ended {
    Exited(i32),
    /// By a signal from outside.
    Killed,
    /// By the user: `/stop`, or quitting.
    Stopped,
}

impl Ended {
    /// The exit code, as a terminal's `$?`: none for a signal.
    fn code(self) -> Option<i32> {
        match self {
            Ended::Exited(code) => Some(code),
            _ => None,
        }
    }

    /// Whether the run went wrong. A stop isn't a failure: the user
    /// wanted it.
    fn failed(self) -> bool {
        !matches!(self, Ended::Exited(0) | Ended::Stopped)
    }

    /// How it ended, for the agent: "exited with 1".
    fn describe(self) -> String {
        match self {
            Ended::Exited(code) => format!("exited with {}", code),
            Ended::Killed => "was killed by a signal".to_string(),
            Ended::Stopped => "was stopped by me".to_string(),
        }
    }
}

/// A shell run still going, listed under the prompt, for `/stop` and for a
/// click on it.
struct Job {
    id: usize,
    command: String,
    started: Instant,
    /// The shell's process id once it runs; its group is what a stop ends.
    pid: Option<u32>,
    /// Set to stop it: the run's thread ends the process, and the run
    /// ends as stopped rather than failed.
    stopped: Arc<AtomicBool>,
}

/// Where a shell run's thread reports: the run's id, the app's channel,
/// and the flag a `/stop` sets.
struct Report {
    id: usize,
    tx: UnboundedSender<AppEvent>,
    stopped: Arc<AtomicBool>,
}

/// A piece of a reply as it was laid out, and what it was laid out from:
/// where it starts, the width, the file it gives (if any) and whether the
/// agent was shown that file whole, and its text.
struct Piece {
    start: usize,
    width: u16,
    path: Option<String>,
    whole: bool,
    key: String,
    /// Laid out while its file was still coming in, only named.
    pending: bool,
    lines: Vec<Line<'static>>,
}

/// A reply as laid out: its text's length, the width, whether it was
/// still coming in, and its lines.
type Laid = (usize, u16, bool, Vec<Line<'static>>);

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
    /// `text` as last drawn, with its length and width and whether it was
    /// still coming in: the conversation redraws every tick, and only a
    /// reply still streaming changes, or one that's just ended.
    rendered: std::cell::RefCell<Option<Laid>>,
    /// When it was last laid out: a reply streaming in is laid out again
    /// at most once a tick, not once a chunk.
    rendered_at: std::cell::Cell<Option<Instant>>,
    /// Each piece of `text` (a run of prose or a code block) as last laid
    /// out: as a reply streams in only its last piece changes, so the ones
    /// before it, a file's diff above all, aren't laid out again.
    pieces: std::cell::RefCell<Vec<Piece>>,
    /// The folder the reply's files are written into, when it was asked to
    /// make them: a file block shows as its change against the file there.
    root: Option<std::path::PathBuf>,
    /// Each file as it was in `root` before the reply, by path, read once:
    /// the reply's files are written when it ends, and the change is
    /// against what was there.
    olds: std::cell::RefCell<std::collections::HashMap<String, Option<String>>>,
    /// The files the agent was shown whole: a reply giving back less of
    /// one is still all of it.
    whole: Vec<String>,
    /// Its blocks done and dealt with as it streams.
    applied: usize,
    /// The files written as their blocks were done, before the reply was.
    written: Vec<String>,
}

impl Reply {
    /// Write each file whose block is done since the last call, as soon as
    /// it is, rather than when the reply is, new or there already.
    fn write_done(&mut self, root: &std::path::Path) {
        let blocks = files::blocks(&self.text);
        while let Some(block) = blocks.get(self.applied).filter(|b| b.closed) {
            self.applied += 1;
            // The reply up to this block, as edits and appends are made on
            // the files as they were before it, and on its blocks before.
            let upto: Vec<&str> = self.text.lines().take(block.end).collect();
            let found = files::extract_with(&upto.join("\n"), |p| self.old(p));
            let Some(file) = found.files.into_iter().find(|f| f.path == block.file.path) else { continue };
            // Read as it was before it's written over: the reply's diffs and
            // its report are against that.
            let _ = self.old(&file.path);
            let mut whole = self.whole.clone();
            whole.extend(found.edited);
            let plan = files::plan(root, vec![file], &whole);
            let ready = [plan.new, plan.changed].concat();
            for path in files::write(root, &ready).0 {
                if !self.written.contains(&path) {
                    self.written.push(path);
                }
            }
        }
    }

    /// `text` rendered as markdown at `width`, its links and bare web
    /// addresses clickable, and its code blocks drawn as blocks: see
    /// `code_lines`.
    fn body(&self, width: u16) -> Vec<Line<'static>> {
        self.layout(width);
        self.rendered.borrow().as_ref().map(|(_, _, _, lines)| lines.clone()).unwrap_or_default()
    }

    /// Lay `text` out at `width` for `body`, unless it's laid out as it
    /// is. A frame measures each entry before it draws, so a reply is laid
    /// out when it's measured, and the frame draws what it measured.
    fn layout(&self, width: u16) {
        let text = self.text.trim_end();
        let mut rendered = self.rendered.borrow_mut();
        let streaming = self.stats.is_none() && self.ended.is_none();
        let fresh = self.rendered_at.get().is_some_and(|at| at.elapsed() < TICK);
        match &*rendered {
            // By the length as it came, so a line just ended counts.
            Some((len, at, was, _)) if *len == self.text.len() && *at == width && *was == streaming => {}
            // Still coming in, and laid out a moment ago: that will do until
            // the next tick.
            Some((_, at, true, _)) if streaming && fresh && *at == width => {}
            _ => {
                self.rendered_at.set(Some(Instant::now()));
                // While the reply comes in, a file still being given is only
                // named: its change shows once its block is done, as a
                // coding harness shows an edit once it's made.
                let replying = streaming;
                let blocks = if self.root.is_some() { files::blocks(text) } else { Vec::new() };
                let mut lines: Vec<Line<'static>> = Vec::new();
                let mut pieces = self.pieces.borrow_mut();
                let mut laid: Vec<Piece> = Vec::new();
                for (i, (code, segment, start)) in segments(text).into_iter().enumerate() {
                    let block = blocks.iter().find(|b| b.fence == start);
                    let file = block.map(|b| &b.file);
                    // A shell block's heredocs that write files, each shown
                    // as its change.
                    let span = start + segment.lines().count();
                    let docs: Vec<&files::Block> = match (code, file) {
                        // Not an edit's pairs, fenced by `segments` when bare.
                        (true, None) if !segment.lines().nth(1).is_some_and(files::is_search) => {
                            blocks.iter().filter(|b| b.fence > start && b.fence < span).collect()
                        }
                        _ => Vec::new(),
                    };
                    let streaming = code && (file.is_some() || !docs.is_empty()) && !files::block_closed(&segment);
                    let pending = replying && streaming;
                    // A file still coming in is laid out again only once
                    // another of its lines is done, not on every chunk: its
                    // diff against the file there is the costly part.
                    // An open block is the reply's last piece, so its last
                    // line is done when the reply ends in a newline.
                    let key = match segment.rfind('\n') {
                        Some(end) if streaming && !self.text.ends_with('\n') => segment[..end].to_string(),
                        _ => segment.clone(),
                    };
                    let path = file.map(|f| f.path.clone()).or_else(|| {
                        (!docs.is_empty()).then(|| docs.iter().map(|d| d.file.path.as_str()).collect::<Vec<_>>().join("\n"))
                    });
                    let whole = path.as_ref().is_some_and(|p| self.whole.contains(p));
                    let part = match pieces.get(i) {
                        Some(p)
                            if p.start == start
                                && p.width == width
                                && p.path == path
                                && p.whole == whole
                                && p.key == key
                                && p.pending == pending =>
                        {
                            p.lines.clone()
                        }
                        _ => match (code, file) {
                            (true, Some(_)) if block.is_some_and(|b| b.edit.is_some()) => {
                                self.edit_part(block.unwrap(), &blocks, pending, &segment, width)
                            }
                            (true, Some(file)) if pending => streaming_part(&file.path, &file.content, false, width),
                            (true, Some(file)) => self.file_part(&file.path, &file.content, false, streaming, &segment, width),
                            (true, None) if !docs.is_empty() => self.heredoc_part(&segment, start, &docs, replying, width),
                            (true, None) => code_lines(&segment, width),
                            (false, _) => prose_lines(&segment, width),
                        },
                    };
                    laid.push(Piece { start, width, path, whole, key, pending, lines: part.clone() });
                    if part.is_empty() {
                        continue;
                    }
                    if !lines.is_empty() {
                        lines.push(Line::default());
                    }
                    lines.extend(part);
                }
                *pieces = laid;
                *rendered = Some((self.text.len(), width, streaming, lines));
            }
        }
    }

    /// A file the reply gives, `content` in place of the file there or, by
    /// a `>>` heredoc, `append`ed to it: its change as a diff. `code` is the
    /// block that gives it, shown as it is when it holds less than the file.
    fn file_part(&self, path: &str, content: &str, append: bool, streaming: bool, code: &str, width: u16) -> Vec<Line<'static>> {
        let old = self.old(path);
        let new = match (&old, append) {
            (Some(old), true) => format!("{old}{content}"),
            _ => content.to_string(),
        };
        match old.as_deref() {
            // A comment standing in for lines of the file: not written, see
            // `apply_reply`, and shown as it is, not as lines taken out.
            Some(old) if !streaming && files::elides(old, &new) => {
                let mut part = vec![Line::from(vec![
                    Span::styled("← ", dim()),
                    Span::styled(path.to_string(), Style::new().bold()),
                    Span::styled(" · leaves lines out, so not written over the file", dim()),
                ])];
                part.extend(code_lines(code, width));
                part
            }
            // Holding less than the file, it isn't all of it: shown as it
            // is, since as a change the rest of the file would read as
            // taken out; and not written, see `reply_files`.
            Some(old) if !append && !streaming && !self.whole.iter().any(|w| w == path) && files::partial(old, &new) => {
                let mut part = vec![Line::from(vec![
                    Span::styled("← ", dim()),
                    Span::styled(path.to_string(), Style::new().bold()),
                    Span::styled(" · less than the file, so not written over it", dim()),
                ])];
                part.extend(code_lines(code, width));
                part
            }
            _ => diff_lines(&files::diff(path, old.as_deref(), &new, streaming), width),
        }
    }

    /// A shell block, `segment` from line `start` of the reply, whose
    /// heredocs `docs` write files: each heredoc as its file's change, the
    /// commands around them as code.
    fn heredoc_part(&self, segment: &str, start: usize, docs: &[&files::Block], replying: bool, width: u16) -> Vec<Line<'static>> {
        let lines: Vec<&str> = segment.lines().collect();
        let opener = lines.first().copied().unwrap_or("```");
        let fence = files::opening_fence(opener).map_or_else(|| "```".to_string(), |(f, _)| f);
        let body_end = if files::block_closed(segment) { lines.len() - 1 } else { lines.len() };
        let mut out: Vec<Line<'static>> = Vec::new();
        let push = |out: &mut Vec<Line<'static>>, part: Vec<Line<'static>>| {
            if part.is_empty() {
                return;
            }
            if !out.is_empty() {
                out.push(Line::default());
            }
            out.extend(part);
        };
        let shell = |from: usize, to: usize| {
            let chunk = &lines[from.min(to)..to];
            if chunk.iter().all(|l| l.trim().is_empty()) {
                return Vec::new();
            }
            code_lines(&format!("{}\n{}\n{}", opener, chunk.join("\n"), fence), width)
        };
        let mut at = 1;
        for doc in docs {
            let from = (doc.fence - start).clamp(at, body_end);
            let to = (doc.end - start).clamp(from, body_end);
            push(&mut out, shell(at, from));
            let code = format!("{}\n{}\n{}", opener, lines[from..to].join("\n"), fence);
            let part = if replying && !doc.closed {
                streaming_part(&doc.file.path, &doc.file.content, doc.append, width)
            } else {
                self.file_part(&doc.file.path, &doc.file.content, doc.append, !doc.closed, &code, width)
            };
            push(&mut out, part);
            at = to;
        }
        push(&mut out, shell(at, body_end));
        out
    }

    /// An edit block `block` of the reply's `blocks`, `segment` its text:
    /// streaming, its search and replace text as they come; done, its
    /// change to the file, as the reply's blocks before it left it; or why
    /// it can't be made.
    fn edit_part(&self, block: &files::Block, blocks: &[files::Block], pending: bool, segment: &str, width: u16) -> Vec<Line<'static>> {
        let path = &block.file.path;
        // Still coming, its SEARCH/REPLACE text is held back: the change
        // shows as a diff once the block's done.
        if pending {
            let received = segment.lines().count().saturating_sub(1);
            return vec![Line::from(vec![
                Span::styled("● ", dim()),
                Span::styled(path.clone(), Style::new().bold()),
                Span::styled(format!(" · editing… {} line{}", received, if received == 1 { "" } else { "s" }), dim()),
            ])];
        }
        // The file as the blocks before this one leave it.
        let mut before = self.old(path);
        for earlier in blocks.iter().filter(|b| b.fence < block.fence && b.file.path == *path) {
            before = match &earlier.edit {
                Some(edits) => files::apply_edits(before.as_deref(), edits).ok().or(before),
                None if earlier.append => Some(format!("{}{}", before.unwrap_or_default(), earlier.file.content)),
                None => Some(earlier.file.content.clone()),
            };
        }
        match files::apply_edits(before.as_deref(), block.edit.as_deref().unwrap_or_default()) {
            Ok(after) => diff_lines(&files::diff(path, before.as_deref(), &after, false), width),
            Err(why) => {
                let mut part = vec![Line::from(vec![
                    Span::styled("← ", dim()),
                    Span::styled(path.clone(), Style::new().bold()),
                    Span::styled(format!(" · not changed: {}", files::plainly(&why)), Style::new().fg(Color::Yellow)),
                ])];
                part.extend(code_lines(segment, width));
                part
            }
        }
    }

    /// The file at `path` under the reply's folder as it was before the
    /// reply, read the first time it's asked for; none when it's new.
    fn old(&self, path: &str) -> Option<String> {
        let root = self.root.as_ref()?;
        self.olds.borrow_mut().entry(path.to_string()).or_insert_with(|| std::fs::read_to_string(root.join(path)).ok()).clone()
    }
}

/// A file a reply is still giving: its name, then its code as it streams
/// in. Nothing is written yet; once its block is done it shows as its
/// change, and it's written when the reply is in.
fn streaming_part(path: &str, content: &str, append: bool, width: u16) -> Vec<Line<'static>> {
    let lines = content.lines().count();
    let mut part = vec![Line::from(vec![
        Span::styled("● ", dim()),
        Span::styled(path.to_string(), Style::new().bold()),
        Span::styled(
            format!(" · {}{} line{}", if append { "adding " } else { "" }, lines, if lines == 1 { "" } else { "s" }),
            dim(),
        ),
    ])];
    let lang = std::path::Path::new(path).extension().and_then(|e| e.to_str()).unwrap_or("");
    let fence = if content.lines().any(|l| l.trim_start().starts_with("```")) { "````" } else { "```" };
    part.extend(code_lines(&format!("{fence}{lang}\n{}\n{fence}", content.trim_end_matches('\n')), width));
    part
}

/// `text` split into its prose and its fenced code blocks, in order, each
/// with whether it's code and the line it starts on. A fence left open
/// runs to the end, as it does while a reply streams.
fn segments(text: &str) -> Vec<(bool, String, usize)> {
    let lines: Vec<&str> = text.lines().collect();
    let mut out: Vec<(bool, String, usize)> = Vec::new();
    let (mut prose, mut i) = (0, 0);
    while i < lines.len() {
        // SEARCH/REPLACE pairs with no fence are an edit all the same:
        // drawn as a block, fenced here, not run together as prose.
        if let Some((end, closed)) = files::bare_edit(&lines, i) {
            if prose < i {
                out.push((false, lines[prose..i].join("\n"), prose));
            }
            let body = &lines[i..end];
            let longest = body.iter().map(|l| l.trim_start().chars().take_while(|c| *c == '`').count()).max().unwrap_or(0);
            let fence = "`".repeat(longest.max(2) + 1);
            let close = if closed { format!("\n{fence}") } else { String::new() };
            out.push((true, format!("{fence}\n{}{close}", body.join("\n")), i));
            i = end;
            prose = end;
            continue;
        }
        let Some((fence, info)) = files::opening_fence(lines[i]) else {
            i += 1;
            continue;
        };
        if prose < i {
            out.push((false, lines[prose..i].join("\n"), prose));
        }
        // A fence in a heredoc's body is the body's: see `files::close_of`.
        let end = files::close_of(&lines, i, &fence, info).map_or(lines.len(), |c| c + 1);
        out.push((true, lines[i..end].join("\n"), i));
        i = end;
        prose = end;
    }
    if prose < lines.len() {
        out.push((false, lines[prose..].join("\n"), prose));
    }
    out
}

/// `markdown` laid out at `width`, links and bare web addresses clickable.
fn prose_lines(markdown: &str, width: u16) -> Vec<Line<'static>> {
    let options = tui_markdown::Options::new(Markdown).table_width(width).table_row_dividers(true);
    let (marked, urls) = crate::links::mark_links(markdown);
    let mut lines: Vec<Line<'static>> = tui_markdown::from_str_with_options(&marked, &options)
        .lines
        .into_iter()
        .map(|line| {
            let spans = line.spans.into_iter().map(|s| Span::styled(s.content.into_owned(), s.style));
            Line::from(spans.collect::<Vec<_>>()).style(line.style)
        })
        .collect();
    crate::links::resolve(&mut lines, &urls);
    lines
}

/// Base16 Ocean Dark, the theme tui-markdown highlights code in: its
/// background and plain text. Behind every code line, the colours read
/// the same on a light terminal as on a dark one, and a block shows as one.
const CODE_BG: Color = Color::Rgb(43, 48, 59);
const CODE_FG: Color = Color::Rgb(192, 197, 206);
/// Columns a tab in code takes it to.
const CODE_TAB: usize = 4;
/// Columns of background either side of the code in a block.
const CODE_PAD: usize = 2;

/// A fenced code block laid out as a block: each line in the theme's
/// colours on its background, tabs expanded, split into rows of `width`
/// rather than wrapped as prose, each row filled to the width, with
/// `CODE_PAD` columns of background either side of the code and a row of
/// it below.
fn code_lines(markdown: &str, width: u16) -> Vec<Line<'static>> {
    let mut rows = Vec::new();
    for line in tui_markdown::from_str_with_options(markdown, &tui_markdown::Options::new(Markdown)).lines {
        let mut column = 0;
        let mut spans: Vec<Span<'static>> = Vec::new();
        for span in line.spans {
            let mut text = String::new();
            for c in span.content.chars() {
                if c == '\t' {
                    let n = CODE_TAB - column % CODE_TAB;
                    text.extend(std::iter::repeat_n(' ', n));
                    column += n;
                } else {
                    text.push(c);
                    column += unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
                }
            }
            let mut style = span.style.bg(CODE_BG);
            if style.fg.is_none() {
                style = style.fg(CODE_FG);
            }
            spans.push(Span::styled(text, style));
        }
        rows.push((spans, CODE_BG));
    }
    block_lines(rows, width)
}

/// Background of a line put in by a change, and of one taken out.
const ADDED_BG: Color = Color::Rgb(31, 58, 43);
const REMOVED_BG: Color = Color::Rgb(69, 37, 41);

/// A file's change laid out as a block, the way opencode shows an edit:
/// a line naming the file with what the change adds and removes, then
/// each hunk under its `@@` header, every line numbered in the old and
/// the new file, those put in on a green background and those taken out
/// on a red one, with their sign ahead of them.
fn diff_lines(diff: &files::Diff, width: u16) -> Vec<Line<'static>> {
    use files::DiffLine;
    let mut head = vec![
        Span::styled(if diff.created { "# " } else { "← " }, dim()),
        Span::styled(diff.path.clone(), Style::new().bold()),
        Span::styled(format!("  +{}", diff.added), Style::new().fg(Color::Green)),
    ];
    if !diff.created {
        head.push(Span::styled(format!(" −{}", diff.removed), Style::new().fg(Color::Red)));
    }
    let mut lines = vec![Line::from(head)];
    // The gutter holds the old and the new line number, as wide as the
    // largest; a new file's lines have only the new one.
    let largest = diff.lines.iter().map(|l| match l {
        DiffLine::Keep(o, n, _) => (*o).max(*n),
        DiffLine::Add(n, _) => *n,
        DiffLine::Remove(o, _) => *o,
        _ => 0,
    }).max().unwrap_or(0);
    let digits = largest.to_string().len().max(2);
    let number = |n: Option<usize>| match n {
        Some(n) => format!("{:>digits$}", n),
        None => " ".repeat(digits),
    };
    let gutter = |old: Option<usize>, new: Option<usize>, bg: Color| {
        let text = if diff.created { format!("{} ", number(new)) } else { format!("{} {} ", number(old), number(new)) };
        Span::styled(text, Style::new().fg(CODE_FG).bg(bg).add_modifier(Modifier::DIM))
    };
    let code = |text: &str, bg: Color| Span::styled(expand_tabs(text, CODE_TAB), Style::new().fg(CODE_FG).bg(bg));
    let rows: Vec<(Vec<Span<'static>>, Color)> = diff
        .lines
        .iter()
        .map(|line| match line {
            DiffLine::Hunk { old, new } => {
                let header = format!("@@ -{},{} +{},{} @@", old.0, old.1, new.0, new.1);
                (vec![Span::styled(header, Style::new().fg(CODE_FG).bg(CODE_BG).add_modifier(Modifier::DIM))], CODE_BG)
            }
            DiffLine::Keep(old, new, text) => {
                (vec![gutter(Some(*old), Some(*new), CODE_BG), Span::styled(" ", Style::new().bg(CODE_BG)), code(text, CODE_BG)], CODE_BG)
            }
            DiffLine::Add(new, text) => (
                vec![
                    gutter(None, Some(*new), ADDED_BG),
                    Span::styled("+", Style::new().fg(Color::Green).bg(ADDED_BG)),
                    code(text, ADDED_BG),
                ],
                ADDED_BG,
            ),
            DiffLine::Remove(old, text) => (
                vec![
                    gutter(Some(*old), None, REMOVED_BG),
                    Span::styled("-", Style::new().fg(Color::Red).bg(REMOVED_BG)),
                    code(text, REMOVED_BG),
                ],
                REMOVED_BG,
            ),
            DiffLine::Fold(n) => (
                vec![Span::styled(format!("… {} more lines", n), Style::new().fg(CODE_FG).bg(CODE_BG).add_modifier(Modifier::DIM))],
                CODE_BG,
            ),
        })
        .collect();
    lines.extend(block_lines(rows, width));
    lines
}

/// Rows of spans laid out as a block: each split into rows of `width`
/// rather than wrapped as prose, each row filled with its background to
/// the width, with `CODE_PAD` columns of background either side and a row
/// of it below.
fn block_lines(rows: Vec<(Vec<Span<'static>>, Color)>, width: u16) -> Vec<Line<'static>> {
    let pad = if width as usize > 2 * CODE_PAD { CODE_PAD } else { 0 };
    let width = (width as usize).saturating_sub(2 * pad);
    let mut out = Vec::new();
    for (spans, bg) in rows {
        if width == 0 {
            out.push(Line::from(spans));
            continue;
        }
        let (mut row, mut used): (Vec<Span<'static>>, usize) = (margin(pad, bg), 0);
        for span in spans {
            let mut piece = String::new();
            for c in span.content.chars() {
                let w = unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
                if used + w > width && used > 0 {
                    row.push(Span::styled(std::mem::take(&mut piece), span.style));
                    out.push(filled(std::mem::replace(&mut row, margin(pad, bg)), used, width + pad, bg));
                    used = 0;
                }
                piece.push(c);
                used += w;
            }
            if !piece.is_empty() {
                row.push(Span::styled(piece, span.style));
            }
        }
        out.push(filled(row, used, width + pad, bg));
    }
    if width > 0 && !out.is_empty() {
        out.push(filled(margin(pad, CODE_BG), 0, width + pad, CODE_BG));
    }
    out
}

/// The key that opens the running commands: `ctrl+b`, which under tmux
/// is its prefix, so there it's pressed twice, as Claude Code has it.
fn tasks_key() -> &'static str {
    if std::env::var_os("TMUX").is_some() { "ctrl+b ctrl+b" } else { "ctrl+b" }
}

/// The `pad` columns of background `bg` a code row starts with.
fn margin(pad: usize, bg: Color) -> Vec<Span<'static>> {
    if pad == 0 { Vec::new() } else { vec![Span::styled(" ".repeat(pad), Style::new().fg(CODE_FG).bg(bg))] }
}

/// `row`, `used` columns of code past its margin, filled with `bg` to
/// `width` more.
fn filled(mut row: Vec<Span<'static>>, used: usize, width: usize, bg: Color) -> Line<'static> {
    if used < width {
        row.push(Span::styled(" ".repeat(width - used), Style::new().fg(CODE_FG).bg(bg)));
    }
    Line::from(row)
}

/// tui-markdown's styles, but with table borders dim like ours (its dark
/// gray is the background color in some themes), headings and code without
/// their `#` and ``` markers. Links don't reach it: `links::mark_links`
/// draws them.
#[derive(Clone)]
struct Markdown;

impl tui_markdown::StyleSheet for Markdown {
    /// Dim, like our other borders, in place of the default dark gray,
    /// which is the background color in some themes and hides the table.
    fn table_border(&self) -> Style {
        dim()
    }

    /// The ``` drawn above and below a code block by default; none drops
    /// both lines.
    fn code_block_fence(&self) -> &str {
        ""
    }

    /// The `#`s drawn before a heading by default; none drops them.
    fn heading_marker(&self, _level: u8) -> &str {
        ""
    }
}

enum Overlay {
    None,
    Models { rows: Option<Result<Vec<ModelRow>, String>>, selected: usize },
    /// `/mcp`: the project's MCP servers.
    Mcp { rows: Option<Result<Vec<mcp::Server>, String>>, selected: usize },
    /// `/project`: the projects to switch to, then "+ New project" and
    /// "+ Import project".
    /// `deleting` once ctrl+d has asked to delete the selected one, until
    /// it's pressed again.
    Projects { rows: Option<Result<Vec<Project>, String>>, selected: usize, deleting: bool },
    Help,
    /// `/status`: rows fill in once the agent answers.
    Status(Option<Vec<(String, String)>>),
    /// `/usage`: this month's credits, re-read on opening (`loading` until
    /// the agent answers).
    Usage { loading: bool },
    /// A shell run, clicked: all it printed, under the prompt.
    Shell(usize),
    /// `ctrl+b`: the commands still running, one selected from the
    /// keyboard, to expand or stop.
    Jobs { selected: usize },
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
    /// Where the files its reply gives are written: only for a turn asked
    /// to make them.
    files_dir: Option<std::path::PathBuf>,
    /// The files the agent was shown whole, which its reply may give back.
    whole: Vec<String>,
    /// The tool calls the reply ended with, run once it's in.
    calls: Vec<chat::ToolCall>,
}

/// What a turn sent, kept for its tool rounds: each round sends it again,
/// with the calls and their results so far after the user's message.
#[derive(Clone, Default)]
struct Sent {
    system: Option<String>,
    message: String,
    /// Ids of the files uploaded with it.
    attachments: Vec<String>,
    cwd: Option<std::path::PathBuf>,
    files_dir: Option<std::path::PathBuf>,
    whole: Vec<String>,
}

/// A turn's native tool calling: the reply asks for `bash` calls, they run
/// as a reply's fenced commands do, asked about first unless auto-approve
/// is on, and their results go back in a turn of their own so the agent
/// goes on, for `TOOL_ROUNDS` rounds at most.
#[derive(Default)]
struct Tooling {
    sent: Sent,
    /// The assistant and tool messages of the rounds so far.
    rounds: Vec<serde_json::Value>,
    /// Rounds sent back so far.
    round: u32,
    /// The last reply's calls, waiting on their results.
    calls: Vec<Call>,
    /// The command whose run failed last, to tell one failing again.
    failed: Option<String>,
    /// Why the next round isn't sent, once it's known it won't be.
    stop: Option<String>,
}

/// One tool call of a reply, as it runs.
struct Call {
    id: String,
    /// The command it runs; none for a call that won't run.
    command: Option<String>,
    /// The shell run it's shown as, once started.
    run: Option<usize>,
    /// What goes back to the agent, once known.
    result: Option<String>,
    /// What the user's next prompt carries instead, should the rounds stop
    /// here: how the run ended, as for a `!command`.
    context: Option<String>,
}

impl Tooling {
    /// A reply of `text` ended calling `calls`: they're kept, with the
    /// reply as the round's assistant message. The commands to run, and
    /// the text to show them in the reply with, as fenced commands show.
    /// A call that can't run, of an unknown tool or with no command, has
    /// its result at once.
    fn called(&mut self, text: &str, calls: Vec<chat::ToolCall>) -> (Vec<String>, String) {
        let content = if text.trim().is_empty() { serde_json::Value::Null } else { text.into() };
        self.rounds.push(serde_json::json!({
            "role": "assistant",
            "content": content,
            "tool_calls": calls.iter().map(chat::ToolCall::to_json).collect::<Vec<_>>(),
        }));
        let (mut commands, mut shown) = (Vec::new(), String::new());
        self.calls = calls
            .into_iter()
            .map(|call| {
                let mut c = Call { id: call.id, command: None, run: None, result: None, context: None };
                match bash_command(&call.name, &call.arguments) {
                    Ok((command, description)) => {
                        let note = description.map_or_else(String::new, |d| format!("# {}\n", d.replace('\n', " ")));
                        shown.push_str(&format!("\n\n```sh\n{}{}\n```\n", note, command));
                        // The same command twice in one reply runs once.
                        if commands.contains(&command) {
                            c.result = Some("Not run: the same command as another call of this reply; see its result.".into());
                        } else {
                            commands.push(command.clone());
                            c.command = Some(command);
                        }
                    }
                    Err(why) => c.result = Some(why),
                }
                c
            })
            .collect();
        (commands, shown)
    }

    /// The call waiting to run `command`, not started yet.
    fn waiting(&mut self, command: &str) -> Option<&mut Call> {
        self.calls.iter_mut().find(|c| c.command.as_deref() == Some(command) && c.run.is_none() && c.result.is_none())
    }

    /// Give the calls of `commands` not run, nor started, `result`.
    fn resolve(&mut self, commands: &[String], result: &str) {
        for command in commands {
            while let Some(call) = self.waiting(command) {
                call.result = Some(result.to_string());
            }
        }
    }
}

/// The command a `bash` call runs, and what it says the command does; an
/// error, for the agent, for a call of another tool or with no command.
fn bash_command(name: &str, arguments: &str) -> Result<(String, Option<String>), String> {
    if name != "bash" {
        return Err(format!("Error: there's no tool named `{}`; the only tool is `bash`.", name));
    }
    let arguments: serde_json::Value = serde_json::from_str(arguments)
        .map_err(|e| format!("Error: the arguments aren't valid JSON ({}); give {{\"command\": \"...\"}}.", e))?;
    let command = arguments["command"].as_str().map(str::trim).filter(|c| !c.is_empty());
    let Some(command) = command else {
        return Err("Error: no `command` given; give {\"command\": \"...\"}.".to_string());
    };
    let description = arguments["description"].as_str().map(str::trim).filter(|d| !d.is_empty()).map(str::to_string);
    Ok((command.to_string(), description))
}

/// What a tool call that ran gives back: how it ended, then what it
/// printed, as a terminal shows it, its middle cut past `TOOL_KEPT`
/// characters, as an error is at the start or the end.
fn tool_output(output: &str, ended: Option<Ended>) -> String {
    let status = match ended {
        Some(Ended::Exited(code)) => format!("Exit code: {}", code),
        Some(Ended::Killed) => "Killed by a signal".to_string(),
        Some(Ended::Stopped) => "Stopped by the user".to_string(),
        None => "Still running in the background; its output so far".to_string(),
    };
    let lines: Vec<String> =
        output.lines().map(|l| trace_of(l).map_or_else(|| l.to_string(), |c| format!("$ {}", c))).collect();
    let printed = lines.join("\n");
    let printed = if printed.trim().is_empty() { "(no output)".to_string() } else { head_tail(&printed, TOOL_KEPT) };
    format!("{}\n{}", status, printed)
}

/// `text` with its middle cut, past `n` characters: the first and last
/// halves of `n` kept, and a note of how much went.
fn head_tail(text: &str, n: usize) -> String {
    let count = text.chars().count();
    if count <= n {
        return text.to_string();
    }
    let half = n / 2;
    let head_end = text.char_indices().nth(half).map_or(text.len(), |(i, _)| i);
    let tail_start = text.char_indices().nth(count - half).map_or(text.len(), |(i, _)| i);
    format!("{}\n…({} characters cut)…\n{}", &text[..head_end], count - 2 * half, &text[tail_start..])
}

/// The working directory's sync to `project`, running until `stop`.
/// A check of the project in `project`, shown in shell entry `id`.
struct Check {
    id: usize,
    project: String,
    command: String,
    /// Asked of the language servers, not run: the folder and the files,
    /// to check with the build once they're clean.
    servers: Option<(std::path::PathBuf, Vec<String>)>,
}

/// What a check's end calls for, by its exit code and whether the round
/// before got anywhere.
#[derive(Debug, PartialEq)]
enum AfterCheck {
    Passed,
    /// Ask the agent to fix it: the round this makes.
    Fix(u32),
    /// The last fix changed nothing in what the check says: left to the
    /// user, as another round would too.
    Stalled,
}

/// The loop goes on as long as the errors are not fixed, round after round,
/// and stops only when a round made no difference: the check's `output` is
/// what it was after the last failure.
/// Words for a reply being written, when the model writing it isn't known.
const WORKING_WORDS: [&str; 12] = [
    "Generating",
    "Composing",
    "Crafting",
    "Drafting",
    "Working",
    "Thinking",
    "Pondering",
    "Brewing",
    "Cooking",
    "Assembling",
    "Tinkering",
    "Conjuring",
];

/// One of `WORKING_WORDS`, by when the turn `started`: the same all
/// through a turn, another the next.
fn working_word(started: Instant) -> &'static str {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    started.hash(&mut hasher);
    WORKING_WORDS[(hasher.finish() % WORKING_WORDS.len() as u64) as usize]
}

/// Errors from the language servers shown, and sent to the agent.
const DIAGNOSTICS_SHOWN: usize = 30;

/// Times the agent's asked again for edits that didn't apply, before it's
/// left to the user.
const EDIT_ROUNDS: u32 = 2;
/// Characters of a file sent back with an edit that didn't apply.
const EDIT_SHOWN: usize = 40_000;

fn after_check(code: Option<i32>, output: &str, fix_round: u32, last_failure: Option<&str>) -> AfterCheck {
    match code {
        Some(0) => AfterCheck::Passed,
        _ if last_failure == Some(output) => AfterCheck::Stalled,
        _ => AfterCheck::Fix(fix_round + 1),
    }
}

struct Sync {
    project: String,
    root: std::path::PathBuf,
    stop: Arc<AtomicBool>,
    /// Set to have the next pass run now, as after the chat writes files.
    nudge: Arc<AtomicBool>,
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
    /// Set while a turn runs, so the background sync waits for it.
    busy: Arc<AtomicBool>,
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
    /// Each entry's height as last laid out, by index: the width and the
    /// entry's version it was measured at, and its rows.
    heights: std::cell::RefCell<Vec<Option<(u16, u64, usize)>>>,
    /// The conversation pane as last drawn, row by row, for copying a
    /// selection out of it, and the link under each cell, for clicking.
    shown: std::cell::RefCell<Shown>,
    /// Text being selected with the mouse, in screen cells.
    selection: Option<Selection>,
    /// Where the left button went down in the footer, for a click on an
    /// action there, which isn't selected over.
    pressed: Option<(u16, u16)>,
    /// What the last selection copied, flashed in the footer for a moment.
    copied: Option<(usize, Instant)>,
    /// The link last clicked, flashed in the footer as it opens.
    opened: Option<(String, Instant)>,
    /// Kept for the app's life: on X11 the clipboard empties when its owner
    /// goes away.
    clipboard: Option<arboard::Clipboard>,
    ticks: usize,
    /// The prompt takes a token for `/login`, masked, instead of a message.
    token_entry: bool,
    /// Files dropped on the prompt, each shown there as a chip such as
    /// `[Image 1]` until the message is sent.
    attachments: Vec<Attachment>,
    /// Prompts sent while a turn (or what follows it) was still going, in
    /// order: each goes once everything before it is done. Esc takes the
    /// last one back before it interrupts.
    queued: Vec<Queued>,
    /// `!command` runs not yet sent: the next prompt carries them to the
    /// agent, so they stay in the session's history.
    shell_context: Vec<String>,
    /// Ids for `!command` runs, so each result finds its entry.
    shell_runs: usize,
    /// The language servers started this session, kept for the next turn.
    lsp: Arc<lsp::Lsp>,
    /// Checks asked of the language servers, for their ids.
    lsp_runs: usize,
    /// The shell runs still going, which `/stop` ends, and quitting too.
    jobs: Vec<Job>,
    /// The check running on the project after a reply's files were
    /// written, if one is.
    check: Option<Check>,
    /// Rounds of fixing a failed check since the user's last prompt.
    fix_round: u32,
    /// The files the last reply wrote as it streamed, each with whether
    /// it made it and the lines added and removed: reported and checked
    /// with the rest once it's in.
    early: Vec<Early>,
    /// Rounds the agent's been asked again for edits that didn't apply,
    /// since the user last asked something.
    edit_round: u32,
    /// Whether the agent's been asked again for the files a reply only
    /// described, in a folder still empty, since the user last asked.
    files_asked: bool,
    /// Whether the agent's been asked for the manifest its source needs,
    /// such as `Cargo.toml`, since the user last asked.
    manifest_asked: bool,
    /// The widest context window among the project's enabled models, in
    /// tokens, once the models are known: how much of the project a prompt
    /// may carry.
    window: Option<u64>,
    /// The project's enabled models, once read; empty asks for one on the
    /// card.
    enabled_models: Option<Vec<String>>,
    /// What the check said when it last failed, to tell a round that fixed
    /// nothing.
    last_failure: Option<String>,
    /// The last reply's shell commands, with their folder, waiting for the
    /// check of its files to pass before they run.
    commands: Option<(std::path::PathBuf, Vec<String>)>,
    /// Those commands once checked, waiting to be allowed: Enter runs
    /// them, Tab runs them and every later reply's too, Esc skips them.
    ask: Option<(std::path::PathBuf, Vec<String>)>,
    /// Allowed for the session: a reply's commands run without asking.
    always_run: bool,
    /// The last turn's tool calling, when it works in a folder of the
    /// user's.
    tooling: Option<Tooling>,
    /// Rounds of fixing a reply's failed command since the user's last
    /// prompt, so a command that keeps failing doesn't go on forever.
    command_round: u32,
    /// The highlighted slash-command hint while typing a command.
    hint_at: usize,
    /// The project's skills as `(name, description)`, hinted and run as
    /// `/<name>`.
    skills: Vec<(String, String)>,
    /// What the agent is doing, as the welcome card shows it; `None` until
    /// first asked.
    agent: Option<crate::AgentState>,
    /// Who's signed in, as the welcome card shows it: `None` until known,
    /// then the account or `None` for signed out.
    account: Option<Option<String>>,
    /// The project's MCP servers, once read: ready, and still connecting.
    connections: Option<(usize, usize)>,
    /// When the agent's state was last asked for; `None` while asking.
    agent_asked: Option<Instant>,
    /// The working directory's sync, while it runs.
    sync: Option<Sync>,
    /// This month's web search usage, shown in the footer once known.
    search: Option<SearchUsage>,
    /// A newer release's version, for the footer to ask for an upgrade.
    update: Option<String>,
    /// Whether this project's models were read since opening it, so the
    /// nudge to pick one comes once.
    models_checked: bool,
    /// This month's credits, read with `search`.
    credits: Option<CreditUsage>,
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
            busy: Arc::new(AtomicBool::new(false)),
            downloads: Vec::new(),
            overlay: Overlay::None,
            input: String::new(),
            cursor: 0,
            history: Vec::new(),
            history_at: None,
            last_esc: None,
            scroll_back: 0,
            scroll_limit: std::cell::Cell::new(0),
            heights: std::cell::RefCell::new(Vec::new()),
            shown: std::cell::RefCell::new(Shown::default()),
            selection: None,
            pressed: None,
            copied: None,
            opened: None,
            clipboard: None,
            ticks: 0,
            token_entry: false,
            attachments: Vec::new(),
            queued: Vec::new(),
            shell_context: Vec::new(),
            shell_runs: 0,
            lsp: Arc::default(),
            lsp_runs: 0,
            jobs: Vec::new(),
            check: None,
            fix_round: 0,
            edit_round: 0,
            files_asked: false,
            manifest_asked: false,
            early: Vec::new(),
            window: None,
            enabled_models: None,
            commands: None,
            ask: None,
            always_run: false,
            tooling: None,
            command_round: 0,
            last_failure: None,
            hint_at: 0,
            skills: Vec::new(),
            agent: None,
            account: None,
            connections: None,
            agent_asked: Some(Instant::now() - AGENT_EVERY),
            sync: None,
            search: None,
            update: None,
            models_checked: false,
            credits: None,
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
            self.send_queued();
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
                _ = tick.tick() => {
                    self.ticks += 1;
                    self.poll_agent();
                }
            }
        }
        // Nothing started here outlives it: a dev server left behind would
        // hold its port, unseen.
        self.stop_jobs(|_| true);
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

    /// Keep the current project's folder in step with it, when it has one
    /// not paused; a sync already running stops.
    fn follow_project(&mut self) {
        self.stop_sync();
        let Some(project) = self.project.clone() else { return };
        let Some(root) = project_root(&project.0) else { return };
        let Some(link) = docent::linked(&root) else { return };
        // A folder docent linked is docent's project: one made before
        // projects carried a client is marked so now, and stays out of
        // Studio's list. Quietly: an agent that doesn't know the field yet
        // refuses, and the next run tries again.
        if link.project == project.0 && !cfg!(test) {
            let id = project.0.clone();
            self.spawn(move |client, _| {
                let _ = crate::try_claim_project(&client, &id);
            });
        }
        if !link.paused {
            self.start_sync(root, project, false);
        }
    }

    /// Keep `root` in step with `project`, linking it first when `link`
    /// says to; a sync already running stops. One away from the working
    /// directory says where it is.
    fn start_sync(&mut self, root: std::path::PathBuf, project: (String, String), link: bool) {
        self.stop_sync();
        let stop = Arc::new(AtomicBool::new(false));
        let nudge = Arc::new(AtomicBool::new(false));
        self.sync = Some(Sync { project: project.0.clone(), root: root.clone(), stop: stop.clone(), nudge: nudge.clone() });
        if link || sync_root().is_none_or(|here| here != root) {
            self.entries.push(Entry::Notice(format!("Syncing {} to project {}", root.display(), project.1)));
        }
        if cfg!(test) {
            return;
        }
        let busy = self.busy.clone();
        self.spawn(move |client, tx| {
            let linked = if link { docent::link(&root, &project.0) } else { Ok(()) };
            match linked {
                Ok(()) => {
                    let idle = || !busy.load(Ordering::Relaxed);
                    docent::follow(&client, &root, SYNC_INTERVAL, &idle, &stop, &nudge, &mut |event| {
                        // Stopped, say nothing more: the project may have switched.
                        if stop.load(Ordering::Relaxed) {
                            return;
                        }
                        let _ = match event {
                            docent::Event::Working(count) => tx.send(AppEvent::Syncing(count)),
                            docent::Event::Summary(text) => {
                                let _ = tx.send(AppEvent::Syncing(0));
                                // Sent, not yet indexed: the status line shows that next.
                                tx.send(AppEvent::Notice(format!("Synced: {}", text)))
                            }
                            docent::Event::Failure(text) => tx.send(AppEvent::Warning(format!("Sync: {}", text))),
                            docent::Event::Note(text) => tx.send(AppEvent::Notice(format!("Sync: {}", text))),
                        };
                    });
                }
                Err(e) => {
                    let _ = tx.send(AppEvent::Warning(format!("Sync: {}", e)));
                }
            }
            let _ = tx.send(AppEvent::SyncEnded(stop));
        });
    }

    fn stop_sync(&mut self) {
        if let Some(sync) = self.sync.take() {
            sync.stop.store(true, Ordering::Relaxed);
        }
    }

    /// `/sync`: link the working directory to the current project and keep
    /// it in step; `status` and `stop` too, of the folder the project syncs
    /// from. A first sync of a big folder asks for `--yes`, one of a project
    /// linked to another folder for `--move`, and the home folder is never
    /// synced.
    fn sync_command(&mut self, arguments: &[&str]) {
        let Some(here) = sync_root() else {
            return self.entries.push(Entry::Error("No working directory to sync".to_string()));
        };
        let current = self.project.clone();
        // The folder the current project syncs from, wherever it is.
        let root = current.as_ref().and_then(|(id, _)| project_root(id)).unwrap_or_else(|| here.clone());
        match arguments {
            flags if flags.iter().all(|f| matches!(*f, "--yes" | "--move")) => {
                let root = here;
                if let Some(refusal) = docent::refusal(&root) {
                    return self.entries.push(Entry::Error(format!("Not syncing: {}", refusal)));
                }
                let Some(project) = current else {
                    return self.entries.push(Entry::Error("No project to sync to yet".to_string()));
                };
                if self.sync.as_ref().is_some_and(|s| s.project == project.0 && s.root == root) {
                    let text = format!("{} is up to date with project {}", root.display(), project.1);
                    return self.entries.push(Entry::Notice(text));
                }
                if let Some(other) = project_root(&project.0).filter(|other| *other != root)
                    && !flags.contains(&"--move")
                {
                    return self.entries.push(Entry::Error(format!(
                        "Project {} syncs {}; /sync --move syncs {} instead",
                        project.1,
                        other.display(),
                        root.display()
                    )));
                }
                let confirmed = flags.contains(&"--yes");
                self.spawn(move |_, tx| {
                    // Pushed before, to this project: only the changes go.
                    let first = docent::linked(&root).is_none_or(|l| l.project != project.0);
                    let size = docent::measure(&root);
                    if first && size.large() && !confirmed {
                        let _ = tx.send(AppEvent::Warning(format!(
                            "{} has {} documents ({}); /sync --yes pushes them all to project {}",
                            root.display(),
                            size.files,
                            format_size(size.bytes),
                            project.1
                        )));
                        return;
                    }
                    let _ = tx.send(AppEvent::SyncLink(root, project));
                });
            }
            ["status"] => {
                let name = |id: &str| match &current {
                    Some((current, name)) if current == id => name.clone(),
                    _ => id.to_string(),
                };
                let text = match docent::linked(&root) {
                    None => Ok(format!("{} isn't synced; /sync syncs it to the current project", root.display())),
                    Some(link) => docent::status(&root).map(|status| {
                        let state = if link.paused { "Paused syncing" } else { "Syncing" };
                        let mut text = format!(
                            "{} {} to project {}: {} documents, {} skills",
                            state,
                            root.display(),
                            name(&link.project),
                            status.documents,
                            status.skills
                        );
                        match status.changes.len() {
                            0 => text.push_str("; up to date"),
                            n => {
                                let shown = status.changes.iter().take(5).cloned().collect::<Vec<_>>().join(", ");
                                let more = if n > 5 { format!(", and {} more", n - 5) } else { String::new() };
                                text.push_str(&format!("; {} to push: {}{}", n, shown, more));
                            }
                        }
                        text
                    }),
                };
                self.entries.push(match text {
                    Ok(text) => Entry::Notice(text),
                    Err(e) => Entry::Error(e),
                });
            }
            ["stop"] => {
                let root = self.sync.as_ref().map_or(root, |s| s.root.clone());
                self.stop_sync();
                self.spawn(move |_, tx| {
                    let _ = tx.send(match docent::pause(&root) {
                        Ok(()) => AppEvent::Notice(format!("Stopped syncing {}; /sync starts again", root.display())),
                        Err(e) => AppEvent::Error(e),
                    });
                });
            }
            other => self.entries.push(Entry::Error(format!(
                "Unknown /sync {}; try /sync status or /sync stop",
                other.join(" ")
            ))),
        }
    }

    fn load_projects(&self) {
        self.spawn(|client, tx| {
            let _ = tx.send(AppEvent::Projects(crate::try_fetch_projects(&client)));
        });
    }

    /// Re-read the web search usage from the platform. Also the last step
    /// of each turn, which may have spent a search.
    fn load_usage(&self) {
        self.spawn(|client, tx| send_usage(&client, &tx));
    }

    /// Ask GitHub whether a newer release is out, once a session.
    fn check_update(&self) {
        if cfg!(test) {
            return;
        }
        self.spawn(|_, tx| {
            if let Some(version) = crate::update::newer_release() {
                let _ = tx.send(AppEvent::Update(version));
            }
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
    /// what the agent gets. Attached files are uploaded first. A turn that's
    /// `making` files works in the folder the chat was opened in, and the
    /// files its reply gives are written there.
    fn send_turn(&mut self, shown: String, message: String, files: Vec<Attachment>, making: bool) {
        let Some((project, _)) = self.project.clone() else { return };
        let files_dir = if making { chat::making_dir(&project) } else { None };
        // Asked to make something, the reply comes as files to write; in a
        // code project, as code that fits the files already there.
        let mut whole = Vec::new();
        // The machine, the folder and how to give files go as the system
        // message, the request alone as the user's: the agent repeats the
        // user's message in its prompt and reads what's around it as data,
        // and a small model then describes files instead of giving them.
        let mut system = None;
        if let Some(root) = &files_dir {
            let context = match code_kind(root) {
                Some(kind) => {
                    let c = files::project_context(root, kind, &message, files::budget_for(self.window));
                    whole = c.whole;
                    format!("{}\n\n", c.text)
                }
                None if files::is_empty_folder(root) => format!("{}\n\n", files::EMPTY_FOLDER),
                None => String::new(),
            };
            system = Some(format!(
                "{}\n{}\n\n{}{}\n\n{}",
                files::system_context(),
                files::folder_note(root),
                context,
                files::HINT,
                files::EDIT_EXAMPLE
            ));
            // Its language server starts now, indexing while the reply comes,
            // to be ready for the files it gives.
            if !cfg!(test)
                && let Some(kind) = files::project_kind(root)
            {
                let (lsp, root, kind) = (self.lsp.clone(), root.clone(), kind.to_string());
                std::thread::spawn(move || lsp.warm(&root, &kind));
            }
        }
        // The framework takes it from here: its orchestrator picks the
        // model and decides whether to search the web, for coding as for
        // anything else. Docent is the harness around its answer.
        let cwd = files_dir.clone().or_else(|| chat::turn_dir(&project));
        self.entries.push(Entry::User(shown));
        if making && files_dir.is_none() {
            let warning = match docent::folder(&project) {
                Some(folder) => format!(
                    "This project is linked to {}, so no files will be written here; open docent there, or /sync this folder",
                    folder.display()
                ),
                None => "This folder syncs to another project, so no files will be written; switch to it with /project".to_string(),
            };
            self.entries.push(Entry::Warning(warning));
        }
        let sent = Sent { system, message, attachments: Vec::new(), cwd, files_dir, whole };
        // A turn in a folder of the user's offers the `bash` tool, and its
        // tool rounds start over: it's a new request, not one going on.
        self.tooling = sent.files_dir.is_some().then(|| Tooling { sent: sent.clone(), ..Default::default() });
        self.start_turn(project, sent, files, Vec::new());
    }

    /// Stream the turn `sent`, with `files` uploaded first and the tool
    /// `rounds` so far after its message, into a new reply.
    fn start_turn(&mut self, project: String, sent: Sent, files: Vec<Attachment>, rounds: Vec<serde_json::Value>) {
        self.entries.push(Entry::Reply(Reply { root: sent.files_dir.clone(), whole: sent.whole.clone(), ..Default::default() }));
        let entry = self.entries.len() - 1;
        self.scroll_back = 0;
        // Made per turn: the agent picks its port once it's up, and a new
        // one each time it restarts.
        let chat = chat::openai_client(crate::api_url());
        let (tx, session, client) = (self.tx.clone(), self.session.clone(), self.client.clone());
        let (files_dir, whole) = (sent.files_dir.clone(), sent.whole.clone());
        let task = tokio::spawn(async move {
            let events = tx.clone();
            let mut on = move |event| {
                let _ = events.send(AppEvent::Chat(event));
            };
            let mut ids = sent.attachments.clone();
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
            // Kept for the turn's tool rounds, which send them again
            // rather than uploading them again.
            if ids.len() > sent.attachments.len() {
                let _ = tx.send(AppEvent::Uploaded(ids.clone()));
            }
            let request = chat::Request {
                system: sent.system.as_deref(),
                message: &sent.message,
                attachments: &ids,
                project_id: &project,
                cwd: sent.cwd.as_deref(),
                session_id: &session,
                tools: sent.files_dir.is_some(),
                rounds: &rounds,
            };
            let result = chat::stream_turn(&chat, &request, &mut on).await;
            let _ = tx.send(AppEvent::TurnDone(result));
        });
        self.turn = Some(Turn {
            started: Instant::now(),
            task,
            entry,
            tokens: 0,
            activity: None,
            files_dir,
            whole,
            calls: Vec::new(),
        });
        self.busy.store(true, Ordering::Relaxed);
    }

    /// Put the project's web search back on after a turn that had it off.
    /// Whether a turn of `prompt` makes files: always in a code project the
    /// chat was opened in, and elsewhere when the prompt asks for something
    /// to be made or names a file of the folder, as a pasted error does.
    fn making(&self, prompt: &str) -> bool {
        let Some((project, _)) = &self.project else { return false };
        files::wants_files(prompt)
            || chat::making_dir(project)
                .is_some_and(|dir| code_kind(&dir).is_some() || files::mentions_file(&dir, prompt))
    }

    /// Write the files a reply gave into the turn's folder, new and changed
    /// alike, as the reply is the edit, then say what changed in each, by
    /// lines; those already as given are left be, and so is a file the
    /// agent wasn't shown `whole` when the reply gives back less of it.
    /// The files written.
    fn reply_files(&mut self, root: &std::path::Path, found: Vec<files::File>, whole: &[String]) -> Vec<String> {
        // Those written as the reply streamed, against the files before it.
        let early = std::mem::take(&mut self.early);
        if found.is_empty() && early.is_empty() {
            return Vec::new();
        }
        let plan = files::plan(root, found, whole);
        if !plan.refused.is_empty() {
            self.entries.push(Entry::Warning(format!("Not written, outside {}: {}", root.display(), plan.refused.join(", "))));
        }
        if !plan.kept.is_empty() {
            let paths: Vec<&str> = plan.kept.iter().map(|f| f.path.as_str()).collect();
            self.entries.push(Entry::Warning(format!(
                "Not written, as the reply holds less than the file and the agent wasn't shown it whole: {}; the change \
                 shows above",
                paths.join(", ")
            )));
        }
        // Each file's lines added and removed, against what's there now.
        let changes: Vec<(&files::File, bool, (usize, usize))> = plan
            .new
            .iter()
            .map(|f| (f, true, files::line_changes("", &f.content)))
            .chain(plan.changed.iter().map(|f| {
                let old = std::fs::read_to_string(root.join(&f.path)).unwrap_or_default();
                (f, false, files::line_changes(&old, &f.content))
            }))
            .collect();
        let (mut written, failed) = files::write(root, &[plan.new.as_slice(), plan.changed.as_slice()].concat());
        if !written.is_empty() || !early.is_empty() {
            self.entries.push(Entry::Notice(format!("Changed in {}:", root.display())));
        }
        let mine = |path: &String| early.iter().any(|e| e.path == *path);
        let reported = early
            .iter()
            .map(|e| {
                let (added, removed) = files::line_changes(e.old.as_deref().unwrap_or(""), &e.new);
                (e.path.as_str(), e.old.is_none(), added, removed)
            })
            .chain(
                changes
                    .iter()
                    .filter(|(f, ..)| written.contains(&f.path) && !mine(&f.path))
                    .map(|(f, created, (added, removed))| (f.path.as_str(), *created, *added, *removed)),
            );
        for (path, created, added, removed) in reported {
            let what = if created { "created" } else { "updated" };
            self.entries.push(Entry::Notice(format!("  {} {} (+{} −{})", what, path, added, removed)));
        }
        for e in &early {
            if !written.contains(&e.path) {
                written.push(e.path.clone());
            }
        }
        for failure in failed {
            self.entries.push(Entry::Error(format!("Failed to write {}", failure)));
        }
        // Documents among them go to the project now, not on the next pass.
        if !written.is_empty()
            && let Some(sync) = &self.sync
        {
            sync.nudge.store(true, Ordering::Relaxed);
        }
        self.scroll_back = 0;
        written
    }

    /// Run `command` with the shell in the working directory, showing its
    /// output as it comes. Ending in `&`, as `npm run dev &`, it's started
    /// in the background: the next prompt doesn't wait for it, and `/stop`
    /// ends it, or a click on its entry.
    fn run_shell(&mut self, command: String) {
        if command.is_empty() {
            return self.entries.push(Entry::Error("Give a command to run: !<command>".to_string()));
        }
        let (command, background) = detached(&command);
        let (id, report) = self.shell_entry(command.clone(), background);
        self.spawn(move |_, tx| {
            let (output, ended) = run_command(&command, None, background, report);
            let _ = tx.send(AppEvent::Shell(id, output, ended));
        });
    }

    /// A shell entry for `command`, and its run as a job: the entry's id,
    /// and what the run's thread reports through.
    fn shell_entry(&mut self, command: String, background: bool) -> (usize, Report) {
        self.shell_runs += 1;
        let id = self.shell_runs;
        let shell = Shell { id, command: command.clone(), output: String::new(), ended: None, background, detached: false };
        self.entries.push(Entry::Shell(shell));
        let stopped = Arc::new(AtomicBool::new(false));
        self.jobs.push(Job { id, command, started: Instant::now(), pid: None, stopped: stopped.clone() });
        self.scroll_back = 0;
        (id, Report { id, tx: self.tx.clone(), stopped })
    }

    /// The shell entry of run `id`, unless a `/clear` took it.
    fn shell_of(&self, id: usize) -> Option<&Shell> {
        self.entries.iter().find_map(|e| match e {
            Entry::Shell(s) if s.id == id => Some(s),
            _ => None,
        })
    }

    /// The shell entry of run `id`, unless a `/clear` took it.
    fn shell_mut(&mut self, id: usize) -> Option<&mut Shell> {
        self.entries.iter_mut().find_map(|e| match e {
            Entry::Shell(s) if s.id == id => Some(s),
            _ => None,
        })
    }

    /// Run `id` reported in: ended, its job is done with; its entry shows
    /// its output and its end, or that it goes on in the background. What
    /// the agent is told of it, for the next prompt.
    fn shell_reported(&mut self, id: usize, output: &str, ended: Option<Ended>) -> Option<String> {
        if ended.is_some() {
            self.jobs.retain(|j| j.id != id);
            // The running commands panel follows the list: an empty one
            // closes it, and the selection stays on a row.
            if let Overlay::Jobs { selected } = &mut self.overlay {
                match self.jobs.len() {
                    0 => self.overlay = Overlay::None,
                    n => *selected = (*selected).min(n - 1),
                }
            }
        }
        let shell = self.shell_mut(id)?;
        let context = shell_context(&shell.command, output, ended, shell.detached);
        shell.output = output.to_string();
        shell.ended = ended;
        shell.detached |= ended.is_none();
        Some(context)
    }

    /// `/stop [word]`: end the shell runs still going, or those with
    /// `word` in their command.
    fn stop_command(&mut self, word: &str) {
        let stopped = self.stop_jobs(|job| word.is_empty() || job.command.contains(word));
        let notice = match (stopped.is_empty(), word.is_empty()) {
            (true, true) => "Nothing is running".to_string(),
            (true, false) => format!("Nothing running has `{}` in it; /stop alone stops everything", word),
            _ => format!("Stopping: {}", stopped.join("; ").replace('\n', "; ")),
        };
        self.entries.push(Entry::Notice(notice));
        self.scroll_back = 0;
    }

    /// Stop the jobs `which` picks: their commands. Each run's thread sees
    /// the flag and ends its run as stopped; the process is ended here as
    /// well, so quitting leaves none behind.
    fn stop_jobs(&self, which: impl Fn(&Job) -> bool) -> Vec<String> {
        let mut stopped = Vec::new();
        for job in self.jobs.iter().filter(|j| which(j)) {
            job.stopped.store(true, Ordering::Relaxed);
            if let Some(pid) = job.pid {
                terminate(pid, false);
            }
            stopped.push(job.command.clone());
        }
        stopped
    }

    /// Check that the project at `root` still builds after `written`
    /// changed, with the command for its kind, or each file's own, as a
    /// shell entry; what comes of it is handled when it ends.
    fn run_check(&mut self, root: &std::path::Path, written: &[String]) {
        // The files written go to their language servers first, as
        // opencode has an edit's errors from the server: quicker than the
        // build, and they're the files' own.
        if !cfg!(test) && lsp::Lsp::serves(written) {
            let Some((project, _)) = self.project.clone() else { return };
            self.lsp_runs += 1;
            let id = self.lsp_runs;
            let servers = Some((root.to_path_buf(), written.to_vec()));
            self.check = Some(Check { id, project, command: "the language server".to_string(), servers });
            let (lsp, dir, written) = (self.lsp.clone(), root.to_path_buf(), written.to_vec());
            self.spawn(move |_, tx| {
                let (asked, problems) = lsp.errors(&dir, &written);
                let _ = tx.send(AppEvent::Diagnostics(id, asked, problems));
            });
            return;
        }
        self.run_build_check(root, written);
    }

    /// Check that the project at `root` still builds after `written`
    /// changed, by its build command or each file's own.
    fn run_build_check(&mut self, root: &std::path::Path, written: &[String]) {
        let Some((project, _)) = self.project.clone() else { return };
        let Some(command) = files::check_command(root, code_kind(root), written) else { return };
        let (id, report) = self.shell_entry(command.clone(), false);
        self.check = Some(Check { id, project, command: command.clone(), servers: None });
        if cfg!(test) {
            return;
        }
        let dir = root.to_path_buf();
        self.spawn(move |_, tx| {
            let (output, ended) = run_command(&command, Some(&dir), false, report);
            let _ = tx.send(AppEvent::Check(id, output, ended.unwrap_or(Ended::Killed)));
        });
    }

    /// A check ended: passed, it's said; failed, the agent is asked to fix
    /// the errors, again and again until they are, or until a round fixes
    /// nothing.
    fn check_ended(&mut self, id: usize, output: String, ended: Ended) {
        self.shell_reported(id, &output, Some(ended));
        let Some(check) = self.check.take_if(|c| c.id == id && c.servers.is_none()) else { return };
        self.check_outcome(check, ended.code(), output);
    }

    /// The language servers have answered about the files written: their
    /// errors go to the agent to fix as a failed check's do; none, the
    /// build is checked.
    fn diagnostics_ended(&mut self, id: usize, servers: Vec<&'static str>, problems: Vec<lsp::Problem>) {
        let Some(mut check) = self.check.take_if(|c| c.id == id && c.servers.is_some()) else { return };
        let Some((root, written)) = check.servers.clone() else { return };
        if servers.is_empty() {
            // None would start: the build is the check.
            return self.run_build_check(&root, &written);
        }
        check.command = servers.join(", ");
        if problems.is_empty() {
            if self.project.as_ref().is_some_and(|(p, _)| *p == check.project) {
                let files = if written.len() == 1 { written[0].clone() } else { format!("{} files", written.len()) };
                self.entries.push(Entry::Notice(format!("{}: no errors in {}", check.command, files)));
                self.run_build_check(&root, &written);
                if self.check.is_none() {
                    self.last_failure = None;
                    self.run_commands();
                }
            }
            return;
        }
        let shown: Vec<String> = problems.iter().take(DIAGNOSTICS_SHOWN).map(|p| p.to_string()).collect();
        let more = problems.len().saturating_sub(DIAGNOSTICS_SHOWN);
        let mut output = shown.join("\n");
        if more > 0 {
            output.push_str(&format!("\n…and {more} more"));
        }
        self.entries.push(Entry::Error(format!("{} reports {} error(s):\n{}", check.command, problems.len(), output)));
        self.check_outcome(check, Some(1), output);
    }

    /// What a check that ended with `code` and `output` calls for: passed,
    /// the reply's commands; failed, a round of fixes, unless the last one
    /// changed nothing.
    fn check_outcome(&mut self, check: Check, code: Option<i32>, output: String) {
        // The project switched meanwhile: its errors aren't this one's.
        if self.project.as_ref().is_none_or(|(p, _)| *p != check.project) {
            self.commands = None;
            self.answer_ask(false, false);
            return;
        }
        match after_check(code, &output, self.fix_round, self.last_failure.as_deref()) {
            AfterCheck::Passed => {
                self.last_failure = None;
                let fixed = if self.fix_round > 0 { format!(" after {} fix(es)", self.fix_round) } else { String::new() };
                self.entries.push(Entry::Notice(format!("{} passed{}", check.command, fixed)));
                self.run_commands();
            }
            AfterCheck::Fix(round) => {
                self.fix_round = round;
                self.last_failure = Some(output.clone());
                self.entries.push(Entry::Notice(format!("Asking the agent to fix it (round {}; esc stops)", round)));
                let shown = format!("Fix the errors from `{}`", check.command);
                let what = match check.servers {
                    Some(_) => format!("The language server ({}) reports these errors in the files after your change", check.command),
                    None => format!("I ran `{}` in the project after your change; it failed", check.command),
                };
                let message = format!(
                    "{what}:\n```\n{}\n```\nFix these errors in the project's files, as before: each file you \
                     change given again whole, every line of it, as a heredoc.",
                    head(&output, SHELL_KEPT)
                );
                self.send_turn(shown, message, Vec::new(), true);
            }
            AfterCheck::Stalled => {
                self.last_failure = None;
                self.entries.push(Entry::Warning(format!(
                    "{} still fails the same way after {} fix(es); the errors are above",
                    check.command,
                    self.fix_round
                )));
                // The tool calls among them are dropped with them: the
                // agent's next round would only build on broken files.
                self.tooling = None;
                if let Some((_, commands)) = self.commands.take() {
                    self.entries.push(Entry::Warning(format!(
                        "Not run, as the check failed: {}",
                        commands.join("; ").replace('\n', "; ")
                    )));
                }
            }
        }
        self.scroll_back = 0;
    }

    /// A reply's files `found` and `commands`, in `root`: written and
    /// checked, as a coding harness writes its edits, then the commands
    /// asked about.
    fn reply_done(&mut self, root: std::path::PathBuf, found: files::Extracted, mut whole: Vec<String>, commands: Vec<String>) {
        // Asked to make something in an empty folder, a small model may say
        // what it made without giving the files: asked once more for them.
        // One calling tools may only be looking around first.
        let calling = self.tooling.as_ref().is_some_and(|t| !t.calls.is_empty());
        if found.files.is_empty() && found.failed.is_empty() && !calling && !self.files_asked && files::is_empty_folder(&root) {
            self.files_asked = true;
            return self.ask_for_files();
        }
        // An edited file is the file there with the edits made: all of it.
        whole.extend(found.edited);
        self.apply_reply(root, found.files, &whole, commands, found.failed);
    }

    /// Write a reply's files, check them, then ask about its commands.
    /// Edits that couldn't be made go back to the agent first, with the
    /// files as they are, as opencode hands a failed edit back to the model.
    fn apply_reply(
        &mut self,
        root: std::path::PathBuf,
        found: Vec<files::File>,
        whole: &[String],
        commands: Vec<String>,
        mut failed: Vec<(String, String)>,
    ) {
        // A file with a comment standing in for lines of it isn't written:
        // it's asked for again whole, as an edit that couldn't be made is.
        let (found, elided): (Vec<files::File>, Vec<files::File>) = found.into_iter().partition(|f| {
            std::fs::read_to_string(root.join(&f.path)).map_or(true, |old| !files::elides(&old, &f.content))
        });
        failed.extend(elided.into_iter().map(|f| (f.path, files::ELIDED.to_string())));
        let written = self.reply_files(&root, found, whole);
        // The reply's commands run once its files are in and checked: a
        // fix round's reply has none of its own, so the first reply's wait.
        if !commands.is_empty() {
            self.commands = Some((root.clone(), commands));
        }
        if !failed.is_empty() {
            for (path, why) in &failed {
                self.entries.push(Entry::Warning(format!("Not changed, {}: {}", path, files::plainly(why))));
            }
            if self.edit_round < EDIT_ROUNDS {
                self.edit_round += 1;
                return self.resend_edits(&root, &failed);
            }
            self.entries.push(Entry::Warning(format!(
                "The edits still don't apply after {} tries; ask again, or change the files by hand",
                EDIT_ROUNDS
            )));
        }
        // Source with no manifest won't build, nor can it be checked: the
        // manifest is asked for first, and the check runs on its reply.
        if !self.manifest_asked
            && let Some((manifest, lang)) = files::missing_manifest(&root, &written)
        {
            self.manifest_asked = true;
            return self.ask_for_manifest(manifest, lang, &written);
        }
        if !written.is_empty() {
            self.run_check(&root, &written);
        } else if self.fix_round > 0 {
            self.entries.push(Entry::Warning("The fix changed no file, so the errors stand; see them above".to_string()));
        }
        if self.check.is_none() {
            self.run_commands();
        }
    }

    /// Ask the agent for the `manifest` the `lang` files it `wrote` need.
    fn ask_for_manifest(&mut self, manifest: &str, lang: &str, wrote: &[String]) {
        self.entries.push(Entry::Notice(format!("No {manifest} came with that reply; asking the agent for it")));
        let layout = if manifest == "Cargo.toml" {
            " A program's code goes in `src/main.rs`: if you wrote it elsewhere, give it again whole at that path."
        } else {
            ""
        };
        let message = format!(
            "You wrote {}, but the folder has no `{manifest}`, so the {lang} project can't build. Give `{manifest}` \
             now, whole, in a fenced block marked `sh`: a line `cat > {manifest} <<'EOF'`, its complete contents with \
             every dependency the code uses, then `EOF` alone on its line.{layout}",
            wrote.iter().map(|p| format!("`{p}`")).collect::<Vec<_>>().join(", ")
        );
        self.send_turn(format!("Add {manifest}"), message, Vec::new(), true);
    }

    /// Ask the agent for the files its last reply described but didn't give;
    /// the folder is still empty.
    fn ask_for_files(&mut self) {
        self.entries.push(Entry::Notice("No files came with that reply; asking the agent for them".to_string()));
        let message = format!(
            "Your last reply gave no files, so nothing was written and the working folder is still empty. Give \
             every file it needs now, each whole in a fenced block marked `sh`: a line `cat > path/to/file <<'EOF'`, \
             the file's complete contents, then `EOF` alone on its line.\n\n{}",
            files::EMPTY_FOLDER
        );
        self.send_turn("Write the files".to_string(), message, Vec::new(), true);
    }

    /// Ask the agent again for the edits that `failed` in `root`, with
    /// each file as it is now.
    fn resend_edits(&mut self, root: &std::path::Path, failed: &[(String, String)]) {
        let mut paths: Vec<&str> = failed.iter().map(|(p, _)| p.as_str()).collect();
        paths.dedup();
        let reasons: Vec<String> = failed.iter().map(|(p, why)| format!("- {p}: {why}")).collect();
        let files: Vec<String> = paths
            .iter()
            .map(|p| match std::fs::read_to_string(root.join(p)) {
                Ok(text) => format!("`{p}` as it is now:\n````\n{}\n````", head(&text, EDIT_SHOWN)),
                Err(_) => format!("`{p}` isn't there."),
            })
            .collect();
        self.entries.push(Entry::Notice(format!("Asking the agent for those edits again (try {} of {})", self.edit_round, EDIT_ROUNDS)));
        let message = format!(
            "These changes from your last reply couldn't be made, so those files weren't changed:\n{}\n\n{}\n\nGive \
             just those files again, each whole as a heredoc: every line of it as it is to be, changed or not, with \
             no comment standing in for lines. Don't repeat the files that were written.\n\n{}",
            reasons.join("\n"),
            files.join("\n\n"),
            files::EDIT_EXAMPLE
        );
        self.send_turn(format!("Redo the edits to {}", paths.join(", ")), message, Vec::new(), true);
    }

    /// The last reply's shell commands, checked, are shown and asked
    /// about, as opencode asks before a tool runs; allowed for the session
    /// already, they run at once.
    fn run_commands(&mut self) {
        // None waiting, the reply's tool calls that needed none may go on.
        let Some((root, commands)) = self.commands.take() else { return self.tool_round_done() };
        if self.always_run {
            return self.run_allowed(root, commands);
        }
        // An earlier question still open is answered by this one.
        self.entries.retain(|e| !matches!(e, Entry::Ask(_)));
        self.entries.push(Entry::Ask(commands.clone()));
        self.ask = Some((root, commands));
        self.scroll_back = 0;
    }

    /// The commands asked about, answered: `run` runs them, else they're
    /// skipped and said so; `always` keeps running them for the session.
    fn answer_ask(&mut self, run: bool, always: bool) {
        let Some((root, commands)) = self.ask.take() else { return };
        self.entries.retain(|e| !matches!(e, Entry::Ask(_)));
        if !run {
            self.entries.push(Entry::Notice(format!("Skipped: {}", commands.join("; ").replace('\n', "; "))));
            self.scroll_back = 0;
            // A tool call skipped is answered so: the agent hears it wasn't
            // run and goes on without it, as it can't go on with no result.
            if let Some(tooling) = &mut self.tooling {
                tooling.resolve(&commands, TOOL_DECLINED);
            }
            return self.tool_round_done();
        }
        if always {
            self.always_run = true;
            self.entries.push(Entry::Notice("Auto-approve on: a reply's commands run without asking (tab turns it off)".to_string()));
        }
        self.run_allowed(root, commands);
    }

    /// The commands of `commands` fit to run: one with a placeholder to
    /// fill in, as `<your-username>`, is left out and said so, as it
    /// would only fail, or do the wrong thing.
    fn runnable(&mut self, commands: Vec<String>) -> Vec<String> {
        let mut fit = Vec::new();
        for command in commands {
            let holes = files::placeholders(&command);
            if holes.is_empty() {
                fit.push(command);
            } else {
                self.entries.push(Entry::Warning(format!(
                    "Not run, as it has {} to fill in: {}",
                    if holes.len() == 1 { "a placeholder" } else { "placeholders" },
                    holes.join(", ")
                )));
            }
        }
        fit
    }

    /// Run `commands` in `root`, each block as one script, one after the
    /// other, shown like a `!` command; one ending in `&` is left running
    /// in the background and the next starts. What they print goes to the
    /// agent with the next prompt, so it can go on from there.
    fn run_allowed(&mut self, root: std::path::PathBuf, commands: Vec<String>) {
        let Some((project, _)) = self.project.clone() else { return };
        let mut runs = Vec::new();
        let asked = commands.clone();
        for command in self.runnable(commands) {
            let (shown, background) = detached(&co_authored(&command));
            let (id, report) = self.shell_entry(shown.clone(), background);
            if let Some(call) = self.tooling.as_mut().and_then(|t| t.waiting(&command)) {
                call.run = Some(id);
            }
            runs.push((id, shown, background, report));
        }
        // A call whose command was left out, for a placeholder in it.
        if let Some(tooling) = &mut self.tooling {
            tooling.resolve(&asked, "Not run: the command has a placeholder to fill in, such as <name>.");
        }
        self.tool_round_done();
        if cfg!(test) {
            return;
        }
        self.spawn(move |_, tx| {
            let mut runs = runs.into_iter();
            while let Some((id, command, background, report)) = runs.next() {
                let (output, ended) = run_command(&command, Some(&root), background, report);
                let _ = tx.send(AppEvent::Ran(id, project.clone(), output, ended));
                // The rest would build on what this one didn't do, or on
                // what the user cut short; they're listed as running from
                // the start, so they go, or they'd wait there forever.
                if ended.is_some_and(|e| e.failed() || e == Ended::Stopped) {
                    let _ = tx.send(AppEvent::Unrun(runs.map(|(id, ..)| id).collect()));
                    break;
                }
            }
        });
    }

    /// Runs `ids` that never started, as one before them failed or was
    /// stopped: off the running list, and their entries out of the chat,
    /// with a note of what wasn't run.
    fn unrun(&mut self, ids: &[usize]) {
        if ids.is_empty() {
            return;
        }
        if let Some(tooling) = &mut self.tooling {
            for call in tooling.calls.iter_mut().filter(|c| c.run.is_some_and(|r| ids.contains(&r)) && c.result.is_none()) {
                call.result = Some("Not run, as the command before it failed or was stopped.".to_string());
            }
        }
        let commands: Vec<String> = ids.iter().filter_map(|id| self.shell_of(*id).map(|s| s.command.clone())).collect();
        self.jobs.retain(|j| !ids.contains(&j.id));
        if let Overlay::Jobs { selected } = &mut self.overlay {
            match self.jobs.len() {
                0 => self.overlay = Overlay::None,
                n => *selected = (*selected).min(n - 1),
            }
        }
        self.entries.retain(|e| !matches!(e, Entry::Shell(s) if ids.contains(&s.id)));
        if !commands.is_empty() {
            self.entries.push(Entry::Notice(format!("Not run: {}", commands.join("; ").replace('\n', "; "))));
        }
        self.tool_round_done();
    }

    /// A reply's script ended. Failed, the agent is asked to put it right,
    /// with other commands or with files, for a few rounds only. Done, it
    /// isn't asked anything: what was printed rides with the next prompt.
    fn ran(&mut self, id: usize, project: String, output: String, ended: Option<Ended>) {
        if self.tooling.as_ref().is_some_and(|t| t.calls.iter().any(|c| c.run == Some(id))) {
            return self.tool_ran(id, project, output, ended);
        }
        let Some(context) = self.shell_reported(id, &output, ended) else { return };
        let command = self.shell_mut(id).map(|s| s.command.clone()).unwrap_or_default();
        self.shell_context.push(context);
        let failed = ended.is_some_and(Ended::failed);
        // Stopped by the user, the rest is theirs to say.
        if ended == Some(Ended::Stopped)
            || !failed
            || self.turn.is_some()
            || self.project.as_ref().is_none_or(|(p, _)| *p != project)
        {
            return;
        }
        if self.command_round >= COMMAND_ROUNDS {
            if failed {
                self.entries.push(Entry::Warning(format!(
                    "Still failing after {} round(s); the output is above, and goes to the agent with your next prompt",
                    self.command_round
                )));
            }
            return;
        }
        self.command_round += 1;
        let ran = std::mem::take(&mut self.shell_context).join("\n\n");
        self.entries.push(Entry::Notice(format!("Asking the agent to fix it (round {}; esc stops)", self.command_round)));
        let shown = format!("Fix the failed command: {}", command.lines().next().unwrap_or_default());
        let message = format!(
            "{}\n\nThat was a command you gave me, run in the project folder, and it failed. Put it right: give \
             the commands to run instead, in a fenced `sh` block, or the files to change, whole, as before. \
             Don't repeat a command that already did its part.",
            ran
        );
        self.send_turn(shown, message, Vec::new(), true);
    }

    /// Tool call run `id` ended: its result is kept for the agent, and once
    /// the reply's calls all have theirs, the turn goes on. Stopped by the
    /// user, the rounds stop too, and what it printed rides with the next
    /// prompt, as the rest is theirs to say.
    fn tool_ran(&mut self, id: usize, project: String, output: String, ended: Option<Ended>) {
        let context = self.shell_reported(id, &output, ended);
        let elsewhere = self.project.as_ref().is_none_or(|(p, _)| *p != project);
        if ended == Some(Ended::Stopped) || elsewhere {
            self.tooling = None;
            if !elsewhere {
                self.shell_context.extend(context);
            }
            return;
        }
        let Some(tooling) = &mut self.tooling else { return };
        let Some(call) = tooling.calls.iter_mut().find(|c| c.run == Some(id)) else { return };
        call.result = Some(tool_output(&output, ended));
        call.context = context;
        let command = call.command.clone().unwrap_or_default();
        if ended.is_some_and(Ended::failed) {
            // The same command failing again won't do better a third time.
            if tooling.failed.as_deref() == Some(command.as_str()) {
                tooling.stop = Some(format!(
                    "`{}` failed again, so the agent isn't asked to go on; its output is above, and goes with your next prompt",
                    command.lines().next().unwrap_or_default()
                ));
            }
            tooling.failed = Some(command);
        } else {
            tooling.failed = None;
        }
        self.tool_round_done();
    }

    /// The last reply's tool calls, once each has its result and nothing
    /// else is going on (its files' check, a question about its commands,
    /// another turn), go back to the agent in a turn of their own, which
    /// carries on the same request. That's done `TOOL_ROUNDS` times at
    /// most, and not when a command failed twice running: then the results
    /// ride with the next prompt instead, as a `!command`'s do.
    fn tool_round_done(&mut self) {
        let ready = self.tooling.as_ref().is_some_and(|t| !t.calls.is_empty() && t.calls.iter().all(|c| c.result.is_some()));
        if !ready || self.turn.is_some() || self.check.is_some() || self.ask.is_some() || self.commands.is_some() {
            return;
        }
        let Some(mut tooling) = self.tooling.take() else { return };
        let Some((project, _)) = self.project.clone() else { return };
        let calls = std::mem::take(&mut tooling.calls);
        let stop = tooling.stop.take().or_else(|| {
            (tooling.round >= TOOL_ROUNDS).then(|| {
                format!("Stopped after {} tool rounds; the output is above, and goes with your next prompt", TOOL_ROUNDS)
            })
        });
        if let Some(why) = stop {
            self.entries.push(Entry::Warning(why));
            self.shell_context.extend(calls.into_iter().filter_map(|c| c.context));
            self.scroll_back = 0;
            return;
        }
        for call in calls {
            tooling.rounds.push(serde_json::json!({
                "role": "tool",
                "tool_call_id": call.id,
                "content": call.result.unwrap_or_default(),
            }));
        }
        tooling.round += 1;
        let (sent, rounds) = (tooling.sent.clone(), tooling.rounds.clone());
        self.tooling = Some(tooling);
        self.start_turn(project, sent, Vec::new(), rounds);
    }

    fn stop_turn(&mut self) {
        self.commands = None;
        self.tooling = None;
        if let Some(turn) = self.turn.take() {
            self.busy.store(false, Ordering::Relaxed);
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
                self.check_update();
                if let Some(tip) = home_tip().filter(|_| !cfg!(test)) {
                    self.entries.push(Entry::Warning(tip));
                }
            }
            AppEvent::SetupDone(Err(e)) => self.setup.failure = Some(e),
            AppEvent::Projects(result) => self.projects_loaded(result),
            AppEvent::SwitchProject(project) => self.switch_project((project.id, project.name)),
            AppEvent::Models(result) => {
                if let Ok(list) = &result {
                    self.window = list.iter().filter(|m| m.enabled).filter_map(|m| m.context).max();
                    self.enabled_models = Some(list.iter().filter(|m| m.enabled).map(|m| m.name.clone()).collect());
                    if !std::mem::replace(&mut self.models_checked, true)
                        && let Some(nudge) = model_nudge(list)
                    {
                        self.entries.push(Entry::Warning(nudge));
                    }
                }
                if let Overlay::Models { rows, selected } = &mut self.overlay {
                    if let Ok(list) = &result {
                        *selected = (*selected).min(list.len().saturating_sub(1));
                    }
                    *rows = Some(result);
                }
            }
            AppEvent::Mcp(result) => {
                // The card counts them too.
                if let Ok(list) = &result {
                    self.connections = Some(connection_counts(list));
                }
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
                self.busy.store(false, Ordering::Relaxed);
                // A turn can add a skill ("save this as a skill"); read the
                // list again so `/` hints it without reopening the project.
                self.load_skills();
                    if let Some(Entry::Reply(reply)) = self.entries.get_mut(turn.entry) {
                    match result {
                        Ok(stats) => {
                            reply.stats = Some(stats);
                            if let Some(root) = turn.files_dir {
                                if reply.root.is_none() {
                                    reply.root = Some(root.clone());
                                }
                                // The files as they were before the reply,
                                // some of its own being written already.
                                let found = files::extract_with(&reply.text, |p| reply.old(p));
                                let mut commands = files::commands(&reply.text);
                                // A reply that called tools runs those: its
                                // fenced commands, if any, only illustrate.
                                if !turn.calls.is_empty() {
                                    let text = reply.text.clone();
                                    commands = Vec::new();
                                    if let Some(tooling) = &mut self.tooling {
                                        let (asked, shown) = tooling.called(&text, turn.calls);
                                        commands = asked;
                                        reply.text.push_str(&shown);
                                    }
                                }
                                self.early = reply
                                    .written
                                    .iter()
                                    .map(|path| Early {
                                        path: path.clone(),
                                        old: reply.old(path),
                                        new: found.files.iter().find(|f| f.path == *path).map_or(String::new(), |f| f.content.clone()),
                                    })
                                    .collect();
                                self.reply_done(root, found, turn.whole, commands);
                                self.tool_round_done();
                            }
                        }
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
            AppEvent::Check(id, output, code) => self.check_ended(id, output, code),
            AppEvent::Diagnostics(id, servers, problems) => self.diagnostics_ended(id, servers, problems),
            AppEvent::Ran(id, project, output, code) => self.ran(id, project, output, code),
            AppEvent::Uploaded(ids) => {
                if let Some(tooling) = &mut self.tooling {
                    tooling.sent.attachments = ids;
                }
            }
            AppEvent::Unrun(ids) => self.unrun(&ids),
            AppEvent::Started(id, pid) => {
                if let Some(job) = self.jobs.iter_mut().find(|j| j.id == id) {
                    job.pid = Some(pid);
                }
            }
            AppEvent::Printed(id, output) => {
                if let Some(shell) = self.shell_mut(id).filter(|s| s.ended.is_none()) {
                    shell.output = output;
                }
            }
            // Gone with a /clear, there's nothing to show or keep.
            AppEvent::Shell(id, output, ended) => {
                if let Some(context) = self.shell_reported(id, &output, ended) {
                    self.shell_context.push(context);
                }
            }
            AppEvent::Usage(usage) => self.search = usage,
            AppEvent::Update(version) => self.update = Some(version),
            AppEvent::Credits(credits) => {
                self.credits = credits;
                if let Overlay::Usage { loading, .. } = &mut self.overlay {
                    *loading = false;
                }
            }
            AppEvent::Account(account) => self.account = Some(account),
            AppEvent::Connections(project, servers) => {
                if self.project.as_ref().is_some_and(|(id, _)| *id == project) {
                    self.connections = Some(connection_counts(&servers));
                }
            }
            // Indexing runs in the background, unannounced; a push done only
            // has the card's document count refreshed.
            AppEvent::Syncing(count) => {
                if count == 0 {
                    // Ask the agent now, rather than at the next poll.
                    self.agent_asked = Some(Instant::now() - AGENT_EVERY);
                }
            }
            AppEvent::SyncLink(root, project) => self.start_sync(root, project, true),
            AppEvent::SyncEnded(stop) => {
                if self.sync.as_ref().is_some_and(|s| Arc::ptr_eq(&s.stop, &stop)) {
                    self.sync = None;
                }
            }
            AppEvent::Agent(project, state) => {
                self.agent_asked = Some(Instant::now());
                // Asked before a switch: ask again, for this one.
                if project.as_deref() != self.project.as_ref().map(|(id, _)| id.as_str()) {
                    self.agent_asked = Some(Instant::now() - AGENT_EVERY);
                    return;
                }
                let down = || crate::AgentState { state: "not responding".to_string(), memory: None, documents: 0, pending: 0 };
                let state = state.unwrap_or_else(down);
                self.agent = Some(state);
            }
            // Only for the project still chatted in.
            AppEvent::Skills(project, skills) => {
                if self.project.as_ref().is_some_and(|(id, _)| *id == project) {
                    self.skills = skills;
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
    /// Once chatting, the list is for `/project` instead.
    fn projects_loaded(&mut self, result: Result<Vec<serde_json::Value>, String>) {
        if !matches!(self.phase, Phase::Connecting) {
            return self.project_list_loaded(result);
        }
        let projects = match result {
            Ok(projects) => projects,
            Err(e) => {
                self.entries.push(Entry::Error(e));
                return;
            }
        };
        let field = |p: &serde_json::Value, k: &str| p[k].as_str().unwrap_or_default().to_string();
        // Without one asked for, the project the folder syncs to.
        let link = sync_root()
            .filter(|root| docent::refusal(root).is_none())
            .and_then(|root| docent::linked(&root));
        let linked = link
            .as_ref()
            .map(|l| l.project.clone())
            .filter(|id| projects.iter().any(|p| field(p, "id") == *id));
        // A folder never linked, or linked to a project the agent no longer
        // has, opened without a project asked for, gets a project of its
        // own, named after it: never the one the last folder left current.
        if self.wanted_project.is_none()
            && linked.is_none()
            && let Some(root) = sync_root().filter(|root| docent::refusal(root).is_none())
            && let Some(folder) = root.file_name().map(|name| name.to_string_lossy().into_owned())
        {
            self.phase = Phase::Ready;
            return self.adopt_folder(root, folder, &projects);
        }
        let chosen = match self.wanted_project.as_ref().or(linked.as_ref()) {
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
                // Nothing here or for the project synced yet: say what /sync would add.
                if linked.is_none() && project_root(&project.0).is_none() {
                    self.hint_unsynced(project.1.clone());
                }
                self.project = Some(project);
                // Its folder keeps syncing, wherever it is, until `/sync stop`.
                self.follow_project();
                self.load_web_search();
                // Read now, so a project with no model enabled says so at once.
                self.load_models();
                self.load_skills();
                self.load_connections();
                self.load_account();
                if let Some(prompt) = self.pending_prompt.take() {
                    let making = self.making(&prompt);
                    self.send_turn(prompt.clone(), prompt, Vec::new(), making);
                }
            }
            None => self.entries.push(Entry::Error("No projects found; create one with /project new <name>".to_string())),
        }
    }

    /// Give `root`, a folder never linked, a project of its own: one named
    /// after it that has no folder yet, else a new one. The chat switches
    /// to it and the folder syncs to it from then on; a large folder is
    /// linked but left paused, for `/sync --yes` to push.
    fn adopt_folder(&mut self, root: std::path::PathBuf, folder: String, projects: &[serde_json::Value]) {
        let field = |p: &serde_json::Value, k: &str| p[k].as_str().unwrap_or_default().to_string();
        let taken: Vec<(String, String)> = projects.iter().map(|p| (field(p, "id"), field(p, "name"))).collect();
        let (name, existing) = adopted(&folder, &taken, |id| docent::folder(id).is_some());
        self.entries.push(Entry::Notice(match &existing {
            Some(_) => format!("Linking {} to project {}", root.display(), name),
            None => format!("Creating project {} for {}", name, root.display()),
        }));
        if cfg!(test) {
            return;
        }
        self.spawn(move |client, tx| {
            let project = match existing {
                Some(id) => Ok((id, name)),
                None => crate::try_create_project(&client, &name, None).map(|p| {
                    let p = Project::from_json(&p);
                    (p.id, p.name)
                }),
            };
            let (id, name) = match project {
                Ok(project) => project,
                Err(e) => return drop(tx.send(AppEvent::Error(e))),
            };
            let project = Project { id: id.clone(), name: name.clone(), description: String::new(), system: false };
            let _ = tx.send(AppEvent::SwitchProject(project));
            let size = docent::measure(&root);
            if !size.large() {
                return drop(tx.send(AppEvent::SyncLink(root, (id, name))));
            }
            let _ = tx.send(match docent::link(&root, &id).and_then(|()| docent::pause(&root)) {
                Ok(()) => AppEvent::Warning(format!(
                    "{} has {} documents ({}); /sync --yes pushes them all to project {}",
                    root.display(),
                    size.files,
                    format_size(size.bytes),
                    name
                )),
                Err(e) => AppEvent::Error(e),
            });
        });
    }

    /// Point out the documents in a folder never synced, which the agent
    /// can't search until `/sync` indexes them.
    fn hint_unsynced(&self, project: String) {
        let Some(root) = sync_root().filter(|root| docent::refusal(root).is_none()) else { return };
        self.spawn(move |_, tx| {
            let found = docent::unsynced(&root);
            if let Some(text) = unsynced_hint(&found, &project) {
                let _ = tx.send(AppEvent::Notice(text));
            }
        });
    }

    /// Fill the `/project` panel: on opening, the current project selected;
    /// a reload (after a delete, or ctrl+r) keeps the selection where it was.
    fn project_list_loaded(&mut self, result: Result<Vec<serde_json::Value>, String>) {
        let Overlay::Projects { rows, selected, deleting } = &mut self.overlay else { return };
        *deleting = false;
        let list: Vec<Project> = match result {
            Ok(projects) => projects.iter().map(Project::from_json).collect(),
            Err(e) => return *rows = Some(Err(e)),
        };
        let current = self.project.as_ref().map(|(id, _)| id.clone());
        *selected = match rows {
            None => list.iter().position(|p| Some(&p.id) == current.as_ref()).unwrap_or(0),
            // The two rows after the list, "+ New project" and "+ Import
            // project", stay chosen as the list shrinks.
            Some(Ok(old)) if *selected >= old.len() => list.len() + (*selected - old.len()).min(1),
            Some(_) => (*selected).min(list.len() + 1),
        };
        *rows = Some(Ok(list));
    }

    /// Chat in `project` from now on, in a new session: the conversation so
    /// far belongs to the old one, so it's cleared as `/clear` does. The
    /// agent is told too, so it stays the current project after a restart.
    fn switch_project(&mut self, project: (String, String)) {
        if self.project.as_ref().is_some_and(|(id, _)| *id == project.0) {
            return self.entries.push(Entry::Notice(format!("Already in project {}", project.1)));
        }
        self.stop_turn();
        self.entries.clear();
        self.attachments.clear();
        self.session = chat::new_session_id();
        self.scroll_back = 0;
        self.web_search = None;
        self.window = None;
        self.enabled_models = None;
        self.models_checked = false;
        self.skills.clear();
        self.connections = None;
        // What the last project's documents were doing isn't this one's.
        self.agent = None;
        if self.agent_asked.is_some() {
            self.agent_asked = Some(Instant::now() - AGENT_EVERY);
        }
        self.entries.push(Entry::Notice(format!("Switched to project {}", project.1)));
        let id = project.0.clone();
        self.project = Some(project);
        self.follow_project();
        self.load_web_search();
        self.load_models();
        self.load_skills();
        self.load_connections();
        // A prompt given on the command line waits for the project opened
        // for the folder.
        if let Some(prompt) = self.pending_prompt.take() {
            let making = self.making(&prompt);
            self.send_turn(prompt.clone(), prompt, Vec::new(), making);
        }
        if cfg!(test) {
            return;
        }
        self.spawn(move |client, tx| {
            if let Err(e) = crate::set_current_project(&client, &id) {
                let _ = tx.send(AppEvent::Error(e));
            }
        });
    }

    fn chat_event(&mut self, event: ChatEvent) {
        let Some(turn) = self.turn.as_mut() else { return };
        let Some(Entry::Reply(reply)) = self.entries.get_mut(turn.entry) else { return };
        match event {
            ChatEvent::Content(content) => {
                turn.tokens += 1;
                reply.text.push_str(&content);
                // A file's written once its block is done, not when the
                // reply is.
                if content.contains('\n')
                    && let Some(root) = &turn.files_dir
                {
                    reply.write_done(root);
                }
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
                // The agent's indexing for the turn isn't shown: the turn's
                // line keeps what it was doing.
                let indexing = step == "index";
                if step == "streaming" {
                    // The agent's "Generating response" says nothing a
                    // reader needs; the model writing the answer does, or,
                    // not known, a word for it, kept for the turn.
                    turn.activity = Some(match &reply.model {
                        Some(model) => format!("Generating with {}", model),
                        None => working_word(turn.started).to_string(),
                    });
                } else if !indexing {
                    turn.activity = Some(message.trim_end_matches(['.', '…']).to_string());
                }
            }
            ChatEvent::Retrying(e) => reply.warnings.push(format!("Connection dropped, retrying: {}", e)),
            ChatEvent::ToolCalls(calls) => turn.calls = calls,
        }
    }

    // ----- keys ------------------------------------------------------------

    /// The wheel scrolls; a left-button drag over the conversation selects,
    /// and letting go copies the selection; a click on a link opens it.
    fn mouse(&mut self, mouse: MouseEvent) {
        let at = (mouse.column, mouse.row);
        match mouse.kind {
            MouseEventKind::ScrollUp => self.scroll(WHEEL_LINES as i32),
            MouseEventKind::ScrollDown => self.scroll(-(WHEEL_LINES as i32)),
            MouseEventKind::Down(MouseButton::Left) => {
                let (area, footer) = (self.shown.borrow().area, self.shown.borrow().footer);
                self.selection = area.contains(at.into()).then_some(Selection { anchor: at, head: at });
                self.pressed = footer.contains(at.into()).then_some(at);
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                if let Some(selection) = &mut self.selection {
                    let area = self.shown.borrow().area;
                    selection.head = clamp_to(area, at);
                }
            }
            MouseEventKind::Up(MouseButton::Left) => {
                let clicked = self.selection.as_ref().filter(|s| s.anchor == s.head).map(|s| s.anchor);
                let clicked = clicked.or(self.pressed.take().filter(|&down| down == at));
                if let Some(url) = clicked.and_then(|at| self.shown.borrow().link_at(at)) {
                    self.selection = None;
                    return match crate::links::action_of(&url) {
                        Some(action) => self.act(action),
                        None => self.open(url),
                    };
                }
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
        let (area, rows) = (&shown.area, &shown.rows);
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

    /// A clicked action: `stop/<id>` ends shell run `id`, as `/stop` does,
    /// without a notice, as the run's entry says it's stopped; `show/<id>`
    /// expands the run under the prompt.
    fn act(&mut self, action: &str) {
        let (what, id) = action.split_once('/').unwrap_or_default();
        let Ok(id) = id.parse::<usize>() else { return };
        match what {
            "stop" => {
                self.stop_jobs(|job| job.id == id);
            }
            "show" if self.shell_of(id).is_some() => self.overlay = Overlay::Shell(id),
            _ => {}
        }
    }

    /// Open a clicked link in the browser.
    fn open(&mut self, url: String) {
        if !cfg!(test) {
            crate::open_browser(&url);
        }
        self.opened = Some((url, Instant::now()));
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
            (KeyCode::Char('b'), true) => return self.open_jobs(),
            (KeyCode::Char('s'), true) => return self.toggle_web_search(),
            _ => {}
        }
        let handled = match self.overlay {
            Overlay::None => false,
            Overlay::Models { .. } => self.key_models(key),
            Overlay::Mcp { .. } => self.key_mcp(key),
            Overlay::Projects { .. } => self.key_projects(key),
            Overlay::Jobs { .. } => self.key_jobs(key),
            // `x` stops the run shown while it goes.
            Overlay::Shell(id) if key.code == KeyCode::Char('x') => {
                self.stop_jobs(|job| job.id == id);
                true
            }
            Overlay::Help | Overlay::Status(_) | Overlay::Usage { .. } | Overlay::Shell(_) => {
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

    /// `/models <name> caps <list|auto>`: set a model's capabilities here,
    /// over the catalog's, so the planner routes to it: what the model has
    /// (`text`, `vision`, `reasoning`, `thinking`), not what a turn is
    /// about; `auto` goes back to the catalog's.
    fn set_capabilities(&mut self, name: String, list: &str) {
        let capabilities = match capability_list(list) {
            Ok(caps) => caps,
            Err(e) => return self.entries.push(Entry::Error(e)),
        };
        if cfg!(test) {
            return self.entries.push(Entry::Notice(format!("Setting {} capabilities: {}", name, capabilities.join(", "))));
        }
        let reload = matches!(self.overlay, Overlay::Models { .. });
        let project = self.project.clone();
        self.spawn(move |client, tx| {
            let _ = tx.send(match crate::set_model_capabilities(&client, &name, &capabilities) {
                Ok(now) if capabilities.is_empty() => {
                    AppEvent::Notice(format!("{} capabilities as the catalog has them: {}", name, now.join(", ")))
                }
                Ok(now) => AppEvent::Notice(format!("{} capabilities set here: {}", name, now.join(", "))),
                Err(e) => AppEvent::Error(e),
            });
            if reload && let Some((id, _)) = project {
                let _ = tx.send(AppEvent::Models(crate::model_rows(&client, &id)));
            }
        });
    }

    fn open_mcp(&mut self) {
        if self.project.is_none() {
            return;
        }
        self.overlay = Overlay::Mcp { rows: None, selected: 0 };
        self.load_mcp();
    }

    /// `/project`: list the projects to switch to. Also with none to chat
    /// in yet, so one can be created.
    /// `/tasks` or `ctrl+b`: the commands still running, the first
    /// selected; none, said so, as `/stop` says it.
    fn open_jobs(&mut self) {
        if self.jobs.is_empty() {
            self.entries.push(Entry::Notice("Nothing is running".to_string()));
            self.scroll_back = 0;
            return;
        }
        self.overlay = Overlay::Jobs { selected: 0 };
    }

    /// The running commands panel's keys; false for any other, which is
    /// typing. `enter` expands the selected run, `x` stops it.
    fn key_jobs(&mut self, key: KeyEvent) -> bool {
        let Overlay::Jobs { selected } = &mut self.overlay else { return false };
        let count = self.jobs.len();
        let id = self.jobs.get(*selected).map(|job| job.id);
        match key.code {
            KeyCode::Esc => self.overlay = Overlay::None,
            KeyCode::Up => *selected = selected.saturating_sub(1),
            KeyCode::Down => *selected = (*selected + 1).min(count.saturating_sub(1)),
            KeyCode::Enter => {
                if let Some(id) = id.filter(|&id| self.shell_of(id).is_some()) {
                    self.overlay = Overlay::Shell(id);
                }
            }
            KeyCode::Char('x') => {
                if let Some(id) = id {
                    self.stop_jobs(|job| job.id == id);
                }
            }
            _ => return false,
        }
        true
    }

    fn open_projects(&mut self) {
        if !matches!(self.phase, Phase::Ready) {
            return;
        }
        self.overlay = Overlay::Projects { rows: None, selected: 0, deleting: false };
        self.load_projects();
    }

    /// Ask for the new project's name in the prompt, as `/project new `.
    fn start_new_project(&mut self) {
        self.overlay = Overlay::None;
        self.input = "/project new ".to_string();
        self.cursor = self.input.chars().count();
        self.history_at = None;
    }

    /// Ask for the archive in the prompt, as `/project import `, where a
    /// dropped zip lands as its path.
    fn start_import_project(&mut self) {
        self.overlay = Overlay::None;
        self.input = "/project import ".to_string();
        self.cursor = self.input.chars().count();
        self.history_at = None;
    }

    /// `/project new <name>`: create a blank project and switch to it.
    fn create_project(&mut self, name: String) {
        if name.is_empty() {
            return self.entries.push(Entry::Error("Name the project: /project new <name>".to_string()));
        }
        self.spawn(move |client, tx| {
            let _ = tx.send(match crate::try_create_project(&client, &name, None) {
                Ok(project) => {
                    let _ = tx.send(AppEvent::Notice(format!("Project {} created", name)));
                    AppEvent::SwitchProject(Project::from_json(&project))
                }
                Err(e) => AppEvent::Error(e),
            });
        });
    }

    /// `/project import <zip>`: create a project from an exported archive,
    /// typed or dropped as a path, and switch to it.
    fn import_project(&mut self, path: &str) {
        let path = match dropped_files(path).as_deref() {
            Some([path]) => path.display().to_string(),
            _ if path.is_empty() => {
                return self.entries.push(Entry::Error("Give the archive: /project import <zip>".to_string()));
            }
            _ => return self.entries.push(Entry::Error(format!("No such file: {}", path))),
        };
        self.entries.push(Entry::Notice(format!("Importing {}", path)));
        self.spawn(move |client, tx| {
            let _ = tx.send(match crate::try_import_project(&client, &path, None) {
                Ok(project) => {
                    let project = Project::from_json(&project);
                    let _ = tx.send(AppEvent::Notice(format!("Project {} imported", project.name)));
                    AppEvent::SwitchProject(project)
                }
                Err(e) => AppEvent::Error(e),
            });
        });
    }

    /// `/project <name or id>`: switch straight to it.
    fn find_project(&mut self, wanted: String) {
        self.spawn(move |client, tx| {
            let _ = tx.send(match crate::try_fetch_projects(&client) {
                Ok(list) => match list.iter().map(Project::from_json).find(|p| p.named(&wanted)) {
                    Some(project) => AppEvent::SwitchProject(project),
                    None => AppEvent::Error(format!("No project named {}; /project lists them", wanted)),
                },
                Err(e) => AppEvent::Error(e),
            });
        });
    }

    /// Delete a project other than the one in use, then reload the panel.
    fn delete_project(&mut self, project: Project) {
        if self.project.as_ref().is_some_and(|(id, _)| *id == project.id) {
            return self.entries.push(Entry::Error(format!(
                "{} is the project in use; switch to another before deleting it",
                project.name
            )));
        }
        self.spawn(move |client, tx| {
            let _ = tx.send(match crate::try_delete_project(&client, &project.id) {
                Ok(()) => AppEvent::Notice(format!("Project {} deleted", project.name)),
                Err(e) => AppEvent::Error(e),
            });
            let _ = tx.send(AppEvent::Projects(crate::try_fetch_projects(&client)));
        });
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
        self.overlay = Overlay::Usage { loading: true };
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

    /// Re-read what the agent is doing every `AGENT_EVERY`, one ask at a
    /// time, once it's set up.
    fn poll_agent(&mut self) {
        if !matches!(self.phase, Phase::Ready) || !self.agent_asked.is_some_and(|at| at.elapsed() >= AGENT_EVERY) {
            return;
        }
        self.agent_asked = None;
        let project = self.project.as_ref().map(|(id, _)| id.clone());
        self.spawn(move |client, tx| {
            let state = crate::agent_state(&client, project.as_deref());
            let _ = tx.send(AppEvent::Agent(project, state));
        });
    }

    /// Read the project's MCP servers, for the welcome card.
    fn load_connections(&self) {
        let Some((project, _)) = self.project.clone() else { return };
        if cfg!(test) {
            return;
        }
        self.spawn(move |client, tx| {
            if let Ok(servers) = mcp::list(&client, &project) {
                let _ = tx.send(AppEvent::Connections(project, servers));
            }
        });
    }

    /// Read who's signed in, for the welcome card.
    fn load_account(&self) {
        if cfg!(test) {
            return;
        }
        self.spawn(|client, tx| {
            let _ = tx.send(AppEvent::Account(crate::account(&client)));
        });
    }

    /// Read the project's skills for the hints; without them, `/<skill>`
    /// still reaches the agent, just unhinted.
    fn load_skills(&self) {
        let Some((project, _)) = self.project.clone() else { return };
        if cfg!(test) {
            return;
        }
        self.spawn(move |client, tx| {
            if let Ok(skills) = crate::project_skills(&client, &project) {
                let _ = tx.send(AppEvent::Skills(project, skills));
            }
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
            let selected = hints[self.hint_at.min(hints.len() - 1)].0.clone();
            match key.code {
                KeyCode::Up => return self.hint_at = self.hint_at.saturating_sub(1),
                KeyCode::Down => return self.hint_at = (self.hint_at + 1).min(hints.len() - 1),
                KeyCode::Tab => {
                    self.input = format!("{} ", selected);
                    self.cursor = self.input.chars().count();
                    return;
                }
                KeyCode::Enter => {
                    self.input = selected;
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
    /// word starting with `/`: the CLI's own and the project's skills
    /// together, by name.
    fn command_hints(&self) -> Vec<(String, String)> {
        if self.token_entry || !self.input.starts_with('/') || self.input.contains(' ') {
            return Vec::new();
        }
        let typed = self.input.to_lowercase();
        // A skill named like a command can't be reached by its name.
        let skills = self
            .skills
            .iter()
            .filter(|(name, _)| !is_command(&format!("/{}", name)))
            .map(|(name, what)| {
                // Its description, or its name when it has none.
                (format!("/{}", name), if what.is_empty() { name.clone() } else { what.clone() })
            });
        let mut hints: Vec<(String, String)> = COMMANDS
            .iter()
            .map(|(name, what)| (name.to_string(), what.to_string()))
            .chain(skills)
            .filter(|(name, _)| name.to_lowercase().starts_with(&typed))
            .collect();
        hints.sort_by_key(|(name, _)| name.to_lowercase());
        hints
    }

    /// Whether `/<word>` names one of the project's skills, as the agent
    /// matches it: ignoring case, spaces, dashes and underscores.
    fn is_skill(&self, command: &str) -> bool {
        let wanted = skill_key(command.trim_start_matches('/'));
        !wanted.is_empty() && self.skills.iter().any(|(name, _)| skill_key(name) == wanted)
    }

    fn key_edit(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        // Any other key in between makes the next Esc a first one again.
        let last_esc = self.last_esc.take();
        let esc_again = key.code == KeyCode::Esc && last_esc.is_some_and(|t| t.elapsed() < DOUBLE_ESC);
        let asked = self.ask.is_some() && self.input.is_empty() && self.turn.is_none() && !self.token_entry;
        match key.code {
            KeyCode::Esc if self.token_entry => {
                self.token_entry = false;
                self.input.clear();
                self.cursor = 0;
            }
            // Esc interrupts the reply: the next queued prompt goes at once.
            KeyCode::Esc if self.turn.is_some() => self.stop_turn(),
            // Up on an empty prompt takes back the last queued one, to edit.
            KeyCode::Up if self.input.is_empty() && !self.queued.is_empty() => {
                let last = self.queued.pop().expect("queued is not empty");
                self.input = last.input;
                self.cursor = self.input.len();
                self.attachments = last.attachments;
                self.history_at = None;
            }
            // The reply's commands, asked about: run, run from now on, or skip.
            KeyCode::Enter if asked && key.modifiers.is_empty() => self.answer_ask(true, false),
            KeyCode::Tab if asked => self.answer_ask(true, true),
            KeyCode::Esc if asked => self.answer_ask(false, false),
            // On an empty prompt, Tab turns auto-approve on or off: whether
            // a reply's commands run without asking.
            KeyCode::Tab if self.input.is_empty() && !self.token_entry => {
                self.always_run = !self.always_run;
                let state = if self.always_run {
                    "on: a reply's commands run without asking"
                } else {
                    "off: a reply's commands are asked about"
                };
                self.entries.push(Entry::Notice(format!("Auto-approve {}", state)));
                self.scroll_back = 0;
            }
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
            // The archive for `/project import`, as its path.
            Some(_) if self.input.trim_start().starts_with("/project import") => {
                if !self.input.ends_with(' ') {
                    self.insert(" ");
                }
                self.insert(text.trim());
            }
            // An exported project, on an empty prompt: the import, to confirm
            // with Enter. Not straight away, as it switches projects.
            Some(paths) if self.input.trim().is_empty() && matches!(paths.as_slice(), [path] if is_archive(path)) => {
                self.input = format!("/project import {}", text.trim());
                self.cursor = self.input.chars().count();
            }
            Some(paths) => {
                for path in paths {
                    if !is_image(&path) && !is_document(&path) {
                        let name = path.file_name().map_or_else(|| path.display().to_string(), |n| n.to_string_lossy().into_owned());
                        let why = if is_archive(&path) {
                            "to import it as a project, drop it on an empty prompt".to_string()
                        } else {
                            "only PNG and JPEG images, and PDF, Word, PowerPoint, Excel, CSV, text, HTML and JSON documents".to_string()
                        };
                        self.entries.push(Entry::Error(format!("Can't attach {}: {}", name, why)));
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
                    send_usage(&client, &tx);
                    let _ = tx.send(AppEvent::Account(crate::account(&client)));
                })
            }
            "/quit" | "/exit" | "/q" | "exit" => self.quit = true,
            "/models" => match words.next() {
                None | Some("list") => self.open_models(),
                Some(name) => {
                    let name = name.to_string();
                    let rest: Vec<&str> = words.collect();
                    match rest.as_slice() {
                        ["caps" | "capabilities", list] => self.set_capabilities(name, list),
                        _ => self.entries.push(Entry::Error(
                            "Try /models <name> caps <coding,dp,...> to set a model's capabilities here, or /models <name> caps auto"
                                .to_string(),
                        )),
                    }
                }
            },
            "/mcp" => match words.next() {
                None | Some("list") => self.open_mcp(),
                Some("add") => self.add_mcp(&words.collect::<Vec<_>>().join(" ")),
                // A bare URL adds it too.
                Some(url) if url.contains("://") => self.add_mcp(url),
                Some(other) => self.entries.push(Entry::Error(format!("Unknown /mcp {}; try /mcp add <url>", other))),
            },
            "/project" | "/projects" => {
                let rest = words.clone().skip(1).collect::<Vec<_>>().join(" ");
                match words.next() {
                    None | Some("list") => self.open_projects(),
                    Some("new") => self.create_project(rest),
                    Some("import") => self.import_project(&rest),
                    Some(_) => self.find_project(input.split_once(' ').map_or("", |(_, name)| name).trim().to_string()),
                }
            }
            "/sync" => self.sync_command(&words.collect::<Vec<_>>()),
            "/stop" => self.stop_command(&words.collect::<Vec<_>>().join(" ")),
            "/tasks" => self.open_jobs(),
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
                self.queued.clear();
                self.stop_turn();
                self.entries.clear();
                self.attachments.clear();
                self.shell_context.clear();
                self.commands = None;
                self.ask = None;
                self.tooling = None;
                self.session = chat::new_session_id();
                self.scroll_back = 0;
            }
            // `!command` runs in the working directory, its output kept for
            // the agent with the next prompt.
            _ if input.starts_with('!') => self.run_shell(input[1..].trim().to_string()),
            // `/<skill>` goes to the agent, which runs that skill.
            _ if command.starts_with('/') && !self.is_skill(command) => {
                self.entries.push(Entry::Error(format!("Unknown command {}; try /help", command)));
            }
            _ => {
                // Only once there's a project.
                if self.project.is_none() {
                    return;
                }
                // The chips still in the prompt are the files to send.
                let files: Vec<Attachment> =
                    std::mem::take(&mut self.attachments).into_iter().filter(|a| input.contains(&a.label)).collect();
                // One turn at a time: the rest wait their turn, in order.
                if !self.idle() || !self.queued.is_empty() {
                    self.queued.push(Queued { input: input.clone(), attachments: files });
                } else {
                    self.send_prompt(input.clone(), files);
                }
            }
        }
        // Never keep a pasted token in the history.
        let kept = if command == "/login" { command.to_string() } else { input.clone() };
        self.history.push(kept);
        self.history_at = None;
        self.input.clear();
        self.cursor = 0;
    }

    /// Send a prompt typed in the chat as a new turn.
    fn send_prompt(&mut self, input: String, files: Vec<Attachment>) {
        // The agent reads each chip as its file's name.
        let mut message = input.clone();
        for file in &files {
            message = message.replace(&file.label, &file.name());
        }
        if !self.shell_context.is_empty() {
            message = format!("{}\n\n{}", std::mem::take(&mut self.shell_context).join("\n\n"), message);
        }
        let making = self.making(&input);
        self.fix_round = 0;
        self.edit_round = 0;
        self.files_asked = false;
        self.manifest_asked = false;
        self.command_round = 0;
        self.last_failure = None;
        self.send_turn(input, message, files, making);
    }

    /// Nothing between the last prompt and the next: no reply streaming,
    /// no question about its files or commands open, no check or command
    /// of its running (fix rounds start from those).
    fn idle(&self) -> bool {
        self.turn.is_none()
            && self.ask.is_none()
            && self.commands.is_none()
            && self.check.is_none()
            && !self.entries.iter().any(|e| matches!(e, Entry::Shell(s) if !s.background && s.ended.is_none()))
    }

    /// Send the next queued prompt once the one before it is all done.
    fn send_queued(&mut self) {
        if self.queued.is_empty() || self.project.is_none() || !self.idle() {
            return;
        }
        let next = self.queued.remove(0);
        self.send_prompt(next.input, next.attachments);
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
            send_usage(&client, &tx);
            let _ = tx.send(AppEvent::Account(crate::account(&client)));
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
            send_usage(&client, &tx);
            let _ = tx.send(AppEvent::Account(crate::account(&client)));
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
                if row.enabled {
                    // Always allowed, even for a model the plan no longer covers.
                    self.disable_model(row.name);
                } else if !row.accessible {
                    self.entries.push(Entry::Error(if self.search.is_some() {
                        format!("{} needs the pro plan; /upgrade to use it", row.name)
                    } else {
                        format!("{} needs a sign-in; /login first", row.name)
                    }));
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

    /// The projects panel's keys; false for any other, which is typing.
    /// The last rows, "+ New project" (or ctrl+n) and "+ Import project",
    /// start `/project new ` or `/project import ` in the prompt. Deleting takes ctrl+d twice, as nothing
    /// brings a project back.
    fn key_projects(&mut self, key: KeyEvent) -> bool {
        let Overlay::Projects { rows, selected, deleting } = &mut self.overlay else { return false };
        let list = rows.as_ref().and_then(|r| r.as_ref().ok());
        let count = list.map_or(0, Vec::len);
        let chosen = list.and_then(|l| l.get(*selected)).cloned();
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let delete = matches!((key.code, ctrl), (KeyCode::Char('d'), true) | (KeyCode::Delete, _));
        // Any key but a second delete takes the question back.
        let asked = std::mem::take(deleting);
        match key.code {
            _ if delete => match chosen {
                Some(project) if asked => self.delete_project(project),
                Some(_) => *deleting = true,
                None => {}
            },
            KeyCode::Esc if asked => {}
            KeyCode::Esc => self.overlay = Overlay::None,
            KeyCode::Up => *selected = selected.saturating_sub(1),
            KeyCode::Down => *selected = (*selected + 1).min(count + 1),
            KeyCode::Char('r') if ctrl => self.load_projects(),
            KeyCode::Char('n') if ctrl => self.start_new_project(),
            KeyCode::Enter => match chosen {
                Some(project) => {
                    self.overlay = Overlay::None;
                    self.switch_project((project.id, project.name));
                }
                None if list.is_some() && *selected == count => self.start_new_project(),
                None if list.is_some() => self.start_import_project(),
                None => {}
            },
            _ => return false,
        }
        true
    }

    // ----- drawing -------------------------------------------------------

    fn draw(&self, frame: &mut Frame) {
        let status = self.status_lines();
        // Command hints take the footer's place while a command is typed,
        // and an open panel takes it in the same way, under the prompt.
        let hints = self.hint_lines(frame.area().width as usize);
        let room = frame.area().height.saturating_sub(12);
        // The commands still running are listed over the key hints.
        let under = match self.panel_height() {
            Some(height) => height.min(room),
            None if hints.is_empty() => (1 + self.jobs.len().min(room as usize)) as u16,
            None => hints.len().max(1) as u16,
        };
        // A blank row at the top, and one between the conversation and the
        // status line, so neither sits cramped against its neighbor.
        let queued = self.queued_lines(frame.area().width as usize);
        let [_, body, _, status_area, queued_area, input, footer] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Min(3),
            Constraint::Length(1),
            Constraint::Length(status.len().max(1) as u16),
            Constraint::Length(queued.len() as u16),
            Constraint::Length(self.input_rows(frame.area().width) + 2),
            Constraint::Length(under),
        ])
        .areas(frame.area());

        self.draw_conversation(frame, body);
        frame.render_widget(Paragraph::new(status), status_area);
        frame.render_widget(Paragraph::new(queued), queued_area);
        self.draw_input(frame, input);
        match &self.overlay {
            Overlay::None if hints.is_empty() => self.draw_footer(frame, footer),
            Overlay::None => frame.render_widget(Paragraph::new(hints), footer),
            Overlay::Models { rows, selected } => self.draw_models(frame, footer, rows, *selected),
            Overlay::Mcp { rows, selected } => self.draw_mcp(frame, footer, rows, *selected),
            Overlay::Projects { rows, selected, deleting } => self.draw_projects(frame, footer, rows, *selected, *deleting),
            Overlay::Help => {
                let block = panel(&bar_hints(&[("esc", "close")]));
                frame.render_widget(Paragraph::new(help_lines()).block(block), footer);
            }
            Overlay::Status(rows) => {
                let block = panel(&bar_hints(&[("esc", "close")]));
                frame.render_widget(Paragraph::new(self.status_panel_lines(rows)).block(block), footer);
            }
            Overlay::Usage { loading } => {
                let block = panel(&bar_hints(&[("esc", "close")]));
                frame.render_widget(Paragraph::new(self.usage_panel_lines(*loading)).block(block), footer);
            }
            Overlay::Shell(id) => self.draw_shell(frame, footer, *id),
            Overlay::Jobs { selected } => self.draw_jobs(frame, footer, *selected),
        }
        // After the footer, whose actions it keeps.
        self.keep_shown(frame, body, footer);
    }

    /// The queued prompts, one row each over the prompt: the first line,
    /// cut to the width, the last few only when there are many.
    fn queued_lines(&self, width: usize) -> Vec<Line<'static>> {
        const SHOWN: usize = 4;
        let mut lines = Vec::new();
        let hidden = self.queued.len().saturating_sub(SHOWN);
        if hidden > 0 {
            lines.push(Line::from(Span::styled(format!("  … {} more queued", hidden), dim())));
        }
        for q in &self.queued[hidden..] {
            let first = q.input.lines().next().unwrap_or_default();
            let room = width.saturating_sub(13);
            let text: String = if first.chars().count() > room {
                format!("{}…", first.chars().take(room.saturating_sub(1)).collect::<String>())
            } else {
                first.to_string()
            };
            // Tagged apart, so it doesn't read as part of the prompt below.
            lines.push(Line::from(vec![
                Span::raw("  "),
                Span::styled("[queued]", Style::new().fg(Color::Yellow)),
                Span::raw(" "),
                Span::styled(text, dim()),
            ]));
        }
        lines
    }

    /// Rows an open panel needs under the prompt, border included.
    fn panel_height(&self) -> Option<u16> {
        let rows = match &self.overlay {
            Overlay::None => return None,
            Overlay::Models { rows: Some(Ok(list)), .. } => list.len(),
            Overlay::Models { .. } => 1,
            Overlay::Mcp { rows: Some(Ok(list)), .. } => list.len().max(1),
            Overlay::Mcp { .. } => 1,
            Overlay::Projects { rows: Some(Ok(list)), .. } => list.len() + 2,
            Overlay::Projects { .. } => 1,
            Overlay::Help => help_lines().len(),
            Overlay::Status(rows) => self.status_panel_lines(rows).len(),
            Overlay::Usage { loading } => self.usage_panel_lines(*loading).len(),
            // Its command, all it printed, and how it ended or that it runs.
            Overlay::Shell(id) => self.shell_of(*id).map_or(1, |s| s.command.lines().count() + s.output.lines().count() + 1),
            Overlay::Jobs { .. } => self.jobs.len().max(1),
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
        // Indexing isn't shown: it runs in the background.
        match (&self.phase, &self.turn) {
            // Setup says one step at a time, as a turn says what it's doing:
            // the step running, with its download's bar while there is one.
            (Phase::Setup, _) if self.setup.failure.is_none() => match self.setup.active() {
                Some(step) => {
                    let took = format_duration(step.started.map_or(Duration::ZERO, |t| t.elapsed()));
                    match step.bytes {
                        Some((done, total)) => {
                            let mut spans = vec![spinner.clone(), Span::styled(format!("{}… ", step.label), dim())];
                            spans.extend(bar_spans(done, total, 20));
                            spans.push(Span::styled(format!(" ({})", took), dim()));
                            lines.push(Line::from(spans));
                        }
                        None if step.detail.is_empty() => lines.push(working(step.label.clone(), took)),
                        None => lines.push(working(step.label.clone(), format!("{} · {}", step.detail, took))),
                    }
                }
                None => lines.push(working("Connecting to the agent".to_string(), format_duration(self.setup.started.elapsed()))),
            },
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
    /// │ █▀▀▀▄ ▄▀▀▀▄ ▄▀▀▀▀ █▀▀▀▀ █▄  █ ▀▀█▀▀    project  notes                     │
    /// │ █   █ █   █ █     █▀▀▀  █ ▀▄█   █               5 docs · 2 connections    │
    /// │ ▀▀▀▀   ▀▀▀   ▀▀▀▀ ▀▀▀▀▀ ▀   ▀   ▀      model    qwen3.5-2b +1 more        │
    /// │ Frontier quality. Local first.         agent    ▰▰▰▱▱▱▱▱▱▱ 31% memory     │
    /// │ by Smartloop · ? for shortcuts         account  mehfuz@smartloop.ai       │
    /// │                                                 CLI 1.0.42 · agent 1.2.10 │
    /// │                                                                           │
    /// │ Here are some commands to get you started                                 │
    /// │   /login     sign in for web search and more models                       │
    /// │   /models    pick a model; the one for your system is marked              │
    /// │   /status    account, model, agent and versions                           │
    /// ╰───────────────────────────────────────────────────────────────────────────╯
    /// ```
    /// The agent's state at a glance: a green square when it's healthy, the
    /// spinner while it indexes or is first asked, and a red square with what's
    /// wrong otherwise.
    fn agent_mark(&self) -> Span<'static> {
        match self.agent.as_ref().map(|a| a.state.as_str()) {
            Some("healthy") => Span::styled("■", Style::new().fg(Color::Green)),
            None | Some("indexing") => Span::styled(self.spinner(), Style::new().fg(pink())),
            Some(problem) => Span::styled(format!("■ {}", problem), Style::new().fg(Color::Red)),
        }
    }

    /// The card's details, by label: the project and what it holds, the
    /// agent's state and memory, who's signed in, and the versions.
    fn card_details(&self) -> Vec<(&'static str, Vec<Span<'static>>)> {
        let project = vec![Span::raw(self.project.as_ref().map_or("…".to_string(), |(_, name)| name.clone()))];
        // What it holds, on a row of its own under it.
        let documents = self.agent.as_ref().map_or(0, |a| a.documents);
        let (ready, connecting) = self.connections.unwrap_or_default();
        let plural = |n: usize, what: &str| format!("{} {}{}", n, what, if n == 1 { "" } else { "s" });
        let mut holds = vec![Span::styled(
            format!("{} · {}", plural(documents, "doc"), plural(ready + connecting, "connection")),
            dim(),
        )];
        // The spinner while some are still connecting.
        if connecting > 0 {
            holds.push(Span::styled(format!(" {}", self.spinner()), Style::new().fg(pink())));
        }
        // How much memory the enabled models take, which also says it's up;
        // the spinner before it while indexing. Without a figure, the mark
        // alone, as for a problem.
        let state = self.agent.as_ref().map(|a| a.state.as_str());
        let memory = self.agent.as_ref().and_then(|a| a.memory).filter(|_| matches!(state, Some("healthy" | "indexing")));
        let mut agent = Vec::new();
        match memory {
            Some(memory) => {
                if state == Some("indexing") {
                    agent.push(self.agent_mark());
                    agent.push(Span::raw(" "));
                }
                agent.extend(memory_bar(memory));
                agent.push(Span::styled(format!(" {}% memory", memory), dim()));
            }
            None => agent.push(self.agent_mark()),
        }
        let account = match &self.account {
            None => vec![Span::styled("…", dim())],
            Some(Some(who)) => vec![Span::raw(who.clone())],
            Some(None) => vec![Span::styled("not signed in · /login", dim())],
        };
        let versions = format!("CLI {} · agent {}", env!("CARGO_PKG_VERSION"), framework::VERSION);
        let model = match self.enabled_models.as_deref() {
            None => vec![Span::styled("…", dim())],
            Some([]) => {
                let warn = Style::new().fg(Color::Yellow);
                let mut spans = vec![Span::styled("⚠ none enabled · ", warn), Span::styled("ctrl+o", key_style())];
                spans.push(Span::styled(" to pick one", warn));
                spans
            }
            Some([one]) => vec![Span::raw(one.clone())],
            Some([first, rest @ ..]) => vec![Span::raw(first.clone()), Span::styled(format!(" +{} more", rest.len()), dim())],
        };
        vec![
            ("project", project),
            ("", holds),
            ("model", model),
            ("agent", agent),
            ("account", account),
            ("", vec![Span::styled(versions, dim())]),
        ]
    }

    fn welcome(&self, width: u16) -> Vec<Line<'static>> {
        let width = width as usize;
        let border = dim();
        let right = self.card_details();
        // Styled per span: the rows are taken apart into spans below.
        let mut left: Vec<Line<'static>> = crate::BANNER
            .iter()
            .map(|row| Line::from(Span::styled(*row, Style::new().fg(pink()))))
            .collect();
        left.push(Line::from(Span::styled("Frontier quality. Local first.", dim())));
        left.push(Line::from(Span::styled("by Smartloop · ? for shortcuts", dim())));
        let left_width = left.iter().map(Line::width).max().unwrap_or(0) + 4;

        // The label, then the value cut to `room`.
        let row = |label: &str, value: &[Span<'static>], room: usize| {
            let mut spans = vec![Span::styled(format!("{:<9}", label), dim())];
            spans.extend(fit(value, room));
            Line::from(spans)
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
                        spans.extend(row(label, value, inner.saturating_sub(left_width + 9)).spans);
                    }
                    Line::from(spans)
                })
                .collect()
        } else {
            let mut body = left;
            body.push(Line::default());
            body.extend(right.iter().map(|(l, v)| row(l, v, inner.saturating_sub(9))));
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
        for line in std::iter::once(&blank).chain(body.iter()) {
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

    /// Remember the conversation as drawn, for copying and for clicking its
    /// links, make the links hyperlinks, and highlight the selection over it.
    /// The footer's actions are kept for clicking too; it isn't selected
    /// over.
    fn keep_shown(&self, frame: &mut Frame, area: Rect, footer: Rect) {
        let buffer = frame.buffer_mut();
        let rows = (area.top()..area.bottom())
            .map(|y| (area.left()..area.right()).map(|x| buffer[(x, y)].symbol().to_string()).collect())
            .collect();
        // Tests read the screen back as text, so they go without the escapes.
        let links = crate::links::apply(buffer, area, !cfg!(test));
        let footer_links = crate::links::apply(buffer, footer, !cfg!(test));
        *self.shown.borrow_mut() = Shown { area, rows, links, footer, footer_links };
        if let Some(selection) = &self.selection {
            for (y, from, to) in selection.rows(area) {
                for x in from..=to {
                    buffer[(x, y)].set_style(Style::new().add_modifier(Modifier::REVERSED));
                }
            }
        }
    }

    /// One entry of the conversation as drawn, after a blank line.
    fn entry_lines(&self, entry: &Entry, width: u16, lines: &mut Vec<Line<'static>>) {
        // A command running in the background is listed under the prompt
        // instead, out of the conversation's way until it ends.
        if matches!(entry, Entry::Shell(s) if s.background && s.ended.is_none()) {
            return;
        }
        lines.push(Line::default());
        match entry {
            Entry::User(text) => {
                for (i, line) in text.lines().enumerate() {
                    let mark = if i == 0 { "> " } else { "  " };
                    lines.push(Line::from(vec![Span::styled(mark, dim()), Span::styled(line.to_string(), dim())]));
                }
            }
            Entry::Reply(reply) => reply_lines(reply, width, lines),
            Entry::Shell(shell) => shell_lines(shell, self.spinner(), SHELL_SHOWN, lines),
            Entry::Ask(commands) => ask_lines(commands, lines),
            Entry::Notice(text) => {
                lines.push(Line::from(vec![Span::styled("■ ", Style::new().fg(Color::Green)), Span::raw(text.clone())]))
            }
            Entry::Warning(text) => lines.push(Line::from(vec![
                Span::styled("■ ", Style::new().fg(Color::Yellow)),
                Span::styled(text.clone(), dim()),
            ])),
            Entry::Error(text) => lines.push(error_line(text)),
        }
    }

    /// The welcome card that opens the conversation, like a first message,
    /// and scrolls away with it, with the setup's card and failure.
    fn head_lines(&self, width: u16) -> Vec<Line<'static>> {
        let mut lines = self.welcome(width);
        if let Some(e) = &self.setup.failure {
            lines.push(Line::default());
            lines.push(error_line(e));
        }
        lines
    }

    /// Draw the conversation: only the entries the screen shows, from the
    /// one the scroll position lands in to the last, so a long conversation
    /// costs no more to draw than a short one. Each entry's height is kept
    /// from the last time it was laid out, so the whole is still measured
    /// for scrolling.
    fn draw_conversation(&self, frame: &mut Frame, area: Rect) {
        let width = area.width;
        let rows_of = |lines: &[Line<'static>]| Paragraph::new(lines.to_vec()).wrap(Wrap { trim: false }).line_count(width);
        let head = self.head_lines(width);
        let head_rows = rows_of(&head);
        let mut cache = self.heights.borrow_mut();
        if cache.len() > self.entries.len() {
            cache.clear();
        }
        cache.resize(self.entries.len(), None);
        let heights: Vec<usize> = self
            .entries
            .iter()
            .enumerate()
            .map(|(i, entry)| {
                // A reply's version is the text it's laid out from: laid
                // out now, so the frame measures and draws the same lines.
                if let Entry::Reply(reply) = entry {
                    reply.layout(width);
                }
                match cache[i] {
                    Some((w, version, rows)) if w == width && version == entry_version(entry) => rows,
                    _ => {
                        let mut lines = Vec::new();
                        self.entry_lines(entry, width, &mut lines);
                        let rows = rows_of(&lines);
                        cache[i] = Some((width, entry_version(entry), rows));
                        rows
                    }
                }
            })
            .collect();
        let total = head_rows + heights.iter().sum::<usize>();
        let height = area.height as usize;
        // Anchored to the bottom, just above the prompt: a short conversation
        // sits low and grows upward, as in Claude Code.
        let bottom = total.saturating_sub(height);
        self.scroll_limit.set(bottom.min(u16::MAX as usize) as u16);
        let top = bottom.saturating_sub(self.scroll_back as usize);
        // Back from the last entry to the one that holds row `top`.
        let mut start = total;
        let mut first = self.entries.len();
        while first > 0 && start > top {
            first -= 1;
            start -= heights[first];
        }
        let mut lines = Vec::new();
        if start > top {
            lines.extend(head);
            start = 0;
        }
        for entry in &self.entries[first..] {
            self.entry_lines(entry, width, &mut lines);
        }
        let paragraph = Paragraph::new(lines).wrap(Wrap { trim: false });
        let drawn = (total.min(height)) as u16;
        let area = Rect { y: area.y + area.height - drawn, height: drawn, ..area };
        frame.render_widget(paragraph.scroll(((top - start).min(u16::MAX as usize) as u16, 0)), area);
    }

    /// Rows the prompt shows at `width`: each line typed, wrapped, up to
    /// `PROMPT_LINES`.
    fn input_rows(&self, width: u16) -> u16 {
        let label = if self.token_entry { "token: " } else if self.input.starts_with('!') { "! " } else { "> " };
        let input = if !self.token_entry && self.input.starts_with('!') { &self.input[1..] } else { self.input.as_str() };
        let room = width.saturating_sub(label.len() as u16 + 1) as usize;
        (prompt_rows(input, room).len() as u16).clamp(1, PROMPT_LINES)
    }

    fn draw_input(&self, frame: &mut Frame, area: Rect) {
        // Between two rules across the screen, as in Claude Code; in the
        // brand color while a `!command` is typed, as it runs in the shell.
        let shell = !self.token_entry && self.input.starts_with('!');
        let rules = if shell { Style::new().fg(pink()) } else { dim() };
        let block = Block::new().borders(Borders::TOP | Borders::BOTTOM).border_style(rules);
        let inner = block.inner(area);
        let ready = matches!(self.phase, Phase::Ready) && self.project.is_some();
        // A shell command has `!` ahead of it, as Claude Code's shell mode
        // does: the `!` typed is what the mark stands for, so it isn't
        // shown twice, and the cursor moves up with it.
        let label = if self.token_entry { "token: " } else if shell { "! " } else { "> " };
        let prompt = Span::styled(label, Style::new().fg(pink()).bold());
        // A token shows as dots.
        let input = if self.token_entry {
            "•".repeat(self.input.chars().count())
        } else if shell {
            self.input[1..].to_string()
        } else {
            self.input.clone()
        };
        let cursor = if shell { self.cursor.saturating_sub(1) } else { self.cursor };
        let line = if self.token_entry && input.is_empty() {
            Line::from(vec![prompt, Span::styled("paste your Smartloop token", dim())])
        } else if input.is_empty() && !ready && !shell {
            // A shell command runs with no agent, so `!` isn't held up.
            Line::from(vec![prompt, Span::styled("waiting for setup…", dim())])
        } else {
            // Each line typed wraps at the prompt's width, under the text
            // rather than the mark; when there are more rows than room, the
            // rows follow the cursor.
            let indent = label.len() as u16;
            let room = inner.width.saturating_sub(indent + 1) as usize;
            let rows = prompt_rows(&input, room);
            let chars: Vec<char> = input.chars().collect();
            let row = cursor_row(&rows, cursor);
            let col: usize = chars[rows[row].0..cursor.min(chars.len())]
                .iter()
                .map(|c| unicode_width::UnicodeWidthChar::width(*c).unwrap_or(0))
                .sum();
            let first = (row + 1).saturating_sub(inner.height as usize);
            let mut shown = Vec::new();
            for (i, &(start, end)) in rows.iter().enumerate().skip(first).take(inner.height as usize) {
                let mark = if i == 0 { prompt.clone() } else { Span::raw(" ".repeat(indent as usize)) };
                let text: String = chars[start..end].iter().collect();
                let mut spans = vec![mark, Span::raw(text)];
                // Until something's typed, say what the prompt takes.
                if input.is_empty() && shell {
                    spans.push(Span::styled("a command to run in the folder; & at its end runs it in the background", dim()));
                } else if input.is_empty() && !self.token_entry {
                    spans.push(Span::styled("Ask anything, or drop a file to attach it", dim()));
                }
                shown.push(Line::from(spans));
                if i == row && matches!(self.overlay, Overlay::None) {
                    let x = inner.x + indent + col as u16;
                    let y = inner.y + (i - first) as u16;
                    frame.set_cursor_position((x.min(inner.right().saturating_sub(1)), y));
                }
            }
            return frame.render_widget(Paragraph::new(shown).block(block), area);
        };
        frame.render_widget(Paragraph::new(line).block(block), area);
    }

    /// `  /models     models: enable, disable, download`, one per matching
    /// command, the highlighted one in pink. A table `width` wide: names in
    /// a column as wide as the longest, descriptions wrapped under their
    /// own column. At most `HINT_ROWS` rows show, around the highlighted
    /// one.
    fn hint_lines(&self, width: usize) -> Vec<Line<'static>> {
        let hints = self.command_hints();
        let at = self.hint_at.min(hints.len().saturating_sub(1));
        // Sized over every match, not just those shown, so the column holds
        // still while scrolling.
        let longest = hints.iter().map(|(name, _)| name.chars().count()).max().unwrap_or(0);
        let column = longest.clamp(HINT_NAME_MIN, HINT_NAME_MAX);
        // The mark before the name, and the gap after it.
        let described = width.saturating_sub(column + 4).max(1);
        let rows: Vec<Vec<Line<'static>>> = hints
            .iter()
            .enumerate()
            .map(|(i, (name, what))| {
                let (mark, style) = if i == at {
                    ("› ", Style::new().fg(pink()).bold())
                } else {
                    ("  ", dim())
                };
                let text = if i == at { Style::new() } else { dim() };
                wrap(what, described)
                    .into_iter()
                    .enumerate()
                    .map(|(n, part)| {
                        let lead = if n == 0 {
                            Span::styled(format!("{}{:<column$}  ", mark, truncate(name, column)), style)
                        } else {
                            Span::raw(" ".repeat(column + 4))
                        };
                        Line::from(vec![lead, Span::styled(part, text)])
                    })
                    .collect()
            })
            .collect();
        // Back from the highlighted hint as far as fits, then on after it.
        let mut first = at;
        let mut used = rows.get(at).map_or(0, Vec::len);
        while first > 0 && used + rows[first - 1].len() <= HINT_ROWS {
            first -= 1;
            used += rows[first].len();
        }
        let mut last = at;
        while last + 1 < rows.len() && used + rows[last + 1].len() <= HINT_ROWS {
            last += 1;
            used += rows[last].len();
        }
        rows.into_iter().skip(first).take(last + 1 - first).flatten().collect()
    }

    /// A shell run expanded under the prompt: its command and all it
    /// printed, the end of it when there's more than fits, and how it
    /// ended, or that it runs, with `✕ stop` to click while it does. Gone
    /// with a `/clear`, the key hints show instead.
    fn draw_shell(&self, frame: &mut Frame, area: Rect, id: usize) {
        let Some(shell) = self.shell_of(id) else { return self.draw_footer(frame, area) };
        let mut lines = Vec::new();
        shell_lines(shell, self.spinner(), usize::MAX, &mut lines);
        let mut bar = bar_hints(&[("esc", "close")]);
        if shell.ended.is_none() {
            // The key stops it, and a click on the key's hint too.
            let stop = crate::links::action(Style::new(), &format!("stop/{}", id));
            bar.extend(key_hints(&[("x", "stop")]).into_iter().map(|span| span.patch_style(stop)));
            bar.push(Span::raw(" "));
        }
        let block = panel(&bar);
        let skip = lines.len().saturating_sub(block.inner(area).height as usize);
        frame.render_widget(Paragraph::new(lines).block(block).scroll((skip.min(u16::MAX as usize) as u16, 0)), area);
    }

    /// `ctrl+b`: the commands still running as the footer lists them, the
    /// selected one marked, with the keys that expand or stop it.
    fn draw_jobs(&self, frame: &mut Frame, area: Rect, selected: usize) {
        let block = panel(&bar_hints(&[("↑↓", "select"), ("enter", "output"), ("x", "stop"), ("esc", "close")]));
        frame.render_widget(Clear, area);
        let lines = self.job_lines(block.inner(area), Some(selected));
        frame.render_widget(Paragraph::new(lines).block(block), area);
    }

    /// A row for each command still running, as many as fit `area`: its
    /// first line, which a click expands, and how long it's been. With one
    /// `selected`, a `›` marks it; `/tasks` or `ctrl+b` picks one to stop
    /// or expand from the keyboard, as `/tasks` does in Claude Code.
    fn job_lines(&self, area: Rect, selected: Option<usize>) -> Vec<Line<'static>> {
        let room = area.height as usize;
        let mut lines = Vec::new();
        for (i, job) in self.jobs.iter().enumerate().rev().take(room).rev() {
            let command = job.command.lines().next().unwrap_or_default();
            let background = if self.entries.iter().any(|e| matches!(e, Entry::Shell(s) if s.id == job.id && s.background)) {
                ", background"
            } else {
                ""
            };
            let chosen = selected == Some(i);
            let mark = if chosen { "› " } else { "  " };
            let name = if chosen { Style::new().fg(pink()).bold() } else { key_style() };
            let show = crate::links::action(name, &format!("show/{}", job.id));
            lines.push(Line::from(vec![
                Span::styled(mark, Style::new().fg(pink()).bold()),
                Span::styled(format!("{} ", self.spinner()), dim()),
                Span::styled(truncate(command, (area.width as usize).saturating_sub(30).max(10)), show),
                Span::styled(format!(" ({}{})", format_duration(job.started.elapsed()), background), dim()),
            ]));
        }
        lines
    }

    /// Key hints under the prompt, under the commands still running.
    fn draw_footer(&self, frame: &mut Frame, area: Rect) {
        let mut lines = self.job_lines(Rect { height: area.height.saturating_sub(1), ..area }, None);
        let mut hints = vec![Span::raw("  ")];
        let recent = |at: &Instant| at.elapsed() < Duration::from_secs(2);
        let opened = self.opened.as_ref().filter(|(_, at)| recent(at));
        if let Some((chars, _)) = self.copied.filter(|(_, at)| recent(at)) {
            hints.push(Span::styled("✓ ", Style::new().fg(Color::Green)));
            hints.push(Span::styled(format!("Copied {} characters", chars), dim()));
        } else if let Some((url, _)) = opened {
            hints.push(Span::styled("↗ ", Style::new().fg(Color::Green)));
            hints.push(Span::styled(format!("Opening {}", url), dim()));
        } else if self.scroll_back > 0 {
            hints.push(Span::styled(format!("↓ {} lines below  ", self.scroll_back), dim()));
            hints.extend(key_hints(&[("pgdn", "scroll down"), ("end", "latest")]));
        } else if self.token_entry {
            hints.extend(key_hints(&[("enter", "sign in"), ("esc", "cancel")]));
        } else if self.input.starts_with('!') {
            // Shell mode, said as Claude Code says it, in the rules' color.
            hints.push(Span::styled("! for shell mode", Style::new().fg(pink())));
        } else if self.ask.is_some() && self.input.is_empty() && self.turn.is_none() {
            hints.extend(key_hints(&[("enter", "run"), ("tab", "always this session"), ("esc", "skip")]));
        } else if self.input.trim() == "/project import" {
            // Asked from the projects panel, or typed: what it wants next.
            hints.push(Span::styled("Drop an exported project's .zip here, or type its path", dim()));
        } else if self.last_esc.is_some_and(|t| t.elapsed() < DOUBLE_ESC) {
            hints.extend(key_hints(&[("esc", "again to clear")]));
        } else if let Some(warning) = self.search_warning(area.width.saturating_sub(2) as usize) {
            // Takes the key hints' place, so a narrow terminal can't hide it.
            hints.extend(warning);
        } else if let Some(notice) = self.update_notice(area.width.saturating_sub(2) as usize) {
            hints.extend(notice);
        } else {
            // The rest live under help, as in Claude Code.
            let mut keys = vec![("enter", if self.turn.is_some() { "queue" } else { "send" }), ("esc", "interrupt")];
            if !self.queued.is_empty() && self.input.is_empty() {
                keys.push(("↑", "unqueue"));
            }
            if self.always_run {
                keys.push(("tab", "auto-approve on"));
            }
            if !self.jobs.is_empty() {
                keys.push((tasks_key(), "running"));
            }
            keys.push(("?", "shortcuts"));
            hints.extend(key_hints(&keys));
        }
        lines.push(Line::from(hints));
        frame.render_widget(Paragraph::new(lines), area);
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

    /// A newer release asks to be installed, in place of the key hints:
    /// the longest wording that fits `width`, down to the version alone.
    fn update_notice(&self, width: usize) -> Option<Vec<Span<'static>>> {
        let version = self.update.as_ref()?;
        let command = crate::update::upgrade_command();
        let wordings = [
            (format!("docent {} is available (you have {}) · upgrade: ", version, env!("CARGO_PKG_VERSION")), Some(command)),
            (format!("docent {} is available · upgrade: ", version), Some(command)),
            (format!("docent {} is available · rerun the installer to upgrade", version), None),
            (format!("docent {} available", version), None),
        ];
        let size = |(text, command): &(String, Option<&str>)| 2 + text.chars().count() + command.map_or(0, |c| c.chars().count());
        let (text, command) = wordings.iter().find(|w| size(w) <= width).unwrap_or(&wordings[wordings.len() - 1]).clone();
        let mut spans = vec![Span::styled("↑ ", Style::new().fg(Color::Yellow)), Span::styled(text, Style::new().fg(Color::Yellow))];
        if let Some(command) = command {
            spans.push(Span::styled(command, key_style()));
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
        let label = |m: &ModelRow| if m.cloud { format!("{} ☁", m.name) } else { m.name.clone() };
        let name_width = list.iter().map(|m| label(m).chars().count()).max().unwrap_or(0) + 2;
        let lines: Vec<Line> = list
            .iter()
            .enumerate()
            .map(|(i, m)| {
                let chosen = i == selected;
                let mut spans = vec![
                    Span::styled(if chosen { "› " } else { "  " }, Style::new().fg(pink()).bold()),
                    Span::styled(
                        format!("{:<w$}", label(m), w = name_width),
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
                    None if m.cloud || !m.accessible => Span::styled("disabled", dim()),
                    None if m.downloaded => Span::styled("downloaded", dim()),
                    None => Span::styled("not downloaded", dim()),
                };
                let state_width = state.width();
                spans.push(state);
                spans.push(Span::raw(" ".repeat(16usize.saturating_sub(state_width))));
                spans.push(Span::styled(m.capabilities.clone(), dim()));
                if m.overridden {
                    spans.push(Span::styled(" · set here", dim()));
                }
                if m.recommended && !m.cloud {
                    spans.push(Span::styled("  · recommended", Style::new().fg(Color::Green)));
                }
                if !m.accessible {
                    // Signed in but still locked: the plan, not the sign-in, is what's missing.
                    let why = if self.search.is_some() { "  · pro plan" } else { "  · sign in to use" };
                    spans.push(Span::styled(why, Style::new().fg(Color::Yellow)));
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

    /// The projects to switch to, the one in use marked, each with its
    /// description; "+ New project" and "+ Import project" last.
    fn draw_projects(
        &self,
        frame: &mut Frame,
        area: Rect,
        rows: &Option<Result<Vec<Project>, String>>,
        selected: usize,
        deleting: bool,
    ) {
        let list = rows.as_ref().and_then(|r| r.as_ref().ok());
        let hint = match list.and_then(|l| l.get(selected)).filter(|_| deleting) {
            Some(project) => {
                let mut spans = vec![Span::raw(" "), Span::styled(format!("Delete {}? ", project.name), Style::new().fg(Color::Red))];
                spans.extend(key_hints(&[("ctrl+d", "delete"), ("esc", "cancel")]));
                spans.push(Span::raw(" "));
                spans
            }
            None => bar_hints(&[
                ("↑↓", "select"),
                ("enter", "switch"),
                ("ctrl+n", "new"),
                ("ctrl+d", "delete"),
                ("ctrl+r", "refresh"),
                ("esc", "close"),
            ]),
        };
        let block = panel(&hint);
        frame.render_widget(Clear, area);
        let list = match rows {
            None => return frame.render_widget(Paragraph::new(format!(" Loading projects {}", self.spinner())).block(block), area),
            Some(Err(e)) => return frame.render_widget(Paragraph::new(e.clone()).block(block).wrap(Wrap { trim: false }), area),
            Some(Ok(list)) => list,
        };
        let current = self.project.as_ref().map(|(id, _)| id.as_str());
        let name_width = list.iter().map(|p| p.name.chars().count()).max().unwrap_or(0).min(40) + 2;
        let mut lines: Vec<Line> = list
            .iter()
            .enumerate()
            .map(|(i, p)| {
                let chosen = i == selected;
                let mut spans = vec![
                    Span::styled(if chosen { "› " } else { "  " }, Style::new().fg(pink()).bold()),
                    Span::styled(
                        format!("{:<w$}", truncate(&p.name, name_width - 2), w = name_width),
                        if chosen { Style::new().fg(pink()).bold() } else { Style::new() },
                    ),
                ];
                let state = match (current == Some(p.id.as_str()), p.system) {
                    (true, _) => Span::styled("✓ current", Style::new().fg(Color::Green)),
                    (false, true) => Span::styled("system", dim()),
                    (false, false) => Span::raw(""),
                };
                let state_width = state.width();
                spans.push(state);
                spans.push(Span::raw(" ".repeat(12usize.saturating_sub(state_width))));
                spans.push(Span::styled(p.description.clone(), dim()));
                Line::from(spans)
            })
            .collect();
        for (i, label) in ["+ New project", "+ Import project"].into_iter().enumerate() {
            let chosen = selected == list.len() + i;
            lines.push(Line::from(vec![
                Span::styled(if chosen { "› " } else { "  " }, Style::new().fg(pink()).bold()),
                Span::styled(label, if chosen { Style::new().fg(pink()).bold() } else { dim() }),
            ]));
        }
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

    /// `/usage`: this month's credits, one pool for cloud models and web
    /// searches, read as Studio's "Plan & usage" card: the bar fills as
    /// they're spent, and stays full and muted when they're unlimited.
    ///
    /// ```text
    ///  Credits     ███████░░░░░░░░░░░░░░░░░░░░░░░  24% used
    ///              480 of 2,000 credits used
    ///              3 calls · 12,000 tokens · Pro · resets Dec 1
    ///              credits cover cloud models and web searches; local models are always free
    ///              /upgrade to Plus for more credits a month
    /// ```
    fn usage_panel_lines(&self, loading: bool) -> Vec<Line<'static>> {
        let label = |text: &str| Span::styled(format!(" {:<12}", text), dim());
        let indent = || Span::raw(format!(" {:<12}", ""));
        let switch = (self.web_search == Some(false)).then(|| {
            Line::from(vec![indent(), Span::styled("web search is off for this project · ctrl+s turns it on", dim())])
        });
        let Some(credits) = self.credits.as_ref() else {
            let text = if loading {
                format!("asking the agent {}", self.spinner())
            } else {
                "sign in with /login to see your usage".to_string()
            };
            return std::iter::once(Line::from(vec![label("Credits"), Span::styled(text, dim())])).chain(switch).collect();
        };
        let mut lines = Vec::new();
        let mut head = vec![label("Credits")];
        match credits.fraction_used() {
            Some(fraction) => {
                let color = if credits.exhausted() {
                    Color::Red
                } else if credits.low() {
                    Color::Yellow
                } else {
                    pink()
                };
                head.push(Span::styled(progress::bar(fraction, USAGE_BAR), Style::new().fg(color)));
                head.push(Span::styled(format!("  {} used", credits.percent()), key_style()));
                if credits.exhausted() || credits.low() {
                    let flag = if credits.exhausted() { " · allowance used" } else { " · running low" };
                    head.push(Span::styled(flag, Style::new().fg(color)));
                }
            }
            None => {
                // Nothing to fill toward: a full, muted bar, as Studio draws it.
                head.push(Span::styled(progress::bar(1.0, USAGE_BAR), dim()));
                let used = credits.amounts().unwrap_or_else(|| "unlimited".to_string());
                head.push(Span::styled(format!("  {}", used), key_style()));
                if credits.amounts().is_some() {
                    head.push(Span::styled(" · unlimited", dim()));
                }
            }
        }
        lines.push(Line::from(head));
        // Metered with the amounts given: what's spent of what's allowed.
        if let Some(amounts) = credits.amounts().filter(|_| !credits.unmetered()) {
            lines.push(Line::from(vec![indent(), Span::styled(amounts, Style::new())]));
        }
        lines.push(Line::from(vec![indent(), Span::styled(credits.detail(), dim())]));
        lines.push(Line::from(vec![indent(), Span::styled(credits.note(), dim())]));
        // The plans with somewhere to go: Pro and Plus for free, Plus for Pro.
        let more = match credits.plan.as_str() {
            "free" => Some(" to Pro or Plus for more credits a month"),
            "paid" => Some(" to Plus for more credits a month"),
            _ => None,
        };
        if let Some(more) = more {
            lines.push(Line::from(vec![indent(), Span::styled("/upgrade", key_style()), Span::styled(more, dim())]));
        }
        lines.extend(switch);
        lines
    }

    fn spinner(&self) -> &'static str {
        SPINNER[self.ticks % SPINNER.len()]
    }
}

/// The project a folder never linked gets, named after it: one already so
/// named, by id, when it has no folder of its own (`has_folder`); else a
/// new one, numbered past any name taken.
fn adopted(folder: &str, taken: &[(String, String)], has_folder: impl Fn(&str) -> bool) -> (String, Option<String>) {
    if let Some((id, _)) = taken.iter().find(|(_, name)| name == folder)
        && !has_folder(id)
    {
        return (folder.to_string(), Some(id.clone()));
    }
    let mut name = folder.to_string();
    let mut n = 2;
    while taken.iter().any(|(_, t)| *t == name) {
        name = format!("{}-{}", folder, n);
        n += 1;
    }
    (name, None)
}

/// What of an entry its height depends on: a reply's text as last laid
/// out, with its warnings, citations and end; a shell entry's output
/// having come. The rest never change.
///
/// Heights are kept by the entry's place in the list, and entries come
/// out of it (an answered question, a finished run): what's in a place
/// changes, so the version tells entries apart by kind and content, not
/// only by how far they've come.
fn entry_version(entry: &Entry) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    std::mem::discriminant(entry).hash(&mut h);
    match entry {
        Entry::Reply(reply) => {
            let laid_out = reply.rendered.borrow().as_ref().map_or(0, |(len, ..)| *len);
            (laid_out, reply.text.len(), reply.warnings.len(), reply.citations.len(), reply.stats.is_some(), reply.ended.is_some()).hash(&mut h);
        }
        Entry::Shell(shell) => (shell.id, shell.output.len(), shell.ended.is_some()).hash(&mut h),
        Entry::Ask(commands) => commands.hash(&mut h),
        Entry::User(text) | Entry::Notice(text) | Entry::Warning(text) | Entry::Error(text) => text.hash(&mut h),
    }
    h.finish()
}

/// The capabilities a `/models <name> caps` list names: `auto` for none
/// of our own, else names split on commas, lower-cased.
fn capability_list(list: &str) -> Result<Vec<String>, String> {
    if list.eq_ignore_ascii_case("auto") {
        return Ok(Vec::new());
    }
    let caps: Vec<String> = list.split(',').map(|c| c.trim().to_lowercase()).filter(|c| !c.is_empty()).collect();
    if caps.is_empty() {
        return Err("Name the capabilities, as in /models <name> caps text,vision,reasoning; or auto".to_string());
    }
    Ok(caps)
}

/// The kind of code project at `root`, as in "Rust", when prompts there are
/// answered with code: by its mark when it has one (set when it was
/// linked), else by its files.
fn code_kind(root: &std::path::Path) -> Option<&'static str> {
    match docent::kind(root) {
        Some(docent::Kind::Code) => Some(files::project_kind(root).unwrap_or("code")),
        Some(docent::Kind::Documents) => None,
        None => files::project_kind(root),
    }
}

/// The folder `/sync` works on: the one the working directory is part of.
fn sync_root() -> Option<std::path::PathBuf> {
    let root = docent::root(&std::env::current_dir().ok()?);
    Some(std::fs::canonicalize(&root).unwrap_or(root))
}

/// The folder `project` syncs from: the one it was last linked to, else the
/// working directory's when that's linked to it.
fn project_root(project: &str) -> Option<std::path::PathBuf> {
    docent::folder(project).or_else(|| sync_root().filter(|root| docent::linked(root).is_some_and(|l| l.project == project)))
}

/// `3 documents here aren't searchable yet (a.pdf, b.docx, and 1 more)…`,
/// or nothing when there are none.
fn unsynced_hint(found: &[String], project: &str) -> Option<String> {
    let n = found.len();
    if n == 0 {
        return None;
    }
    let shown = found.iter().take(2).cloned().collect::<Vec<_>>().join(", ");
    let more = if n > 2 { format!(", and {} more", n - 2) } else { String::new() };
    let (noun, verb) = if n == 1 { ("document", "isn't") } else { ("documents", "aren't") };
    Some(format!("{n} {noun} here {verb} searchable yet ({shown}{more}); /sync adds them to project {project}"))
}

/// A reply as Claude Code lays one out: the answer under a `■` (square, like the setup blocks), its
/// sources, then `⎿` with the model that answered and how it went.
fn reply_lines(reply: &Reply, width: u16, lines: &mut Vec<Line<'static>>) {
    for warning in &reply.warnings {
        lines.push(Line::from(vec![
            Span::styled("■ ", Style::new().fg(Color::Yellow)),
            Span::styled(warning.clone(), dim()),
        ]));
    }
    for (i, line) in reply.body(width.saturating_sub(2)).into_iter().enumerate() {
        let mark = if i == 0 { Span::raw("■ ") } else { Span::raw("  ") };
        lines.push(Line::from(std::iter::once(mark).chain(line.spans).collect::<Vec<_>>()));
    }
    if !reply.citations.is_empty() {
        lines.push(Line::default());
        lines.push(Line::styled("  References", dim()));
        for (i, label) in reply.citations.iter().enumerate() {
            let mut spans = vec![Span::styled(format!("  [{}] ", i + 1), dim())];
            spans.extend(crate::links::linkify(label, Style::new()));
            lines.push(Line::from(spans));
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
/// The prompt's rows: each line of `text` wrapped to `room` cells, at the
/// last space that fits when there is one, as `(start, end)` char offsets
/// into `text`. A line break is a char of its own, on no row.
fn prompt_rows(text: &str, room: usize) -> Vec<(usize, usize)> {
    let room = room.max(1);
    let chars: Vec<char> = text.chars().collect();
    let mut rows = Vec::new();
    let mut start = 0;
    loop {
        let end = chars[start..].iter().position(|c| *c == '\n').map_or(chars.len(), |i| start + i);
        // This line, cut into rows.
        let mut at = start;
        loop {
            let mut width = 0;
            let mut cut = at;
            while cut < end {
                let w = unicode_width::UnicodeWidthChar::width(chars[cut]).unwrap_or(0);
                if width + w > room {
                    break;
                }
                width += w;
                cut += 1;
            }
            if cut >= end {
                rows.push((at, end));
                break;
            }
            // Back to just after the last space, so a word isn't split.
            if let Some(space) = chars[at..cut].iter().rposition(|c| *c == ' ').filter(|i| *i > 0) {
                cut = at + space + 1;
            }
            rows.push((at, cut));
            at = cut;
        }
        if end >= chars.len() {
            return rows;
        }
        start = end + 1;
    }
}

/// The row `cursor` (a char offset) is on. At a wrap, the cursor goes to
/// the start of the next row.
fn cursor_row(rows: &[(usize, usize)], cursor: usize) -> usize {
    rows.iter().rposition(|(start, _)| *start <= cursor).unwrap_or(0)
}

/// With no model enabled, which to pick: the one the agent recommends for
/// this machine's RAM, a reasoning model first since it resolves tool calls
/// and plans. `None` when one is enabled already.
fn recommended_pick(models: &[ModelRow]) -> Option<&ModelRow> {
    let recommended: Vec<&ModelRow> = models.iter().filter(|m| m.recommended && m.accessible).collect();
    recommended
        .iter()
        .find(|m| m.capabilities.split(", ").any(|c| c == "reasoning"))
        .or(recommended.first())
        .copied()
}

fn model_nudge(models: &[ModelRow]) -> Option<String> {
    if models.is_empty() || models.iter().any(|m| m.enabled) {
        return None;
    }
    Some(match recommended_pick(models) {
        Some(m) => format!(
            "No model selected. Recommended for your system: {} · enable it in /models (ctrl+o)",
            m.name
        ),
        None => "No model selected · open /models (ctrl+o) to enable one".to_string(),
    })
}

fn selected_model(message: &str) -> Option<String> {
    let name = message.strip_prefix("Selected ")?.split_whitespace().next()?;
    Some(name.to_string())
}

/// Lines of output a `!command` shows, from its end; the agent gets more.
const SHELL_SHOWN: usize = 20;
/// How many times a reply's failed command is sent back to be fixed.
const COMMAND_ROUNDS: u32 = 3;
/// How many times a reply's tool calls go back with their results, per
/// prompt: the agent looks around, builds and tests, a step a round.
const TOOL_ROUNDS: u32 = 8;
/// Characters of a tool call's output the agent gets, its start and end.
const TOOL_KEPT: usize = 6000;
/// A tool call's result when the user skips its command.
const TOOL_DECLINED: &str = "The user declined to run this command, so it wasn't run. Don't run it again; go on \
                             without it, or say what you'd need.";
/// Characters of output the agent gets, from its end.
const SHELL_KEPT: usize = 8000;

/// How long a background command (`… &`) is watched before what waits on
/// it goes on: a failure that comes at once, as a missing script, is still
/// caught.
const BACKGROUND_GRACE: Duration = Duration::from_secs(3);
/// How often a running command's output goes to the screen, at most.
const FLUSH: Duration = Duration::from_millis(100);
/// Bytes of a running command's output kept, from its end: a dev server's
/// log doesn't grow without end.
const RAW_KEPT: usize = 64 * 1024;
/// How long a stopped command gets to end on its own before it's killed.
const KILL_AFTER: Duration = Duration::from_secs(3);

/// Run `command` with the platform's shell, its stdin closed, in `dir` or
/// the working directory, on a thread of its own: what it printed, as a
/// terminal would show it, and how it ended. As it runs, its output goes
/// to the screen as `Printed`, and its process id as `Started`, so `/stop`
/// can end it. `background`, it's given `BACKGROUND_GRACE` to fail; still
/// running then, this returns `None` with its output so far, and its end
/// comes to the app as a `Shell` event.
fn run_command(command: &str, dir: Option<&std::path::Path>, background: bool, report: Report) -> (String, Option<Ended>) {
    let (settled_tx, settled) = std::sync::mpsc::channel();
    let (command, dir) = (command.to_string(), dir.map(std::path::Path::to_path_buf));
    std::thread::spawn(move || stream(&command, dir.as_deref(), background, &report, settled_tx));
    settled.recv().unwrap_or_else(|_| ("Failed to run".to_string(), Some(Ended::Killed)))
}

/// `run_command`'s thread. The shell runs in a process group of its own,
/// so a stop ends what it started too. `settled` gets the end, or the
/// output so far once a background command outlives its grace; after
/// that, the end goes to the app as `Shell`.
fn stream(
    command: &str,
    dir: Option<&std::path::Path>,
    background: bool,
    report: &Report,
    settled: std::sync::mpsc::Sender<(String, Option<Ended>)>,
) {
    // Stopped before its turn came, as in a reply's chain.
    if report.stopped.load(Ordering::Relaxed) {
        let _ = settled.send((String::new(), Some(Ended::Stopped)));
        return;
    }
    let mut shell = if cfg!(windows) {
        let mut c = std::process::Command::new("cmd");
        c.arg("/C");
        c
    } else {
        let mut c = std::process::Command::new(std::env::var("SHELL").unwrap_or_else(|_| "sh".to_string()));
        c.arg("-c");
        c
    };
    if let Some(dir) = dir {
        shell.current_dir(dir);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        shell.process_group(0);
    }
    let mut child = match shell
        .arg(traced(command))
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(e) => {
            let _ = settled.send((format!("Failed to run: {}", e), Some(Ended::Exited(127))));
            return;
        }
    };
    let pid = child.id();
    let _ = report.tx.send(AppEvent::Started(report.id, pid));
    // Both pipes are read on threads of their own, into one stream of
    // chunks, as they come.
    let (chunks_tx, chunks) = std::sync::mpsc::channel::<Vec<u8>>();
    let pipes: [Option<Box<dyn std::io::Read + Send>>; 2] =
        [child.stdout.take().map(|p| Box::new(p) as _), child.stderr.take().map(|p| Box::new(p) as _)];
    for mut pipe in pipes.into_iter().flatten() {
        let chunks_tx = chunks_tx.clone();
        std::thread::spawn(move || {
            let mut buf = [0u8; 8192];
            while let Ok(n) = pipe.read(&mut buf) {
                if n == 0 || chunks_tx.send(buf[..n].to_vec()).is_err() {
                    break;
                }
            }
        });
    }
    drop(chunks_tx);
    let text = |raw: &[u8]| strip_ansi(&String::from_utf8_lossy(raw)).trim_end().to_string();
    let started = Instant::now();
    let mut raw: Vec<u8> = Vec::new();
    let (mut dirty, mut flushed) = (false, Instant::now());
    let mut detached = false;
    let mut exited = None;
    let mut stopping: Option<Instant> = None;
    let mut killed = false;
    loop {
        match chunks.recv_timeout(FLUSH) {
            Ok(chunk) => {
                raw.extend(chunk);
                if raw.len() > 2 * RAW_KEPT {
                    let from = raw.len() - RAW_KEPT;
                    let cut = raw[from..].iter().position(|&b| b == b'\n').map_or(from, |i| from + i + 1);
                    raw.drain(..cut);
                }
                dirty = true;
            }
            // The shell gone and nothing more coming: what still holds the
            // pipes is something it left behind, not waited for.
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) if exited.is_some() => break,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }
        if dirty && flushed.elapsed() >= FLUSH {
            let _ = report.tx.send(AppEvent::Printed(report.id, text(&raw)));
            (dirty, flushed) = (false, Instant::now());
        }
        if exited.is_none() {
            exited = child.try_wait().ok().flatten();
        }
        if background && !detached && started.elapsed() >= BACKGROUND_GRACE {
            detached = true;
            let _ = settled.send((text(&raw), None));
        }
        if report.stopped.load(Ordering::Relaxed) {
            match stopping {
                None => {
                    stopping = Some(Instant::now());
                    terminate(pid, false);
                }
                Some(since) if !killed && since.elapsed() >= KILL_AFTER => {
                    killed = true;
                    terminate(pid, true);
                }
                _ => {}
            }
        }
    }
    let status = exited.or_else(|| child.wait().ok());
    let ended = if report.stopped.load(Ordering::Relaxed) {
        Ended::Stopped
    } else {
        match status.and_then(|s| s.code()) {
            Some(code) => Ended::Exited(code),
            None => Ended::Killed,
        }
    };
    let output = text(&raw);
    if detached {
        let _ = report.tx.send(AppEvent::Shell(report.id, output, Some(ended)));
    } else {
        let _ = settled.send((output, Some(ended)));
    }
}

/// End the shell of `pid` and what it started: its process group, with
/// SIGTERM, or SIGKILL when `hard`.
#[cfg(unix)]
fn terminate(pid: u32, hard: bool) {
    let signal = if hard { libc::SIGKILL } else { libc::SIGTERM };
    // SAFETY: a plain system call, on a process group we started.
    unsafe {
        libc::kill(-(pid as i32), signal);
    }
}

/// End the shell of `pid` and its process tree.
#[cfg(windows)]
fn terminate(pid: u32, _hard: bool) {
    let _ = std::process::Command::new("taskkill")
        .args(["/T", "/F", "/PID", &pid.to_string()])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
}

/// Who docent's commits are co-authored by, as git and GitHub read it.
const CO_AUTHOR: &str = "Co-authored-by: Docent <docent@smartloop.ai>";

/// `command`, a reply's, with each `git commit` in it given docent as a
/// co-author, by a trailer git adds to the message's end: run as shown.
/// One that names it already is left as it is.
fn co_authored(command: &str) -> String {
    if command.contains(CO_AUTHOR) {
        return command.to_string();
    }
    let trailer = format!("git commit --trailer \"{CO_AUTHOR}\"");
    let mut out = String::with_capacity(command.len());
    let mut rest = command;
    while let Some(at) = rest.find("git commit") {
        let (before, after) = (&rest[..at], &rest[at + "git commit".len()..]);
        // A command of its own: at a line's start or after `&&`, `;`, `|`,
        // `(` and the like; and `git commit` itself, not `git commit-tree`.
        let line = before.rsplit('\n').next().unwrap_or("").trim_end();
        let starts = line.is_empty() || line.ends_with(['&', ';', '|', '(', '{']);
        let ends = after.chars().next().is_none_or(char::is_whitespace);
        out.push_str(before);
        out.push_str(if starts && ends { &trailer } else { "git commit" });
        rest = after;
    }
    out.push_str(rest);
    out
}

/// `command` without a `&` at its end, and whether it had one: a command
/// to start in the background, as `npm run dev &`. `&&` isn't that.
fn detached(command: &str) -> (String, bool) {
    match command.trim_end().strip_suffix('&') {
        Some(rest) if !rest.ends_with('&') => (rest.trim_end().to_string(), true),
        _ => (command.to_string(), false),
    }
}

/// A line of output that is the shell saying which command it runs next:
/// the trace `set -x` prints ahead of each, with `TRACE` as its prompt.
const TRACE: &str = "\u{1f}$ ";

/// `command` as the shell runs it: for the POSIX shells, with their
/// stderr joined to stdout, so the lines keep the order a terminal shows,
/// and each command traced on a `TRACE` line ahead of what it prints, so
/// a script of several shows each one over its own output. The user's
/// shell can be another, as fish, and runs the command as given.
fn traced(command: &str) -> String {
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "sh".to_string());
    let name = std::path::Path::new(&shell).file_name().and_then(|n| n.to_str()).unwrap_or("sh");
    if cfg!(unix) && matches!(name, "sh" | "bash" | "zsh" | "dash" | "ksh" | "mksh") {
        format!("exec 2>&1; PS4='{}'; set -x\n{}", TRACE, command)
    } else {
        command.to_string()
    }
}

/// The command a line of output traces, if it's one: a `TRACE` line, led
/// by one more marker per nesting, as bash marks a `$(...)`.
fn trace_of(line: &str) -> Option<&str> {
    let (marker, prompt) = TRACE.split_at(1);
    let rest = line.trim_start_matches(marker);
    (rest.len() < line.len()).then_some(rest)?.strip_prefix(prompt)
}

/// `text` as a terminal would have shown it: without its escape sequences
/// (colours, cursor moves, titles), and a line redrawn with a carriage
/// return keeping only its last version.
fn strip_ansi(text: &str) -> String {
    let mut out = String::new();
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\x1b' => match chars.next() {
                // CSI: parameters, then one final byte.
                Some('[') => while chars.next().is_some_and(|c| !('\x40'..='\x7e').contains(&c)) {},
                // OSC: up to BEL or ESC \.
                Some(']') => {
                    while let Some(c) = chars.next() {
                        if c == '\x07' || (c == '\x1b' && chars.next() == Some('\\')) {
                            break;
                        }
                    }
                }
                _ => {}
            },
            '\r' if chars.peek() != Some(&'\n') => {
                let start = out.rfind('\n').map_or(0, |i| i + 1);
                out.truncate(start);
            }
            '\r' => {}
            _ => out.push(c),
        }
    }
    out
}

/// The first `n` characters of `text`, with a note when that's not all.
fn head(text: &str, n: usize) -> String {
    match text.char_indices().nth(n) {
        Some((i, _)) => format!("{}\n(more output cut)", &text[..i]),
        None => text.to_string(),
    }
}

/// `line` with its tabs as spaces, to every `stop` columns.
fn expand_tabs(line: &str, stop: usize) -> String {
    let mut out = String::new();
    let mut column = 0;
    for c in line.chars() {
        if c == '\t' {
            let n = stop - column % stop;
            out.extend(std::iter::repeat_n(' ', n));
            column += n;
        } else {
            out.push(c);
            column += unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
        }
    }
    out
}

/// What the agent is told of a shell run, ahead of the next prompt: how
/// it ended, or that it goes on in the background; `since`, the end of
/// one it was told goes on.
fn shell_context(command: &str, output: &str, ended: Option<Ended>, since: bool) -> String {
    let start = output.char_indices().rev().nth(SHELL_KEPT - 1).map_or(0, |(i, _)| i);
    let cut = if start > 0 { "(earlier output cut)\n" } else { "" };
    let what = match ended {
        None => format!("I started `{}` in the shell; it's still running in the background, so far it printed", command),
        Some(ended) if since => format!("The background `{}` I started in the shell has since {}", command, ended.describe()),
        Some(ended) => format!("I ran `{}` in the shell; it {}", command, ended.describe()),
    };
    let shown: Vec<String> =
        output[start..].lines().map(|l| trace_of(l).map_or_else(|| l.to_string(), |c| format!("$ {}", c))).collect();
    format!("{}:\n```\n{}{}\n```", what, cut, shown.join("\n"))
}

/// The entry for a shell run, with the last `shown` lines it printed. Its
/// command line is clicked to expand it under the prompt, and while it
/// runs, the `✕ stop` closing it is clicked to stop it, as `/stop` does.
fn shell_lines(shell: &Shell, spinner: &str, shown: usize, lines: &mut Vec<Line<'static>>) {
    let show = |style: Style| crate::links::action(style, &format!("show/{}", shell.id));
    let stop = crate::links::action(Style::new().fg(Color::Yellow).bold(), &format!("stop/{}", shell.id));
    let mark = match shell.ended {
        None => Span::styled(format!("{} ", spinner), show(Style::new().fg(Color::Cyan))),
        Some(Ended::Exited(0)) => Span::styled("! ", show(Style::new().fg(Color::Cyan).bold())),
        Some(Ended::Stopped) => Span::styled("! ", show(Style::new().fg(Color::Yellow).bold())),
        Some(_) => Span::styled("! ", show(Style::new().fg(Color::Red).bold())),
    };
    // The mark leads the script's first line. The shell traces each
    // command as it runs it, and each shows on a `$` line over what it
    // printed, behind a bar: the first trace is the first line again, so
    // it's left out. Until the shell traces, a script of several commands
    // lists the rest after the first, `$` ahead. How it ended, or that it
    // runs on, closes the entry.
    let mut command = shell.command.lines();
    let first = command.next().unwrap_or_default();
    lines.push(Line::from(vec![mark, Span::styled(first.to_string(), show(key_style()))]));
    let all: Vec<&str> = shell.output.lines().collect();
    if !all.iter().any(|l| trace_of(l).is_some()) {
        for more in command {
            lines.push(Line::from(vec![Span::styled("$ ", dim()), Span::styled(more.to_string(), key_style())]));
        }
    }
    let skipped = all.len().saturating_sub(shown);
    if skipped > 0 {
        lines.push(Line::from(vec![Span::styled("  │ ", dim()), Span::styled(format!("… {} earlier lines", skipped), dim())]));
    }
    let mut traces = 0;
    for (i, line) in all.iter().enumerate() {
        let Some(traced) = trace_of(line) else {
            if i >= skipped {
                lines.push(Line::from(vec![Span::styled("  │ ", dim()), Span::styled(expand_tabs(line, 8), dim())]));
            }
            continue;
        };
        traces += 1;
        if i >= skipped && (traces > 1 || traced != first.trim()) {
            lines.push(Line::from(vec![Span::styled("$ ", dim()), Span::styled(traced.to_string(), key_style())]));
        }
    }
    let end = match shell.ended {
        None => {
            let what = if shell.background { "  … running in the background" } else { "  … running" };
            vec![Span::styled(what, dim()), Span::styled("  ✕ stop", stop)]
        }
        Some(Ended::Exited(0)) => vec![Span::styled("  ✓ done", Style::new().fg(Color::Green))],
        Some(Ended::Exited(c)) => vec![Span::styled(format!("  ✗ exited with {}", c), Style::new().fg(Color::Red))],
        Some(Ended::Killed) => vec![Span::styled("  ✗ killed by a signal", Style::new().fg(Color::Red))],
        Some(Ended::Stopped) => vec![Span::styled("  ■ stopped", Style::new().fg(Color::Yellow))],
    };
    lines.push(Line::from(end));
}

/// The reply's commands, to allow or skip: each script's lines. The keys
/// that answer show in the footer, which keeps them while they apply.
fn ask_lines(commands: &[String], lines: &mut Vec<Line<'static>>) {
    lines.push(Line::from(vec![
        Span::styled("? ", Style::new().fg(Color::Yellow).bold()),
        Span::styled("Run these in the folder?", Style::new().bold()),
    ]));
    for command in commands {
        for line in command.lines() {
            lines.push(Line::from(vec![Span::styled("  $ ", dim()), Span::styled(line.to_string(), key_style())]));
        }
        let holes = files::placeholders(command);
        if !holes.is_empty() {
            lines.push(Line::from(vec![
                Span::styled("    ■ ", Style::new().fg(Color::Yellow)),
                Span::styled(format!("won't run as it is: fill in {}", holes.join(", ")), Style::new().fg(Color::Yellow)),
            ]));
        }
    }
}

fn error_line(text: &str) -> Line<'static> {
    Line::from(vec![
        Span::styled("■ ", Style::new().fg(Color::Red)),
        Span::styled("Error: ", Style::new().fg(Color::Red).bold()),
        Span::raw(text.to_string()),
    ])
}

/// Read this month's searches and credits from the platform, for the
/// footer and `/usage`.
fn send_usage(client: &Client, tx: &UnboundedSender<AppEvent>) {
    let _ = tx.send(AppEvent::Usage(crate::usage::fetch(client)));
    let _ = tx.send(AppEvent::Credits(crate::usage::fetch_credits(client)));
}

/// Re-read the project's models: signing in or out changes which ones are
/// accessible.
fn refresh_models(client: &Client, tx: &UnboundedSender<AppEvent>, project: Option<(String, String)>) {
    if let Some((id, _)) = project {
        let _ = tx.send(AppEvent::Models(crate::model_rows(client, &id)));
    }
}

/// The conversation pane as last drawn.
#[derive(Default)]
struct Shown {
    area: Rect,
    /// Each cell's text, row by row.
    rows: Vec<Vec<String>>,
    /// Each cell's link, if it's in one.
    links: Vec<Vec<Option<u32>>>,
    /// The footer under the prompt, and its cells' actions.
    footer: Rect,
    footer_links: Vec<Vec<Option<u32>>>,
}

impl Shown {
    /// The URL of the link at screen position `at`, if one's there, in the
    /// conversation or in the footer.
    fn link_at(&self, at: (u16, u16)) -> Option<String> {
        let id = link_in(self.area, &self.links, at).or_else(|| link_in(self.footer, &self.footer_links, at))?;
        crate::links::url(id)
    }
}

/// The link number at screen position `at` in `area`, whose cells' links
/// are `links`.
fn link_in(area: Rect, links: &[Vec<Option<u32>>], (x, y): (u16, u16)) -> Option<u32> {
    let (x, y) = (x.checked_sub(area.x)?, y.checked_sub(area.y)?);
    *links.get(y as usize)?.get(x as usize)?
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
        command("/models", "pick a model; the one for your system is marked"),
        command("/status", "account, model, agent and versions"),
    ]
}

/// Said on start in the home folder: a project wants a folder of its own,
/// where its files are written, its commands run, and it can sync.
fn home_tip() -> Option<String> {
    let here = std::env::current_dir().ok()?;
    docent::refusal(&here)?;
    let start = if cfg!(windows) { "mkdir my-app; cd my-app; docent" } else { "mkdir my-app && cd my-app && docent" };
    Some(format!(
        "You're in your home folder. Start docent in a folder for your project so it can write files and run commands there: {}",
        start
    ))
}

/// A prompt waiting for the turn before it, with the files its chips name.
struct Queued {
    input: String,
    attachments: Vec<Attachment>,
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

/// An exported project, for `/project import`.
fn is_archive(path: &std::path::Path) -> bool {
    path.extension().and_then(|e| e.to_str()).is_some_and(|e| e.eq_ignore_ascii_case("zip"))
}

/// The documents the agent reads as text; it has a parser for these and
/// stores anything else as bytes the model never sees.
fn is_document(path: &std::path::Path) -> bool {
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or_default().to_ascii_lowercase();
    matches!(ext.as_str(), "pdf" | "docx" | "pptx" | "xlsx" | "csv" | "txt" | "html" | "htm" | "json")
}

/// The slash commands, for hints as they're typed.
/// `/exit` and `/q` also quit, but aren't hinted.
const COMMANDS: [(&str, &str); 14] = [
    ("/help", "shortcuts and commands"),
    ("/login", "sign in in the browser (--token to paste one)"),
    ("/logout", "log out"),
    ("/models", "enable, disable and download models"),
    ("/mcp", "MCP servers; /mcp add <url> to connect one"),
    ("/project", "switch, create, import and delete projects"),
    ("/status", "account, model, agent and versions"),
    ("/sync", "sync this folder's documents to the project"),
    ("/tasks", "the commands still running: pick one to see its output or stop it"),
    ("/stop", "stop the shell commands still running; /stop <word> those with word in them"),
    ("/usage", "credits used this month"),
    ("/upgrade", "Pro or Plus plan: more credits a month"),
    ("/clear", "clear the chat and start a new session"),
    ("/quit", "quit"),
];

/// Whether `command` is one of the CLI's own, aliases included.
fn is_command(command: &str) -> bool {
    let command = command.to_lowercase();
    COMMANDS.iter().any(|(name, _)| *name == command) || ["/exit", "/q", "/projects", "/?"].contains(&command.as_str())
}

/// A skill name as the agent compares it: lowercase, without spaces,
/// dashes or underscores.
fn skill_key(name: &str) -> String {
    name.chars().filter(|c| !c.is_whitespace() && *c != '-' && *c != '_').flat_map(char::to_lowercase).collect()
}

/// The welcome card's memory bar, in cells.
const MEMORY_BAR: usize = 10;

/// Memory use as a meter, `MEMORY_BAR` cells wide and drawn as the setup
/// steps' bars are: each filled cell
/// colored for the pressure it stands for, green to 70%, yellow to 90%, red
/// past it, as the search budget warns.
fn memory_bar(percent: u8) -> Vec<Span<'static>> {
    let filled = ((percent.min(100) as usize * MEMORY_BAR) + 50) / 100;
    (0..MEMORY_BAR)
        .map(|cell| {
            if cell >= filled {
                return Span::styled("▱", dim());
            }
            let level = (cell + 1) * 100 / MEMORY_BAR;
            let color = match level {
                91.. => Color::Red,
                71.. => Color::Yellow,
                _ => Color::Green,
            };
            Span::styled("▰", Style::new().fg(color))
        })
        .collect()
}

/// MCP servers ready, and still connecting.
fn connection_counts(servers: &[mcp::Server]) -> (usize, usize) {
    let connecting = servers.iter().filter(|s| s.connecting()).count();
    let ready = servers.iter().filter(|s| s.state == "ready").count();
    (ready, connecting)
}

/// At most this many rows of command hints show at once; the rest scroll.
const HINT_ROWS: usize = 12;
/// The hints' name column fits the longest name, within these bounds.
const HINT_NAME_MIN: usize = 12;
const HINT_NAME_MAX: usize = 28;

/// Every key and command, for the help panel.
fn help_lines() -> Vec<Line<'static>> {
    let rows = [
        ("[?] /help", "these shortcuts"),
        ("[enter]", "send the prompt"),
        ("[shift+enter] [alt+enter]", "new line (also [ctrl+j])"),
        ("[enter] while it replies", "queue the prompt: it goes once the reply and what follows are done"),
        ("[esc]", "interrupt the reply; the next queued prompt goes at once"),
        ("[↑] with prompts queued", "take back the last queued prompt to edit it"),
        ("[esc] [esc]", "clear the prompt"),
        ("[enter] [tab] [esc]", "a reply's commands, asked about: run, always this session, skip"),
        ("[tab]", "on an empty prompt: auto-approve on or off, so a reply's commands run without asking"),
        ("[↑] [↓]", "earlier prompts, past the prompt's first or last line"),
        ("wheel [pgup] [pgdn]", "scroll the conversation"),
        ("drag", "select and copy to the clipboard"),
        ("drop a file", "attach an image or document as [Image 1] or [Doc 1]"),
        ("[ctrl+o] /models", "models: enable, disable, download"),
        ("[ctrl+s]", "turn web search on or off for the project"),
        ("/mcp", "MCP servers: list, remove"),
        ("/mcp add <url>", "connect an MCP server by its URL"),
        ("[ctrl+p] /project", "projects: switch (in a new session), create, delete"),
        ("/project <name>", "switch to a project by its name or ID"),
        ("/project new <name>", "create a project and switch to it"),
        ("/project import <zip>", "import an exported project"),
        ("/sync", "keep this folder's documents in the project"),
        ("/sync status", "what's synced, and what's waiting"),
        ("/sync --move", "sync this folder instead of the project's other one"),
        ("/sync stop", "stop syncing the project's folder"),
        ("/models <name> caps <list|auto>", "set a model's capabilities here, over the catalog's"),
        ("/status", "account, model, agent and versions"),
        ("/login", "sign in in the browser (--token to paste one)"),
        ("/logout", "log out"),
        ("/usage", "credits used this month"),
        ("/upgrade", "Pro or Plus plan: more credits a month"),
        ("/clear", "clear the chat and start a new session"),
        ("/<skill> [prompt]", "run one of the project's skills"),
        ("!<command>", "run a shell command; the next prompt shares its output"),
        ("!<command> &", "run it in the background, as a dev server, listed under the prompt until it ends"),
        ("/stop [word]", "stop the shell commands still running, or those with word in them"),
        ("/tasks [ctrl+b]", "the commands still running: [↑] [↓] select, [enter] all it printed, [x] stop"),
        ("click a command", "running under the prompt, or in its entry: all it printed; its ✕ stop ends it"),
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

/// `text` in lines at most `width` wide, broken between words; a word
/// longer than a line is broken inside it.
fn wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut lines = vec![String::new()];
    for word in text.split_whitespace() {
        let mut word: Vec<char> = word.chars().collect();
        let line = lines.last_mut().expect("a line");
        let used = line.chars().count();
        if used > 0 && used + 1 + word.len() <= width {
            line.push(' ');
            line.extend(word);
            continue;
        }
        if used > 0 {
            lines.push(String::new());
        }
        while word.len() > width {
            let rest = word.split_off(width);
            *lines.last_mut().expect("a line") = word.into_iter().collect();
            lines.push(String::new());
            word = rest;
        }
        lines.last_mut().expect("a line").extend(word);
    }
    lines
}

/// `spans` cut to `width` cells, the last one that fits ending in `…`.
fn fit(spans: &[Span<'static>], width: usize) -> Vec<Span<'static>> {
    let mut left = width;
    let mut kept = Vec::new();
    for span in spans {
        let n = span.content.chars().count();
        if n <= left {
            left -= n;
            kept.push(span.clone());
        } else {
            if left > 0 {
                kept.push(Span::styled(truncate(&span.content, left), span.style));
            }
            break;
        }
    }
    kept
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

    /// A run's report with nobody listening.
    fn report() -> Report {
        let (tx, _) = unbounded_channel();
        Report { id: 1, tx, stopped: Arc::new(AtomicBool::new(false)) }
    }

    #[test]
    fn indexing_is_not_shown_during_a_turn() {
        let mut app = app();
        app.phase = Phase::Ready;
        let agent = crate::AgentState { state: "indexing".into(), memory: None, documents: 1, pending: 0 };
        app.apply(AppEvent::Agent(app.project.as_ref().map(|p| p.0.clone()), Some(agent)));
        app.entries.push(Entry::Reply(Reply::default()));
        let runtime = tokio::runtime::Runtime::new().unwrap();
        app.turn = Some(Turn {
            started: Instant::now(),
            task: runtime.spawn(async {}),
            entry: app.entries.len() - 1,
            tokens: 0,
            activity: None,
            files_dir: None,
            whole: Vec::new(),
            calls: Vec::new(),
        });
        app.chat_event(ChatEvent::Status { step: "index".into(), message: "Reading Testamento ejemplo.pdf".into() });

        // The turn's row alone, with the agent indexing too: no file, no
        // "Indexing".
        let shown = screen(&app, 80, 24);
        assert!(shown.contains("Thinking…"), "{}", shown);
        assert!(!shown.contains("Indexing"), "{}", shown);
        assert!(!shown.contains("Reading Testamento ejemplo.pdf"), "{}", shown);

        // Past indexing, the turn's row follows the agent.
        app.chat_event(ChatEvent::Status { step: "explore".into(), message: "Searching documents".into() });
        let shown = screen(&app, 80, 24);
        assert!(shown.contains("Searching documents…"), "{}", shown);
        assert!(!shown.contains("Indexing documents…"), "{}", shown);

        // The answer being written says which model writes it, not the
        // agent's "Generating response".
        app.chat_event(ChatEvent::Status { step: "model".into(), message: "Selected minimax-m3 for the response".into() });
        app.chat_event(ChatEvent::Status { step: "streaming".into(), message: "Generating response...".into() });
        let shown = screen(&app, 80, 24);
        assert!(shown.contains("Generating with minimax-m3…"), "{}", shown);
        assert!(!shown.contains("Generating response"), "{}", shown);

        // Nor once the turn is over.
        app.turn = None;
        assert!(!screen(&app, 80, 24).contains("Indexing"));
    }

    #[test]
    fn hints_at_documents_only_a_sync_makes_searchable() {
        assert_eq!(unsynced_hint(&[], "Docs"), None);
        assert_eq!(
            unsynced_hint(&["a.pdf".into()], "Docs").unwrap(),
            "1 document here isn't searchable yet (a.pdf); /sync adds them to project Docs"
        );
        let found: Vec<String> = ["a.pdf", "b.docx", "c.pptx", "d.xlsx"].map(String::from).into();
        assert_eq!(
            unsynced_hint(&found, "Docs").unwrap(),
            "4 documents here aren't searchable yet (a.pdf, b.docx, and 2 more); /sync adds them to project Docs"
        );
    }

    #[test]
    fn indexing_never_takes_the_status_line() {
        let mut app = app();
        app.phase = Phase::Ready;
        app.apply(AppEvent::Syncing(3));
        assert!(!screen(&app, 80, 24).contains("Indexing"));
        app.apply(AppEvent::Syncing(0));
        let agent = crate::AgentState { state: "indexing".into(), memory: None, documents: 5, pending: 3 };
        app.apply(AppEvent::Agent(app.project.as_ref().map(|p| p.0.clone()), Some(agent)));
        assert!(!screen(&app, 80, 24).contains("Indexing"));
    }

    #[test]
    fn sync_links_the_folder_to_the_current_project_only() {
        let mut app = app();
        app.phase = Phase::Ready;
        let last = |app: &App| match app.entries.last() {
            Some(Entry::Error(e) | Entry::Notice(e)) => e.clone(),
            _ => String::new(),
        };
        // Nothing to sync to, so nothing is made.
        app.input = "/sync".into();
        app.submit();
        assert!(last(&app).contains("No project to sync to"), "{}", last(&app));
        app.input = "/sync now".into();
        app.submit();
        assert!(last(&app).contains("Unknown /sync now"));

        app.project = Some(("p1".into(), "notes".into()));
        app.apply(AppEvent::SyncLink(sync_root().unwrap(), ("p1".into(), "notes".into())));
        assert!(last(&app).contains("to project notes"));
        app.input = "/sync".into();
        app.submit();
        assert!(last(&app).contains("up to date with project notes"));
        // Its thread ending lets `/sync` start it again.
        let stop = app.sync.as_ref().unwrap().stop.clone();
        app.apply(AppEvent::SyncEnded(stop));
        assert!(app.sync.is_none());
    }

    #[test]
    fn a_project_syncs_from_its_own_folder_wherever_the_chat_is() {
        let elsewhere = tempfile::tempdir().unwrap();
        let elsewhere = std::fs::canonicalize(elsewhere.path()).unwrap();
        docent::link(&elsewhere, "p9").unwrap();
        let mut app = app();
        app.phase = Phase::Ready;
        app.project = Some(("p1".into(), "Default".into()));

        // Switched to, the project's folder syncs, and says where it is.
        app.switch_project(("p9".into(), "Contracts".into()));
        assert_eq!(app.sync.as_ref().map(|s| s.root.clone()), Some(elsewhere.clone()));
        let text = format!("Syncing {} to project Contracts", elsewhere.display());
        assert!(app.entries.iter().any(|e| matches!(e, Entry::Notice(n) if *n == text)));

        // Here isn't linked to it without --move.
        app.input = "/sync".into();
        app.submit();
        let Some(Entry::Error(e)) = app.entries.last() else { panic!("no error") };
        assert!(e.starts_with(&format!("Project Contracts syncs {}; /sync --move syncs", elsewhere.display())), "{}", e);

        // Switched away, the stopped sync and what the agent said of the
        // last project's documents don't read as this one indexing.
        let stop = app.sync.as_ref().unwrap().stop.clone();
        app.apply(AppEvent::Agent(app.project.as_ref().map(|p| p.0.clone()), Some(crate::AgentState { state: "indexing".into(), memory: None, documents: 0, pending: 2 })));
        app.switch_project(("p1".into(), "Default".into()));
        app.apply(AppEvent::SyncEnded(stop));
        app.apply(AppEvent::Agent(Some("p9".into()), Some(crate::AgentState { state: "indexing".into(), memory: None, documents: 0, pending: 2 })));
        assert!(!screen(&app, 80, 24).contains("Indexing"));
        app.switch_project(("p9".into(), "Contracts".into()));

        // Paused, switching back to it leaves it alone.
        docent::pause(&elsewhere).unwrap();
        app.switch_project(("p1".into(), "Default".into()));
        app.switch_project(("p9".into(), "Contracts".into()));
        assert!(app.sync.is_none());
        let _ = std::fs::remove_dir_all(&elsewhere);
    }

    #[test]
    fn a_bang_command_runs_and_its_output_rides_with_the_next_prompt() {
        let mut app = app();
        app.phase = Phase::Ready;
        app.input = "!echo hello".into();
        app.submit();
        assert_eq!(app.history, vec!["!echo hello".to_string()]);
        let Some(Entry::Shell(shell)) = app.entries.last() else { panic!("no shell entry") };
        assert_eq!(shell.command, "echo hello");
        let (output, ended) = run_command("echo hello", None, false, report());
        assert_eq!((output.as_str(), ended), (format!("{}echo hello\nhello", TRACE).as_str(), Some(Ended::Exited(0))));
        app.apply(AppEvent::Shell(shell.id, output, ended));
        let text = screen(&app, 80, 24);
        assert!(text.contains("! echo hello") && text.contains("  │ hello") && text.contains("  ✓ done"), "{}", text);
        // The shell's trace of the one command is the entry's first line again.
        assert!(!text.contains("$ echo hello"), "{}", text);
        assert_eq!(app.shell_context, vec!["I ran `echo hello` in the shell; it exited with 0:\n```\n$ echo hello\nhello\n```"]);
        // A /clear drops what wasn't sent, and a result arriving after it.
        app.input = "/clear".into();
        app.submit();
        app.apply(AppEvent::Shell(1, "late".into(), Some(Ended::Exited(0))));
        assert!(app.shell_context.is_empty() && app.entries.is_empty());
    }

    #[test]
    fn the_prompt_rules_turn_pink_while_a_bang_command_is_typed() {
        let rule_colors = |app: &App| {
            let mut terminal = Terminal::new(TestBackend::new(40, 12)).unwrap();
            terminal.draw(|f| app.draw(f)).unwrap();
            let buffer = terminal.backend().buffer();
            (0..12).filter(|&y| buffer[(0, y)].symbol() == "─").map(|y| buffer[(0, y)].fg).collect::<Vec<_>>()
        };
        let mut app = app();
        app.phase = Phase::Ready;
        assert!(rule_colors(&app).iter().all(|&c| c != pink()));
        app.input = "!ls".into();
        let colors = rule_colors(&app);
        assert!(colors.len() >= 2 && colors.iter().rev().take(2).all(|&c| c == pink()), "{:?}", colors);
        // As in Claude Code's shell mode: `!` leads the prompt in the `>`'s
        // place, not twice over, and the footer says what mode this is.
        let text = screen(&app, 40, 12);
        assert!(text.contains("! ls") && !text.contains("> !ls") && text.contains("! for shell mode"), "{}", text);
        app.input = "!".into();
        app.cursor = 1;
        assert!(screen(&app, 80, 12).contains("! a command to run in the folder"));
        app.input.clear();
        assert!(!screen(&app, 40, 12).contains("shell mode"));
    }

    #[test]
    fn a_failed_bang_command_shows_its_exit_code_and_keeps_the_tail() {
        let (output, ended) = run_command("echo oops >&2; exit 3", None, false, report());
        assert_eq!((output.as_str(), ended), (format!("{T}echo oops\noops\n{T}exit 3", T = TRACE).as_str(), Some(Ended::Exited(3))));
        let long = "z".repeat(SHELL_KEPT + 10);
        let kept = shell_context("cat big", &long, Some(Ended::Exited(0)), false);
        assert!(kept.contains("(earlier output cut)\n") && kept.matches('z').count() == SHELL_KEPT, "{}", kept.len());
        let mut lines = Vec::new();
        let shell =
            Shell { id: 1, command: "false".into(), output: String::new(), ended: Some(Ended::Exited(1)), background: false, detached: false };
        shell_lines(&shell, "", SHELL_SHOWN, &mut lines);
        assert!(lines.iter().any(|l| l.to_string() == "  ✗ exited with 1"));
    }

    #[test]
    fn a_streaming_reply_is_drawn_whole_on_the_frame_it_is_laid_out_again() {
        // A reply laid out a moment ago keeps its layout until the next
        // tick; once it's laid out again, with more lines, the frame
        // measures it as the new layout, so its tail isn't cut off for a
        // frame and back the next: that flickers.
        let mut app = app();
        app.phase = Phase::Ready;
        app.entries.push(Entry::Reply(Reply { text: "Here:\n```rust\nfn one() {}\n".into(), ..Default::default() }));
        let text = screen(&app, 60, 20);
        assert!(text.contains("fn one() {}"), "{}", text);
        let Some(Entry::Reply(reply)) = app.entries.last_mut() else { panic!("no reply") };
        for i in 2..6 {
            reply.text.push_str(&format!("fn f{}() {{}}\n", i));
        }
        // The last layout is a tick old, so this frame lays it out again.
        reply.rendered_at.set(Some(Instant::now() - 2 * TICK));
        let text = screen(&app, 60, 20);
        assert!(text.contains("fn one() {}") && text.contains("fn f5() {}"), "{}", text);
        let text = screen(&app, 60, 20);
        assert!(text.contains("fn one() {}") && text.contains("fn f5() {}"), "{}", text);
    }

    #[test]
    fn a_script_shows_each_command_over_what_it_printed() {
        let script = "git fetch --prune\ngit branch -a";
        let mut shell = Shell { id: 1, command: script.into(), output: String::new(), ended: None, background: false, detached: false };
        let rows = |shell: &Shell| {
            let mut lines = Vec::new();
            shell_lines(shell, "⠋", SHELL_SHOWN, &mut lines);
            lines.iter().map(|l| l.to_string()).collect::<Vec<_>>()
        };
        // Before the shell traces, the script's lines are listed.
        assert_eq!(rows(&shell), ["⠋ git fetch --prune", "$ git branch -a", "  … running  ✕ stop"]);
        // Traced, each command sits over its own output; the first is the
        // head line, so it isn't repeated.
        shell.output = format!("{T}git fetch --prune\nFrom github.com:x/y\n - [deleted] old\n{T}git branch -a\n* main", T = TRACE);
        shell.ended = Some(Ended::Exited(0));
        assert_eq!(
            rows(&shell),
            ["! git fetch --prune", "  │ From github.com:x/y", "  │  - [deleted] old", "$ git branch -a", "  │ * main", "  ✓ done"]
        );
        // bash marks a nested `$(...)` with one more marker; a `+` or `$`
        // in the output itself is output.
        assert_eq!(trace_of("\u{1f}\u{1f}$ echo b"), Some("echo b"));
        assert_eq!(trace_of("$ not a trace"), None);
        assert_eq!(trace_of("+ not one either"), None);
    }

    #[test]
    fn a_command_streams_its_output_and_a_stop_or_an_ampersand_frees_the_wait() {
        // What it prints shows as it comes, ahead of its end.
        let (tx, mut rx) = unbounded_channel();
        let listened = Report { id: 7, tx, stopped: Arc::new(AtomicBool::new(false)) };
        let (output, ended) = run_command("echo one; sleep 0.4; echo two", None, false, listened);
        let traced = format!("{T}echo one\none\n{T}sleep 0.4\n{T}echo two\ntwo", T = TRACE);
        assert_eq!((output.as_str(), ended), (traced.as_str(), Some(Ended::Exited(0))));
        let mut printed = Vec::new();
        let mut started = false;
        while let Ok(event) = rx.try_recv() {
            match event {
                AppEvent::Started(7, pid) => started = pid > 0,
                AppEvent::Printed(7, text) => printed.push(text),
                _ => panic!("another event"),
            }
        }
        assert!(started);
        assert!(printed.first().is_some_and(|t| t.contains("\none") && !t.contains("two")), "{:?}", printed);
        // Stopped, a run ends as stopped, not failed.
        let stopped = Arc::new(AtomicBool::new(false));
        let flag = stopped.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            flag.store(true, Ordering::Relaxed);
        });
        let (tx, _) = unbounded_channel();
        let since = Instant::now();
        let (_, ended) = run_command("sleep 30", None, false, Report { id: 8, tx, stopped });
        assert_eq!(ended, Some(Ended::Stopped));
        assert!(since.elapsed() < Duration::from_secs(5));
        assert!(!Ended::Stopped.failed() && Ended::Killed.failed() && Ended::Exited(2).failed() && !Ended::Exited(0).failed());
        // `&` at the end marks a command to run in the background.
        assert_eq!(detached("npm run dev &"), ("npm run dev".to_string(), true));
        assert_eq!(detached("a && b"), ("a && b".to_string(), false));
        assert_eq!(detached("npm install\nnpm run dev &\n"), ("npm install\nnpm run dev".to_string(), true));
        // In the background, a quick failure is still caught...
        let (_, ended) = run_command("exit 2", None, true, report());
        assert_eq!(ended, Some(Ended::Exited(2)));
        // ...and one that keeps going is left to it, its end coming as an
        // event when it does.
        let (tx, mut rx) = unbounded_channel();
        let stopped = Arc::new(AtomicBool::new(false));
        let (output, ended) = run_command("echo up; sleep 30", None, true, Report { id: 9, tx, stopped: stopped.clone() });
        let up = format!("{T}echo up\nup\n{T}sleep 30", T = TRACE);
        assert_eq!((output.as_str(), ended), (up.as_str(), None));
        stopped.store(true, Ordering::Relaxed);
        let end = loop {
            match rx.blocking_recv() {
                Some(AppEvent::Shell(9, output, ended)) => break (output, ended),
                Some(_) => {}
                None => panic!("no end"),
            }
        };
        assert_eq!(end, (up, Some(Ended::Stopped)));
    }

    #[test]
    fn scripts_left_unrun_after_one_fails_leave_the_running_list() {
        let mut app = app();
        app.phase = Phase::Ready;
        let (first, _) = app.shell_entry("git pull".into(), false);
        let (second, _) = app.shell_entry("cd app\ngit branch -d old".into(), false);
        let (third, _) = app.shell_entry("git push".into(), false);
        app.apply(AppEvent::Shell(first, "fatal".into(), Some(Ended::Exited(1))));
        // Never started, so nothing would end them: they'd be listed
        // under the prompt forever, and `/stop` would have nothing to end.
        app.apply(AppEvent::Unrun(vec![second, third]));
        assert!(app.jobs.is_empty());
        assert!(!app.entries.iter().any(|e| matches!(e, Entry::Shell(s) if s.id != first)));
        assert!(matches!(app.entries.last(), Some(Entry::Notice(n)) if n == "Not run: cd app; git branch -d old; git push"));
    }

    #[test]
    fn stop_ends_the_commands_still_running_and_a_background_one_is_told_of_twice() {
        let mut app = app();
        app.phase = Phase::Ready;
        app.input = "/stop".into();
        app.submit();
        assert!(matches!(app.entries.last(), Some(Entry::Notice(n)) if n == "Nothing is running"));
        let (id, report) = app.shell_entry("npm run dev".into(), true);
        let (_, other) = app.shell_entry("cargo test".into(), false);
        app.apply(AppEvent::Started(id, 4242));
        assert_eq!(app.jobs.iter().map(|j| j.pid).collect::<Vec<_>>(), [Some(4242), None]);
        let text = screen(&app, 80, 24);
        assert!(text.contains("… running  ✕ stop"), "{}", text);
        // In the background, it's listed under the prompt, not in the chat,
        // until it ends.
        assert!(!text.contains("running in the background"), "{}", text);
        assert!(text.lines().any(|r| r.contains("npm run dev (") && r.contains(", background)")), "{}", text);
        // By a word, only the commands with it; `/stop` alone, all.
        app.jobs[0].pid = None;
        app.input = "/stop npm".into();
        app.submit();
        assert!(report.stopped.load(Ordering::Relaxed) && !other.stopped.load(Ordering::Relaxed));
        assert!(matches!(app.entries.last(), Some(Entry::Notice(n)) if n == "Stopping: npm run dev"));
        app.apply(AppEvent::Shell(id, "up".into(), Some(Ended::Stopped)));
        assert_eq!(app.jobs.len(), 1);
        assert!(screen(&app, 80, 24).contains("  ■ stopped"));
        assert_eq!(
            app.shell_context.last().map(String::as_str),
            Some("I ran `npm run dev` in the shell; it was stopped by me:\n```\nup\n```")
        );
        app.input = "/stop nothing".into();
        app.submit();
        assert!(matches!(app.entries.last(), Some(Entry::Notice(n)) if n.starts_with("Nothing running has `nothing` in it")));
        // A background one told of as running has its end told as that
        // one's; what it prints shows meanwhile.
        let (id, _) = app.shell_entry("npm start".into(), true);
        app.apply(AppEvent::Shell(id, "listening".into(), None));
        assert!(app.shell_context.last().unwrap().starts_with("I started `npm start` in the shell; it's still running"));
        app.apply(AppEvent::Printed(id, "listening\nGET /".into()));
        assert!(!screen(&app, 80, 30).contains("GET /"), "a background run's log stays out of the chat");
        app.overlay = Overlay::Shell(id);
        assert!(screen(&app, 80, 30).contains("│ GET /"), "but expands under the prompt");
        app.overlay = Overlay::None;
        app.apply(AppEvent::Shell(id, "listening\nGET /\ncrash".into(), Some(Ended::Exited(1))));
        assert!(app.shell_context.last().unwrap().starts_with("The background `npm start` I started in the shell has since exited with 1"));
        assert_eq!(app.jobs.len(), 1);
        app.stop_jobs(|_| true);
        assert!(other.stopped.load(Ordering::Relaxed));
    }

    #[test]
    fn a_click_expands_a_shell_run_under_the_prompt_and_its_stop_ends_it() {
        let mut app = app();
        app.phase = Phase::Ready;
        let (id, report) = app.shell_entry("sleep 30".into(), false);
        let (other_id, other) = app.shell_entry("npm run dev".into(), true);
        let mouse = |kind, (column, row): (u16, u16)| MouseEvent { kind, column, row, modifiers: KeyModifiers::NONE };
        let click = |app: &mut App, wanted: &str, nth: usize| {
            let text = screen(app, 80, 24);
            let rows: Vec<&str> = text.lines().collect();
            let y = rows.iter().enumerate().filter(|(_, r)| r.contains(wanted)).nth(nth).map(|(y, _)| y).expect(&text);
            let x = rows[y][..rows[y].find(wanted).unwrap()].chars().count() as u16;
            app.mouse(mouse(MouseEventKind::Down(MouseButton::Left), (x, y as u16)));
            app.mouse(mouse(MouseEventKind::Up(MouseButton::Left), (x, y as u16)));
        };
        app.apply(AppEvent::Printed(id, "tick".into()));
        // Under the prompt, each one still running, with how long it's been.
        let text = screen(&app, 80, 24);
        let listed = |text: &str, wanted: &str| text.lines().find(|r| r.contains(wanted) && r.ends_with(')')).map(str::to_string);
        assert!(listed(&text, "sleep 30 (").is_some(), "{}", text);
        assert!(listed(&text, "npm run dev (").is_some_and(|r| r.contains(", background)")), "{}", text);
        // The output is nothing to click.
        click(&mut app, "│ tick", 0);
        assert!(!report.stopped.load(Ordering::Relaxed) && app.opened.is_none() && matches!(app.overlay, Overlay::None));
        // The entry's command line expands it: all it printed, under the
        // prompt, where its `✕ stop` is too.
        click(&mut app, "sleep 30", 0);
        assert!(matches!(app.overlay, Overlay::Shell(i) if i == id));
        let text = screen(&app, 80, 24);
        assert!(text.lines().any(|r| r.contains("[esc] close") && r.contains("[x] stop")) && text.contains("│ tick"), "{}", text);
        app.overlay = Overlay::None;
        // So does its line under the prompt; the panel's `✕ stop` stops
        // that one, and that one alone.
        click(&mut app, "npm run dev (", 0);
        assert!(matches!(app.overlay, Overlay::Shell(i) if i == other_id));
        click(&mut app, "[x] stop", 0);
        assert!(other.stopped.load(Ordering::Relaxed) && !report.stopped.load(Ordering::Relaxed));
        assert!(app.opened.is_none() && app.copied.is_none());
        app.apply(AppEvent::Shell(other_id, String::new(), Some(Ended::Stopped)));
        assert!(screen(&app, 80, 24).contains("  ■ stopped"));
        // Ended, it's off the list under the prompt, and its command line
        // still expands it; the entry's `✕ stop` stops the one left.
        app.overlay = Overlay::None;
        let text = screen(&app, 80, 24);
        assert!(listed(&text, "npm run dev (").is_none() && listed(&text, "sleep 30 (").is_some(), "{}", text);
        click(&mut app, "npm run dev", 0);
        assert!(matches!(app.overlay, Overlay::Shell(i) if i == other_id));
        app.overlay = Overlay::None;
        click(&mut app, "✕ stop", 0);
        assert!(report.stopped.load(Ordering::Relaxed));
        app.apply(AppEvent::Shell(id, "tick".into(), Some(Ended::Stopped)));
        assert!(app.jobs.is_empty() && !screen(&app, 80, 24).contains("✕ stop"));
    }

    #[test]
    fn ctrl_b_selects_a_running_command_from_the_keyboard_to_expand_or_stop() {
        let mut app = app();
        app.phase = Phase::Ready;
        let press = |app: &mut App, code: KeyCode, modifiers: KeyModifiers| app.key(KeyEvent::new(code, modifiers));
        press(&mut app, KeyCode::Char('b'), KeyModifiers::CONTROL);
        assert!(matches!(app.entries.last(), Some(Entry::Notice(n)) if n == "Nothing is running"));
        let (id, report) = app.shell_entry("sleep 30".into(), false);
        let (other_id, other) = app.shell_entry("npm run dev".into(), true);
        assert!(screen(&app, 80, 24).contains(&format!("[{}] running", tasks_key())));
        // `/tasks` opens the list as the key does, as in Claude Code.
        app.input = "/tasks".into();
        app.submit();
        assert!(matches!(app.overlay, Overlay::Jobs { selected: 0 }));
        app.overlay = Overlay::None;
        press(&mut app, KeyCode::Char('b'), KeyModifiers::CONTROL);
        assert!(matches!(app.overlay, Overlay::Jobs { selected: 0 }));
        let text = screen(&app, 80, 24);
        assert!(text.contains("› ⠋ sleep 30 (") || text.contains("› [-] sleep 30 ("), "{}", text);
        assert!(text.contains("[x] stop"), "{}", text);
        // Down, then enter: the second one, expanded; esc back to the prompt.
        press(&mut app, KeyCode::Down, KeyModifiers::NONE);
        press(&mut app, KeyCode::Down, KeyModifiers::NONE);
        assert!(matches!(app.overlay, Overlay::Jobs { selected: 1 }));
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        assert!(matches!(app.overlay, Overlay::Shell(i) if i == other_id));
        // `x` there stops it, and that one alone.
        press(&mut app, KeyCode::Char('x'), KeyModifiers::NONE);
        assert!(other.stopped.load(Ordering::Relaxed) && !report.stopped.load(Ordering::Relaxed));
        press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
        assert!(matches!(app.overlay, Overlay::None));
        // Back in the list, a run ending keeps the selection on a row, and
        // `x` stops the selected one; the last one gone closes the list.
        press(&mut app, KeyCode::Char('b'), KeyModifiers::CONTROL);
        press(&mut app, KeyCode::Down, KeyModifiers::NONE);
        app.apply(AppEvent::Shell(other_id, String::new(), Some(Ended::Stopped)));
        assert!(matches!(app.overlay, Overlay::Jobs { selected: 0 }));
        press(&mut app, KeyCode::Char('x'), KeyModifiers::NONE);
        assert!(report.stopped.load(Ordering::Relaxed));
        app.apply(AppEvent::Shell(id, String::new(), Some(Ended::Stopped)));
        assert!(matches!(app.overlay, Overlay::None) && app.jobs.is_empty());
        // Typing anything else leaves the list for the prompt.
        let (_, _) = app.shell_entry("cargo test".into(), false);
        press(&mut app, KeyCode::Char('b'), KeyModifiers::CONTROL);
        press(&mut app, KeyCode::Char('h'), KeyModifiers::NONE);
        assert!(matches!(app.overlay, Overlay::None) && app.input == "h");
    }

    #[test]
    fn a_failed_check_is_fixed_until_it_passes_or_a_round_changes_nothing() {
        assert_eq!(after_check(Some(0), "", 0, None), AfterCheck::Passed);
        assert_eq!(after_check(Some(0), "", 7, Some("err")), AfterCheck::Passed);
        assert_eq!(after_check(Some(1), "err A", 0, None), AfterCheck::Fix(1));
        assert_eq!(after_check(None, "err B", 1, Some("err A")), AfterCheck::Fix(2));
        assert_eq!(after_check(Some(101), "err C", 9, Some("err B")), AfterCheck::Fix(10));
        assert_eq!(after_check(Some(1), "err C", 10, Some("err C")), AfterCheck::Stalled);
        assert_eq!(head("abc", 5), "abc");
        assert_eq!(head("abcdef", 3), "abc\n(more output cut)");
    }

    #[test]
    fn a_bash_tool_call_names_its_command_or_says_why_it_cant_run() {
        assert_eq!(bash_command("bash", r#"{"command": " ls -la ", "description": "List"}"#), Ok(("ls -la".into(), Some("List".into()))));
        assert_eq!(bash_command("bash", r#"{"command": "pwd"}"#), Ok(("pwd".into(), None)));
        assert!(bash_command("python", r#"{"code": "1"}"#).unwrap_err().contains("no tool named `python`"));
        assert!(bash_command("bash", "{not json").unwrap_err().contains("aren't valid JSON"));
        assert!(bash_command("bash", r#"{"command": ""}"#).unwrap_err().contains("no `command` given"));
    }

    #[test]
    fn a_tool_calls_output_keeps_its_end_and_its_start() {
        assert_eq!(tool_output("", Some(Ended::Exited(0))), "Exit code: 0\n(no output)");
        assert_eq!(tool_output(&format!("{TRACE}make\nok"), Some(Ended::Exited(2))), "Exit code: 2\n$ make\nok");
        assert!(tool_output("up", None).starts_with("Still running in the background"));
        let long = format!("start{}end", "x".repeat(10_000));
        let kept = tool_output(&long, Some(Ended::Killed));
        assert!(kept.starts_with("Killed by a signal\nstart") && kept.ends_with("end"), "{}", &kept[..40]);
        assert!(kept.contains(&format!("…({} characters cut)…", 10_008 - TOOL_KEPT)));
        assert!(kept.chars().count() < TOOL_KEPT + 100);
        assert_eq!(head_tail("héllo wörld", 4), "hé\n…(7 characters cut)…\nld");
    }

    #[test]
    fn a_replys_tool_calls_run_as_its_commands_and_their_output_goes_back_until_it_answers() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Cargo.toml"), "[package]\n").unwrap();
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let _entered = runtime.enter();
        let mut app = app();
        app.phase = Phase::Ready;
        app.project = Some(("p1".into(), "app".into()));
        let sent = Sent { system: Some("sys".into()), message: "build it".into(), files_dir: Some(dir.path().to_path_buf()), ..Default::default() };
        app.tooling = Some(Tooling { sent, ..Default::default() });
        app.entries.push(Entry::Reply(Reply::default()));
        app.turn = Some(Turn {
            started: Instant::now(),
            task: runtime.spawn(async {}),
            entry: app.entries.len() - 1,
            tokens: 0,
            activity: None,
            files_dir: Some(dir.path().to_path_buf()),
            whole: Vec::new(),
            calls: Vec::new(),
        });
        let call = |id: &str, name: &str, arguments: &str| chat::ToolCall { id: id.into(), name: name.into(), arguments: arguments.into() };
        // A reply ends calling tools: its text, then the turn's end.
        let reply = |app: &mut App, text: &str, calls: Vec<chat::ToolCall>| {
            app.apply(AppEvent::Chat(ChatEvent::Content(text.into())));
            app.apply(AppEvent::Chat(ChatEvent::ToolCalls(calls)));
            app.apply(AppEvent::TurnDone(Ok(TurnStats { tokens: 1, elapsed: Duration::from_secs(1) })));
        };
        let key = |app: &mut App, code: KeyCode| app.key(KeyEvent::new(code, KeyModifiers::NONE));
        let run_of = |app: &App| app.tooling.as_ref().and_then(|t| t.calls.iter().find_map(|c| c.run)).expect("a call runs");
        let last_round = |app: &App| app.tooling.as_ref().unwrap().rounds.last().cloned().unwrap();

        // A `bash` call is asked about as a fenced command is, and shows in
        // the reply as one; another tool's is answered there and then. The
        // fenced block in its text only illustrates, so it isn't run.
        reply(
            &mut app,
            "Building first.\n\n```sh\nrm -rf /\n```\n",
            vec![call("call_1", "bash", r#"{"command":"cargo build","description":"Build it"}"#), call("call_2", "python", "{}")],
        );
        assert!(matches!(app.entries.last(), Some(Entry::Ask(c)) if c == &["cargo build"]));
        let Some(Entry::Reply(shown)) = app.entries.iter().rev().find(|e| matches!(e, Entry::Reply(_))) else { panic!() };
        assert!(shown.text.ends_with("```sh\n# Build it\ncargo build\n```\n"), "{}", shown.text);
        let tooling = app.tooling.as_ref().unwrap();
        assert_eq!(tooling.rounds.len(), 1);
        assert_eq!(tooling.rounds[0]["content"], "Building first.\n\n```sh\nrm -rf /\n```\n");
        assert_eq!(tooling.rounds[0]["tool_calls"][1]["function"]["name"], "python");
        assert!(tooling.calls[1].result.as_deref().is_some_and(|r| r.contains("no tool named `python`")));
        // Run, and its output goes back in a turn of its own, the request
        // carried on: no prompt shows for it, nor does the output wait for
        // the next one.
        key(&mut app, KeyCode::Enter);
        let id = run_of(&app);
        let entries = app.entries.len();
        app.apply(AppEvent::Ran(id, "p1".into(), "Compiling app\nFinished".into(), Some(Ended::Exited(0))));
        assert!(app.turn.is_some(), "the next round was sent");
        assert!(matches!(app.entries.last(), Some(Entry::Reply(_))) && app.entries.len() == entries + 1);
        assert!(app.shell_context.is_empty());
        let tooling = app.tooling.as_ref().unwrap();
        assert_eq!(tooling.round, 1);
        assert_eq!(tooling.rounds[1], serde_json::json!({"role": "tool", "tool_call_id": "call_1", "content": "Exit code: 0\nCompiling app\nFinished"}));
        assert_eq!(tooling.rounds[2]["tool_call_id"], "call_2");

        // Skipped, the agent hears so and goes on without it.
        reply(&mut app, "", vec![call("call_3", "bash", r#"{"command":"cargo publish"}"#)]);
        assert_eq!(app.tooling.as_ref().unwrap().rounds[3]["content"], serde_json::Value::Null);
        key(&mut app, KeyCode::Esc);
        assert!(matches!(app.entries.iter().rev().nth(1), Some(Entry::Notice(n)) if n == "Skipped: cargo publish"));
        assert!(app.turn.is_some());
        assert_eq!(last_round(&app)["content"], TOOL_DECLINED);

        // Auto-approved, a call runs at once. The same command failing
        // twice running stops the rounds; its output rides with the next
        // prompt.
        app.always_run = true;
        reply(&mut app, "", vec![call("call_4", "bash", r#"{"command":"cargo test"}"#)]);
        app.apply(AppEvent::Ran(run_of(&app), "p1".into(), "test failed".into(), Some(Ended::Exited(101))));
        assert_eq!(last_round(&app)["content"], "Exit code: 101\ntest failed");
        assert_eq!(app.tooling.as_ref().unwrap().round, 3);
        reply(&mut app, "", vec![call("call_5", "bash", r#"{"command":"cargo test"}"#)]);
        app.apply(AppEvent::Ran(run_of(&app), "p1".into(), "test failed".into(), Some(Ended::Exited(101))));
        assert!(app.turn.is_none(), "no round after the same failure twice");
        assert!(matches!(app.entries.last(), Some(Entry::Warning(w)) if w.starts_with("`cargo test` failed again")));
        assert_eq!(app.shell_context.len(), 1);
        assert!(app.shell_context[0].starts_with("I ran `cargo test` in the shell; it exited with 101"));
        assert!(app.idle(), "the next prompt goes");

        // A reply with no tool calls is as before: its fenced commands run.
        app.shell_context.clear();
        app.send_turn("go".into(), "go".into(), Vec::new(), false);
        let entry = app.entries.len() - 1;
        let turn = app.turn.as_mut().unwrap();
        (turn.files_dir, turn.entry) = (Some(dir.path().to_path_buf()), entry);
        let sent = Sent { files_dir: Some(dir.path().to_path_buf()), ..Default::default() };
        app.tooling = Some(Tooling { sent, ..Default::default() });
        reply(&mut app, "```sh\ngit status\n```\n", Vec::new());
        assert!(matches!(app.entries.last(), Some(Entry::Shell(s)) if s.command == "git status"));
        assert!(app.tooling.as_ref().unwrap().calls.is_empty());

        // The rounds stop at the cap, as they do after a repeated failure.
        app.tooling.as_mut().unwrap().round = TOOL_ROUNDS;
        app.entries.push(Entry::Reply(Reply::default()));
        app.turn = Some(Turn {
            started: Instant::now(),
            task: runtime.spawn(async {}),
            entry: app.entries.len() - 1,
            tokens: 0,
            activity: None,
            files_dir: Some(dir.path().to_path_buf()),
            whole: Vec::new(),
            calls: Vec::new(),
        });
        reply(&mut app, "", vec![call("call_6", "bash", r#"{"command":"ls"}"#)]);
        app.apply(AppEvent::Ran(run_of(&app), "p1".into(), "src".into(), Some(Ended::Exited(0))));
        assert!(app.turn.is_none());
        assert!(matches!(app.entries.last(), Some(Entry::Warning(w)) if w.starts_with("Stopped after 8 tool rounds")));
        assert_eq!(app.shell_context.len(), 1);
    }

    #[test]
    fn a_replys_shell_commands_run_in_the_folder_once_its_files_pass_the_check() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Cargo.toml"), "[package]\n").unwrap();
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let _entered = runtime.enter();
        let turn = |app: &mut App, text: &str| {
            app.entries.push(Entry::Reply(Reply { text: text.into(), ..Default::default() }));
            app.turn = Some(Turn {
                started: Instant::now(),
                task: runtime.spawn(async {}),
                entry: app.entries.len() - 1,
                tokens: 0,
                activity: None,
                files_dir: Some(dir.path().to_path_buf()),
                whole: Vec::new(),
                calls: Vec::new(),
            });
            app.apply(AppEvent::TurnDone(Ok(TurnStats { tokens: 1, elapsed: Duration::from_secs(1) })));
        };
        let mut app = app();
        app.phase = Phase::Ready;
        app.project = Some(("p1".into(), "app".into()));
        let key = |app: &mut App, code: KeyCode| app.key(KeyEvent::new(code, KeyModifiers::NONE));
        let shells = |app: &App| -> Vec<String> {
            app.entries.iter().filter_map(|e| match e { Entry::Shell(s) => Some(s.command.clone()), _ => None }).collect()
        };
        // No file to write: the commands are asked about straight away; Esc
        // skips them, and says so.
        turn(&mut app, "Committing:\n\n```sh\ngit add -A\ngit commit -m \"wip\"\n```\n\n```sh\ngit push\n```\n");
        assert!(matches!(app.entries.last(), Some(Entry::Ask(c)) if c.len() == 2));
        assert!(app.commands.is_none());
        key(&mut app, KeyCode::Esc);
        assert!(app.ask.is_none());
        assert!(!app.entries.iter().any(|e| matches!(e, Entry::Ask(_))));
        assert!(matches!(app.entries.last(), Some(Entry::Notice(n)) if n == "Skipped: git add -A; git commit -m \"wip\"; git push"));
        assert!(shells(&app).is_empty());
        // Enter runs them, each block its own.
        turn(&mut app, "```sh\ngit add -A\ngit commit -m \"wip\"\n```\n\n```sh\ngit push\n```\n");
        key(&mut app, KeyCode::Enter);
        // A commit is co-authored by docent.
        assert_eq!(shells(&app), [format!("git add -A\ngit commit --trailer \"{CO_AUTHOR}\" -m \"wip\""), "git push".to_string()]);
        // With a file: the check first, the question once it passes; Tab
        // runs them, and the next reply's without asking.
        turn(&mut app, "`src/main.rs`\n```rust\nfn main() {}\n```\n\n```bash\ngit commit -am \"main\"\n```\n");
        let check = app.check.as_ref().expect("a check runs");
        assert_eq!(app.commands.as_ref().map(|(_, c)| c.len()), Some(1));
        assert!(matches!(app.entries.last(), Some(Entry::Shell(s)) if s.command == "cargo check --quiet"));
        let id = check.id;
        app.apply(AppEvent::Check(id, String::new(), Ended::Exited(0)));
        assert!(matches!(app.entries.last(), Some(Entry::Ask(c)) if c == &["git commit -am \"main\""]));
        assert!(app.commands.is_none());
        key(&mut app, KeyCode::Tab);
        assert!(app.always_run);
        assert!(matches!(app.entries.last(), Some(Entry::Shell(s)) if s.command == format!("git commit --trailer \"{CO_AUTHOR}\" -am \"main\"") && s.ended.is_none()));
        turn(&mut app, "```sh\ngit push\n```\n");
        assert!(matches!(app.entries.last(), Some(Entry::Shell(s)) if s.command == "git push"));
        assert!(app.ask.is_none());
        // Tab on an empty prompt turns auto-approve off, and on again.
        key(&mut app, KeyCode::Tab);
        assert!(!app.always_run);
        assert!(matches!(app.entries.last(), Some(Entry::Notice(n)) if n.starts_with("Auto-approve off")));
        key(&mut app, KeyCode::Tab);
        assert!(app.always_run);
        assert!(screen(&app, 80, 24).contains("[tab] auto-approve on"));
        key(&mut app, KeyCode::Tab);
        // A prompt being typed keeps its keys: the question waits, drawn
        // with the commands and the keys that answer it.
        app.always_run = false;
        turn(&mut app, "```sh\ngit push\n```\n");
        app.input = "and".into();
        key(&mut app, KeyCode::Esc);
        assert!(app.ask.is_some());
        app.input.clear();
        let mut terminal = Terminal::new(TestBackend::new(80, 30)).unwrap();
        terminal.draw(|f| app.draw(f)).unwrap();
        let rows: Vec<String> = (0..30)
            .map(|y| (0..80).map(|x| terminal.backend().buffer()[(x, y)].symbol().to_string()).collect::<String>())
            .collect();
        let text = rows.join("\n");
        assert!(rows.iter().any(|r| r.starts_with("? Run these in the folder?")), "{}", text);
        assert!(rows.iter().any(|r| r.starts_with("  $ git push")), "{}", text);
        assert_eq!(rows.iter().filter(|r| r.contains("[enter] run  [tab] always this session  [esc] skip")).count(), 1, "{}", text);
        assert!(rows.last().unwrap().contains("[enter] run"), "{}", text);
        key(&mut app, KeyCode::Enter);
        // A command with a placeholder is left out, and the rest run.
        turn(&mut app, "```sh\ngit remote add origin https://github.com/<your-username>/app.git\n```\n\n```sh\ngit push\n```\n");
        key(&mut app, KeyCode::Enter);
        let last = shells(&app);
        assert_eq!(last.last().map(String::as_str), Some("git push"));
        assert!(!last.iter().any(|c| c.contains("remote add")));
        assert!(app.entries.iter().any(|e| matches!(e, Entry::Warning(w) if w == "Not run, as it has a placeholder to fill in: <your-username>")));
        // A script that fails goes back to the agent to fix, for a few
        // rounds only; one that's done isn't followed up.
        let id = app.shell_runs;
        let ran = |app: &mut App, output: &str, code: i32| {
            app.apply(AppEvent::Ran(id, "p1".into(), output.into(), Some(Ended::Exited(code))));
        };
        ran(&mut app, "fatal: no upstream", 128);
        assert_eq!(app.command_round, 1);
        assert!(app.turn.is_some(), "a fix turn was sent");
        assert!(app.shell_context.is_empty(), "the output went with it");
        assert!(matches!(app.entries.last(), Some(Entry::Reply(_))));
        assert!(app.entries.iter().any(|e| matches!(e, Entry::User(u) if u == "Fix the failed command: git push")));
        app.stop_turn();
        ran(&mut app, "Everything up-to-date", 0);
        assert!(app.turn.is_none(), "one in the middle just rides along");
        assert_eq!(app.shell_context.len(), 1);
        ran(&mut app, "Everything up-to-date", 0);
        assert_eq!(app.command_round, 1);
        assert!(app.turn.is_none(), "done, the agent isn't asked what it did");
        assert_eq!(app.shell_context.len(), 2, "the output rides with the next prompt");
        app.command_round = COMMAND_ROUNDS;
        ran(&mut app, "fatal: no upstream", 128);
        assert!(app.turn.is_none());
        assert!(matches!(app.entries.last(), Some(Entry::Warning(w)) if w.starts_with("Still failing after 3 round(s)")));
        // A check that fails for good drops them, and says so.
        turn(&mut app, "`src/main.rs`\n```rust\nfn main() { 1 }\n```\n\n```sh\ngit push\n```\n");
        let id = app.check.as_ref().unwrap().id;
        app.fix_round = 10;
        app.last_failure = Some("err".into());
        app.apply(AppEvent::Check(id, "err".into(), Ended::Exited(1)));
        assert!(app.commands.is_none());
        assert!(matches!(app.entries.last(), Some(Entry::Warning(w)) if w == "Not run, as the check failed: git push"), "{:?}", app.entries.len());
    }

    #[test]
    fn written_files_are_checked_and_a_passing_check_is_told() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Cargo.toml"), "[package]\n").unwrap();
        let mut app = app();
        app.phase = Phase::Ready;
        app.project = Some(("p1".into(), "app".into()));
        app.run_check(dir.path(), &["src/main.rs".to_string()]);
        let check = app.check.as_ref().expect("a check runs");
        assert_eq!(check.command, "cargo check --quiet");
        assert!(matches!(app.entries.last(), Some(Entry::Shell(s)) if s.command == "cargo check --quiet" && s.ended.is_none()));
        let id = check.id;
        app.apply(AppEvent::Check(id, String::new(), Ended::Exited(0)));
        assert!(app.check.is_none());
        assert!(matches!(app.entries.last(), Some(Entry::Notice(n)) if n == "cargo check --quiet passed"), "{:?}", app.entries.len());
        // A stale result, from a check since replaced, is dropped.
        app.apply(AppEvent::Check(id, "late".into(), Ended::Exited(1)));
        assert!(matches!(app.entries.last(), Some(Entry::Notice(_))));
    }

    #[test]
    fn a_reply_shorter_than_an_unseen_file_is_not_written() {
        let dir = tempfile::tempdir().unwrap();
        let long: String = (1..=100).map(|i| format!("line {}\n", i)).collect();
        std::fs::write(dir.path().join("big.rs"), &long).unwrap();
        let mut app = app();
        app.phase = Phase::Ready;
        let file = |path: &str, content: &str| files::File { path: path.into(), content: content.into() };
        let written = app.reply_files(dir.path(), vec![file("big.rs", "fn short() {}\n"), file("b.rs", "b\n")], &[]);
        assert_eq!(written, ["b.rs"]);
        assert_eq!(std::fs::read_to_string(dir.path().join("big.rs")).unwrap(), long);
        let text = screen(&app, 120, 24);
        assert!(text.contains("Not written, as the reply holds less than the file"), "{}", text);
    }

    #[test]
    fn models_caps_sets_a_models_capabilities_here() {
        assert_eq!(capability_list("Vision, reasoning"), Ok(vec!["vision".to_string(), "reasoning".to_string()]));
        assert_eq!(capability_list("auto"), Ok(vec![]));
        assert!(capability_list(" , ").is_err());
        let mut app = app();
        app.phase = Phase::Ready;
        app.project = Some(("p1".into(), "app".into()));
        let last = |app: &App| match app.entries.last() {
            Some(Entry::Error(e) | Entry::Notice(e)) => e.clone(),
            _ => String::new(),
        };
        app.input = "/models qwen3.5-2b caps coding,general".into();
        app.submit();
        assert_eq!(last(&app), "Setting qwen3.5-2b capabilities: coding, general");
        app.input = "/models qwen3.5-2b".into();
        app.submit();
        assert!(last(&app).starts_with("Try /models <name> caps"), "{}", last(&app));
        let rows: Vec<ModelRow> = vec![ModelRow {
            name: "qwen3.5-2b".into(),
            capabilities: "general, coding".into(),
            overridden: true,
            context: Some(32_768),
            access: "free".into(),
            accessible: true,
            downloaded: true,
            enabled: true,
            cloud: false,
            local: false,
            recommended: false,
        }];
        app.overlay = Overlay::Models { rows: Some(Ok(rows)), selected: 0 };
        let text = screen(&app, 100, 24);
        assert!(text.contains("general, coding · set here"), "{}", text);
    }

    #[test]
    fn a_new_folder_gets_a_project_named_after_it() {
        let taken = vec![("p1".to_string(), "app".to_string()), ("p2".to_string(), "app-2".to_string())];
        // A project so named with no folder is reused.
        assert_eq!(adopted("app", &taken, |_| false), ("app".to_string(), Some("p1".to_string())));
        // One linked elsewhere leaves the name taken: the next free one.
        assert_eq!(adopted("app", &taken, |_| true), ("app-3".to_string(), None));
        assert_eq!(adopted("site", &taken, |_| true), ("site".to_string(), None));
    }

    #[test]
    fn a_replys_edits_change_the_files_and_those_that_dont_apply_are_asked_for_again() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "one\ntwo\nthree\n").unwrap();
        std::fs::write(dir.path().join("b.txt"), "b\n").unwrap();
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let _entered = runtime.enter();
        let turn = |app: &mut App, text: &str| {
            app.entries.push(Entry::Reply(Reply { text: text.into(), ..Default::default() }));
            app.turn = Some(Turn {
                started: Instant::now(),
                task: runtime.spawn(async {}),
                entry: app.entries.len() - 1,
                tokens: 0,
                activity: None,
                files_dir: Some(dir.path().to_path_buf()),
                whole: Vec::new(),
                calls: Vec::new(),
            });
            app.apply(AppEvent::TurnDone(Ok(TurnStats { tokens: 1, elapsed: Duration::from_secs(1) })));
        };
        let mut app = app();
        app.phase = Phase::Ready;
        app.project = Some(("p1".into(), "app".into()));
        app.always_run = true;
        let reply = "```edit a.txt\n<<<<<<< SEARCH\ntwo\n=======\n2\n>>>>>>> REPLACE\n```\n\n\
                     ```edit b.txt\n<<<<<<< SEARCH\nzzz\n=======\ny\n>>>>>>> REPLACE\n```\n";
        turn(&mut app, reply);
        // The edit that applies is made; it takes most of the file out of
        // nothing, it's still the whole file.
        assert_eq!(std::fs::read_to_string(dir.path().join("a.txt")).unwrap(), "one\n2\nthree\n");
        assert_eq!(std::fs::read_to_string(dir.path().join("b.txt")).unwrap(), "b\n");
        assert!(app.entries.iter().any(|e| matches!(e, Entry::Warning(w) if w.starts_with("Not changed, b.txt: the lines it replaces aren't in the file"))));
        // The one that doesn't goes back to the agent, with the file.
        assert_eq!(app.edit_round, 1);
        assert!(app.turn.is_some(), "the edit was asked for again");
        assert!(app.entries.iter().any(|e| matches!(e, Entry::User(u) if u == "Redo the edits to b.txt")));
        app.stop_turn();
        // Only so many times.
        app.edit_round = EDIT_ROUNDS;
        turn(&mut app, reply);
        assert!(app.turn.is_none());
        assert!(app.entries.iter().any(|e| matches!(e, Entry::Warning(w) if w.starts_with("The edits still don't apply"))));
    }

    #[test]
    fn a_file_given_with_lines_left_out_is_asked_for_again() {
        let dir = tempfile::tempdir().unwrap();
        let old = "fn a() {}\nfn b() {}\nfn c() {}\n";
        std::fs::write(dir.path().join("a.rs"), old).unwrap();
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let _entered = runtime.enter();
        let mut app = app();
        app.phase = Phase::Ready;
        app.project = Some(("p1".into(), "app".into()));
        app.always_run = true;
        let reply = "```sh\ncat > a.rs <<'EOF'\nfn a() { 1 }\n// ... rest unchanged\nfn c() {}\nEOF\n```\n\n\
                     ```sh\ncat > b.rs <<'EOF'\nfn b() {}\nEOF\n```\n";
        app.entries.push(Entry::Reply(Reply { text: reply.into(), ..Default::default() }));
        app.turn = Some(Turn {
            started: Instant::now(),
            task: runtime.spawn(async {}),
            entry: app.entries.len() - 1,
            tokens: 0,
            activity: None,
            files_dir: Some(dir.path().to_path_buf()),
            whole: Vec::new(),
            calls: Vec::new(),
        });
        app.apply(AppEvent::TurnDone(Ok(TurnStats { tokens: 1, elapsed: Duration::from_secs(1) })));
        // The whole file is written; the one with a comment for lines isn't.
        assert_eq!(std::fs::read_to_string(dir.path().join("a.rs")).unwrap(), old);
        assert_eq!(std::fs::read_to_string(dir.path().join("b.rs")).unwrap(), "fn b() {}\n");
        assert!(app.entries.iter().any(|e| matches!(e, Entry::Warning(w) if w.starts_with("Not changed, a.rs: it leaves lines out"))));
        assert_eq!(app.edit_round, 1);
        assert!(app.turn.is_some(), "the file was asked for again");
        app.stop_turn();
    }

    #[test]
    fn an_edit_block_streams_then_shows_as_its_change() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "one\ntwo\nthree\n").unwrap();
        let mut reply = Reply { root: Some(dir.path().to_path_buf()), ..Default::default() };
        let rows = |reply: &Reply| -> Vec<String> { reply.body(60).iter().map(|l| l.to_string().trim_end().to_string()).collect() };
        reply.text = "```edit a.txt\n<<<<<<< SEARCH\ntwo\n=======\n2".into();
        let shown = rows(&reply);
        // Its SEARCH/REPLACE text is held back while it comes.
        assert_eq!(shown, ["● a.txt · editing… 4 lines"]);
        reply.text.push_str("\n>>>>>>> REPLACE\n```\n");
        reply.rendered_at.set(None);
        let shown = rows(&reply);
        assert!(shown.contains(&"← a.txt  +1 −1".to_string()), "{:?}", shown);
        // One that can't be made says why, with its text.
        reply.text.push_str("\n```edit a.txt\n<<<<<<< SEARCH\nnope\n=======\nx\n>>>>>>> REPLACE\n```\n");
        reply.stats = Some(TurnStats::default());
        let shown = rows(&reply);
        assert!(shown.iter().any(|r| r.starts_with("← a.txt · not changed: the lines it replaces aren't in the file")), "{:?}", shown);
    }

    #[test]
    fn an_edit_given_bare_shows_as_its_change_not_as_prose() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "one\ntwo\nthree\n").unwrap();
        let mut reply = Reply { root: Some(dir.path().to_path_buf()), ..Default::default() };
        let rows = |reply: &Reply| -> Vec<String> { reply.body(60).iter().map(|l| l.to_string().trim_end().to_string()).collect() };
        reply.text = "Fix:\n\na.txt\n<<<<<<< SEARCH\ntwo\n=======\n2\n>>>>>>> REPLACE\n\nDone.\n".into();
        reply.stats = Some(TurnStats::default());
        let shown = rows(&reply);
        assert!(shown.contains(&"← a.txt  +1 −1".to_string()), "{:?}", shown);
        assert!(!shown.iter().any(|r| r.contains("SEARCH")), "{:?}", shown);
        assert_eq!(files::extract_in(dir.path(), &reply.text).files[0].content, "one\n2\nthree\n");
    }

    #[test]
    fn a_language_servers_errors_go_to_the_agent_and_none_lead_to_the_build_check() {
        let dir = tempfile::tempdir().unwrap();
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let _entered = runtime.enter();
        let mut app = app();
        app.phase = Phase::Ready;
        app.project = Some(("p1".into(), "app".into()));
        let asked = |app: &mut App, id: usize| {
            let servers = Some((dir.path().to_path_buf(), vec!["main.c".to_string()]));
            app.check = Some(Check { id, project: "p1".into(), command: "the language server".into(), servers });
        };
        // Errors: shown, and sent to be fixed.
        asked(&mut app, 1);
        let problem = lsp::Problem { path: "main.c".into(), line: 2, column: 12, message: "use of undeclared identifier 'x'".into() };
        app.apply(AppEvent::Diagnostics(1, vec!["clangd"], vec![problem]));
        assert!(app.entries.iter().any(|e| matches!(e, Entry::Error(t) if t.starts_with("clangd reports 1 error(s):\nmain.c:2:12: use of undeclared"))));
        assert_eq!(app.fix_round, 1);
        assert!(app.turn.is_some());
        assert!(app.entries.iter().any(|e| matches!(e, Entry::User(u) if u == "Fix the errors from `clangd`")));
        app.stop_turn();
        // A build check's end isn't taken for the servers'.
        asked(&mut app, 2);
        app.apply(AppEvent::Check(2, String::new(), Ended::Exited(0)));
        assert!(app.check.is_some());
        // None: said, and the build is checked.
        app.apply(AppEvent::Diagnostics(2, vec!["clangd"], Vec::new()));
        assert!(app.entries.iter().any(|e| matches!(e, Entry::Notice(n) if n == "clangd: no errors in main.c")));
        assert!(app.check.as_ref().is_none_or(|c| c.servers.is_none()));
    }

    #[test]
    fn a_replys_files_are_written_as_their_blocks_are_done() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "one\ntwo\n").unwrap();
        std::fs::write(dir.path().join("log.txt"), "first\n").unwrap();
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let _entered = runtime.enter();
        let mut app = app();
        app.phase = Phase::Ready;
        app.project = Some(("p1".into(), "app".into()));
        let start = |app: &mut App| {
            app.entries.push(Entry::Reply(Reply { root: Some(dir.path().to_path_buf()), ..Default::default() }));
            app.turn = Some(Turn {
                started: Instant::now(),
                task: runtime.spawn(async {}),
                entry: app.entries.len() - 1,
                tokens: 0,
                activity: None,
                files_dir: Some(dir.path().to_path_buf()),
                whole: Vec::new(),
                calls: Vec::new(),
            });
        };
        let chunk = |app: &mut App, text: &str| app.apply(AppEvent::Chat(ChatEvent::Content(text.into())));
        let read = |name: &str| std::fs::read_to_string(dir.path().join(name)).ok();
        start(&mut app);
        chunk(&mut app, "Here:\n\n```sh\ncat > new.txt <<'EOF'\nhello\n");
        assert_eq!(read("new.txt"), None, "not while it's coming");
        chunk(&mut app, "EOF\ncat >> new.txt <<'EOF'\nmore\nEOF\n");
        // A new file, as soon as its heredoc's done; and added to after.
        assert_eq!(read("new.txt").as_deref(), Some("hello\nmore\n"));
        chunk(&mut app, "```\n\n```edit a.txt\n<<<<<<< SEARCH\ntwo\n=======\n2\n>>>>>>> REPLACE\n```\n");
        // A change to a file that's there is written as soon as it's done
        // too, without asking.
        assert_eq!(read("a.txt").as_deref(), Some("one\n2\n"));
        app.apply(AppEvent::TurnDone(Ok(TurnStats::default())));
        assert!(app.ask.is_none());
        // Nothing made twice, and each change reported once.
        assert_eq!(read("new.txt").as_deref(), Some("hello\nmore\n"));
        assert_eq!(read("a.txt").as_deref(), Some("one\n2\n"));
        let notices: Vec<&String> = app.entries.iter().filter_map(|e| match e { Entry::Notice(n) => Some(n), _ => None }).collect();
        assert!(notices.iter().any(|n| n.as_str() == "  created new.txt (+2 −0)"), "{notices:?}");
        assert!(notices.iter().any(|n| n.as_str() == "  updated a.txt (+1 −1)"), "{notices:?}");
        // And again, made once.
        start(&mut app);
        chunk(&mut app, "```sh\ncat >> log.txt <<'EOF'\nsecond\nEOF\n```\n");
        assert_eq!(read("log.txt").as_deref(), Some("first\nsecond\n"));
        chunk(&mut app, "```edit a.txt\n<<<<<<< SEARCH\n2\n=======\n3\n>>>>>>> REPLACE\n```\n");
        assert_eq!(read("a.txt").as_deref(), Some("one\n3\n"));
        app.apply(AppEvent::TurnDone(Ok(TurnStats::default())));
        assert_eq!(read("log.txt").as_deref(), Some("first\nsecond\n"));
        assert_eq!(read("a.txt").as_deref(), Some("one\n3\n"));
        assert!(!app.entries.iter().any(|e| matches!(e, Entry::Warning(w) if w.starts_with("Not changed"))));
    }

    #[test]
    fn a_turns_working_word_stays_the_same_through_it() {
        let started = Instant::now();
        assert_eq!(working_word(started), working_word(started));
        assert!(WORKING_WORDS.contains(&working_word(started)));
    }

    #[test]
    fn a_replys_commits_are_co_authored_by_docent() {
        let with = |c: &str| format!("git commit --trailer \"{CO_AUTHOR}\"{c}");
        assert_eq!(co_authored("git commit -m \"wip\""), with(" -m \"wip\""));
        assert_eq!(co_authored("git add -A && git commit -am x\ngit push"), format!("git add -A && {}\ngit push", with(" -am x")));
        assert_eq!(co_authored("git add .; git commit"), format!("git add .; {}", with("")));
        assert_eq!(co_authored("git add -A\n  git commit -m x"), format!("git add -A\n  {}", with(" -m x")));
        // Not another command, a word in a message, or one already so.
        assert_eq!(co_authored("git commit-tree abc"), "git commit-tree abc");
        assert_eq!(co_authored("echo \"run git commit\""), "echo \"run git commit\"");
        let done = format!("git commit -m x --trailer \"{CO_AUTHOR}\"");
        assert_eq!(co_authored(&done), done);
    }

    #[test]
    fn a_reply_with_no_files_for_an_empty_folder_asks_for_them_once() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = app();
        app.phase = Phase::Ready;
        let asked = |app: &App| {
            app.entries.iter().filter(|e| matches!(e, Entry::Notice(n) if n.starts_with("No files came with that reply"))).count()
        };
        app.reply_done(dir.path().into(), files::Extracted::default(), Vec::new(), vec!["which cargo".into()]);
        assert_eq!(asked(&app), 1);
        // The reply's commands wait for the files rather than run alone.
        assert!(app.commands.is_none() && app.ask.is_none());
        // Still none: asked once only, and its commands go ahead.
        app.reply_done(dir.path().into(), files::Extracted::default(), Vec::new(), vec!["which cargo".into()]);
        assert_eq!(asked(&app), 1);
        assert!(app.ask.is_some());
    }

    #[test]
    fn rust_source_with_no_cargo_toml_asks_for_it_once() {
        let dir = tempfile::tempdir().unwrap();
        let file = |path: &str, content: &str| files::File { path: path.into(), content: content.into() };
        let mut app = app();
        app.phase = Phase::Ready;
        let asked = |app: &App| {
            app.entries.iter().filter(|e| matches!(e, Entry::Notice(n) if n.starts_with("No Cargo.toml came"))).count()
        };
        let main = files::Extracted { files: vec![file("main.rs", "fn main() {}\n")], ..Default::default() };
        app.reply_done(dir.path().into(), main, Vec::new(), Vec::new());
        assert_eq!(std::fs::read_to_string(dir.path().join("main.rs")).unwrap(), "fn main() {}\n");
        assert_eq!(asked(&app), 1);
        // Asked once: a second reply without it isn't asked about again.
        let again = files::Extracted { files: vec![file("src/main.rs", "fn main() {}\n")], ..Default::default() };
        app.reply_done(dir.path().into(), again, Vec::new(), Vec::new());
        assert_eq!(asked(&app), 1);
    }

    #[test]
    fn a_reply_with_no_files_for_a_folder_with_files_asks_nothing() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("notes.md"), "hi").unwrap();
        let mut app = app();
        app.phase = Phase::Ready;
        app.reply_done(dir.path().into(), files::Extracted::default(), Vec::new(), Vec::new());
        assert!(!app.entries.iter().any(|e| matches!(e, Entry::Notice(n) if n.starts_with("No files came"))));
    }

    #[test]
    fn a_replys_files_are_written_without_asking_and_only_its_commands_are_asked() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("main.rs"), "old\n").unwrap();
        std::fs::write(dir.path().join("Cargo.toml"), "[package]\nname = \"t\"\n").unwrap();
        let file = |path: &str, content: &str| files::File { path: path.into(), content: content.into() };
        let mut app = app();
        app.phase = Phase::Ready;
        app.reply_done(
            dir.path().into(),
            files::Extracted { files: vec![file("main.rs", "new\n"), file("a.rs", "a\n")], ..Default::default() },
            Vec::new(),
            vec!["git add -A".into()],
        );
        assert_eq!(std::fs::read_to_string(dir.path().join("main.rs")).unwrap(), "new\n");
        assert_eq!(std::fs::read_to_string(dir.path().join("a.rs")).unwrap(), "a\n");
        assert!(app.entries.iter().any(|e| matches!(e, Entry::Notice(n) if n == "  updated main.rs (+1 −1)")));
        // Checked first in a code folder, then the commands are asked about.
        if let Some(check) = app.check.as_ref().map(|c| c.id) {
            app.apply(AppEvent::Check(check, String::new(), Ended::Exited(0)));
        }
        assert!(app.ask.is_some());
    }

    #[test]
    fn writing_a_replys_files_reports_each_change() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("main.rs"), "old\n").unwrap();
        std::fs::write(dir.path().join("same.rs"), "same\n").unwrap();
        let file = |path: &str, content: &str| files::File { path: path.into(), content: content.into() };
        let mut app = app();
        app.phase = Phase::Ready;
        let nudge = Arc::new(AtomicBool::new(false));
        app.sync = Some(Sync { project: "p1".into(), root: dir.path().into(), stop: Arc::default(), nudge: nudge.clone() });
        let written = app.reply_files(
            dir.path(),
            vec![file("main.rs", "new\n"), file("same.rs", "same\n"), file("src/a.rs", "a\n")],
            &[],
        );
        // The folder's sync pushes what was written now.
        assert!(nudge.load(Ordering::Relaxed));
        assert_eq!(written, ["src/a.rs", "main.rs"]);
        assert_eq!(std::fs::read_to_string(dir.path().join("main.rs")).unwrap(), "new\n");
        assert_eq!(std::fs::read_to_string(dir.path().join("src/a.rs")).unwrap(), "a\n");
        let text = screen(&app, 120, 24);
        assert!(text.contains("created src/a.rs (+1 −0)"), "{}", text);
        assert!(text.contains("updated main.rs (+1 −1)"), "{}", text);
        // Unchanged, it isn't reported.
        assert!(!text.contains("same.rs"), "{}", text);
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
        let banner = rows.iter().position(|r| r.contains("█▀▀▀▄ ▄▀▀▀▄")).expect(&text);
        assert!(rows[banner].contains("project  Default"), "{}", text);
        assert!(rows[banner + 1].contains("         0 docs · 0 connections"), "{}", text);
        assert!(rows[banner + 2].contains("model    …"), "{}", text);
        assert!(rows[banner + 3].contains(&format!("agent    {}", app.spinner())), "{}", text);
        assert!(rows[banner + 4].contains("account  …"), "{}", text);
        let versions = format!("         CLI {} · agent {}", env!("CARGO_PKG_VERSION"), framework::VERSION);
        assert!(rows[banner + 3].contains("Frontier quality. Local first."), "{}", text);
        assert!(rows[banner + 4].contains("by Smartloop") && rows[banner + 5].contains(&versions), "{}", text);

        // Filled in as the agent and the platform answer.
        let state = |state: &str| crate::AgentState { state: state.into(), memory: Some(31), documents: 5, pending: 0 };
        app.apply(AppEvent::Agent(app.project.as_ref().map(|p| p.0.clone()), Some(state("healthy"))));
        let server = |state: &str| mcp::Server {
            id: state.into(), name: state.into(), location: String::new(), state: state.into(), tools: 0, error: None,
        };
        app.apply(AppEvent::Connections("p1".into(), vec![server("ready"), server("provisioning"), server("failed")]));
        app.apply(AppEvent::Connections("p0".into(), Vec::new()));
        app.apply(AppEvent::Account(Some("mehfuz@smartloop.ai".into())));
        let filled = screen(&app, 80, 30);
        assert!(filled.contains("project  Default"), "{}", filled);
        assert!(filled.contains(&format!("         5 docs · 2 connections {}", app.spinner())), "{}", filled);
        assert!(filled.contains("agent    ▰▰▰▱▱▱▱▱▱▱ 31% memory"), "{}", filled);
        app.apply(AppEvent::Agent(app.project.as_ref().map(|p| p.0.clone()), Some(state("indexing"))));
        assert!(screen(&app, 80, 30).contains(&format!("agent    {} ▰▰▰▱▱▱▱▱▱▱ 31% memory", app.spinner())));
        app.apply(AppEvent::Agent(app.project.as_ref().map(|p| p.0.clone()), Some(crate::AgentState { memory: None, ..state("healthy") })));
        // No figure to draw: the square says it's up.
        assert!(screen(&app, 80, 30).contains("agent    ■ "));
        assert!(filled.contains("account  mehfuz@smartloop.ai"), "{}", filled);
        assert!(!filled.contains("healthy"), "{}", filled);
        app.apply(AppEvent::Account(None));
        assert!(screen(&app, 80, 30).contains("account  not signed in · /login"));
        app.apply(AppEvent::Agent(app.project.as_ref().map(|p| p.0.clone()), None));
        assert!(screen(&app, 80, 30).contains("agent    ■ not responding"));
        // Only as wide as what it holds, not the screen.
        let wide = screen(&app, 120, 30);
        let top = wide.lines().find(|r| r.starts_with('╭')).expect(&wide);
        assert!(top.ends_with('╮') && top.chars().count() < 100, "{}", wide);
        assert!(rows[banner + 7].contains("Here are some commands to get you started"), "{}", text);
        assert!(rows[banner + 8].contains("/login"), "{}", text);
        assert!(!text.contains("search the web"), "{}", text);
        assert!(rows[banner - 2].starts_with('╭') && rows[banner + 11].starts_with('╰'), "{}", text);
        for i in 0..40 {
            app.entries.push(Entry::Notice(format!("line {}", i)));
        }
        let text = screen(&app, 80, 24);
        assert!(!text.contains("█▀▀▀▄ ▄▀▀▀▄"), "{}", text);
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
        app.agent = Some(crate::AgentState { state: "healthy".into(), memory: Some(42), documents: 12, pending: 0 });
        app.connections = Some((2, 0));
        app.account = Some(Some("you@example.com".into()));
        app.entries.push(Entry::User(
            "[Doc 1] Issue a mutual NDA to Northwind Labs from our template: 2-year term, California law".into(),
        ));
        app.entries.push(Entry::Reply(Reply {
            warnings: Vec::new(),
            model: Some("sl-mini".into()),
            text: "Drafted `nda-northwind-labs.docx` from your template:\n\n\
                   | Clause | Terms | Section |\n\
                   |---|---|---|\n\
                   | **Parties** | Smartloop Inc. and Northwind Labs, Inc. | 1 |\n\
                   | **Term** | 2 years from signing; confidentiality survives 3 | 5 |\n\
                   | **Governing law** | California, venue in San Francisco | 9 |\n\n\
                   Signature blocks are left blank for both parties."
                .into(),
            citations: vec!["mutual-nda-template.docx".into()],
            stats: Some(TurnStats { tokens: 96, elapsed: Duration::from_secs(14) }),
            ended: None,
            ..Default::default()
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
            files_dir: None,
            whole: Vec::new(),
            calls: Vec::new(),
        });
        app.input = "".into();

        let (width, height) = (100, 46);
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|f| app.draw(f)).unwrap();
        let buffer = terminal.backend().buffer();
        let color = |c: Color| match c {
            Color::Rgb(r, g, b) => format!("#{:02x}{:02x}{:02x}", r, g, b),
            Color::Indexed(169) => "#e55d9c".to_string(),
            Color::Indexed(147) => "#b1b9f9".to_string(),
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
        assert!(screen(&app, 80, 24).contains("█▀▀▀▄ ▄▀▀▀▄"), "the top shows the welcome card");
        app.scroll(-1000);
        assert_eq!(app.scroll_back, 0);
        assert!(screen(&app, 80, 24).contains("line 59"));
    }

    #[test]
    fn clicking_a_link_opens_it_and_dragging_over_one_copies() {
        let mut app = app();
        app.phase = Phase::Ready;
        app.entries.push(Entry::Reply(Reply {
            text: "Read [the docs](https://smartloop.ai/docs) first.".into(),
            citations: vec!["Guide (https://smartloop.ai/guide)".into()],
            ..Default::default()
        }));
        let text = screen(&app, 80, 24);
        let rows: Vec<&str> = text.lines().collect();
        let find = |wanted: &str| {
            let y = rows.iter().position(|r| r.contains(wanted)).expect(&text);
            let x = rows[y][..rows[y].find(wanted).unwrap()].chars().count();
            (x as u16, y as u16)
        };
        let mouse = |kind, (column, row): (u16, u16)| MouseEvent { kind, column, row, modifiers: KeyModifiers::NONE };
        let click = |app: &mut App, at| {
            app.mouse(mouse(MouseEventKind::Down(MouseButton::Left), at));
            app.mouse(mouse(MouseEventKind::Up(MouseButton::Left), at));
            app.opened.take().map(|(url, _)| url)
        };

        let (x, y) = find("the docs");
        assert_eq!(click(&mut app, (x + 4, y)).as_deref(), Some("https://smartloop.ai/docs"));
        assert_eq!(click(&mut app, find("https://smartloop.ai/docs")).as_deref(), Some("https://smartloop.ai/docs"));
        assert_eq!(click(&mut app, find("https://smartloop.ai/guide")).as_deref(), Some("https://smartloop.ai/guide"));
        assert_eq!(click(&mut app, find("Read")), None, "plain text opens nothing");
        assert_eq!(app.copied, None);

        app.mouse(mouse(MouseEventKind::Down(MouseButton::Left), (x, y)));
        app.mouse(mouse(MouseEventKind::Drag(MouseButton::Left), (x + 7, y)));
        app.mouse(mouse(MouseEventKind::Up(MouseButton::Left), (x + 7, y)));
        assert_eq!(app.opened, None);
        assert_eq!(app.copied.map(|(n, _)| n), Some(8));

        app.copied = None;
        app.mouse(mouse(MouseEventKind::Down(MouseButton::Left), (x, y)));
        app.mouse(mouse(MouseEventKind::Up(MouseButton::Left), (x, y)));
        assert!(screen(&app, 80, 24).contains("Opening https://smartloop.ai/docs"));
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
    fn usage_shows_only_credits_and_how_to_get_more() {
        let mut app = app();
        app.phase = Phase::Ready;
        app.project = Some(("p1".into(), "Default".into()));
        app.overlay = Overlay::Usage { loading: true };
        assert!(screen(&app, 80, 24).contains("asking the agent"));
        app.apply(AppEvent::Usage(Some(SearchUsage {
            used: 12,
            limit: Some(50),
            plan: "free".into(),
            resets_on: Some("2026-11-01".into()),
        })));
        app.apply(AppEvent::Credits(Some(CreditUsage {
            percent_used: Some(24.0),
            used: None,
            limit: None,
            plan: "free".into(),
            resets_at: None,
            requests: 3,
            tokens: 12_000,
        })));
        let text = screen(&app, 100, 24);
        assert!(!text.contains("Web search"), "{}", text);
        assert!(!text.contains("38 left of 50"), "{}", text);
        for expected in ["Credits", "24% used", "/upgrade to Pro or Plus for more credits a month"] {
            assert!(text.contains(expected), "{}\n{}", expected, text);
        }
        // ctrl+s turned search off: say so, and how to turn it back on.
        app.apply(AppEvent::WebSearch { enabled: false, toggled: true });
        let text = screen(&app, 100, 30);
        assert!(text.contains("web search is off for this project · ctrl+s turns it on"), "{}", text);
        // Signed out: nothing to show but how to sign in.
        app.apply(AppEvent::Credits(None));
        assert!(screen(&app, 100, 24).contains("sign in with /login"));
    }

    #[test]
    fn usage_shows_credits_used_of_what_is_allowed() {
        let mut app = app();
        app.phase = Phase::Ready;
        app.project = Some(("p1".into(), "Default".into()));
        app.overlay = Overlay::Usage { loading: false };
        let credits = |used, limit, percent_used, plan: &str| CreditUsage {
            percent_used,
            used,
            limit,
            plan: plan.into(),
            resets_at: None,
            requests: 316,
            tokens: 11_848_606,
        };
        // Metered with the amounts: the share, then what's spent of what's allowed.
        app.apply(AppEvent::Credits(Some(credits(Some(1_700.0), Some(2_000.0), Some(85.0), "paid"))));
        let text = screen(&app, 100, 30);
        for expected in [
            "85% used · running low",
            "1,700 of 2,000 credits used",
            "316 calls · 11,848,606 tokens · Pro",
            "/upgrade to Plus for more credits a month",
        ] {
            assert!(text.contains(expected), "{}\n{}", expected, text);
        }
        // Unmetered: the credits spent, and no bar.
        app.apply(AppEvent::Credits(Some(credits(Some(565.66), None, None, "admin"))));
        let text = screen(&app, 100, 30);
        assert!(text.contains("Credits     ██████████████████████████████  566 credits used · unlimited"), "{}", text);
        assert!(text.contains("unlimited credits for cloud models and web searches"), "{}", text);
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
    fn project_switches_to_a_new_session() {
        let mut app = app();
        app.phase = Phase::Ready;
        app.project = Some(("p1".into(), "Default".into()));
        app.entries.push(Entry::Notice("earlier".into()));
        app.input = "/project".into();
        app.submit();
        let project = |id: &str, name: &str| serde_json::json!({ "id": id, "name": name });
        app.apply(AppEvent::Projects(Ok(vec![project("p1", "Default"), project("p2", "Research")])));
        let text = screen(&app, 80, 24);
        assert!(text.contains("› Default   ✓ current"), "{}", text);
        assert!(text.contains("  Research"), "{}", text);

        app.key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(matches!(app.overlay, Overlay::None));
        assert_eq!(app.project, Some(("p2".into(), "Research".into())));
        assert_ne!(app.session, "cli-test");
        assert!(matches!(&app.entries[..], [Entry::Notice(n)] if n == "Switched to project Research"));
    }

    #[test]
    fn projects_create_delete_and_switch_from_the_panel() {
        let mut app = app();
        app.phase = Phase::Ready;
        app.project = Some(("p2".into(), "Research".into()));
        let project = |id: &str, name: &str, system| {
            serde_json::json!({ "id": id, "name": name, "description": format!("about {}", name), "system": system })
        };
        let list = || vec![project("p1", "general_chat", true), project("p2", "Research", false), project("p3", "Legal", false)];
        app.key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL));
        assert!(matches!(app.overlay, Overlay::Projects { rows: None, .. }));
        app.apply(AppEvent::Projects(Ok(list())));
        let text = screen(&app, 100, 30);
        assert!(text.contains("  general_chat  system      about general_chat"), "{}", text);
        assert!(text.contains("› Research      ✓ current   about Research"), "{}", text);
        assert!(text.contains("  + New project"), "{}", text);
        assert!(text.contains("  + Import project"), "{}", text);
        assert!(text.contains("[enter] switch  [ctrl+n] new  [ctrl+d] delete"), "{}", text);

        // The project in use can't be deleted out from under the chat.
        let ctrl_d = KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL);
        app.key(ctrl_d);
        assert!(screen(&app, 100, 30).contains("Delete Research? [ctrl+d] delete  [esc] cancel"));
        app.key(ctrl_d);
        assert!(matches!(app.entries.last(), Some(Entry::Error(e)) if e.contains("Research is the project in use")));

        // Esc takes the question back before it closes the panel.
        app.key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        app.key(ctrl_d);
        assert!(matches!(app.overlay, Overlay::Projects { deleting: true, .. }));
        app.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(matches!(app.overlay, Overlay::Projects { deleting: false, .. }));

        // A reload after a delete keeps the selection, the new row included.
        app.key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert!(screen(&app, 100, 30).contains("› + New project"));
        app.apply(AppEvent::Projects(Ok(list()[..2].to_vec())));
        assert!(screen(&app, 100, 30).contains("› + New project"));

        // The last row asks for a name in the prompt; ctrl+d does nothing there.
        app.key(ctrl_d);
        assert!(matches!(app.overlay, Overlay::Projects { deleting: false, .. }));
        app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(matches!(app.overlay, Overlay::None));
        assert_eq!((app.input.as_str(), app.cursor), ("/project new ", 13));
        // ctrl+n does the same from any row, even while the list loads.
        app.input.clear();
        app.overlay = Overlay::Projects { rows: None, selected: 0, deleting: false };
        app.key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::CONTROL));
        assert_eq!(app.input, "/project new ");

        // The row under it asks for the archive, which a drop then fills in.
        app.input.clear();
        app.overlay = Overlay::Projects { rows: None, selected: 0, deleting: false };
        app.apply(AppEvent::Projects(Ok(list())));
        for _ in 0..5 {
            app.key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        }
        assert!(screen(&app, 100, 30).contains("› + Import project"));
        app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(matches!(app.overlay, Overlay::None));
        assert_eq!((app.input.as_str(), app.cursor), ("/project import ", 16));
        assert!(screen(&app, 100, 30).contains("Drop an exported project's .zip here, or type its path"));
        app.input.clear();

        // A project just created or imported is switched to, and a reply
        // still running stops: its session is gone.
        let runtime = tokio::runtime::Runtime::new().unwrap();
        app.entries.push(Entry::Reply(Reply::default()));
        app.turn = Some(Turn { started: Instant::now(), task: runtime.spawn(async {}), entry: app.entries.len() - 1, tokens: 0, activity: None, files_dir: None, whole: Vec::new(), calls: Vec::new() });
        app.apply(AppEvent::SwitchProject(Project::from_json(&project("p4", "Contracts", false))));
        assert!(app.turn.is_none());
        assert_eq!(app.project, Some(("p4".into(), "Contracts".into())));
        assert!(matches!(&app.entries[..], [Entry::Notice(n)] if n == "Switched to project Contracts"));
    }

    #[test]
    fn project_commands_check_what_they_are_given() {
        let mut app = app();
        app.phase = Phase::Ready;
        app.project = Some(("p1".into(), "Default".into()));
        app.input = "/project new".into();
        app.submit();
        assert!(matches!(app.entries.last(), Some(Entry::Error(e)) if e.contains("/project new <name>")));
        app.input = "/project import /no/such/archive.zip".into();
        app.submit();
        assert!(matches!(app.entries.last(), Some(Entry::Error(e)) if e == "No such file: /no/such/archive.zip"));

        // With no project to chat in yet, the panel still opens, to make one.
        app.project = None;
        app.input = "/project".into();
        app.submit();
        app.apply(AppEvent::Projects(Ok(Vec::new())));
        assert!(screen(&app, 80, 24).contains("› + New project"));

        let research = Project { id: "abc123".into(), name: "Research Notes".into(), description: String::new(), system: false };
        assert!(research.named("abc123") && research.named("research notes"));
        assert!(!research.named("Research"));
    }

    #[test]
    fn with_no_model_enabled_the_recommended_one_is_named() {
        let model = |name: &str, caps: &str, recommended, enabled| ModelRow {
            name: name.into(),
            capabilities: caps.into(),
            overridden: false,
            context: None,
            access: "free".into(),
            accessible: true,
            downloaded: false,
            enabled,
            cloud: false,
            local: false,
            recommended,
        };
        let list = vec![
            model("big", "general, reasoning", false, false),
            model("small", "general", true, false),
            model("thinker", "general, reasoning", true, false),
        ];
        let nudge = model_nudge(&list).unwrap();
        assert!(nudge.contains("Recommended for your system: thinker"), "{}", nudge);
        let none_recommended = vec![model("big", "general", false, false)];
        assert_eq!(model_nudge(&none_recommended).as_deref(), Some("No model selected · open /models (ctrl+o) to enable one"));
        let enabled = vec![model("big", "general", false, true), model("small", "general", true, false)];
        assert_eq!(model_nudge(&enabled), None);
    }

    #[test]
    fn with_no_model_enabled_the_card_says_how_to_pick_one() {
        let mut app = app();
        app.phase = Phase::Ready;
        app.project = Some(("p1".into(), "Default".into()));
        let row = |name: &str, recommended, enabled| ModelRow {
            name: name.into(),
            capabilities: "general, reasoning".into(),
            overridden: false,
            context: None,
            access: "free".into(),
            accessible: true,
            downloaded: false,
            enabled,
            cloud: false,
            local: false,
            recommended,
        };
        app.apply(AppEvent::Models(Ok(vec![row("big", false, false), row("small", true, false)])));
        let text = screen(&app, 120, 30);
        assert!(text.contains("model    ⚠ none enabled · ctrl+o to pick one"), "{}", text);
        assert!(text.contains("Recommended for your system: small"), "{}", text);
        assert!(text.contains("Ask anything"), "{}", text);
        app.apply(AppEvent::Models(Ok(vec![row("big", false, true), row("small", true, true)])));
        let text = screen(&app, 120, 30);
        assert!(text.contains("model    big +1 more"), "{}", text);
        assert!(text.contains("Ask anything"), "{}", text);
    }

    #[test]
    fn a_long_prompt_wraps_onto_the_next_row() {
        assert_eq!(prompt_rows("", 10), [(0, 0)]);
        // At the last space that fits, the space kept on the row.
        assert_eq!(prompt_rows("hello world again", 12), [(0, 12), (12, 17)]);
        // A word longer than the row is cut.
        assert_eq!(prompt_rows("abcdefghij", 4), [(0, 4), (4, 8), (8, 10)]);
        // A line break starts a row of its own.
        assert_eq!(prompt_rows("ab\ncd", 10), [(0, 2), (3, 5)]);
        assert_eq!(cursor_row(&prompt_rows("hello world again", 12), 13), 1);

        let mut app = app();
        app.phase = Phase::Ready;
        app.project = Some(("p1".into(), "Default".into()));
        app.input = "make a python script that renames my photos by the date they were taken".into();
        app.cursor = app.input.chars().count();
        assert_eq!(app.input_rows(40), 2);
        let text = screen(&app, 40, 24);
        assert!(text.contains("> make a python script that renames my"), "{}", text);
        assert!(text.contains("  photos by the date they were taken"), "{}", text);
    }

    #[test]
    fn a_newer_release_asks_for_an_upgrade_in_the_footer() {
        let mut app = app();
        app.phase = Phase::Ready;
        app.apply(AppEvent::Update("9.9.9".into()));
        let text = screen(&app, 140, 24);
        assert!(text.contains("docent 9.9.9 is available"), "{}", text);
        let narrow = screen(&app, 30, 24);
        assert!(narrow.contains("docent 9.9.9 available"), "{}", narrow);
    }

    #[test]
    fn models_show_as_a_list() {
        let mut app = app();
        app.phase = Phase::Ready;
        app.project = Some(("p1".into(), "Default".into()));
        let model = |name: &str, downloaded, enabled| ModelRow {
            name: name.into(),
            capabilities: "general, vision".into(),
            overridden: false,
            context: None,
            access: "free".into(),
            accessible: true,
            downloaded,
            enabled,
            cloud: false,
            local: false,
            recommended: false,
        };
        let cloud = ModelRow { cloud: true, ..model("sonnet", false, false) };
        app.overlay = Overlay::Models {
            rows: Some(Ok(vec![model("gemma4-e4b", false, false), model("qwen3.5-2b", true, true), cloud])),
            selected: 1,
        };
        let text = screen(&app, 80, 24);
        assert!(text.contains("  gemma4-e4b  not downloaded  general, vision"), "{}", text);
        assert!(text.contains("› qwen3.5-2b  ✓ enabled"), "{}", text);
        assert!(text.contains("  sonnet ☁    disabled"), "{}", text);
        assert!(!text.contains("Capabilities") && !text.contains("Models"), "{}", text);
    }

    #[test]
    fn a_dropped_zip_fills_in_the_import() {
        let dir = std::env::temp_dir().join(format!("slp-zip-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let archive = dir.join("my project.zip");
        std::fs::write(&archive, b"zip").unwrap();
        // As a terminal drops it: the space escaped.
        let dropped = archive.display().to_string().replace(' ', "\\ ");
        let mut app = app();
        app.phase = Phase::Ready;
        app.project = Some(("p1".into(), "Default".into()));

        // On an empty prompt (a drop on the projects panel closes it first).
        app.paste(&dropped);
        assert_eq!(app.input, format!("/project import {}", dropped));
        assert_eq!(app.cursor, app.input.chars().count());
        assert!(app.attachments.is_empty() && app.entries.is_empty());
        // Nothing's imported until Enter, and then it's the file dropped.
        assert!(app.turn.is_none());

        // After a typed `/project import`, as its path.
        app.input = "/project import".into();
        app.cursor = app.input.chars().count();
        app.paste(&dropped);
        assert_eq!(app.input, format!("/project import {}", dropped));
        assert_eq!(dropped_files(app.input.split_once("import ").unwrap().1).unwrap(), [archive.clone()]);
        let _ = std::fs::remove_dir_all(&dir);
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
        assert!(matches!(app.entries.last(), Some(Entry::Error(e)) if e.contains("Can't attach bundle.zip: to import it")));
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
        assert_eq!(app.input_rows(80), 4);

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
    fn slash_hints_and_runs_the_projects_skills() {
        // Sending a turn spawns it on the runtime.
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let _entered = runtime.enter();
        let mut app = app();
        app.phase = Phase::Ready;
        app.project = Some(("p1".into(), "Default".into()));
        let skills = vec![
            ("morning".to_string(), "the day's brief".to_string()),
            ("Meeting_Notes".to_string(), String::new()),
            ("status".to_string(), "shadowed by /status".to_string()),
        ];
        // A late answer for a project switched away from is dropped.
        app.apply(AppEvent::Skills("p0".into(), skills.clone()));
        assert!(app.skills.is_empty());
        app.apply(AppEvent::Skills("p1".into(), skills));

        app.input = "/m".into();
        let names: Vec<String> = app.command_hints().into_iter().map(|(name, _)| name).collect();
        assert_eq!(names, ["/mcp", "/Meeting_Notes", "/models", "/morning"]);
        app.input = "/".into();
        let names: Vec<String> = app.command_hints().into_iter().map(|(name, _)| name).collect();
        assert_eq!(names.last().map(String::as_str), Some("/usage"));
        assert!(names.windows(2).all(|w| w[0].to_lowercase() <= w[1].to_lowercase()), "{:?}", names);
        assert!(!app.command_hints().iter().any(|(_, what)| what.contains("shadowed")));
        app.input = "/mor".into();
        let text = screen(&app, 80, 24);
        assert!(text.contains("› /morning"), "{}", text);
        assert!(text.contains("the day's brief"), "{}", text);
        assert!(!text.contains("skill:"), "{}", text);
        // A long name widens the column instead of running into its text.
        app.apply(AppEvent::Skills("p1".into(), vec![
            ("weekly-competitor-digest".to_string(), "rivals' news this week".to_string()),
            ("wiki".to_string(), "notes kept as linked pages, compiled from what you add and kept up to date as sources change".to_string()),
        ]));
        app.input = "/w".into();
        let text = screen(&app, 80, 24);
        assert!(text.contains("› /weekly-competitor-digest  rivals' news this week"), "{}", text);
        // The rest wraps under the description column, not cut off.
        assert!(text.contains("  /wiki                      notes kept as linked pages, compiled from what you"), "{}", text);
        let column = |needle: &str| text.lines().find_map(|l| l.find(needle)).unwrap();
        let rest = text.lines().find(|l| l.contains("add and kept up to date as sources change")).unwrap();
        assert_eq!(rest.trim_start().len(), rest.len() - column("notes kept"), "{}", text);
        assert!(!text.lines().any(|l| (l.contains("notes kept") || l.contains("add and kept")) && l.contains('…')), "{}", text);
        app.apply(AppEvent::Skills("p1".into(), vec![
            ("morning".to_string(), "the day's brief".to_string()),
            ("Meeting_Notes".to_string(), String::new()),
            ("status".to_string(), "shadowed by /status".to_string()),
        ]));
        app.input = "/mee".into();
        assert_eq!(app.command_hints(), [("/Meeting_Notes".to_string(), "Meeting_Notes".to_string())]);

        // Sent to the agent as typed, which runs the skill, in any spelling.
        app.input = "/meeting-notes summarize today".into();
        app.submit();
        assert!(matches!(app.entries.last(), Some(Entry::Reply(_))));
        assert!(app.entries.iter().any(|e| matches!(e, Entry::User(t) if t == "/meeting-notes summarize today")));
        app.stop_turn();
        app.input = "/nightly".into();
        app.submit();
        assert!(matches!(app.entries.last(), Some(Entry::Error(e)) if e.contains("Unknown command /nightly")));
    }

    #[test]
    fn memory_bar_colors_each_cell_by_its_pressure() {
        let colors = |percent| memory_bar(percent).into_iter().map(|s| (s.content.to_string(), s.style.fg)).collect::<Vec<_>>();
        let bar = colors(95);
        assert_eq!(bar.iter().map(|(c, _)| c.as_str()).collect::<String>(), "▰▰▰▰▰▰▰▰▰▰");
        assert_eq!(bar[6].1, Some(Color::Green));
        assert_eq!((bar[7].1, bar[8].1), (Some(Color::Yellow), Some(Color::Yellow)));
        assert_eq!(bar[9].1, Some(Color::Red));
        let low = colors(31);
        assert_eq!(low.iter().map(|(c, _)| c.as_str()).collect::<String>(), "▰▰▰▱▱▱▱▱▱▱");
        assert!(low[..3].iter().all(|(_, c)| *c == Some(Color::Green)));
        assert_eq!(colors(0).iter().filter(|(c, _)| c == "▰").count(), 0);
    }

    #[test]
    fn wraps_between_words() {
        assert_eq!(wrap("the day's brief", 40), ["the day's brief"]);
        assert_eq!(wrap("one two three four", 9), ["one two", "three", "four"]);
        assert_eq!(wrap("abcdefghij kl", 4), ["abcd", "efgh", "ij", "kl"]);
        assert_eq!(wrap("", 10), [""]);
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
    fn setup_says_the_step_running_on_the_status_line() {
        let mut app = app();
        for event in [
            StepEvent::Add("Start agent".into()),
            StepEvent::Detail(0, "port 38540".into()),
            StepEvent::Done(0),
            StepEvent::Add("Load model".into()),
            StepEvent::Start(1),
        ] {
            app.apply(AppEvent::Setup(event));
        }
        let text = screen(&app, 80, 30);
        assert!(text.contains("Load model… (0s)"), "{}", text);
        assert!(!text.contains("Booting up") && !text.contains("Start agent"), "{}", text);
        app.apply(AppEvent::Setup(StepEvent::Add("Decision model".into())));
        app.apply(AppEvent::Setup(StepEvent::Done(1)));
        app.apply(AppEvent::Setup(StepEvent::Progress(2, 377 << 20, 769 << 20)));
        let text = screen(&app, 100, 30);
        assert!(text.contains("Decision model… ") && text.contains("49%"), "{}", text);
    }

    #[test]
    fn code_blocks_are_drawn_as_blocks() {
        let reply = Reply {
            text: "Go:\n\n```go\nfunc main() {\n\tfmt.Println(\"a long line that wraps\")\n}\n```\n\nThen `run`.".into(),
            ..Default::default()
        };
        let lines = reply.body(20);
        let rows: Vec<String> = lines.iter().map(|l| l.to_string()).collect();
        let width = |row: &str| unicode_width::UnicodeWidthStr::width(row);
        assert_eq!(rows[0], "Go:");
        assert_eq!(rows[1], "");
        // Tabs expanded, rows cut at the width less the margins, each
        // filled to it.
        assert_eq!(rows[2], "  func main() {     ");
        assert_eq!(rows[3], "      fmt.Println(  ");
        assert_eq!(rows[4], "  \"a long line tha  ");
        assert_eq!(rows[5], "  t wraps\")         ");
        assert_eq!(rows[6], "  }                 ");
        // A row of background closes the block.
        assert_eq!(rows[7], "                    ");
        assert!(rows[2..8].iter().all(|row| width(row) == 20), "{:?}", rows);
        assert!(lines[2..8].iter().all(|l| l.spans.iter().all(|s| s.style.bg == Some(CODE_BG))), "{:?}", lines);
        assert!(lines[2..8].iter().all(|l| l.spans.iter().all(|s| s.style.fg.is_some())), "{:?}", lines);
        assert_eq!(rows[8], "");
        assert_eq!(rows[9], "Then run.");
        // A block still open while the reply streams is code to its end.
        assert_eq!(segments("a\n```\nb\n"), [(false, "a".to_string(), 0), (true, "```\nb".to_string(), 1)]);
        assert_eq!(
            segments("```\nb\n```\n```\nc\n```\nd"),
            [(true, "```\nb\n```".to_string(), 0), (true, "```\nc\n```".to_string(), 3), (false, "d".to_string(), 6)]
        );
    }

    #[test]
    fn a_file_streaming_in_is_laid_out_again_once_a_line_is_done() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("main.rs"), "fn main() {\n    old();\n}\n").unwrap();
        let mut reply = Reply { root: Some(dir.path().to_path_buf()), ..Default::default() };
        let shown = |reply: &Reply| reply.body(40).iter().map(|l| l.to_string()).collect::<Vec<_>>().join("\n");
        // While the reply comes in, the file is named, with its lines.
        reply.text = "Here:\n\n`main.rs`\n```rust\nfn main() {\n    ne".into();
        assert!(shown(&reply).contains("● main.rs · 2 lines"), "{}", shown(&reply));
        // More of the same line: it isn't laid out again.
        reply.text.push_str("w();");
        reply.rendered_at.set(None);
        assert!(shown(&reply).contains("· 2 lines"));
        // Another line done, it is.
        reply.text.push_str("\n}\n");
        reply.rendered_at.set(None);
        assert!(shown(&reply).contains("· 3 lines"), "{}", shown(&reply));
        // Its code streams in; it isn't a change yet.
        assert!(shown(&reply).contains("new();"), "{}", shown(&reply));
        assert!(!shown(&reply).contains("← main.rs"), "{}", shown(&reply));
        // The reply in, its change shows.
        reply.text.push_str("```\n");
        reply.stats = Some(TurnStats::default());
        assert!(shown(&reply).contains("← main.rs"), "{}", shown(&reply));
        // The prose before it is kept as it was laid out.
        assert_eq!(reply.pieces.borrow()[0].key, "Here:\n\n`main.rs`");
    }

    #[test]
    fn a_reply_making_files_shows_each_as_its_change_once_its_in() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("main.rs"), "fn main() {\n    old();\n}\n").unwrap();
        let mut reply = Reply { root: Some(dir.path().to_path_buf()), ..Default::default() };
        // Streaming, a file still coming shows its code, heredocs too, with the
        // commands around them.
        reply.text = "Here:\n\n```sh\ncat > main.rs <<'EOF'\nfn main() {\n    new();\n}\nEOF\ngit add -A\ncat > lib.rs <<'EOF'\npub".into();
        let rows: Vec<String> = reply.body(40).iter().map(|l| l.to_string().trim_end().to_string()).collect();
        // A file done shows its change while the next is still coming.
        assert!(rows.contains(&"← main.rs  +1 −1".to_string()), "{:?}", rows);
        assert!(rows.contains(&"● lib.rs · 1 line".to_string()), "{:?}", rows);
        assert!(rows.iter().any(|r| r.contains("git add -A")), "{:?}", rows);
        // In, each heredoc is its change.
        reply.text.push_str(" fn l() {}\nEOF\n```\n");
        reply.stats = Some(TurnStats::default());
        let rows: Vec<String> = reply.body(40).iter().map(|l| l.to_string().trim_end().to_string()).collect();
        assert!(rows.contains(&"← main.rs  +1 −1".to_string()), "{:?}", rows);
        assert!(rows.contains(&"# lib.rs  +1".to_string()), "{:?}", rows);
        assert!(rows.iter().any(|r| r.contains("git add -A")), "{:?}", rows);
        // Cut short with its block still open, the old file's end isn't
        // gone yet.
        let mut reply = Reply { root: Some(dir.path().to_path_buf()), ended: Some("stopped".into()), ..Default::default() };
        reply.text = "Here:\n\n`main.rs`\n```rust\nfn main() {\n    new();".into();
        let rows: Vec<String> = reply.body(40).iter().map(|l| l.to_string().trim_end().to_string()).collect();
        let at = |rows: &[String]| rows.iter().position(|r| r == "← main.rs  +1 −1").unwrap_or_else(|| panic!("{:?}", rows));
        let h = at(&rows);
        // Each hunk under its header, the lines numbered in the old and
        // the new file, as opencode shows an edit.
        assert_eq!(rows[h + 1..h + 5], ["  @@ -1,2 +1,2 @@", "   1  1  fn main() {", "   2    -    old();", "      2 +    new();"]);
        assert!(!rows.iter().any(|r| r.contains("-}")), "{:?}", rows);
        // Closed: the whole change. A new file is all additions.
        reply.ended = None;
        reply.stats = Some(TurnStats::default());
        reply.text = "Here:\n\n`main.rs`\n```rust\nfn main() {\n    new();\n}\n```\n\n`lib.rs`\n```rust\npub fn l() {}\n```\n".into();
        reply.rendered_at.set(None);
        let rows: Vec<String> = reply.body(40).iter().map(|l| l.to_string().trim_end().to_string()).collect();
        let h = at(&rows);
        assert_eq!(rows[h + 2..h + 6], ["   1  1  fn main() {", "   2    -    old();", "      2 +    new();", "   3  3  }"]);
        assert!(rows.contains(&"# lib.rs  +1".to_string()), "{:?}", rows);
        assert!(rows.contains(&"   1 +pub fn l() {}".to_string()), "{:?}", rows);
        // Written since, the change still reads against the file as it was.
        std::fs::write(dir.path().join("main.rs"), "fn main() {\n    new();\n}\n").unwrap();
        reply.text.push(' ');
        reply.rendered_at.set(None);
        let rows: Vec<String> = reply.body(40).iter().map(|l| l.to_string().trim_end().to_string()).collect();
        at(&rows);
        // A reply holding much less than the file isn't all of it: shown
        // as it is, not as the rest of the file taken out.
        let long: String = (1..=30).map(|i| format!("line {}\n", i)).collect();
        std::fs::write(dir.path().join("big.rs"), &long).unwrap();
        reply.text = "`big.rs`\n```rust\nline 1\nline 2\n```\n".into();
        reply.rendered_at.set(None);
        let rows: Vec<String> = reply.body(40).iter().map(|l| l.to_string().trim_end().to_string()).collect();
        assert!(rows.contains(&"← big.rs · less than the file, so not written over it".to_string()), "{:?}", rows);
        assert!(rows.contains(&"  line 1".to_string()) && !rows.iter().any(|r| r.contains("-line")), "{:?}", rows);
        // Shown the file whole, the agent's shorter one is all of it.
        reply.whole = vec!["big.rs".into()];
        reply.text.push_str("\nDone.");
        reply.rendered_at.set(None);
        let rows: Vec<String> = reply.body(40).iter().map(|l| l.to_string().trim_end().to_string()).collect();
        assert!(rows.contains(&"← big.rs  +0 −28".to_string()), "{:?}", rows);
        // Without a folder, a reply's code shows as it is.
        let plain = Reply { text: "`main.rs`\n```rust\nfn main() {}\n```".into(), ..Default::default() };
        let rows: Vec<String> = plain.body(40).iter().map(|l| l.to_string().trim_end().to_string()).collect();
        assert!(rows.contains(&"  fn main() {}".to_string()), "{:?}", rows);
    }

    #[test]
    fn shell_output_loses_its_escape_codes_and_keeps_its_columns() {
        assert_eq!(strip_ansi("\x1b[32mok\x1b[0m done"), "ok done");
        assert_eq!(strip_ansi("\x1b]0;title\x07x\x1b]2;t\x1b\\y"), "xy");
        assert_eq!(strip_ansi("50%\r100%\ndone\r\n"), "100%\ndone\n");
        assert_eq!(expand_tabs("a\tbc\td", 4), "a   bc  d");
        assert_eq!(expand_tabs("名\tx", 8), "名      x");
    }

    #[test]
    fn br_breaks_a_reply_line() {
        let reply = Reply { text: "one<br>two<br>\nthree<br>\n- four".into(), ..Default::default() };
        let rows: Vec<String> = reply.body(40).iter().map(|l| l.to_string()).collect();
        assert_eq!(rows, ["one", "two", "three", "", "- four"]);
    }

    #[test]
    fn links_in_tables_keep_the_borders_straight() {
        let reply = Reply {
            text: "| Name | Link |\n|---|---|\n\
                   | Crates | https://crates.io/ |\n\
                   | Docs | [https://doc.rust-lang.org/](https://doc.rust-lang.org/)<br>Language docs |\n\
                   | Guide | see [the guide](https://example.com/a/long/guide/path) here |"
                .into(),
            ..Default::default()
        };
        let lines = reply.body(40);
        let rows: Vec<String> = lines.iter().map(|l| l.to_string()).collect();
        let text = rows.join("\n");
        let width = |row: &String| unicode_width::UnicodeWidthStr::width(row.as_str());
        assert!(rows.iter().all(|row| width(row) == width(&rows[0])), "{}", text);
        assert!(!text.contains("<br>") && !text.contains("(https://crates.io") && !text.contains("(https://doc."), "{}", text);
        assert!(text.contains("│ Language docs"), "{}", text);
        let links = |wanted: &str| -> Vec<Option<String>> {
            let spans = lines.iter().flat_map(|l| &l.spans).filter(|s| s.content.contains(wanted));
            spans.map(|s| crate::links::id(s.style).and_then(crate::links::url)).collect()
        };
        assert_eq!(links("crates.io"), [Some("https://crates.io/".to_string())]);
        assert_eq!(links("doc.rust-lang"), [Some("https://doc.rust-lang.org/".to_string())]);
        // The guide's address wraps over two rows, and opens from both.
        let guide = Some("https://example.com/a/long/guide/path".to_string());
        assert_eq!(links("the guide"), vec![guide.clone()]);
        assert_eq!(links("(https://example.com"), vec![guide.clone()]);
        assert_eq!(links("/guide/path)"), [guide]);
        assert_eq!(links(" here"), [None]);
        let padding = lines.iter().flat_map(|l| &l.spans).filter(|s| s.content.trim().is_empty());
        assert!(padding.into_iter().all(|s| crate::links::id(s.style).is_none()), "padding is no link");
    }

    #[test]
    fn reply_links_and_bare_addresses_open_but_code_does_not() {
        let reply = Reply {
            text: "Read [the docs](https://smartloop.ai/docs), https://a.io/x_y_z*1 and `https://not.a/link`.\n\n\
                   ```\nhttps://nor.this\n```\n\n[notes](notes.md) and [top](#top), <https://auto.link>."
                .into(),
            ..Default::default()
        };
        let lines = reply.body(200);
        let spans: Vec<&Span> = lines.iter().flat_map(|l| &l.spans).collect();
        let span = |wanted: &str| *spans.iter().find(|s| s.content.contains(wanted)).unwrap_or_else(|| panic!("{wanted}: {lines:?}"));
        let link = |wanted: &str| crate::links::id(span(wanted).style).and_then(crate::links::url);
        let text: String = lines.iter().map(|l| l.to_string() + "\n").collect();

        assert_eq!(link("the docs").as_deref(), Some("https://smartloop.ai/docs"));
        assert_eq!(link("https://smartloop.ai/docs").as_deref(), Some("https://smartloop.ai/docs"));
        assert_eq!(span("https://a.io").content, "https://a.io/x_y_z*1");
        assert_eq!(link("https://a.io").as_deref(), Some("https://a.io/x_y_z*1"));
        assert_eq!(link("https://auto.link").as_deref(), Some("https://auto.link"));
        assert_eq!(link("not.a"), None);
        assert_eq!(link("nor.this"), None);
        assert_eq!(link("notes"), None);
        assert_eq!(link("top"), None);
        assert!(text.contains("https://a.io/x_y_z*1 and") && !text.contains("<https://auto.link>"), "{}", text);
        assert!(text.contains("notes (notes.md)") && text.contains("top,"), "{}", text);
        assert!(!text.contains("(https://a.io") && !text.contains("(https://auto.link"), "{}", text);
        assert!(spans.iter().all(|s| s.style.underline_color != Some(Color::Indexed(0))), "{:?}", lines);
    }

    #[test]
    fn replies_render_as_markdown() {
        let mut app = app();
        app.phase = Phase::Ready;
        app.project = Some(("p1".into(), "Default".into()));
        app.entries.push(Entry::Reply(Reply {
            text: "## Plan\n\n- **one** step\n- `two`\n\n```rust\nfn main() {}\n```\n\n| a | b |\n|---|---|\n| 1 | 2 |\n| 3 | 4 |".into(),
            ..Default::default()
        }));
        let text = screen(&app, 40, 30);
        let rows: Vec<&str> = text.lines().collect();
        let top = rows.iter().position(|r| r.starts_with("■ Plan")).expect(&text);
        assert_eq!(
            &rows[top..top + 15],
            [
                "■ Plan",
                "",
                "  - one step",
                "  - two",
                "",
                "    fn main() {}",
                "",
                "",
                "  ┌───┬───┐",
                "  │ a │ b │",
                "  ├───┼───┤",
                "  │ 1 │ 2 │",
                "  ├───┼───┤",
                "  │ 3 │ 4 │",
                "  └───┴───┘",
            ],
            "{}",
            text
        );
        assert!(!text.contains("**") && !text.contains("```") && !text.contains('#'), "{}", text);
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
            ..Default::default()
        }));
        let runtime = tokio::runtime::Runtime::new().unwrap();
        app.turn = Some(Turn {
            started: Instant::now(),
            task: runtime.spawn(async {}),
            entry: 1,
            tokens: 40,
            activity: Some("Reading roadmap.pdf".into()),
            files_dir: None,
            whole: Vec::new(),
            calls: Vec::new(),
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
        assert!(rows[23].contains("[enter] queue  [esc] interrupt  [?] shortcuts"), "{}", text);
    }

    #[test]
    fn answering_a_command_question_keeps_the_conversation_scrollable() {
        let mut app = app();
        app.phase = Phase::Ready;
        for i in 0..30 {
            app.entries.push(Entry::Notice(format!("line {i}")));
        }
        let commands: Vec<String> = (0..12).map(|i| format!("echo step {i}")).collect();
        app.entries.push(Entry::Ask(commands.clone()));
        app.ask = Some((std::path::PathBuf::from("."), commands));
        screen(&app, 80, 24);
        // Skipped: the question goes, a one-line notice takes its place.
        app.answer_ask(false, false);
        screen(&app, 80, 24);
        let limit = app.scroll_limit.get();
        // Measured afresh, the conversation is just as long.
        app.heights.borrow_mut().clear();
        screen(&app, 80, 24);
        assert_eq!(app.scroll_limit.get(), limit);
        // And scrolling up shows what's above.
        app.scroll(i32::MAX / 2);
        assert!(screen(&app, 80, 24).contains("line 0"));
    }

    #[test]
    fn prompts_sent_during_a_turn_queue_and_esc_lets_the_next_in() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let _inside = runtime.enter();
        let mut app = app();
        app.phase = Phase::Ready;
        app.project = Some(("p1".into(), "notes".into()));
        app.input = "first".into();
        app.submit();
        assert!(app.turn.is_some());

        // While it replies, the next prompts wait, shown over the prompt.
        for prompt in ["second", "third", "fourth"] {
            app.input = prompt.into();
            app.submit();
        }
        assert!(app.input.is_empty());
        let queued: Vec<&str> = app.queued.iter().map(|q| q.input.as_str()).collect();
        assert_eq!(queued, ["second", "third", "fourth"]);
        let text = screen(&app, 100, 24);
        assert!(text.contains("[queued] second") && text.contains("[queued] third"), "{}", text);
        assert!(text.contains("[esc] interrupt") && text.contains("[↑] unqueue"), "{}", text);

        // Up on the empty prompt takes back the last one, to edit.
        app.key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(app.input, "fourth");
        assert_eq!(app.queued.len(), 2);
        app.input.clear();
        app.cursor = 0;

        // Esc interrupts the reply even with prompts queued, and the next
        // one goes at once, as its own turn.
        app.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(app.turn.is_none());
        app.send_queued();
        assert!(app.turn.is_some());
        assert_eq!(app.queued.len(), 1);
        assert!(screen(&app, 80, 24).contains("> second"));
        app.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        app.send_queued();
        assert!(app.queued.is_empty() && app.turn.is_some());
        assert!(screen(&app, 80, 24).contains("> third"));
    }

    #[test]
    fn a_queued_prompt_waits_for_the_question_about_the_reply() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let _inside = runtime.enter();
        let mut app = app();
        app.phase = Phase::Ready;
        app.project = Some(("p1".into(), "notes".into()));
        app.ask = Some((std::path::PathBuf::from("."), vec!["ls".into()]));
        app.input = "next".into();
        app.submit();
        app.send_queued();
        assert_eq!(app.queued.len(), 1);
        assert!(app.turn.is_none());
        app.ask = None;
        app.send_queued();
        assert!(app.queued.is_empty() && app.turn.is_some());
    }
}
