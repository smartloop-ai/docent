//! Run the local agent without the studio app.
//!
//! The SLP framework is built into this binary (the `smartloop` crate), so
//! there is nothing to install: the agent is this executable started
//! detached as `__agent-serve` with `SLP_HOME=~/.smartloop`, the way
//! `slp-service.js` launches `slp agent start`. An `slp agent start` the
//! studio app runs on the same home is used as well, when it is this
//! framework version.

use std::fs;
use std::io::{BufRead, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use reqwest::blocking::Client;
use sysinfo::{Pid, Process, ProcessRefreshKind, ProcessStatus, ProcessesToUpdate, Signal, System, UpdateKind};

use crate::agent::{CLI_VERSION_FLAG, SERVE_SUBCOMMAND};
use crate::fail;
use crate::progress::{Checklist, Steps, format_size};

/// SLP framework version built into this CLI.
pub const VERSION: &str = smartloop::VERSION;
const DEFAULT_PORT: u16 = 38540;
const DEFAULT_DOWNLOAD_URL: &str = "https://dl.smartloop.ai";
/// Embedding GGUF SLP loads for document search (AppSettings.embedding_gguf_file).
const EMBEDDING_FILE: &str = "bge-m3-Q4_K_M.gguf";
const START_TIMEOUT: Duration = Duration::from_secs(120);
/// How long `smartloop agent stop` waits for a graceful exit before killing.
const STOP_TIMEOUT: Duration = Duration::from_secs(10);

/// `~/.smartloop`, or `SLP_HOME` when set — the same home SLP itself resolves.
pub fn install_dir() -> PathBuf {
    if let Ok(home) = std::env::var("SLP_HOME")
        && !home.is_empty()
    {
        return PathBuf::from(home);
    }
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .unwrap_or_else(|| fail("Cannot find the home directory".to_string()));
    PathBuf::from(home).join(".smartloop")
}

/// Port the local agent listens on: `SLP_PORT` (as the studio app honors
/// it), else the one the running agent bound, else 38540 (where an agent
/// from before random ports listens).
pub fn port() -> u16 {
    fixed_port().or_else(bound_port).unwrap_or(DEFAULT_PORT)
}

/// The port asked for with `SLP_PORT`. Without one the agent is started on a
/// free port of its choosing, so it never clashes with whatever else holds
/// 38540.
fn fixed_port() -> Option<u16> {
    std::env::var("SLP_PORT").ok().and_then(|p| p.parse().ok()).filter(|&p| p > 0)
}

/// Where the agent writes the port it bound once it's up:
/// `~/.smartloop/server.port`. It removes the file when it stops.
fn port_path() -> PathBuf {
    install_dir().join("server.port")
}

fn bound_port() -> Option<u16> {
    fs::read_to_string(port_path()).ok()?.trim().parse().ok().filter(|&p| p > 0)
}

fn is_non_empty_file(path: &Path) -> bool {
    fs::metadata(path).map(|m| m.is_file() && m.len() > 0).unwrap_or(false)
}

fn download(url: &str, dest: &Path, list: &mut dyn Steps, step: usize) -> Result<u64, String> {
    download_with(url, dest, list, step, STALL_TIMEOUT)
}

fn download_with(url: &str, dest: &Path, list: &mut dyn Steps, step: usize, stall: Duration) -> Result<u64, String> {
    // Blocking reqwest applies this to each read, so it catches a stall
    // without limiting how long the whole download may take.
    let client = Client::builder()
        .timeout(stall)
        .build()
        .map_err(|e| format!("Failed to build HTTP client: {}", e))?;
    let partial = dest.with_file_name(format!(
        "{}.part",
        dest.file_name().and_then(|n| n.to_str()).unwrap_or("download")
    ));

    let mut fetch = Fetch {
        downloaded: fs::metadata(&partial).map(|m| m.len()).unwrap_or(0),
        total: 0,
    };
    let mut failures = 0;
    loop {
        let before = fetch.downloaded;
        match fetch.attempt(&client, url, &partial, list, step, stall) {
            Ok(()) => break,
            Err(Failure::Fatal(e)) => {
                let _ = fs::remove_file(&partial);
                return Err(e);
            }
            Err(Failure::Retry(e)) => {
                // Attempts that got somewhere don't count against the limit.
                failures = if fetch.downloaded > before { 1 } else { failures + 1 };
                if failures >= DOWNLOAD_ATTEMPTS {
                    return Err(format!("Failed to download {}: {}", url, e));
                }
                list.warn(&format!("{}, resuming at {} ...", e, format_size(fetch.downloaded)));
                std::thread::sleep(Duration::from_secs(2 * failures as u64));
            }
        }
    }

    fs::rename(&partial, dest)
        .map_err(|e| format!("Failed to move download into {}: {}", dest.display(), e))?;
    Ok(fetch.downloaded)
}

/// Give up after this many failed download attempts in a row.
const DOWNLOAD_ATTEMPTS: u32 = 5;
/// A download read that brings nothing for this long counts as stalled.
const STALL_TIMEOUT: Duration = Duration::from_secs(30);

/// Bytes on disk so far and the full size, carried across attempts.
struct Fetch {
    downloaded: u64,
    total: u64,
}

enum Failure {
    /// The connection stalled or dropped; try again from where it stopped.
    Retry(String),
    /// Retrying won't help (not found, disk full).
    Fatal(String),
}

impl Fetch {
    /// One request, appending to `partial` from `downloaded` on.
    fn attempt(
        &mut self,
        client: &Client,
        url: &str,
        partial: &Path,
        list: &mut dyn Steps,
        step: usize,
        stall: Duration,
    ) -> Result<(), Failure> {
        let mut request = client.get(url);
        if self.downloaded > 0 {
            request = request.header(reqwest::header::RANGE, format!("bytes={}-", self.downloaded));
        }
        let mut response = request
            .send()
            .map_err(|e| Failure::Retry(format!("Connection failed ({})", e)))?;
        let status = response.status();
        let length = response.content_length().unwrap_or(0);

        let mut file = if status == reqwest::StatusCode::PARTIAL_CONTENT {
            self.total = content_range_total(&response).unwrap_or(self.downloaded + length);
            fs::OpenOptions::new().append(true).open(partial)
        } else if status == reqwest::StatusCode::RANGE_NOT_SATISFIABLE {
            // The `.part` is no prefix of this file (it changed on the
            // server); start over.
            let _ = fs::remove_file(partial);
            self.downloaded = 0;
            return Err(Failure::Retry("Partial download is stale".to_string()));
        } else if status.is_success() {
            // The whole file: nothing to resume, or the server ignored the range.
            self.downloaded = 0;
            self.total = length;
            fs::File::create(partial)
        } else if status.is_server_error() {
            return Err(Failure::Retry(format!("Server answered {}", status)));
        } else {
            return Err(Failure::Fatal(format!("Failed to download {}: {}", url, status)));
        }
        .map_err(|e| Failure::Fatal(format!("Failed to open {}: {}", partial.display(), e)))?;

        let mut buf = vec![0u8; 256 * 1024];
        list.progress(step, self.downloaded, self.total);
        loop {
            let n = match response.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => n,
                Err(e) if is_timeout(&e) => {
                    return Err(Failure::Retry(format!("Download stalled for {}s", stall.as_secs())));
                }
                Err(e) => return Err(Failure::Retry(format!("Connection dropped ({})", e))),
            };
            file.write_all(&buf[..n])
                .map_err(|e| Failure::Fatal(format!("Failed to write {}: {}", partial.display(), e)))?;
            self.downloaded += n as u64;
            list.progress(step, self.downloaded, self.total);
        }
        if self.total > 0 && self.downloaded < self.total {
            return Err(Failure::Retry("Connection closed early".to_string()));
        }
        Ok(())
    }
}

/// A read that hit the client's timeout; reqwest wraps it in a plain I/O
/// error.
fn is_timeout(e: &std::io::Error) -> bool {
    e.kind() == std::io::ErrorKind::TimedOut
        || e.get_ref()
            .and_then(|inner| inner.downcast_ref::<reqwest::Error>())
            .is_some_and(|r| r.is_timeout())
}

/// The full size from a 206's `Content-Range: bytes 100-999/1000`.
fn content_range_total(response: &reqwest::blocking::Response) -> Option<u64> {
    response
        .headers()
        .get(reqwest::header::CONTENT_RANGE)?
        .to_str()
        .ok()?
        .rsplit('/')
        .next()?
        .parse()
        .ok()
}

fn is_healthy(client: &Client, base_url: &str) -> bool {
    client
        .get(format!("{}/health", base_url))
        .timeout(Duration::from_secs(3))
        .send()
        .map(|r| r.status().is_success())
        .unwrap_or(false)
}

/// Make sure an agent answers at `base_url` and is ready to chat. For the
/// local default endpoint (`local`), the framework is installed and started
/// when nothing answers, and a first run pulls the models it needs; a custom
/// `SMARTLOOP_API_URL` is left to whoever runs it.
///
/// Locally, the agent process decides: a running one that doesn't answer yet
/// is still starting and is waited on rather than started a second time.
/// One left running after its home folder was deleted is killed first, so a
/// fresh install and agent take its place.
///
/// Returns whether it had anything to do.
pub fn ensure_running(client: &Client, base_url: &str, local: bool) -> bool {
    let mut list = Checklist::new();
    prepare(&mut list, client, base_url, local).unwrap_or_else(|e| fail(e));
    list.summary("Setup complete")
}

/// The work behind `ensure_running`, reported on `list`: the checklist on
/// stderr, or the TUI's setup view. Fails with a message only when a remote
/// agent doesn't answer; a failed step stops through `list`.
pub fn prepare(list: &mut dyn Steps, client: &Client, base_url: &str, local: bool) -> Result<(), String> {
    if !local {
        if !is_healthy(client, base_url) {
            return Err(format!("No agent is running at {}", base_url));
        }
        return Ok(());
    }
    let home = install_dir();
    if !home.exists()
        && let Some(pid) = agent_pid()
    {
        stop_for(list, Pid::from_u32(pid), "home missing");
    }
    // An agent from another build still answers, so it stops here and this
    // binary's agent starts in its place below.
    if let Some(pid) = outdated_agent() {
        stop_for(list, pid, &format!("upgrading to {}", env!("CARGO_PKG_VERSION")));
    }
    let running = agent_pid().is_some();
    let healthy = is_healthy(client, base_url);
    // With the agent down, queue every step now so the whole list shows from
    // the start; which setup steps have work to do is only known once the
    // agent answers.
    let mut queued = None;
    if !healthy {
        let start = list.add(if running { "Wait for agent" } else { "Start agent" });
        list.set_detail(start, &port_detail(running));
        queued = Some(queue_setup(list));
        if running {
            list.start(start);
            await_agent(list, start, client, None);
        } else {
            // Something other than an `slp` process may serve the port (a
            // dev build, say); only start one when nothing answers either.
            launch(list, start, client);
        }
    }
    // Known only now that the agent is up, when it picked its own port.
    let base_url = &crate::base_url();
    if needs_setup(client, base_url) {
        let steps = queued.unwrap_or_else(|| queue_setup(list));
        setup(list, &steps, client, base_url);
    } else if let Some(steps) = queued {
        for step in steps.all() {
            list.set_detail(step, "ready");
            list.done(step);
        }
    }
    Ok(())
}

/// Stop the agent as a step on `list`, saying why in `reason`.
fn stop_for(list: &mut dyn Steps, pid: Pid, reason: &str) {
    let step = list.add(&format!("Stop agent (pid {})", pid));
    list.set_detail(step, reason);
    list.start(step);
    kill_agent(pid);
    list.done(step);
}

/// The running agent when it is another build than this one: one this
/// binary served for another CLI version (an upgrade in place), or an `slp`
/// the CLI installed before the framework was built in, for another
/// framework version. An `slp` run from outside the home (a dev build, say)
/// is left alone.
fn outdated_agent() -> Option<Pid> {
    let system = processes();
    let pid = find_agent(&system)?;
    let process = system.process(pid)?;
    let args = args_of(process);
    let outdated = match served_cli_version(&args) {
        Some(version) => version != env!("CARGO_PKG_VERSION"),
        None => process.exe().is_some_and(|exe| runs_other_version(exe, &install_dir())),
    };
    outdated.then_some(pid)
}

/// The `--cli-version` an agent served by this binary was started with;
/// `None` for an `slp agent start`.
fn served_cli_version(args: &[String]) -> Option<&str> {
    if !args.iter().any(|a| a == SERVE_SUBCOMMAND) {
        return None;
    }
    Some(
        args.windows(2)
            .find(|w| w[0] == CLI_VERSION_FLAG)
            .map_or("", |w| w[1].as_str()),
    )
}

/// Whether `exe` is installed under `home` for a version other than this one.
fn runs_other_version(exe: &Path, home: &Path) -> bool {
    let home = fs::canonicalize(home).unwrap_or_else(|_| home.to_path_buf());
    let exe = fs::canonicalize(exe).unwrap_or_else(|_| exe.to_path_buf());
    exe.starts_with(&home) && !exe.starts_with(home.join(VERSION))
}

/// The start step's detail before the agent answers: its port, when known.
fn port_detail(running: bool) -> String {
    match fixed_port().or_else(|| bound_port().filter(|_| running)) {
        Some(port) => format!("port {}", port),
        None => "free port".to_string(),
    }
}

/// The steps `setup` works through, queued before it runs.
#[derive(Clone, Copy)]
struct SetupSteps {
    embeddings: usize,
    model: usize,
    project: usize,
    load: usize,
    services: usize,
}

impl SetupSteps {
    fn all(&self) -> [usize; 5] {
        [self.embeddings, self.model, self.project, self.load, self.services]
    }
}

fn queue_setup(list: &mut dyn Steps) -> SetupSteps {
    SetupSteps {
        embeddings: list.add("Embeddings (bge-m3)"),
        model: list.add("Base model"),
        project: list.add("Default project"),
        load: list.add("Load model"),
        services: list.add("Skills and connections"),
    }
}

/// `smartloop agent start`: bring the local agent up and ready, or say it
/// already is.
pub fn start(client: &Client, base_url: &str) {
    if !ensure_running(client, base_url, true)
        && let Some(pid) = agent_pid()
    {
        eprintln!("Agent already running (pid {}, port {})", pid, port());
    }
}

/// Same test the studio app runs before bootstrapping: no project yet, or no
/// model loaded.
fn needs_setup(client: &Client, base_url: &str) -> bool {
    let get = |path: &str| -> Option<serde_json::Value> {
        client.get(format!("{}{}", base_url, path)).send().ok()?.json().ok()
    };
    let has_projects = get("/v1/projects")
        .and_then(|d| d["projects"].as_array().map(|p| !p.is_empty()))
        .unwrap_or(false);
    let model_loaded = get("/health")
        .map(|h| h["status"] == "healthy")
        .unwrap_or(false);
    !has_projects || !model_loaded
}

/// First-run setup: make sure the workspace exists, fetch the embedding model
/// document search needs, then let `/v1/bootstrap` download the chat model,
/// create the default project and load it.
fn setup(list: &mut dyn Steps, steps: &SetupSteps, client: &Client, base_url: &str) {
    let SetupSteps { embeddings, model, project, load, services } = *steps;

    // A plain read that creates the workspace when there isn't one.
    let _ = client.get(format!("{}/v1/models/workspace", base_url)).send();
    match workspace_dir() {
        Some(workspace) => ensure_embeddings(list, embeddings, &workspace),
        // The agent fetches it itself the first time it indexes.
        None => {
            list.set_detail(embeddings, "on first index");
            list.done(embeddings);
        }
    }

    stream_progress(base_url, "/v1/bootstrap", serde_json::json!({}), list, model, &|status| {
        match status {
            "downloading" => Stage::Start(model),
            "download_complete" => Stage::Done(model),
            "model_ready" => Stage::Present(model),
            "creating_project" => Stage::Start(project),
            "project_created" => Stage::Done(project),
            "loading" => Stage::Start(load),
            "model_loaded" => Stage::Done(load),
            "setup" => Stage::Start(services),
            "setup_complete" => Stage::Done(services),
            _ => Stage::Other,
        }
    });
    list.finish_all();
}

/// The workspace SLP is using: `config.json`'s workspace id, or the only
/// UUID-named directory under the home that has a `models/` dir.
fn workspace_dir() -> Option<PathBuf> {
    let home = install_dir();
    for _ in 0..30 {
        if let Some(id) = fs::read_to_string(home.join("config.json"))
            .ok()
            .and_then(|c| serde_json::from_str::<serde_json::Value>(&c).ok())
            .and_then(|c| c["workspace"]["id"].as_str().map(str::to_string))
        {
            return Some(home.join(id));
        }

        let candidates: Vec<PathBuf> = fs::read_dir(&home)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.len() == 36 && n.chars().all(|c| c.is_ascii_hexdigit() || c == '-'))
                    && p.join("models").is_dir()
            })
            .collect();
        if candidates.len() == 1 {
            return candidates.into_iter().next();
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    None
}

/// Download the embedding GGUF to `<workspace>/models/embeddings/`, where SLP
/// looks for it (AppSettings.embedding_gguf_file / embedding_gguf_base_url).
/// Without it the agent downloads it on first index and falls back to TF-IDF
/// retrieval until then.
fn ensure_embeddings(list: &mut dyn Steps, step: usize, workspace: &Path) {
    let (source, file) = embedding_source();
    let dir = workspace.join("models").join("embeddings");
    let target = dir.join(&file);
    if is_non_empty_file(&target) {
        list.set_detail(step, "downloaded");
        list.done(step);
        return;
    }
    list.start(step);
    if let Err(e) = fs::create_dir_all(&dir) {
        list.fail(step, format!("Failed to create {}: {}", dir.display(), e));
    }

    let url = format!("{}/{}", source, file);
    download(&url, &target, list, step).unwrap_or_else(|e| list.fail(step, e));
    list.done(step);
}

/// Where the embedding GGUF comes from and its file name.
fn embedding_source() -> (String, String) {
    let file = std::env::var("SLP_EMBEDDING_GGUF_FILE").unwrap_or_else(|_| EMBEDDING_FILE.to_string());
    let base = std::env::var("SLP_EMBEDDING_GGUF_BASE_URL")
        .unwrap_or_else(|_| format!("{}/embeddings", DEFAULT_DOWNLOAD_URL));
    (base.trim_end_matches('/').to_string(), file)
}

/// Where one SSE status frame lands on the checklist.
pub enum Stage {
    /// The step began.
    Start(usize),
    /// The step finished.
    Done(usize),
    /// The step had nothing to do: its files were already there.
    Present(usize),
    /// Not a step of its own.
    Other,
}

/// POST `body` to an SSE endpoint that reports long-running work
/// (`/v1/bootstrap`, `/v1/init`) and show it on `list`: `stage` maps each
/// frame's `status` to a step, and byte progress goes to `download`. A step
/// starting finishes the ones before it, which covers stages the server skips
/// without saying (an existing project). Fails on an error frame, which
/// arrives inside a 200 response. Downloads can take minutes, so the request
/// has no timeout.
pub fn stream_progress(
    base_url: &str,
    path: &str,
    body: serde_json::Value,
    list: &mut dyn Steps,
    download: usize,
    stage: &dyn Fn(&str) -> Stage,
) {
    let client = Client::builder()
        .timeout(None)
        .build()
        .unwrap_or_else(|e| fail(format!("Failed to build HTTP client: {}", e)));
    let response = client
        .post(format!("{}{}", base_url, path))
        .json(&body)
        .send()
        .unwrap_or_else(|e| list.fail(download, format!("Failed to reach the agent: {}", e)));
    if !response.status().is_success() {
        let status = response.status();
        let detail = response
            .json::<serde_json::Value>()
            .ok()
            .and_then(|b| b["detail"].as_str().map(str::to_string))
            .unwrap_or_else(|| status.to_string());
        let hint = if matches!(status.as_u16(), 401 | 403) {
            "; sign in with `docent login`"
        } else {
            ""
        };
        list.fail(download, format!("{} ({}){}", detail, status, hint));
    }

    // Per-file byte counts: a model can be several files (weights, vision
    // projector), shown as one bar over their sum.
    let mut files: std::collections::HashMap<String, (u64, u64)> = std::collections::HashMap::new();
    let mut event = String::new();
    let mut current = download;

    for line in std::io::BufReader::new(response).lines() {
        let line = line.unwrap_or_else(|e| list.fail(current, format!("Stream dropped: {}", e)));
        let line = line.trim();
        if line.is_empty() {
            event.clear();
            continue;
        }
        if let Some(name) = line.strip_prefix("event:") {
            event = name.trim().to_string();
            continue;
        }
        let Some(data) = line.strip_prefix("data:") else { continue };
        let Ok(data) = serde_json::from_str::<serde_json::Value>(data.trim()) else { continue };

        if event == "error" || data["status"] == "error" {
            let message = data["message"].as_str().unwrap_or("unknown error").to_string();
            list.fail(current, message);
        }

        if let (Some(done), Some(total)) = (data["downloaded"].as_u64(), data["total"].as_u64())
            && total > 0
        {
            let name = data["filename"].as_str().unwrap_or_default().to_string();
            files.insert(name, (done, total));
            let (done, total) = files.values().fold((0, 0), |(d, t), (fd, ft)| (d + fd, t + ft));
            list.progress(download, done, total);
            continue;
        }

        let status = data["status"].as_str().unwrap_or_default();
        let message = data["message"].as_str().unwrap_or_default();
        if let Some(name) = model_name_in(message)
            && (status == "downloading" || status == "model_ready")
        {
            list.add_name(download, &name);
        }

        match stage(status) {
            Stage::Start(step) => {
                list.finish_before(step);
                list.start(step);
                current = step;
            }
            Stage::Done(step) => {
                list.finish_before(step);
                list.done(step);
            }
            Stage::Present(step) => {
                list.finish_before(step);
                list.set_detail(step, "downloaded");
                list.done(step);
            }
            Stage::Other => {}
        }
    }
}

/// The model named in a stage message such as "Downloading model sl-mini..."
/// or "Model sl-mini already available.".
fn model_name_in(message: &str) -> Option<String> {
    let mut words = message.split_whitespace();
    words.find(|w| w.eq_ignore_ascii_case("model"))?;
    let name = words.next()?.trim_end_matches(['.', '…']);
    (!name.is_empty()).then(|| name.to_string())
}

/// Start this binary's agent as `step`, detached from this process and
/// logging to `~/.smartloop/server.log`, and wait until `/health` answers.
fn launch(list: &mut dyn Steps, step: usize, client: &Client) {
    list.start(step);
    let binary = std::env::current_exe()
        .unwrap_or_else(|e| list.fail(step, format!("Cannot find this executable: {}", e)));

    let home = install_dir();
    let log_path = home.join("server.log");
    let mut log = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .unwrap_or_else(|e| list.fail(step, format!("Failed to open {}: {}", log_path.display(), e)));
    let _ = writeln!(log, "\n--- Service starting from smartloop CLI (pid {}) ---", std::process::id());
    let log_err = log
        .try_clone()
        .unwrap_or_else(|e| list.fail(step, format!("Failed to open {}: {}", log_path.display(), e)));

    let mut cmd = Command::new(&binary);
    cmd.args([SERVE_SUBCOMMAND, CLI_VERSION_FLAG, env!("CARGO_PKG_VERSION")]);
    match fixed_port() {
        Some(port) => {
            cmd.args(["--port", &port.to_string()]);
        }
        // Port 0: the agent binds a free one and writes it to `server.port`.
        // Through the environment, since `--port 0` reads as no port.
        None => {
            cmd.env("SLP_API_PORT", "0");
        }
    }
    // A file left by an agent that didn't stop cleanly would point at a dead
    // port; the new agent writes its own once it binds.
    let _ = fs::remove_file(port_path());
    cmd.env("SLP_HOME", &home)
        .current_dir(&home)
        .stdin(Stdio::null())
        .stdout(log)
        .stderr(log_err);
    detach(&mut cmd);

    // Never waited on once healthy: the agent outlives the CLI on purpose.
    #[allow(clippy::zombie_processes)]
    let child = cmd
        .spawn()
        .unwrap_or_else(|e| list.fail(step, format!("Failed to start {}: {}", binary.display(), e)));
    write_pid(child.id());
    await_agent(list, step, client, Some(child));
}

/// Wait until the agent answers `/health`, failing as soon as no agent
/// process is left to wait on. `child` is the agent this CLI just spawned.
fn await_agent(list: &mut dyn Steps, step: usize, client: &Client, mut child: Option<std::process::Child>) {
    let log_path = install_dir().join("server.log");
    let started = Instant::now();
    while started.elapsed() < START_TIMEOUT {
        // Read each time: an agent on a free port only says which once bound.
        let port_known = fixed_port().is_some() || bound_port().is_some();
        if port_known && is_healthy(client, &crate::base_url()) {
            list.set_detail(step, &format!("port {}", port()));
            list.done(step);
            return;
        }
        if let Some(status) = child.as_mut().and_then(|c| c.try_wait().ok().flatten()) {
            // Lost a race for the port: the studio app (or another CLI)
            // started an agent at the same moment, so ours could not bind.
            // Theirs coming up is as good as ours; with none left, fail.
            child = None;
            if agent_pid().is_none() {
                list.fail(
                    step,
                    format!("Agent exited before becoming healthy ({}); see {}", status, log_path.display()),
                );
            }
        } else if child.is_none() && agent_pid().is_none() {
            list.fail(step, format!("Agent exited before becoming healthy; see {}", log_path.display()));
        }
        std::thread::sleep(Duration::from_secs(1));
    }
    list.fail(step, format!("Timed out waiting for the agent to start; see {}", log_path.display()));
}

/// Where the CLI records the agent's pid: `~/.smartloop/agent.pid`. SLP's
/// own `agents.json` can name a start that lost the port race and exited.
fn pid_path() -> PathBuf {
    install_dir().join("agent.pid")
}

fn write_pid(pid: u32) {
    let _ = fs::write(pid_path(), pid.to_string());
}

fn processes() -> System {
    let mut system = System::new();
    let kind = ProcessRefreshKind::nothing()
        .with_exe(UpdateKind::OnlyIfNotSet)
        .with_cmd(UpdateKind::OnlyIfNotSet)
        .with_cwd(UpdateKind::OnlyIfNotSet);
    system.refresh_processes_specifics(ProcessesToUpdate::All, true, kind);
    system
}

fn args_of(process: &Process) -> Vec<String> {
    process.cmd().iter().map(|a| a.to_string_lossy().into_owned()).collect()
}

/// Whether the process is a live agent that serves this home — this
/// binary's `__agent-serve`, or an `slp agent start` such as the studio
/// app's — so a pid file pointing at a reused pid is not mistaken for it.
fn is_agent(process: &Process) -> bool {
    if process.status() == ProcessStatus::Zombie {
        return false;
    }
    let args = args_of(process);
    let name = process.exe().and_then(|e| e.file_name()).unwrap_or(process.name());
    let is_slp = name == if cfg!(windows) { "slp.exe" } else { "slp" };
    let starts_agent = args.iter().any(|a| a == SERVE_SUBCOMMAND)
        || (is_slp && args.windows(2).any(|w| w[0] == "agent" && w[1] == "start"));
    let agent_port = args.windows(2).find(|w| w[0] == "--port").and_then(|w| w[1].parse::<u16>().ok());
    starts_agent && is_our_port(agent_port) && serves_home(process.cwd(), &install_dir())
}

/// Whether an agent working in `cwd` belongs to `home`: the CLI starts it
/// there, and it only moves into its projects, which live under it. An
/// agent for another `SLP_HOME` (a test, a second workspace) is left alone.
/// One whose directory can't be read is given the benefit of the doubt.
fn serves_home(cwd: Option<&Path>, home: &Path) -> bool {
    let Some(cwd) = cwd else { return true };
    let canonical = |p: &Path| fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    canonical(cwd).starts_with(canonical(home))
}

/// Whether an agent started with `--port agent_port` (or none) is ours. With
/// `SLP_PORT` set it must be on that port, no `--port` meaning SLP's default.
/// Without, the agent picks its own port, so one started on no port in
/// particular is ours; only one pinned elsewhere (a dev build, say) is not.
fn is_our_port(agent_port: Option<u16>) -> bool {
    match fixed_port() {
        Some(fixed) => agent_port.unwrap_or(DEFAULT_PORT) == fixed,
        None => matches!(agent_port, None | Some(0) | Some(DEFAULT_PORT)),
    }
}

/// The running agent: the pid on file when it is still the agent, otherwise
/// any `slp agent start` on our port, such as one the studio app started.
fn find_agent(system: &System) -> Option<Pid> {
    let recorded = fs::read_to_string(pid_path())
        .ok()
        .and_then(|p| p.trim().parse().ok())
        .map(Pid::from_u32)
        .filter(|&pid| system.process(pid).is_some_and(is_agent));
    let found = recorded.or_else(|| {
        system
            .processes()
            .iter()
            .filter(|(_, p)| is_agent(p))
            // The agent's own workers run the same binary; take the parent.
            .filter(|(_, p)| !p.parent().and_then(|pp| system.process(pp)).is_some_and(is_agent))
            .map(|(&pid, _)| pid)
            .min()
    });
    match found {
        Some(pid) if recorded.is_none() => write_pid(pid.as_u32()),
        None => {
            let _ = fs::remove_file(pid_path());
        }
        _ => {}
    }
    found
}

/// Pid of the running local agent, if any.
pub fn agent_pid() -> Option<u32> {
    find_agent(&processes()).map(Pid::as_u32)
}

/// The agent and every process under it, so its workers stop with it.
fn process_tree(system: &System, root: Pid) -> Vec<Pid> {
    let mut tree = vec![root];
    let mut i = 0;
    while i < tree.len() {
        let parent = tree[i];
        tree.extend(
            system
                .processes()
                .iter()
                .filter(|(_, p)| p.parent() == Some(parent))
                .map(|(&pid, _)| pid),
        );
        i += 1;
    }
    tree
}

/// `smartloop agent stop`: stop the agent by its pid.
pub fn stop() {
    let Some(pid) = find_agent(&processes()) else {
        eprintln!("Agent is not running");
        return;
    };
    kill_agent(pid);
    eprintln!("Stopped agent (pid {})", pid);
}

/// Ask the agent and its workers to exit, kill whatever is left after
/// `STOP_TIMEOUT`, and forget its pid.
fn kill_agent(pid: Pid) {
    let mut system = processes();
    let tree = process_tree(&system, pid);
    for p in tree.iter().filter_map(|&p| system.process(p)) {
        // Windows has no SIGTERM; there it is a plain kill.
        if p.kill_with(Signal::Term).is_none() {
            p.kill();
        }
    }

    let alive = |system: &System| {
        tree.iter()
            .filter(|&&p| system.process(p).is_some_and(|p| p.status() != ProcessStatus::Zombie))
            .count()
    };
    let started = Instant::now();
    loop {
        system.refresh_processes_specifics(ProcessesToUpdate::Some(&tree), true, ProcessRefreshKind::nothing());
        if alive(&system) == 0 {
            break;
        }
        if started.elapsed() >= STOP_TIMEOUT {
            for p in tree.iter().filter_map(|&p| system.process(p)) {
                p.kill();
            }
            break;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    let _ = fs::remove_file(pid_path());
    let _ = fs::remove_file(port_path());
}

/// Keep the agent alive after the CLI exits and out of reach of the
/// terminal's Ctrl-C.
#[cfg(unix)]
fn detach(cmd: &mut Command) {
    use std::os::unix::process::CommandExt;
    cmd.process_group(0);
}

#[cfg(windows)]
fn detach(cmd: &mut Command) {
    use std::os::windows::process::CommandExt;
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    cmd.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader};
    use std::net::TcpListener;

    #[test]
    fn an_agent_in_another_home_is_not_ours() {
        let home = Path::new("/Users/me/.smartloop");
        assert!(serves_home(Some(home), home));
        assert!(serves_home(Some(&home.join("772a9194")), home));
        assert!(!serves_home(Some(Path::new("/tmp/other-home")), home));
        assert!(!serves_home(Some(Path::new("/Users/me/.smartloop-dev")), home));
        assert!(serves_home(None, home));
    }

    #[test]
    fn an_slp_from_another_version_is_outdated() {
        let home = Path::new("/Users/me/.smartloop");
        assert!(!runs_other_version(&home.join(VERSION).join("slp"), home));
        assert!(runs_other_version(&home.join("1.2.7").join("slp"), home));
        // A dev build serving this home is not ours to replace.
        assert!(!runs_other_version(Path::new("/opt/slp/bin/slp"), home));
    }

    #[test]
    fn an_agent_on_no_port_in_particular_is_ours() {
        if fixed_port().is_some() {
            return;
        }
        assert!(is_our_port(None));
        assert!(is_our_port(Some(0)));
        // Started by a CLI from before random ports.
        assert!(is_our_port(Some(DEFAULT_PORT)));
        assert!(!is_our_port(Some(9000)));
    }

    #[test]
    fn reads_the_cli_version_an_agent_was_served_by() {
        let args = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let served = args(&["/usr/local/bin/docent", SERVE_SUBCOMMAND, CLI_VERSION_FLAG, "1.0.34"]);
        assert_eq!(served_cli_version(&served), Some("1.0.34"));
        // Served without a version: never this build's.
        assert_eq!(served_cli_version(&args(&["smartloop", SERVE_SUBCOMMAND])), Some(""));
        assert_eq!(served_cli_version(&args(&["slp", "agent", "start"])), None);
    }

    /// A server that stalls halfway through the first response and serves
    /// the rest from the Range the retry asks for.
    #[test]
    fn download_resumes_after_a_stall() {
        let body: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/file.bin", listener.local_addr().unwrap());
        let served = body.clone();
        let server = std::thread::spawn(move || {
            let mut ranges = Vec::new();
            for (i, stream) in listener.incoming().take(2).enumerate() {
                let mut stream = stream.unwrap();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut range = None;
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).unwrap();
                    if line.trim().is_empty() {
                        break;
                    }
                    if let Some(v) = line.to_ascii_lowercase().strip_prefix("range: bytes=") {
                        range = v.trim().trim_end_matches('-').parse::<usize>().ok();
                    }
                }
                ranges.push(range);
                if i == 0 {
                    let head = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n", served.len());
                    stream.write_all(head.as_bytes()).unwrap();
                    stream.write_all(&served[..served.len() / 2]).unwrap();
                    stream.flush().unwrap();
                    std::thread::sleep(Duration::from_secs(3));
                } else {
                    let from = range.unwrap_or(0);
                    let head = format!(
                        "HTTP/1.1 206 Partial Content\r\nContent-Length: {}\r\nContent-Range: bytes {}-{}/{}\r\n\r\n",
                        served.len() - from,
                        from,
                        served.len() - 1,
                        served.len()
                    );
                    stream.write_all(head.as_bytes()).unwrap();
                    stream.write_all(&served[from..]).unwrap();
                }
            }
            ranges
        });

        let dir = std::env::temp_dir().join(format!("slp-download-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let dest = dir.join("file.bin");
        let mut list = Checklist::new();
        let step = list.add("Test file");
        let size = download_with(&url, &dest, &mut list, step, Duration::from_secs(1)).unwrap();

        assert_eq!(size, body.len() as u64);
        assert_eq!(fs::read(&dest).unwrap(), body);
        assert_eq!(server.join().unwrap(), vec![None, Some(body.len() / 2)]);
        let _ = fs::remove_dir_all(&dir);
    }
}
