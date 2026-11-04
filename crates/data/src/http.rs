//! Shared blocking HTTP client helpers.

use anyhow::{anyhow, Context, Result};
use reqwest::blocking::{Client, RequestBuilder};
use std::time::Duration;

/// Browser-like User-Agent: CBOE's CDN and Yahoo reject obvious bot agents.
pub const BROWSER_UA: &str =
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/124.0 Safari/537.36";

/// Default per-request timeout.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(15);
/// Timeout for large downloads (full CBOE chains for SPY/SPX are 5-15 MB).
pub const LARGE_TIMEOUT: Duration = Duration::from_secs(90);

/// Build the shared client: cookies on (Yahoo crumb flow), gzip, 15 s timeout.
pub fn build_client() -> Result<Client> {
    Client::builder()
        .user_agent(BROWSER_UA)
        .cookie_store(true)
        .gzip(true)
        .connect_timeout(DEFAULT_TIMEOUT)
        .timeout(DEFAULT_TIMEOUT)
        .build()
        .context("failed to build HTTP client")
}

/// Send a request and return the body as text, turning non-2xx answers into
/// descriptive errors (status + first bytes of body).
pub fn send_text(req: RequestBuilder, what: &str) -> Result<String> {
    let resp = req.send().with_context(|| format!("{what}: request failed (network unreachable or blocked?)"))?;
    let status = resp.status();
    let url = resp.url().clone();
    let body = resp.text().with_context(|| format!("{what}: failed to read response body"))?;
    if !status.is_success() {
        let mut snippet: String = body.chars().take(200).collect();
        if body.len() > 200 {
            snippet.push('…');
        }
        return Err(anyhow!("{what}: HTTP {status} from {}: {}", redact(url.as_str()), snippet.trim()));
    }
    Ok(body)
}

/// Remove secrets (apiKey / token query params) from a URL before showing it.
pub fn redact(url: &str) -> String {
    let mut out = String::with_capacity(url.len());
    let (base, query) = match url.split_once('?') {
        Some((b, q)) => (b, Some(q)),
        None => (url, None),
    };
    out.push_str(base);
    if let Some(q) = query {
        out.push('?');
        let parts: Vec<String> = q
            .split('&')
            .map(|kv| match kv.split_once('=') {
                Some((k, _)) if matches!(k.to_ascii_lowercase().as_str(), "apikey" | "api_key" | "token" | "access_token" | "crumb") => {
                    format!("{k}=***")
                }
                _ => kv.to_string(),
            })
            .collect();
        out.push_str(&parts.join("&"));
    }
    out
}

#[cfg(test)]
mod tests {
    #[test]
    fn redacts() {
        assert_eq!(
            super::redact("https://api.polygon.io/v3/x?cursor=abc&apiKey=SECRET"),
            "https://api.polygon.io/v3/x?cursor=abc&apiKey=***"
        );
    }
}
