//! Multi-leg option strategy model, analytics and strategy templates.
//!
//! # Conventions
//! * Leg `quantity` is signed: `+` long, `-` short. Options are counted in
//!   **contracts** (multiplier 100), stock in **shares** (multiplier 1).
//! * `entry_price` and every per-unit price are **per share**.
//! * Position values and P&L are in **dollars** for the whole strategy.
//! * [`Strategy::entry_cost`] is the net cash paid to open every leg
//!   (stock included). Positive = debit (paid), negative = credit (received).
//!   [`Strategy::net_premium`] is the same but restricted to option legs.
//! * P&L at a spot/date = mark-to-model value − entry cost.
//! * Aggregated Greeks ([`Strategy::greeks`]) are in **position terms**:
//!   - `price`: position value in $;
//!   - `delta`: share-equivalents ($ P&L per $1 move of the underlying);
//!   - `gamma`: change of share-equivalent delta per $1 move;
//!   - `theta`: $ per calendar day;
//!   - `vega`: $ per 1 vol point (0.01);
//!   - `rho`: $ per 1 rate point (0.01).
//! * Probability functions start the lognormal spot distribution at
//!   `params.spot` on `self.created` (re-anchor `created` to "today" when
//!   analysing an existing position).

use crate::chain::{OptionChain, OptionKind, OptionQuote};
use crate::pricing::{bs_greeks, bs_price, implied_vol, norm_cdf, price_with_style, strike_for_delta, year_frac, Greeks};
use anyhow::{anyhow, bail, Result};
use chrono::{Datelike, Duration, NaiveDate, Weekday};
use serde::{Deserialize, Serialize};

/// Contract multiplier of US equity/index options.
pub const OPTION_MULTIPLIER: f64 = 100.0;
/// Minimum volatility used when marking (after applying `vol_shift`).
const MIN_VOL: f64 = 0.005;

// ---------------------------------------------------------------------------
// Legs
// ---------------------------------------------------------------------------

/// What a leg holds.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum LegInstrument {
    Option { kind: OptionKind, strike: f64, expiry: NaiveDate },
    Stock,
}

/// One leg of a strategy.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Leg {
    pub instrument: LegInstrument,
    /// Signed size: + long / − short, in contracts (options) or shares (stock).
    pub quantity: f64,
    /// Fill price per share.
    pub entry_price: f64,
    /// 100 for options, 1 for stock.
    pub multiplier: f64,
    /// Volatility used to mark the leg (decimal). Ignored for stock.
    pub iv: f64,
}

impl Leg {
    /// Option leg with the standard 100 multiplier.
    pub fn option(kind: OptionKind, strike: f64, expiry: NaiveDate, quantity: f64, entry_price: f64, iv: f64) -> Self {
        Leg {
            instrument: LegInstrument::Option { kind, strike, expiry },
            quantity,
            entry_price,
            multiplier: OPTION_MULTIPLIER,
            iv,
        }
    }

    /// Stock leg (`shares` signed, multiplier 1).
    pub fn stock(shares: f64, entry_price: f64) -> Self {
        Leg { instrument: LegInstrument::Stock, quantity: shares, entry_price, multiplier: 1.0, iv: 0.0 }
    }

    pub fn is_option(&self) -> bool {
        matches!(self.instrument, LegInstrument::Option { .. })
    }
    pub fn is_long(&self) -> bool {
        self.quantity > 0.0
    }
    pub fn kind(&self) -> Option<OptionKind> {
        match self.instrument {
            LegInstrument::Option { kind, .. } => Some(kind),
            LegInstrument::Stock => None,
        }
    }
    pub fn strike(&self) -> Option<f64> {
        match self.instrument {
            LegInstrument::Option { strike, .. } => Some(strike),
            LegInstrument::Stock => None,
        }
    }
    pub fn expiry(&self) -> Option<NaiveDate> {
        match self.instrument {
            LegInstrument::Option { expiry, .. } => Some(expiry),
            LegInstrument::Stock => None,
        }
    }
    /// True when an option leg has reached (or passed) its expiry on `date`.
    pub fn is_expired(&self, date: NaiveDate) -> bool {
        self.expiry().is_some_and(|e| date >= e)
    }

    /// Signed cash paid to open the leg ($). Positive = debit.
    pub fn cost(&self) -> f64 {
        self.quantity * self.entry_price * self.multiplier
    }

    /// Marking volatility after the what-if shift.
    pub fn marking_vol(&self, params: &MarketParams) -> f64 {
        (self.iv + params.vol_shift).max(MIN_VOL)
    }

    /// Model value of one unit (per share) at `spot` on `date`.
    /// Expired options are worth intrinsic value.
    pub fn unit_value(&self, spot: f64, date: NaiveDate, params: &MarketParams) -> f64 {
        match self.instrument {
            LegInstrument::Stock => spot,
            LegInstrument::Option { kind, strike, expiry } => {
                let t = year_frac(date, expiry);
                if t <= 0.0 {
                    intrinsic(kind, spot, strike)
                } else {
                    let v = self.marking_vol(params);
                    if params.american {
                        price_with_style(kind, spot, strike, t, params.r, params.q, v, true)
                    } else {
                        bs_price(kind, spot, strike, t, params.r, params.q, v)
                    }
                }
            }
        }
    }

    /// Signed position value of the leg ($).
    pub fn value(&self, spot: f64, date: NaiveDate, params: &MarketParams) -> f64 {
        self.quantity * self.multiplier * self.unit_value(spot, date, params)
    }

    /// Position Greeks of this leg (see module docs for units). Greeks use
    /// Black-Scholes even when `params.american` is set; `price` honours the style.
    pub fn greeks(&self, spot: f64, date: NaiveDate, params: &MarketParams) -> Greeks {
        let scale = self.quantity * self.multiplier;
        match self.instrument {
            LegInstrument::Stock => Greeks { price: spot * scale, delta: scale, ..Default::default() },
            LegInstrument::Option { kind, strike, expiry } => {
                let t = year_frac(date, expiry).max(0.0);
                let mut g = bs_greeks(kind, spot, strike, t, params.r, params.q, self.marking_vol(params));
                g.price = self.unit_value(spot, date, params);
                g * scale
            }
        }
    }

    /// Human readable one-line description, e.g. `+1 C 600 2027-01-15 @ 12.30 (IV 21.0%)`.
    pub fn describe(&self) -> String {
        match self.instrument {
            LegInstrument::Stock => format!("{:+} 股 @ {:.2}", self.quantity, self.entry_price),
            LegInstrument::Option { kind, strike, expiry } => format!(
                "{:+} {} {} {} @ {:.2} (IV {:.1}%)",
                self.quantity,
                kind.label(),
                fmt_strike(strike),
                expiry,
                self.entry_price,
                self.iv * 100.0
            ),
        }
    }
}

fn fmt_strike(k: f64) -> String {
    if (k - k.round()).abs() < 1e-9 {
        format!("{k:.0}")
    } else {
        format!("{k:.2}")
    }
}

fn intrinsic(kind: OptionKind, s: f64, k: f64) -> f64 {
    match kind {
        OptionKind::Call => (s - k).max(0.0),
        OptionKind::Put => (k - s).max(0.0),
    }
}

/// Market assumptions used for marking.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct MarketParams {
    /// Reference (current) spot. Used to size scan ranges and as the start of
    /// probability distributions; valuation functions take their own `spot`.
    pub spot: f64,
    pub r: f64,
    pub q: f64,
    /// Additive volatility shift applied to every leg's IV (0.05 = +5 vol pts).
    pub vol_shift: f64,
    /// Price options as American (binomial) instead of European BS.
    pub american: bool,
}

impl MarketParams {
    pub fn new(spot: f64, r: f64, q: f64) -> Self {
        MarketParams { spot, r, q, vol_shift: 0.0, american: false }
    }
    pub fn with_vol_shift(mut self, shift: f64) -> Self {
        self.vol_shift = shift;
        self
    }
    pub fn with_american(mut self, american: bool) -> Self {
        self.american = american;
        self
    }
}

impl Default for MarketParams {
    fn default() -> Self {
        MarketParams::new(100.0, 0.04, 0.0)
    }
}

// ---------------------------------------------------------------------------
// Strategy
// ---------------------------------------------------------------------------

/// A multi-leg position.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Strategy {
    pub name: String,
    pub underlying: String,
    pub legs: Vec<Leg>,
    /// Date the position was opened (start of probability / MC horizons).
    pub created: NaiveDate,
}

/// Result of [`Strategy::max_profit_loss`].
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct MaxProfitLoss {
    /// Highest P&L found on the scan (finite even when `profit_unbounded`).
    pub max_profit: f64,
    pub max_profit_spot: f64,
    /// Lowest P&L found on the scan, as a P&L value (negative = loss).
    pub max_loss: f64,
    pub max_loss_spot: f64,
    /// P&L keeps rising as spot → ∞ (e.g. long call).
    pub profit_unbounded: bool,
    /// P&L keeps falling as spot → ∞ (e.g. naked short call).
    pub loss_unbounded: bool,
}

/// One-stop summary for GUIs/CLIs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StrategySummary {
    pub entry_cost: f64,
    pub net_premium: f64,
    pub capital_required: f64,
    pub margin_required: f64,
    /// Position Greeks at `params.spot` on `created`.
    pub greeks: Greeks,
    /// Breakevens at the evaluation date.
    pub breakevens: Vec<f64>,
    pub max_profit_loss: MaxProfitLoss,
    pub probability_of_profit: f64,
    pub expected_pnl: f64,
    /// Dollar delta / capital required (≈ effective leverage vs. holding stock).
    pub leverage: f64,
    pub evaluation_date: NaiveDate,
}

impl Strategy {
    pub fn new(name: impl Into<String>, underlying: impl Into<String>, created: NaiveDate) -> Self {
        Strategy { name: name.into(), underlying: underlying.into(), legs: Vec::new(), created }
    }
    pub fn with_leg(mut self, leg: Leg) -> Self {
        self.legs.push(leg);
        self
    }
    pub fn add_leg(&mut self, leg: Leg) {
        self.legs.push(leg);
    }

    /// Net option premium ($): Σ qty·price·multiplier over option legs. > 0 = debit paid.
    pub fn net_premium(&self) -> f64 {
        self.legs.iter().filter(|l| l.is_option()).map(Leg::cost).sum()
    }

    /// Net cash to open every leg including stock ($). > 0 = debit paid.
    pub fn entry_cost(&self) -> f64 {
        self.legs.iter().map(Leg::cost).sum()
    }

    pub fn earliest_expiry(&self) -> Option<NaiveDate> {
        self.legs.iter().filter_map(Leg::expiry).min()
    }
    pub fn latest_expiry(&self) -> Option<NaiveDate> {
        self.legs.iter().filter_map(Leg::expiry).max()
    }
    /// Distinct option strikes, sorted.
    pub fn strikes(&self) -> Vec<f64> {
        let mut s: Vec<f64> = self.legs.iter().filter_map(Leg::strike).collect();
        s.sort_by(f64::total_cmp);
        s.dedup_by(|a, b| (*a - *b).abs() < 1e-9);
        s
    }

    /// Mark-to-model value of the whole position ($).
    pub fn value_at(&self, spot: f64, date: NaiveDate, params: &MarketParams) -> f64 {
        self.legs.iter().map(|l| l.value(spot, date, params)).sum()
    }

    /// P&L ($) = value − entry cost.
    pub fn pnl_at(&self, spot: f64, date: NaiveDate, params: &MarketParams) -> f64 {
        self.value_at(spot, date, params) - self.entry_cost()
    }

    /// `n` evenly spaced (spot, P&L) points between `spot_lo` and `spot_hi`.
    pub fn pnl_curve(&self, spot_lo: f64, spot_hi: f64, n: usize, date: NaiveDate, params: &MarketParams) -> Vec<(f64, f64)> {
        linspace(spot_lo, spot_hi, n)
            .into_iter()
            .map(|s| (s, self.pnl_at(s, date, params)))
            .collect()
    }

    /// P&L curves for several dates (e.g. today, mid-way, expiry) on a common spot grid.
    pub fn pnl_curves(&self, spot_lo: f64, spot_hi: f64, n: usize, dates: &[NaiveDate], params: &MarketParams) -> Vec<(NaiveDate, Vec<(f64, f64)>)> {
        dates.iter().map(|d| (*d, self.pnl_curve(spot_lo, spot_hi, n, *d, params))).collect()
    }

    /// Date used for "at expiry" analysis: earliest option expiry, or `created` for stock-only.
    pub fn expiry_date(&self) -> NaiveDate {
        self.earliest_expiry().unwrap_or(self.created)
    }

    /// P&L ($) at the earliest expiry. Legs expiring then are worth intrinsic;
    /// later-dated legs are valued with the model (their IV + `vol_shift`).
    pub fn payoff_at_expiry(&self, spot: f64, params: &MarketParams) -> f64 {
        self.pnl_at(spot, self.expiry_date(), params)
    }

    /// Aggregated position Greeks (units in module docs).
    pub fn greeks(&self, spot: f64, date: NaiveDate, params: &MarketParams) -> Greeks {
        self.legs.iter().fold(Greeks::default(), |acc, l| acc + l.greeks(spot, date, params))
    }

    fn ref_spot(&self, params: &MarketParams) -> f64 {
        if params.spot > 0.0 {
            return params.spot;
        }
        let ks = self.strikes();
        if !ks.is_empty() {
            return ks.iter().sum::<f64>() / ks.len() as f64;
        }
        self.legs.iter().map(|l| l.entry_price).fold(0.0, f64::max).max(1.0)
    }

    /// Scan grid over (0, 5·spot] plus every strike (kinks), sorted.
    fn scan_grid(&self, params: &MarketParams, n: usize) -> Vec<f64> {
        let s = self.ref_spot(params);
        let mut g = linspace(s * 1e-6, 5.0 * s, n);
        g.extend(self.strikes().into_iter().filter(|k| *k > 0.0 && *k < 5.0 * s));
        g.push(s);
        g.sort_by(f64::total_cmp);
        g.dedup_by(|a, b| (*a - *b).abs() < 1e-12);
        g
    }

    /// Spots where P&L crosses zero on `date`, found by scanning (0, 5·spot] and bisection.
    pub fn breakevens(&self, date: NaiveDate, params: &MarketParams) -> Vec<f64> {
        let grid = self.scan_grid(params, 1200);
        let f = |s: f64| self.pnl_at(s, date, params);
        let vals: Vec<f64> = grid.iter().map(|s| f(*s)).collect();
        let mut out = Vec::new();
        for i in 1..grid.len() {
            let (a, b) = (vals[i - 1], vals[i]);
            if a == 0.0 && i == 1 {
                out.push(grid[0]);
            }
            if (a < 0.0 && b > 0.0) || (a > 0.0 && b < 0.0) {
                out.push(bisect(&f, grid[i - 1], grid[i], a));
            } else if b == 0.0 && a != 0.0 {
                out.push(grid[i]);
            }
        }
        out.dedup_by(|a, b| (*a - *b).abs() < 1e-6 * a.abs().max(1.0));
        out
    }

    /// Maximum profit / loss on `date` by numerical scan over (0, 5·spot].
    /// Unboundedness is flagged from the P&L slope at the upper boundary
    /// (|slope| > 0.5 share-equivalents).
    pub fn max_profit_loss(&self, date: NaiveDate, params: &MarketParams) -> MaxProfitLoss {
        let grid = self.scan_grid(params, 1200);
        let mut best = (f64::NEG_INFINITY, 0.0);
        let mut worst = (f64::INFINITY, 0.0);
        for &s in &grid {
            let p = self.pnl_at(s, date, params);
            if p > best.0 {
                best = (p, s);
            }
            if p < worst.0 {
                worst = (p, s);
            }
        }
        let hi = 5.0 * self.ref_spot(params);
        let slope = (self.pnl_at(hi, date, params) - self.pnl_at(0.98 * hi, date, params)) / (0.02 * hi);
        MaxProfitLoss {
            max_profit: best.0,
            max_profit_spot: best.1,
            max_loss: worst.0,
            max_loss_spot: worst.1,
            profit_unbounded: slope > 0.5,
            loss_unbounded: slope < -0.5,
        }
    }

    /// Lognormal terminal spot at standard-normal quantile `z`.
    fn spot_at_z(s0: f64, z: f64, t: f64, sigma: f64, drift: f64) -> f64 {
        s0 * ((drift - 0.5 * sigma * sigma) * t + sigma * t.sqrt() * z).exp()
    }

    /// Probability that P&L on `date` is > 0, with ln S_T ~ N(ln S0 + (drift − σ²/2)T, σ²T),
    /// S0 = `params.spot`, T = `date − created`. Use `drift = r − q` for risk-neutral,
    /// or an expected annual return for a "real-world" view. Profit regions are located
    /// in z-space and integrated exactly with the normal CDF.
    pub fn probability_of_profit(&self, date: NaiveDate, params: &MarketParams, sigma: f64, drift: f64) -> f64 {
        let s0 = self.ref_spot(params);
        let t = year_frac(self.created, date).max(0.0);
        if t <= 0.0 || sigma <= 0.0 {
            return if self.pnl_at(s0, date, params) > 0.0 { 1.0 } else { 0.0 };
        }
        let f = |z: f64| self.pnl_at(Self::spot_at_z(s0, z, t, sigma, drift), date, params);
        let zs = linspace(-8.0, 8.0, 1601);
        let vals: Vec<f64> = zs.iter().map(|z| f(*z)).collect();
        let mut prob = 0.0;
        let mut start: Option<f64> = if vals[0] > 0.0 { Some(f64::NEG_INFINITY) } else { None };
        for i in 1..zs.len() {
            let (a, b) = (vals[i - 1] > 0.0, vals[i] > 0.0);
            if a != b {
                let root = bisect_bool(&|z| f(z) > 0.0, zs[i - 1], zs[i], a);
                if b {
                    start = Some(root);
                } else if let Some(s) = start.take() {
                    prob += norm_cdf(root) - cdf_or_zero(s);
                }
            }
        }
        if let Some(s) = start {
            prob += 1.0 - cdf_or_zero(s);
        }
        prob.clamp(0.0, 1.0)
    }

    /// Expected P&L ($) on `date` under the same lognormal model as
    /// [`Self::probability_of_profit`] (Simpson integration over z ∈ [−8, 8]).
    pub fn expected_pnl(&self, date: NaiveDate, params: &MarketParams, sigma: f64, drift: f64) -> f64 {
        let s0 = self.ref_spot(params);
        let t = year_frac(self.created, date).max(0.0);
        if t <= 0.0 || sigma <= 0.0 {
            return self.pnl_at(s0, date, params);
        }
        let n = 1600; // even
        let (a, b) = (-8.0, 8.0);
        let h = (b - a) / n as f64;
        let mut sum = 0.0;
        for i in 0..=n {
            let z = a + h * i as f64;
            let w = if i == 0 || i == n { 1.0 } else if i % 2 == 1 { 4.0 } else { 2.0 };
            let pdf = (-0.5 * z * z).exp() / (2.0 * std::f64::consts::PI).sqrt();
            sum += w * pdf * self.pnl_at(Self::spot_at_z(s0, z, t, sigma, drift), date, params);
        }
        sum * h / 3.0
    }

    /// Contract counts: (long calls, short calls, long puts, short puts, net stock shares).
    fn exposure_counts(&self) -> (f64, f64, f64, f64, f64) {
        let (mut lc, mut sc, mut lp, mut sp, mut sh) = (0.0, 0.0, 0.0, 0.0, 0.0);
        for l in &self.legs {
            match (l.kind(), l.quantity > 0.0) {
                (Some(OptionKind::Call), true) => lc += l.quantity,
                (Some(OptionKind::Call), false) => sc -= l.quantity,
                (Some(OptionKind::Put), true) => lp += l.quantity,
                (Some(OptionKind::Put), false) => sp -= l.quantity,
                (None, _) => sh += l.quantity * l.multiplier,
            }
        }
        (lc, sc, lp, sp, sh)
    }

    /// Rough Reg-T style margin (buying power) requirement ($):
    /// * no uncovered shorts → same as the bounded case of [`Self::capital_required`];
    /// * otherwise: cost of long legs + Σ over uncovered short options of
    ///   `(premium + max(20%·S − OTM amount, 10%·S (calls) / 10%·K (puts))) × 100`.
    ///
    /// Short calls are covered by long calls and long stock (per 100 shares),
    /// short puts by long puts. Spread margin on covered portions is ignored.
    pub fn margin_required(&self, params: &MarketParams) -> f64 {
        let s = self.ref_spot(params);
        let (lc, sc, lp, sp, shares) = self.exposure_counts();
        let unc_calls = (sc - lc - shares.max(0.0) / OPTION_MULTIPLIER).max(0.0);
        let unc_puts = (sp - lp).max(0.0);
        if unc_calls <= 1e-9 && unc_puts <= 1e-9 {
            return self.bounded_capital(params);
        }
        let long_cost: f64 = self.legs.iter().filter(|l| l.quantity > 0.0).map(Leg::cost).sum();
        let mut req = long_cost;
        for l in self.legs.iter().filter(|l| l.quantity < 0.0) {
            if let LegInstrument::Option { kind, strike, .. } = l.instrument {
                let frac = match kind {
                    OptionKind::Call if sc > 0.0 => unc_calls / sc,
                    OptionKind::Put if sp > 0.0 => unc_puts / sp,
                    _ => 0.0,
                };
                let otm = match kind {
                    OptionKind::Call => (strike - s).max(0.0),
                    OptionKind::Put => (s - strike).max(0.0),
                };
                let floor = match kind {
                    OptionKind::Call => 0.10 * s,
                    OptionKind::Put => 0.10 * strike,
                };
                let per_share = l.entry_price + (0.20 * s - otm).max(floor);
                req += per_share * l.multiplier * (-l.quantity) * frac;
            }
        }
        req.max(0.0)
    }

    fn bounded_capital(&self, params: &MarketParams) -> f64 {
        let m = self.max_profit_loss(self.expiry_date(), params);
        (-m.max_loss).max(self.entry_cost()).max(0.0)
    }

    /// Rough capital tied up ($), cash-account style:
    /// * bounded loss (long options, debit/credit spreads, covered calls,
    ///   cash-secured puts, condors, collars…) → `max(max loss at earliest expiry, entry cost)`,
    ///   e.g. credit spread = width·100 − credit, CSP = strike·100 − credit;
    /// * unbounded loss (naked short calls) → [`Self::margin_required`] (Reg-T ≈ 20% rule).
    pub fn capital_required(&self, params: &MarketParams) -> f64 {
        let m = self.max_profit_loss(self.expiry_date(), params);
        if m.loss_unbounded {
            self.margin_required(params)
        } else {
            (-m.max_loss).max(self.entry_cost()).max(0.0)
        }
    }

    /// Everything a strategy card needs. Greeks are taken at `params.spot` on `created`;
    /// breakevens / max P&L / probabilities on `date` (typically the earliest expiry).
    pub fn summary(&self, date: NaiveDate, params: &MarketParams, sigma: f64, drift: f64) -> StrategySummary {
        let s = self.ref_spot(params);
        let greeks = self.greeks(s, self.created, params);
        let capital = self.capital_required(params);
        StrategySummary {
            entry_cost: self.entry_cost(),
            net_premium: self.net_premium(),
            capital_required: capital,
            margin_required: self.margin_required(params),
            greeks,
            breakevens: self.breakevens(date, params),
            max_profit_loss: self.max_profit_loss(date, params),
            probability_of_profit: self.probability_of_profit(date, params, sigma, drift),
            expected_pnl: self.expected_pnl(date, params, sigma, drift),
            leverage: if capital > 0.0 { greeks.delta * s / capital } else { 0.0 },
            evaluation_date: date,
        }
    }
}

fn cdf_or_zero(z: f64) -> f64 {
    if z == f64::NEG_INFINITY {
        0.0
    } else {
        norm_cdf(z)
    }
}

/// Evenly spaced points (inclusive). `n < 2` returns `[lo]`.
pub(crate) fn linspace(lo: f64, hi: f64, n: usize) -> Vec<f64> {
    if n < 2 {
        return vec![lo];
    }
    let step = (hi - lo) / (n - 1) as f64;
    (0..n).map(|i| lo + step * i as f64).collect()
}

/// Bisection for a sign change of `f` in [a, b]; `fa = f(a)`.
fn bisect(f: &impl Fn(f64) -> f64, mut a: f64, mut b: f64, mut fa: f64) -> f64 {
    for _ in 0..80 {
        let m = 0.5 * (a + b);
        let fm = f(m);
        if fm == 0.0 {
            return m;
        }
        if (fm > 0.0) == (fa > 0.0) {
            a = m;
            fa = fm;
        } else {
            b = m;
        }
        if (b - a).abs() < 1e-10 * b.abs().max(1.0) {
            break;
        }
    }
    0.5 * (a + b)
}

/// Bisection on a boolean predicate whose value at `a` is `pa`.
fn bisect_bool(p: &impl Fn(f64) -> bool, mut a: f64, mut b: f64, pa: bool) -> f64 {
    for _ in 0..60 {
        let m = 0.5 * (a + b);
        if p(m) == pa {
            a = m;
        } else {
            b = m;
        }
        if (b - a).abs() < 1e-12 {
            break;
        }
    }
    0.5 * (a + b)
}

// ---------------------------------------------------------------------------
// Expiry & strike helpers
// ---------------------------------------------------------------------------

/// Third Friday of a month (standard monthly option expiry).
pub fn third_friday(year: i32, month: u32) -> NaiveDate {
    NaiveDate::from_weekday_of_month_opt(year, month, Weekday::Fri, 3)
        .expect("every month has a third Friday")
}

/// Standard monthly expiry (3rd Friday) closest to `as_of + dte` days, strictly after `as_of`.
pub fn monthly_expiry_near(as_of: NaiveDate, dte: i64) -> NaiveDate {
    let target = as_of + Duration::days(dte.max(1));
    let (mut y, mut m) = (as_of.year(), as_of.month());
    let mut best: Option<NaiveDate> = None;
    loop {
        let e = third_friday(y, m);
        if e > as_of {
            let better = best.is_none_or(|b| (e - target).num_days().abs() < (b - target).num_days().abs());
            if better {
                best = Some(e);
            }
            if e > target {
                break;
            }
        }
        m += 1;
        if m > 12 {
            m = 1;
            y += 1;
        }
    }
    best.expect("loop always finds an expiry")
}

/// Next monthly expiry strictly after `date`.
pub fn next_monthly_expiry(date: NaiveDate) -> NaiveDate {
    let e = third_friday(date.year(), date.month());
    if e > date {
        return e;
    }
    let (y, m) = if date.month() == 12 { (date.year() + 1, 1) } else { (date.year(), date.month() + 1) };
    third_friday(y, m)
}

/// Typical listed strike increment for an underlying price level
/// (0.5 below $25, 1 below $250, 5 below $10 000 (SPX), else 25 (NDX)).
pub fn strike_step(spot: f64) -> f64 {
    if spot < 25.0 {
        0.5
    } else if spot < 250.0 {
        1.0
    } else if spot < 10_000.0 {
        5.0
    } else {
        25.0
    }
}

/// Round a theoretical strike to the listed increment for `spot`.
pub fn round_strike(strike: f64, spot: f64) -> f64 {
    let step = strike_step(spot);
    ((strike / step).round() * step).max(step)
}

// ---------------------------------------------------------------------------
// Templates
// ---------------------------------------------------------------------------

/// Market outlook a template expresses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Outlook {
    StronglyBullish,
    Bullish,
    MildlyBullish,
    Neutral,
    Volatile,
    Bearish,
    Hedge,
}

impl Outlook {
    pub fn label_zh(self) -> &'static str {
        match self {
            Outlook::StronglyBullish => "強烈看多",
            Outlook::Bullish => "看多",
            Outlook::MildlyBullish => "溫和看多",
            Outlook::Neutral => "中性盤整",
            Outlook::Volatile => "看大波動",
            Outlook::Bearish => "看空",
            Outlook::Hedge => "避險",
        }
    }
    pub fn label_en(self) -> &'static str {
        match self {
            Outlook::StronglyBullish => "Strongly bullish",
            Outlook::Bullish => "Bullish",
            Outlook::MildlyBullish => "Mildly bullish",
            Outlook::Neutral => "Neutral",
            Outlook::Volatile => "Volatile",
            Outlook::Bearish => "Bearish",
            Outlook::Hedge => "Hedge",
        }
    }
}

/// Built-in strategy templates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum StrategyTemplate {
    LongCall,
    LeapsStockReplacement,
    PoorMansCoveredCall,
    BullCallSpread,
    BullPutSpread,
    CoveredCall,
    CashSecuredPut,
    ProtectivePut,
    Collar,
    SyntheticLong,
    RiskReversal,
    LongCallDiagonal,
    CallCalendar,
    IronCondor,
    LongStraddle,
    LongPut,
}

/// Every template, in display order (most relevant to long-term leveraged bulls first).
pub const ALL_TEMPLATES: [StrategyTemplate; 16] = [
    StrategyTemplate::LeapsStockReplacement,
    StrategyTemplate::PoorMansCoveredCall,
    StrategyTemplate::LongCall,
    StrategyTemplate::BullCallSpread,
    StrategyTemplate::SyntheticLong,
    StrategyTemplate::RiskReversal,
    StrategyTemplate::BullPutSpread,
    StrategyTemplate::LongCallDiagonal,
    StrategyTemplate::CoveredCall,
    StrategyTemplate::CashSecuredPut,
    StrategyTemplate::ProtectivePut,
    StrategyTemplate::Collar,
    StrategyTemplate::CallCalendar,
    StrategyTemplate::IronCondor,
    StrategyTemplate::LongStraddle,
    StrategyTemplate::LongPut,
];

/// Template knobs. Deltas are **absolute values** (0.30 means a 0.30-delta call
/// or a −0.30-delta put). Single-expiry templates use `long_dte` for every leg;
/// two-expiry templates (PMCC, diagonal, calendar) use `long_dte` for the back
/// month and `short_dte` for the front month.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct TemplateParams {
    /// |Δ| of the main long option leg.
    pub long_delta: f64,
    /// |Δ| of the main short option leg (or of the calendar strike).
    pub short_delta: f64,
    /// Calendar days to the (back-month) expiry.
    pub long_dte: i64,
    /// Calendar days to the front-month expiry.
    pub short_dte: i64,
    /// Spread width as a fraction of the anchor strike (verticals, condor wings).
    /// `<= 0` makes a bull call spread pick its short strike by `short_delta` instead.
    pub width_pct: f64,
    /// Number of strategy units (contracts per leg; stock legs get 100 × qty shares).
    pub qty: f64,
    /// Risk-free rate used when deltas/IVs must be computed.
    pub r: f64,
    /// Dividend yield used when deltas/IVs must be computed.
    pub q: f64,
}

impl Default for TemplateParams {
    fn default() -> Self {
        TemplateParams {
            long_delta: 0.50,
            short_delta: 0.30,
            long_dte: 120,
            short_dte: 30,
            width_pct: 0.05,
            qty: 1.0,
            r: 0.04,
            q: 0.01,
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum StrikeRule {
    /// Absolute delta target.
    Delta(f64),
    /// Strike nearest spot.
    Atm,
    /// `factor × strike of leg i`, strictly above (`true`) or below (`false`) it.
    Relative(usize, f64, bool),
    /// Same strike as leg i (nearest available).
    SameAs(usize),
}

#[derive(Debug, Clone, Copy)]
struct LegSpec {
    /// `None` = stock.
    kind: Option<OptionKind>,
    /// Units per strategy unit, signed (+1/−1 contracts, ±100 shares).
    sign: f64,
    /// Back-month (`true`) or front-month expiry.
    back: bool,
    strike: StrikeRule,
}

impl LegSpec {
    fn opt(kind: OptionKind, sign: f64, back: bool, strike: StrikeRule) -> Self {
        LegSpec { kind: Some(kind), sign, back, strike }
    }
    fn stock() -> Self {
        LegSpec { kind: None, sign: 100.0, back: true, strike: StrikeRule::Atm }
    }
}

impl StrategyTemplate {
    /// Stable identifier (snake case) for configs / CLI arguments.
    pub fn key(self) -> &'static str {
        use StrategyTemplate::*;
        match self {
            LongCall => "long_call",
            LeapsStockReplacement => "leaps",
            PoorMansCoveredCall => "pmcc",
            BullCallSpread => "bull_call_spread",
            BullPutSpread => "bull_put_spread",
            CoveredCall => "covered_call",
            CashSecuredPut => "cash_secured_put",
            ProtectivePut => "protective_put",
            Collar => "collar",
            SyntheticLong => "synthetic_long",
            RiskReversal => "risk_reversal",
            LongCallDiagonal => "call_diagonal",
            CallCalendar => "call_calendar",
            IronCondor => "iron_condor",
            LongStraddle => "long_straddle",
            LongPut => "long_put",
        }
    }

    /// Parse from [`Self::key`] (also accepts the English name, case-insensitive).
    pub fn from_key(s: &str) -> Option<Self> {
        let s = s.trim().to_ascii_lowercase();
        ALL_TEMPLATES
            .iter()
            .copied()
            .find(|t| t.key() == s || t.name_en().to_ascii_lowercase() == s)
    }

    pub fn name_zh(self) -> &'static str {
        use StrategyTemplate::*;
        match self {
            LongCall => "買入買權",
            LeapsStockReplacement => "LEAPS 股票替代",
            PoorMansCoveredCall => "窮人備兌買權 (PMCC)",
            BullCallSpread => "多頭買權價差",
            BullPutSpread => "多頭賣權價差",
            CoveredCall => "備兌買權",
            CashSecuredPut => "現金擔保賣權",
            ProtectivePut => "保護性賣權",
            Collar => "領口策略",
            SyntheticLong => "合成多頭",
            RiskReversal => "風險逆轉",
            LongCallDiagonal => "買權對角價差",
            CallCalendar => "買權日曆價差",
            IronCondor => "鐵兀鷹",
            LongStraddle => "買入跨式",
            LongPut => "買入賣權（避險）",
        }
    }

    pub fn name_en(self) -> &'static str {
        use StrategyTemplate::*;
        match self {
            LongCall => "Long Call",
            LeapsStockReplacement => "LEAPS Stock Replacement",
            PoorMansCoveredCall => "Poor Man's Covered Call",
            BullCallSpread => "Bull Call Spread",
            BullPutSpread => "Bull Put Spread",
            CoveredCall => "Covered Call",
            CashSecuredPut => "Cash-Secured Put",
            ProtectivePut => "Protective Put",
            Collar => "Collar",
            SyntheticLong => "Synthetic Long",
            RiskReversal => "Risk Reversal",
            LongCallDiagonal => "Long Call Diagonal",
            CallCalendar => "Call Calendar",
            IronCondor => "Iron Condor",
            LongStraddle => "Long Straddle",
            LongPut => "Long Put",
        }
    }

    pub fn outlook(self) -> Outlook {
        use StrategyTemplate::*;
        match self {
            LongCall | LeapsStockReplacement | SyntheticLong => Outlook::StronglyBullish,
            BullCallSpread | RiskReversal => Outlook::Bullish,
            PoorMansCoveredCall | BullPutSpread | CoveredCall | CashSecuredPut | LongCallDiagonal => Outlook::MildlyBullish,
            ProtectivePut | Collar => Outlook::Hedge,
            CallCalendar | IronCondor => Outlook::Neutral,
            LongStraddle => Outlook::Volatile,
            LongPut => Outlook::Bearish,
        }
    }

    /// 1–5 rating of how well the template fits a long-term leveraged-bull plan.
    pub fn bull_leverage_fit(self) -> u8 {
        use StrategyTemplate::*;
        match self {
            LeapsStockReplacement => 5,
            PoorMansCoveredCall | SyntheticLong => 4,
            LongCall | BullCallSpread | RiskReversal => 3,
            LongCallDiagonal | BullPutSpread | ProtectivePut | LongPut => 2,
            CashSecuredPut | CoveredCall | Collar | CallCalendar => 1,
            IronCondor | LongStraddle => 1,
        }
    }

    /// Substantive Traditional Chinese description: mechanics, when to use,
    /// max profit/loss, risks, and suitability for long-term leveraged bulls.
    pub fn description_zh(self) -> &'static str {
        use StrategyTemplate::*;
        match self {
            LongCall => "【機制】支付權利金買進一口買權，取得在到期前以履約價買進 100 股標的的權利。\
【何時用】預期標的在到期前會明顯上漲，且隱含波動率（IV）不高時較划算。\
【損益】最大虧損＝已付權利金；獲利理論上無上限；到期損益兩平＝履約價＋權利金。\
【風險】時間價值每日流失（Theta），漲得不夠多或太慢，即使方向正確也可能虧損；IV 下降（IV crush）會侵蝕價值；價外買權歸零機率高。\
【對長期槓桿做多】短天期價平買權槓桿最大但耗損最快，適合戰術性加碼；長期持有建議改用較深價內、較長天期的 LEAPS。",
            LeapsStockReplacement => "【機制】買進 12–24 個月到期、Delta 約 0.75–0.85 的深價內買權，用約 15–30% 的資金取得接近 0.8 倍持股的上漲曝險，剩餘資金可放短債或貨幣基金。權利金大部分是內含價值，外在價值（時間價值）占比低，每日 Theta 耗損相對小。\
【何時用】長期看多大盤或科技指數（QQQ、SPY、SPX、NDX），想以較少資金取得 2–4 倍的槓桿；IV 位於相對低檔時建倉最划算。\
【損益】最大虧損＝已付權利金（金額遠小於持有等量股票，但以百分比計可能是 100%）；獲利不封頂；損益兩平＝履約價＋權利金（因深價內，通常只需小幅上漲）。\
【操作要點】剩餘約 6–9 個月、或 Delta 漂移到 0.90 以上 / 0.65 以下時，「滾倉（roll）」到更遠天期並重新回到目標 Delta，避免進入 Theta 加速區，並可在大漲後鎖定部分獲利。\
【風險】大跌時槓桿放大虧損比例；不領股息；買賣價差與流動性（選擇成交量大的月選或一月 LEAPS）；SPX/NDX 為歐式、現金交割、無提前履約風險但單口名目金額大，QQQ/SPY 為美式。\
【對長期槓桿做多】這是最貼近您目標的核心結構；建議以「總曝險（Delta×標的價×100×口數）÷ 帳戶淨值」控制整體槓桿，而非只看權利金。",
            PoorMansCoveredCall => "【機制】買進一張長天期深價內 LEAPS 買權（Delta≈0.80）作為股票替代，再每月賣出 30–45 天、Delta≈0.25–0.30 的價外買權收取權利金，用短腳權利金抵銷 LEAPS 的時間價值耗損。\
【何時用】溫和看多、盤整偏多，或 IV 偏高使賣出買權的收益較佳時。\
【損益】最大虧損約為淨支出（LEAPS 成本減去收到的權利金）；最大獲利有限：標的大漲超過短腳履約價時，短腳虧損會抵銷 LEAPS 的漲幅。建倉時應確認「兩腳履約價寬度 > 淨支出」，否則大漲時反而可能虧損。\
【風險】強勢牛市中上檔被封頂、需要向上向後轉倉（roll up and out）；美式短腳（QQQ/SPY）在除息前可能被提前指派；短腳收入在急跌時無法彌補 LEAPS 的損失。\
【對長期槓桿做多】可降低持有成本、讓部位在盤整期也有收益，但會犧牲強勁行情的爆發力；強烈看多時可減少賣出口數、選擇更價外（Delta 0.15–0.20）的短腳，或只在 IV 高檔時才賣。",
            BullCallSpread => "【機制】買進較低履約價買權，同時賣出同到期、較高履約價買權。\
【何時用】看多但預期漲幅有限，或 IV 偏高時用賣出腳降低成本與 Vega 曝險。\
【損益】最大虧損＝淨支出；最大獲利＝履約價寬度×100 − 淨支出；損益兩平＝低履約價＋每股淨支出。\
【風險】上檔封頂，漲過高履約價後不再增加獲利；到期前價值變化較慢，通常需接近到期才拿得到大部分最大獲利；提前指派風險（美式）。\
【對長期槓桿做多】成本低、風險明確，適合「預期 6–12 個月漲 10–15%」這類有目標價的看法；長天期垂直價差也可作為 LEAPS 的低成本替代，但在長期牛市中會錯失超額漲幅。",
            BullPutSpread => "【機制】賣出較高履約價賣權，同時買進較低履約價賣權作保護，收取淨權利金（信用價差）。\
【何時用】中性到看多、IV 偏高、認為標的在到期前不會跌破賣出履約價時。\
【損益】最大獲利＝收到的權利金；最大虧損＝履約價寬度×100 − 權利金；損益兩平＝高履約價 − 每股權利金。Theta 為正，時間站在你這邊。\
【風險】勝率高但盈虧比差，急跌時一次虧損可抵銷多次收入；接近到期 Gamma 風險高；美式可能被提前指派。\
【對長期槓桿做多】本身不提供上漲槓桿，屬於收租型策略；可作為 LEAPS 部位的輔助收入，或在回檔時「以更低價位表達看多」。",
            CoveredCall => "【機制】持有 100 股標的，同時賣出一口價外買權（常見 Delta 0.20–0.30、30–45 天）收取權利金。\
【何時用】溫和看多或盤整，並願意在履約價賣出持股時。\
【損益】最大獲利＝(履約價 − 股票成本)×100＋權利金；最大虧損＝股票跌到 0 的損失扣除權利金（與單純持股相近）；損益兩平＝股票成本 − 每股權利金。\
【風險】大漲時獲利被封頂（在 QQQ 這類長期強勢標的上，長期績效常落後單純持有）；下跌保護只有權利金那麼多；被指派後需重新買回。\
【對長期槓桿做多】沒有槓桿，且會降低上漲參與度，與您的目標方向相反；適合作為比較基準，或在想降低波動、增加現金流時使用。",
            CashSecuredPut => "【機制】賣出價外賣權，並預留足以履約買股的現金（履約價×100）。到期時標的高於履約價就賺取全部權利金；跌破則以「履約價 − 權利金」的有效成本買進股票。\
【何時用】想在較低價位買進指數 ETF、IV 偏高（例如回檔後）時。\
【損益】最大獲利＝權利金；最大虧損＝(履約價 − 每股權利金)×100（標的歸零）；損益兩平＝履約價 − 每股權利金。\
【風險】大跌時虧損與持股相當，上漲時卻只賺權利金；尾端風險（崩盤）不對稱。\
【對長期槓桿做多】以現金擔保時曝險低於 1 倍、沒有槓桿；若改在保證金帳戶以 Reg-T（約 20%）賣出則變成高槓桿做多，尾端風險巨大，必須嚴格控制口數。",
            ProtectivePut => "【機制】持有股票的同時買進價外賣權，等於替持股買保險。\
【何時用】擔心短期事件風險（財報季、FOMC、選舉），或已有大幅未實現獲利想鎖住下檔。\
【損益】最大虧損＝(股票成本 − 賣權履約價)×股數＋賣權權利金；上漲獲利不封頂，但需扣除保險費。\
【風險】保險費持續耗損，長期滾動買保護會明顯拖累報酬，IV 高時尤其昂貴。\
【對長期槓桿做多】可與 LEAPS 或融資持股搭配，限制槓桿部位的尾端風險，避免在崩盤中被迫出場；建議只在 IV 低檔或關鍵事件前選擇性使用。",
            Collar => "【機制】持股＋買進價外賣權＋賣出價外買權，用賣出買權的收入支付賣權保險費（常可做到接近零成本）。\
【何時用】想保住既有獲利、降低波動，或在不確定期間暫時「鎖倉」。\
【損益】損益被限制在兩個履約價之間：最大虧損約＝(股票成本 − 賣權履約價)×股數＋淨權利金；最大獲利約＝(買權履約價 − 股票成本)×股數 − 淨權利金。\
【風險】上漲被封頂、短買權可能被提前指派、到期轉倉成本。\
【對長期槓桿做多】會降低上漲參與，不符合積極做多目標；可作為大幅獲利後的階段性防守工具。",
            SyntheticLong => "【機制】買進價平買權、同時賣出同履約價同到期的賣權，損益幾乎等同持有 100 股（Delta≈1.0），淨權利金接近零（差額反映利率與股息，即期貨的持有成本）。\
【何時用】強烈看多、想在保證金帳戶以極少現金取得 1:1 的股票曝險。\
【損益】獲利不封頂；最大虧損（標的歸零）≈(履約價＋每股淨支出)×100，與持有 100 股相當；損益兩平≈履約價＋每股淨支出。\
【風險】短賣權需要保證金，大跌時虧損與持股相同但投入資金少，槓桿極高，可能被追繳保證金（margin call）；美式短賣權可能被提前指派；隱含融資利率可能高於或低於券商利率。\
【對長期槓桿做多】資金效率最高的做多結構之一，但下行毫無保護；請用「若標的跌 30–50% 我能承受多少」來決定口數，而不是用保證金上限。",
            RiskReversal => "【機制】賣出價外賣權，用收到的權利金買進價外買權，常可做到接近零成本。\
【何時用】看多，且市場偏斜（put skew）使賣權相對昂貴時，可利用偏斜換取上檔曝險；回檔後 IV 升高時最划算。\
【損益】兩個履約價之間損益平坦；漲破買權履約價後獲利不封頂；跌破賣權履約價後虧損如同持股（最大虧損≈賣權履約價×100 減淨收入）。\
【風險】下跌時承擔類似持股的虧損，卻在小漲時沒有獲利；需要保證金、可能被追繳；短賣權可能被提前指派。\
【對長期槓桿做多】比合成多頭「更便宜」的看多結構，適合在恐慌回檔時布局，但必須嚴格控制口數並預先規劃跌破賣權履約價時的處理方式。",
            LongCallDiagonal => "【機制】買進較長天期、較低（或價平）履約價的買權，賣出較短天期、較高履約價的買權。與 PMCC 類似，但長腳天期較短（3–6 個月）、Delta 較低，對價格與波動率變化更敏感。\
【何時用】溫和看多，且近月 IV 高於遠月（期限結構倒掛）或預期短期盤整後再上漲時。\
【損益】最大虧損約為淨支出；最大獲利出現在短腳到期時標的接近短腳履約價。\
【風險】大漲（短腳被軋）或大跌（長腳貶值）都可能虧損；需持續管理短腳轉倉；IV 下降不利長腳。\
【對長期槓桿做多】屬於戰術型收租結構，可補充長期 LEAPS 部位，但不建議作為主要的長期做多工具。",
            CallCalendar => "【機制】賣出近月買權、買進同履約價的遠月買權，賺取近月時間價值流失較快的差異；整體 Vega 為正（IV 上升有利）。\
【何時用】預期短期盤整在履約價附近、IV 處於低檔，或近月 IV 明顯高於遠月時。\
【損益】最大虧損約為淨支出；最大獲利出現在近月到期時標的剛好位於履約價附近（金額取決於當時遠月的 IV）。\
【風險】標的大幅偏離履約價時兩腳價值趨同而虧損；遠月 IV 下降；事件（財報）導致近月 IV 崩跌的時機風險。\
【對長期槓桿做多】方向性弱，主要表達對時間價值與波動率的看法，不是主力做多工具；可用較高履約價做成偏多的日曆價差。",
            IronCondor => "【機制】同時賣出價外賣權價差與價外買權價差收取權利金，押注標的在到期前維持在兩個短腳履約價之間。\
【何時用】預期盤整、IV 偏高（IV Rank 高）時。\
【損益】最大獲利＝收到的總權利金；最大虧損＝較寬一側的價差寬度×100 − 權利金；兩個損益兩平＝短賣權履約價 − 權利金、短買權履約價＋權利金。\
【風險】勝率高但盈虧比差，大行情（尤其急跌）造成數倍於收入的虧損；需要積極調整；接近到期 Gamma 風險大。\
【對長期槓桿做多】與方向性做多目標無關，僅作為中性市況的對照組或收入策略；在長期上漲的指數上，買權側常被突破。",
            LongStraddle => "【機制】同時買進同履約價（通常價平）同到期的買權與賣權，押注大幅波動而不論方向。\
【何時用】重大事件前且 IV 尚未充分反映，或預期實現波動率將高於隱含波動率時。\
【損益】最大虧損＝總權利金（到期時標的剛好在履約價）；兩個損益兩平＝履約價 ± 每股總權利金；上漲獲利無上限，下跌獲利最多到履約價。\
【風險】Theta 耗損最大；事件後 IV 崩跌（IV crush）可能讓方向對了也虧錢。\
【對長期槓桿做多】不符合方向性做多目標；價平跨式價格本身就是市場隱含的「預期波動（Expected Move）」，可作為設定 LEAPS 目標價與滾倉時機的參考。",
            LongPut => "【機制】買進賣權，標的下跌時獲利，可作為看空部位或多頭組合的保險。\
【何時用】看空，或作為整體多頭組合（LEAPS、持股）的尾端避險；VIX／IV 低檔時保險最便宜。\
【損益】最大虧損＝權利金；最大獲利＝(履約價 − 每股權利金)×100（標的歸零）；損益兩平＝履約價 − 每股權利金。\
【風險】時間價值耗損，長期持續避險成本可觀；價外賣權多數情況下歸零。\
【對長期槓桿做多】建議以小比例（例如每年組合價值的 0.5–2%）購買較價外、較長天期（3–6 個月）的賣權作為災難保險，降低槓桿部位在崩盤中被掃出場的風險。",
        }
    }

    /// Recommended default knobs for this template.
    pub fn default_params(self) -> TemplateParams {
        use StrategyTemplate::*;
        let d = TemplateParams::default();
        match self {
            LongCall => TemplateParams { long_delta: 0.50, long_dte: 120, ..d },
            LeapsStockReplacement => TemplateParams { long_delta: 0.80, long_dte: 540, ..d },
            PoorMansCoveredCall => TemplateParams { long_delta: 0.80, long_dte: 540, short_delta: 0.30, short_dte: 38, ..d },
            BullCallSpread => TemplateParams { long_delta: 0.60, long_dte: 120, width_pct: 0.07, short_delta: 0.30, ..d },
            BullPutSpread => TemplateParams { short_delta: 0.30, long_dte: 45, width_pct: 0.03, ..d },
            CoveredCall => TemplateParams { short_delta: 0.30, long_dte: 35, ..d },
            CashSecuredPut => TemplateParams { short_delta: 0.30, long_dte: 45, ..d },
            ProtectivePut => TemplateParams { long_delta: 0.30, long_dte: 180, ..d },
            Collar => TemplateParams { long_delta: 0.25, short_delta: 0.25, long_dte: 90, ..d },
            SyntheticLong => TemplateParams { long_delta: 0.50, short_delta: 0.50, long_dte: 365, ..d },
            RiskReversal => TemplateParams { long_delta: 0.30, short_delta: 0.25, long_dte: 180, ..d },
            LongCallDiagonal => TemplateParams { long_delta: 0.65, long_dte: 120, short_delta: 0.30, short_dte: 30, ..d },
            CallCalendar => TemplateParams { short_delta: 0.50, long_dte: 75, short_dte: 30, ..d },
            IronCondor => TemplateParams { short_delta: 0.16, long_dte: 45, width_pct: 0.03, ..d },
            LongStraddle => TemplateParams { long_dte: 45, ..d },
            LongPut => TemplateParams { long_delta: 0.30, long_dte: 90, ..d },
        }
    }

    /// True for templates with two different expiries.
    pub fn is_multi_expiry(self) -> bool {
        matches!(self, StrategyTemplate::PoorMansCoveredCall | StrategyTemplate::LongCallDiagonal | StrategyTemplate::CallCalendar)
    }

    fn specs(self, p: &TemplateParams) -> Vec<LegSpec> {
        use OptionKind::{Call, Put};
        use StrategyTemplate::*;
        use StrikeRule::*;
        let w = p.width_pct;
        match self {
            LongCall | LeapsStockReplacement => vec![LegSpec::opt(Call, 1.0, true, Delta(p.long_delta))],
            PoorMansCoveredCall | LongCallDiagonal => vec![
                LegSpec::opt(Call, 1.0, true, Delta(p.long_delta)),
                LegSpec::opt(Call, -1.0, false, Delta(p.short_delta)),
            ],
            BullCallSpread => vec![
                LegSpec::opt(Call, 1.0, true, Delta(p.long_delta)),
                LegSpec::opt(Call, -1.0, true, if w > 0.0 { Relative(0, 1.0 + w, true) } else { Delta(p.short_delta) }),
            ],
            BullPutSpread => vec![
                LegSpec::opt(Put, -1.0, true, Delta(p.short_delta)),
                LegSpec::opt(Put, 1.0, true, Relative(0, 1.0 - w.max(0.005), false)),
            ],
            CoveredCall => vec![LegSpec::stock(), LegSpec::opt(Call, -1.0, true, Delta(p.short_delta))],
            CashSecuredPut => vec![LegSpec::opt(Put, -1.0, true, Delta(p.short_delta))],
            ProtectivePut => vec![LegSpec::stock(), LegSpec::opt(Put, 1.0, true, Delta(p.long_delta))],
            Collar => vec![
                LegSpec::stock(),
                LegSpec::opt(Put, 1.0, true, Delta(p.long_delta)),
                LegSpec::opt(Call, -1.0, true, Delta(p.short_delta)),
            ],
            SyntheticLong => vec![LegSpec::opt(Call, 1.0, true, Atm), LegSpec::opt(Put, -1.0, true, SameAs(0))],
            RiskReversal => vec![
                LegSpec::opt(Put, -1.0, true, Delta(p.short_delta)),
                LegSpec::opt(Call, 1.0, true, Delta(p.long_delta)),
            ],
            CallCalendar => vec![
                LegSpec::opt(Call, -1.0, false, Delta(p.short_delta)),
                LegSpec::opt(Call, 1.0, true, SameAs(0)),
            ],
            IronCondor => vec![
                LegSpec::opt(Put, -1.0, true, Delta(p.short_delta)),
                LegSpec::opt(Put, 1.0, true, Relative(0, 1.0 - w.max(0.005), false)),
                LegSpec::opt(Call, -1.0, true, Delta(p.short_delta)),
                LegSpec::opt(Call, 1.0, true, Relative(2, 1.0 + w.max(0.005), true)),
            ],
            LongStraddle => vec![LegSpec::opt(Call, 1.0, true, Atm), LegSpec::opt(Put, 1.0, true, SameAs(0))],
            LongPut => vec![LegSpec::opt(Put, 1.0, true, Delta(p.long_delta))],
        }
    }

    fn strategy_name(self) -> String {
        format!("{} ({})", self.name_zh(), self.name_en())
    }
}

/// Signed delta target for a kind from an absolute delta.
fn signed_delta(kind: OptionKind, abs_delta: f64) -> f64 {
    abs_delta.abs() * kind.sign()
}

// ---------------------------------------------------------------------------
// Builders
// ---------------------------------------------------------------------------

/// Sanity range for a provider IV (decimal).
pub(crate) fn sane_iv(iv: Option<f64>) -> Option<f64> {
    iv.filter(|v| v.is_finite() && *v > 0.01 && *v < 3.0)
}

struct ChainPicker<'a> {
    chain: &'a OptionChain,
    as_of: NaiveDate,
    r: f64,
    q: f64,
}

impl<'a> ChainPicker<'a> {
    fn t(&self, e: NaiveDate) -> f64 {
        year_frac(self.as_of, e).max(1.0 / 365.0)
    }

    fn iv_of(&self, q: &OptionQuote) -> Option<f64> {
        sane_iv(q.iv).or_else(|| {
            implied_vol(q.kind, q.mid(), self.chain.spot, q.strike, self.t(q.expiry), self.r, self.q).filter(|v| *v > 0.01 && *v < 3.0)
        })
    }

    fn fallback_iv(&self, e: NaiveDate) -> f64 {
        if let Some(v) = sane_iv(self.chain.atm_iv(e)) {
            return v;
        }
        let k = self.chain.atm_strike(e);
        let near: Vec<f64> = self
            .chain
            .quotes
            .iter()
            .filter(|q| q.expiry == e && k.is_some_and(|k| (q.strike - k).abs() / k < 0.05))
            .filter_map(|q| self.iv_of(q))
            .collect();
        if near.is_empty() {
            0.20
        } else {
            near.iter().sum::<f64>() / near.len() as f64
        }
    }

    fn delta_of(&self, q: &OptionQuote, fallback: f64) -> f64 {
        if let Some(d) = q.delta.filter(|d| d.is_finite() && d.abs() <= 1.0 && *d != 0.0) {
            return d;
        }
        let iv = self.iv_of(q).unwrap_or(fallback);
        bs_greeks(q.kind, self.chain.spot, q.strike, self.t(q.expiry), self.r, self.q, iv).delta
    }

    fn candidates(&self, e: NaiveDate, kind: OptionKind) -> Vec<&'a OptionQuote> {
        self.chain.slice(e, kind).into_iter().filter(|q| q.mid() > 0.0).collect()
    }

    fn by_delta(&self, e: NaiveDate, kind: OptionKind, abs_delta: f64) -> Result<&'a OptionQuote> {
        let target = signed_delta(kind, abs_delta);
        let fb = self.fallback_iv(e);
        self.candidates(e, kind)
            .into_iter()
            .min_by(|a, b| (self.delta_of(a, fb) - target).abs().total_cmp(&(self.delta_of(b, fb) - target).abs()))
            .ok_or_else(|| anyhow!("no priced {kind:?} quotes for expiry {e}"))
    }

    fn by_strike(&self, e: NaiveDate, kind: OptionKind, target: f64, filter: impl Fn(f64) -> bool) -> Result<&'a OptionQuote> {
        self.candidates(e, kind)
            .into_iter()
            .filter(|q| filter(q.strike))
            .min_by(|a, b| (a.strike - target).abs().total_cmp(&(b.strike - target).abs()))
            .ok_or_else(|| anyhow!("no priced {kind:?} quote near strike {target:.2} for {e}"))
    }

    fn leg(&self, q: &OptionQuote, qty: f64) -> Leg {
        let iv = self.iv_of(q).unwrap_or_else(|| self.fallback_iv(q.expiry));
        Leg::option(q.kind, q.strike, q.expiry, qty, q.mid(), iv)
    }
}

/// Front / back expiries for a template from a chain.
fn chain_expiries(chain: &OptionChain, t: StrategyTemplate, p: &TemplateParams) -> Result<(NaiveDate, NaiveDate)> {
    let back = chain.expiry_near_dte(p.long_dte).ok_or_else(|| anyhow!("chain has no future expiries"))?;
    if !t.is_multi_expiry() {
        return Ok((back, back));
    }
    let front = chain.expiry_near_dte(p.short_dte).ok_or_else(|| anyhow!("chain has no future expiries"))?;
    if back > front {
        return Ok((front, back));
    }
    // Back month must be strictly later than the front month.
    let later = chain.expiries().into_iter().find(|e| *e > front);
    match later {
        Some(b) => Ok((front, b)),
        None => {
            let earlier = chain.expiries().into_iter().filter(|e| *e > chain.as_of_date() && *e < front).max();
            earlier.map(|f| (f, front)).ok_or_else(|| anyhow!("{} needs two distinct expiries", t.name_en()))
        }
    }
}

/// Build a template from a real chain: expiries via `expiry_near_dte`, strikes by delta
/// (provider deltas when present, otherwise BS deltas from quote IV), fills at mid.
pub fn build_from_chain(template: StrategyTemplate, chain: &OptionChain, params: &TemplateParams) -> Result<Strategy> {
    if chain.spot <= 0.0 {
        bail!("chain spot must be positive");
    }
    let as_of = chain.as_of_date();
    let picker = ChainPicker { chain, as_of, r: params.r, q: params.q };
    let (front, back) = chain_expiries(chain, template, params)?;
    let mut strat = Strategy::new(template.strategy_name(), chain.underlying.clone(), as_of);
    let mut strikes: Vec<f64> = Vec::new();
    for spec in template.specs(params) {
        let Some(kind) = spec.kind else {
            strat.add_leg(Leg::stock(spec.sign * params.qty, chain.spot));
            strikes.push(chain.spot);
            continue;
        };
        let e = if spec.back { back } else { front };
        let q = match spec.strike {
            StrikeRule::Delta(d) => picker.by_delta(e, kind, d)?,
            StrikeRule::Atm => picker.by_strike(e, kind, chain.spot, |_| true)?,
            StrikeRule::SameAs(i) => picker.by_strike(e, kind, strikes[i], |_| true)?,
            StrikeRule::Relative(i, f, above) => {
                let anchor = strikes[i];
                picker.by_strike(e, kind, anchor * f, |k| if above { k > anchor + 1e-9 } else { k < anchor - 1e-9 })?
            }
        };
        strikes.push(q.strike);
        strat.add_leg(picker.leg(q, spec.sign * params.qty));
    }
    Ok(strat)
}

/// Build a template synthetically with flat `iv` Black-Scholes prices, strikes rounded
/// to the listed increment ([`round_strike`]) and standard monthly expiries
/// ([`monthly_expiry_near`]). Useful for back-of-envelope analysis and backtests.
pub fn build_synthetic(
    template: StrategyTemplate,
    spot: f64,
    iv: f64,
    r: f64,
    q: f64,
    as_of: NaiveDate,
    params: &TemplateParams,
) -> Strategy {
    let back0 = monthly_expiry_near(as_of, params.long_dte);
    let (front, back) = if template.is_multi_expiry() {
        let front = monthly_expiry_near(as_of, params.short_dte);
        let back = if back0 > front { back0 } else { next_monthly_expiry(front) };
        (front, back)
    } else {
        (back0, back0)
    };
    let step = strike_step(spot);
    let mut strat = Strategy::new(template.strategy_name(), "", as_of);
    let mut strikes: Vec<f64> = Vec::new();
    for spec in template.specs(params) {
        let Some(kind) = spec.kind else {
            strat.add_leg(Leg::stock(spec.sign * params.qty, spot));
            strikes.push(spot);
            continue;
        };
        let e = if spec.back { back } else { front };
        let t = year_frac(as_of, e).max(1.0 / 365.0);
        let k = match spec.strike {
            StrikeRule::Delta(d) => round_strike(strike_for_delta(kind, signed_delta(kind, d), spot, t, r, q, iv), spot),
            StrikeRule::Atm => round_strike(spot, spot),
            StrikeRule::SameAs(i) => strikes[i],
            StrikeRule::Relative(i, f, above) => {
                let anchor = strikes[i];
                let k = round_strike(anchor * f, spot);
                if above && k <= anchor {
                    anchor + step
                } else if !above && k >= anchor {
                    (anchor - step).max(step)
                } else {
                    k
                }
            }
        };
        strikes.push(k);
        let price = bs_price(kind, spot, k, t, r, q, iv);
        strat.add_leg(Leg::option(kind, k, e, spec.sign * params.qty, price, iv));
    }
    strat
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::chain::ExerciseStyle;
    use crate::pricing::d1_d2;
    use OptionKind::{Call, Put};

    fn d(y: i32, m: u32, dd: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, dd).unwrap()
    }

    fn mp(spot: f64) -> MarketParams {
        MarketParams::new(spot, 0.04, 0.0)
    }

    #[test]
    fn third_friday_and_monthly() {
        assert_eq!(third_friday(2027, 1), d(2027, 1, 15));
        assert_eq!(third_friday(2026, 11), d(2026, 11, 20));
        assert_eq!(third_friday(2026, 5), d(2026, 5, 15));
        let as_of = d(2026, 11, 4);
        assert_eq!(monthly_expiry_near(as_of, 1), d(2026, 11, 20));
        assert_eq!(monthly_expiry_near(as_of, 45), d(2026, 12, 18));
        let e = monthly_expiry_near(as_of, 540);
        assert!(((e - as_of).num_days() - 540).abs() <= 16, "{e}");
        assert_eq!(e.weekday(), Weekday::Fri);
        // Strictly after as_of, even on expiry day.
        assert_eq!(monthly_expiry_near(d(2026, 11, 20), 0), d(2026, 12, 18));
        assert_eq!(next_monthly_expiry(d(2026, 12, 18)), d(2027, 1, 15));
    }

    #[test]
    fn strike_rounding() {
        assert_eq!(strike_step(20.0), 0.5);
        assert_eq!(round_strike(512.6, 520.0), 515.0);
        assert_eq!(round_strike(512.4, 200.0), 512.0);
        assert_eq!(round_strike(21_013.0, 21_000.0), 21_025.0);
    }

    #[test]
    fn long_call_payoff_and_greeks() {
        let e = d(2027, 1, 15);
        let s = Strategy::new("lc", "SPY", d(2026, 1, 15)).with_leg(Leg::option(Call, 100.0, e, 2.0, 5.0, 0.2));
        let p = mp(100.0);
        assert!((s.entry_cost() - 1000.0).abs() < 1e-9);
        assert!((s.net_premium() - 1000.0).abs() < 1e-9);
        assert!((s.payoff_at_expiry(120.0, &p) - (2.0 * 100.0 * 15.0)).abs() < 1e-9);
        assert!((s.payoff_at_expiry(80.0, &p) + 1000.0).abs() < 1e-9);
        let be = s.breakevens(e, &p);
        assert_eq!(be.len(), 1);
        assert!((be[0] - 105.0).abs() < 1e-6, "{be:?}");
        let m = s.max_profit_loss(e, &p);
        assert!(m.profit_unbounded && !m.loss_unbounded);
        assert!((m.max_loss + 1000.0).abs() < 1e-6);
        // Greeks in position terms.
        let g = s.greeks(100.0, d(2026, 1, 15), &p);
        let single = bs_greeks(Call, 100.0, 100.0, 1.0, 0.04, 0.0, 0.2);
        assert!((g.delta - 200.0 * single.delta).abs() < 1e-9);
        assert!((g.vega - 200.0 * single.vega).abs() < 1e-9);
        assert!((g.theta - 200.0 * single.theta).abs() < 1e-9);
        assert!((s.capital_required(&p) - 1000.0).abs() < 1e-6);
    }

    #[test]
    fn bull_call_spread_analytics() {
        let e = d(2027, 1, 15);
        let (k1, k2, c1, c2) = (100.0, 110.0, 7.0, 3.0);
        let s = Strategy::new("bcs", "QQQ", d(2026, 6, 1))
            .with_leg(Leg::option(Call, k1, e, 1.0, c1, 0.2))
            .with_leg(Leg::option(Call, k2, e, -1.0, c2, 0.2));
        let p = mp(100.0);
        let debit = c1 - c2;
        let be = s.breakevens(e, &p);
        assert_eq!(be.len(), 1);
        assert!((be[0] - (k1 + debit)).abs() < 1e-6, "{be:?}");
        let m = s.max_profit_loss(e, &p);
        assert!((m.max_profit - (k2 - k1 - debit) * 100.0).abs() < 1e-6);
        assert!((m.max_loss + debit * 100.0).abs() < 1e-6);
        assert!(!m.profit_unbounded && !m.loss_unbounded);
        assert!((s.capital_required(&p) - 400.0).abs() < 1e-6);
        // P&L curve is monotone non-decreasing at expiry.
        let curve = s.pnl_curve(80.0, 130.0, 51, e, &p);
        assert_eq!(curve.len(), 51);
        assert!(curve.windows(2).all(|w| w[1].1 >= w[0].1 - 1e-9));
    }

    #[test]
    fn credit_spread_and_csp_capital() {
        let e = d(2026, 12, 18);
        let bps = Strategy::new("bps", "SPY", d(2026, 11, 4))
            .with_leg(Leg::option(Put, 600.0, e, -1.0, 6.0, 0.2))
            .with_leg(Leg::option(Put, 580.0, e, 1.0, 2.0, 0.2));
        let p = mp(620.0);
        assert!((bps.entry_cost() + 400.0).abs() < 1e-9);
        assert!((bps.capital_required(&p) - (2000.0 - 400.0)).abs() < 1e-6);
        let be = bps.breakevens(e, &p);
        assert!((be[0] - 596.0).abs() < 1e-6);

        let csp = Strategy::new("csp", "SPY", d(2026, 11, 4)).with_leg(Leg::option(Put, 600.0, e, -1.0, 6.0, 0.2));
        assert!((csp.capital_required(&p) - (60_000.0 - 600.0)).abs() < 1.0);
        // Reg-T: 6 + max(0.2*620 - 20, 0.1*600) = 6 + 104 = 110 per share.
        assert!((csp.margin_required(&p) - 11_000.0).abs() < 1e-6);
    }

    #[test]
    fn naked_call_is_unbounded_and_uses_reg_t() {
        let e = d(2026, 12, 18);
        let s = Strategy::new("nc", "SPY", d(2026, 11, 4)).with_leg(Leg::option(Call, 650.0, e, -1.0, 4.0, 0.2));
        let p = mp(620.0);
        let m = s.max_profit_loss(e, &p);
        assert!(m.loss_unbounded && !m.profit_unbounded);
        // 4 + max(124 - 30, 62) = 98 per share.
        assert!((s.capital_required(&p) - 9_800.0).abs() < 1e-6);
        // Covered by stock: no longer naked.
        let cc = s.clone().with_leg(Leg::stock(100.0, 620.0));
        assert!(!cc.max_profit_loss(e, &p).loss_unbounded);
        assert!((cc.capital_required(&p) - (62_000.0 - 400.0)).abs() < 1.0);
    }

    #[test]
    fn iron_condor_two_breakevens() {
        let e = d(2026, 12, 18);
        let s = Strategy::new("ic", "SPY", d(2026, 11, 4))
            .with_leg(Leg::option(Put, 580.0, e, 1.0, 1.0, 0.2))
            .with_leg(Leg::option(Put, 600.0, e, -1.0, 3.0, 0.2))
            .with_leg(Leg::option(Call, 650.0, e, -1.0, 3.0, 0.2))
            .with_leg(Leg::option(Call, 670.0, e, 1.0, 1.0, 0.2));
        let p = mp(625.0);
        let be = s.breakevens(e, &p);
        assert_eq!(be.len(), 2, "{be:?}");
        assert!((be[0] - 596.0).abs() < 1e-6 && (be[1] - 654.0).abs() < 1e-6);
        let m = s.max_profit_loss(e, &p);
        assert!((m.max_profit - 400.0).abs() < 1e-6);
        assert!((m.max_loss + 1600.0).abs() < 1e-6);
        assert!((s.capital_required(&p) - 1600.0).abs() < 1e-6);
    }

    #[test]
    fn pop_matches_closed_form_for_long_call() {
        let created = d(2026, 1, 2);
        let e = created + Duration::days(365);
        let (s0, k, prem, sigma, mu) = (100.0, 105.0, 6.0, 0.25, 0.08);
        let s = Strategy::new("lc", "QQQ", created).with_leg(Leg::option(Call, k, e, 1.0, prem, sigma));
        let p = mp(s0);
        let pop = s.probability_of_profit(e, &p, sigma, mu);
        // P(S_T > K + prem) = N(d2) with drift mu.
        let t = 1.0;
        let (_, d2) = d1_d2(s0, k + prem, t, mu, 0.0, sigma);
        assert!((pop - norm_cdf(d2)).abs() < 1e-8, "{pop} vs {}", norm_cdf(d2));
        // Expected P&L under risk-neutral drift = e^{rT}·BS − premium·100.
        let r = 0.04;
        let ev = s.expected_pnl(e, &p, sigma, r);
        let analytic = (bs_price(Call, s0, k, t, r, 0.0, sigma) * (r * t).exp() - prem) * 100.0;
        assert!((ev - analytic).abs() < 0.05, "{ev} vs {analytic}");
    }

    #[test]
    fn pop_iron_condor_between_strikes() {
        let created = d(2026, 1, 2);
        let e = created + Duration::days(45);
        let s = Strategy::new("ic", "SPY", created)
            .with_leg(Leg::option(Put, 90.0, e, 1.0, 0.5, 0.2))
            .with_leg(Leg::option(Put, 95.0, e, -1.0, 1.5, 0.2))
            .with_leg(Leg::option(Call, 105.0, e, -1.0, 1.5, 0.2))
            .with_leg(Leg::option(Call, 110.0, e, 1.0, 0.5, 0.2));
        let p = mp(100.0);
        let (sigma, mu) = (0.2, 0.0);
        let t = 45.0 / 365.0;
        let z = |x: f64| ((x / 100.0).ln() - (mu - 0.5 * sigma * sigma) * t) / (sigma * t.sqrt());
        let expected = norm_cdf(z(107.0)) - norm_cdf(z(93.0));
        let pop = s.probability_of_profit(e, &p, sigma, mu);
        assert!((pop - expected).abs() < 1e-8, "{pop} vs {expected}");
    }

    #[test]
    fn synthetic_long_behaves_like_stock() {
        let as_of = d(2026, 11, 4);
        let tp = StrategyTemplate::SyntheticLong.default_params();
        let s = build_synthetic(StrategyTemplate::SyntheticLong, 500.0, 0.2, 0.04, 0.0, as_of, &tp);
        assert_eq!(s.legs.len(), 2);
        let g = s.greeks(500.0, as_of, &mp(500.0));
        assert!((g.delta - 100.0).abs() < 1e-6, "{}", g.delta);
        assert!(g.gamma.abs() < 1e-9);
        let m = s.max_profit_loss(s.expiry_date(), &mp(500.0));
        assert!(m.profit_unbounded);
        assert!(s.margin_required(&mp(500.0)) < s.capital_required(&mp(500.0)));
    }

    #[test]
    fn synthetic_templates_build_and_are_consistent() {
        let as_of = d(2026, 11, 4);
        let spot = 520.0;
        for t in ALL_TEMPLATES {
            let tp = t.default_params();
            let s = build_synthetic(t, spot, 0.22, 0.04, 0.006, as_of, &tp);
            assert!(!s.legs.is_empty(), "{t:?}");
            assert!(!t.description_zh().is_empty() && t.description_zh().chars().count() > 150, "{t:?}");
            assert_eq!(StrategyTemplate::from_key(t.key()), Some(t));
            // P&L at creation is ~0 (fills at model value).
            let pnl0 = s.pnl_at(spot, as_of, &MarketParams::new(spot, 0.04, 0.006));
            assert!(pnl0.abs() < 1e-6, "{t:?} pnl0={pnl0}");
            let p = MarketParams::new(spot, 0.04, 0.006);
            let cap = s.capital_required(&p);
            assert!(cap.is_finite() && cap >= 0.0, "{t:?}");
            let sum = s.summary(s.expiry_date(), &p, 0.22, 0.04);
            assert!((0.0..=1.0).contains(&sum.probability_of_profit));
            for l in &s.legs {
                if let Some(k) = l.strike() {
                    assert!((k / 5.0 - (k / 5.0).round()).abs() < 1e-9, "{t:?} strike {k}");
                }
            }
        }
        // Spot-check structure.
        let leaps = build_synthetic(StrategyTemplate::LeapsStockReplacement, spot, 0.22, 0.04, 0.006, as_of, &StrategyTemplate::LeapsStockReplacement.default_params());
        let g = leaps.greeks(spot, as_of, &MarketParams::new(spot, 0.04, 0.006));
        assert!((g.delta - 80.0).abs() < 3.0, "{}", g.delta);
        let pmcc = build_synthetic(StrategyTemplate::PoorMansCoveredCall, spot, 0.22, 0.04, 0.006, as_of, &StrategyTemplate::PoorMansCoveredCall.default_params());
        assert!(pmcc.legs[0].expiry().unwrap() > pmcc.legs[1].expiry().unwrap());
        assert!(pmcc.legs[0].strike().unwrap() < pmcc.legs[1].strike().unwrap());
        let cal = build_synthetic(StrategyTemplate::CallCalendar, spot, 0.22, 0.04, 0.006, as_of, &StrategyTemplate::CallCalendar.default_params());
        assert_eq!(cal.legs[0].strike(), cal.legs[1].strike());
        assert!(cal.legs[1].expiry() > cal.legs[0].expiry());
        assert!(cal.entry_cost() > 0.0);
    }

    /// Synthetic chain with BS-priced quotes (flat or skewed vol).
    pub(crate) fn toy_chain(spot: f64, as_of: NaiveDate, expiries: &[NaiveDate], vol: impl Fn(f64, f64) -> f64, with_delta: bool) -> OptionChain {
        let (r, q) = (0.04, 0.01);
        let mut quotes = Vec::new();
        for &e in expiries {
            let t = year_frac(as_of, e);
            let mut k = (spot * 0.5 / 5.0).round() * 5.0;
            while k <= spot * 1.6 {
                for kind in [Call, Put] {
                    let v = vol(k, t);
                    let g = bs_greeks(kind, spot, k, t, r, q, v);
                    let mid = g.price;
                    if mid < 0.05 {
                        continue;
                    }
                    let half = (mid * 0.01).max(0.01);
                    quotes.push(OptionQuote {
                        contract: format!("X{}{}{}", e, kind.label(), k),
                        expiry: e,
                        strike: k,
                        kind,
                        bid: mid - half,
                        ask: mid + half,
                        last: mid,
                        volume: 100.0,
                        open_interest: 1000.0,
                        iv: Some(v),
                        delta: if with_delta { Some(g.delta) } else { None },
                        gamma: Some(g.gamma),
                        theta: None,
                        vega: None,
                    });
                }
                k += 5.0;
            }
        }
        OptionChain {
            underlying: "TEST".into(),
            spot,
            as_of: as_of.and_hms_opt(16, 0, 0).unwrap(),
            source: "test".into(),
            style: ExerciseStyle::American,
            quotes,
        }
    }

    #[test]
    fn build_from_chain_picks_by_delta() {
        let as_of = d(2026, 11, 4);
        let exps = [d(2026, 12, 18), d(2027, 3, 19), d(2027, 6, 17), d(2028, 1, 21)];
        for with_delta in [true, false] {
            let chain = toy_chain(500.0, as_of, &exps, |_, _| 0.2, with_delta);
            let mut tp = StrategyTemplate::PoorMansCoveredCall.default_params();
            tp.q = 0.01;
            let s = build_from_chain(StrategyTemplate::PoorMansCoveredCall, &chain, &tp).unwrap();
            assert_eq!(s.legs.len(), 2);
            assert_eq!(s.legs[0].expiry(), Some(d(2028, 1, 21)));
            assert_eq!(s.legs[1].expiry(), Some(d(2026, 12, 18)));
            let t_long = year_frac(as_of, d(2028, 1, 21));
            let dl = bs_greeks(Call, 500.0, s.legs[0].strike().unwrap(), t_long, 0.04, 0.01, 0.2).delta;
            assert!((dl - 0.80).abs() < 0.03, "{dl}");
            assert!((s.legs[0].iv - 0.2).abs() < 1e-6);
            assert!(s.legs[1].quantity < 0.0);

            for t in ALL_TEMPLATES {
                let s = build_from_chain(t, &chain, &t.default_params()).unwrap_or_else(|e| panic!("{t:?}: {e}"));
                assert!(!s.legs.is_empty());
                assert_eq!(s.underlying, "TEST");
            }
            let bcs = build_from_chain(StrategyTemplate::BullCallSpread, &chain, &StrategyTemplate::BullCallSpread.default_params()).unwrap();
            assert!(bcs.legs[1].strike().unwrap() > bcs.legs[0].strike().unwrap());
            let ic = build_from_chain(StrategyTemplate::IronCondor, &chain, &StrategyTemplate::IronCondor.default_params()).unwrap();
            let ks: Vec<f64> = ic.legs.iter().map(|l| l.strike().unwrap()).collect();
            assert!(ks[1] < ks[0] && ks[0] < ks[2] && ks[2] < ks[3], "{ks:?}");
        }
    }

    #[test]
    fn american_and_vol_shift_marking() {
        let e = d(2027, 11, 19);
        let s = Strategy::new("lp", "SPY", d(2026, 11, 4)).with_leg(Leg::option(Put, 120.0, e, 1.0, 20.0, 0.2));
        let euro = MarketParams::new(100.0, 0.05, 0.0);
        let amer = euro.with_american(true);
        let v_e = s.value_at(100.0, d(2026, 11, 4), &euro);
        let v_a = s.value_at(100.0, d(2026, 11, 4), &amer);
        assert!(v_a > v_e);
        let shifted = s.value_at(100.0, d(2026, 11, 4), &euro.with_vol_shift(0.05));
        assert!(shifted > v_e);
        // Expired legs → intrinsic.
        assert!((s.value_at(90.0, e, &euro) - 3000.0).abs() < 1e-9);
        assert!(s.legs[0].is_expired(e));
    }

    #[test]
    fn serde_roundtrip() {
        let s = build_synthetic(StrategyTemplate::Collar, 500.0, 0.2, 0.04, 0.01, d(2026, 11, 4), &StrategyTemplate::Collar.default_params());
        let js = serde_json::to_string(&s).unwrap();
        let back: Strategy = serde_json::from_str(&js).unwrap();
        assert_eq!(s, back);
        assert!(s.legs[1].describe().contains('P'));
    }
}
