//! Install and run the SLP framework without the studio app.
//!
//! Mirrors studio-desktop's `slp-setup.js` fallback path: the framework is
//! downloaded from `https://dl.smartloop.ai/slp/<version>/<plat>-<arch>-slp.<ext>`
//! and extracted into `~/.smartloop/<version>/`, with the same marker files so
//! the studio app and the CLI share one install. The agent is then started as
//! a detached `slp agent start` with `SLP_HOME=~/.smartloop`, the way
//! `slp-service.js` launches it.

use std::fs;
use std::io::{BufRead, IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use reqwest::blocking::Client;

use crate::fail;

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

/// Download and extract the framework unless this version is already present.
/// Returns the binary path.
pub fn ensure_installed() -> PathBuf {
    let binary = binary_path();
    if is_non_empty_file(&binary) {
        return binary;
    }

    let root = install_dir();
    let version_dir = root.join(VERSION);
    let cache_dir = root.join("cache");
    for dir in [&root, &cache_dir] {
        fs::create_dir_all(dir)
            .unwrap_or_else(|e| fail(format!("Failed to create {}: {}", dir.display(), e)));
    }
    // Start from a clean version dir so a half-finished earlier install
    // can't leave stale files next to the new ones.
    let _ = fs::remove_dir_all(&version_dir);
    fs::create_dir_all(&version_dir)
        .unwrap_or_else(|e| fail(format!("Failed to create {}: {}", version_dir.display(), e)));

    let url = archive_url();
    let file_name = url.rsplit('/').next().unwrap_or("slp-archive");
    let archive = cache_dir.join(format!("slp-{}-{}", VERSION, file_name));

    eprintln!("Installing SLP framework {} into {}", VERSION, version_dir.display());
    download(&url, &archive, "Downloading framework");

    eprintln!("Extracting...");
    let result = extract(&archive, &version_dir);
    let _ = fs::remove_file(&archive);
    if let Err(e) = result {
        let _ = fs::remove_dir_all(&version_dir);
        fail(e);
    }

    if !is_non_empty_file(&binary) {
        fail(format!(
            "Binary missing or empty at {} after install — the archive may be incomplete",
            binary.display()
        ));
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

    let verified = Command::new(&binary)
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !verified {
        fail(format!("Installation verification failed: '{} --version' did not succeed", binary.display()));
    }

    write_markers(&root);
    eprintln!("Installed SLP framework {}", VERSION);
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
fn download(url: &str, dest: &Path, label: &str) {
    // No overall timeout: the archive is hundreds of MB.
    let client = Client::builder()
        .timeout(None)
        .build()
        .unwrap_or_else(|e| fail(format!("Failed to build HTTP client: {}", e)));
    let mut response = client
        .get(url)
        .send()
        .unwrap_or_else(|e| fail(format!("Failed to download {}: {}", url, e)));
    if !response.status().is_success() {
        fail(format!("Failed to download {}: {}", url, response.status()));
    }

    let total = response.content_length().unwrap_or(0);
    let partial = dest.with_file_name(format!(
        "{}.part",
        dest.file_name().and_then(|n| n.to_str()).unwrap_or("download")
    ));
    let mut file = fs::File::create(&partial)
        .unwrap_or_else(|e| fail(format!("Failed to create {}: {}", partial.display(), e)));

    let show_progress = std::io::stderr().is_terminal();
    let mut buf = vec![0u8; 256 * 1024];
    let mut downloaded: u64 = 0;
    let mut last_report = Instant::now();

    loop {
        let n = match response.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) => {
                let _ = fs::remove_file(&partial);
                fail(format!("Download interrupted: {}", e));
            }
        };
        if let Err(e) = file.write_all(&buf[..n]) {
            let _ = fs::remove_file(&partial);
            fail(format!("Failed to write {}: {}", partial.display(), e));
        }
        downloaded += n as u64;

        if show_progress && last_report.elapsed() >= Duration::from_millis(200) {
            last_report = Instant::now();
            report_progress(label, downloaded, total);
        }
    }
    drop(file);

    if total > 0 && downloaded != total {
        let _ = fs::remove_file(&partial);
        fail(format!("Download of {} incomplete: {} of {} bytes", url, downloaded, total));
    }
    if show_progress {
        report_progress(label, downloaded, total);
        eprintln!();
    }
    fs::rename(&partial, dest)
        .unwrap_or_else(|e| fail(format!("Failed to move download into {}: {}", dest.display(), e)));
}

fn report_progress(label: &str, downloaded: u64, total: u64) {
    let mb = |b: u64| b as f64 / (1024.0 * 1024.0);
    if total > 0 {
        eprint!(
            "\r{}... {:.0}/{:.0} MB ({}%)",
            label,
            mb(downloaded),
            mb(total),
            downloaded * 100 / total
        );
    } else {
        eprint!("\r{}... {:.0} MB", label, mb(downloaded));
    }
    let _ = std::io::stderr().flush();
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
    if !is_healthy(client, base_url) {
        if !local {
            fail(format!("No agent is running at {}", base_url));
        }
        start(client, base_url);
    }
    if local && needs_setup(client, base_url) {
        setup(client, base_url);
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
pub fn setup(client: &Client, base_url: &str) {
    // A plain read that creates the workspace when there isn't one.
    let _ = client.get(format!("{}/v1/models/workspace", base_url)).send();

    match workspace_dir() {
        Some(workspace) => ensure_embeddings(&workspace),
        None => eprintln!("No workspace found yet; the agent will fetch embeddings on first index"),
    }

    bootstrap(base_url);
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
fn ensure_embeddings(workspace: &Path) {
    let file = std::env::var("SLP_EMBEDDING_GGUF_FILE").unwrap_or_else(|_| EMBEDDING_FILE.to_string());
    let dir = workspace.join("models").join("embeddings");
    let target = dir.join(&file);
    if is_non_empty_file(&target) {
        return;
    }
    fs::create_dir_all(&dir)
        .unwrap_or_else(|e| fail(format!("Failed to create {}: {}", dir.display(), e)));

    let base = std::env::var("SLP_EMBEDDING_GGUF_BASE_URL")
        .unwrap_or_else(|_| format!("{}/embeddings", DEFAULT_DOWNLOAD_URL));
    download(
        &format!("{}/{}", base.trim_end_matches('/'), file),
        &target,
        "Downloading embeddings",
    );
}

/// Run `POST /v1/bootstrap` and print its SSE progress. Downloads the default
/// chat model when the workspace has none, which can take minutes, so the
/// request has no timeout.
fn bootstrap(base_url: &str) {
    let client = Client::builder()
        .timeout(None)
        .build()
        .unwrap_or_else(|e| fail(format!("Failed to build HTTP client: {}", e)));
    let response = client
        .post(format!("{}/v1/bootstrap", base_url))
        .json(&serde_json::json!({}))
        .send()
        .unwrap_or_else(|e| fail(format!("Failed to bootstrap: {}", e)));
    if !response.status().is_success() {
        fail(format!("Failed to bootstrap: {}", response.status()));
    }

    let show_progress = std::io::stderr().is_terminal();
    let mut event = String::new();
    let mut on_progress_line = false;
    let mut last_message = String::new();

    for line in std::io::BufReader::new(response).lines() {
        let line = line.unwrap_or_else(|e| fail(format!("Bootstrap stream dropped: {}", e)));
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
            if on_progress_line {
                eprintln!();
            }
            fail(format!(
                "Setup failed: {}",
                data["message"].as_str().unwrap_or("bootstrap failed")
            ));
        }

        let downloaded = data["downloaded"].as_u64();
        let total = data["total"].as_u64().unwrap_or(0);
        if let (Some(downloaded), true) = (downloaded, total > 0) {
            if show_progress {
                let name = data["filename"].as_str().unwrap_or("model");
                report_progress(&format!("Downloading {}", name), downloaded, total);
                on_progress_line = true;
            }
            continue;
        }

        let message = data["message"].as_str().unwrap_or_default().trim().to_string();
        if message.is_empty() || message == last_message {
            continue;
        }
        if on_progress_line {
            eprintln!();
            on_progress_line = false;
        }
        eprintln!("{}", message);
        last_message = message;
    }
    if on_progress_line {
        eprintln!();
    }
}

/// Start `slp agent start` detached from this process, logging to
/// `~/.smartloop/server.log`, and wait until `/health` answers.
pub fn start(client: &Client, base_url: &str) {
    if is_healthy(client, base_url) {
        eprintln!("Agent already running at {}", base_url);
        return;
    }

    let binary = ensure_installed();
    let home = install_dir();
    let log_path = home.join("server.log");
    let mut log = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .unwrap_or_else(|e| fail(format!("Failed to open {}: {}", log_path.display(), e)));
    let _ = writeln!(log, "\n--- Service starting from smartloop CLI (pid {}) ---", std::process::id());
    let log_err = log
        .try_clone()
        .unwrap_or_else(|e| fail(format!("Failed to open {}: {}", log_path.display(), e)));

    let mut cmd = Command::new(&binary);
    cmd.args(["agent", "start", "--port", &port().to_string()])
        .env("SLP_HOME", &home)
        .current_dir(&home)
        .stdin(Stdio::null())
        .stdout(log)
        .stderr(log_err);
    detach(&mut cmd);

    eprintln!("Starting agent...");
    // Never waited on once healthy: the agent outlives the CLI on purpose.
    #[allow(clippy::zombie_processes)]
    let mut child = cmd
        .spawn()
        .unwrap_or_else(|e| fail(format!("Failed to start {}: {}", binary.display(), e)));

    let started = Instant::now();
    while started.elapsed() < START_TIMEOUT {
        if is_healthy(client, base_url) {
            eprintln!("Agent running at {}", base_url);
            return;
        }
        if let Ok(Some(status)) = child.try_wait() {
            // Lost a race for the port: the studio app (or another CLI)
            // started an agent at the same moment, so ours could not bind.
            // Theirs coming up is as good as ours.
            // A crash with the port free fails right away.
            let waiting = Instant::now();
            while port_in_use() && waiting.elapsed() < START_TIMEOUT {
                if is_healthy(client, base_url) {
                    eprintln!("Agent running at {}", base_url);
                    return;
                }
                std::thread::sleep(Duration::from_secs(1));
            }
            fail(format!(
                "Agent exited before becoming healthy ({}); see {}",
                status,
                log_path.display()
            ));
        }
        std::thread::sleep(Duration::from_secs(1));
    }
    fail(format!("Timed out waiting for the agent to start; see {}", log_path.display()));
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
