//! Live market-data providers. Every provider exposes pure `parse_*` functions
//! (unit-tested on captured payloads in `tests/fixtures`) plus blocking `fetch_*`
//! functions that perform the HTTP calls.

pub mod cboe;
pub mod polygon;
pub mod tradier;
pub mod yahoo;

use optlab_core::ExerciseStyle;

/// Cash-settled index roots that trade European-style options.
pub const INDEX_ROOTS: &[&str] = &[
    "SPX", "SPXW", "SPXPM", "XSP", "NDX", "NDXP", "XND", "RUT", "RUTW", "MRUT", "VIX", "VIXW", "DJX", "OEX", "XEO",
];

/// Strip provider prefixes (`^`, `_`, `$`, `I:`, `.`) and upper-case.
pub fn clean_symbol(sym: &str) -> String {
    let s = sym.trim().to_ascii_uppercase();
    let s = s.strip_prefix("I:").unwrap_or(&s);
    s.trim_start_matches(['^', '_', '$', '.']).to_string()
}

/// True when the (cleaned) symbol is a cash-settled index with European options.
pub fn is_index_symbol(sym: &str) -> bool {
    INDEX_ROOTS.contains(&clean_symbol(sym).as_str())
}

/// Exercise style for an underlying: European for index roots, American otherwise.
pub fn style_for(sym: &str) -> ExerciseStyle {
    if is_index_symbol(sym) {
        ExerciseStyle::European
    } else {
        ExerciseStyle::American
    }
}

/// Treat 0 / negative / non-finite as "missing".
pub(crate) fn positive(v: Option<f64>) -> Option<f64> {
    v.filter(|x| x.is_finite() && *x > 0.0)
}

pub(crate) fn finite(v: Option<f64>) -> Option<f64> {
    v.filter(|x| x.is_finite())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn symbols() {
        assert_eq!(clean_symbol("^spx"), "SPX");
        assert_eq!(clean_symbol("_NDX"), "NDX");
        assert_eq!(clean_symbol("I:SPX"), "SPX");
        assert!(is_index_symbol("^VIX"));
        assert!(!is_index_symbol("SPY"));
        assert_eq!(style_for("XSP"), ExerciseStyle::European);
        assert_eq!(style_for("QQQ"), ExerciseStyle::American);
    }
}
