//! A checklist of setup steps drawn on stderr under the Smartloop banner:
//!
//! ```text
//! █▀ █▀▄▀█ ▄▀█ █▀█ ▀█▀ █   █▀█ █▀█ █▀█
//! ▄█ █ ▀ █ █▀█ █▀▄  █  █▄▄ █▄█ █▄█ █▀▀
//!
//! [✓] SLP framework 1.2.7                667 MB
//! [✓] Start agent                    port 38540
//! [✓] Embeddings (bge-m3)                417 MB
//! [•] Chat model sl-mini
//!     ██████████████▋░░░░░░░░░░░░░░░   49%  377 MB/769 MB
//! [ ] Default project
//! [ ] Load model
//! [ ] Skills and connections
//!
//! ✓ Setup complete in 1min 12s
//! ```
//!
//! On a terminal the list redraws in place: finished steps stay put, the
//! active one carries a progress bar in Smartloop pink while it downloads,
//! and otherwise its elapsed time, so a slow step (macOS checking a fresh
//! framework on first launch) visibly isn't stuck. Off a terminal (logs, CI)
//! each step prints one line when it finishes.

use std::io::IsTerminal;
use std::time::{Duration, Instant};

use indicatif::{MultiProgress, ProgressBar, ProgressDrawTarget, ProgressState, ProgressStyle};

const BAR_WIDTH: usize = 30;
const LABEL_WIDTH: usize = 28;
const DETAIL_WIDTH: usize = 12;
/// Redraws per second, so fast downloads don't flood the terminal.
const REDRAW_HZ: u8 = 10;
/// An active step shows its elapsed time once it has run this long.
const SHOW_ELAPSED_AFTER: Duration = Duration::from_secs(2);

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
    /// Dim text in the right column: a phase while active, a result when done.
    detail: String,
    /// Size of the step's download, shown when it finishes without a detail.
    downloaded: Option<u64>,
    /// Whether the line currently carries the download bar.
    fetching: bool,
    bar: ProgressBar,
}

/// Where setup reports its steps: the checklist on stderr, or the TUI.
pub trait Steps {
    /// Queue a step, shown as pending until it starts.
    fn add(&mut self, label: &str) -> usize;
    /// Name what a step works on once it's known, e.g. "Chat model" becomes
    /// "Chat model sl-mini".
    fn add_name(&mut self, step: usize, name: &str);
    /// Set the dim text shown right of the label, e.g. "port 38540".
    fn set_detail(&mut self, step: usize, detail: &str);
    fn start(&mut self, step: usize);
    /// Show a phase of an active step, e.g. "unpacking…", in place of its bar.
    fn note(&mut self, step: usize, detail: &str);
    /// Update the step's download bar.
    fn progress(&mut self, step: usize, done: u64, total: u64);
    /// Report something that doesn't end the step, e.g. a download retrying.
    fn warn(&mut self, text: &str);
    fn done(&mut self, step: usize);
    /// Mark the step failed and stop with the error.
    fn fail(&mut self, step: usize, message: String) -> !;
    /// Steps queued so far.
    fn len(&self) -> usize;

    /// Finish every step before `step` that is still open: the server moved
    /// past them, whether or not it said so.
    fn finish_before(&mut self, step: usize) {
        for i in 0..step {
            self.done(i);
        }
    }

    /// Mark every step that hasn't finished as done.
    fn finish_all(&mut self) {
        for i in 0..self.len() {
            self.done(i);
        }
    }
}

/// `label` with `name` appended, unless it names it already.
pub fn with_name(label: &str, name: &str) -> Option<String> {
    (!label.split_whitespace().any(|w| w == name)).then(|| format!("{} {}", label, name))
}

pub struct Checklist {
    multi: MultiProgress,
    steps: Vec<Step>,
    tty: bool,
    /// Whether the banner is out, so a run that had nothing to do stays quiet.
    printed: bool,
    started: Instant,
}

impl Checklist {
    pub fn new() -> Self {
        let tty = std::io::stderr().is_terminal();
        let target = if tty {
            ProgressDrawTarget::stderr_with_hz(REDRAW_HZ)
        } else {
            ProgressDrawTarget::hidden()
        };
        Checklist {
            multi: MultiProgress::with_draw_target(target),
            steps: Vec::new(),
            tty,
            printed: false,
            started: Instant::now(),
        }
    }

}

impl Steps for Checklist {
    /// Queue a step, shown as pending until it starts.
    fn add(&mut self, label: &str) -> usize {
        self.banner();
        let bar = self.multi.add(ProgressBar::new(0));
        self.steps.push(Step {
            label: label.to_string(),
            state: State::Pending,
            detail: String::new(),
            downloaded: None,
            fetching: false,
            bar,
        });
        let step = self.steps.len() - 1;
        self.redraw(step);
        step
    }

    fn add_name(&mut self, step: usize, name: &str) {
        if let Some(label) = with_name(&self.steps[step].label, name) {
            self.steps[step].label = label;
            self.redraw(step);
        }
    }

    /// Set the dim text shown right of the label, e.g. "port 38540".
    fn set_detail(&mut self, step: usize, detail: &str) {
        self.steps[step].detail = detail.to_string();
        self.redraw(step);
    }

    fn start(&mut self, step: usize) {
        if self.steps[step].state == State::Pending {
            self.steps[step].state = State::Active;
            let bar = &self.steps[step].bar;
            bar.reset_elapsed();
            bar.enable_steady_tick(Duration::from_millis(1000 / REDRAW_HZ as u64));
            self.redraw(step);
        }
    }

    /// Show a phase of an active step, e.g. "unpacking…", in place of its bar.
    fn note(&mut self, step: usize, detail: &str) {
        self.start(step);
        let s = &mut self.steps[step];
        s.detail = detail.to_string();
        s.fetching = false;
        self.redraw(step);
    }

    /// Update the step's download bar; indicatif limits how often it redraws.
    fn progress(&mut self, step: usize, done: u64, total: u64) {
        self.start(step);
        let s = &mut self.steps[step];
        s.downloaded = Some(total.max(done));
        s.bar.set_length(total);
        s.bar.set_position(done);
        if !s.fetching {
            s.fetching = true;
            s.detail.clear();
            self.redraw(step);
        }
    }

    /// Print a line above the list that doesn't end the step, e.g. a
    /// download retrying.
    fn warn(&mut self, text: &str) {
        if self.tty {
            // Printed while the list is lifted, with a real newline:
            // `MultiProgress::println` pads lines to the terminal width
            // instead, so copying the output joins them into one.
            self.multi.suspend(|| eprintln!("{}{}{}", DIM, text, RESET));
        } else {
            eprintln!("{}", text);
        }
    }

    fn done(&mut self, step: usize) {
        let s = &mut self.steps[step];
        if s.state == State::Done {
            return;
        }
        s.state = State::Done;
        s.fetching = false;
        if s.detail.is_empty()
            && let Some(size) = s.downloaded
        {
            s.detail = format_size(size);
        }
        if !self.tty {
            eprintln!("[✓] {}  {}", s.label, s.detail);
        }
        self.redraw(step);
        self.steps[step].bar.finish();
    }

    /// Mark the step failed, leave the list on screen, and exit with the error.
    fn fail(&mut self, step: usize, message: String) -> ! {
        let s = &mut self.steps[step];
        s.state = State::Failed;
        s.fetching = false;
        if !self.tty {
            eprintln!("[✗] {}", s.label);
        }
        self.redraw(step);
        for s in &self.steps {
            s.bar.finish();
        }
        crate::fail(message)
    }

    fn len(&self) -> usize {
        self.steps.len()
    }
}

impl Checklist {
    /// Close with `✓ <text> in 1min 12s` below the list, if there was one.
    /// Returns whether it printed.
    pub fn summary(&mut self, text: &str) -> bool {
        if self.steps.is_empty() {
            return false;
        }
        self.finish_all();
        let elapsed = format_duration(self.started.elapsed());
        if self.tty {
            self.multi.suspend(|| eprintln!("\n{}✓{} {}{} in {}{}", GREEN, RESET, BOLD, text, elapsed, RESET));
        } else {
            eprintln!("✓ {} in {}", text, elapsed);
        }
        true
    }


    /// Restyle the step's line for its current state.
    fn redraw(&self, step: usize) {
        let s = &self.steps[step];
        let label = format!("{:<w$}", truncate(&s.label, LABEL_WIDTH), w = LABEL_WIDTH);
        let detail = if s.detail.is_empty() {
            String::new()
        } else {
            format!(" {}{:>w$}{}", DIM, s.detail, RESET, w = DETAIL_WIDTH)
        };
        let line = match s.state {
            State::Done => format!("[{}✓{}] {}{}", GREEN, RESET, label, detail),
            State::Failed => format!("[{}✗{}] {}", RED, RESET, label.trim_end()),
            State::Pending => format!("{}[ ] {}{}", DIM, label.trim_end(), RESET),
            State::Active => format!("[{}•{}] {}{}", pink(), RESET, label, detail),
        };
        // Escape braces so the line can sit in an indicatif template.
        let line = line.replace('{', "{{").replace('}', "}}");
        s.bar.set_style(match s.state {
            State::Active if s.fetching => download_style(&line),
            State::Active => active_style(&line, !s.detail.is_empty()),
            _ => plain_style(&line),
        });
    }

    /// The Smartloop banner, above the first step.
    fn banner(&mut self) {
        if self.printed || !self.tty {
            return;
        }
        self.printed = true;
        let pink = pink();
        self.multi.suspend(|| {
            for row in BANNER {
                eprintln!("{}{}{}", pink, row, RESET);
            }
            eprintln!();
        });
    }
}

impl Drop for Checklist {
    fn drop(&mut self) {
        for s in &self.steps {
            s.bar.finish();
        }
    }
}

/// The Smartloop wordmark SLP prints at startup.
const BANNER: [&str; 2] = [
    "█▀ █▀▄▀█ ▄▀█ █▀█ ▀█▀ █   █▀█ █▀█ █▀█",
    "▄█ █ ▀ █ █▀█ █▀▄  █  █▄▄ █▄█ █▄█ █▀▀",
];

const BOLD: &str = "\x1b[1m";
const DIM: &str = "\x1b[2m";
const GREEN: &str = "\x1b[32m";
const RED: &str = "\x1b[31m";
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

/// A finished, failed or pending step: just its line.
fn plain_style(line: &str) -> ProgressStyle {
    ProgressStyle::with_template(line).expect("valid template")
}

/// A running step without a bar: its line, then the time so far once it's
/// slow enough to matter, after the detail if there is one.
fn active_style(line: &str, has_detail: bool) -> ProgressStyle {
    ProgressStyle::with_template(&format!("{}{{took}}", line))
        .expect("valid template")
        .with_key("took", move |state: &ProgressState, w: &mut dyn std::fmt::Write| {
            if state.elapsed() >= SHOW_ELAPSED_AFTER {
                let took = format_duration(state.elapsed());
                let _ = if has_detail {
                    write!(w, " {}{}{}", DIM, took, RESET)
                } else {
                    write!(w, " {}{:>w$}{}", DIM, took, RESET, w = DETAIL_WIDTH)
                };
            }
        })
}

/// A running download: its line, and the bar indented under it.
fn download_style(line: &str) -> ProgressStyle {
    ProgressStyle::with_template(&format!("{}\n    {{fetch}}", line))
        .expect("valid template")
        .with_key("fetch", |state: &ProgressState, w: &mut dyn std::fmt::Write| {
            let _ = write!(w, "{}", bar_line(state.pos(), state.len().unwrap_or(0)));
        })
}

fn bar_line(done: u64, total: u64) -> String {
    if total == 0 {
        return format!("{}{}{}", DIM, format_size(done), RESET);
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

fn progress_bar(fraction: f64) -> String {
    bar(fraction, BAR_WIDTH)
}

/// A `width`-cell bar; eighth-width blocks make its leading edge move
/// smoothly.
pub fn bar(fraction: f64, width: usize) -> String {
    const PARTIAL: [&str; 8] = ["", "▏", "▎", "▍", "▌", "▋", "▊", "▉"];
    let eighths = (fraction.clamp(0.0, 1.0) * (width * 8) as f64) as usize;
    let full = eighths / 8;
    let mut bar = "█".repeat(full);
    if full < width {
        let partial = PARTIAL[eighths % 8];
        bar.push_str(partial);
        let used = full + usize::from(!partial.is_empty());
        bar.push_str(&"░".repeat(width - used));
    }
    bar
}

pub fn format_size(bytes: u64) -> String {
    let mb = bytes as f64 / (1024.0 * 1024.0);
    if mb >= 1024.0 {
        format!("{:.1} GB", mb / 1024.0)
    } else if mb >= 10.0 {
        format!("{:.0} MB", mb)
    } else {
        format!("{:.1} MB", mb)
    }
}

/// `42s`, `1min 5s`, `1h 2min`.
pub fn format_duration(d: Duration) -> String {
    let secs = d.as_secs();
    match secs {
        0..=59 => format!("{}s", secs),
        60..=3599 => format!("{}min {}s", secs / 60, secs % 60),
        _ => format!("{}h {}min", secs / 3600, secs % 3600 / 60),
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
    fn styles_build() {
        let line = "[•] {{braced}} label";
        plain_style(line);
        active_style(line, true);
        active_style(line, false);
        download_style(line);
    }

    #[test]
    fn progress_bar_is_always_full_width() {
        for step in 0..=1000 {
            let cells = progress_bar(step as f64 / 1000.0).chars().count();
            assert_eq!(cells, BAR_WIDTH, "at {}", step);
        }
    }

    #[test]
    fn durations_read_short() {
        assert_eq!(format_duration(Duration::from_secs(42)), "42s");
        assert_eq!(format_duration(Duration::from_secs(65)), "1min 5s");
        assert_eq!(format_duration(Duration::from_secs(3720)), "1h 2min");
    }
}
