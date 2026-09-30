//! Setup steps logged apt-style on stderr, the way Ubuntu installs packages,
//! under the Smartloop banner:
//!
//! ```text
//! █▀ █▀▄▀█ ▄▀█ █▀█ ▀█▀ █   █▀█ █▀█ █▀█
//! ▄█ █ ▀ █ █▀█ █▀▄  █  █▄▄ █▄█ █▄█ █▀▀
//!
//! Downloading SLP framework 1.2.7 ...
//! Get:1 https://dl.smartloop.ai/slp/1.2.7 darwin-arm64-slp.tar.gz [667 MB]
//! Fetched 667 MB in 42s (15.8 MB/s)
//! Unpacking slp (1.2.7) ...
//! Setting up slp (1.2.7) ...
//! Starting agent on port 38540 ...
//! Downloading default model ...
//! 45% [2 sl-mini 346 MB/769 MB]                               15.8 MB/s 27s
//! Progress: [ 40%] [#########################.....................................]
//! ```
//!
//! Each step prints one line when it starts; steps that turn out to be done
//! already (a model on disk) print nothing. A running download shows apt's
//! fetch line, replaced by `Get:` and `Fetched` lines when it finishes, and
//! the overall `Progress:` bar stays pinned at the bottom until the end. Off
//! a terminal (logs, CI) only the lines print.

use std::io::IsTerminal;
use std::time::{Duration, Instant};

use indicatif::{MultiProgress, ProgressBar, ProgressDrawTarget, ProgressState, ProgressStyle};

/// Redraws per second, so fast downloads don't flood the terminal.
const REDRAW_HZ: u8 = 10;
/// Resolution of the overall bar.
const OVERALL_UNITS: u64 = 1000;

#[derive(Clone, Copy, PartialEq)]
enum State {
    Pending,
    Active,
    Done,
}

struct Step {
    label: String,
    state: State,
    /// Where a download step fetches from and what, for its `Get:` line.
    download: Option<(String, String)>,
}

/// The download on screen.
struct Fetch {
    step: usize,
    /// apt's running `Get:` number.
    number: usize,
    bar: ProgressBar,
    started: Instant,
    /// Bytes already on disk when it started, e.g. a resumed download.
    resumed: u64,
    done: u64,
}

pub struct Checklist {
    multi: MultiProgress,
    steps: Vec<Step>,
    tty: bool,
    /// The pinned `Progress:` bar, shown from the first line on.
    overall: Option<ProgressBar>,
    fetch: Option<Fetch>,
    /// The line of the phase running now, with a spinner and elapsed time
    /// until the next line replaces it, so a slow step visibly isn't stuck.
    activity: Option<(String, ProgressBar)>,
    downloads: usize,
    /// Steps this run expects in all, including ones not queued yet.
    planned: usize,
    /// Whether any line printed, so a run that had nothing to do stays quiet.
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
            overall: None,
            fetch: None,
            activity: None,
            downloads: 0,
            planned: 0,
            printed: false,
            started: Instant::now(),
        }
    }

    /// Queue a step; it prints nothing until it starts.
    pub fn add(&mut self, label: &str) -> usize {
        self.push(label, None)
    }

    /// Queue a download of `file` from `source`, e.g. a URL's directory and
    /// its file name.
    pub fn add_download(&mut self, label: &str, source: &str, file: &str) -> usize {
        self.push(label, Some((source.to_string(), file.to_string())))
    }

    fn push(&mut self, label: &str, download: Option<(String, String)>) -> usize {
        self.steps.push(Step {
            label: label.to_string(),
            state: State::Pending,
            download,
        });
        self.steps.len() - 1
    }

    /// Expect `more` steps beyond those queued, so the overall bar doesn't
    /// fill up before later phases queue theirs.
    pub fn expect(&mut self, more: usize) {
        self.planned = self.planned.max(self.steps.len() + more);
    }

    /// Name what a download step fetches once it's known, e.g. the model.
    pub fn set_file(&mut self, step: usize, file: &str) {
        if let Some((_, f)) = &mut self.steps[step].download {
            *f = file.to_string();
        }
    }

    pub fn start(&mut self, step: usize) {
        if self.steps[step].state == State::Pending {
            self.steps[step].state = State::Active;
            let label = self.steps[step].label.clone();
            self.activity(&format!("{} ...", label));
        }
    }

    /// Log a phase within an active step, e.g. "Unpacking slp (1.2.7)".
    pub fn note(&mut self, step: usize, text: &str) {
        self.start(step);
        self.end_fetch();
        self.activity(&format!("{} ...", text));
    }

    /// Update the step's download; indicatif limits how often it redraws.
    pub fn progress(&mut self, step: usize, done: u64, total: u64) {
        self.start(step);
        if self.fetch.as_ref().is_some_and(|f| f.step != step) {
            self.end_fetch();
        }
        if self.fetch.is_none() {
            self.commit();
            self.downloads += 1;
            let overall = self.overall();
            let file = self.steps[step].download.as_ref().map(|(_, f)| f.clone()).unwrap_or_default();
            let bar = ProgressBar::new(total)
                .with_style(fetch_style())
                .with_prefix(format!("{} {}", self.downloads, file));
            let bar = self.multi.insert_before(&overall, bar);
            self.fetch = Some(Fetch {
                step,
                number: self.downloads,
                bar,
                started: Instant::now(),
                resumed: done,
                done: 0,
            });
        }
        let fetch = self.fetch.as_mut().expect("fetch started above");
        fetch.done = done;
        fetch.bar.set_length(total);
        fetch.bar.set_position(done);
        self.update_overall();
    }

    /// Log a line that doesn't end the step, e.g. a download retrying.
    pub fn warn(&mut self, text: &str) {
        self.line(text);
    }

    pub fn done(&mut self, step: usize) {
        self.commit();
        if self.fetch.as_ref().is_some_and(|f| f.step == step) {
            self.end_fetch();
        }
        if self.steps[step].state != State::Done {
            self.steps[step].state = State::Done;
            self.update_overall();
        }
    }

    /// Finish every step before `step` that is still open: the server moved
    /// past them, whether or not it said so.
    pub fn finish_before(&mut self, step: usize) {
        for i in 0..step {
            self.done(i);
        }
    }

    /// Mark every step that hasn't finished as done.
    pub fn finish_all(&mut self) {
        for i in 0..self.steps.len() {
            self.done(i);
        }
    }

    /// Close with `✓ <text> in 1min 12s`, if any step printed. Returns
    /// whether it printed.
    pub fn summary(&mut self, text: &str) -> bool {
        self.clear();
        if !self.printed {
            return false;
        }
        let elapsed = format_duration(self.started.elapsed());
        if self.tty {
            eprintln!("{}✓{} {}{} in {}{}", GREEN, RESET, BOLD, text, elapsed, RESET);
        } else {
            eprintln!("✓ {} in {}", text, elapsed);
        }
        true
    }

    /// Log the step as failed, apt's `Err:` for a download, and exit with the
    /// error.
    pub fn fail(&mut self, step: usize, message: String) -> ! {
        self.commit();
        if let Some(fetch) = self.fetch.take() {
            fetch.bar.finish_and_clear();
            if let Some((source, file)) = self.steps[fetch.step].download.clone() {
                self.line(&format!("Err:{} {} {}", fetch.number, source, file));
            }
        }
        self.steps[step].state = State::Done;
        self.clear();
        crate::fail(message)
    }

    /// Print a log line above the bars, after the banner on the first one.
    fn line(&mut self, text: &str) {
        self.commit();
        if self.tty {
            self.banner();
            self.overall();
            // Printed while the bars are lifted, with a real newline:
            // `MultiProgress::println` pads lines to the terminal width
            // instead, so copying the log joins them into one.
            self.multi.suspend(|| eprintln!("{}", text));
        } else {
            self.printed = true;
            eprintln!("{}", text);
        }
    }

    /// Show `text` as the running phase: `Setting up slp (1.2.7) ... ⠹ 23s`.
    fn activity(&mut self, text: &str) {
        if !self.tty {
            self.line(text);
            return;
        }
        self.commit();
        self.banner();
        let overall = self.overall();
        let bar = ProgressBar::new_spinner()
            .with_style(activity_style())
            .with_message(text.to_string());
        let bar = self.multi.insert_before(&overall, bar);
        bar.enable_steady_tick(Duration::from_millis(100));
        self.activity = Some((text.to_string(), bar));
    }

    /// Replace the running phase's spinner with its plain line.
    fn commit(&mut self) {
        if let Some((text, bar)) = self.activity.take() {
            bar.finish_and_clear();
            self.multi.suspend(|| eprintln!("{}", text));
        }
    }

    /// The Smartloop banner, before the first line.
    fn banner(&mut self) {
        if self.printed {
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

    /// The pinned `Progress:` bar, created with the first line.
    fn overall(&mut self) -> ProgressBar {
        if let Some(bar) = &self.overall {
            return bar.clone();
        }
        let bar = self.multi.add(ProgressBar::new(OVERALL_UNITS).with_style(overall_style()));
        self.overall = Some(bar.clone());
        self.update_overall();
        bar
    }

    /// Steps done out of those expected, with the running download counted
    /// by its bytes; steps queued later never move it back.
    fn update_overall(&mut self) {
        let Some(bar) = &self.overall else { return };
        let done = self.steps.iter().filter(|s| s.state == State::Done).count() as f64;
        let fetching = self.fetch.as_ref().map_or(0.0, |f| match f.bar.length() {
            Some(total) if total > 0 => (f.done as f64 / total as f64).min(1.0),
            _ => 0.0,
        });
        let total = self.steps.len().max(self.planned).max(1) as f64;
        let units = ((done + fetching) / total * OVERALL_UNITS as f64) as u64;
        if units > bar.position() {
            bar.set_position(units);
        }
    }

    /// Swap a finished download's fetch line for apt's `Get:` and `Fetched`.
    fn end_fetch(&mut self) {
        let Some(fetch) = self.fetch.take() else { return };
        fetch.bar.finish_and_clear();
        let Some((source, file)) = self.steps[fetch.step].download.clone() else { return };
        // `Fetched` counts only this run's bytes, not a resumed download's.
        let fetched = fetch.done.saturating_sub(fetch.resumed);
        let elapsed = fetch.started.elapsed();
        let speed = fetched as f64 / elapsed.as_secs_f64().max(0.001);
        self.line(&format!("Get:{} {} {} [{}]", fetch.number, source, file, format_size(fetch.done)));
        self.line(&format!(
            "Fetched {} in {} ({}/s)",
            format_size(fetched),
            format_duration(elapsed),
            format_size(speed as u64)
        ));
    }

    /// Take every bar off the screen, as apt does when it finishes.
    fn clear(&mut self) {
        self.commit();
        if let Some(fetch) = self.fetch.take() {
            fetch.bar.finish_and_clear();
        }
        if let Some(bar) = self.overall.take() {
            bar.finish_and_clear();
        }
    }
}

impl Drop for Checklist {
    fn drop(&mut self) {
        self.clear();
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
const RESET: &str = "\x1b[0m";
/// apt's `Progress:` label: black on green.
const PROGRESS_LABEL: &str = "\x1b[30;42m";

/// Smartloop brand pink (#e55d9c, `--sl-brand-primary` in the studio app),
/// in 24-bit color where the terminal supports it and the nearest 256-color
/// shade elsewhere (Terminal.app has no truecolor).
fn pink() -> &'static str {
    let truecolor = std::env::var("COLORTERM")
        .map(|v| v.contains("truecolor") || v.contains("24bit"))
        .unwrap_or(false);
    if truecolor { "\x1b[38;2;229;93;156m" } else { "\x1b[38;5;169m" }
}

/// apt's fetch line: `45% [2 sl-mini 346 MB/769 MB]    15.8 MB/s 27s`.
fn fetch_style() -> ProgressStyle {
    ProgressStyle::with_template("{pct}[{prefix} {sizes}]{wide_msg} {rate}")
        .expect("valid template")
        .with_key("pct", |state: &ProgressState, w: &mut dyn std::fmt::Write| {
            if state.len().is_some_and(|t| t > 0) {
                let _ = write!(w, "{}% ", (state.fraction() * 100.0) as u64);
            }
        })
        .with_key("sizes", |state: &ProgressState, w: &mut dyn std::fmt::Write| {
            let _ = match state.len().filter(|&t| t > 0) {
                Some(total) => write!(w, "{}/{}", format_size(state.pos()), format_size(total)),
                None => write!(w, "{}", format_size(state.pos())),
            };
        })
        .with_key("rate", |state: &ProgressState, w: &mut dyn std::fmt::Write| {
            let _ = write!(w, "{}/s", format_size(state.per_sec() as u64));
            if state.len().is_some_and(|t| t > 0) && state.per_sec() > 0.0 {
                let _ = write!(w, " {}", format_duration(state.eta()));
            }
        })
}

/// The running phase: its line, then a dim spinner and time so far.
fn activity_style() -> ProgressStyle {
    ProgressStyle::with_template(&format!("{{msg}} {}{{spinner}} {{elapsed}}{}", DIM, RESET))
        .expect("valid template")
        .tick_chars("⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏ ")
}

/// apt's pinned bar: `Progress: [ 40%] [#########.............]`.
fn overall_style() -> ProgressStyle {
    let template = format!("{}Progress: [{{pct}}]{} [{{wide_bar}}]", PROGRESS_LABEL, RESET);
    ProgressStyle::with_template(&template)
        .expect("valid template")
        .progress_chars("#.")
        .with_key("pct", |state: &ProgressState, w: &mut dyn std::fmt::Write| {
            let _ = write!(w, "{:>3}%", (state.fraction() * 100.0) as u64);
        })
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

/// apt's durations: `42s`, `1min 5s`, `1h 2min`.
fn format_duration(d: Duration) -> String {
    let secs = d.as_secs();
    match secs {
        0..=59 => format!("{}s", secs),
        60..=3599 => format!("{}min {}s", secs / 60, secs % 60),
        _ => format!("{}h {}min", secs / 3600, secs % 3600 / 60),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn styles_build() {
        fetch_style();
        overall_style();
        activity_style();
    }

    #[test]
    fn durations_read_like_apt() {
        assert_eq!(format_duration(Duration::from_secs(42)), "42s");
        assert_eq!(format_duration(Duration::from_secs(65)), "1min 5s");
        assert_eq!(format_duration(Duration::from_secs(3720)), "1h 2min");
    }
}
