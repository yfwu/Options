//! Rolling LEAPS call backtester ("stock replacement" / leveraged long beta).
//!
//! The engine holds one call line at a time on the underlying, marks it every
//! day with the synthetic IV surface ([`SynthVolModel`]) driven by a volatility
//! index, and rolls / resizes it according to a [`RollConfig`].
//!
//! ## Daily loop (all trades at the day's close)
//! 1. Accrue interest on idle cash (`cash_yield`), using the previous day's rate.
//! 2. If the option has reached its expiry it is settled at intrinsic value.
//! 3. Mark to model: `σ = vol_model.iv(K, S, T, vol_index)`, BS price & Greeks.
//! 4. If equity ≤ 0.1% of initial capital ⇒ **wiped out**, stop.
//! 5. Triggers, first match wins:
//!    `DTE ≤ roll_dte` → roll; `delta > roll_up_delta` → roll (re-strike up);
//!    `delta < roll_down_delta` → roll (re-strike down); option mid ≥ entry mid ×
//!    (1+`profit_take_pct`) → roll; exposure outside `target·(1±band)` → resize the
//!    same contract (or re-strike if, in `Delta` mode, the delta drifted more than
//!    [`RESTRIKE_DELTA_GAP`] from target). With no position the engine opens one.
//! 6. Opening size: `contracts = target_exposure·equity / (Δ·S·multiplier)`, capped
//!    so premium ≤ `max_premium_pct·equity` and never more than the available cash
//!    (no margin, so equity can never go negative).
//!
//! ## Underlying price vs total return
//! `prices.adj_close` is treated as a total-return index. Calls do **not** receive
//! dividends, so the option's underlying is reconstructed as
//! `spot_t = adj_t · e^{q·(t_last − t)}` (a price-return path that ends at the last
//! adjusted close) and options are priced with dividend yield `q`. Buy & hold uses
//! the adjusted close directly. Thus the dividend handicap of calls is modelled.
//!
//! ## Expiries
//! Expiries snap to the monthly 3rd-Friday closest to `as_of + entry_dte`
//! (exchange holidays ignored; settlement happens on the first bar on/after it).
//!
//! The final equity is marked at model mid (no liquidation cost deducted).

use anyhow::{bail, Result};
use chrono::{Datelike, Duration, NaiveDate};
use serde::{Deserialize, Serialize};

use crate::chain::OptionKind;
use crate::history::{PriceSeries, ScalarSeries};
use crate::pricing::{bs_greeks, Greeks};
use crate::stats::{compute_stats, PerformanceStats};
use crate::synth::{CostModel, SynthVolModel};

// ---------------------------------------------------------------------------
// Inputs
// ---------------------------------------------------------------------------

/// Risk-free rate source.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum RateSource {
    /// Constant annual rate, decimal (0.04 = 4%).
    Constant(f64),
    /// Dated series in **percent** (e.g. 3-month T-bill yield 4.35), forward-filled.
    Series(ScalarSeries),
}

impl Default for RateSource {
    fn default() -> Self {
        RateSource::Constant(0.03)
    }
}

impl RateSource {
    /// Annual decimal rate on `date`.
    pub fn rate_at(&self, date: NaiveDate) -> f64 {
        match self {
            RateSource::Constant(r) => *r,
            RateSource::Series(s) => s.value_at(date).unwrap_or(0.0) / 100.0,
        }
    }
}

/// Approximate US 3-month T-bill yield (percent), half-year averages 1990–2026.
///
/// Built-in so historical backtests reflect the large swings in financing cost
/// (6% in 2000, ~0% in 2009–2015 and 2020–21, 5% in 2023–24). Values are rounded
/// approximations from memory of FRED `TB3MS`; replace with real data when available.
pub fn approx_us_tbill_rates() -> ScalarSeries {
    #[rustfmt::skip]
    const H: [(i32, f64, f64); 37] = [
        (1990, 7.8, 7.4), (1991, 6.0, 5.3), (1992, 3.9, 3.1), (1993, 3.0, 3.0), (1994, 3.6, 4.9),
        (1995, 5.8, 5.4), (1996, 5.0, 5.1), (1997, 5.1, 5.1), (1998, 5.0, 4.5), (1999, 4.4, 4.9),
        (2000, 5.7, 6.1), (2001, 4.4, 2.5), (2002, 1.7, 1.6), (2003, 1.1, 0.95), (2004, 1.0, 1.8),
        (2005, 2.7, 3.6), (2006, 4.6, 4.9), (2007, 5.0, 4.0), (2008, 1.9, 0.9), (2009, 0.2, 0.1),
        (2010, 0.12, 0.14), (2011, 0.07, 0.03), (2012, 0.08, 0.10), (2013, 0.07, 0.04), (2014, 0.04, 0.03),
        (2015, 0.02, 0.08), (2016, 0.30, 0.38), (2017, 0.70, 1.15), (2018, 1.70, 2.20), (2019, 2.40, 1.80),
        (2020, 0.60, 0.10), (2021, 0.04, 0.05), (2022, 0.50, 3.00), (2023, 4.80, 5.30), (2024, 5.25, 4.80),
        (2025, 4.30, 4.00), (2026, 3.70, 3.60),
    ];
    let mut pts = Vec::with_capacity(H.len() * 2);
    for &(y, h1, h2) in H.iter() {
        pts.push((NaiveDate::from_ymd_opt(y, 1, 1).unwrap(), h1));
        pts.push((NaiveDate::from_ymd_opt(y, 7, 1).unwrap(), h2));
    }
    ScalarSeries::new("US 3M T-bill (approx)", pts)
}

/// Market data for a backtest.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct BacktestData {
    /// Underlying daily bars; `adj_close` is used as total-return index.
    pub prices: PriceSeries,
    /// Vol index in points (VIX 18.5), forward-filled onto price dates.
    pub vol_index: ScalarSeries,
    pub rate: RateSource,
    /// Continuous dividend yield of the underlying (SPY ≈ 0.013–0.018, QQQ ≈ 0.006–0.008).
    pub dividend_yield: f64,
}

impl BacktestData {
    pub fn new(prices: PriceSeries, vol_index: ScalarSeries, rate: RateSource, dividend_yield: f64) -> Self {
        Self { prices, vol_index, rate, dividend_yield }
    }
}

/// How the call strike is chosen at entry/roll.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum StrikeSelection {
    /// Target call delta at the surface IV (0.80 = classic stock replacement).
    Delta(f64),
    /// Strike / spot (0.80 = 20% in the money).
    Moneyness(f64),
}

/// Rolling LEAPS strategy parameters.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RollConfig {
    pub initial_capital: f64,
    pub strike: StrikeSelection,
    /// Calendar days to expiry when opening (snapped to a monthly expiry).
    pub entry_dte: u32,
    /// Roll when calendar DTE ≤ this (0 = hold to expiry).
    pub roll_dte: u32,
    /// Re-strike (roll up) when delta rises above this.
    pub roll_up_delta: Option<f64>,
    /// Re-strike (roll down) when delta falls below this.
    pub roll_down_delta: Option<f64>,
    /// Roll when the option mid has gained this fraction since entry (1.0 = +100%).
    pub profit_take_pct: Option<f64>,
    /// Delta-adjusted notional / equity (1.5 = 150% long exposure).
    pub target_exposure: f64,
    /// Resize when exposure leaves `target·(1±band)`. None/0 ⇒ resize only at rolls.
    pub rebalance_band: Option<f64>,
    /// Max fraction of equity paid as option premium when buying.
    pub max_premium_pct: f64,
    pub integer_contracts: bool,
    pub multiplier: f64,
    /// Idle cash earns the risk-free rate.
    pub cash_yield: bool,
    pub vol_model: SynthVolModel,
    pub costs: CostModel,
    /// Leveraged-ETF benchmark: borrow spread over the risk-free rate.
    pub lev_borrow_spread: f64,
    /// Leveraged-ETF benchmark: annual expense ratio.
    pub lev_expense_ratio: f64,
    pub start: Option<NaiveDate>,
    pub end: Option<NaiveDate>,
}

impl Default for RollConfig {
    /// Classic stock replacement: 0.80-delta call, ~18 months to expiry, roll at
    /// ~6 months (before theta accelerates), 1.5× delta exposure, ±25% band.
    fn default() -> Self {
        Self {
            initial_capital: 100_000.0,
            strike: StrikeSelection::Delta(0.80),
            entry_dte: 540,
            roll_dte: 180,
            roll_up_delta: None,
            roll_down_delta: None,
            profit_take_pct: None,
            target_exposure: 1.5,
            rebalance_band: Some(0.25),
            max_premium_pct: 0.9,
            integer_contracts: false,
            multiplier: 100.0,
            cash_yield: true,
            vol_model: SynthVolModel::spy(),
            costs: CostModel::spy(),
            lev_borrow_spread: 0.005,
            lev_expense_ratio: 0.0095,
            start: None,
            end: None,
        }
    }
}

impl RollConfig {
    /// Default config calibrated for SPY (VIX-driven).
    pub fn spy() -> Self {
        Self::default()
    }
    /// Default config calibrated for QQQ (VXN-driven).
    pub fn qqq() -> Self {
        Self { vol_model: SynthVolModel::qqq(), costs: CostModel::qqq(), ..Self::default() }
    }
    /// SPX index options (European, cash-settled; multiplier 100 on a ~$6,000 index).
    pub fn spx() -> Self {
        Self { vol_model: SynthVolModel::spx(), costs: CostModel::spx(), ..Self::default() }
    }
    /// Short human label, e.g. `Δ0.80 540/180d 1.50x`.
    pub fn label(&self) -> String {
        let k = match self.strike {
            StrikeSelection::Delta(d) => format!("Δ{d:.2}"),
            StrikeSelection::Moneyness(m) => format!("K/S{m:.2}"),
        };
        let mut s = format!("{k} {}/{}d {:.2}x", self.entry_dte, self.roll_dte, self.target_exposure);
        if let Some(u) = self.roll_up_delta {
            s.push_str(&format!(" up>{u:.2}"));
        }
        if let Some(b) = self.rebalance_band.filter(|b| *b > 0.0) {
            s.push_str(&format!(" ±{:.0}%", b * 100.0));
        }
        s
    }
}

// ---------------------------------------------------------------------------
// Outputs
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TradeAction {
    Open,
    Roll,
    Rebalance,
    Close,
}

/// One executed option trade.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Trade {
    pub date: NaiveDate,
    pub action: TradeAction,
    pub kind: OptionKind,
    pub strike: f64,
    pub expiry: NaiveDate,
    /// Signed contracts (+ buy, − sell).
    pub contracts: f64,
    /// Fill price per share.
    pub price: f64,
    pub underlying: f64,
    pub delta: f64,
    pub iv: f64,
    /// Spread + slippage + commission paid on this trade (dollars).
    pub cost: f64,
    pub reason: String,
}

/// End-of-day state.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct DayRecord {
    pub date: NaiveDate,
    pub equity: f64,
    /// Option underlying (price-return reconstruction, see module docs).
    pub spot: f64,
    pub contracts: f64,
    pub strike: f64,
    pub dte: i64,
    /// Per-option delta.
    pub delta: f64,
    /// Δ·S·multiplier·contracts / equity.
    pub exposure: f64,
    /// Market value of the option position.
    pub option_value: f64,
    pub cash: f64,
    pub iv: f64,
    /// Position theta, dollars per calendar day (negative).
    pub theta: f64,
}

/// Result of [`run_backtest`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BacktestResult {
    pub config: RollConfig,
    pub records: Vec<DayRecord>,
    pub trades: Vec<Trade>,
    pub stats: PerformanceStats,
    /// `[0]` = buy & hold (adj close), `[1]` = daily-rebalanced L× (L = target exposure).
    pub benchmarks: Vec<(String, Vec<(NaiveDate, f64)>)>,
    pub benchmark_stats: Vec<(String, PerformanceStats)>,
    pub total_costs: f64,
    pub n_rolls: usize,
    /// Average exposure on days with an open position.
    pub avg_exposure: f64,
    /// Average extrinsic value at entry, per year, as a fraction of spot (cost of carry).
    pub avg_extrinsic_pct_annual: f64,
    /// Average extrinsic at entry per year as a fraction of delta-notional
    /// (≈ implied financing + insurance cost per unit of exposure).
    pub avg_carry_pct_of_exposure: f64,
    /// Average position theta per year as a fraction of equity.
    pub avg_theta_pct_equity: f64,
    /// Average risk-free rate over the period (used for Sharpe).
    pub avg_rate: f64,
    pub wiped_out: bool,
    pub wiped_date: Option<NaiveDate>,
}

impl BacktestResult {
    /// Daily (date, equity) curve.
    pub fn equity_curve(&self) -> Vec<(NaiveDate, f64)> {
        self.records.iter().map(|r| (r.date, r.equity)).collect()
    }
}

// ---------------------------------------------------------------------------
// Expiry calendar (private on purpose: no coupling with strategy.rs)
// ---------------------------------------------------------------------------

pub(crate) fn third_friday(year: i32, month: u32) -> NaiveDate {
    let first = NaiveDate::from_ymd_opt(year, month, 1).expect("valid month");
    let wd = first.weekday().num_days_from_monday() as i64; // Fri = 4
    let off = (4 - wd).rem_euclid(7);
    first + Duration::days(off + 14)
}

fn shift_month(year: i32, month: u32, k: i32) -> (i32, u32) {
    let idx = year * 12 + month as i32 - 1 + k;
    (idx.div_euclid(12), (idx.rem_euclid(12) + 1) as u32)
}

/// Monthly 3rd-Friday expiry nearest to `as_of + dte`, strictly later than `as_of + min_dte`.
pub(crate) fn snap_monthly_expiry(as_of: NaiveDate, dte: u32, min_dte: u32) -> NaiveDate {
    let target = as_of + Duration::days(dte as i64);
    let min_date = as_of + Duration::days(min_dte as i64);
    let mut best: Option<NaiveDate> = None;
    for k in -1..=1 {
        let (y, m) = shift_month(target.year(), target.month(), k);
        let e = third_friday(y, m);
        if e <= min_date {
            continue;
        }
        if best.map_or(true, |b| (e - target).num_days().abs() < (b - target).num_days().abs()) {
            best = Some(e);
        }
    }
    best.unwrap_or_else(|| {
        // fall back to the first monthly expiry after min_date
        let (mut y, mut m) = (min_date.year(), min_date.month());
        loop {
            let e = third_friday(y, m);
            if e > min_date {
                return e;
            }
            (y, m) = shift_month(y, m, 1);
        }
    })
}

// ---------------------------------------------------------------------------
// Engine
// ---------------------------------------------------------------------------

/// Data aligned onto the price calendar (shared by optimiser / bootstrap).
#[derive(Debug, Clone)]
pub(crate) struct Aligned {
    pub symbol: String,
    pub dates: Vec<NaiveDate>,
    /// Total-return index (adj close).
    pub adj: Vec<f64>,
    /// Option underlying (price return).
    pub spot: Vec<f64>,
    /// Vol index points.
    pub vol: Vec<f64>,
    /// Decimal annual rate.
    pub rate: Vec<f64>,
    pub q: f64,
}

impl Aligned {
    pub fn new(data: &BacktestData) -> Result<Self> {
        if data.prices.len() < 2 {
            bail!("price history has fewer than 2 bars");
        }
        if data.vol_index.is_empty() {
            bail!("volatility index series is empty");
        }
        let bars = &data.prices.bars;
        let dates: Vec<NaiveDate> = bars.iter().map(|b| b.date).collect();
        let adj: Vec<f64> = bars.iter().map(|b| if b.adj_close > 0.0 { b.adj_close } else { b.close }).collect();
        // forward-fill vol index with a merge cursor
        let pts = &data.vol_index.points;
        let mut vol = Vec::with_capacity(dates.len());
        let mut j = 0usize;
        for &d in &dates {
            while j + 1 < pts.len() && pts[j + 1].0 <= d {
                j += 1;
            }
            vol.push(pts[j].1.max(1.0));
        }
        let rate = dates.iter().map(|&d| data.rate.rate_at(d)).collect();
        Ok(Self::from_parts(data.prices.symbol.clone(), dates, adj, vol, rate, data.dividend_yield))
    }

    pub fn from_parts(symbol: String, dates: Vec<NaiveDate>, adj: Vec<f64>, vol: Vec<f64>, rate: Vec<f64>, q: f64) -> Self {
        let last = *dates.last().expect("non-empty");
        let spot = dates
            .iter()
            .zip(&adj)
            .map(|(&d, &a)| a * (q * (last - d).num_days() as f64 / 365.25).exp())
            .collect();
        Self { symbol, dates, adj, spot, vol, rate, q }
    }

    /// Inclusive index range for [start, end].
    pub fn range(&self, start: Option<NaiveDate>, end: Option<NaiveDate>) -> Option<(usize, usize)> {
        let i0 = start.map_or(0, |s| self.dates.partition_point(|d| *d < s));
        let i1 = end.map_or(self.dates.len(), |e| self.dates.partition_point(|d| *d <= e));
        if i1 == 0 || i0 + 1 >= i1 {
            None
        } else {
            Some((i0, i1 - 1))
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct Pos {
    strike: f64,
    expiry: NaiveDate,
    n: f64,
    entry_mid: f64,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct SimOut {
    pub curve: Vec<(NaiveDate, f64)>,
    pub records: Vec<DayRecord>,
    pub trades: Vec<Trade>,
    pub total_costs: f64,
    pub n_rolls: usize,
    pub avg_exposure: f64,
    pub avg_extrinsic_pct_annual: f64,
    pub avg_carry_pct_of_exposure: f64,
    pub avg_theta_pct_equity: f64,
    pub avg_rate: f64,
    pub wiped_out: bool,
    pub wiped_date: Option<NaiveDate>,
    pub i0: usize,
    pub i1: usize,
}

#[inline]
fn mark(vm: &SynthVolModel, s: f64, k: f64, expiry: NaiveDate, d: NaiveDate, r: f64, q: f64, vix: f64) -> (f64, Greeks, f64) {
    let t = ((expiry - d).num_days() as f64 / 365.0).max(0.0);
    let iv = vm.iv(k, s, t.max(1.0 / 365.0), vix);
    (t, bs_greeks(OptionKind::Call, s, k, t, r, q, iv), iv)
}

/// On an exposure-band breach in `Delta` mode, re-strike instead of resizing when
/// the option delta has drifted further than this from the target delta (avoids
/// piling into far-OTM calls after a sell-off or holding near-1-delta calls).
pub const RESTRIKE_DELTA_GAP: f64 = 0.15;

pub(crate) fn validate(cfg: &RollConfig) -> Result<()> {
    if !(cfg.initial_capital > 0.0) {
        bail!("initial_capital must be > 0");
    }
    if cfg.entry_dte < 2 || cfg.entry_dte <= cfg.roll_dte {
        bail!("entry_dte ({}) must exceed roll_dte ({})", cfg.entry_dte, cfg.roll_dte);
    }
    match cfg.strike {
        StrikeSelection::Delta(d) if !(d > 0.0 && d < 1.0) => bail!("delta must be in (0,1)"),
        StrikeSelection::Moneyness(m) if !(m > 0.0) => bail!("moneyness must be > 0"),
        _ => {}
    }
    if !(cfg.target_exposure >= 0.0) || !(cfg.max_premium_pct > 0.0) || !(cfg.multiplier > 0.0) {
        bail!("target_exposure >= 0, max_premium_pct > 0 and multiplier > 0 required");
    }
    Ok(())
}

/// Core simulation on aligned data. `full` = keep daily records and trade list.
pub(crate) fn simulate(al: &Aligned, cfg: &RollConfig, full: bool) -> Result<SimOut> {
    validate(cfg)?;
    let Some((i0, i1)) = al.range(cfg.start, cfg.end) else {
        bail!("no data in the requested date range");
    };
    let vm = &cfg.vol_model;
    let cm = &cfg.costs;
    let mult = cfg.multiplier;
    let q = al.q;
    let comm = cm.commission_per_contract;
    let band = cfg.rebalance_band.unwrap_or(0.0);
    let wipe_level = cfg.initial_capital * 1e-3;
    let min_dte_new = cfg.roll_dte + 14;

    let n_days = i1 - i0 + 1;
    let mut out = SimOut { i0, i1, ..Default::default() };
    out.curve.reserve(n_days);
    if full {
        out.records.reserve(n_days);
    }

    let mut cash = cfg.initial_capital;
    let mut pos: Option<Pos> = None;
    let mut opened_before = false;
    let (mut exp_sum, mut exp_cnt) = (0.0, 0usize);
    let (mut ext_sum, mut carry_sum, mut ext_cnt) = (0.0, 0.0, 0usize);
    let mut theta_sum = 0.0;
    let mut rate_sum = 0.0;
    let mut prev_date = al.dates[i0];
    let mut prev_rate = al.rate[i0];

    for i in i0..=i1 {
        let d = al.dates[i];
        let s = al.spot[i];
        let vix = al.vol[i];
        let r = al.rate[i];
        rate_sum += r;
        if i > i0 && cfg.cash_yield && cash > 0.0 {
            let dt = (d - prev_date).num_days() as f64 / 365.0;
            cash *= (prev_rate * dt).exp();
        }
        prev_date = d;
        prev_rate = r;

        // 2. expiry settlement
        if let Some(p) = pos {
            if d >= p.expiry {
                let intr = (s - p.strike).max(0.0);
                let c = if intr > 0.0 { comm * p.n } else { 0.0 };
                cash += intr * p.n * mult - c;
                out.total_costs += c;
                if full {
                    out.trades.push(Trade {
                        date: d,
                        action: TradeAction::Close,
                        kind: OptionKind::Call,
                        strike: p.strike,
                        expiry: p.expiry,
                        contracts: -p.n,
                        price: intr,
                        underlying: s,
                        delta: if intr > 0.0 { 1.0 } else { 0.0 },
                        iv: 0.0,
                        cost: c,
                        reason: "到期結算".into(),
                    });
                }
                pos = None;
            }
        }

        // 3. mark
        let mut g = Greeks::default();
        let mut iv = 0.0;
        if let Some(p) = pos {
            let (_, gg, v) = mark(vm, s, p.strike, p.expiry, d, r, q, vix);
            g = gg;
            iv = v;
        }
        let pos_val = pos.map_or(0.0, |p| p.n * mult * g.price);
        let equity = cash + pos_val;

        // 4. wipe-out
        if equity <= wipe_level {
            out.wiped_out = true;
            out.wiped_date = Some(d);
            let eq = equity.max(0.0);
            out.curve.push((d, eq));
            if full {
                out.records.push(DayRecord {
                    date: d,
                    equity: eq,
                    spot: s,
                    contracts: pos.map_or(0.0, |p| p.n),
                    strike: pos.map_or(0.0, |p| p.strike),
                    dte: pos.map_or(0, |p| (p.expiry - d).num_days()),
                    delta: g.delta,
                    exposure: 0.0,
                    option_value: pos_val,
                    cash,
                    iv,
                    theta: 0.0,
                });
                if i < i1 {
                    out.records.push(DayRecord { date: al.dates[i1], spot: al.spot[i1], ..*out.records.last().unwrap() });
                }
            }
            if i < i1 {
                out.curve.push((al.dates[i1], eq));
            }
            break;
        }

        // 5. triggers
        enum Act {
            Nothing,
            Open,
            Roll(&'static str),
            Rebalance,
        }
        let act = match pos {
            None => Act::Open,
            Some(p) => {
                let dte = (p.expiry - d).num_days();
                let exposure = if equity > 0.0 { g.delta * s * mult * p.n / equity } else { 0.0 };
                if dte <= cfg.roll_dte as i64 {
                    Act::Roll("DTE 到達展期門檻")
                } else if cfg.roll_up_delta.is_some_and(|x| g.delta > x) {
                    Act::Roll("Delta 過高，上移履約價")
                } else if cfg.roll_down_delta.is_some_and(|x| g.delta < x) {
                    Act::Roll("Delta 過低，下移履約價")
                } else if cfg.profit_take_pct.is_some_and(|x| g.price >= p.entry_mid * (1.0 + x)) {
                    Act::Roll("獲利了結並重設履約價")
                } else if band > 0.0 && cfg.target_exposure > 0.0 && (exposure / cfg.target_exposure - 1.0).abs() > band {
                    match cfg.strike {
                        StrikeSelection::Delta(td) if (g.delta - td).abs() > RESTRIKE_DELTA_GAP => {
                            Act::Roll("曝險與 Delta 皆偏離，重設履約價")
                        }
                        _ => Act::Rebalance,
                    }
                } else {
                    Act::Nothing
                }
            }
        };

        let mut traded = false;
        match act {
            Act::Nothing => {}
            Act::Rebalance => {
                let p = pos.as_mut().unwrap();
                let target_n = cfg.target_exposure * equity / (g.delta * s * mult).max(1e-12);
                let buy = cm.buy_price(g.price);
                let sell = cm.sell_price(g.price);
                let mut new_n = target_n;
                if new_n > p.n {
                    let cap_n = cfg.max_premium_pct * equity / (g.price * mult).max(1e-12);
                    let cash_n = p.n + cash / (buy * mult + comm);
                    new_n = new_n.min(cap_n.max(p.n)).min(cash_n);
                }
                if cfg.integer_contracts {
                    new_n = if new_n > p.n { new_n.floor() } else { new_n.round() };
                    if target_n > p.n {
                        // round to nearest when affordable
                        let rn = target_n.round();
                        let cap_n = cfg.max_premium_pct * equity / (g.price * mult).max(1e-12);
                        if rn <= cap_n.max(p.n) && (rn - p.n) * (buy * mult + comm) <= cash {
                            new_n = rn;
                        }
                    }
                }
                let dn = new_n - p.n;
                let meaningful = if cfg.integer_contracts { dn.abs() >= 1.0 } else { dn.abs() > 0.01 * p.n.max(1e-9) };
                if meaningful {
                    let (px, c) = if dn > 0.0 {
                        (buy, dn * (buy - g.price) * mult + dn * comm)
                    } else {
                        (sell, -dn * (g.price - sell) * mult - dn * comm)
                    };
                    // selling options worth less than the commission makes no sense
                    let meaningful = dn > 0.0 || sell * mult > comm;
                    if meaningful {
                    cash -= dn * px * mult + dn.abs() * comm;
                    out.total_costs += c;
                    p.n = new_n;
                    traded = true;
                    if full {
                        out.trades.push(Trade {
                            date: d,
                            action: TradeAction::Rebalance,
                            kind: OptionKind::Call,
                            strike: p.strike,
                            expiry: p.expiry,
                            contracts: dn,
                            price: px,
                            underlying: s,
                            delta: g.delta,
                            iv,
                            cost: c,
                            reason: "曝險偏離目標，再平衡口數".into(),
                        });
                    }
                    }
                }
            }
            Act::Open | Act::Roll(_) => {
                let is_roll = matches!(act, Act::Roll(_));
                let reason = match act {
                    Act::Roll(r) => r,
                    _ if opened_before => "重新建倉",
                    _ => "初始建倉",
                };
                // close existing
                if let Some(p) = pos.take() {
                    let mut sell = cm.sell_price(g.price);
                    let mut leg_comm = comm;
                    if sell * mult <= comm {
                        // worthless: abandon instead of paying commission to sell
                        sell = 0.0;
                        leg_comm = 0.0;
                    }
                    let c = p.n * (g.price - sell) * mult + p.n * leg_comm;
                    cash += p.n * sell * mult - p.n * leg_comm;
                    out.total_costs += c;
                    if full {
                        out.trades.push(Trade {
                            date: d,
                            action: TradeAction::Roll,
                            kind: OptionKind::Call,
                            strike: p.strike,
                            expiry: p.expiry,
                            contracts: -p.n,
                            price: sell,
                            underlying: s,
                            delta: g.delta,
                            iv,
                            cost: c,
                            reason: reason.into(),
                        });
                    }
                    traded = true;
                }
                // open new
                let equity_now = cash;
                let expiry = snap_monthly_expiry(d, cfg.entry_dte, min_dte_new);
                let t = (expiry - d).num_days() as f64 / 365.0;
                let k = match cfg.strike {
                    StrikeSelection::Delta(dl) => vm.strike_for_delta(OptionKind::Call, dl, s, t, r, q, vix),
                    StrikeSelection::Moneyness(m) => m * s,
                };
                let (_, ng, niv) = mark(vm, s, k, expiry, d, r, q, vix);
                let mid = ng.price;
                let buy = cm.buy_price(mid);
                if mid > 0.0 && ng.delta > 1e-6 && equity_now > 0.0 {
                    let unit = buy * mult + comm;
                    let target_n = cfg.target_exposure * equity_now / (ng.delta * s * mult);
                    let cap_n = cfg.max_premium_pct * equity_now / unit;
                    let cash_n = cash / unit;
                    let mut n = target_n.min(cap_n).min(cash_n).max(0.0);
                    if cfg.integer_contracts {
                        let rn = target_n.round();
                        n = if rn <= cap_n.min(cash_n) { rn } else { cap_n.min(cash_n).floor() };
                    }
                    if n > 0.0 {
                        let c = n * (buy - mid) * mult + n * comm;
                        cash -= n * unit;
                        out.total_costs += c;
                        pos = Some(Pos { strike: k, expiry, n, entry_mid: mid });
                        g = ng;
                        iv = niv;
                        traded = true;
                        if is_roll || opened_before {
                            out.n_rolls += 1;
                        }
                        opened_before = true;
                        let ext = mid - (s - k).max(0.0);
                        if t > 0.0 {
                            ext_sum += ext / s / t;
                            carry_sum += ext / (ng.delta * s) / t;
                            ext_cnt += 1;
                        }
                        if full {
                            out.trades.push(Trade {
                                date: d,
                                action: if is_roll { TradeAction::Roll } else { TradeAction::Open },
                                kind: OptionKind::Call,
                                strike: k,
                                expiry,
                                contracts: n,
                                price: buy,
                                underlying: s,
                                delta: ng.delta,
                                iv: niv,
                                cost: c,
                                reason: reason.into(),
                            });
                        }
                    }
                }
            }
        }

        // re-mark after trades for the record (only the position changed, not the mark)
        let (n, k, exp) = pos.map_or((0.0, 0.0, d), |p| (p.n, p.strike, p.expiry));
        let opt_val = n * mult * g.price;
        let eq = cash + opt_val;
        let exposure = if eq > 0.0 { g.delta * s * mult * n / eq } else { 0.0 };
        if n > 0.0 {
            exp_sum += exposure;
            exp_cnt += 1;
            if eq > 0.0 {
                theta_sum += -g.theta * 365.0 * n * mult / eq;
            }
        }
        let _ = traded;
        out.curve.push((d, eq));
        if full {
            out.records.push(DayRecord {
                date: d,
                equity: eq,
                spot: s,
                contracts: n,
                strike: k,
                dte: (exp - d).num_days(),
                delta: if n > 0.0 { g.delta } else { 0.0 },
                exposure,
                option_value: opt_val,
                cash,
                iv: if n > 0.0 { iv } else { 0.0 },
                theta: n * mult * g.theta,
            });
        }
    }
    out.avg_exposure = if exp_cnt > 0 { exp_sum / exp_cnt as f64 } else { 0.0 };
    out.avg_theta_pct_equity = if exp_cnt > 0 { theta_sum / exp_cnt as f64 } else { 0.0 };
    if ext_cnt > 0 {
        out.avg_extrinsic_pct_annual = ext_sum / ext_cnt as f64;
        out.avg_carry_pct_of_exposure = carry_sum / ext_cnt as f64;
    }
    out.avg_rate = rate_sum / n_days as f64;
    Ok(out)
}

/// Buy & hold of the total-return index over `[i0, i1]`.
pub(crate) fn buy_hold_curve(al: &Aligned, i0: usize, i1: usize, capital: f64) -> Vec<(NaiveDate, f64)> {
    let a0 = al.adj[i0];
    (i0..=i1).map(|i| (al.dates[i], capital * al.adj[i] / a0)).collect()
}

/// Daily-rebalanced leveraged-ETF-like curve: `L·r − (L−1)·(rf + spread if L>1) − ER`.
pub(crate) fn leveraged_curve(al: &Aligned, i0: usize, i1: usize, capital: f64, lev: f64, spread: f64, er: f64) -> Vec<(NaiveDate, f64)> {
    let mut v = capital;
    let mut out = Vec::with_capacity(i1 - i0 + 1);
    out.push((al.dates[i0], v));
    for i in i0 + 1..=i1 {
        let dt = (al.dates[i] - al.dates[i - 1]).num_days() as f64 / 365.0;
        let r = al.adj[i] / al.adj[i - 1] - 1.0;
        let fin = (lev - 1.0) * (al.rate[i - 1] + if lev > 1.0 { spread } else { 0.0 });
        v *= 1.0 + lev * r - fin * dt - er * dt;
        v = v.max(0.0);
        out.push((al.dates[i], v));
    }
    out
}

/// Run one LEAPS roll backtest.
pub fn run_backtest(data: &BacktestData, cfg: &RollConfig) -> Result<BacktestResult> {
    let al = Aligned::new(data)?;
    run_on_aligned(&al, cfg)
}

pub(crate) fn run_on_aligned(al: &Aligned, cfg: &RollConfig) -> Result<BacktestResult> {
    let sim = simulate(al, cfg, true)?;
    let stats = compute_stats(&sim.curve, sim.avg_rate);
    let bh = buy_hold_curve(al, sim.i0, sim.i1, cfg.initial_capital);
    let lev = leveraged_curve(al, sim.i0, sim.i1, cfg.initial_capital, cfg.target_exposure, cfg.lev_borrow_spread, cfg.lev_expense_ratio);
    let sym = if al.symbol.is_empty() { "標的".to_string() } else { al.symbol.clone() };
    let benchmarks = vec![
        (format!("買進持有 {sym}"), bh),
        (format!("每日再平衡 {:.2}x {sym}", cfg.target_exposure), lev),
    ];
    let benchmark_stats = benchmarks.iter().map(|(n, c)| (n.clone(), compute_stats(c, sim.avg_rate))).collect();
    Ok(BacktestResult {
        config: cfg.clone(),
        records: sim.records,
        trades: sim.trades,
        stats,
        benchmarks,
        benchmark_stats,
        total_costs: sim.total_costs,
        n_rolls: sim.n_rolls,
        avg_exposure: sim.avg_exposure,
        avg_extrinsic_pct_annual: sim.avg_extrinsic_pct_annual,
        avg_carry_pct_of_exposure: sim.avg_carry_pct_of_exposure,
        avg_theta_pct_equity: sim.avg_theta_pct_equity,
        avg_rate: sim.avg_rate,
        wiped_out: sim.wiped_out,
        wiped_date: sim.wiped_date,
    })
}

#[cfg(test)]
pub(crate) mod test_util {
    use super::*;
    use crate::history::Bar;
    use rand::{rngs::StdRng, Rng, SeedableRng};
    use rand_distr::StandardNormal;

    /// Business-day GBM path with constant vol index.
    pub fn gbm_data(years: f64, mu: f64, sigma: f64, vix: f64, r: f64, q: f64, seed: u64) -> BacktestData {
        let mut rng = StdRng::seed_from_u64(seed);
        let mut d = NaiveDate::from_ymd_opt(2000, 1, 3).unwrap();
        let n = (years * 252.0) as usize;
        let dt = 1.0 / 252.0;
        let mut p = 100.0;
        let mut bars = Vec::with_capacity(n);
        let mut vols = Vec::with_capacity(n);
        for _ in 0..n {
            bars.push(Bar { date: d, open: p, high: p, low: p, close: p, adj_close: p, volume: 0.0 });
            vols.push((d, vix));
            let z: f64 = rng.sample(StandardNormal);
            p *= ((mu - 0.5 * sigma * sigma) * dt + sigma * dt.sqrt() * z).exp();
            d += Duration::days(if d.weekday() == chrono::Weekday::Fri { 3 } else { 1 });
        }
        BacktestData::new(PriceSeries::new("TEST", bars), ScalarSeries::new("VOL", vols), RateSource::Constant(r), q)
    }

    /// Deterministic path from explicit closes (business days from 2000-01-03).
    pub fn path_data(closes: &[f64], vix: f64, r: f64) -> BacktestData {
        let mut d = NaiveDate::from_ymd_opt(2000, 1, 3).unwrap();
        let mut bars = Vec::new();
        let mut vols = Vec::new();
        for &p in closes {
            bars.push(Bar { date: d, open: p, high: p, low: p, close: p, adj_close: p, volume: 0.0 });
            vols.push((d, vix));
            d += Duration::days(if d.weekday() == chrono::Weekday::Fri { 3 } else { 1 });
        }
        BacktestData::new(PriceSeries::new("PATH", bars), ScalarSeries::new("VOL", vols), RateSource::Constant(r), 0.0)
    }
}

#[cfg(test)]
mod tests {
    use super::test_util::*;
    use super::*;

    #[test]
    fn third_friday_and_snap() {
        assert_eq!(third_friday(2026, 1), NaiveDate::from_ymd_opt(2026, 1, 16).unwrap());
        assert_eq!(third_friday(2025, 11), NaiveDate::from_ymd_opt(2025, 11, 21).unwrap());
        assert_eq!(third_friday(2027, 1), NaiveDate::from_ymd_opt(2027, 1, 15).unwrap());
        let as_of = NaiveDate::from_ymd_opt(2026, 11, 4).unwrap();
        let e = snap_monthly_expiry(as_of, 540, 194);
        assert_eq!(e.weekday(), chrono::Weekday::Fri);
        assert!(((e - as_of).num_days() - 540).abs() <= 20);
        assert_eq!(shift_month(2026, 12, 1), (2027, 1));
        assert_eq!(shift_month(2026, 1, -1), (2025, 12));
    }

    #[test]
    fn deep_itm_no_cost_tracks_buy_hold() {
        let data = gbm_data(10.0, 0.08, 0.18, 18.0, 0.0, 0.0, 7);
        let cfg = RollConfig {
            strike: StrikeSelection::Moneyness(0.3),
            target_exposure: 1.0,
            rebalance_band: Some(0.02),
            max_premium_pct: 1.0,
            costs: CostModel::zero(),
            cash_yield: false,
            ..RollConfig::default()
        };
        let res = run_backtest(&data, &cfg).unwrap();
        let bh = &res.benchmark_stats[0].1;
        assert!((res.stats.cagr - bh.cagr).abs() < 0.005, "lp {} bh {}", res.stats.cagr, bh.cagr);
        assert!((res.avg_exposure - 1.0).abs() < 0.03, "{}", res.avg_exposure);
        assert!((res.stats.max_drawdown - bh.max_drawdown).abs() < 0.03);
    }

    #[test]
    fn costs_reduce_returns_monotonically() {
        let data = gbm_data(12.0, 0.08, 0.2, 20.0, 0.03, 0.01, 11);
        let mut last = f64::INFINITY;
        for costs in [CostModel::zero(), CostModel::spy(), CostModel::conservative()] {
            let cfg = RollConfig { costs, ..RollConfig::default() };
            let res = run_backtest(&data, &cfg).unwrap();
            assert!(res.stats.final_value < last, "{} !< {}", res.stats.final_value, last);
            last = res.stats.final_value;
            if costs == CostModel::zero() {
                assert!(res.total_costs.abs() < 1e-9);
            } else {
                assert!(res.total_costs > 0.0);
            }
        }
    }

    #[test]
    fn integer_contracts_are_integers() {
        let data = gbm_data(5.0, 0.08, 0.2, 20.0, 0.03, 0.01, 3);
        let cfg = RollConfig { integer_contracts: true, ..RollConfig::default() };
        let res = run_backtest(&data, &cfg).unwrap();
        assert!(res.records.iter().all(|r| (r.contracts - r.contracts.round()).abs() < 1e-12));
        assert!(res.trades.iter().all(|t| (t.contracts - t.contracts.round()).abs() < 1e-12));
        assert!(res.records.iter().any(|r| r.contracts >= 1.0));
        let bad: Vec<_> = res.records.iter().filter(|r| r.cash < -1e-6).take(3).collect();
        assert!(bad.is_empty(), "{bad:?}");
    }

    #[test]
    fn roll_count_matches_schedule() {
        let data = gbm_data(10.0, 0.08, 0.18, 18.0, 0.03, 0.01, 5);
        let cfg = RollConfig { rebalance_band: None, ..RollConfig::default() };
        let res = run_backtest(&data, &cfg).unwrap();
        // ~360 day holding period over ~10 calendar years ⇒ ~10 rolls
        assert!((8..=12).contains(&res.n_rolls), "rolls = {}", res.n_rolls);
        assert!(res.trades.iter().all(|t| t.action != TradeAction::Rebalance));
        assert_eq!(res.trades.iter().filter(|t| t.action == TradeAction::Open).count(), 1);
        // every opened contract has DTE in [roll_dte, entry_dte + 1 month]
        for t in res.trades.iter().filter(|t| t.contracts > 0.0) {
            let dte = (t.expiry - t.date).num_days();
            assert!((500..=575).contains(&dte), "{dte}");
        }
    }

    #[test]
    fn wipe_out_stops() {
        // short-dated ATM calls held to expiry, all-in, market falls 40% ⇒ worthless
        let mut closes = vec![100.0; 5];
        closes.extend((0..120).map(|i| 100.0 * (1.0 - 0.4 * i as f64 / 119.0)));
        closes.extend(vec![60.0; 200]);
        let data = path_data(&closes, 20.0, 0.0);
        let cfg = RollConfig {
            strike: StrikeSelection::Delta(0.5),
            entry_dte: 60,
            roll_dte: 0,
            target_exposure: 50.0,
            max_premium_pct: 1.0,
            rebalance_band: None,
            costs: CostModel::zero(),
            ..RollConfig::default()
        };
        let res = run_backtest(&data, &cfg).unwrap();
        assert!(res.wiped_out);
        assert!(res.stats.cagr <= -0.99, "{}", res.stats.cagr);
        assert_eq!(res.records.last().unwrap().date, data.prices.last_date().unwrap());
    }

    #[test]
    fn calls_forgo_dividends() {
        let base = gbm_data(8.0, 0.08, 0.18, 18.0, 0.02, 0.0, 9);
        let mut divs = base.clone();
        divs.dividend_yield = 0.03;
        let cfg = RollConfig { costs: CostModel::zero(), ..RollConfig::default() };
        let a = run_backtest(&base, &cfg).unwrap();
        let b = run_backtest(&divs, &cfg).unwrap();
        // Same total-return path; with q=3% the price path grows 3%/yr slower, but the
        // forward (and so the call premium) is lower too: the call's excess return
        // depends on the *total* return minus r, so the CAGR barely changes.
        assert!(b.records[0].spot > a.records[0].spot * 1.2); // anchored at the last date
        assert!((b.stats.cagr - a.stats.cagr).abs() < 0.01, "{} vs {}", b.stats.cagr, a.stats.cagr);
        assert!((a.benchmark_stats[0].1.cagr - b.benchmark_stats[0].1.cagr).abs() < 1e-12);
    }

    #[test]
    fn leveraged_benchmark_basic() {
        let data = gbm_data(5.0, 0.08, 0.2, 20.0, 0.0, 0.0, 1);
        let cfg = RollConfig { target_exposure: 1.0, lev_expense_ratio: 0.0, ..RollConfig::default() };
        let res = run_backtest(&data, &cfg).unwrap();
        let a = &res.benchmarks[0].1;
        let b = &res.benchmarks[1].1;
        assert!((a.last().unwrap().1 - b.last().unwrap().1).abs() < 1e-6 * a.last().unwrap().1);
    }

    #[test]
    fn date_range_respected() {
        let data = gbm_data(6.0, 0.08, 0.2, 20.0, 0.02, 0.0, 2);
        let s = NaiveDate::from_ymd_opt(2001, 6, 1).unwrap();
        let e = NaiveDate::from_ymd_opt(2003, 6, 1).unwrap();
        let cfg = RollConfig { start: Some(s), end: Some(e), ..RollConfig::default() };
        let res = run_backtest(&data, &cfg).unwrap();
        assert!(res.records.first().unwrap().date >= s);
        assert!(res.records.last().unwrap().date <= e);
        assert!(run_backtest(&data, &RollConfig { entry_dte: 100, roll_dte: 120, ..cfg }).is_err());
    }
}
