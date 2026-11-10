//! Chat turns streamed from the agent's SSE endpoint, reported as events so
//! the plain line-based chat and the TUI can each show them their own way.

use std::io::{IsTerminal, Write};
use std::time::{Duration, Instant};

use async_openai::{Client as OpenAIClient, config::OpenAIConfig};
use futures_util::StreamExt;
use smartloop::api::model::{Citation, CitationSource};

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

/// Stream one chat turn, reporting each event to `on` the moment it arrives.
/// Returns an error message when the stream drops mid-way instead of
/// exiting, so an interactive session can keep going after a server hiccup.
///
/// Long-running tool steps can leave the connection idle long enough for the
/// server (or a proxy in front of it) to drop it, which surfaces as a stream
/// error before any content has streamed. Retry once in that case since a
/// fresh connection usually succeeds.
///
/// `system`, when given, goes as a system message ahead of `message`: the
/// agent adds it to its own system prompt, so instructions there are
/// followed, where in the user's message they'd be read as data.
pub async fn stream_turn(
    client: &OpenAIClient<OpenAIConfig>,
    system: Option<&str>,
    message: &str,
    attachments: &[String],
    project_id: &str,
    cwd: Option<&std::path::Path>,
    session_id: &str,
    on: &mut (dyn FnMut(ChatEvent) + Send),
) -> Result<TurnStats, String> {
    match stream_turn_once(client, system, message, attachments, project_id, cwd, session_id, on).await {
        Ok(stats) => Ok(stats),
        Err((e, tokens)) if tokens == 0 => {
            on(ChatEvent::Retrying(e));
            stream_turn_once(client, system, message, attachments, project_id, cwd, session_id, on)
                .await
                .map_err(|(e, _)| e)
        }
        Err((e, _)) => Err(e),
    }
}

async fn stream_turn_once(
    client: &OpenAIClient<OpenAIConfig>,
    system: Option<&str>,
    message: &str,
    attachments: &[String],
    project_id: &str,
    cwd: Option<&std::path::Path>,
    session_id: &str,
    on: &mut (dyn FnMut(ChatEvent) + Send),
) -> Result<TurnStats, (String, u64)> {
    // The server-side orchestrator always picks the model that actually
    // serves the turn; "sl-mini" here just names the entry point it routes
    // through, not a choice the caller gets to make.
    // Files uploaded as assets ride along by id; the agent puts their
    // contents in front of the message.
    let mut user = serde_json::json!({"role": "user", "content": message});
    if !attachments.is_empty() {
        user["attachments"] = serde_json::json!(attachments);
    }
    let cwd = cwd.map(|dir| dir.to_string_lossy().into_owned());
    let body = serde_json::json!({
        "model": "sl-mini",
        "messages": system.map_or_else(
            || vec![user.clone()],
            |system| vec![serde_json::json!({"role": "system", "content": system}), user.clone()],
        ),
        "session_id": session_id,
        "project_id": project_id,
        "cwd": cwd,
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
fn print_citations(out: &mut impl Write, citations: &[Citation], project_id: &str) {
    let labels: Vec<String> = references(citations, Some(project_id)).into_iter().map(|r| r.label).collect();
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
    let mut citations: Vec<Citation> = Vec::new();
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
    let cwd = turn_dir(project_id);
    let stats = stream_turn(client, None, message, &[], project_id, cwd.as_deref(), session_id, &mut on).await?;

    let mut out = std::io::stdout().lock();
    if !at_line_start {
        let _ = writeln!(out);
    }
    print_citations(&mut out, &citations, project_id);
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
}
