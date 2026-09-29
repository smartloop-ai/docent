//! A checklist of setup steps drawn on stderr:
//!
//! ```text
//! [✓] SLP framework 1.2.7              667 MB
//! [✓] Start agent                  port 38540
//! [✓] Embeddings (bge-m3)              417 MB
//! [ ] Chat model sl-mini
//!     ██████████████▋░░░░░░░░░░░░░░░   49%  377/769 MB
//! [ ] Default project
//! ```
//!
//! On a terminal the whole list redraws in place, so finished steps stay put
//! and the active one carries a progress bar in Smartloop pink. Off a
//! terminal (logs, CI) each step prints one line when it finishes.

use std::io::{IsTerminal, Write};
use std::time::{Duration, Instant};

const BAR_WIDTH: usize = 30;
const LABEL_WIDTH: usize = 28;
const DETAIL_WIDTH: usize = 12;
const REDRAW_EVERY: Duration = Duration::from_millis(100);

#[derive(Clone, Copy, PartialEq)]
enum State {
    Pending,
    Active,
    Done,
    Failed,
}

struct Step {
    label: String,
    state: State,
    detail: String,
    bytes: Option<(u64, u64)>,
}

pub struct Checklist {
    steps: Vec<Step>,
    tty: bool,
    /// Lines the last draw left on screen, to move back over on redraw.
    drawn: usize,
    last_draw: Option<Instant>,
}

impl Checklist {
    pub fn new() -> Self {
        Checklist {
            steps: Vec::new(),
            tty: std::io::stderr().is_terminal(),
            drawn: 0,
            last_draw: None,
        }
    }

    /// Queue a step, shown as pending until it starts.
    pub fn add(&mut self, label: &str) -> usize {
        self.steps.push(Step {
            label: label.to_string(),
            state: State::Pending,
            detail: String::new(),
            bytes: None,
        });
        self.draw();
        self.steps.len() - 1
    }

    pub fn set_label(&mut self, step: usize, label: &str) {
        self.steps[step].label = label.to_string();
        self.draw();
    }

    pub fn start(&mut self, step: usize) {
        if self.steps[step].state == State::Pending {
            self.steps[step].state = State::Active;
            self.draw();
        }
    }

    /// Show a short note on an active step, e.g. "extracting…".
    pub fn note(&mut self, step: usize, detail: &str) {
        self.steps[step].detail = detail.to_string();
        self.steps[step].bytes = None;
        self.draw();
    }

    /// Update the step's byte progress; redraws at most every 100ms.
    pub fn progress(&mut self, step: usize, done: u64, total: u64) {
        let s = &mut self.steps[step];
        s.state = State::Active;
        s.detail.clear();
        s.bytes = Some((done, total));
        if self.last_draw.is_none_or(|t| t.elapsed() >= REDRAW_EVERY) {
            self.draw();
        }
    }

    pub fn done(&mut self, step: usize, detail: &str) {
        let s = &mut self.steps[step];
        if s.state == State::Done {
            return;
        }
        s.state = State::Done;
        s.detail = detail.to_string();
        s.bytes = None;
        if !self.tty {
            eprintln!("[✓] {}  {}", s.label, s.detail);
        }
        self.draw();
    }

    /// Finish every step before `step` that is still open: the server moved
    /// past them, whether or not it said so.
    pub fn finish_before(&mut self, step: usize) {
        for i in 0..step {
            if matches!(self.steps[i].state, State::Pending | State::Active) {
                let detail = self.steps[i].detail.clone();
                self.done(i, &detail);
            }
        }
    }

    /// Mark every step that hasn't finished as done.
    pub fn finish_all(&mut self) {
        for i in 0..self.steps.len() {
            if self.steps[i].state != State::Done {
                let detail = self.steps[i].detail.clone();
                self.done(i, &detail);
            }
        }
    }

    /// Mark the step failed, leave the list on screen, and exit with the error.
    pub fn fail(&mut self, step: usize, message: String) -> ! {
        let s = &mut self.steps[step];
        s.state = State::Failed;
        s.detail.clear();
        s.bytes = None;
        if !self.tty {
            eprintln!("[✗] {}", s.label);
        }
        self.draw();
        crate::fail(message)
    }

    fn draw(&mut self) {
        if !self.tty {
            return;
        }
        self.last_draw = Some(Instant::now());

        let mut out = String::new();
        if self.drawn > 0 {
            // Back to the first line of the previous draw.
            out.push_str(&format!("\x1b[{}F", self.drawn));
        }
        let mut lines = 0;
        for step in &self.steps {
            out.push_str("\x1b[2K");
            out.push_str(&step_line(step));
            out.push('\n');
            lines += 1;
            if let (State::Active, Some((done, total))) = (step.state, step.bytes) {
                out.push_str("\x1b[2K    ");
                out.push_str(&bar_line(done, total));
                out.push('\n');
                lines += 1;
            }
        }
        // A step that lost its bar leaves one stale line below; clear it.
        let stale = self.drawn.saturating_sub(lines);
        out.push_str(&"\x1b[2K\n".repeat(stale));
        self.drawn = lines + stale;

        let mut err = std::io::stderr().lock();
        let _ = err.write_all(out.as_bytes());
        let _ = err.flush();
    }
}

const GREEN: &str = "\x1b[32m";
const RED: &str = "\x1b[31m";
const DIM: &str = "\x1b[2m";
const RESET: &str = "\x1b[0m";

/// Smartloop brand pink (#e55d9c, `--sl-brand-primary` in the studio app),
/// in 24-bit color where the terminal supports it and the nearest 256-color
/// shade elsewhere (Terminal.app has no truecolor).
fn pink() -> &'static str {
    let truecolor = std::env::var("COLORTERM")
        .map(|v| v.contains("truecolor") || v.contains("24bit"))
        .unwrap_or(false);
    if truecolor { "\x1b[38;2;229;93;156m" } else { "\x1b[38;5;169m" }
}

fn step_line(step: &Step) -> String {
    let label = format!("{:<w$}", truncate(&step.label, LABEL_WIDTH), w = LABEL_WIDTH);
    let detail = if step.detail.is_empty() {
        String::new()
    } else {
        format!(" {}{:>w$}{}", DIM, step.detail, RESET, w = DETAIL_WIDTH)
    };
    match step.state {
        State::Done => format!("[{}✓{}] {}{}", GREEN, RESET, label, detail),
        State::Failed => format!("[{}✗{}] {}", RED, RESET, label.trim_end()),
        State::Active => format!("[{}•{}] {}{}", pink(), RESET, label, detail),
        State::Pending => format!("{}[ ] {}{}", DIM, label.trim_end(), RESET),
    }
}

fn bar_line(done: u64, total: u64) -> String {
    if total == 0 {
        return format!("{}{}", DIM, format_size(done)) + RESET;
    }
    let fraction = (done as f64 / total as f64).clamp(0.0, 1.0);
    format!(
        "{}{}{}  {:>3}%  {}/{}",
        pink(),
        progress_bar(fraction),
        RESET,
        (fraction * 100.0) as u64,
        format_size(done),
        format_size(total)
    )
}

/// Eighth-width blocks make the bar's leading edge move smoothly.
fn progress_bar(fraction: f64) -> String {
    const PARTIAL: [&str; 8] = ["", "▏", "▎", "▍", "▌", "▋", "▊", "▉"];
    let eighths = (fraction * (BAR_WIDTH * 8) as f64) as usize;
    let full = eighths / 8;
    let mut bar = "█".repeat(full);
    if full < BAR_WIDTH {
        let partial = PARTIAL[eighths % 8];
        bar.push_str(partial);
        let used = full + usize::from(!partial.is_empty());
        bar.push_str(&"░".repeat(BAR_WIDTH - used));
    }
    bar
}

pub fn format_size(bytes: u64) -> String {
    let mb = bytes as f64 / (1024.0 * 1024.0);
    if mb >= 1024.0 {
        format!("{:.1} GB", mb / 1024.0)
    } else {
        format!("{:.0} MB", mb)
    }
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

    #[test]
    fn progress_bar_is_always_full_width() {
        for step in 0..=1000 {
            let cells = progress_bar(step as f64 / 1000.0).chars().count();
            assert_eq!(cells, BAR_WIDTH, "at {}", step);
        }
    }
}
