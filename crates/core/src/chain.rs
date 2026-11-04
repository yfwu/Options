//! Option chain data model shared by every provider and analysis module.

use chrono::{NaiveDate, NaiveDateTime};
use serde::{Deserialize, Serialize};

/// Call or put.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum OptionKind {
    Call,
    Put,
}

impl OptionKind {
    /// +1 for calls, -1 for puts (handy for put/call symmetric formulas).
    pub fn sign(self) -> f64 {
        match self {
            OptionKind::Call => 1.0,
            OptionKind::Put => -1.0,
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            OptionKind::Call => "C",
            OptionKind::Put => "P",
        }
    }
}

/// Exercise style of a contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum ExerciseStyle {
    /// Index options such as SPX / NDX / XSP.
    European,
    /// Equity & ETF options such as SPY / QQQ.
    #[default]
    American,
}

/// One quoted option contract.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OptionQuote {
    /// OCC style contract symbol, e.g. `SPY270115C00600000`.
    pub contract: String,
    pub expiry: NaiveDate,
    pub strike: f64,
    pub kind: OptionKind,
    pub bid: f64,
    pub ask: f64,
    pub last: f64,
    pub volume: f64,
    pub open_interest: f64,
    /// Implied volatility as a decimal (0.20 = 20%), if the provider supplies one.
    pub iv: Option<f64>,
    pub delta: Option<f64>,
    pub gamma: Option<f64>,
    pub theta: Option<f64>,
    pub vega: Option<f64>,
}

impl OptionQuote {
    /// Mid price; falls back to last trade when one side of the book is empty.
    pub fn mid(&self) -> f64 {
        if self.bid > 0.0 && self.ask > 0.0 && self.ask >= self.bid {
            0.5 * (self.bid + self.ask)
        } else if self.ask > 0.0 && self.bid <= 0.0 {
            0.5 * self.ask
        } else {
            self.last.max(0.0)
        }
    }

    /// Relative bid/ask spread (ask-bid)/mid; `None` when not computable.
    pub fn spread_pct(&self) -> Option<f64> {
        let m = self.mid();
        if self.bid > 0.0 && self.ask > 0.0 && m > 0.0 {
            Some((self.ask - self.bid) / m)
        } else {
            None
        }
    }

    /// Calendar days to expiry measured from `as_of`.
    pub fn dte(&self, as_of: NaiveDate) -> i64 {
        (self.expiry - as_of).num_days()
    }
}

/// A full option chain snapshot for one underlying.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OptionChain {
    pub underlying: String,
    pub spot: f64,
    /// Timestamp of the snapshot (exchange time if known).
    pub as_of: NaiveDateTime,
    /// Name of the provider that produced this chain (e.g. "cboe", "yahoo").
    pub source: String,
    pub style: ExerciseStyle,
    pub quotes: Vec<OptionQuote>,
}

impl OptionChain {
    pub fn as_of_date(&self) -> NaiveDate {
        self.as_of.date()
    }

    /// Sorted, de-duplicated list of expiries.
    pub fn expiries(&self) -> Vec<NaiveDate> {
        let mut e: Vec<NaiveDate> = self.quotes.iter().map(|q| q.expiry).collect();
        e.sort();
        e.dedup();
        e
    }

    /// All quotes for one expiry and kind, sorted by strike.
    pub fn slice(&self, expiry: NaiveDate, kind: OptionKind) -> Vec<&OptionQuote> {
        let mut v: Vec<&OptionQuote> = self
            .quotes
            .iter()
            .filter(|q| q.expiry == expiry && q.kind == kind)
            .collect();
        v.sort_by(|a, b| a.strike.partial_cmp(&b.strike).unwrap_or(std::cmp::Ordering::Equal));
        v
    }

    /// Sorted list of strikes available for an expiry (either kind).
    pub fn strikes(&self, expiry: NaiveDate) -> Vec<f64> {
        let mut s: Vec<f64> = self
            .quotes
            .iter()
            .filter(|q| q.expiry == expiry)
            .map(|q| q.strike)
            .collect();
        s.sort_by(|a, b| a.partial_cmp(b).unwrap());
        s.dedup_by(|a, b| (*a - *b).abs() < 1e-9);
        s
    }

    /// Find the quote with an exact strike/expiry/kind.
    pub fn find(&self, expiry: NaiveDate, strike: f64, kind: OptionKind) -> Option<&OptionQuote> {
        self.quotes
            .iter()
            .find(|q| q.expiry == expiry && q.kind == kind && (q.strike - strike).abs() < 1e-6)
    }

    /// Expiry closest to `target_dte` calendar days.
    pub fn expiry_near_dte(&self, target_dte: i64) -> Option<NaiveDate> {
        let d0 = self.as_of_date();
        self.expiries()
            .into_iter()
            .filter(|e| (*e - d0).num_days() > 0)
            .min_by_key(|e| ((*e - d0).num_days() - target_dte).abs())
    }

    /// Quote whose (provider or computed) delta is closest to `target_delta`.
    /// For puts pass a negative target (e.g. -0.25).
    pub fn find_by_delta(
        &self,
        expiry: NaiveDate,
        kind: OptionKind,
        target_delta: f64,
    ) -> Option<&OptionQuote> {
        self.slice(expiry, kind)
            .into_iter()
            .filter(|q| q.delta.is_some())
            .min_by(|a, b| {
                let da = (a.delta.unwrap() - target_delta).abs();
                let db = (b.delta.unwrap() - target_delta).abs();
                da.partial_cmp(&db).unwrap()
            })
    }

    /// Strike closest to spot for an expiry.
    pub fn atm_strike(&self, expiry: NaiveDate) -> Option<f64> {
        self.strikes(expiry).into_iter().min_by(|a, b| {
            (a - self.spot).abs().partial_cmp(&(b - self.spot).abs()).unwrap()
        })
    }

    /// Average of call/put IV at the ATM strike (provider IVs only).
    pub fn atm_iv(&self, expiry: NaiveDate) -> Option<f64> {
        let k = self.atm_strike(expiry)?;
        let ivs: Vec<f64> = [OptionKind::Call, OptionKind::Put]
            .iter()
            .filter_map(|kind| self.find(expiry, k, *kind).and_then(|q| q.iv))
            .filter(|v| *v > 0.0 && v.is_finite())
            .collect();
        if ivs.is_empty() {
            None
        } else {
            Some(ivs.iter().sum::<f64>() / ivs.len() as f64)
        }
    }
}

/// Parse an OCC option symbol such as `SPY270115C00600000` or `SPXW261120P05800000`.
/// Returns (root, expiry, kind, strike).
pub fn parse_occ_symbol(sym: &str) -> Option<(String, NaiveDate, OptionKind, f64)> {
    let s = sym.trim().trim_start_matches("O:");
    if s.len() < 16 {
        return None;
    }
    let tail = &s[s.len() - 15..];
    let root = s[..s.len() - 15].trim().to_string();
    let date = NaiveDate::parse_from_str(&tail[0..6], "%y%m%d").ok()?;
    let kind = match &tail[6..7] {
        "C" | "c" => OptionKind::Call,
        "P" | "p" => OptionKind::Put,
        _ => return None,
    };
    let strike: f64 = tail[7..].parse::<u64>().ok()? as f64 / 1000.0;
    Some((root, date, kind, strike))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn occ_parse() {
        let (root, d, k, s) = parse_occ_symbol("SPY270115C00600000").unwrap();
        assert_eq!(root, "SPY");
        assert_eq!(d, NaiveDate::from_ymd_opt(2027, 1, 15).unwrap());
        assert_eq!(k, OptionKind::Call);
        assert!((s - 600.0).abs() < 1e-9);
        let (root, _, k, s) = parse_occ_symbol("SPXW261120P05800000").unwrap();
        assert_eq!(root, "SPXW");
        assert_eq!(k, OptionKind::Put);
        assert!((s - 5800.0).abs() < 1e-9);
        assert!(parse_occ_symbol("garbage").is_none());
    }
}
