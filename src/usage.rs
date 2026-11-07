//! Usage for `/usage`: web search and credits, read from the platform
//! (api.smartloop.ai).
//!
//! Web search usage is read from the platform (api.smartloop.ai) rather than
//! the agent: `GET /v1/websearch/status` carries the month's allowance and
//! what has been spent of it, which the agent's own gate reduces to a yes/no.
//! Credits, one monthly pool for cloud models and web searches, come from
//! the agent's `GET /v1/chat/quota`, which relays the platform's answer: the
//! same reading Studio's "Plan & usage" card takes.
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

}

/// The account's credits this month, as the agent's `/v1/chat/quota` relays
/// them from the platform, the same reading Studio's "Plan & usage" card
/// takes. One pool covers cloud models and web searches. Every account gets
/// the share spent; the amounts come only to accounts allowed to see them,
/// and an unmetered one has amounts but no share.
#[derive(Clone, Debug, PartialEq)]
pub struct CreditUsage {
    /// Of the allowance, spent, 0 to 100; `None` when unmetered.
    pub percent_used: Option<f64>,
    /// Credits spent and allowed, when the platform gives them.
    pub used: Option<f64>,
    pub limit: Option<f64>,
    /// `free`, `paid`, `plus` or `admin`.
    pub plan: String,
    /// When the count goes back to zero, as the platform's ISO timestamp.
    pub resets_at: Option<String>,
    /// Cloud calls this month, and the tokens they read and wrote.
    pub requests: u64,
    pub tokens: u64,
}

impl CreditUsage {
    fn from_quota(quota: &serde_json::Value) -> Option<Self> {
        if !quota.is_object() || quota.get("plan").is_none() {
            return None;
        }
        Some(CreditUsage {
            percent_used: quota["percent_used"].as_f64(),
            used: quota["used"].as_f64(),
            limit: quota["limit"].as_f64(),
            plan: quota["plan"].as_str().unwrap_or("free").to_string(),
            resets_at: quota["resets_at"].as_str().map(str::to_string),
            requests: quota["requests"].as_u64().unwrap_or_default(),
            tokens: quota["prompt_tokens"].as_u64().unwrap_or_default()
                + quota["completion_tokens"].as_u64().unwrap_or_default(),
        })
    }

    pub fn unmetered(&self) -> bool {
        self.percent_used.is_none()
    }

    /// Share spent, 0 to 1; `None` when unmetered.
    pub fn fraction_used(&self) -> Option<f64> {
        self.percent_used.map(|p| (p / 100.0).clamp(0.0, 1.0))
    }

    /// All of this month's credits spent.
    pub fn exhausted(&self) -> bool {
        self.percent_used.is_some_and(|p| p >= 100.0)
    }

    /// 80% or more spent, short of all of it.
    pub fn low(&self) -> bool {
        !self.exhausted() && self.percent_used.is_some_and(|p| p >= 80.0)
    }

    /// `24%`, never `0%` once anything is spent.
    pub fn percent(&self) -> String {
        match self.percent_used {
            Some(p) if p > 0.0 && p < 1.0 => "<1%".to_string(),
            Some(p) => format!("{}%", p.round() as u64),
            None => String::new(),
        }
    }

    /// The plan as Studio names it.
    pub fn plan_label(&self) -> &'static str {
        match self.plan.as_str() {
            "paid" => "Pro",
            "plus" => "Plus",
            "admin" => "Admin",
            _ => "Free",
        }
    }

    /// `resets in 5h` within the last day, else `resets Dec 1`.
    pub fn reset_label(&self) -> Option<String> {
        self.reset_label_at(now_secs())
    }

    fn reset_label_at(&self, now: i64) -> Option<String> {
        let at = self.resets_at.as_deref()?;
        let day = day_number(at)?;
        let hour = at.get(11..13).and_then(|h| h.parse::<i64>().ok()).unwrap_or(0);
        let hours = (day * 86_400 + hour * 3_600 - now + 3_599).div_euclid(3_600);
        if hours <= 24 {
            return Some(format!("resets in {}h", hours.max(1)));
        }
        const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
        let month = at.get(5..7)?.parse::<usize>().ok().filter(|m| (1..=12).contains(m))?;
        let date = at.get(8..10)?.parse::<u32>().ok()?;
        Some(format!("resets {} {}", MONTHS[month - 1], date))
    }

    /// The amounts: `480 of 2,000 credits used`, or `566 credits used`
    /// unmetered; `None` when the platform keeps them back.
    pub fn amounts(&self) -> Option<String> {
        let used = thousands(self.used?.round() as u64);
        Some(match self.limit {
            Some(limit) => format!("{} of {} credits used", used, thousands(limit.round() as u64)),
            None => format!("{} credits used", used),
        })
    }

    /// Under `/usage`'s bar: `316 calls · 11,848,606 tokens · Admin · resets Dec 1`.
    pub fn detail(&self) -> String {
        let mut parts = vec![
            format!("{} call{}", thousands(self.requests), if self.requests == 1 { "" } else { "s" }),
            format!("{} tokens", thousands(self.tokens)),
            self.plan_label().to_string(),
        ];
        parts.extend(self.reset_label());
        parts.join(" · ")
    }

    /// What the credits are for, or that they're gone, as Studio says it.
    pub fn note(&self) -> &'static str {
        if self.exhausted() {
            "this month's credits are used: cloud models and web search pause until the reset; local models keep working"
        } else if self.unmetered() {
            "unlimited credits for cloud models and web searches; local models are always free"
        } else if self.plan == "free" {
            "credits cover web searches, one per search; local models are always free"
        } else {
            "credits cover cloud models and web searches; local models are always free"
        }
    }
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

/// The signed-in account's web search usage, or `None` when nobody is
/// signed in or the platform can't be reached — the footer then shows
/// nothing.
pub fn fetch(client: &Client) -> Option<SearchUsage> {
    SearchUsage::from_status(&platform_status(client, "/v1/websearch/status")?)
}

/// The signed-in account's credits, read through the agent as Studio reads
/// them; `None` while signed out (401), without cloud access (403) or with
/// the platform out of reach.
pub fn fetch_credits(client: &Client) -> Option<CreditUsage> {
    let response = client.get(format!("{}/chat/quota", crate::api_url())).timeout(TIMEOUT).send().ok()?;
    if !response.status().is_success() {
        return None;
    }
    CreditUsage::from_quota(&response.json().ok()?)
}

/// A status reply from the platform under the agent's token, had again
/// with a fresh one after a 401.
fn platform_status(client: &Client, path: &str) -> Option<serde_json::Value> {
    let token = stored_token()?;
    match status(client, &token, path) {
        Err(401) => {
            // Have the agent refresh the token, then try the new one.
            let _ = client.get(format!("{}/auth/status", crate::api_url())).timeout(TIMEOUT).send();
            let token = stored_token()?;
            status(client, &token, path).ok()
        }
        result => result.ok(),
    }
}

fn status(client: &Client, token: &str, path: &str) -> Result<serde_json::Value, u16> {
    let response = client
        .get(format!("{}{}", platform_url(), path))
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
    fn reads_credits_as_the_agent_relays_them() {
        // Unmetered: amounts, no share.
        let admin = serde_json::json!({
            "limit": null, "used": 565.66, "remaining": null, "percent_used": null, "reserved": 0,
            "percent_available": null, "in_flight": 0, "prompt_tokens": 11213282, "completion_tokens": 635324,
            "requests": 316, "period": "2026-11", "resets_at": "2026-12-01T00:00:00+00:00", "plan": "admin"
        });
        let credits = CreditUsage::from_quota(&admin).unwrap();
        assert!(credits.unmetered() && !credits.exhausted());
        assert_eq!(credits.amounts().as_deref(), Some("566 credits used"));
        let nov_6 = day_number("2026-11-06").unwrap() * 86_400;
        assert_eq!(credits.reset_label_at(nov_6).as_deref(), Some("resets Dec 1"));
        assert_eq!(credits.reset_label_at(nov_6 + 24 * 86_400 + 19 * 3_600).as_deref(), Some("resets in 5h"));
        assert!(credits.note().starts_with("unlimited credits"));

        // Metered, amounts kept back: the share alone.
        let paid = serde_json::json!({
            "limit": null, "used": null, "remaining": null, "percent_used": 84.4, "requests": 3,
            "prompt_tokens": 10000, "completion_tokens": 2000, "resets_at": "2026-12-01T00:00:00+00:00", "plan": "paid"
        });
        let credits = CreditUsage::from_quota(&paid).unwrap();
        assert_eq!((credits.percent(), credits.low(), credits.amounts()), ("84%".to_string(), true, None));
        assert!((credits.fraction_used().unwrap() - 0.844).abs() < 1e-9);
        assert_eq!(credits.plan_label(), "Pro");
        assert!(credits.detail().starts_with("3 calls · 12,000 tokens · Pro · resets "));

        // Metered with amounts, and spent.
        let spent = serde_json::json!({"limit": 500.0, "used": 500.0, "percent_used": 100.0, "plan": "plus"});
        let credits = CreditUsage::from_quota(&spent).unwrap();
        assert_eq!(credits.amounts().as_deref(), Some("500 of 500 credits used"));
        assert!(credits.exhausted() && !credits.low());
        assert!(credits.note().starts_with("this month's credits are used"));
        assert_eq!(CreditUsage::from_quota(&serde_json::json!({"detail": "no"})), None);
        assert_eq!(CreditUsage::from_quota(&serde_json::json!({"percent_used": 1.0, "used": 0.4, "plan": "free"})).unwrap().percent(), "1%");

    }

    #[test]
    fn groups_thousands() {
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
