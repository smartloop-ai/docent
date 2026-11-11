//! Chat turns streamed from the agent's SSE endpoint, reported as events for
//! the TUI to show.

use std::time::{Duration, Instant};

use async_openai::{Client as OpenAIClient, config::OpenAIConfig};
use futures_util::StreamExt;
use serde::Deserialize;

/// What kind of source a [`Citation`] names, and so what opening it does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CitationSource {
    /// A file on this machine, opened from `document_path` or `path`.
    Document,
    /// A web page, opened from `url`.
    Web,
    /// A connected app's tool call; nothing to open.
    Mcp,
}

/// A source an answer drew on, as the agent's `chat.citations` event sends
/// it.  Docent's own type, not the smartloop crate's: the reply comes over
/// HTTP, so reading it needn't wait for a smartloop release that carries a
/// new field (a field this agent doesn't send yet reads as absent), and an
/// older agent's citations still read.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct Citation {
    /// Absent from an older agent's citation of a document by name alone.
    #[serde(default)]
    pub document_id: String,
    #[serde(default)]
    pub document_name: Option<String>,
    /// What kind of source this is; `None` for one docent doesn't know.
    #[serde(default, deserialize_with = "known_source")]
    pub source: Option<CitationSource>,
    /// Where a web citation opens.
    #[serde(default)]
    pub url: Option<String>,
    /// A document's file, relative to its project (`documents/invoice.pdf`).
    #[serde(default)]
    pub document_path: Option<String>,
    /// The pages of a document the answer drew on.
    #[serde(default)]
    pub page_numbers: Option<Vec<serde_json::Value>>,
    /// An explored file's absolute path.
    #[serde(default)]
    pub path: Option<String>,
    /// The connected app an MCP citation's tool belongs to.
    #[serde(default)]
    pub server_name: Option<String>,
    /// The tool an MCP citation names.
    #[serde(default)]
    pub tool_name: Option<String>,
}

/// A citation's `source`, read leniently: a kind docent doesn't know reads
/// as none rather than failing the whole event.
fn known_source<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<Option<CitationSource>, D::Error> {
    Ok(Option::<String>::deserialize(deserializer)?.as_deref().and_then(|s| match s {
        "document" => Some(CitationSource::Document),
        "web" => Some(CitationSource::Web),
        "mcp" => Some(CitationSource::Mcp),
        _ => None,
    }))
}

/// What one chat turn reports while it streams.
#[derive(Debug, Clone)]
pub enum ChatEvent {
    /// Answer text, as it arrives.
    Content(String),
    /// A `chat.status` progress line, e.g. `[tools] searching documents`.
    Status { step: String, message: String },
    /// Sources the answer used, sent once after the last token.
    Citations(Vec<Citation>),
    /// The connection dropped before any content; the turn is sent again.
    Retrying(String),
    /// The tool calls the reply ended with, once each is whole: the turn
    /// goes on once they've run and their results are sent back.
    ToolCalls(Vec<ToolCall>),
}

/// A finished turn's size and time.
#[derive(Debug, Clone, Copy, Default)]
pub struct TurnStats {
    pub tokens: u64,
    pub elapsed: Duration,
}

impl TurnStats {
    pub fn summary(&self) -> String {
        let secs = self.elapsed.as_secs_f64();
        let rate = if secs > 0.0 && self.tokens > 0 { self.tokens as f64 / secs } else { 0.0 };
        format!("{} tokens, {:.1} tok/s, {:.0}s", self.tokens, rate, secs)
    }
}

/// Build the async SSE client used for chat streaming. Uses `async-openai`'s
/// `eventsource_stream`-based SSE parser (the same approach real OpenAI SDKs
/// use) instead of a hand-rolled line reader over a raw socket.
pub fn openai_client(api_url: String) -> OpenAIClient<OpenAIConfig> {
    OpenAIClient::with_config(
        OpenAIConfig::new()
            .with_api_base(api_url)
            .with_api_key("not-needed"),
    )
}

/// A fresh session id; one underpins a whole conversation, so the service
/// keeps the context across turns.
pub fn new_session_id() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    format!("cli-{}-{}", nanos, std::process::id())
}

/// What one chat turn sends.
pub struct Request<'a> {
    /// Goes as a system message ahead of `message`: the agent adds it to
    /// its own system prompt, so instructions there are followed, where in
    /// the user's message they'd be read as data.
    pub system: Option<&'a str>,
    pub message: &'a str,
    /// Ids of files uploaded as assets, which ride along on the user's
    /// message.
    pub attachments: &'a [String],
    pub project_id: &'a str,
    pub cwd: Option<&'a std::path::Path>,
    pub session_id: &'a str,
    /// Offer the `bash` tool: only on a turn that works in a folder of the
    /// user's, where its commands can run. A server that won't give tools
    /// to the model it picked ignores it and answers in text, as before.
    pub tools: bool,
    /// The turn's tool rounds so far, after the user's message: each
    /// assistant message with its `tool_calls`, then a `tool` message with
    /// each call's result.
    pub rounds: &'a [serde_json::Value],
}

/// A tool call the reply asked for: the call's id, which its result goes
/// back with, the tool's name, and its arguments as the JSON text given.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

impl ToolCall {
    /// The call as the assistant message carries it back, OpenAI-style.
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "id": self.id,
            "type": "function",
            "function": {"name": self.name, "arguments": self.arguments},
        })
    }
}

/// The one tool docent offers: a shell command, run in the project folder
/// through the same approval as a reply's fenced commands. Files are still
/// written from the reply's text, so there's no tool for that, and the
/// description says so: offered only `bash`, a model asked to refactor wrote
/// every new file through it (`cat > f <<EOF`), whole files inside one tool
/// call's arguments. A tool call isn't streamed, so that showed nothing for
/// minutes and ran into the answer cap, cutting the call off. Written in the
/// reply, files stream as diffs and are written as each block ends.
pub fn bash_tool() -> serde_json::Value {
    serde_json::json!({
        "type": "function",
        "function": {
            "name": "bash",
            "description": "Run a shell command in the project folder and return its output (stdout+stderr, exit code). Use it to look around the project, build, test, or run git. Never use it to create or change files (no cat > file, heredocs, sed -i, tee, or echo >): write new files whole and change existing ones in your reply, as the instructions describe.",
            "parameters": {
                "type": "object",
                "properties": {
                    "command": {"type": "string", "description": "The command to run"},
                    "description": {"type": "string", "description": "What the command does, in a few words"},
                },
                "required": ["command"],
            },
        },
    })
}

/// The tool calls of a streamed reply, put together from their deltas: a
/// call comes in pieces by its `index`, its id and name first, then its
/// arguments a bit at a time.
#[derive(Debug, Default)]
pub struct ToolCalls {
    calls: Vec<(u64, ToolCall)>,
}

impl ToolCalls {
    /// Add one chunk's `delta.tool_calls`.
    pub fn push(&mut self, deltas: &serde_json::Value) {
        let Some(deltas) = deltas.as_array() else { return };
        for (n, delta) in deltas.iter().enumerate() {
            // Some servers leave the index out of a lone call.
            let index = delta["index"].as_u64().unwrap_or(n as u64);
            let at = match self.calls.iter().position(|(i, _)| *i == index) {
                Some(at) => at,
                None => {
                    self.calls.push((index, ToolCall { id: String::new(), name: String::new(), arguments: String::new() }));
                    self.calls.len() - 1
                }
            };
            let call = &mut self.calls[at].1;
            if let Some(id) = delta["id"].as_str().filter(|id| !id.is_empty()) {
                call.id = id.to_string();
            }
            // The name comes whole, once; a server repeating it in every
            // chunk isn't giving a longer one.
            if let Some(name) = delta["function"]["name"].as_str()
                && !call.name.ends_with(name)
            {
                call.name.push_str(name);
            }
            if let Some(arguments) = delta["function"]["arguments"].as_str() {
                call.arguments.push_str(arguments);
            }
        }
    }

    pub fn is_empty(&self) -> bool {
        self.calls.is_empty()
    }

    /// The calls, in order; one the server gave no id gets one, as its
    /// result must name it.
    pub fn finish(mut self) -> Vec<ToolCall> {
        self.calls.sort_by_key(|(i, _)| *i);
        self.calls
            .into_iter()
            .map(|(i, mut call)| {
                if call.id.is_empty() {
                    call.id = format!("call_{}", i);
                }
                call
            })
            .collect()
    }
}

/// The body of a turn's request.
pub fn request_body(request: &Request) -> serde_json::Value {
    // The server-side orchestrator always picks the model that actually
    // serves the turn; "sl-mini" here just names the entry point it routes
    // through, not a choice the caller gets to make.
    // Files uploaded as assets ride along by id; the agent puts their
    // contents in front of the message.
    let mut user = serde_json::json!({"role": "user", "content": request.message});
    if !request.attachments.is_empty() {
        user["attachments"] = serde_json::json!(request.attachments);
    }
    let mut messages = Vec::new();
    if let Some(system) = request.system {
        messages.push(serde_json::json!({"role": "system", "content": system}));
    }
    messages.push(user);
    messages.extend(request.rounds.iter().cloned());
    let cwd = request.cwd.map(|dir| dir.to_string_lossy().into_owned());
    let mut body = serde_json::json!({
        "model": "sl-mini",
        "messages": messages,
        "session_id": request.session_id,
        "project_id": request.project_id,
        "cwd": cwd,
        "stream": true,
    });
    if request.tools {
        body["tools"] = serde_json::json!([bash_tool()]);
        body["tool_choice"] = serde_json::json!("auto");
    }
    body
}

/// Stream one chat turn, reporting each event to `on` the moment it arrives.
/// Returns an error message when the stream drops mid-way instead of
/// exiting, so an interactive session can keep going after a server hiccup.
///
/// Long-running tool steps can leave the connection idle long enough for the
/// server (or a proxy in front of it) to drop it, which surfaces as a stream
/// error before any content has streamed. Retry once in that case since a
/// fresh connection usually succeeds.
pub async fn stream_turn(
    client: &OpenAIClient<OpenAIConfig>,
    request: &Request<'_>,
    on: &mut (dyn FnMut(ChatEvent) + Send),
) -> Result<TurnStats, String> {
    match stream_turn_once(client, request, on).await {
        Ok(stats) => Ok(stats),
        Err((e, 0)) => {
            on(ChatEvent::Retrying(e));
            stream_turn_once(client, request, on).await.map_err(|(e, _)| e)
        }
        Err((e, _)) => Err(e),
    }
}

async fn stream_turn_once(
    client: &OpenAIClient<OpenAIConfig>,
    request: &Request<'_>,
    on: &mut (dyn FnMut(ChatEvent) + Send),
) -> Result<TurnStats, (String, u64)> {
    let body = request_body(request);
    let started = Instant::now();
    let mut stream = client
        .chat()
        .create_stream_byot::<serde_json::Value, serde_json::Value>(body)
        .await
        .map_err(|e| (format!("Failed to run: {}", e), 0))?;

    // `shown` counts what went to `on`: a drop before any of it is retried.
    // Tool calls go to `on` only once whole, so their chunks aren't shown,
    // though they count in the turn's size.
    let (mut tokens, mut shown): (u64, u64) = (0, 0);
    let mut calls = ToolCalls::default();
    while let Some(event) = stream.next().await {
        let event = event.map_err(|e| (format!("Failed to read stream: {}", e), shown))?;

        match event["object"].as_str() {
            Some("chat.completion.chunk") => {
                let delta = &event["choices"][0]["delta"];
                if let Some(content) = delta["content"].as_str() {
                    tokens += 1;
                    shown += 1;
                    on(ChatEvent::Content(content.to_string()));
                }
                if delta["tool_calls"].is_array() {
                    tokens += 1;
                    calls.push(&delta["tool_calls"]);
                }
                if event["choices"][0]["finish_reason"].is_string() {
                    break;
                }
            }
            Some("chat.citations") => {
                if let Some(list) = event["citations"].as_array() {
                    let citations = list.iter().filter_map(|c| serde_json::from_value(c.clone()).ok()).collect();
                    on(ChatEvent::Citations(citations));
                }
            }
            Some("chat.status") => match event["message"].as_str() {
                Some(message) if !message.trim().is_empty() => on(ChatEvent::Status {
                    step: event["step"].as_str().unwrap_or_default().to_string(),
                    message: message.trim().to_string(),
                }),
                _ => {}
            },
            _ => {}
        }
    }
    // Ended with `finish_reason: "tool_calls"`, or with calls seen whatever
    // the reason given: the reply waits on their results.
    if !calls.is_empty() {
        on(ChatEvent::ToolCalls(calls.finish()));
    }

    Ok(TurnStats { tokens, elapsed: started.elapsed() })
}

/// The folder a turn of `project_id` works in: the agent binds the project
/// to it on first use, then indexes and explores it. That's the folder the
/// project syncs from, when it has one, wherever the chat was opened; never
/// one linked to another project.
pub fn turn_dir(project_id: &str) -> Option<std::path::PathBuf> {
    std::env::current_dir().ok().and_then(|here| crate::docent::workspace(project_id, &here))
}

/// The folder a turn that makes files works in: the one the chat was opened
/// in, so the agent reads the code that's there and the files land next to
/// it. None when that folder syncs to another project, or this project is
/// linked to a folder elsewhere: the agent only works in the folder its
/// project is bound to, and sent any other it falls back to its own state
/// under `~/.smartloop`, writing and citing files there.
pub fn making_dir(project_id: &str) -> Option<std::path::PathBuf> {
    let here = std::env::current_dir().ok()?;
    making_dir_in(project_id, &here, crate::docent::folder(project_id))
}

/// `making_dir` for a chat opened in `here`, with `folder` the one the
/// project is linked to on this machine: that folder when `here` is in it,
/// else `here` when the project has no folder and `here` isn't linked to
/// another project.
pub fn making_dir_in(project_id: &str, here: &std::path::Path, folder: Option<std::path::PathBuf>) -> Option<std::path::PathBuf> {
    let real = |p: &std::path::Path| p.canonicalize().unwrap_or_else(|_| p.to_path_buf());
    match folder {
        Some(folder) => real(here).starts_with(real(&folder)).then_some(folder),
        None => {
            let linked = crate::docent::linked(&crate::docent::root(here));
            linked.is_none_or(|l| l.project == project_id).then(|| here.to_path_buf())
        }
    }
}

/// `text` trimmed, when there's anything left.
fn text(text: &Option<String>) -> Option<&str> {
    text.as_deref().map(str::trim).filter(|t| !t.is_empty())
}

/// What kind of source `citation` is. An agent older than `source` (the
/// studio app's, say, which docent uses when it's running) sends none: a
/// link is a web page and a tool call names its tool.
fn kind(citation: &Citation) -> Option<CitationSource> {
    citation.source.or_else(|| {
        if text(&citation.url).is_some() {
            Some(CitationSource::Web)
        } else if text(&citation.tool_name).is_some() {
            Some(CitationSource::Mcp)
        } else {
            None
        }
    })
}

/// What a citation is called: its document's name, else its id.
fn name(citation: &Citation) -> Option<&str> {
    text(&citation.document_name).or_else(|| Some(citation.document_id.trim()).filter(|id| !id.is_empty()))
}

/// What to show for `citation`, as the studio app labels its reference
/// pills: a web source by its URL, a tool by its name, a document by the
/// path the agent gave or else its name (or `as_named`), then the pages
/// cited.
pub fn label(citation: &Citation, as_named: Option<&str>) -> Option<String> {
    let own = name(citation);
    let label = match kind(citation) {
        Some(CitationSource::Web) => match (text(&citation.url), own) {
            (Some(url), Some(name)) if name != url && !name.starts_with("http") => format!("{} ({})", name, url),
            (url, name) => url.or(name)?.to_string(),
        },
        Some(CitationSource::Mcp) => own?.to_string(),
        Some(CitationSource::Document) | None => {
            // A document in a hidden folder is the agent's own state, its
            // copy under `~/.smartloop`, not one of the user's to cite.
            let path = text(&citation.path);
            if path.or(own).is_some_and(in_hidden_folder) {
                return None;
            }
            as_named.or(path).or(own)?.to_string()
        }
    };
    let pages = citation.page_numbers.as_deref().unwrap_or_default();
    Some(match pages {
        [] => label,
        [page] => format!("{}, p. {}", label, page_text(page)),
        pages => format!("{}, pp. {}", label, page_ranges(pages)),
    })
}

/// A page as the document's index recorded it: `3`, or `iv`.
fn page_text(page: &serde_json::Value) -> String {
    page.as_str().map_or_else(|| page.to_string(), str::to_string)
}

/// `pages` with each run of consecutive numbers as a range: `1–3, 7, iv`.
fn page_ranges(pages: &[serde_json::Value]) -> String {
    let mut runs: Vec<(String, Option<(u64, u64)>)> = Vec::new();
    for page in pages {
        match (runs.last_mut(), page.as_u64()) {
            (Some((_, Some((_, last)))), Some(n)) if n == *last + 1 => *last = n,
            (_, n) => runs.push((page_text(page), n.map(|n| (n, n)))),
        }
    }
    let runs: Vec<String> = runs
        .into_iter()
        .map(|(page, run)| match run {
            Some((first, last)) if first != last => format!("{}–{}", first, last),
            _ => page,
        })
        .collect();
    runs.join(", ")
}

/// A citation as the References list shows it, and what clicking it opens:
/// a web page, or a file on this machine as a `file://` URL.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Reference {
    pub label: String,
    pub target: Option<String>,
}

impl From<&str> for Reference {
    fn from(label: &str) -> Self {
        Reference { label: label.to_string(), target: None }
    }
}

/// What clicking `citation` opens. A web source opens its page and a tool
/// nothing. A document opens, best first: the file the agent gave the full
/// path of; the file under `root` that `/sync` pushed by the document's
/// name; the copy the agent keeps under `project_dir`, at the
/// `document_path` it gave or else in `documents/` by name, which only an
/// agent on this machine has. A file that isn't there is passed over.
pub fn reference(
    citation: &Citation,
    root: Option<&std::path::Path>,
    project_dir: Option<&std::path::Path>,
) -> Option<Reference> {
    let found = |target: Option<String>| Some(Reference { label: label(citation, None)?, target });
    match kind(citation) {
        Some(CitationSource::Web) => {
            let url = text(&citation.url).filter(|u| u.starts_with("https://") || u.starts_with("http://"));
            return found(url.map(str::to_string));
        }
        Some(CitationSource::Mcp) => return found(None),
        Some(CitationSource::Document) | None => {}
    }
    let given = text(&citation.path).and_then(crate::links::target).filter(|t| t.starts_with("file://"));
    if given.is_some() {
        return found(given);
    }
    if let Some(name) = name(citation) {
        if let Some((key, path)) = root.and_then(|root| crate::docent::synced_file(root, name)) {
            let label = label(citation, Some(&key))?;
            return Some(Reference { label, target: Some(crate::links::file_url(&path)) });
        }
        let copy = project_dir
            .into_iter()
            .flat_map(|dir| {
                let given = text(&citation.document_path).map(|p| dir.join(p));
                given.into_iter().chain(std::iter::once(dir.join("documents").join(name)))
            })
            .find(|p| p.is_file());
        if let Some(copy) = copy {
            return found(Some(crate::links::file_url(&copy)));
        }
    }
    found(None)
}

/// The references for `citations`, found from the folder `project` syncs
/// from (its own, else the working directory's when that's linked to it)
/// and the project's folder at the agent when the agent runs here.
pub fn references(citations: &[Citation], project: Option<&str>) -> Vec<Reference> {
    let root = project.and_then(|project| {
        crate::docent::folder(project).or_else(|| {
            let here = crate::docent::root(&std::env::current_dir().ok()?);
            crate::docent::linked(&here).is_some_and(|l| l.project == project).then_some(here)
        })
    });
    let project_dir = project.filter(|_| crate::is_local()).and_then(crate::framework::project_dir);
    citations.iter().filter_map(|c| reference(c, root.as_deref(), project_dir.as_deref())).collect()
}

/// Whether `path` lies under a folder whose name starts with `.`, as the
/// agent's own state under `~/.smartloop` does.
pub fn in_hidden_folder(path: &str) -> bool {
    let path = path.replace('\\', "/");
    path.rsplit_once('/')
        .is_some_and(|(dirs, _)| dirs.split('/').any(|d| d.len() > 1 && d.starts_with('.') && d != ".."))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::links::file_url;
    use serde_json::json;
    use std::fs;

    fn link(root: &std::path::Path, project: &str) {
        std::fs::create_dir_all(root.join(".docent")).unwrap();
        std::fs::write(root.join(".docent/manifest.json"), format!(r#"{{"project":"{}"}}"#, project)).unwrap();
    }

    #[test]
    fn files_are_made_in_the_projects_folder_only() {
        let folder = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        link(folder.path(), "p1");
        let sub = folder.path().join("src");
        std::fs::create_dir_all(&sub).unwrap();
        // Opened in the project's folder, or under it: its root.
        let own = Some(folder.path().to_path_buf());
        assert_eq!(making_dir_in("p1", folder.path(), own.clone()), own);
        assert_eq!(making_dir_in("p1", &sub, own.clone()), own);
        // Opened elsewhere: the agent wouldn't work there.
        assert_eq!(making_dir_in("p1", elsewhere.path(), own), None);
        // A project with no folder works where the chat was opened, unless
        // that's another project's.
        assert_eq!(making_dir_in("p2", elsewhere.path(), None), Some(elsewhere.path().to_path_buf()));
        assert_eq!(making_dir_in("p2", folder.path(), None), None);
    }

    #[test]
    fn tool_calls_are_put_together_from_their_deltas() {
        let mut calls = ToolCalls::default();
        assert!(calls.is_empty());
        let chunks = [
            serde_json::json!([{"index": 0, "id": "call_a", "type": "function", "function": {"name": "bash", "arguments": ""}}]),
            serde_json::json!([{"index": 0, "function": {"arguments": "{\"comm"}}]),
            serde_json::json!([{"index": 1, "id": "call_b", "function": {"name": "ba"}}, {"index": 0, "function": {"arguments": "and\": \"ls\"}"}}]),
            serde_json::json!([{"index": 1, "function": {"name": "sh", "arguments": "{\"command\":\"pwd\"}"}}]),
            // A server repeating the name in every chunk.
            serde_json::json!([{"index": 1, "function": {"name": "bash"}}]),
        ];
        for chunk in &chunks {
            calls.push(chunk);
        }
        assert_eq!(
            calls.finish(),
            [
                ToolCall { id: "call_a".into(), name: "bash".into(), arguments: r#"{"command": "ls"}"#.into() },
                ToolCall { id: "call_b".into(), name: "bash".into(), arguments: r#"{"command":"pwd"}"#.into() },
            ]
        );
        // One with no index or id still gets an id for its result.
        let mut calls = ToolCalls::default();
        calls.push(&serde_json::json!([{"function": {"name": "bash", "arguments": "{}"}}]));
        assert_eq!(calls.finish()[0].id, "call_0");
    }

    #[test]
    fn tools_are_offered_on_making_turns_with_their_rounds_after_the_users_message() {
        let call = ToolCall { id: "call_1".into(), name: "bash".into(), arguments: r#"{"command":"ls"}"#.into() };
        let rounds = vec![
            serde_json::json!({"role": "assistant", "content": null, "tool_calls": [call.to_json()]}),
            serde_json::json!({"role": "tool", "tool_call_id": "call_1", "content": "Exit code: 0\nsrc"}),
        ];
        let request = Request {
            system: Some("sys"),
            message: "list it",
            attachments: &[],
            project_id: "p1",
            cwd: None,
            session_id: "s1",
            tools: true,
            rounds: &rounds,
        };
        let body = request_body(&request);
        assert_eq!(body["tool_choice"], "auto");
        assert_eq!(body["tools"][0]["type"], "function");
        assert_eq!(body["tools"][0]["function"]["name"], "bash");
        assert_eq!(body["tools"][0]["function"]["parameters"]["required"], serde_json::json!(["command"]));
        let roles: Vec<&str> = body["messages"].as_array().unwrap().iter().map(|m| m["role"].as_str().unwrap()).collect();
        assert_eq!(roles, ["system", "user", "assistant", "tool"]);
        assert_eq!(
            body["messages"][2]["tool_calls"][0],
            serde_json::json!({"id": "call_1", "type": "function", "function": {"name": "bash", "arguments": "{\"command\":\"ls\"}"}})
        );
        assert_eq!(body["messages"][3]["tool_call_id"], "call_1");

        // A turn that isn't making files offers none, and is as before.
        let plain = request_body(&Request { system: None, tools: false, rounds: &[], attachments: &["a1".into()], ..request });
        assert!(plain.get("tools").is_none() && plain.get("tool_choice").is_none());
        assert_eq!(plain["messages"], serde_json::json!([{"role": "user", "content": "list it", "attachments": ["a1"]}]));
    }

    #[test]
    fn documents_in_hidden_folders_are_not_cited() {
        assert!(in_hidden_folder("/Users/me/.smartloop/ws/project/src/main.rs"));
        assert!(in_hidden_folder("~/.smartloop/x.md"));
        assert!(in_hidden_folder(".docent/skills/a.md"));
        assert!(!in_hidden_folder("src/main.rs"));
        assert!(!in_hidden_folder(".gitignore"));
        assert!(!in_hidden_folder("../src/main.rs"));
        assert!(!in_hidden_folder("roadmap.pdf"));

        let hidden = sent("document", json!({"document_name": "main.rs", "path": "/Users/me/.smartloop/ws/p/src/main.rs"}));
        assert_eq!(label(&hidden, None), None);
        let hidden_name = sent("document", json!({"document_name": "/Users/me/.smartloop/ws/p/notes.md"}));
        assert_eq!(label(&hidden_name, None), None);
        let shown = sent("document", json!({"document_name": "roadmap.pdf", "path": "docs/roadmap.pdf"}));
        assert_eq!(label(&shown, None).as_deref(), Some("docs/roadmap.pdf"));
        let web = sent("web", json!({"document_name": "https://smartloop.ai/guide", "url": "https://smartloop.ai/guide"}));
        assert_eq!(label(&web, None).as_deref(), Some("https://smartloop.ai/guide"));
    }

    /// A citation as the agent sends it: its `source`, and the document's
    /// name as its id when it gives none.
    fn sent(source: &str, mut fields: serde_json::Value) -> Citation {
        fields["source"] = json!(source);
        if fields.get("document_id").is_none() {
            fields["document_id"] = fields["document_name"].clone();
        }
        serde_json::from_value(fields).unwrap()
    }

    #[test]
    fn labels_documents_web_sources_tools_and_pages() {
        let document = sent("document", json!({"document_id": "9f2c", "document_name": "plan.pdf", "page_numbers": [2, 5]}));
        assert_eq!(label(&document, None).as_deref(), Some("plan.pdf, pp. 2, 5"));
        let placed = sent("document", json!({"document_name": "plan.pdf", "document_path": "documents/2024/plan.pdf"}));
        assert_eq!(label(&placed, None).as_deref(), Some("plan.pdf"), "where the agent keeps it isn't its name");

        let page = sent("document", json!({"document_name": "plan.pdf", "page_numbers": [3]}));
        assert_eq!(label(&page, None).as_deref(), Some("plan.pdf, p. 3"));
        let run = sent("document", json!({"document_name": "fund.pdf", "page_numbers": [1, 2, 3, 4, 7, 9, 10]}));
        assert_eq!(label(&run, None).as_deref(), Some("fund.pdf, pp. 1–4, 7, 9–10"));
        let roman = sent("document", json!({"document_name": "fund.pdf", "page_numbers": ["iv", 1, 2]}));
        assert_eq!(label(&roman, None).as_deref(), Some("fund.pdf, pp. iv, 1–2"));

        let web = sent("web", json!({"document_name": "https://a.io/x", "url": "https://a.io/x"}));
        assert_eq!(label(&web, None).as_deref(), Some("https://a.io/x"));
        let named = sent("web", json!({"document_name": "Guide", "url": "https://a.io/guide"}));
        assert_eq!(label(&named, None).as_deref(), Some("Guide (https://a.io/guide)"));

        let tool = sent("mcp", json!({"document_id": "mcp:Linear.list_issues", "document_name": "Linear · list_issues"}));
        assert_eq!(label(&tool, None).as_deref(), Some("Linear · list_issues"));

        let given = sent("document", json!({"document_name": "plan.pdf", "path": " /docs/plan.pdf "}));
        assert_eq!(label(&given, None).as_deref(), Some("/docs/plan.pdf"));
        assert_eq!(label(&sent("document", json!({"document_id": "", "page_numbers": [1]})), None), None);
    }

    #[test]
    fn opens_the_best_place_a_citation_names() {
        // Not tempfile's `.tmp` name: a hidden folder isn't cited.
        let dir = tempfile::Builder::new().prefix("cite").tempdir().unwrap();
        let root = dir.path().join("project");
        let project_dir = dir.path().join("slp");
        fs::create_dir_all(root.join("notes")).unwrap();
        fs::create_dir_all(root.join(crate::docent::DIR)).unwrap();
        fs::create_dir_all(project_dir.join("documents")).unwrap();
        fs::write(root.join("notes/plan.md"), "plan").unwrap();
        fs::write(project_dir.join("documents/notes__plan.md"), "plan").unwrap();
        fs::write(project_dir.join("documents/upload.pdf"), "pdf").unwrap();
        let manifest = json!({"project": "p", "documents": {"notes/plan.md": {
            "hash": "h", "size": 4, "mtime": 1, "remote": "notes__plan.md", "synced_at": 1,
        }}});
        fs::write(root.join(crate::docent::DIR).join("manifest.json"), manifest.to_string()).unwrap();
        let open = |value: serde_json::Value| reference(&sent("document", value), Some(&root), Some(&project_dir)).unwrap();

        // A web source opens its page, and a tool nothing, even one named
        // like a file that's here.
        let web = sent("web", json!({"document_name": "https://a.io", "url": "https://a.io"}));
        assert_eq!(reference(&web, Some(&root), Some(&project_dir)).unwrap().target.as_deref(), Some("https://a.io"));
        let tool = sent("mcp", json!({"document_name": "upload.pdf", "tool_name": "fetch"}));
        assert_eq!(reference(&tool, Some(&root), Some(&project_dir)).unwrap().target, None);

        // A path the agent gives wins, when it's there.
        let given = root.join("notes/plan.md");
        let with_path = open(json!({"document_name": "upload.pdf", "path": given.to_str().unwrap()}));
        assert_eq!(with_path.target, Some(file_url(&given)));
        let gone = open(json!({"document_name": "upload.pdf", "path": "/nowhere/upload.pdf"}));
        assert_eq!(gone.target, Some(file_url(&project_dir.join("documents/upload.pdf"))));

        // A synced document opens the original, named as it is here.
        let synced = open(json!({"document_name": "notes__plan.md", "page_numbers": [1]}));
        assert_eq!(synced, Reference { label: "notes/plan.md, p. 1".into(), target: Some(file_url(&root.join("notes/plan.md"))) });

        // Gone here since its sync: the agent's copy.
        fs::remove_file(root.join("notes/plan.md")).unwrap();
        let copy = open(json!({"document_name": "notes__plan.md"}));
        assert_eq!(copy, Reference { label: "notes__plan.md".into(), target: Some(file_url(&project_dir.join("documents/notes__plan.md"))) });

        // The agent's copy where it says it keeps it, over the guess by name.
        fs::create_dir_all(project_dir.join("documents/2024")).unwrap();
        fs::write(project_dir.join("documents/2024/upload.pdf"), "pdf").unwrap();
        let placed = open(json!({"document_name": "upload.pdf", "document_path": "documents/2024/upload.pdf"}));
        assert_eq!(placed.target, Some(file_url(&project_dir.join("documents/2024/upload.pdf"))));
        let stale = open(json!({"document_name": "upload.pdf", "document_path": "documents/old/upload.pdf"}));
        assert_eq!(stale.target, Some(file_url(&project_dir.join("documents/upload.pdf"))));

        // An agent from before `source`: a link still opens, a tool still
        // doesn't, and a document cited by name alone still reads.
        let old = |value: serde_json::Value| {
            let citation: Citation = serde_json::from_value(value).unwrap();
            reference(&citation, Some(&root), Some(&project_dir)).unwrap()
        };
        assert_eq!(old(json!({"document_name": "Guide", "url": "https://a.io"})).target.as_deref(), Some("https://a.io"));
        assert_eq!(old(json!({"document_id": "mcp:x.fetch", "document_name": "upload.pdf", "tool_name": "fetch"})).target, None);
        assert_eq!(old(json!({"document_name": "upload.pdf"})).target, Some(file_url(&project_dir.join("documents/upload.pdf"))));

        // An agent elsewhere has no copy here; nothing found opens nothing.
        let remote = reference(&sent("document", json!({"document_name": "upload.pdf"})), None, None).unwrap();
        assert_eq!(remote, Reference { label: "upload.pdf".into(), target: None });
        assert_eq!(open(json!({"document_name": "missing.pdf"})).target, None);
    }

    #[test]
    fn an_older_agents_citation_and_an_unknown_source_still_read() {
        let old: Citation = serde_json::from_value(serde_json::json!({"document_id": "d1", "document_name": "invoice.pdf"})).unwrap();
        assert_eq!(old.document_name.as_deref(), Some("invoice.pdf"));
        assert_eq!(old.source, None);
        let newer: Citation =
            serde_json::from_value(serde_json::json!({"source": "spreadsheet", "path": "/tmp/a.csv", "added_later": 1})).unwrap();
        assert_eq!(newer.source, None, "a kind docent doesn't know reads as none");
        assert_eq!(newer.path.as_deref(), Some("/tmp/a.csv"));
    }
}
