//! CSV import with header auto-detection.
//!
//! * Option chains in "long" format (one row per contract) as exported by
//!   Yahoo copy/paste, thinkorswim, Interactive Brokers, Barchart, OptionStrat, etc.
//! * Daily price histories (Yahoo, Stooq, CBOE `m/d/Y`, lowercase variants, 2-column `Date,value`).
//! * Scalar series (VIX-style indices, FRED rates).

use crate::providers::{clean_symbol, style_for};
use crate::util::{norm_header, parse_date, parse_num};
use anyhow::{anyhow, Context, Result};
use chrono::{NaiveDate, NaiveDateTime};
use optlab_core::chain::parse_occ_symbol;
use optlab_core::{Bar, ExerciseStyle, OptionChain, OptionKind, OptionQuote, PriceSeries, ScalarSeries};
use std::path::Path;

fn reader(text: &str) -> csv::Reader<&[u8]> {
    let delim = sniff_delimiter(text);
    csv::ReaderBuilder::new()
        .delimiter(delim)
        .flexible(true)
        .trim(csv::Trim::All)
        .comment(Some(b'#'))
        .from_reader(text.as_bytes())
}

fn sniff_delimiter(text: &str) -> u8 {
    let line = text.lines().find(|l| !l.trim().is_empty() && !l.starts_with('#')).unwrap_or("");
    let count = |c: char| line.matches(c).count();
    let (comma, semi, tab) = (count(','), count(';'), count('\t'));
    if tab > comma && tab >= semi {
        b'\t'
    } else if semi > comma {
        b';'
    } else {
        b','
    }
}

fn strip_bom(s: &str) -> &str {
    s.strip_prefix('\u{feff}').unwrap_or(s)
}

/// Find the first header whose normalised name is in `names` (exact), then prefix matches.
fn find_col(headers: &[String], names: &[&str]) -> Option<usize> {
    for n in names {
        if let Some(i) = headers.iter().position(|h| h == n) {
            return Some(i);
        }
    }
    None
}

// ======================================================================= chains

/// Options for [`parse_chain_csv`].
#[derive(Debug, Clone, Default)]
pub struct ChainCsvOptions {
    /// Underlying symbol; if `None` it is taken from an underlying column or the OCC root.
    pub underlying: Option<String>,
    /// Spot price; if `None` taken from an underlying-price column or estimated by put-call parity.
    pub spot: Option<f64>,
    /// Snapshot time; defaults to now (New York).
    pub as_of: Option<NaiveDateTime>,
    /// Exercise style; defaults by underlying (European for index roots).
    pub style: Option<ExerciseStyle>,
}

const C_CONTRACT: &[&str] = &[
    "contract", "contractsymbol", "contractname", "optionsymbol", "occsymbol", "occ", "symbol", "option", "ticker",
    "localsymbol", "instrument", "optioncode",
];
const C_EXPIRY: &[&str] = &[
    "expiry", "expiration", "expirationdate", "expdate", "exp", "expiry date", "expires", "maturity", "maturitydate",
    "lasttradedateorcontractmonth", "expirydate",
];
const C_STRIKE: &[&str] = &["strike", "strikeprice", "k", "strikes"];
const C_TYPE: &[&str] = &["type", "optiontype", "putcall", "callput", "cp", "pc", "right", "kind", "side", "callorput"];
const C_BID: &[&str] = &["bid", "bidprice", "bestbid"];
const C_ASK: &[&str] = &["ask", "askprice", "bestask", "offer"];
const C_LAST: &[&str] = &["last", "lastprice", "lasttrade", "lasttradeprice", "price", "close", "mark"];
const C_VOL: &[&str] = &["volume", "vol", "totalvolume", "dayvolume"];
const C_OI: &[&str] = &["openinterest", "oi", "openint", "opnint"];
const C_IV: &[&str] = &["iv", "impliedvolatility", "impliedvol", "impvol", "volatility", "midiv", "ivmid", "optionimpliedvolatility"];
const C_DELTA: &[&str] = &["delta"];
const C_GAMMA: &[&str] = &["gamma"];
const C_THETA: &[&str] = &["theta"];
const C_VEGA: &[&str] = &["vega"];
const C_UNDERLYING: &[&str] = &["underlying", "underlyingsymbol", "root", "rootsymbol", "underlyingticker"];
const C_SPOT: &[&str] = &["underlyingprice", "underlyinglast", "spot", "spotprice", "stockprice", "undprice", "underlyingclose"];

fn parse_kind(s: &str) -> Option<OptionKind> {
    match s.trim().to_ascii_lowercase().as_str() {
        "c" | "call" | "calls" | "ce" => Some(OptionKind::Call),
        "p" | "put" | "puts" | "pe" => Some(OptionKind::Put),
        _ => None,
    }
}

/// Parse a long-format option chain CSV. Required: either an OCC contract column,
/// or expiry + strike + type columns. All other fields are optional.
///
/// IV is accepted as a decimal (0.25), a percent string (`25.3%`), or a bare
/// percent number (> 3.0 is interpreted as percent).
pub fn parse_chain_csv(text: &str, opts: &ChainCsvOptions) -> Result<OptionChain> {
    let text = strip_bom(text);
    let mut rdr = reader(text);
    let headers: Vec<String> = rdr.headers().context("CSV: cannot read header row")?.iter().map(norm_header).collect();
    let col = |names: &[&str]| find_col(&headers, &names.iter().map(|n| norm_header(n)).collect::<Vec<_>>().iter().map(|s| s.as_str()).collect::<Vec<_>>());
    let c_contract = col(C_CONTRACT);
    let c_exp = col(C_EXPIRY);
    let c_strike = col(C_STRIKE);
    let c_type = col(C_TYPE);
    if c_contract.is_none() && (c_exp.is_none() || c_strike.is_none() || c_type.is_none()) {
        return Err(anyhow!(
            "CSV chain: need an OCC contract column or expiry+strike+type columns; found headers {:?}",
            headers
        ));
    }
    let (c_bid, c_ask, c_last, c_vol, c_oi) = (col(C_BID), col(C_ASK), col(C_LAST), col(C_VOL), col(C_OI));
    let (c_iv, c_d, c_g, c_t, c_v) = (col(C_IV), col(C_DELTA), col(C_GAMMA), col(C_THETA), col(C_VEGA));
    let (c_und, c_spot) = (col(C_UNDERLYING), col(C_SPOT));

    let mut quotes = Vec::new();
    let mut und_from_file: Option<String> = None;
    let mut spot_from_file: Option<f64> = None;
    for rec in rdr.records() {
        let rec = match rec {
            Ok(r) => r,
            Err(_) => continue,
        };
        let get = |c: Option<usize>| c.and_then(|i| rec.get(i)).unwrap_or("");
        let num = |c: Option<usize>| parse_num(get(c));
        let raw_contract = get(c_contract).trim().to_string();
        let occ = parse_occ_symbol(&raw_contract.replace(' ', ""));
        let expiry = parse_date(get(c_exp)).or(occ.as_ref().map(|o| o.1));
        let strike = num(c_strike).or(occ.as_ref().map(|o| o.3));
        let kind = parse_kind(get(c_type)).or(occ.as_ref().map(|o| o.2));
        let (Some(expiry), Some(strike), Some(kind)) = (expiry, strike, kind) else { continue };
        if und_from_file.is_none() {
            let u = get(c_und).trim();
            if !u.is_empty() {
                und_from_file = Some(clean_symbol(u));
            } else if let Some(o) = &occ {
                und_from_file = Some(clean_symbol(&o.0));
            }
        }
        if spot_from_file.is_none() {
            spot_from_file = num(c_spot).filter(|v| *v > 0.0);
        }
        let iv = c_iv.and_then(|i| rec.get(i)).and_then(|s| {
            let v = parse_num(s)?;
            let v = if s.contains('%') || v > 3.0 { v / 100.0 } else { v };
            (v > 0.0).then_some(v)
        });
        quotes.push(OptionQuote {
            contract: if occ.is_some() { raw_contract.replace(' ', "") } else { String::new() },
            expiry,
            strike,
            kind,
            bid: num(c_bid).unwrap_or(0.0).max(0.0),
            ask: num(c_ask).unwrap_or(0.0).max(0.0),
            last: num(c_last).unwrap_or(0.0).max(0.0),
            volume: num(c_vol).unwrap_or(0.0),
            open_interest: num(c_oi).unwrap_or(0.0),
            iv,
            delta: num(c_d),
            gamma: num(c_g),
            theta: num(c_t),
            vega: num(c_v),
        });
    }
    if quotes.is_empty() {
        return Err(anyhow!("CSV chain: no valid contract rows"));
    }
    let underlying = opts
        .underlying
        .as_deref()
        .map(clean_symbol)
        .or(und_from_file)
        .unwrap_or_else(|| "UNKNOWN".to_string());
    // Fill synthetic OCC symbols where missing so contracts stay addressable.
    for q in quotes.iter_mut().filter(|q| q.contract.is_empty()) {
        q.contract = format!(
            "{}{}{}{:08}",
            underlying,
            q.expiry.format("%y%m%d"),
            q.kind.label(),
            (q.strike * 1000.0).round() as u64
        );
    }
    let spot = opts
        .spot
        .or(spot_from_file)
        .or_else(|| estimate_spot_from_parity(&quotes))
        .ok_or_else(|| anyhow!("CSV chain: no spot price given and it could not be estimated (need calls and puts)"))?;
    Ok(OptionChain {
        style: opts.style.unwrap_or_else(|| style_for(&underlying)),
        underlying,
        spot,
        as_of: opts.as_of.unwrap_or_else(crate::util::now_new_york),
        source: "csv".to_string(),
        quotes,
    })
}

/// Load a chain CSV from disk (see [`parse_chain_csv`]).
pub fn load_chain_csv(path: impl AsRef<Path>, opts: &ChainCsvOptions) -> Result<OptionChain> {
    let p = path.as_ref();
    let text = std::fs::read_to_string(p).with_context(|| format!("cannot read {}", p.display()))?;
    parse_chain_csv(&text, opts).with_context(|| format!("while parsing {}", p.display()))
}

/// Spot estimate from put-call parity at the nearest expiry: S ≈ K + C − P at the
/// strike where |C − P| is smallest (ignores carry; good to ~0.5% for short expiries).
pub fn estimate_spot_from_parity(quotes: &[OptionQuote]) -> Option<f64> {
    let mut exps: Vec<NaiveDate> = quotes.iter().map(|q| q.expiry).collect();
    exps.sort();
    exps.dedup();
    for e in exps {
        let mut best: Option<(f64, f64)> = None; // (|c-p|, estimate)
        for c in quotes.iter().filter(|q| q.expiry == e && q.kind == OptionKind::Call) {
            let Some(p) = quotes
                .iter()
                .find(|q| q.expiry == e && q.kind == OptionKind::Put && (q.strike - c.strike).abs() < 1e-9)
            else {
                continue;
            };
            let (cm, pm) = (c.mid(), p.mid());
            if cm <= 0.0 || pm <= 0.0 {
                continue;
            }
            let d = (cm - pm).abs();
            if best.is_none_or(|b| d < b.0) {
                best = Some((d, c.strike + cm - pm));
            }
        }
        if let Some((_, s)) = best {
            if s > 0.0 {
                return Some(s);
            }
        }
    }
    None
}

// ======================================================================= prices

const P_DATE: &[&str] = &["date", "datetime", "time", "timestamp", "observationdate", "day", "fecha", "tradedate", "dt"];
const P_OPEN: &[&str] = &["open", "openprice", "apertura", "o"];
const P_HIGH: &[&str] = &["high", "highprice", "maximo", "max", "h"];
const P_LOW: &[&str] = &["low", "lowprice", "minimo", "min", "l"];
const P_CLOSE: &[&str] = &["close", "closeprice", "last", "price", "cierre", "closelast", "c", "value", "px", "settle"];
const P_ADJ: &[&str] = &["adjclose", "adjustedclose", "adjcloseprice", "adjusted", "adj", "closeadj", "adjustedcloseprice"];
const P_VOL: &[&str] = &["volume", "vol", "v"];

/// Parse a daily price-history CSV with auto-detected columns.
///
/// Recognised layouts include Yahoo (`Date,Open,High,Low,Close,Adj Close,Volume`),
/// lowercase variants (`date,open,...,adj_close`), Stooq (`Date,Open,High,Low,Close,Volume`),
/// CBOE index history (`DATE,OPEN,HIGH,LOW,CLOSE` with `m/d/Y`), and any 2-column
/// `date,value` file. Missing OHLC fall back to close; missing adjusted close = close.
pub fn parse_price_csv(text: &str, symbol: &str) -> Result<PriceSeries> {
    let text = strip_bom(text);
    let mut rdr = reader(text);
    let headers: Vec<String> = rdr.headers().context("CSV: cannot read header row")?.iter().map(norm_header).collect();
    let date_c = find_col(&headers, P_DATE).unwrap_or(0);
    let close_c = find_col(&headers, P_CLOSE)
        .or_else(|| find_col(&headers, P_ADJ))
        .or_else(|| first_value_col(&headers, date_c))
        .ok_or_else(|| anyhow!("CSV prices: no close/value column in {:?}", headers))?;
    let adj_c = find_col(&headers, P_ADJ);
    let (o_c, h_c, l_c, v_c) =
        (find_col(&headers, P_OPEN), find_col(&headers, P_HIGH), find_col(&headers, P_LOW), find_col(&headers, P_VOL));
    let mut bars = Vec::new();
    for rec in rdr.records().flatten() {
        let Some(date) = rec.get(date_c).and_then(parse_date) else { continue };
        let num = |c: Option<usize>| c.and_then(|i| rec.get(i)).and_then(parse_num);
        let Some(close) = num(Some(close_c)).filter(|c| *c > 0.0) else { continue };
        bars.push(Bar {
            date,
            open: num(o_c).filter(|v| *v > 0.0).unwrap_or(close),
            high: num(h_c).filter(|v| *v > 0.0).unwrap_or(close),
            low: num(l_c).filter(|v| *v > 0.0).unwrap_or(close),
            close,
            adj_close: num(adj_c).filter(|v| *v > 0.0).unwrap_or(close),
            volume: num(v_c).unwrap_or(0.0),
        });
    }
    if bars.is_empty() {
        return Err(anyhow!("CSV prices: no valid rows (headers {:?})", headers));
    }
    Ok(PriceSeries::new(symbol, bars))
}

fn first_value_col(headers: &[String], date_c: usize) -> Option<usize> {
    (0..headers.len()).find(|i| *i != date_c)
}

/// Load a price CSV from disk (see [`parse_price_csv`]).
pub fn load_price_csv(path: impl AsRef<Path>, symbol: &str) -> Result<PriceSeries> {
    let p = path.as_ref();
    let text = std::fs::read_to_string(p).with_context(|| format!("cannot read {}", p.display()))?;
    parse_price_csv(&text, symbol).with_context(|| format!("while parsing {}", p.display()))
}

// ======================================================================= scalars

/// Parse a dated scalar series (VIX close, FRED rate in %, ...).
///
/// `column` selects a value column by (case/punctuation-insensitive) name; by
/// default a close-like column is used, else the first non-date column.
/// FRED's `.` and empty cells are skipped.
pub fn parse_scalar_csv(text: &str, name: &str, column: Option<&str>) -> Result<ScalarSeries> {
    let text = strip_bom(text);
    let mut rdr = reader(text);
    let headers: Vec<String> = rdr.headers().context("CSV: cannot read header row")?.iter().map(norm_header).collect();
    let date_c = find_col(&headers, P_DATE).unwrap_or(0);
    let val_c = match column {
        Some(c) => {
            let n = norm_header(c);
            headers.iter().position(|h| *h == n).ok_or_else(|| anyhow!("CSV: column '{c}' not found in {:?}", headers))?
        }
        None => find_col(&headers, P_CLOSE)
            .or_else(|| first_value_col(&headers, date_c))
            .ok_or_else(|| anyhow!("CSV: no value column in {:?}", headers))?,
    };
    let mut pts = Vec::new();
    for rec in rdr.records().flatten() {
        let Some(d) = rec.get(date_c).and_then(parse_date) else { continue };
        let Some(v) = rec.get(val_c).and_then(parse_num) else { continue };
        pts.push((d, v));
    }
    if pts.is_empty() {
        return Err(anyhow!("CSV scalar: no valid rows (headers {:?})", headers));
    }
    Ok(ScalarSeries::new(name, pts))
}

/// Load a scalar CSV from disk (see [`parse_scalar_csv`]).
pub fn load_scalar_csv(path: impl AsRef<Path>, name: &str, column: Option<&str>) -> Result<ScalarSeries> {
    let p = path.as_ref();
    let text = std::fs::read_to_string(p).with_context(|| format!("cannot read {}", p.display()))?;
    parse_scalar_csv(&text, name, column).with_context(|| format!("while parsing {}", p.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(y: i32, m: u32, dd: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, dd).unwrap()
    }

    #[test]
    fn chain_yahoo_like_with_occ() {
        let csv = "Contract Name,Last Trade Date (EDT),Strike,Last Price,Bid,Ask,Change,% Change,Volume,Open Interest,Implied Volatility\n\
QQQ250321C00500000,2025-02-21 3:59PM EST,500,31.2,30.9,31.4,0,0%,1200,5000,22.15%\n\
QQQ250321P00500000,2025-02-21 3:59PM EST,500,4.1,4.0,4.2,0,0%,800,7000,23.40%\n";
        let ch = parse_chain_csv(csv, &ChainCsvOptions::default()).unwrap();
        assert_eq!(ch.underlying, "QQQ");
        assert_eq!(ch.quotes.len(), 2);
        let c = &ch.quotes[0];
        assert_eq!(c.expiry, d(2025, 3, 21));
        assert_eq!(c.kind, OptionKind::Call);
        assert!((c.iv.unwrap() - 0.2215).abs() < 1e-9);
        assert_eq!(c.open_interest, 5000.0);
        // parity: 500 + 31.15 - 4.1 = 527.05
        assert!((ch.spot - 527.05).abs() < 1e-6, "{}", ch.spot);
    }

    #[test]
    fn chain_columns_without_contract() {
        let csv = "Underlying;Expiration;Strike;Right;Bid;Ask;Delta;IV;Open Int\n\
SPX;20 DEC 26;6000;C;250.5;252.0;0.55;0.18;100\n\
SPX;20 DEC 26;6000;P;180.0;181.5;-0.45;0.19;90\n";
        let ch = parse_chain_csv(csv, &ChainCsvOptions { spot: Some(6070.0), ..Default::default() }).unwrap();
        assert_eq!(ch.underlying, "SPX");
        assert_eq!(ch.style, ExerciseStyle::European);
        assert_eq!(ch.spot, 6070.0);
        assert_eq!(ch.quotes[0].contract, "SPX261220C06000000");
        assert_eq!(ch.quotes[1].delta, Some(-0.45));
        assert_eq!(ch.quotes[1].open_interest, 90.0);
    }

    #[test]
    fn chain_missing_cols_errors() {
        assert!(parse_chain_csv("a,b\n1,2\n", &ChainCsvOptions::default()).is_err());
    }

    #[test]
    fn prices_variants() {
        let yahoo = "Date,Open,High,Low,Close,Adj Close,Volume\n2024-01-02,10,11,9,10.5,10.0,100\n2024-01-03,10.5,11,10,10.8,10.3,200\n";
        let s = parse_price_csv(yahoo, "X").unwrap();
        assert_eq!(s.len(), 2);
        assert_eq!(s.bars[0].adj_close, 10.0);
        assert_eq!(s.bars[1].volume, 200.0);

        let lower = "date,open,high,low,close,adj_close,volume\n2024-01-03,1,2,0.5,1.5,1.4,7\n2024-01-02,1,2,0.5,1.2,1.1,7\n";
        let s = parse_price_csv(lower, "X").unwrap();
        assert_eq!(s.first_date(), Some(d(2024, 1, 2)));
        assert_eq!(s.bars[1].adj_close, 1.4);

        let cboe = "DATE,OPEN,HIGH,LOW,CLOSE\n03/16/2020,57.83,83.56,57.83,82.69\n";
        let s = parse_price_csv(cboe, "VIX").unwrap();
        assert_eq!(s.bars[0].date, d(2020, 3, 16));
        assert_eq!(s.bars[0].close, 82.69);
        assert_eq!(s.bars[0].adj_close, 82.69);

        let two = "Date,adj_close\n1999-03-10,43.02\n1999-03-11,.\n1999-03-12,43.5\n";
        let s = parse_price_csv(two, "QQQ").unwrap();
        assert_eq!(s.len(), 2);
        assert_eq!(s.bars[1].open, 43.5);

        let stooq = "Date,Open,High,Low,Close,Volume\n2026-11-02,750,755,748,752.1,1000\n";
        assert_eq!(parse_price_csv(stooq, "SPY").unwrap().bars[0].close, 752.1);
    }

    #[test]
    fn scalar_variants() {
        let fred = "observation_date,DGS3MO\n2026-09-07,\n2026-09-08,4.01\n2026-09-09,.\n";
        let s = parse_scalar_csv(fred, "DGS3MO", None).unwrap();
        assert_eq!(s.points, vec![(d(2026, 9, 8), 4.01)]);
        let vix = "Date,VIX,SPY\n2020-03-16,82.69,220.1\n";
        let s = parse_scalar_csv(vix, "VIX", Some("VIX")).unwrap();
        assert_eq!(s.points[0].1, 82.69);
        let cboe = "DATE,OPEN,HIGH,LOW,CLOSE\n3/16/2020,57.83,83.56,57.83,82.69\n";
        assert_eq!(parse_scalar_csv(cboe, "VIX", None).unwrap().points[0].1, 82.69);
        let comment = "# FRED DGS3MO\n# note\nDATE,VALUE\n2020-04-01,0.09\n";
        assert_eq!(parse_scalar_csv(comment, "r", None).unwrap().points[0].1, 0.09);
    }
}
