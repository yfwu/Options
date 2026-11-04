//! Live network tests. Ignored by default (the CI container blocks these hosts);
//! run locally with `cargo test -p optlab-data --test live -- --ignored --nocapture`.

use optlab_data::{history, http, providers, Config, DataService, Provider, Store};

fn svc() -> DataService {
    let dir = std::env::temp_dir().join(format!("optlab-live-{}", std::process::id()));
    DataService::with_store(Config::load().unwrap_or_default(), Store::at(dir)).unwrap()
}

#[test]
#[ignore]
fn live_cboe_spy_and_spx() {
    let c = http::build_client().unwrap();
    let ch = providers::cboe::fetch_chain(&c, "SPY").unwrap();
    println!("SPY {} contracts spot {} as_of {}", ch.quotes.len(), ch.spot, ch.as_of);
    assert!(ch.quotes.len() > 1000);
    let spx = providers::cboe::fetch_chain(&c, "SPX").unwrap();
    assert_eq!(spx.style, optlab_core::ExerciseStyle::European);
    let q = providers::cboe::fetch_quote(&c, "_VIX").unwrap();
    println!("VIX {} iv30 {:?}", q.price, q.iv30);
}

#[test]
#[ignore]
fn live_yahoo_chain_and_history() {
    let s = svc();
    let y = providers::yahoo::YahooClient::new(s.http_client().clone());
    let ch = y.fetch_chain("QQQ", Some(3)).unwrap();
    println!("Yahoo QQQ {} contracts, expiries {:?}", ch.quotes.len(), ch.expiries());
    assert!(!ch.quotes.is_empty());
    let h = y.fetch_history("SPY", None, None).unwrap();
    println!("Yahoo SPY {} bars {:?}..{:?}", h.len(), h.first_date(), h.last_date());
    assert!(h.len() > 5000);
}

#[test]
#[ignore]
fn live_histories() {
    let c = http::build_client().unwrap();
    let v = history::fetch_cboe_index(&c, "VIX").unwrap();
    println!("CBOE VIX {} pts last {:?}", v.len(), v.last());
    let r = history::fetch_fred(&c, "DGS3MO").unwrap();
    println!("FRED DGS3MO last {:?}", r.last());
    match history::fetch_stooq(&c, "SPY") {
        Ok(s) => println!("Stooq SPY {} bars", s.len()),
        Err(e) => println!("Stooq failed (may need API key now): {e:#}"),
    }
}

#[test]
#[ignore]
fn live_auto() {
    let s = svc();
    let f = s.fetch_chain_auto_detailed("QQQ").unwrap();
    println!("auto QQQ via {:?} ({} contracts), errors {:?}", f.provider, f.chain.quotes.len(), f.errors);
    if s.config().has_tradier() {
        let t = s.fetch_chain(Provider::Tradier, "SPY").unwrap();
        println!("Tradier SPY {} contracts", t.quotes.len());
    }
    if s.config().has_polygon() {
        let p = s.fetch_chain(Provider::Polygon, "SPY").unwrap();
        println!("Polygon SPY {} contracts", p.quotes.len());
    }
}
