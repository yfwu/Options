//! Store and config round trips in a temporary directory.

use chrono::{NaiveDate, NaiveDateTime};
use optlab_core::{ExerciseStyle, OptionChain, OptionKind, OptionQuote};
use optlab_data::{Config, DataService, Provider, Store};
use std::path::PathBuf;
use std::time::Duration;

fn tmp(tag: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
    let p = std::env::temp_dir().join(format!("optlab-data-test-{tag}-{}-{nanos}", std::process::id()));
    std::fs::create_dir_all(&p).unwrap();
    p
}

fn chain(ts: &str, source: &str) -> OptionChain {
    OptionChain {
        underlying: "QQQ".into(),
        spot: 600.0,
        as_of: NaiveDateTime::parse_from_str(ts, "%Y-%m-%d %H:%M:%S").unwrap(),
        source: source.into(),
        style: ExerciseStyle::American,
        quotes: vec![OptionQuote {
            contract: "QQQ271217C00600000".into(),
            expiry: NaiveDate::from_ymd_opt(2027, 12, 17).unwrap(),
            strike: 600.0,
            kind: OptionKind::Call,
            bid: 90.0,
            ask: 91.0,
            last: 90.5,
            volume: 10.0,
            open_interest: 1000.0,
            iv: Some(0.22),
            delta: Some(0.62),
            gamma: None,
            theta: None,
            vega: None,
        }],
    }
}

#[test]
fn snapshot_roundtrip() {
    let dir = tmp("store");
    let st = Store::at(&dir);
    let a = chain("2026-11-02 16:15:00", "cboe");
    let b = chain("2026-11-03 16:15:00", "yahoo");
    let pa = st.save_chain(&a).unwrap();
    st.save_chain(&b).unwrap();
    assert!(pa.ends_with("chains/QQQ/20261102-161500-cboe.json"));
    let list = st.list_snapshots(Some("qqq")).unwrap();
    assert_eq!(list.len(), 2);
    assert_eq!(list[0].source, "cboe");
    assert_eq!(list[1].as_of, b.as_of);
    assert_eq!(st.list_snapshots(None).unwrap().len(), 2);
    assert_eq!(st.load_chain(&list[0].path).unwrap(), a);
    assert_eq!(st.latest_snapshot("QQQ").unwrap().unwrap(), b);
    assert!(st.latest_snapshot("SPY").unwrap().is_none());

    st.cache_put("hist_QQQ", &vec![1.0, 2.0]).unwrap();
    assert_eq!(st.cache_get::<Vec<f64>>("hist_QQQ", Some(Duration::from_secs(60))), Some(vec![1.0, 2.0]));
    assert_eq!(st.cache_get::<Vec<f64>>("hist_QQQ", Some(Duration::ZERO)), None);
    assert_eq!(st.cache_get::<Vec<f64>>("missing", None), None);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn config_roundtrip() {
    let dir = tmp("cfg");
    let p = dir.join("sub/config.toml");
    assert_eq!(Config::load_file(&p).unwrap(), Config::default());
    let c = Config {
        tradier_token: Some("tok".into()),
        tradier_sandbox: true,
        polygon_api_key: None,
        default_provider: Some(Provider::Yahoo),
        risk_free_rate: Some(0.041),
        data_dir: Some(dir.join("data")),
        max_expiries: Some(12),
    };
    c.save_to(&p).unwrap();
    let text = std::fs::read_to_string(&p).unwrap();
    assert!(text.contains("default_provider = \"yahoo\""), "{text}");
    assert_eq!(Config::load_file(&p).unwrap(), c);
    // Partial files fill defaults.
    std::fs::write(&p, "polygon_api_key = \"k\"\n").unwrap();
    let c2 = Config::load_file(&p).unwrap();
    assert!(c2.has_polygon() && !c2.has_tradier() && !c2.tradier_sandbox);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn offline_service_falls_back() {
    let dir = tmp("svc");
    let cfg = Config { data_dir: Some(dir.clone()), risk_free_rate: None, ..Config::default() };
    let svc = DataService::new(cfg).unwrap();
    svc.set_offline(true);
    assert_eq!(svc.available_providers(), vec![Provider::Cboe, Provider::Yahoo]);
    // No snapshot yet -> error mentioning offline.
    let e = svc.fetch_chain_auto("QQQ").unwrap_err().to_string();
    assert!(e.contains("no option chain"), "{e}");
    svc.store().save_chain(&chain("2026-11-03 16:15:00", "cboe")).unwrap();
    let f = svc.fetch_chain_auto_detailed("QQQ").unwrap();
    assert!(f.from_snapshot && f.provider.is_none());
    // History / vol / rate fall back to bundled data.
    let h = svc.fetch_history_detailed("QQQ", Some(NaiveDate::from_ymd_opt(2020, 1, 1).unwrap()), None).unwrap();
    assert_eq!(h.source, "bundled");
    assert_eq!(h.series.first_date(), Some(NaiveDate::from_ymd_opt(2020, 1, 2).unwrap()));
    assert_eq!(svc.fetch_vol_index_detailed("VIX").unwrap().source, "bundled");
    let r = svc.risk_free_rate();
    assert!(r > 0.0 && r < 0.2);
    assert!(svc.fetch_history("ZZZZ", None, None).is_err());
    std::fs::remove_dir_all(&dir).ok();
}
