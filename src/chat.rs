//! Chat turns streamed from the agent's SSE endpoint, reported as events so
//! the plain line-based chat and the TUI can each show them their own way.

use std::io::{IsTerminal, Write};
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
}

/// A finished turn's size and time.
#[derive(Debug, Clone, Copy)]
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
    message: &str,
    project_id: &str,
    session_id: &str,
    on: &mut (dyn FnMut(ChatEvent) + Send),
) -> Result<TurnStats, String> {
    match stream_turn_once(client, message, project_id, session_id, on).await {
        Ok(stats) => Ok(stats),
        Err((e, tokens)) if tokens == 0 => {
            on(ChatEvent::Retrying(e));
            stream_turn_once(client, message, project_id, session_id, on)
                .await
                .map_err(|(e, _)| e)
        }
        Err((e, _)) => Err(e),
    }
}

async fn stream_turn_once(
    client: &OpenAIClient<OpenAIConfig>,
    message: &str,
    project_id: &str,
    session_id: &str,
    on: &mut (dyn FnMut(ChatEvent) + Send),
) -> Result<TurnStats, (String, u64)> {
    // The server-side orchestrator always picks the model that actually
    // serves the turn; "sl-mini" here just names the entry point it routes
    // through, not a choice the caller gets to make.
    let body = serde_json::json!({
        "model": "sl-mini",
        "messages": [{"role": "user", "content": message}],
        "session_id": session_id,
        "project_id": project_id,
        "stream": true,
    });

    let started = Instant::now();
    let mut stream = client
        .chat()
        .create_stream_byot::<serde_json::Value, serde_json::Value>(body)
        .await
        .map_err(|e| (format!("Failed to run: {}", e), 0))?;

    let mut tokens: u64 = 0;
    while let Some(event) = stream.next().await {
        let event = event.map_err(|e| (format!("Failed to read stream: {}", e), tokens))?;

        match event["object"].as_str() {
            Some("chat.completion.chunk") => {
                if let Some(content) = event["choices"][0]["delta"]["content"].as_str() {
                    tokens += 1;
                    on(ChatEvent::Content(content.to_string()));
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

    Ok(TurnStats { tokens, elapsed: started.elapsed() })
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
        (None, name) => field(&["file_path", "path", "source_path"]).or(name),
    }
}

/// ANSI color for a `chat.status` step name, grouped by what the step is
/// doing rather than its exact label (the server's step vocabulary isn't a
/// fixed contract, so unrecognized steps still get a sensible default).
fn step_color(step: &str) -> &'static str {
    match step {
        "tools" | "web_search" | "explore" => "\x1b[36m", // cyan
        "plan" => "\x1b[35m",                             // magenta
        "model" => "\x1b[33m",                            // yellow
        "preparing" | "streaming" => "\x1b[32m",          // green
        "error" => "\x1b[31m",                            // red
        _ => "\x1b[34m",                                  // blue
    }
}

/// Print a `[step] message` progress line on stderr, coloring the step tag
/// when stderr is a terminal and leaving plain text otherwise (piped output,
/// redirected logs).
fn print_status(step: &str, message: &str) {
    if std::io::stderr().is_terminal() {
        eprintln!(
            "\x1b[2m[\x1b[0m{}{}\x1b[0m\x1b[2m]\x1b[0m {}",
            step_color(step),
            step,
            message
        );
    } else {
        eprintln!("[{}] {}", step, message);
    }
}

/// Print the answer's sources under it as a numbered "References" list. On
/// stdout, like the answer, so piping a reply keeps its sources.
fn print_citations(out: &mut impl Write, citations: &[serde_json::Value]) {
    let labels: Vec<String> = citations.iter().filter_map(citation_label).collect();
    if labels.is_empty() {
        return;
    }

    let styled = std::io::stdout().is_terminal();
    let (dim, reset) = if styled { ("\x1b[2m", "\x1b[0m") } else { ("", "") };
    let _ = writeln!(out, "\n{}References{}", dim, reset);
    for (i, label) in labels.iter().enumerate() {
        let _ = writeln!(out, "{}[{}]{} {}", dim, i + 1, reset, label);
    }
    let _ = out.flush();
}

/// One turn printed line by line: the answer on stdout as it streams,
/// status lines on stderr so they don't corrupt it, then its sources and a
/// token/throughput summary.
async fn print_turn(
    client: &OpenAIClient<OpenAIConfig>,
    message: &str,
    project_id: &str,
    session_id: &str,
) -> Result<(), String> {
    // Whether the answer so far ends in a newline. A status line printed
    // mid-answer (e.g. "[streaming] Response complete" right after the last
    // token) must start on its own line rather than trail the text.
    let mut at_line_start = true;
    let mut citations: Vec<serde_json::Value> = Vec::new();
    let mut on = |event: ChatEvent| {
        let mut out = std::io::stdout().lock();
        match event {
            ChatEvent::Content(content) => {
                let _ = out.write_all(content.as_bytes());
                let _ = out.flush();
                if !content.is_empty() {
                    at_line_start = content.ends_with('\n');
                }
            }
            ChatEvent::Citations(list) => citations = list,
            ChatEvent::Status { step, message } => {
                if !at_line_start {
                    let _ = writeln!(out);
                    let _ = out.flush();
                    at_line_start = true;
                }
                print_status(&step, &message);
            }
            ChatEvent::Retrying(e) => print_status("error", &format!("connection dropped, retrying: {}", e)),
        }
    };
    let stats = stream_turn(client, message, project_id, session_id, &mut on).await?;

    let mut out = std::io::stdout().lock();
    if !at_line_start {
        let _ = writeln!(out);
    }
    print_citations(&mut out, &citations);
    print_status("stats", &stats.summary());
    Ok(())
}

/// Line-based chat with the local agent, for pipes and `--plain`: answer a
/// first prompt, then keep reading new ones from stdin until EOF, `/quit`,
/// or `exit`.
pub async fn run_plain(
    client: &OpenAIClient<OpenAIConfig>,
    first_prompt: Option<String>,
    project_id: String,
    session: Option<String>,
) {
    let session_id = session.unwrap_or_else(new_session_id);
    eprintln!("session: {}", session_id);

    let stdin = std::io::stdin();
    let report = |result: Result<(), String>| {
        if let Err(e) = result {
            if std::io::stdin().is_terminal() {
                eprintln!("{}", e);
            } else {
                crate::fail(e);
            }
        }
    };

    if let Some(prompt) = first_prompt.filter(|p| !p.trim().is_empty()) {
        println!("> {}", prompt);
        report(print_turn(client, &prompt, &project_id, &session_id).await);
    }

    loop {
        print!("> ");
        let _ = std::io::stdout().flush();

        let mut input = String::new();
        if stdin
            .read_line(&mut input)
            .unwrap_or_else(|e| crate::fail(format!("Failed to read input: {}", e)))
            == 0
        {
            break;
        }

        let input = input.trim();
        if input.is_empty() {
            continue;
        }
        if matches!(input, "/quit" | "/exit" | "/q" | "exit" | "Exit") {
            break;
        }

        report(print_turn(client, input, &project_id, &session_id).await);
    }
}
