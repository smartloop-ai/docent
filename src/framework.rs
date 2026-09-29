//! Install and run the SLP framework without the studio app.
//!
//! Mirrors studio-desktop's `slp-setup.js` fallback path: the framework is
//! downloaded from `https://dl.smartloop.ai/slp/<version>/<plat>-<arch>-slp.<ext>`
//! and extracted into `~/.smartloop/<version>/`, with the same marker files so
//! the studio app and the CLI share one install. The agent is then started as
//! a detached `slp agent start` with `SLP_HOME=~/.smartloop`, the way
//! `slp-service.js` launches it.

use std::fs;
use std::io::{BufRead, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use reqwest::blocking::Client;

use crate::fail;
use crate::progress::{Checklist, format_size};

/// SLP framework version this CLI installs and runs.
pub const VERSION: &str = "1.2.7";
const DEFAULT_PORT: u16 = 38540;
const DEFAULT_DOWNLOAD_URL: &str = "https://dl.smartloop.ai";
/// Embedding GGUF SLP loads for document search (AppSettings.embedding_gguf_file).
const EMBEDDING_FILE: &str = "bge-m3-Q4_K_M.gguf";
const START_TIMEOUT: Duration = Duration::from_secs(120);

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
/// it) or 38540.
pub fn port() -> u16 {
    std::env::var("SLP_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .filter(|&p| p > 0)
        .unwrap_or(DEFAULT_PORT)
}

fn binary_name() -> &'static str {
    if cfg!(windows) { "slp.exe" } else { "slp" }
}

pub fn binary_path() -> PathBuf {
    install_dir().join(VERSION).join(binary_name())
}

fn is_non_empty_file(path: &Path) -> bool {
    fs::metadata(path).map(|m| m.is_file() && m.len() > 0).unwrap_or(false)
}

/// Platform segment of the archive name; only the builds the CDN publishes.
fn platform() -> (&'static str, &'static str) {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => ("darwin", "arm64"),
        ("linux", "x86_64") => ("linux", "amd64"),
        ("windows", "x86_64") => ("windows", "amd64"),
        ("macos", _) => fail("Only Apple Silicon (arm64) is supported on macOS".to_string()),
        ("linux", _) => fail("Only x86_64 (amd64) is supported on Linux".to_string()),
        (os, arch) => fail(format!("Unsupported platform: {}-{}", os, arch)),
    }
}

fn archive_url() -> String {
    let base = std::env::var("SLP_BASE_URL").unwrap_or_else(|_| DEFAULT_DOWNLOAD_URL.to_string());
    let (plat, arch) = platform();
    let ext = if plat == "windows" { "zip" } else { "tar.gz" };
    format!("{}/slp/{}/{}-{}-slp.{}", base.trim_end_matches('/'), VERSION, plat, arch, ext)
}

/// Download and extract the framework unless this version is already present,
/// as one step on `list`. Returns the binary path.
fn ensure_installed(list: &mut Checklist) -> PathBuf {
    let binary = binary_path();
    if is_non_empty_file(&binary) {
        return binary;
    }

    let step = list.add(&format!("SLP framework {}", VERSION));
    list.start(step);

    let root = install_dir();
    let version_dir = root.join(VERSION);
    let cache_dir = root.join("cache");
    for dir in [&root, &cache_dir] {
        if let Err(e) = fs::create_dir_all(dir) {
            list.fail(step, format!("Failed to create {}: {}", dir.display(), e));
        }
    }
    // Start from a clean version dir so a half-finished earlier install
    // can't leave stale files next to the new ones.
    let _ = fs::remove_dir_all(&version_dir);
    if let Err(e) = fs::create_dir_all(&version_dir) {
        list.fail(step, format!("Failed to create {}: {}", version_dir.display(), e));
    }

    let url = archive_url();
    let file_name = url.rsplit('/').next().unwrap_or("slp-archive");
    let archive = cache_dir.join(format!("slp-{}-{}", VERSION, file_name));

    let size = download(&url, &archive, list, step).unwrap_or_else(|e| list.fail(step, e));

    list.note(step, "extracting…");
    let result = extract(&archive, &version_dir);
    let _ = fs::remove_file(&archive);
    if let Err(e) = result {
        let _ = fs::remove_dir_all(&version_dir);
        list.fail(step, e);
    }

    if !is_non_empty_file(&binary) {
        list.fail(
            step,
            format!(
                "Binary missing or empty at {} after install — the archive may be incomplete",
                binary.display()
            ),
        );
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&binary, fs::Permissions::from_mode(0o755));
    }

    // Strip quarantine so Gatekeeper doesn't block the freshly downloaded binary.
    if cfg!(target_os = "macos") {
        let _ = Command::new("xattr")
            .args(["-dr", "com.apple.quarantine"])
            .arg(&version_dir)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }

    list.note(step, "verifying…");
    let verified = Command::new(&binary)
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !verified {
        list.fail(
            step,
            format!("Installation verification failed: '{} --version' did not succeed", binary.display()),
        );
    }

    write_markers(&root);
    list.done(step, &format_size(size));
    binary
}

/// Same bookkeeping files slp-setup.js writes, so the studio app sees this
/// install as its own.
fn write_markers(root: &Path) {
    let _ = fs::write(root.join("version"), VERSION);
    let _ = fs::write(root.join(".installed-version"), VERSION);

    let installed = root.join("installed");
    let listed = fs::read_to_string(&installed).unwrap_or_default();
    if !listed.lines().any(|v| v.trim() == VERSION)
        && let Ok(mut file) = fs::OpenOptions::new().create(true).append(true).open(&installed)
    {
        let _ = writeln!(file, "{}", VERSION);
    }
}

/// Stream `url` to `dest` through a `.part` file renamed on success, so an
/// interrupted download never leaves a truncated file at the real path.
/// Progress goes to `step` on `list`. Returns the size downloaded.
fn download(url: &str, dest: &Path, list: &mut Checklist, step: usize) -> Result<u64, String> {
    // No overall timeout: the archive is hundreds of MB.
    let client = Client::builder()
        .timeout(None)
        .build()
        .map_err(|e| format!("Failed to build HTTP client: {}", e))?;
    let mut response = client
        .get(url)
        .send()
        .map_err(|e| format!("Failed to download {}: {}", url, e))?;
    if !response.status().is_success() {
        return Err(format!("Failed to download {}: {}", url, response.status()));
    }

    let total = response.content_length().unwrap_or(0);
    let partial = dest.with_file_name(format!(
        "{}.part",
        dest.file_name().and_then(|n| n.to_str()).unwrap_or("download")
    ));
    let mut file = fs::File::create(&partial)
        .map_err(|e| format!("Failed to create {}: {}", partial.display(), e))?;

    let mut buf = vec![0u8; 256 * 1024];
    let mut downloaded: u64 = 0;
    list.progress(step, 0, total);

    loop {
        let n = match response.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) => {
                let _ = fs::remove_file(&partial);
                return Err(format!("Download interrupted: {}", e));
            }
        };
        if let Err(e) = file.write_all(&buf[..n]) {
            let _ = fs::remove_file(&partial);
            return Err(format!("Failed to write {}: {}", partial.display(), e));
        }
        downloaded += n as u64;
        list.progress(step, downloaded, total);
    }
    drop(file);

    if total > 0 && downloaded != total {
        let _ = fs::remove_file(&partial);
        return Err(format!("Download of {} incomplete: {} of {} bytes", url, downloaded, total));
    }
    fs::rename(&partial, dest)
        .map_err(|e| format!("Failed to move download into {}: {}", dest.display(), e))?;
    Ok(downloaded)
}

/// Extract with the system `tar` (bsdtar on macOS and Windows 10+, which
/// also reads zip). The archive may wrap everything in a top-level `slp/`
/// directory; strip it so the binary lands at `<version_dir>/slp`.
fn extract(archive: &Path, dest: &Path) -> Result<(), String> {
    let gz = archive.extension().is_some_and(|e| e == "gz");

    let listing = Command::new("tar")
        .arg(if gz { "-tzf" } else { "-tf" })
        .arg(archive)
        .output()
        .map_err(|e| format!("Failed to run tar: {}", e))?;
    if !listing.status.success() {
        return Err(format!(
            "Failed to read archive: {}",
            String::from_utf8_lossy(&listing.stderr).trim()
        ));
    }
    let listing = String::from_utf8_lossy(&listing.stdout);
    let mut top_level = listing
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(|l| l.split('/').next().unwrap_or_default());
    let strip_wrapper = top_level.next() == Some("slp") && top_level.all(|t| t == "slp");

    let mut cmd = Command::new("tar");
    cmd.arg(if gz { "-xzf" } else { "-xf" }).arg(archive).arg("-C").arg(dest);
    if strip_wrapper {
        cmd.arg("--strip-components=1");
    }
    let out = cmd.output().map_err(|e| format!("Failed to run tar: {}", e))?;
    if !out.status.success() {
        return Err(format!(
            "Failed to extract archive: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(())
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
pub fn ensure_running(client: &Client, base_url: &str, local: bool) {
    let healthy = is_healthy(client, base_url);
    if !healthy && !local {
        fail(format!("No agent is running at {}", base_url));
    }
    let mut list = Checklist::new();
    if !healthy {
        launch(&mut list, client, base_url);
    }
    if local && needs_setup(client, base_url) {
        setup(&mut list, client, base_url);
    }
}

/// `smartloop agent start`: bring the local agent up and ready, or say it
/// already is.
pub fn start(client: &Client, base_url: &str) {
    if is_healthy(client, base_url) {
        eprintln!("Agent already running at {}", base_url);
    }
    ensure_running(client, base_url, true);
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
fn setup(list: &mut Checklist, client: &Client, base_url: &str) {
    let embeddings = list.add("Embeddings (bge-m3)");
    let model = list.add("Chat model");
    let project = list.add("Default project");
    let load = list.add("Load model");
    let services = list.add("Skills and connections");

    // A plain read that creates the workspace when there isn't one.
    list.start(embeddings);
    let _ = client.get(format!("{}/v1/models/workspace", base_url)).send();
    match workspace_dir() {
        Some(workspace) => ensure_embeddings(list, embeddings, &workspace),
        // The agent fetches it itself the first time it indexes.
        None => list.done(embeddings, "on first index"),
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
fn ensure_embeddings(list: &mut Checklist, step: usize, workspace: &Path) {
    let file = std::env::var("SLP_EMBEDDING_GGUF_FILE").unwrap_or_else(|_| EMBEDDING_FILE.to_string());
    let dir = workspace.join("models").join("embeddings");
    let target = dir.join(&file);
    if is_non_empty_file(&target) {
        list.done(step, "downloaded");
        return;
    }
    if let Err(e) = fs::create_dir_all(&dir) {
        list.fail(step, format!("Failed to create {}: {}", dir.display(), e));
    }

    let base = std::env::var("SLP_EMBEDDING_GGUF_BASE_URL")
        .unwrap_or_else(|_| format!("{}/embeddings", DEFAULT_DOWNLOAD_URL));
    let url = format!("{}/{}", base.trim_end_matches('/'), file);
    let size = download(&url, &target, list, step).unwrap_or_else(|e| list.fail(step, e));
    list.done(step, &format_size(size));
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
    list: &mut Checklist,
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
            "; sign in with `smartloop login`"
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
            list.set_label(download, &format!("Chat model {}", name));
        }

        match stage(status) {
            Stage::Start(step) => {
                list.finish_before(step);
                list.start(step);
                current = step;
            }
            Stage::Done(step) => {
                list.finish_before(step);
                let total: u64 = files.values().map(|(_, t)| t).sum();
                let detail = if step == download && total > 0 { format_size(total) } else { String::new() };
                list.done(step, &detail);
            }
            Stage::Present(step) => {
                list.finish_before(step);
                list.done(step, "downloaded");
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

/// Install the framework if needed and start `slp agent start` detached from
/// this process, logging to `~/.smartloop/server.log`, then wait until
/// `/health` answers.
fn launch(list: &mut Checklist, client: &Client, base_url: &str) {
    let binary = ensure_installed(list);
    let step = list.add("Start agent");
    list.start(step);

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
    cmd.args(["agent", "start", "--port", &port().to_string()])
        .env("SLP_HOME", &home)
        .current_dir(&home)
        .stdin(Stdio::null())
        .stdout(log)
        .stderr(log_err);
    detach(&mut cmd);

    // Never waited on once healthy: the agent outlives the CLI on purpose.
    #[allow(clippy::zombie_processes)]
    let mut child = cmd
        .spawn()
        .unwrap_or_else(|e| list.fail(step, format!("Failed to start {}: {}", binary.display(), e)));

    let running = format!("port {}", port());
    let started = Instant::now();
    while started.elapsed() < START_TIMEOUT {
        if is_healthy(client, base_url) {
            list.done(step, &running);
            return;
        }
        if let Ok(Some(status)) = child.try_wait() {
            // Lost a race for the port: the studio app (or another CLI)
            // started an agent at the same moment, so ours could not bind.
            // Theirs coming up is as good as ours. A crash with the port
            // free fails right away.
            let waiting = Instant::now();
            while port_in_use() && waiting.elapsed() < START_TIMEOUT {
                if is_healthy(client, base_url) {
                    list.done(step, &running);
                    return;
                }
                std::thread::sleep(Duration::from_secs(1));
            }
            list.fail(
                step,
                format!("Agent exited before becoming healthy ({}); see {}", status, log_path.display()),
            );
        }
        std::thread::sleep(Duration::from_secs(1));
    }
    list.fail(step, format!("Timed out waiting for the agent to start; see {}", log_path.display()));
}

/// Whether something is listening on the agent port, healthy or not.
fn port_in_use() -> bool {
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port()));
    std::net::TcpStream::connect_timeout(&addr, Duration::from_secs(1)).is_ok()
}

/// Stop the agent through the framework's own `slp agent stop`.
pub fn stop() {
    let binary = binary_path();
    if !is_non_empty_file(&binary) {
        fail(format!("SLP framework {} is not installed", VERSION));
    }
    let status = Command::new(&binary)
        .args(["agent", "stop"])
        .env("SLP_HOME", install_dir())
        .current_dir(install_dir())
        .status()
        .unwrap_or_else(|e| fail(format!("Failed to run {}: {}", binary.display(), e)));
    if !status.success() {
        fail(format!("`slp agent stop` exited with {}", status));
    }
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
