//! MCP servers for `/mcp`: the project's connections, as the agent keeps
//! them, and adding one by its URL.
//!
//! Registration goes through the agent's `/v1/projects/{id}/mcp/register`,
//! the same call the studio app makes. A server that takes the connection
//! as is (no auth, or a `?token=` in the URL) is saved straight away; one
//! that wants OAuth answers with an `auth_url` instead, and the sign-in
//! finishes on the agent's own callback, so all the CLI does is open the
//! browser and watch the list. Either way tools are discovered in the
//! background: a server shows `connecting` until its `indexing.state` turns
//! `ready` or `failed`.

use std::time::{Duration, Instant};

use reqwest::blocking::Client;

/// Registering waits on the server's handshake, and OAuth discovery may
/// take a few round trips to a third party.
const REGISTER_TIMEOUT: Duration = Duration::from_secs(120);
/// How long to watch a new server come up: long enough to finish an OAuth
/// sign-in in the browser.
const WATCH_FOR: Duration = Duration::from_secs(300);
const POLL_EVERY: Duration = Duration::from_secs(2);

/// One server as the `/mcp` panel shows it.
#[derive(Clone, Debug, PartialEq)]
pub struct Server {
    pub id: String,
    pub name: String,
    /// The URL for a remote server, the command line for a local one.
    pub location: String,
    /// `provisioning`, `ready` or `failed`.
    pub state: String,
    pub tools: usize,
    pub error: Option<String>,
}

impl Server {
    fn from_json(server: &serde_json::Value) -> Self {
        let text = |key: &str| server[key].as_str().unwrap_or_default().to_string();
        let location = match server["server_url"].as_str() {
            Some(url) => url.to_string(),
            None => {
                let args = server["args"].as_array().into_iter().flatten().filter_map(|a| a.as_str());
                std::iter::once(text("command").as_str()).chain(args).collect::<Vec<_>>().join(" ")
            }
        };
        let indexing = &server["indexing"];
        Server {
            id: text("id"),
            name: text("name"),
            location: location.trim().to_string(),
            state: indexing["state"].as_str().unwrap_or("ready").to_string(),
            tools: indexing["tools"]
                .as_u64()
                .map(|n| n as usize)
                .unwrap_or_else(|| server["tools"].as_array().map_or(0, Vec::len)),
            error: indexing["error"].as_str().or(server["last_error"].as_str()).map(str::to_string),
        }
    }

    pub fn connecting(&self) -> bool {
        self.state == "provisioning"
    }
}

/// What registering a URL came back with.
pub enum Added {
    /// Saved; tools are being discovered.
    Server(Server),
    /// The server wants a sign-in at this URL first.
    SignIn(String),
}

fn project_url(project_id: &str) -> String {
    format!("{}/projects/{}/mcp", crate::api_url(), project_id)
}

pub fn list(client: &Client, project_id: &str) -> Result<Vec<Server>, String> {
    let reply = crate::request_json(client, project_url(project_id), "list MCP servers")?;
    Ok(reply["servers"].as_array().into_iter().flatten().map(Server::from_json).collect())
}

/// Register the server at `url` with the project.
pub fn add(client: &Client, project_id: &str, url: &str) -> Result<Added, String> {
    let action = "add the MCP server";
    let response = client
        .post(format!("{}/register", project_url(project_id)))
        .timeout(REGISTER_TIMEOUT)
        .json(&serde_json::json!({ "server_type": "remote", "server_url": url }))
        .send()
        .map_err(|e| format!("Failed to {}: {}", action, e))?;
    if !response.status().is_success() {
        return Err(crate::error_message(action, response));
    }
    let reply: serde_json::Value = response.json().map_err(|e| format!("Failed to {}: {}", action, e))?;
    if let Some(auth_url) = reply["auth_url"].as_str() {
        return Ok(Added::SignIn(auth_url.to_string()));
    }
    match &reply["server"] {
        server if server.is_object() => Ok(Added::Server(Server::from_json(server))),
        _ => Err(format!("Failed to {}: the agent sent no server back", action)),
    }
}

pub fn remove(client: &Client, project_id: &str, server_id: &str) -> Result<(), String> {
    let action = "remove the MCP server";
    let response = client
        .delete(format!("{}/{}", project_url(project_id), server_id))
        .send()
        .map_err(|e| format!("Failed to {}: {}", action, e))?;
    if !response.status().is_success() {
        return Err(crate::error_message(action, response));
    }
    Ok(())
}

/// Wait for the server at `url` (or with `id`, once known) to finish
/// connecting. `None` when it never showed up or never settled in time.
pub fn watch(client: &Client, project_id: &str, url: &str, id: Option<&str>) -> Option<Server> {
    let started = Instant::now();
    while started.elapsed() < WATCH_FOR {
        std::thread::sleep(POLL_EVERY);
        let Ok(servers) = list(client, project_id) else { continue };
        let found = servers.into_iter().find(|s| match id {
            Some(id) => s.id == id,
            None => same_url(&s.location, url),
        });
        if let Some(server) = found.filter(|s| !s.connecting()) {
            return Some(server);
        }
    }
    None
}

/// The agent keeps a URL without its `?token=`, and may drop a trailing
/// slash.
fn same_url(saved: &str, typed: &str) -> bool {
    let bare = |u: &str| u.split('?').next().unwrap_or_default().trim_end_matches('/').to_ascii_lowercase();
    bare(saved) == bare(typed)
}

/// `/mcp add <url>`: only web URLs; a local server is set up in the studio
/// app or with a definition file.
pub fn parse_url(text: &str) -> Result<String, String> {
    let url = text.trim();
    if url.is_empty() {
        return Err("Usage: /mcp add <url>".to_string());
    }
    let parsed = reqwest::Url::parse(url).map_err(|_| format!("Not a URL: {}", url))?;
    if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().is_none() {
        return Err(format!("Not an http(s) URL: {}", url));
    }
    Ok(url.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_a_listed_server() {
        let remote = Server::from_json(&serde_json::json!({
            "id": "s1", "name": "Linear", "server_url": "https://mcp.linear.app/mcp",
            "tools": [{}, {}], "indexing": { "state": "ready", "in_progress": false, "tools": 2, "error": null },
        }));
        assert_eq!(remote.location, "https://mcp.linear.app/mcp");
        assert_eq!((remote.state.as_str(), remote.tools, remote.error), ("ready", 2, None));

        let local = Server::from_json(&serde_json::json!({
            "id": "s2", "name": "fetch", "command": "uvx", "args": ["mcp-server-fetch"],
            "indexing": { "state": "provisioning", "tools": 0 },
        }));
        assert_eq!(local.location, "uvx mcp-server-fetch");
        assert!(local.connecting());
    }

    #[test]
    fn matches_the_saved_url() {
        assert!(same_url("https://example.com/mcp", "https://Example.com/mcp/?token=abc"));
        assert!(!same_url("https://example.com/mcp", "https://example.com/other"));
    }

    #[test]
    fn takes_only_web_urls() {
        assert_eq!(parse_url(" https://example.com/mcp ").unwrap(), "https://example.com/mcp");
        assert!(parse_url("").is_err());
        assert!(parse_url("example.com").is_err());
        assert!(parse_url("file:///etc/passwd").is_err());
    }
}
