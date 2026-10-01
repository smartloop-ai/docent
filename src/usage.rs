//! Usage for `/usage`: web search, read from the platform (api.smartloop.ai),
//! and tokens, read from the agent's own log of every turn.
//!
//! Web search usage is read from the platform (api.smartloop.ai) rather than
//! the agent: `GET /v1/websearch/status` carries the month's allowance and
//! what has been spent of it, which the agent's own gate reduces to a yes/no.
//!
//! The CLI holds no token of its own. The agent keeps the signed-in one in
//! `SLP_HOME/.credentials`, Fernet-encrypted under `SLP_HOME/.key`, so it is
//! read from there with the same preference the agent has (access token,
//! then developer token). Refreshing it stays the agent's job: on a 401 the
//! agent's `/v1/auth/status` is asked once, which redeems the refresh token,
//! and the file is read again.

use reqwest::blocking::Client;
use std::time::Duration;

/// The platform API, overridable as the agent's `SLP_API_BASE` is.
fn platform_url() -> String {
    std::env::var("SLP_API_BASE")
        .unwrap_or_else(|_| "https://api.smartloop.ai".to_string())
        .trim_end_matches('/')
        .to_string()
}

/// The status call sits behind the footer, so it doesn't get to hang.
const TIMEOUT: Duration = Duration::from_secs(5);

/// The account's web search allowance this month.
#[derive(Clone, Debug, PartialEq)]
pub struct SearchUsage {
    pub used: u64,
    /// `None` when unmetered (admins).
    pub limit: Option<u64>,
    /// `free`, `paid` or `admin`.
    pub plan: String,
    /// When the count goes back to zero, as `YYYY-MM-DD`.
    pub resets_on: Option<String>,
}

impl SearchUsage {
    fn from_status(status: &serde_json::Value) -> Option<Self> {
        let quota = &status["quota"];
        if !quota.is_object() {
            return None;
        }
        Some(SearchUsage {
            used: quota["used"].as_u64().unwrap_or_default(),
            limit: quota["limit"].as_u64(),
            plan: quota["plan"].as_str().unwrap_or("free").to_string(),
            resets_on: quota["resets_at"].as_str().and_then(|at| at.get(..10)).map(str::to_string),
        })
    }

    /// Spent at least 70% of the allowance: the footer starts warning.
    pub fn nearly_exhausted(&self) -> bool {
        self.limit.is_some_and(|limit| limit > 0 && self.used * 10 >= limit * 7)
    }

    /// Share of the allowance spent, as a whole percentage.
    pub fn percent_used(&self) -> u64 {
        self.limit.filter(|&l| l > 0).map_or(0, |limit| (self.used * 100 / limit).min(100))
    }

    /// Searches left this month; `None` when unmetered.
    pub fn remaining(&self) -> Option<u64> {
        self.limit.map(|limit| limit.saturating_sub(self.used))
    }

    pub fn exhausted(&self) -> bool {
        self.limit.is_some_and(|limit| self.used >= limit)
    }

    /// Under `/usage`'s bar: `12 used · free plan · resets 2026-11-01`.
    pub fn detail(&self) -> String {
        let mut parts = vec![format!("{} used", self.used), format!("{} plan", self.plan)];
        match (self.limit, &self.resets_on) {
            (Some(_), Some(on)) => parts.push(format!("resets {}", on)),
            (None, _) => parts.push("unmetered".to_string()),
            _ => {}
        }
        parts.join(" · ")
    }
}

/// The signed-in account's usage, or `None` when nobody is signed in or the
/// platform can't be reached — the footer then shows nothing.
pub fn fetch(client: &Client) -> Option<SearchUsage> {
    let token = stored_token()?;
    match status(client, &token) {
        Err(401) => {
            // Have the agent refresh the token, then try the new one.
            let _ = client.get(format!("{}/auth/status", crate::api_url())).timeout(TIMEOUT).send();
            let token = stored_token()?;
            status(client, &token).ok()
        }
        result => result.ok(),
    }
    .as_ref()
    .and_then(SearchUsage::from_status)
}

fn status(client: &Client, token: &str) -> Result<serde_json::Value, u16> {
    let response = client
        .get(format!("{}/v1/websearch/status", platform_url()))
        .bearer_auth(token)
        .timeout(TIMEOUT)
        .send()
        .map_err(|_| 0u16)?;
    if !response.status().is_success() {
        return Err(response.status().as_u16());
    }
    response.json().map_err(|_| 0)
}

/// The token the agent would send: its access token, else a developer token.
fn stored_token() -> Option<String> {
    let home = crate::framework::install_dir();
    let key = std::fs::read_to_string(home.join(".key")).ok()?;
    let sealed = std::fs::read_to_string(home.join(".credentials")).ok()?;
    let plain = fernet::Fernet::new(key.trim())?.decrypt(sealed.trim()).ok()?;
    let creds: serde_json::Value = serde_json::from_slice(&plain).ok()?;
    ["access_token", "developer_token"]
        .iter()
        .find_map(|k| creds[k].as_str().filter(|t| !t.is_empty()))
        .map(str::to_string)
        .or_else(|| std::env::var("SLP_DEVELOPER_TOKEN").ok().filter(|t| !t.is_empty()))
}

/// What the same traffic would have cost on a typical hosted frontier
/// model, per token: the saving running locally represents. The web app's
/// dashboard prices it the same way.
const COST_INPUT: f64 = 2.5 / 1_000_000.0;
const COST_OUTPUT: f64 = 10.0 / 1_000_000.0;

/// Days the token figures cover, as on the web app's dashboard.
pub const TOKEN_DAYS: usize = 7;

/// Tokens the agent processed on this machine, one entry per day (UTC),
/// oldest first, today last.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TokenStats {
    /// `(prompt, completion)` tokens per day.
    pub days: Vec<(u64, u64)>,
}

impl TokenStats {
    /// The last `TOKEN_DAYS` days of `SLP_HOME/token_usage.jsonl`, where the
    /// agent appends a line per turn (and from which it pushes to the
    /// platform's `/v1/usage/tokens`, which the dashboard charts).
    pub fn load() -> Self {
        let log = std::fs::read_to_string(crate::framework::install_dir().join("token_usage.jsonl")).unwrap_or_default();
        Self::from_log(&log, today())
    }

    fn from_log(log: &str, today: i64) -> Self {
        let mut days = vec![(0, 0); TOKEN_DAYS];
        for record in log.lines().filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok()) {
            let Some(day) = record["timestamp"].as_str().and_then(day_number) else { continue };
            let ago = today - day;
            if !(0..TOKEN_DAYS as i64).contains(&ago) {
                continue;
            }
            let slot = &mut days[TOKEN_DAYS - 1 - ago as usize];
            slot.0 += record["prompt_tokens"].as_u64().unwrap_or_default();
            slot.1 += record["completion_tokens"].as_u64().unwrap_or_default();
        }
        TokenStats { days }
    }

    pub fn prompt(&self) -> u64 {
        self.days.iter().map(|d| d.0).sum()
    }

    pub fn completion(&self) -> u64 {
        self.days.iter().map(|d| d.1).sum()
    }

    pub fn total(&self) -> u64 {
        self.prompt() + self.completion()
    }

    /// Estimated dollars saved over the days covered.
    pub fn saved(&self) -> f64 {
        self.prompt() as f64 * COST_INPUT + self.completion() as f64 * COST_OUTPUT
    }

    /// The days as a sparkline, one cell each: `▁▁▃▂█▅▁`.
    pub fn sparkline(&self) -> String {
        const CELLS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
        let max = self.days.iter().map(|d| d.0 + d.1).max().unwrap_or(0).max(1);
        self.days
            .iter()
            .map(|d| CELLS[((d.0 + d.1) * 7).div_ceil(max) as usize])
            .collect()
    }
}

/// Today as days since 1970-01-01, UTC.
fn today() -> i64 {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    (secs / 86_400) as i64
}

/// The day of an ISO timestamp (`2026-10-01T…`), as days since 1970-01-01.
fn day_number(timestamp: &str) -> Option<i64> {
    let date = timestamp.get(..10)?;
    let mut parts = date.split('-').map(|p| p.parse::<i64>().ok());
    let (y, m, d) = (parts.next()??, parts.next()??, parts.next()??);
    // Howard Hinnant's days_from_civil.
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (m + if m > 2 { -3 } else { 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    Some(era * 146_097 + doe - 719_468)
}

/// `12,345`.
pub fn thousands(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn usage(used: u64, limit: Option<u64>) -> SearchUsage {
        SearchUsage { used, limit, plan: "free".to_string(), resets_on: Some("2026-11-01".to_string()) }
    }

    #[test]
    fn reads_the_quota_from_a_status_reply() {
        let status = serde_json::json!({
            "enabled": true,
            "quota": {"limit": 50, "used": 12, "remaining": 38, "period": "2026-10",
                      "resets_at": "2026-11-01T00:00:00Z", "plan": "free"}
        });
        assert_eq!(SearchUsage::from_status(&status), Some(usage(12, Some(50))));
        assert_eq!(SearchUsage::from_status(&serde_json::json!({"enabled": false})), None);
    }

    #[test]
    fn totals_the_last_seven_days_of_tokens() {
        let today = day_number("2026-10-01T00:00:00Z").unwrap();
        let log = [
            r#"{"timestamp": "2026-10-01T19:20:49+00:00", "prompt_tokens": 1000000, "completion_tokens": 100000}"#,
            r#"{"timestamp": "2026-09-29T08:00:00+00:00", "prompt_tokens": 500, "completion_tokens": 50}"#,
            r#"{"timestamp": "2026-09-20T08:00:00+00:00", "prompt_tokens": 9999, "completion_tokens": 9999}"#,
            "not json",
        ]
        .join("\n");
        let stats = TokenStats::from_log(&log, today);
        assert_eq!(stats.days.len(), TOKEN_DAYS);
        assert_eq!(stats.days[6], (1_000_000, 100_000));
        assert_eq!(stats.days[4], (500, 50));
        assert_eq!(stats.total(), 1_100_550);
        // $2.50 for the million in, $1.00 for the hundred thousand out.
        assert!((stats.saved() - 3.5).abs() < 0.01);
        assert_eq!(stats.sparkline(), "▁▁▁▁▂▁█");
        assert_eq!(thousands(1_100_550), "1,100,550");
        assert_eq!(thousands(999), "999");
    }

    #[test]
    fn knows_when_the_allowance_is_spent() {
        assert!(!usage(34, Some(50)).nearly_exhausted());
        assert!(usage(35, Some(50)).nearly_exhausted());
        assert_eq!(usage(35, Some(50)).percent_used(), 70);
        assert!(usage(50, Some(50)).exhausted());
        assert!(!usage(500, None).exhausted());
        assert_eq!(usage(12, Some(50)).remaining(), Some(38));
        assert_eq!(usage(60, Some(50)).remaining(), Some(0));
    }
}
