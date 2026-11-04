//! Live historical series: CBOE index history CSVs, Stooq, FRED and Yahoo charts.

use crate::csv_import::{parse_price_csv, parse_scalar_csv};
use crate::http::send_text;
use crate::providers::clean_symbol;
use crate::providers::yahoo::YahooClient;
use anyhow::{anyhow, Result};
use chrono::NaiveDate;
use optlab_core::{PriceSeries, ScalarSeries};
use reqwest::blocking::Client;

/// CBOE daily index history CSV (`DATE,OPEN,HIGH,LOW,CLOSE`, `m/d/Y`).
/// Works for `VIX`, `VXN`, `VIX3M`, `VIX9D`, `VIX1Y`, `VVIX` (`DATE,VVIX`), `SKEW`, ...
pub fn cboe_index_history_url(name: &str) -> String {
    format!("https://cdn.cboe.com/api/global/us_indices/daily_prices/{}_History.csv", clean_symbol(name))
}

fn check_csv(body: &str, what: &str) -> Result<()> {
    let head = body.trim_start();
    if head.starts_with('<') || head.is_empty() || !head.lines().next().unwrap_or("").contains(',') {
        return Err(anyhow!(
            "{what}: response is not CSV (first bytes: {:?})",
            head.chars().take(80).collect::<String>()
        ));
    }
    Ok(())
}

/// Download a CBOE index history as OHLC bars.
pub fn fetch_cboe_index_ohlc(client: &Client, name: &str) -> Result<PriceSeries> {
    let what = format!("CBOE {} history", clean_symbol(name));
    let body = send_text(client.get(cboe_index_history_url(name)).header("Referer", "https://www.cboe.com/"), &what)?;
    check_csv(&body, &what)?;
    parse_price_csv(&body, &clean_symbol(name))
}

/// Download a CBOE index history as a close series.
pub fn fetch_cboe_index(client: &Client, name: &str) -> Result<ScalarSeries> {
    let what = format!("CBOE {} history", clean_symbol(name));
    let body = send_text(client.get(cboe_index_history_url(name)).header("Referer", "https://www.cboe.com/"), &what)?;
    check_csv(&body, &what)?;
    parse_scalar_csv(&body, &clean_symbol(name), None)
}

/// Stooq symbol: US tickers get `.us` (`spy.us`), indices keep `^` (`^spx`).
pub fn stooq_symbol(sym: &str) -> String {
    let t = sym.trim().to_ascii_lowercase();
    if t.starts_with('^') || t.contains('.') {
        return t;
    }
    let c = clean_symbol(&t).to_ascii_lowercase();
    if crate::providers::is_index_symbol(&c) {
        format!("^{c}")
    } else {
        format!("{c}.us")
    }
}

pub fn stooq_url(sym: &str) -> String {
    format!("https://stooq.com/q/d/l/?s={}&i=d", stooq_symbol(sym))
}

/// Stooq daily CSV (`Date,Open,High,Low,Close,Volume`, split-adjusted; no separate
/// adjusted close, so `adj_close = close`).
pub fn fetch_stooq(client: &Client, sym: &str) -> Result<PriceSeries> {
    let what = format!("Stooq {}", stooq_symbol(sym));
    let body = send_text(client.get(stooq_url(sym)), &what)?;
    check_csv(&body, &what)?;
    if body.starts_with("No data") {
        return Err(anyhow!("{what}: no data"));
    }
    parse_price_csv(&body, &clean_symbol(sym))
}

pub fn fred_url(series_id: &str) -> String {
    format!("https://fred.stlouisfed.org/graph/fredgraph.csv?id={}", series_id.trim().to_ascii_uppercase())
}

/// FRED series via the public `fredgraph.csv` endpoint (no key). Values as published
/// (DGS3MO is in percent).
pub fn fetch_fred(client: &Client, series_id: &str) -> Result<ScalarSeries> {
    let id = series_id.trim().to_ascii_uppercase();
    let what = format!("FRED {id}");
    let body = send_text(client.get(fred_url(&id)), &what)?;
    check_csv(&body, &what)?;
    parse_scalar_csv(&body, &id, None)
}

/// Yahoo daily history (adjusted close included).
pub fn fetch_yahoo(yahoo: &YahooClient, sym: &str, start: Option<NaiveDate>, end: Option<NaiveDate>) -> Result<PriceSeries> {
    yahoo.fetch_history(sym, start, end)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn urls() {
        assert_eq!(cboe_index_history_url("^vix"), "https://cdn.cboe.com/api/global/us_indices/daily_prices/VIX_History.csv");
        assert_eq!(stooq_url("SPY"), "https://stooq.com/q/d/l/?s=spy.us&i=d");
        assert_eq!(stooq_symbol("SPX"), "^spx");
        assert_eq!(fred_url("dgs3mo"), "https://fred.stlouisfed.org/graph/fredgraph.csv?id=DGS3MO");
    }
    #[test]
    fn rejects_html() {
        assert!(check_csv("<html>captcha</html>", "x").is_err());
        assert!(check_csv("Date,Open\n", "x").is_ok());
    }
}
