//! Ready-made research inputs assembled from bundled data: backtest data with the
//! right vol index, real 3M T-bill history, a dividend-yield default and the matching
//! synthetic-vol / cost presets. Shared by the CLI and the desktop app.

use crate::bundled;
use chrono::NaiveDate;
use optlab_core::backtest::{approx_us_tbill_rates, BacktestData, RateSource, RollConfig};
use optlab_core::history::ScalarSeries;
use optlab_core::synth::{CostModel, SynthVolModel};

/// Risk-free rate history: bundled FRED DGS3MO (1981+) spliced onto the approximate
/// T-bill table only for dates before the FRED series starts.
pub fn rate_source() -> RateSource {
    let approx = approx_us_tbill_rates();
    match bundled::rate("DGS3MO") {
        Some(r) if !r.is_empty() => {
            let first = r.points[0].0;
            let mut pts: Vec<(NaiveDate, f64)> = approx.points.iter().copied().filter(|(d, _)| *d < first).collect();
            pts.extend(r.points.iter().copied());
            RateSource::Series(ScalarSeries::new("3M T-bill (FRED DGS3MO)", pts))
        }
        _ => RateSource::Series(approx),
    }
}

/// Typical trailing dividend yield used for option forwards.
pub fn default_dividend_yield(symbol: &str) -> f64 {
    match symbol.to_ascii_uppercase().as_str() {
        "SPY" | "VTI" | "SPX" | "XSP" => 0.014,
        "QQQ" | "NDX" => 0.006,
        "MSFT" => 0.008,
        "AVGO" => 0.013,
        "TSM" => 0.015,
        "SOXX" => 0.007,
        "GOOG" | "META" => 0.004,
        "NVDA" => 0.0003,
        _ => 0.0,
    }
}

/// True when the underlying's options are priced off VXN (Nasdaq/tech).
pub fn uses_vxn(symbol: &str) -> bool {
    bundled::default_vol_index_name(symbol) == "VXN"
}

/// Synthetic vol model preset matching the underlying's vol index.
pub fn vol_model_for(symbol: &str) -> SynthVolModel {
    if uses_vxn(symbol) {
        SynthVolModel::qqq()
    } else {
        SynthVolModel::spy()
    }
}

/// Cost preset for the underlying.
pub fn cost_model_for(symbol: &str) -> CostModel {
    match symbol.to_ascii_uppercase().as_str() {
        "SPX" | "XSP" => CostModel::spx(),
        "SPY" | "VTI" => CostModel::spy(),
        _ => CostModel::qqq(),
    }
}

/// Default roll config calibrated for the underlying.
pub fn roll_config_for(symbol: &str) -> RollConfig {
    RollConfig { vol_model: vol_model_for(symbol), costs: cost_model_for(symbol), ..RollConfig::default() }
}

/// Backtest inputs from bundled data. Returns the data and the vol-index name used.
pub fn backtest_data(symbol: &str) -> anyhow::Result<(BacktestData, String)> {
    let sym = symbol.to_ascii_uppercase();
    let prices = bundled::prices(&sym).ok_or_else(|| anyhow::anyhow!("no bundled price history for {sym}"))?;
    let vname = bundled::default_vol_index_name(&sym).to_string();
    let vol = bundled::default_vol_index_for(&sym).ok_or_else(|| anyhow::anyhow!("no bundled {vname} history"))?;
    Ok((BacktestData::new(prices, vol, rate_source(), default_dividend_yield(&sym)), vname))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn qqq_and_spy_inputs() {
        for s in ["QQQ", "SPY"] {
            let (d, v) = backtest_data(s).unwrap();
            assert!(d.prices.len() > 5000, "{s}");
            assert!(!d.vol_index.is_empty());
            assert_eq!(v, if s == "QQQ" { "VXN" } else { "VIX" });
            let r = optlab_core::backtest::run_backtest(&d, &roll_config_for(s)).unwrap();
            assert!(r.stats.years > 20.0);
        }
        // FRED rate: 5.55% on 2023-07-31
        if let RateSource::Series(s) = rate_source() {
            let v = s.value_at(NaiveDate::from_ymd_opt(2023, 7, 31).unwrap()).unwrap();
            assert!((v - 5.55).abs() < 0.05, "{v}");
        }
    }
}
