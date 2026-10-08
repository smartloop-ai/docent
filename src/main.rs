use std::io::{IsTerminal, Write};
use std::process::exit;

use clap::{CommandFactory, FromArgMatches, Parser, Subcommand};
use prettytable::{Attr, Cell, Row, Table, color};
use reqwest::blocking::{Client, Response, multipart};

use progress::Steps;

mod agent;
mod chat;
mod framework;
mod mcp;
mod progress;
mod tui;
mod usage;

/// The Docent wordmark: `--help`, the setup checklist and the welcome card.
/// `install.sh` prints its own copy.
pub const BANNER: [&str; 3] = [
    "█▀▀▀▄ ▄▀▀▀▄ ▄▀▀▀▀ █▀▀▀▀ █▄  █ ▀▀█▀▀",
    "█   █ █   █ █     █▀▀▀  █ ▀▄█   █",
    "▀▀▀▀   ▀▀▀   ▀▀▀▀ ▀▀▀▀▀ ▀   ▀   ▀",
];

#[derive(Parser)]
#[command(
    name = "smartloop", 
    version, 
    author,
    about=format!("\n{}\n\nDocent by Smartloop: your private AI assistant", BANNER.join("\n")),
)]

struct Args {
    /// Without one, `docent` opens the chat, as `docent run` does.
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    /// Sign in through app.smartloop.ai in the browser, or with a token
    Login {
        /// Sign in with a token instead; prompts for it (input hidden) when
        /// given without a value
        #[arg(long, num_args = 0..=1, default_missing_value = "")]
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
    /// List, enable, and disable the models the orchestrator can pick
    Model {
        #[command(subcommand)]
        command: ModelCommands,
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
        /// Chat line by line instead of in the full-screen app; the default
        /// when stdin or stdout isn't a terminal
        #[arg(long)]
        plain: bool,
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
enum ModelCommands {
    /// List available models and whether each is downloaded and enabled
    List {
        /// Project ID; defaults to the current project
        #[arg(long, short)]
        project: Option<String>,
    },
    /// Enable a model for a project, downloading it first when needed
    Enable {
        /// Model name, as shown by `docent model list`
        name: String,
        /// Project ID; defaults to the current project
        #[arg(long, short)]
        project: Option<String>,
    },
    /// Disable a model for a project and unload it
    Disable {
        /// Model name, as shown by `docent model list`
        name: String,
        /// Project ID; defaults to the current project
        #[arg(long, short)]
        project: Option<String>,
    },
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
pub fn base_url() -> String {
    let base = std::env::var("SMARTLOOP_API_URL")
        .unwrap_or_else(|_| format!("http://localhost:{}", framework::port()));
    base.trim_end_matches('/').to_string()
}

/// True when the CLI talks to the agent it manages itself: the default local
/// endpoint rather than a `SMARTLOOP_API_URL` someone else runs.
pub fn is_local() -> bool {
    std::env::var("SMARTLOOP_API_URL").is_err()
}

/// Client for talking to the agent, after making sure one is up. Starts the
/// local framework (installing it on first use) when nothing answers.
fn agent_client() -> Client {
    let client = Client::new();
    framework::ensure_running(&client, &base_url(), is_local());
    client
}

pub fn api_url() -> String {
    format!("{}/v1", base_url())
}

fn projects_url() -> String {
    format!("{}/projects", api_url())
}

/// Print an error and stop; used instead of panicking so failures read as
/// CLI output rather than a Rust backtrace.
pub fn fail(message: String) -> ! {
    if std::io::stderr().is_terminal() {
        eprintln!("\x1b[1;31mError:\x1b[0m {}", message);
    } else {
        eprintln!("Error: {}", message);
    }
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
    try_fetch_projects(client).unwrap_or_else(|e| fail(e))
}

pub fn try_fetch_projects(client: &Client) -> Result<Vec<serde_json::Value>, String> {
    let mut data = request_json(client, projects_url(), "list projects")?;
    match data["projects"].take() {
        serde_json::Value::Array(projects) => Ok(projects),
        _ => Err("Expected projects to be an array".to_string()),
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
        fail("No projects found; create one with `docent project create`".to_string());
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
    report_created(try_create_project(client, &name, description).unwrap_or_else(|e| fail(e)), "Project created");
}

/// Create an empty project and return it as the agent lists it.
pub fn try_create_project(client: &Client, name: &str, description: Option<String>) -> Result<serde_json::Value, String> {
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
        .map_err(|e| format!("Failed to create project: {}", e))?;

    if !response.status().is_success() {
        return Err(error_message("create project", response));
    }

    response.json().map_err(|e| format!("Failed to parse response as JSON: {}", e))
}

/// Upload a file for a chat message to reference: the agent turns documents
/// into markdown and returns the asset's id for `message.attachments`.
pub fn upload_asset(client: &Client, path: &std::path::Path) -> Result<String, String> {
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("file").to_string();
    let form = multipart::Form::new()
        .file("file", path)
        .map_err(|e| format!("Failed to read {}: {}", path.display(), e))?;
    let response = client
        .post(format!("{}/assets", api_url()))
        .multipart(form)
        .send()
        .map_err(|e| format!("Failed to upload {}: {}", name, e))?;
    if !response.status().is_success() {
        return Err(error_message(&format!("upload {}", name), response));
    }
    let asset: serde_json::Value = response
        .json()
        .map_err(|e| format!("Failed to parse response as JSON: {}", e))?;
    asset["asset_id"]
        .as_str()
        .map(str::to_string)
        .ok_or_else(|| format!("The agent returned no asset id for {}", name))
}

/// Create a project from a zip archive produced by an earlier export.
fn import_project(client: &Client, path: String, name: Option<String>) {
    report_created(try_import_project(client, &path, name).unwrap_or_else(|e| fail(e)), "Project imported");
}

/// Import a project archive and return the new project.
pub fn try_import_project(client: &Client, path: &str, name: Option<String>) -> Result<serde_json::Value, String> {
    let mut form = multipart::Form::new()
        .file("file", path)
        .map_err(|e| format!("Failed to read {}: {}", path, e))?;

    if let Some(name) = name {
        form = form.text("name", name);
    }

    let response = client
        .post(format!("{}/import", projects_url()))
        .multipart(form)
        .send()
        .map_err(|e| format!("Failed to import project: {}", e))?;

    if !response.status().is_success() {
        return Err(error_message("import project", response));
    }

    response.json().map_err(|e| format!("Failed to parse response as JSON: {}", e))
}

/// Print the confirmation line and a one-row table for a newly created project.
fn report_created(project: serde_json::Value, action: &str) {
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

    let pid = if is_local() { framework::agent_pid() } else { None };
    let Some(health) = get_json(client, format!("{}/health", base_url())) else {
        match pid {
            // The process is up but not answering: still starting, or hung.
            Some(pid) => println!("Status:   not responding (pid {})", pid),
            None => println!("Status:   not running"),
        }
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

/// The given project, or the server's current one (the first when none is
/// marked current).
fn resolve_project(client: &Client, project: Option<String>) -> String {
    if let Some(project) = project {
        return project;
    }
    let projects = fetch_projects(client);
    projects
        .iter()
        .find(|p| p["current"].as_bool().unwrap_or_default())
        .or_else(|| projects.first())
        .and_then(|p| p["id"].as_str())
        .map(str::to_string)
        .unwrap_or_else(|| fail("No projects found; create one with `docent project create`".to_string()))
}

fn project_models_url(project_id: &str) -> String {
    format!("{}/{}/models", projects_url(), project_id)
}

fn get_json_or_fail(client: &Client, url: String, action: &str) -> serde_json::Value {
    request_json(client, url, action).unwrap_or_else(|e| fail(e))
}

fn request_json(client: &Client, url: String, action: &str) -> Result<serde_json::Value, String> {
    let response = client
        .get(url)
        .send()
        .map_err(|e| format!("Failed to {}: {}", action, e))?;
    if !response.status().is_success() {
        return Err(error_message(action, response));
    }
    response
        .json()
        .map_err(|e| format!("Failed to parse response as JSON: {}", e))
}

/// The project's registered models, keyed by name: the downloaded/enabled
/// state the catalog doesn't carry.
fn project_model_state(
    client: &Client,
    project_id: &str,
) -> Result<std::collections::HashMap<String, serde_json::Value>, String> {
    Ok(request_json(client, project_models_url(project_id), "list project models")?
        .as_array()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .map(|m| (m["model_name"].as_str().unwrap_or_default().to_string(), m))
        .collect())
}

/// One model as `model list` and the TUI show it.
#[derive(Debug, Clone)]
pub struct ModelRow {
    pub name: String,
    pub capabilities: String,
    pub access: String,
    /// False when the model needs a sign-in first.
    pub accessible: bool,
    pub downloaded: bool,
    pub enabled: bool,
}

/// The catalog (`/v1/models`) joined with the project's state. A model the
/// project has never registered is neither downloaded nor enabled for it.
pub fn model_rows(client: &Client, project_id: &str) -> Result<Vec<ModelRow>, String> {
    let catalog = request_json(client, format!("{}/models", api_url()), "list models")?;
    let state = project_model_state(client, project_id)?;
    Ok(catalog
        .as_array()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .map(|model| {
            let name = model["name"].as_str().unwrap_or_default().to_string();
            let entry = state.get(&name);
            let flag = |key: &str| entry.and_then(|m| m[key].as_bool()).unwrap_or_default();
            ModelRow {
                capabilities: model["capabilities"]
                    .as_array()
                    .map(|c| c.iter().filter_map(|v| v.as_str()).collect::<Vec<_>>().join(", "))
                    .unwrap_or_default(),
                access: model["access_level"].as_str().unwrap_or_default().to_string(),
                accessible: model["is_accessible"].as_bool() != Some(false),
                downloaded: flag("downloaded"),
                enabled: flag("enabled"),
                name,
            }
        })
        .collect())
}

fn list_models(client: &Client, project_id: &str) {
    let rows = model_rows(client, project_id).unwrap_or_else(|e| fail(e));

    let flag = |on: bool| {
        if on {
            Cell::new("true").with_style(Attr::ForegroundColor(color::GREEN))
        } else {
            Cell::new("false")
        }
    };

    let mut table = Table::new();
    table.add_row(Row::new(vec![
        Cell::new("Name"),
        Cell::new("Capabilities"),
        Cell::new("Access"),
        Cell::new("Downloaded"),
        Cell::new("Enabled"),
    ]));

    for row in &rows {
        let access = if row.accessible {
            Cell::new(&row.access)
        } else {
            Cell::new(&format!("{} (sign in)", row.access)).with_style(Attr::ForegroundColor(color::YELLOW))
        };
        table.add_row(Row::new(vec![
            Cell::new(&row.name),
            Cell::new(&row.capabilities),
            access,
            flag(row.downloaded),
            flag(row.enabled),
        ]));
    }

    table.printstd();
}

/// PATCH a project's model entry; the server canonicalizes the name.
pub fn patch_project_model(client: &Client, project_id: &str, name: &str, enabled: bool) -> Result<(), String> {
    let action = if enabled { "enable model" } else { "disable model" };
    let response = client
        .patch(format!("{}/{}", project_models_url(project_id), name))
        .json(&serde_json::json!({ "enabled": enabled }))
        .send()
        .map_err(|e| format!("Failed to {}: {}", action, e))?;
    if !response.status().is_success() {
        return Err(error_message(action, response));
    }
    Ok(())
}

/// Make `project_id` the agent's current project, as the studio app does on
/// picking one: `/v1/models/load` records the selection (which a client
/// that names no project gets, and which the next launch opens in) and loads
/// the project's model. Loading can take a while, so this gets longer than
/// the client's usual 30 seconds.
pub fn set_current_project(client: &Client, project_id: &str) -> Result<(), String> {
    let action = "switch project";
    let response = client
        .post(format!("{}/models/load", api_url()))
        .json(&serde_json::json!({ "project_id": project_id }))
        .timeout(std::time::Duration::from_secs(600))
        .send()
        .map_err(|e| format!("Failed to {}: {}", action, e))?;
    if !response.status().is_success() {
        return Err(error_message(action, response));
    }
    Ok(())
}

/// Whether web search is on for the project: the agent's own per-project
/// switch, which the studio app toggles with Cmd/Ctrl+Alt+S.
pub fn web_search_enabled(client: &Client, project_id: &str) -> Result<bool, String> {
    let reply = request_json(client, format!("{}/{}/websearch", projects_url(), project_id), "read web search")?;
    Ok(reply["enabled"].as_bool().unwrap_or_default())
}

/// Turn web search on or off for the project; returns the new state.
pub fn set_web_search(client: &Client, project_id: &str, enabled: bool) -> Result<bool, String> {
    let action = if enabled { "turn on web search" } else { "turn off web search" };
    let response = client
        .patch(format!("{}/{}/websearch", projects_url(), project_id))
        .json(&serde_json::json!({ "enabled": enabled }))
        .send()
        .map_err(|e| format!("Failed to {}: {}", action, e))?;
    if !response.status().is_success() {
        return Err(error_message(action, response));
    }
    let reply: serde_json::Value = response.json().map_err(|e| format!("Failed to {}: {}", action, e))?;
    Ok(reply["enabled"].as_bool().unwrap_or(enabled))
}

/// Same sequence as the studio app's model toggle: weights are fetched
/// through `/v1/init` (scoped to the project) when they aren't on disk yet,
/// and only then is the model switched on, so it is never enabled without
/// weights.
fn enable_model(client: &Client, project_id: &str, name: &str) {
    let mut list = progress::Checklist::new();
    enable_steps(&mut list, client, project_id, name);
    // Clear the progress bar before printing below it.
    drop(list);
    println!("Model {} enabled", name);
}

/// The steps of enabling `name`, reported on `list`: the checklist, or the
/// TUI's downloads panel.
pub fn enable_steps(list: &mut dyn Steps, client: &Client, project_id: &str, name: &str) {
    let download = list.add(&format!("Download {}", name));
    let enable = list.add("Enable for project");

    let downloaded = project_model_state(client, project_id)
        .unwrap_or_else(|e| list.fail(download, e))
        .get(name)
        .and_then(|m| m["downloaded"].as_bool())
        .unwrap_or_default();
    if downloaded {
        list.set_detail(download, "downloaded");
        list.done(download);
    } else {
        list.start(download);
        framework::stream_progress(
            &base_url(),
            "/v1/init",
            serde_json::json!({ "model_name": name, "project_id": project_id }),
            list,
            download,
            &|status| match status {
                "downloading" => framework::Stage::Start(download),
                "download_complete" => framework::Stage::Done(download),
                "model_ready" => framework::Stage::Present(download),
                _ => framework::Stage::Other,
            },
        );
        list.finish_before(enable);
    }

    list.start(enable);
    if let Err(e) = patch_project_model(client, project_id, name, true) {
        list.fail(enable, e);
    }
    list.done(enable);
}

fn disable_model(client: &Client, project_id: &str, name: &str) {
    // A model the project never registered has nothing to switch off; the
    // PATCH would answer 404 for it.
    let registered = project_model_state(client, project_id)
        .unwrap_or_else(|e| fail(e))
        .contains_key(name);
    let catalog_has = get_json_or_fail(client, format!("{}/models", api_url()), "list models")
        .as_array()
        .is_some_and(|c| c.iter().any(|m| m["name"] == name));
    if !registered && catalog_has {
        println!("Model {} is not enabled", name);
        return;
    }
    patch_project_model(client, project_id, name, false).unwrap_or_else(|e| fail(e));
    println!("Model {} disabled", name);
}

/// Sign in: in the browser by default, or with a token from `--token` or
/// piped in. The agent owns the credential store and uses it for every
/// platform call.
fn login(client: &Client, token: Option<String>) {
    if token.is_none() && std::io::stdin().is_terminal() {
        let result = browser_login(client, |url| {
            println!("Opening {} to sign in…", url);
            println!("Waiting for the browser; ctrl+c to cancel");
        });
        match result {
            Ok(who) => println!("{}", who),
            Err(e) => fail(e),
        }
        return;
    }
    let token = token.filter(|t| !t.is_empty()).unwrap_or_else(|| {
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
    match try_login(client, &token) {
        Ok(who) => println!("{}", who),
        Err(e) => fail(e),
    }
}

/// Send `token` to the agent as a developer token, the kind meant for the
/// CLI and scripts. Returns "Logged in as …" for the user it resolves to.
pub fn try_login(client: &Client, token: &str) -> Result<String, String> {
    let token = token.trim();
    if token.is_empty() {
        return Err("No token given".to_string());
    }

    let response = client
        .post(format!("{}/auth/token", api_url()))
        .json(&serde_json::json!({ "token": token, "type": "developer_token" }))
        .send()
        .map_err(|e| format!("Failed to log in: {}", e))?;
    if !response.status().is_success() {
        return Err(error_message("log in", response));
    }

    let status: serde_json::Value = response
        .json()
        .map_err(|e| format!("Failed to parse response as JSON: {}", e))?;

    // The agent resolves the user with the token; no user means the platform
    // refused it.
    match logged_in_as(&status) {
        Some(who) => Ok(who),
        None => {
            let _ = client
                .delete(format!("{}/auth/token?type=developer_token", api_url()))
                .send();
            Err("The token was not accepted".to_string())
        }
    }
}

/// "Logged in as …" for the user an `/auth/status` reply names.
fn logged_in_as(status: &serde_json::Value) -> Option<String> {
    let email = status["user"]["email"].as_str()?;
    Some(match status["user"]["name"].as_str() {
        Some(name) if !name.is_empty() => format!("Logged in as {} <{}>", name, email),
        _ => format!("Logged in as {}", email),
    })
}

/// The web app the browser signs in on, overridable for staging.
pub fn app_url() -> String {
    std::env::var("SMARTLOOP_APP_URL")
        .unwrap_or_else(|_| "https://app.smartloop.ai".to_string())
        .trim_end_matches('/')
        .to_string()
}

/// How long the browser has to finish signing in.
const BROWSER_LOGIN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);

/// Sign in on app.smartloop.ai, as the desktop app does: its login page, in
/// desktop mode, hands the tokens to the agent's `/v1/auth/callback` on this
/// machine, so all that's left here is to wait for them to land. `on_url`
/// is told the page opened, for when no browser comes up.
pub fn browser_login(client: &Client, on_url: impl FnOnce(&str)) -> Result<String, String> {
    let status = request_json(client, format!("{}/auth/status", api_url()), "read sign-in status")?;
    if status["has_access_token"].as_bool().unwrap_or_default() {
        if let Some(who) = logged_in_as(&status) {
            return Ok(format!("{}; log out first to switch accounts", who));
        }
    }

    // The page posts to 127.0.0.1, so it has to be an agent on this machine.
    let base = base_url();
    let host = base.split("://").nth(1).unwrap_or(&base);
    let (name, port) = host.rsplit_once(':').unwrap_or((host, "80"));
    if !matches!(name, "localhost" | "127.0.0.1") {
        return Err(format!("Browser sign-in needs an agent on this machine, not {}; use --token", base));
    }
    let url = format!("{}/login?mode=desktop&port={}", app_url(), port);
    on_url(&url);
    open_browser(&url);

    let started = std::time::Instant::now();
    while started.elapsed() < BROWSER_LOGIN_TIMEOUT {
        std::thread::sleep(std::time::Duration::from_secs(2));
        let Ok(status) = request_json(client, format!("{}/auth/status", api_url()), "read sign-in status") else {
            continue;
        };
        if status["has_access_token"].as_bool().unwrap_or_default() {
            if let Some(who) = logged_in_as(&status) {
                return Ok(who);
            }
        }
    }
    Err("Timed out waiting for the browser sign-in".to_string())
}

/// Where `/upgrade` goes: the web app's checkout hand-off, which sends a
/// free account to Stripe checkout and a subscribed one to its billing
/// portal, signing in first if needed.
pub const UPGRADE_URL: &str = "https://app.smartloop.ai/upgrade";

/// Open `url` in the default browser. Best effort: the URL is shown too.
pub fn open_browser(url: &str) {
    let mut command = if cfg!(target_os = "macos") {
        std::process::Command::new("open")
    } else if cfg!(windows) {
        // Not `cmd /C start`, which would split the URL at its `&`.
        let mut c = std::process::Command::new("rundll32");
        c.arg("url.dll,FileProtocolHandler");
        c
    } else {
        std::process::Command::new("xdg-open")
    };
    let _ = command
        .arg(url)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

fn logout(client: &Client) {
    try_logout(client).unwrap_or_else(|e| fail(e));
    println!("Logged out");
}

/// What `/status` in the chat app shows, as label and value: who's signed
/// in, the agent and its health, the loaded model and the agent process.
/// The account can take a platform round trip; this runs off the UI.
pub fn status_rows(client: &Client) -> Vec<(String, String)> {
    let mut rows = Vec::new();
    let account = request_json(client, format!("{}/auth/status", api_url()), "read sign-in status")
        .ok()
        .and_then(|status| {
            let user = &status["user"];
            user["email"].as_str().map(|email| match user["name"].as_str() {
                Some(name) if !name.is_empty() => format!("{} <{}>", name, email),
                _ => email.to_string(),
            })
        })
        .unwrap_or_else(|| "not signed in · /login".to_string());
    rows.push(("account".to_string(), account));

    let health = get_json(client, format!("{}/health", base_url()));
    let state = health.as_ref().and_then(|h| h["status"].as_str()).unwrap_or("not responding");
    rows.push(("agent".to_string(), format!("{} · {}", api_url(), state)));

    let model = health.as_ref().map(|h| {
        if !h["model_loaded"].as_bool().unwrap_or_default() {
            return "not loaded".to_string();
        }
        let mut details = Vec::new();
        if let Some(quantization) = h["quantization"].as_str() {
            details.push(quantization.to_string());
        }
        if let Some(n_ctx) = h["n_ctx"].as_u64() {
            details.push(format!("{} ctx", n_ctx));
        }
        if let Some(size) = h["model_size_bytes"].as_u64() {
            details.push(format_bytes(size));
        }
        let name = h["model_name"].as_str().unwrap_or_default();
        if details.is_empty() { name.to_string() } else { format!("{} ({})", name, details.join(", ")) }
    });
    rows.push(("model".to_string(), model.unwrap_or_else(|| "unknown".to_string())));

    if let Some(agents) = get_json(client, format!("{}/agents", base_url()))
        && agents["supervised"].as_bool().unwrap_or_default()
    {
        let supervisor = &agents["supervisor"];
        let children = agents["agents"].as_array().map_or(0, Vec::len);
        rows.push((
            "process".to_string(),
            format!(
                "pid {} · {} · {} project agent{}",
                supervisor["pid"],
                format_bytes(supervisor["rss_bytes"].as_u64().unwrap_or_default()),
                children,
                if children == 1 { "" } else { "s" }
            ),
        ));
    }
    rows.push((
        "version".to_string(),
        format!("CLI {} · agent {}", env!("CARGO_PKG_VERSION"), framework::VERSION),
    ));
    rows
}

pub fn try_logout(client: &Client) -> Result<(), String> {
    let response = client
        .delete(format!("{}/auth/token", api_url()))
        .send()
        .map_err(|e| format!("Failed to log out: {}", e))?;
    if !response.status().is_success() {
        return Err(error_message("log out", response));
    }
    Ok(())
}

fn delete_project(client: &Client, id: String) {
    try_delete_project(client, &id).unwrap_or_else(|e| fail(e));
    println!("Project deleted successfully");
}

pub fn try_delete_project(client: &Client, id: &str) -> Result<(), String> {
    let response = client
        .delete(format!("{}/{}", projects_url(), id))
        .send()
        .map_err(|e| format!("Failed to delete project: {}", e))?;

    if !response.status().is_success() {
        return Err(error_message("delete project", response));
    }
    Ok(())
}

fn main() {
    // The agent and its workers run from this same binary.
    let argv: Vec<String> = std::env::args().collect();
    if let Some(code) = agent::dispatch(&argv) {
        exit(code);
    }

    // Installed as both `smartloop` and `docent`: name it as it was run.
    let docent = std::env::args_os()
        .next()
        .and_then(|a| std::path::Path::new(&a).file_stem().map(|s| s == "docent"))
        .unwrap_or_default();
    let name = if docent { "docent" } else { "smartloop" };
    let args = Args::from_arg_matches(&Args::command().name(name).bin_name(name).get_matches())
        .unwrap_or_else(|e| e.exit());

    // Bare `smartloop` is `smartloop run`.
    let command = args.command.unwrap_or(Commands::Run { prompt: None, project: None, session: None, plain: false });
    match command {
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
        Commands::Model { command } => {
            let client = agent_client();
            match command {
                ModelCommands::List { project } => {
                    list_models(&client, &resolve_project(&client, project))
                }
                ModelCommands::Enable { name, project } => {
                    enable_model(&client, &resolve_project(&client, project), &name)
                }
                ModelCommands::Disable { name, project } => {
                    disable_model(&client, &resolve_project(&client, project), &name)
                }
            }
        }
        Commands::Agent { command } => match command {
            AgentCommands::Status => agent_status(&Client::new()),
            AgentCommands::Start => {
                if !is_local() {
                    fail("SMARTLOOP_API_URL is set; start that agent where it runs".to_string());
                }
                framework::start(&Client::new(), &base_url());
            }
            AgentCommands::Stop => framework::stop(),
        },
        Commands::Run { prompt, project, session, plain } => {
            let terminal = std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
            if terminal && !plain {
                // The app shows setup itself, so the agent may still be down.
                let client = Client::new();
                tui::run(&client, prompt, project, session);
                return;
            }
            // Resolved before the async runtime starts: the blocking client
            // must not run inside it.
            let client = agent_client();
            let project = project.unwrap_or_else(|| select_project(&client));
            let runtime = tokio::runtime::Runtime::new()
                .unwrap_or_else(|e| fail(format!("Failed to start async runtime: {}", e)));
            let chat = chat::openai_client(api_url());
            runtime.block_on(chat::run_plain(&chat, prompt, project, session));
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_installer_prints_the_same_banner() {
        let install = include_str!("../install.sh");
        for row in super::BANNER {
            assert!(install.contains(row), "install.sh is missing {:?}", row);
        }
    }
}
