//! Parser tests on captured real payloads (see tests/fixtures/README.md).

use chrono::{NaiveDate, NaiveDateTime};
use optlab_core::{ExerciseStyle, OptionKind};
use optlab_data::providers::{cboe, polygon, tradier, yahoo};

fn fixture(name: &str) -> String {
    let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name);
    std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()))
}

fn d(y: i32, m: u32, dd: u32) -> NaiveDate {
    NaiveDate::from_ymd_opt(y, m, dd).unwrap()
}

fn close(a: f64, b: f64) -> bool {
    (a - b).abs() < 1e-9
}

#[test]
fn cboe_dov_chain() {
    let ch = cboe::parse_chain(&fixture("cboe_delayed_DOV_2026-09-10.json"), "DOV").unwrap();
    assert_eq!(ch.underlying, "DOV");
    assert_eq!(ch.source, "cboe");
    assert_eq!(ch.style, ExerciseStyle::American);
    assert!(close(ch.spot, 191.42));
    // Payload timestamp 2026-09-10 03:09:50 UTC = 2026-09-09 23:09:50 EDT (after the 9 Sep session).
    assert_eq!(ch.as_of, NaiveDateTime::parse_from_str("2026-09-09 23:09:50", "%Y-%m-%d %H:%M:%S").unwrap());
    assert_eq!(ch.quotes.len(), 454);
    assert_eq!(ch.quotes.iter().filter(|q| q.kind == OptionKind::Call).count(), 227);
    assert_eq!(
        ch.expiries(),
        vec![d(2026, 9, 18), d(2026, 10, 16), d(2026, 11, 20), d(2026, 12, 18), d(2027, 1, 15), d(2027, 3, 19), d(2028, 1, 21)]
    );
    assert_eq!(ch.quotes.iter().filter(|q| q.expiry == d(2028, 1, 21)).count(), 78);

    let p = ch.find(d(2026, 10, 16), 190.0, OptionKind::Put).unwrap();
    assert_eq!(p.contract, "DOV261016P00190000");
    assert!(close(p.bid, 3.6) && close(p.ask, 5.9) && close(p.last, 5.15));
    assert_eq!(p.iv, Some(0.2426));
    assert_eq!(p.delta, Some(-0.4278));
    assert_eq!(p.gamma, Some(0.0265));
    assert_eq!(p.theta, Some(-0.0838));
    assert_eq!(p.vega, Some(0.2382));
    assert!(close(p.open_interest, 2.0));

    let c = ch.find(d(2026, 10, 16), 200.0, OptionKind::Call).unwrap();
    assert_eq!(c.delta, Some(0.3428));
    assert_eq!(c.iv, Some(0.2614));
    assert!(close(c.open_interest, 6.0));

    // Contracts without an IV have no greeks.
    let no_iv = ch.quotes.iter().filter(|q| q.iv.is_none()).count();
    assert_eq!(no_iv, 90);
    assert!(ch.quotes.iter().filter(|q| q.iv.is_none()).all(|q| q.delta.is_none()));

    // The chain payload also carries the quote fields (IV30).
    let q = cboe::parse_quote(&fixture("cboe_delayed_DOV_2026-09-10.json")).unwrap();
    assert_eq!(q.symbol, "DOV");
    assert!(close(q.price, 191.42));
    assert_eq!(q.iv30, Some(24.942));
    assert_eq!(q.security_type.as_deref(), Some("stock"));
    assert_eq!(q.last_trade_time.unwrap().to_string(), "2026-09-09 16:00:00");
}

#[test]
fn cboe_index_style_and_bad_payload() {
    let json = fixture("cboe_delayed_DOV_2026-09-10.json");
    // The style follows the requested underlying (index roots are European).
    assert_eq!(cboe::parse_chain(&json, "_SPX").unwrap().style, ExerciseStyle::European);
    assert!(cboe::parse_chain("{\"oops\":1}", "SPY").is_err());
    assert!(cboe::parse_chain("<html>blocked</html>", "SPY").is_err());
}

#[test]
fn yahoo_qqq_options() {
    let page = yahoo::parse_options_page(&fixture("yahoo_v7_options_QQQ_2025-02-24.json")).unwrap();
    assert_eq!(page.underlying, "QQQ");
    assert!(close(page.spot, 526.12));
    assert_eq!(page.market_time.unwrap().to_string(), "2025-02-21 16:00:00");
    assert_eq!(page.expiration_dates.len(), 30);
    assert_eq!(page.expiration_dates[0], 1740355200);
    assert_eq!(page.quotes.len(), 180);
    let ch = yahoo::chain_from_pages("QQQ", &[page]).unwrap();
    assert_eq!(ch.source, "yahoo");
    assert_eq!(ch.expiries(), vec![d(2025, 2, 24)]);
    assert_eq!(ch.slice(d(2025, 2, 24), OptionKind::Call).len(), 78);
    assert_eq!(ch.slice(d(2025, 2, 24), OptionKind::Put).len(), 102);
    let c = ch.find(d(2025, 2, 24), 530.0, OptionKind::Call).unwrap();
    assert_eq!(c.contract, "QQQ250224C00530000");
    assert!(close(c.last, 0.86) && close(c.volume, 33937.0));
    // Weekend capture: bid = ask = 0 everywhere, so Yahoo's IVs are discarded and no greeks.
    assert!(ch.quotes.iter().all(|q| q.iv.is_none() && q.delta.is_none()));
    assert!(close(c.mid(), 0.86));
}

#[test]
fn yahoo_spy_chart() {
    let s = yahoo::parse_chart(&fixture("yahoo_v8_chart_SPY_5y_2026-03-24.json"), "SPY").unwrap();
    assert_eq!(s.symbol, "SPY");
    // 1257 timestamps; the last daily row (null close) and the live row share a date.
    assert_eq!(s.len(), 1256);
    assert_eq!(s.first_date(), Some(d(2021, 3, 24)));
    assert_eq!(s.last_date(), Some(d(2026, 3, 24)));
    let b0 = s.bars[0];
    assert!((b0.close - 387.519989).abs() < 1e-4);
    assert!((b0.adj_close - 361.937073).abs() < 1e-4);
    assert!(close(b0.open, 391.0));
    assert!(close(s.bars.last().unwrap().close, 653.875));
    assert!(s.bars.windows(2).all(|w| w[0].date < w[1].date));
    assert!(s.bars.iter().all(|b| b.adj_close <= b.close + 1e-9));
}

#[test]
fn tradier_pltr_chain() {
    let (quotes, upd) = tradier::parse_chain(&fixture("tradier_chain_PLTR_2024-08-26_rewrapped.json")).unwrap();
    assert_eq!(quotes.len(), 10);
    assert!(quotes.iter().all(|q| q.kind == OptionKind::Put && q.expiry == d(2024, 9, 20)));
    let q = quotes.iter().find(|q| q.contract == "PLTR240920P00028000").unwrap();
    assert!(close(q.strike, 28.0) && close(q.bid, 0.49) && close(q.ask, 0.51) && close(q.last, 0.5));
    assert_eq!(q.iv, Some(0.505205));
    assert!((q.delta.unwrap() + 0.2036825591836027).abs() < 1e-12);
    assert!(close(q.open_interest, 7056.0) && close(q.volume, 1317.0));
    assert!(upd.unwrap() >= NaiveDateTime::parse_from_str("2024-08-26 20:00:00", "%Y-%m-%d %H:%M:%S").unwrap());
}

/// Envelope shapes from Tradier's public API documentation (doc-derived, not captured).
#[test]
fn tradier_doc_shapes() {
    let exp = r#"{"expirations":{"date":["2026-12-18","2026-11-20","2027-01-15"]}}"#;
    assert_eq!(tradier::parse_expirations(exp).unwrap(), vec![d(2026, 11, 20), d(2026, 12, 18), d(2027, 1, 15)]);
    // Single element collapsed into a scalar, and empty -> null.
    assert_eq!(tradier::parse_expirations(r#"{"expirations":{"date":"2026-12-18"}}"#).unwrap().len(), 1);
    assert!(tradier::parse_expirations(r#"{"expirations":null}"#).unwrap().is_empty());
    assert!(tradier::parse_chain(r#"{"options":null}"#).unwrap().0.is_empty());
    let q = r#"{"quotes":{"quote":{"symbol":"SPY","last":null,"bid":500.0,"ask":500.2,"prevclose":499.0,"trade_date":1724702399000}}}"#;
    let qs = tradier::parse_quotes(q).unwrap();
    assert_eq!(qs.len(), 1);
    assert!(close(qs[0].price, 500.1));
}

#[test]
fn polygon_nvda_snapshot() {
    let p = polygon::parse_snapshot_page(&fixture("polygon_snapshot_NVDA_2025-09-29.json")).unwrap();
    assert_eq!(p.quotes.len(), 10);
    assert_eq!(p.underlying_price, Some(181.07));
    assert!(p.next_url.as_deref().unwrap().starts_with("https://api.polygon.io/v3/snapshot/options/NVDA?cursor="));
    // underlying last_updated 1759190399.75 s = 2025-09-29 19:59:59 EDT
    assert_eq!(p.as_of.unwrap().to_string(), "2025-09-29 19:59:59");
    let q = &p.quotes[0];
    assert_eq!(q.contract, "NVDA251003C00050000");
    assert_eq!(q.kind, OptionKind::Call);
    assert_eq!(q.expiry, d(2025, 10, 3));
    assert!(close(q.strike, 50.0));
    assert!(close(q.last, 131.65));
    assert!(close(q.open_interest, 43.0) && close(q.volume, 8.0));
    assert!((q.iv.unwrap() - 6.889417828154338).abs() < 1e-12);
    assert!((q.delta.unwrap() - 0.9854048189538928).abs() < 1e-12);
    // This plan returned no last_quote: bid/ask are 0, mid falls back to last.
    assert!(q.bid == 0.0 && q.ask == 0.0);
    let ch = polygon::chain_from_snapshot("NVDA", p, None).unwrap();
    assert!(close(ch.spot, 181.07));
    assert_eq!(ch.source, "polygon");
    let err = polygon::parse_snapshot_page(r#"{"status":"NOT_AUTHORIZED","message":"bad key","results":[]}"#);
    assert!(err.unwrap_err().to_string().contains("bad key"));
}
