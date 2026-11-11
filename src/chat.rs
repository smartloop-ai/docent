//! Chat turns streamed from the agent's SSE endpoint, reported as events for
//! the TUI to show.

use std::time::{Duration, Instant};

use async_openai::{Client as OpenAIClient, config::OpenAIConfig};
use futures_util::StreamExt;

/// What one chat turn reports while it streams.
#[derive(Debug, Clone)]
pub enum ChatEvent {
    /// Answer text, as it arrives.
    Content(String),
    /// A `chat.status` progress line, e.g. `[tools] searching documents`.
    Status { step: String, message: String },
    /// Sources the answer used, sent once after the last token.
    Citations(Vec<serde_json::Value>),
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
/// written from the reply's text, so there's no tool for that.
pub fn bash_tool() -> serde_json::Value {
    serde_json::json!({
        "type": "function",
        "function": {
            "name": "bash",
            "description": "Run a shell command in the project folder and return its output (stdout+stderr, exit code). Use it to inspect the project, build, test, or run git.",
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
                    on(ChatEvent::Citations(list.clone()));
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

/// What to show for one citation, as the studio app labels its reference
/// pills: a web source by its URL, a document by the path the agent gave or
/// else its name.
pub fn citation_label(citation: &serde_json::Value) -> Option<String> {
    let field = |keys: &[&str]| {
        keys.iter()
            .filter_map(|k| citation[*k].as_str())
            .map(str::trim)
            .find(|v| !v.is_empty())
            .map(str::to_string)
    };
    let name = field(&["document_name", "document_id"]);
    let url = field(&["url", "link", "source_url", "href", "web_url"]);

    match (url, name) {
        (Some(url), Some(name)) if name != url && !name.starts_with("http") => {
            Some(format!("{} ({})", name, url))
        }
        (Some(url), _) => Some(url),
        (None, name) => {
            let path = field(&["file_path", "path", "source_path"]);
            // A document in a hidden folder is the agent's own state, its
            // copy under `~/.smartloop`, not one of the user's to cite.
            if path.as_deref().or(name.as_deref()).is_some_and(in_hidden_folder) {
                return None;
            }
            path.or(name)
        }
    }
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

        let hidden = serde_json::json!({"document_name": "main.rs", "file_path": "/Users/me/.smartloop/ws/p/src/main.rs"});
        assert_eq!(citation_label(&hidden), None);
        let hidden_name = serde_json::json!({"document_name": "/Users/me/.smartloop/ws/p/notes.md"});
        assert_eq!(citation_label(&hidden_name), None);
        let shown = serde_json::json!({"document_name": "roadmap.pdf", "file_path": "docs/roadmap.pdf"});
        assert_eq!(citation_label(&shown).as_deref(), Some("docs/roadmap.pdf"));
        let web = serde_json::json!({"url": "https://smartloop.ai/guide"});
        assert_eq!(citation_label(&web).as_deref(), Some("https://smartloop.ai/guide"));
    }
}
