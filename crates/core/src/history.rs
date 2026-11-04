//! Historical time series (underlying prices, volatility indices, rates).

use chrono::NaiveDate;
use serde::{Deserialize, Serialize};

/// One daily bar.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Bar {
    pub date: NaiveDate,
    pub open: f64,
    pub high: f64,
    pub low: f64,
    pub close: f64,
    /// Dividend/split adjusted close (total-return proxy). Equal to `close` when unknown.
    pub adj_close: f64,
    pub volume: f64,
}

/// A daily price history, sorted ascending by date with unique dates.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct PriceSeries {
    pub symbol: String,
    pub bars: Vec<Bar>,
}

impl PriceSeries {
    pub fn new(symbol: impl Into<String>, mut bars: Vec<Bar>) -> Self {
        bars.sort_by_key(|b| b.date);
        bars.dedup_by_key(|b| b.date);
        bars.retain(|b| b.close.is_finite() && b.close > 0.0);
        Self { symbol: symbol.into(), bars }
    }
    pub fn len(&self) -> usize {
        self.bars.len()
    }
    pub fn is_empty(&self) -> bool {
        self.bars.is_empty()
    }
    pub fn first_date(&self) -> Option<NaiveDate> {
        self.bars.first().map(|b| b.date)
    }
    pub fn last_date(&self) -> Option<NaiveDate> {
        self.bars.last().map(|b| b.date)
    }
    pub fn dates(&self) -> Vec<NaiveDate> {
        self.bars.iter().map(|b| b.date).collect()
    }
    pub fn closes(&self) -> Vec<f64> {
        self.bars.iter().map(|b| b.close).collect()
    }
    pub fn adj_closes(&self) -> Vec<f64> {
        self.bars.iter().map(|b| b.adj_close).collect()
    }
    /// Daily simple returns of the adjusted close (len = n-1).
    pub fn returns(&self) -> Vec<f64> {
        self.bars
            .windows(2)
            .map(|w| w[1].adj_close / w[0].adj_close - 1.0)
            .collect()
    }
    /// Daily log returns of the adjusted close (len = n-1).
    pub fn log_returns(&self) -> Vec<f64> {
        self.bars
            .windows(2)
            .map(|w| (w[1].adj_close / w[0].adj_close).ln())
            .collect()
    }
    /// Restrict to [start, end] inclusive.
    pub fn slice(&self, start: Option<NaiveDate>, end: Option<NaiveDate>) -> PriceSeries {
        let bars = self
            .bars
            .iter()
            .filter(|b| start.map_or(true, |s| b.date >= s) && end.map_or(true, |e| b.date <= e))
            .copied()
            .collect();
        PriceSeries { symbol: self.symbol.clone(), bars }
    }
    /// Close on or before `date` (last known value).
    pub fn close_on_or_before(&self, date: NaiveDate) -> Option<f64> {
        let idx = self.bars.partition_point(|b| b.date <= date);
        if idx == 0 {
            None
        } else {
            Some(self.bars[idx - 1].close)
        }
    }
    /// Annualised realised volatility of the last `window` daily log returns.
    pub fn realized_vol(&self, window: usize) -> Option<f64> {
        let lr = self.log_returns();
        if lr.len() < window || window < 2 {
            return None;
        }
        let w = &lr[lr.len() - window..];
        let m = w.iter().sum::<f64>() / w.len() as f64;
        let v = w.iter().map(|x| (x - m).powi(2)).sum::<f64>() / (w.len() - 1) as f64;
        Some((v * 252.0).sqrt())
    }
}

/// A simple dated scalar series (e.g. VIX close in vol points, or a rate in %).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct ScalarSeries {
    pub name: String,
    pub points: Vec<(NaiveDate, f64)>,
}

impl ScalarSeries {
    pub fn new(name: impl Into<String>, mut points: Vec<(NaiveDate, f64)>) -> Self {
        points.sort_by_key(|p| p.0);
        points.dedup_by_key(|p| p.0);
        points.retain(|p| p.1.is_finite());
        Self { name: name.into(), points }
    }
    pub fn len(&self) -> usize {
        self.points.len()
    }
    pub fn is_empty(&self) -> bool {
        self.points.is_empty()
    }
    /// Value on or before `date` (forward-filled). Falls back to the first point.
    pub fn value_at(&self, date: NaiveDate) -> Option<f64> {
        if self.points.is_empty() {
            return None;
        }
        let idx = self.points.partition_point(|p| p.0 <= date);
        if idx == 0 {
            Some(self.points[0].1)
        } else {
            Some(self.points[idx - 1].1)
        }
    }
    pub fn last(&self) -> Option<(NaiveDate, f64)> {
        self.points.last().copied()
    }
    /// Percentile rank (0..1) of the latest value within the trailing `lookback` points.
    pub fn percentile_rank(&self, lookback: usize) -> Option<f64> {
        let (_, last) = self.last()?;
        let n = self.points.len();
        let start = n.saturating_sub(lookback);
        let w = &self.points[start..];
        if w.len() < 2 {
            return None;
        }
        let below = w.iter().filter(|p| p.1 < last).count();
        Some(below as f64 / (w.len() - 1) as f64)
    }
    /// IV-rank style: (last - min) / (max - min) over trailing `lookback` points.
    pub fn range_rank(&self, lookback: usize) -> Option<f64> {
        let (_, last) = self.last()?;
        let n = self.points.len();
        let w = &self.points[n.saturating_sub(lookback)..];
        let mn = w.iter().map(|p| p.1).fold(f64::INFINITY, f64::min);
        let mx = w.iter().map(|p| p.1).fold(f64::NEG_INFINITY, f64::max);
        if mx > mn {
            Some((last - mn) / (mx - mn))
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn d(y: i32, m: u32, dd: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, dd).unwrap()
    }
    #[test]
    fn scalar_ffill() {
        let s = ScalarSeries::new("x", vec![(d(2020, 1, 2), 1.0), (d(2020, 1, 6), 2.0)]);
        assert_eq!(s.value_at(d(2020, 1, 1)), Some(1.0));
        assert_eq!(s.value_at(d(2020, 1, 3)), Some(1.0));
        assert_eq!(s.value_at(d(2020, 1, 6)), Some(2.0));
        assert_eq!(s.value_at(d(2021, 1, 6)), Some(2.0));
    }
}
