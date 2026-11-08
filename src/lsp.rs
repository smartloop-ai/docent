//! Language servers, as opencode uses them: after a reply's files are
//! written, each goes to the server for its language, when one is on the
//! PATH (rust-analyzer, gopls, typescript-language-server, pyright, clangd),
//! and the errors it reports go back to the agent to fix. A server is
//! started once per project folder and language, and kept for the session.
//!
//! The protocol is JSON-RPC over the server's stdin and stdout, each
//! message headed by its `Content-Length`; only the little docent needs
//! is spoken: `initialize`, opening, changing and saving a file, and the
//! diagnostics the server publishes.

use serde_json::{Value, json};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

/// A language server docent can start: its name, the command and its
/// arguments, and the file extensions it serves.
pub struct Spec {
    pub name: &'static str,
    command: &'static str,
    args: &'static [&'static str],
    exts: &'static [&'static str],
}

/// The servers docent knows, in order: for a language with two, the
/// first on the PATH serves it.
const SPECS: [Spec; 6] = [
    Spec { name: "rust-analyzer", command: "rust-analyzer", args: &[], exts: &["rs"] },
    Spec { name: "gopls", command: "gopls", args: &[], exts: &["go"] },
    Spec {
        name: "typescript-language-server",
        command: "typescript-language-server",
        args: &["--stdio"],
        exts: &["ts", "tsx", "js", "jsx", "mjs", "cjs", "mts", "cts"],
    },
    Spec { name: "pyright", command: "pyright-langserver", args: &["--stdio"], exts: &["py", "pyi"] },
    Spec { name: "pylsp", command: "pylsp", args: &[], exts: &["py", "pyi"] },
    Spec { name: "clangd", command: "clangd", args: &[], exts: &["c", "h", "cc", "cpp", "cxx", "hpp", "hh", "hxx"] },
];

/// How long a server has to answer `initialize`: it may index first.
const START: Duration = Duration::from_secs(30);
/// How long to wait for a file's diagnostics after it's changed.
const WAIT: Duration = Duration::from_secs(8);
/// Once each file has diagnostics, how long the server may go quiet
/// before they're taken as final: some publish twice, quick ones first.
const SETTLE: Duration = Duration::from_millis(400);

/// The server for the file at `path`, by its extension, when one is on
/// the PATH.
pub fn spec_for(path: &str) -> Option<&'static Spec> {
    let ext = Path::new(path).extension()?.to_str()?.to_lowercase();
    SPECS.iter().find(|s| s.exts.contains(&ext.as_str()) && crate::files::on_path(s.command))
}

/// The server a project of `kind` would use, to start ahead of the
/// files it will be sent.
pub fn spec_for_kind(kind: &str) -> Option<&'static Spec> {
    let ext = match kind {
        "Rust" => "rs",
        "Go" => "go",
        "JavaScript/TypeScript" => "ts",
        "Python" => "py",
        "C/C++" => "c",
        _ => return None,
    };
    spec_for(&format!("x.{ext}"))
}

/// The language a file is, as the protocol names it.
fn language_id(path: &str) -> &'static str {
    match Path::new(path).extension().and_then(|e| e.to_str()).unwrap_or("") {
        "rs" => "rust",
        "go" => "go",
        "ts" | "mts" | "cts" => "typescript",
        "tsx" => "typescriptreact",
        "js" | "mjs" | "cjs" => "javascript",
        "jsx" => "javascriptreact",
        "py" | "pyi" => "python",
        "c" | "h" => "c",
        _ => "cpp",
    }
}

/// An error a server reports in a file, by its path relative to the
/// project, and its line and column from 1.
#[derive(Debug, Clone, PartialEq)]
pub struct Problem {
    pub path: String,
    pub line: u64,
    pub column: u64,
    pub message: String,
}

impl std::fmt::Display for Problem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}:{}: {}", self.path, self.line, self.column, self.message)
    }
}

/// The servers started this session, by project folder and name; `None`
/// for one that wouldn't start, so it isn't tried again.
#[derive(Default)]
pub struct Lsp {
    servers: Mutex<HashMap<(PathBuf, &'static str), Started>>,
}

/// A server as it was started: none when it wouldn't.
type Started = Option<Arc<Server>>;

impl Lsp {
    /// Start the server for a project of `kind` at `root`, so it's ready
    /// by the time the reply's files are written.
    pub fn warm(&self, root: &Path, kind: &str) {
        if let Some(spec) = spec_for_kind(kind) {
            self.server(root, spec);
        }
    }

    /// The errors the servers report in `paths`, relative to `root`, as
    /// they are on disk now; and the servers asked. Files no server on
    /// the PATH serves are left out.
    pub fn errors(&self, root: &Path, paths: &[String]) -> (Vec<&'static str>, Vec<Problem>) {
        let mut by_server: Vec<(&'static Spec, Vec<&String>)> = Vec::new();
        for path in paths {
            let Some(spec) = spec_for(path) else { continue };
            match by_server.iter_mut().find(|(s, _)| s.name == spec.name) {
                Some((_, list)) => list.push(path),
                None => by_server.push((spec, vec![path])),
            }
        }
        let (mut asked, mut problems) = (Vec::new(), Vec::new());
        for (spec, paths) in by_server {
            let Some(server) = self.server(root, spec) else { continue };
            asked.push(spec.name);
            problems.extend(server.errors(&paths));
        }
        (asked, problems)
    }

    /// Whether a server on the PATH serves any of `paths`.
    pub fn serves(paths: &[String]) -> bool {
        paths.iter().any(|p| spec_for(p).is_some())
    }

    fn server(&self, root: &Path, spec: &'static Spec) -> Option<Arc<Server>> {
        let root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
        let mut servers = self.servers.lock().unwrap();
        // One that's ended is started again.
        if let Some(Some(server)) = servers.get(&(root.clone(), spec.name))
            && server.state.0.lock().unwrap().ended
        {
            servers.remove(&(root.clone(), spec.name));
        }
        servers.entry((root.clone(), spec.name)).or_insert_with(|| Server::start(spec, &root).map(Arc::new)).clone()
    }
}

/// What the server's sent: the diagnostics for each file, by path, with
/// when they came; the answers to requests, by id; and whether it's ended.
#[derive(Default)]
struct State {
    diagnostics: HashMap<PathBuf, (u64, Vec<Value>)>,
    answers: HashMap<i64, Value>,
    published: u64,
    ended: bool,
}

/// A running language server for one project folder.
struct Server {
    root: PathBuf,
    child: Mutex<Child>,
    stdin: Arc<Mutex<ChildStdin>>,
    state: Arc<(Mutex<State>, Condvar)>,
    next_id: Mutex<i64>,
    /// Each file opened in the server, by path, with its version.
    versions: Mutex<HashMap<PathBuf, i64>>,
}

impl Server {
    fn start(spec: &Spec, root: &Path) -> Option<Server> {
        let mut child = Command::new(spec.command)
            .args(spec.args)
            .current_dir(root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .ok()?;
        let stdin = Arc::new(Mutex::new(child.stdin.take()?));
        let stdout = child.stdout.take()?;
        let state: Arc<(Mutex<State>, Condvar)> = Arc::default();
        {
            let (state, stdin) = (state.clone(), stdin.clone());
            std::thread::spawn(move || read_messages(stdout, &state, &stdin));
        }
        let server = Server {
            root: root.to_path_buf(),
            child: Mutex::new(child),
            stdin,
            state,
            next_id: Mutex::new(0),
            versions: Mutex::default(),
        };
        let uri = file_uri(root);
        let name = root.file_name().and_then(|n| n.to_str()).unwrap_or("project");
        let params = json!({
            "processId": std::process::id(),
            "rootUri": uri,
            "rootPath": root,
            "workspaceFolders": [{ "uri": uri, "name": name }],
            // rust-analyzer runs `cargo check` on save unless told not to:
            // docent's own check does that after, and two would contend.
            "initializationOptions": if spec.name == "rust-analyzer" { json!({ "checkOnSave": false }) } else { json!({}) },
            "capabilities": {
                "textDocument": {
                    "synchronization": { "didSave": true, "dynamicRegistration": false },
                    "publishDiagnostics": { "versionSupport": false },
                },
                "workspace": { "configuration": true, "workspaceFolders": true },
            },
        });
        server.request("initialize", params, START)?;
        server.notify("initialized", json!({}));
        Some(server)
    }

    fn send(&self, message: &Value) {
        let body = message.to_string();
        let mut stdin = self.stdin.lock().unwrap();
        let _ = write!(stdin, "Content-Length: {}\r\n\r\n{}", body.len(), body);
        let _ = stdin.flush();
    }

    fn notify(&self, method: &str, params: Value) {
        self.send(&json!({ "jsonrpc": "2.0", "method": method, "params": params }));
    }

    /// Ask the server `method` and wait for its answer, up to `wait`.
    fn request(&self, method: &str, params: Value, wait: Duration) -> Option<Value> {
        let id = {
            let mut next = self.next_id.lock().unwrap();
            *next += 1;
            *next
        };
        self.send(&json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }));
        let (lock, ready) = &*self.state;
        let deadline = Instant::now() + wait;
        let mut state = lock.lock().unwrap();
        loop {
            if let Some(answer) = state.answers.remove(&id) {
                return answer.get("error").is_none().then_some(answer);
            }
            let left = deadline.checked_duration_since(Instant::now())?;
            if state.ended {
                return None;
            }
            state = ready.wait_timeout(state, left).unwrap().0;
        }
    }

    /// Open `path` in the server with its contents on disk, or tell it
    /// they've changed, and that it's saved.
    fn touch(&self, path: &Path) {
        let Ok(text) = std::fs::read_to_string(path) else { return };
        let uri = file_uri(path);
        let version = {
            let mut versions = self.versions.lock().unwrap();
            let version = versions.entry(path.to_path_buf()).or_insert(0);
            *version += 1;
            *version
        };
        if version == 1 {
            let document = json!({ "uri": uri, "languageId": language_id(&path.to_string_lossy()), "version": 1, "text": text });
            self.notify("textDocument/didOpen", json!({ "textDocument": document }));
        } else {
            self.notify(
                "textDocument/didChange",
                json!({ "textDocument": { "uri": uri, "version": version }, "contentChanges": [{ "text": text }] }),
            );
        }
        self.notify("textDocument/didSave", json!({ "textDocument": { "uri": uri }, "text": text }));
    }

    /// The errors the server reports in `paths`, relative to its root,
    /// once they've changed: each file's diagnostics after it's touched,
    /// and then until the server's gone quiet a moment.
    fn errors(&self, paths: &[&String]) -> Vec<Problem> {
        let files: Vec<PathBuf> = paths.iter().map(|p| self.root.join(p)).collect();
        let since = self.state.0.lock().unwrap().published;
        for file in &files {
            self.touch(file);
        }
        let (lock, ready) = &*self.state;
        let deadline = Instant::now() + WAIT;
        let mut state = lock.lock().unwrap();
        let mut quiet_from: Option<(Instant, u64)> = None;
        loop {
            let now = Instant::now();
            let all_in = files.iter().all(|f| state.diagnostics.get(f).is_some_and(|(at, _)| *at > since));
            if all_in {
                match quiet_from {
                    Some((at, seen)) if seen == state.published && now >= at + SETTLE => break,
                    Some((_, seen)) if seen == state.published => {}
                    _ => quiet_from = Some((now, state.published)),
                }
            }
            if now >= deadline || state.ended {
                break;
            }
            let until = quiet_from.map_or(deadline, |(at, _)| (at + SETTLE).min(deadline));
            state = ready.wait_timeout(state, until.saturating_duration_since(now).max(Duration::from_millis(10))).unwrap().0;
        }
        let mut problems = Vec::new();
        for (file, path) in files.iter().zip(paths) {
            let Some((_, list)) = state.diagnostics.get(file) else { continue };
            problems.extend(list.iter().filter_map(|d| problem(path, d)));
        }
        problems
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        if let Ok(mut child) = self.child.lock() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// An error among a file's diagnostics, `path` being the file's.
fn problem(path: &str, diagnostic: &Value) -> Option<Problem> {
    // Severity 1 is an error; a warning or a hint isn't the agent's to fix.
    if diagnostic.get("severity").and_then(Value::as_u64).unwrap_or(1) != 1 {
        return None;
    }
    let start = &diagnostic["range"]["start"];
    let message = diagnostic["message"].as_str()?.lines().next().unwrap_or("").trim().to_string();
    let source = diagnostic.get("source").and_then(Value::as_str);
    Some(Problem {
        path: path.to_string(),
        line: start["line"].as_u64().unwrap_or(0) + 1,
        column: start["character"].as_u64().unwrap_or(0) + 1,
        message: match source {
            Some(source) => format!("{message} ({source})"),
            None => message,
        },
    })
}

/// Read the server's messages until it ends: diagnostics and answers go
/// into `state`; a request of its own is answered at once, with nothing
/// set (a configuration asked for is each left as the server's default).
fn read_messages(stdout: impl Read, state: &(Mutex<State>, Condvar), stdin: &Mutex<ChildStdin>) {
    let mut reader = BufReader::new(stdout);
    while let Some(message) = read_message(&mut reader) {
        let method = message.get("method").and_then(Value::as_str);
        match (method, message.get("id")) {
            (Some(method), Some(id)) => {
                let result = match method {
                    "workspace/configuration" => {
                        let items = message["params"]["items"].as_array().map_or(0, Vec::len);
                        Value::Array(vec![Value::Null; items])
                    }
                    _ => Value::Null,
                };
                let body = json!({ "jsonrpc": "2.0", "id": id, "result": result }).to_string();
                let mut stdin = stdin.lock().unwrap();
                let _ = write!(stdin, "Content-Length: {}\r\n\r\n{}", body.len(), body);
                let _ = stdin.flush();
            }
            (Some("textDocument/publishDiagnostics"), None) => {
                let params = &message["params"];
                let Some(path) = params["uri"].as_str().and_then(uri_path) else { continue };
                let list = params["diagnostics"].as_array().cloned().unwrap_or_default();
                let mut held = state.0.lock().unwrap();
                held.published += 1;
                let at = held.published;
                held.diagnostics.insert(path, (at, list));
                drop(held);
                state.1.notify_all();
            }
            (None, Some(id)) => {
                if let Some(id) = id.as_i64() {
                    state.0.lock().unwrap().answers.insert(id, message);
                    state.1.notify_all();
                }
            }
            _ => {}
        }
    }
    state.0.lock().unwrap().ended = true;
    state.1.notify_all();
}

/// One message from `reader`: headers to a blank line, then a body of
/// `Content-Length` bytes. None once the stream ends.
fn read_message(reader: &mut impl BufRead) -> Option<Value> {
    loop {
        let mut length = None;
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line).ok()? == 0 {
                return None;
            }
            let line = line.trim_end();
            if line.is_empty() {
                break;
            }
            if let Some((name, value)) = line.split_once(':')
                && name.eq_ignore_ascii_case("content-length")
            {
                length = value.trim().parse::<usize>().ok();
            }
        }
        let Some(length) = length else { continue };
        let mut body = vec![0; length];
        reader.read_exact(&mut body).ok()?;
        if let Ok(message) = serde_json::from_slice(&body) {
            return Some(message);
        }
    }
}

/// `path`, absolute, as a `file://` URI, its bytes other than those safe
/// in a path percent-encoded.
fn file_uri(path: &Path) -> String {
    let mut uri = String::from("file://");
    for byte in path.to_string_lossy().bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'/' | b'-' | b'_' | b'.' | b'~' => uri.push(byte as char),
            _ => uri.push_str(&format!("%{byte:02X}")),
        }
    }
    uri
}

/// The path a `file://` URI names, decoded.
fn uri_path(uri: &str) -> Option<PathBuf> {
    let rest = uri.strip_prefix("file://")?;
    let bytes = rest.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && let Some(byte) = rest.get(i + 1..i + 3).and_then(|h| u8::from_str_radix(h, 16).ok())
        {
            out.push(byte);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    Some(PathBuf::from(String::from_utf8(out).ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_path_goes_to_a_uri_and_back() {
        let path = Path::new("/tmp/my app/src/a+b.rs");
        let uri = file_uri(path);
        assert_eq!(uri, "file:///tmp/my%20app/src/a%2Bb.rs");
        assert_eq!(uri_path(&uri).unwrap(), path);
        // As a server may write it, unencoded or encoded otherwise.
        assert_eq!(uri_path("file:///tmp/my app/x.rs").unwrap(), Path::new("/tmp/my app/x.rs"));
        assert_eq!(uri_path("file:///c%3A/x.rs").unwrap(), Path::new("/c:/x.rs"));
    }

    #[test]
    fn messages_are_read_by_their_length() {
        let a = r#"{"jsonrpc":"2.0","id":1,"result":{}}"#;
        let b = r#"{"jsonrpc":"2.0","method":"x"}"#;
        let stream = format!("Content-Length: {}\r\nContent-Type: x\r\n\r\n{}Content-Length: {}\r\n\r\n{}", a.len(), a, b.len(), b);
        let mut reader = BufReader::new(stream.as_bytes());
        assert_eq!(read_message(&mut reader).unwrap()["id"], 1);
        assert_eq!(read_message(&mut reader).unwrap()["method"], "x");
        assert!(read_message(&mut reader).is_none());
    }

    #[test]
    fn only_errors_are_problems() {
        let error = json!({ "range": { "start": { "line": 2, "character": 4 } }, "severity": 1, "message": "mismatched types\nexpected i32", "source": "rustc" });
        let warning = json!({ "range": { "start": { "line": 0, "character": 0 } }, "severity": 2, "message": "unused" });
        assert_eq!(problem("src/main.rs", &error).unwrap().to_string(), "src/main.rs:3:5: mismatched types (rustc)");
        assert!(problem("src/main.rs", &warning).is_none());
    }

    /// Against clangd itself, when it's installed: an error in a file
    /// written is reported, and once it's fixed, it's gone.
    #[test]
    #[ignore = "starts clangd"]
    fn clangd_reports_an_error_in_a_file_written_and_drops_it_once_fixed() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("main.c"), "int main(void) {\n    return undefined_thing;\n}\n").unwrap();
        let lsp = Lsp::default();
        let files = ["main.c".to_string()];
        let (asked, problems) = lsp.errors(dir.path(), &files);
        assert_eq!(asked, ["clangd"]);
        assert!(problems.iter().any(|p| p.line == 2 && p.message.contains("undefined_thing")), "{problems:?}");
        std::fs::write(dir.path().join("main.c"), "int main(void) {\n    return 0;\n}\n").unwrap();
        let (_, problems) = lsp.errors(dir.path(), &files);
        assert!(problems.is_empty(), "{problems:?}");
    }

    /// Against rust-analyzer itself, when it's installed: a type error in
    /// a file written is reported.
    #[test]
    #[ignore = "starts rust-analyzer"]
    fn rust_analyzer_reports_an_error_in_a_file_written() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Cargo.toml"), "[package]\nname = \"x\"\nversion = \"0.1.0\"\nedition = \"2021\"\n").unwrap();
        std::fs::create_dir(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/main.rs"), "fn main() {\n    let x: i32 = \"no\";\n    undefined_fn();\n}\n").unwrap();
        let lsp = Lsp::default();
        let (asked, problems) = lsp.errors(dir.path(), &["src/main.rs".to_string()]);
        assert_eq!(asked, ["rust-analyzer"]);
        assert!(!problems.is_empty(), "{problems:?}");
        assert!(problems.iter().all(|p| p.path == "src/main.rs"), "{problems:?}");
    }
}
