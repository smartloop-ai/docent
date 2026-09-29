use std::io::{IsTerminal, Write};
use std::process::exit;

use async_openai::{Client as OpenAIClient, config::OpenAIConfig};
use clap::{Parser, Subcommand};
use futures_util::StreamExt;
use prettytable::{Attr, Cell, Row, Table, color};
use reqwest::blocking::{Client, Response, multipart};

mod framework;

const LOGO: &str = r#"
█▀ █▀▄▀█ ▄▀█ █▀█ ▀█▀ █   █▀█ █▀█ █▀█
▄█ █ ▀ █ █▀█ █▀▄  █  █▄▄ █▄█ █▄█ █▀▀
"#;

#[derive(Parser)]
#[command(
    name = "smartloop", 
    version, 
    author,
    about=format!("{}\nLocal AI assistant and model orchestrator", LOGO),
)]

struct Args {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Sign in by pasting a Smartloop token
    Login {
        /// Token to store; prompts for it (input hidden) when omitted
        #[arg(long)]
        token: Option<String>,
    },
    /// Clear stored credentials
    Logout,
    /// Manage projects
    Project {
        /// Name of the project
        #[command(subcommand)]
        command: ProjectCommands,
    },
    /// Start, stop, and inspect the local agent
    Agent {
        #[command(subcommand)]
        command: AgentCommands,
    },
    /// Stream an interactive chat with the local agent
    Run {
        /// First message to send; the conversation continues interactively
        #[arg(value_name = "PROMPT")]
        prompt: Option<String>,
        /// Project ID to use; prompts for one when omitted
        #[arg(long, short)]
        project: Option<String>,
        /// Session ID to resume; a fresh one is created when omitted
        #[arg(long, short)]
        session: Option<String>,
    },
}

#[derive(Subcommand)]
enum AgentCommands {
    /// Show the agent's endpoint, whether it is running, its model, and each project agent
    Status,
    /// Install the SLP framework if needed, start the agent, and download its models
    Start,
    /// Stop the local agent
    Stop,
}

#[derive(Subcommand)]
enum ProjectCommands {
    /// List all projects
    List,
    /// Create a project from a blank template, or from an exported archive
    Create {
        /// Name of the project; optional with --import, where it renames the imported project
        #[arg(long)]
        name: Option<String>,
        /// Description of the project
        #[arg(long, conflicts_with = "import")]
        description: Option<String>,
        /// Path to a zip archive produced by an earlier project export
        #[arg(long, value_name = "ZIP")]
        import: Option<String>,
    },
    /// Delete a project by ID
    Delete {
        /// ID of the project to delete
        #[arg(long)]
        id: String,
    },
}

/// Base URL of the Smartloop agent, overridable for non-default installs.
fn base_url() -> String {
    let base = std::env::var("SMARTLOOP_API_URL")
        .unwrap_or_else(|_| format!("http://localhost:{}", framework::port()));
    base.trim_end_matches('/').to_string()
}

/// True when the CLI talks to the agent it manages itself: the default local
/// endpoint rather than a `SMARTLOOP_API_URL` someone else runs.
fn is_local() -> bool {
    std::env::var("SMARTLOOP_API_URL").is_err()
}

/// Client for talking to the agent, after making sure one is up. Starts the
/// local framework (installing it on first use) when nothing answers.
fn agent_client() -> Client {
    let client = Client::new();
    framework::ensure_running(&client, &base_url(), is_local());
    client
}

fn api_url() -> String {
    format!("{}/v1", base_url())
}

fn projects_url() -> String {
    format!("{}/projects", api_url())
}

/// Print an error and stop; used instead of panicking so failures read as
/// CLI output rather than a Rust backtrace.
fn fail(message: String) -> ! {
    eprintln!("Error: {}", message);
    exit(1);
}

/// Turn a non-2xx response into a message, preferring FastAPI's `detail` field
/// over the bare status code.
fn error_message(action: &str, response: Response) -> String {
    let status = response.status();
    let detail = response
        .json::<serde_json::Value>()
        .ok()
        .and_then(|body| body["detail"].as_str().map(str::to_string));

    match detail {
        Some(detail) => format!("Failed to {}: {} ({})", action, detail, status),
        None => format!("Failed to {}: {}", action, status),
    }
}

fn print_projects(projects: &[serde_json::Value]) {

    let mut table = Table::new();

    table.add_row(Row::new(vec![
        Cell::new("ID"),
        Cell::new("Name"),
        Cell::new("System"),
    ]));

    for project in projects {
        let id = project["id"].as_str().unwrap_or_default();
        let name = project["name"].as_str().unwrap_or_default();
        let system = if project["system"].as_bool().unwrap_or_default() {
            Cell::new("true").with_style(Attr::ForegroundColor(color::MAGENTA))
        } else {
            Cell::new("false")
        };
        table.add_row(Row::new(vec![
            Cell::new(id),
            Cell::new(name),
            system,
        ]));
    }

    table.printstd();

}

fn fetch_projects(client: &Client) -> Vec<serde_json::Value> {
    let response = client
        .get(projects_url())
        .send()
        .unwrap_or_else(|e| fail(format!("Failed to list projects: {}", e)));

    if !response.status().is_success() {
        fail(error_message("list projects", response));
    }

    let mut data: serde_json::Value = response
        .json()
        .unwrap_or_else(|e| fail(format!("Failed to parse response as JSON: {}", e)));

    match data["projects"].take() {
        serde_json::Value::Array(projects) => projects,
        _ => fail("Expected projects to be an array".to_string()),
    }
}

fn list_projects(client: &Client) {
    print_projects(&fetch_projects(client));
}

/// Pick the project a `run` session chats in. The chat request has to name a
/// project: without one the server's supervisor handles the stream itself and
/// drops it after the first event. Offers a numbered list on a terminal,
/// defaulting to the server's current project; non-interactive runs take the
/// current project without asking.
fn select_project(client: &Client) -> String {
    let projects = fetch_projects(client);
    if projects.is_empty() {
        fail("No projects found; create one with `smartloop project create`".to_string());
    }

    let default = projects
        .iter()
        .position(|p| p["current"].as_bool().unwrap_or_default())
        .unwrap_or(0);
    let id = |i: usize| projects[i]["id"].as_str().unwrap_or_default().to_string();
    let name = |i: usize| projects[i]["name"].as_str().unwrap_or_default();

    if projects.len() == 1 || !std::io::stdin().is_terminal() {
        eprintln!("project: {}", name(default));
        return id(default);
    }

    for i in 0..projects.len() {
        let marker = if i == default { " (current)" } else { "" };
        eprintln!("  {}. {}{}", i + 1, name(i), marker);
    }

    loop {
        eprint!("Select a project [{}]: ", default + 1);
        let _ = std::io::stderr().flush();

        let mut input = String::new();
        if std::io::stdin()
            .read_line(&mut input)
            .unwrap_or_else(|e| fail(format!("Failed to read input: {}", e)))
            == 0
        {
            exit(0);
        }

        let input = input.trim();
        if input.is_empty() {
            return id(default);
        }
        match input.parse::<usize>() {
            Ok(n) if (1..=projects.len()).contains(&n) => return id(n - 1),
            _ => eprintln!("Enter a number from 1 to {}", projects.len()),
        }
    }
}

/// Create an empty project — the blank template is a project with no skills,
/// which the API seeds with the workspace defaults.
fn create_project(client: &Client, name: String, description: Option<String>) {
    let mut body = serde_json::json!({
        "name": name,
        "system": false,
        "skills": [],
    });

    if let Some(description) = description {
        body["description"] = serde_json::Value::String(description);
    }

    let response = client
        .post(projects_url())
        .json(&body)
        .send()
        .unwrap_or_else(|e| fail(format!("Failed to create project: {}", e)));

    if !response.status().is_success() {
        fail(error_message("create project", response));
    }

    report_created(response, "Project created");
}

/// Create a project from a zip archive produced by an earlier export.
fn import_project(client: &Client, path: String, name: Option<String>) {
    let mut form = multipart::Form::new()
        .file("file", &path)
        .unwrap_or_else(|e| fail(format!("Failed to read {}: {}", path, e)));

    if let Some(name) = name {
        form = form.text("name", name);
    }

    let response = client
        .post(format!("{}/import", projects_url()))
        .multipart(form)
        .send()
        .unwrap_or_else(|e| fail(format!("Failed to import project: {}", e)));

    if !response.status().is_success() {
        fail(error_message("import project", response));
    }

    report_created(response, "Project imported");
}

/// Print the confirmation line and a one-row table for a newly created project.
fn report_created(response: Response, action: &str) {
    let project: serde_json::Value = response
        .json()
        .unwrap_or_else(|e| fail(format!("Failed to parse response as JSON: {}", e)));

    println!("{} successfully", action);
    print_projects(&[project]);
}

/// GET a JSON document from the agent, `None` when it is unreachable or
/// answers with an error.
fn get_json(client: &Client, url: String) -> Option<serde_json::Value> {
    let response = client.get(url).send().ok()?;
    if !response.status().is_success() {
        return None;
    }
    response.json().ok()
}

fn format_bytes(bytes: u64) -> String {
    let mb = bytes as f64 / (1024.0 * 1024.0);
    if mb >= 1024.0 {
        format!("{:.1} GB", mb / 1024.0)
    } else {
        format!("{:.0} MB", mb)
    }
}

/// Report the agent's health from `/health` and its per-project processes from
/// `/agents`. Exits non-zero when the agent cannot be reached, so scripts can
/// use it as a liveness check.
fn agent_status(client: &Client) {
    println!("Endpoint: {}", base_url());

    let Some(health) = get_json(client, format!("{}/health", base_url())) else {
        println!("Status:   not running");
        exit(1);
    };

    println!("Status:   {}", health["status"].as_str().unwrap_or("unknown"));

    let model = health["model_name"].as_str().unwrap_or_default();
    if health["model_loaded"].as_bool().unwrap_or_default() {
        let mut details = Vec::new();
        if let Some(quantization) = health["quantization"].as_str() {
            details.push(quantization.to_string());
        }
        if let Some(n_ctx) = health["n_ctx"].as_u64() {
            details.push(format!("{} ctx", n_ctx));
        }
        if let Some(size) = health["model_size_bytes"].as_u64() {
            details.push(format_bytes(size));
        }
        if details.is_empty() {
            println!("Model:    {}", model);
        } else {
            println!("Model:    {} ({})", model, details.join(", "));
        }
    } else {
        println!("Model:    not loaded");
    }

    let Some(agents) = get_json(client, format!("{}/agents", base_url())) else {
        return;
    };
    if !agents["supervised"].as_bool().unwrap_or_default() {
        return;
    }

    let supervisor = &agents["supervisor"];
    println!(
        "Process:  pid {}, {}",
        supervisor["pid"],
        format_bytes(supervisor["rss_bytes"].as_u64().unwrap_or_default())
    );

    let children = agents["agents"].as_array().cloned().unwrap_or_default();
    if children.is_empty() {
        println!("No project agents running");
        return;
    }

    // Project agents report only their id; show the name alongside it.
    let names: std::collections::HashMap<String, String> =
        get_json(client, projects_url())
            .and_then(|data| data["projects"].as_array().cloned())
            .unwrap_or_default()
            .iter()
            .map(|p| {
                (
                    p["id"].as_str().unwrap_or_default().to_string(),
                    p["name"].as_str().unwrap_or_default().to_string(),
                )
            })
            .collect();

    let mut table = Table::new();
    table.add_row(Row::new(vec![
        Cell::new("Project"),
        Cell::new("PID"),
        Cell::new("Port"),
        Cell::new("Alive"),
        Cell::new("Idle"),
        Cell::new("Memory"),
    ]));

    for agent in &children {
        let id = agent["project_id"].as_str().unwrap_or_default();
        let project = names.get(id).map(String::as_str).unwrap_or(id);
        let alive = if agent["alive"].as_bool().unwrap_or_default() {
            Cell::new("true").with_style(Attr::ForegroundColor(color::GREEN))
        } else {
            Cell::new("false").with_style(Attr::ForegroundColor(color::RED))
        };
        table.add_row(Row::new(vec![
            Cell::new(project),
            Cell::new(&agent["pid"].to_string()),
            Cell::new(&agent["port"].to_string()),
            alive,
            Cell::new(&format!("{:.0}s", agent["idle_seconds"].as_f64().unwrap_or_default())),
            Cell::new(&format_bytes(agent["rss_bytes"].as_u64().unwrap_or_default())),
        ]));
    }

    table.printstd();
}

/// Store a pasted token with the agent, which owns the credential store and
/// uses it for every platform call. Sent as a developer token, the kind meant
/// for the CLI and scripts.
fn login(client: &Client, token: Option<String>) {
    let token = token.unwrap_or_else(|| {
        if !std::io::stdin().is_terminal() {
            let mut input = String::new();
            std::io::stdin()
                .read_line(&mut input)
                .unwrap_or_else(|e| fail(format!("Failed to read token: {}", e)));
            return input;
        }
        rpassword::prompt_password("Paste your token: ")
            .unwrap_or_else(|e| fail(format!("Failed to read token: {}", e)))
    });
    let token = token.trim();
    if token.is_empty() {
        fail("No token given".to_string());
    }

    let response = client
        .post(format!("{}/auth/token", api_url()))
        .json(&serde_json::json!({ "token": token, "type": "developer_token" }))
        .send()
        .unwrap_or_else(|e| fail(format!("Failed to log in: {}", e)));
    if !response.status().is_success() {
        fail(error_message("log in", response));
    }

    let status: serde_json::Value = response
        .json()
        .unwrap_or_else(|e| fail(format!("Failed to parse response as JSON: {}", e)));

    // The agent resolves the user with the token; no user means the platform
    // refused it.
    match status["user"]["email"].as_str() {
        Some(email) => match status["user"]["name"].as_str() {
            Some(name) if !name.is_empty() => println!("Logged in as {} <{}>", name, email),
            _ => println!("Logged in as {}", email),
        },
        None => {
            let _ = client
                .delete(format!("{}/auth/token?type=developer_token", api_url()))
                .send();
            fail("The token was not accepted".to_string());
        }
    }
}

fn logout(client: &Client) {
    let response = client
        .delete(format!("{}/auth/token", api_url()))
        .send()
        .unwrap_or_else(|e| fail(format!("Failed to log out: {}", e)));
    if !response.status().is_success() {
        fail(error_message("log out", response));
    }
    println!("Logged out");
}

fn delete_project(client: &Client, id: String) {
    let response = client
        .delete(format!("{}/{}", projects_url(), id))
        .send()
        .unwrap_or_else(|e| fail(format!("Failed to delete project: {}", e)));

    if !response.status().is_success() {
        fail(error_message("delete project", response));
    }

    println!("Project deleted successfully");
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

/// Build the async SSE client used for chat streaming. Uses `async-openai`'s
/// `eventsource_stream`-based SSE parser (the same approach real OpenAI SDKs
/// use) instead of a hand-rolled line reader over a raw socket.
fn openai_client() -> OpenAIClient<OpenAIConfig> {
    OpenAIClient::with_config(
        OpenAIConfig::new()
            .with_api_base(api_url())
            .with_api_key("not-needed"),
    )
}

/// Stream one chat turn from the service's SSE endpoint, printing content the
/// moment it arrives. Progress/status events are shown on stderr so they do
/// not corrupt the answer. Returns an error message when the stream drops
/// mid-way instead of exiting, so an interactive session can keep going after
/// a server hiccup. Prints a token/throughput summary on stderr at [DONE].
///
/// Long-running tool steps can leave the connection idle long enough for the
/// server (or a proxy in front of it) to drop it, which surfaces as a stream
/// error before any content has streamed. Retry once in that case since a
/// fresh connection usually succeeds.
async fn run_turn(
    client: &OpenAIClient<OpenAIConfig>,
    message: &str,
    project_id: &str,
    session_id: &str,
) -> Result<(), String> {
    match run_turn_once(client, message, project_id, session_id).await {
        Ok(_) => Ok(()),
        Err((e, tokens)) if tokens == 0 => {
            print_status("error", &format!("connection dropped, retrying: {}", e));
            run_turn_once(client, message, project_id, session_id)
                .await
                .map_err(|(e, _)| e)
        }
        Err((e, _)) => Err(e),
    }
}

async fn run_turn_once(
    client: &OpenAIClient<OpenAIConfig>,
    message: &str,
    project_id: &str,
    session_id: &str,
) -> Result<(), (String, u64)> {
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

    let started = std::time::Instant::now();
    let mut stream = client
        .chat()
        .create_stream_byot::<serde_json::Value, serde_json::Value>(body)
        .await
        .map_err(|e| (format!("Failed to run: {}", e), 0))?;

    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    let mut tokens: u64 = 0;
    // Whether the answer so far ends in a newline. A status line printed
    // mid-answer (e.g. "[streaming] Response complete" right after the last
    // token) must start on its own line rather than trail the text.
    let mut at_line_start = true;

    while let Some(event) = stream.next().await {
        let event = event.map_err(|e| (format!("Failed to read stream: {}", e), tokens))?;

        match event["object"].as_str() {
            Some("chat.completion.chunk") => {
                if let Some(content) = event["choices"][0]["delta"]["content"].as_str() {
                    tokens += 1;
                    let _ = out.write_all(content.as_bytes());
                    let _ = out.flush();
                    if !content.is_empty() {
                        at_line_start = content.ends_with('\n');
                    }
                }
                if event["choices"][0]["finish_reason"].is_string() {
                    break;
                }
            }
            Some("chat.status") => match event["message"].as_str() {
                Some(message) if !message.trim().is_empty() => {
                    if !at_line_start {
                        let _ = writeln!(out);
                        let _ = out.flush();
                        at_line_start = true;
                    }
                    print_status(event["step"].as_str().unwrap_or_default(), message.trim());
                }
                _ => {}
            },
            _ => {}
        }
    }

    if !at_line_start {
        let _ = writeln!(out);
    }
    let elapsed = started.elapsed().as_secs_f64();
    let tokens_per_sec = if elapsed > 0.0 && tokens > 0 {
        tokens as f64 / elapsed
    } else {
        0.0
    };
    print_status(
        "stats",
        &format!("{} tokens, {:.1} tok/s, {:.0}s", tokens, tokens_per_sec, elapsed),
    );

    Ok(())
}

/// Interactive chat with the local agent: turn a reply, then keep reading new
/// prompts from stdin until EOF, `/quit`, or `exit`. One `session_id` underpins
/// the whole conversation, so the service keeps the context across turns.
async fn run_chat(
    client: &OpenAIClient<OpenAIConfig>,
    first_prompt: Option<String>,
    project_id: String,
    session: Option<String>,
) {
    let session_id = session.unwrap_or_else(|| {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default();
        format!(
            "cli-{}-{}",
            nanos,
            std::process::id()
        )
    });
    eprintln!("session: {}", session_id);

    let stdin = std::io::stdin();

    if let Some(prompt) = first_prompt.filter(|p| !p.trim().is_empty()) {
        println!("> {}", prompt);
        if let Err(e) = run_turn(client, &prompt, &project_id, &session_id).await {
            if std::io::stdin().is_terminal() {
                eprintln!("{}", e);
            } else {
                fail(e);
            }
        }
    }

    loop {
        print!("> ");
        let _ = std::io::stdout().flush();

        let mut input = String::new();
        if stdin
            .read_line(&mut input)
            .unwrap_or_else(|e| fail(format!("Failed to read input: {}", e)))
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

        if let Err(e) = run_turn(client, input, &project_id, &session_id).await {
            if std::io::stdin().is_terminal() {
                eprintln!("{}", e);
            } else {
                fail(e);
            }
        }
    }
}

fn main() {
    let args = Args::parse();

    match args.command {
        Commands::Login { token } => login(&agent_client(), token),
        Commands::Logout => logout(&agent_client()),
        Commands::Project { command } => {
            let client = agent_client();
            match command {
                ProjectCommands::List => list_projects(&client),
                ProjectCommands::Create { name, description, import } => {
                    match import {
                        Some(path) => import_project(&client, path, name),
                        None => match name {
                            Some(name) => create_project(&client, name, description),
                            None => fail(
                                "--name is required when creating a project without --import".to_string(),
                            ),
                        },
                    }
                }
                ProjectCommands::Delete { id } => delete_project(&client, id),
            }
        }
        Commands::Agent { command } => match command {
            AgentCommands::Status => agent_status(&Client::new()),
            AgentCommands::Start => {
                if !is_local() {
                    fail("SMARTLOOP_API_URL is set; start that agent where it runs".to_string());
                }
                let client = Client::new();
                framework::start(&client, &base_url());
                framework::ensure_running(&client, &base_url(), true);
            }
            AgentCommands::Stop => framework::stop(),
        },
        Commands::Run { prompt, project, session } => {
            // Resolved before the async runtime starts: the blocking client
            // must not run inside it.
            let client = agent_client();
            let project = project.unwrap_or_else(|| select_project(&client));
            let runtime = tokio::runtime::Runtime::new()
                .unwrap_or_else(|e| fail(format!("Failed to start async runtime: {}", e)));
            let chat = openai_client();
            runtime.block_on(run_chat(&chat, prompt, project, session));
        }
    }
}
