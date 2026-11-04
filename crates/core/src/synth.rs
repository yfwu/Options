//! Synthetic option pricing for historical backtests.
//!
//! Free historical option chains do not exist, so the LEAPS backtester prices
//! options with Black-Scholes using an implied-volatility *surface* reconstructed
//! from a volatility index (VIX for SPY/SPX, VXN for QQQ/NDX):
//!
//! ```text
//! σ_short       = vol_index/100 · iv_scale                       (≈ 1-month ATM IV)
//! W(κ, T)       = (1 − e^{−κT}) / (κT)
//! σ_ATM(T)      = σ_LR + (σ_short − σ_LR) · [w·W(κ_f,T) + (1−w)·W(κ_s,T)]  (two-factor term structure,
//!                 default κ_f = 4, κ_s = 0.5, w = 0.75; legacy single-κ: W(kappa,T))
//! x             = ln(K/S) / √T                                   (time-scaled log-moneyness)
//! m             = clamp(√(skew_ref_vol / σ_ATM(T)), 0.7, 1.5)    (skew is steeper when vol is low)
//! σ(K, T)       = σ_ATM(T) + m · (−skew_slope · x + smile_curv · x²)   (x < 0)
//!                 slope halved on the call wing (x > 0, `CALL_WING_FACTOR`)
//! ```
//!
//! clamped to `[min_vol, max_vol]`. With `skew_slope > 0` lower strikes carry a
//! higher IV (equity put skew); a deep in-the-money LEAPS call (e.g. K/S = 0.85 at
//! 18 months) therefore prices at roughly ATM + 4 vol points, which is in line
//! with SPY/SPX quotes.
//!
//! ## Term structure calibration ([`TermModel`])
//! A single reversion speed cannot fit both the steep contango of calm markets and
//! the deep inversion of crises. Two factors (75% of a vol shock decays fast, 25%
//! slowly) give, for SPY: σ3m/σ1m = 1.13 / 1.01 / 0.92 / 0.86 at VIX 12 / 20 / 40 / 82.7
//! (CBOE VIX3M/VIX bucket averages: 1.18 / 1.13 / 1.08 / 0.99 / 0.87 for VIX <15 /
//! 15–20 / 20–30 / 30–45 / >45 — mid-regime contango is somewhat too flat), and at
//! VIX 82.7 (2020-03-16) 1y / 1.5y ATM ≈ 40% / 36% (market ≈ 30–40% for 1y SPX),
//! versus 54% / 48% for the legacy κ = 1 model, which over-priced LEAPS in crises
//! (shallower drawdowns, inflated option "edge"). Select the old model with
//! `TermModel::SingleKappa` for comparisons.
//!
//! Notes / simplifications (documented on purpose):
//! * VIX is a 30-day variance-swap rate, which sits above 30-day ATM IV because it
//!   includes the skew. The presets use `iv_scale ≈ 0.9` for that reason.
//! * Moneyness is measured against spot (not forward); the difference at LEAPS
//!   horizons is absorbed by the calibration of `skew_slope`.
//! * Prices are European Black-Scholes with a continuous dividend yield. Early
//!   exercise of a long call on SPY/QQQ is almost never optimal except right before
//!   an ex-dividend date for very deep ITM calls, so the error is small.
//! * The variance risk premium is implicitly included: options are bought at IV,
//!   which historically exceeds subsequent realised volatility on average.

use serde::{Deserialize, Serialize};

use crate::chain::OptionKind;
use crate::pricing::{bs_greeks, strike_for_delta, Greeks};

/// Upside (x > 0) skew slope as a fraction of `skew_slope`: equity call wings are flatter.
pub const CALL_WING_FACTOR: f64 = 0.5;

/// ATM term-structure model of [`SynthVolModel`].
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum TermModel {
    /// Legacy single-speed mean reversion with `SynthVolModel::kappa` (κ = 1 in the
    /// presets): too sticky to the vol index in crises.
    SingleKappa,
    /// Two-factor reversion: `fast_weight` of the shock reverts at `fast_kappa`, the
    /// rest at `slow_kappa` (per year).
    TwoFactor { fast_kappa: f64, slow_kappa: f64, fast_weight: f64 },
}

impl Default for TermModel {
    /// Calibrated two-factor model (κ_f 4, κ_s 0.5, w 0.75). See module docs.
    fn default() -> Self {
        TermModel::TwoFactor { fast_kappa: 4.0, slow_kappa: 0.5, fast_weight: 0.75 }
    }
}

#[inline]
fn reversion_weight(kappa: f64, t: f64) -> f64 {
    let kt = kappa * t;
    if kt < 1e-8 {
        1.0
    } else {
        (1.0 - (-kt).exp()) / kt
    }
}

/// Implied-volatility surface driven by a volatility index. See module docs.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct SynthVolModel {
    /// Ratio of short-dated ATM option IV to the vol index (VIX ≈ 1.1× ATM IV ⇒ ~0.9).
    pub iv_scale: f64,
    /// Long-run ATM implied volatility the term structure reverts to (decimal).
    pub long_run_vol: f64,
    /// Term-structure mean-reversion speed per year for [`TermModel::SingleKappa`]
    /// (higher ⇒ long-dated IV hugs σ_LR). Ignored by the two-factor model.
    pub kappa: f64,
    /// ATM term-structure model (default: calibrated two-factor).
    #[serde(default)]
    pub term_model: TermModel,
    /// IV change per unit of time-scaled log-moneyness `ln(K/S)/√T` (positive ⇒ put skew).
    pub skew_slope: f64,
    /// Smile curvature on `x²` (small, positive).
    pub smile_curv: f64,
    /// ATM vol at which `skew_slope` applies unscaled; skew is scaled by √(ref/σ_ATM).
    pub skew_ref_vol: f64,
    pub min_vol: f64,
    pub max_vol: f64,
}

impl Default for SynthVolModel {
    fn default() -> Self {
        Self::spy()
    }
}

impl SynthVolModel {
    /// SPY options driven by VIX.
    pub fn spy() -> Self {
        Self {
            iv_scale: 0.90,
            long_run_vol: 0.195,
            kappa: 1.0,
            term_model: TermModel::default(),
            skew_slope: 0.30,
            smile_curv: 0.05,
            skew_ref_vol: 0.18,
            min_vol: 0.06,
            max_vol: 1.50,
        }
    }
    /// SPX/XSP index options driven by VIX (same surface as SPY).
    pub fn spx() -> Self {
        Self::spy()
    }
    /// QQQ options driven by VXN (Nasdaq-100 vol index). Flatter skew, higher base vol.
    pub fn qqq() -> Self {
        Self {
            iv_scale: 0.92,
            long_run_vol: 0.235,
            kappa: 1.0,
            term_model: TermModel::default(),
            skew_slope: 0.25,
            smile_curv: 0.05,
            skew_ref_vol: 0.22,
            min_vol: 0.08,
            max_vol: 1.80,
        }
    }
    /// NDX index options driven by VXN (same surface as QQQ).
    pub fn ndx() -> Self {
        Self::qqq()
    }
    /// Same surface with a different term-structure model (e.g. `TermModel::SingleKappa`
    /// to reproduce the legacy κ = 1 behaviour).
    pub fn with_term_model(mut self, term_model: TermModel) -> Self {
        self.term_model = term_model;
        self
    }

    /// ATM implied vol for maturity `t` (years) given the vol index in points (e.g. 18.5).
    #[inline]
    pub fn atm_vol(&self, t: f64, vol_index_pts: f64) -> f64 {
        let t = t.max(1.0 / 365.0);
        let short = (vol_index_pts / 100.0 * self.iv_scale).max(0.0);
        let w = match self.term_model {
            TermModel::SingleKappa => reversion_weight(self.kappa, t),
            TermModel::TwoFactor { fast_kappa, slow_kappa, fast_weight } => {
                let fw = fast_weight.clamp(0.0, 1.0);
                fw * reversion_weight(fast_kappa, t) + (1.0 - fw) * reversion_weight(slow_kappa, t)
            }
        };
        (self.long_run_vol + (short - self.long_run_vol) * w).clamp(self.min_vol, self.max_vol)
    }

    /// Calibrate `long_run_vol` and `iv_scale` to an observed ATM term structure
    /// (e.g. [`crate::surface::VolSurface::atm_term_structure`] of a live chain) given
    /// the vol index level on the same day. Points are `(dte_days, atm_iv_decimal)`;
    /// tenors under 14 days are ignored. Weighted least squares with weight ∝ √T so the
    /// long end (what LEAPS backtests depend on) dominates. Returns the calibrated
    /// model and the RMS error in vol points, or `None` with fewer than 2 usable points.
    pub fn calibrate_term_structure(&self, points: &[(i64, f64)], vol_index_pts: f64) -> Option<(SynthVolModel, f64)> {
        let pts: Vec<(f64, f64)> = points
            .iter()
            .filter(|(d, iv)| *d >= 14 && iv.is_finite() && *iv > 0.01 && *iv < 3.0)
            .map(|(d, iv)| (*d as f64 / 365.0, *iv))
            .collect();
        if pts.len() < 2 || vol_index_pts.is_nan() || vol_index_pts <= 0.0 {
            return None;
        }
        let err = |m: &SynthVolModel| -> f64 {
            let (mut se, mut sw) = (0.0, 0.0);
            for &(t, iv) in &pts {
                let w = t.sqrt();
                se += w * (m.atm_vol(t, vol_index_pts) - iv).powi(2);
                sw += w;
            }
            (se / sw).sqrt()
        };
        let mut best = (*self, err(self));
        // Coarse grid then two refinements: σ_LR ∈ [8%, 60%], iv_scale ∈ [0.6, 1.3].
        let (mut lo_l, mut hi_l, mut lo_s, mut hi_s) = (0.08, 0.60, 0.60, 1.30);
        for _ in 0..3 {
            let n = 26;
            for i in 0..=n {
                for j in 0..=n {
                    let mut m = *self;
                    m.long_run_vol = lo_l + (hi_l - lo_l) * i as f64 / n as f64;
                    m.iv_scale = lo_s + (hi_s - lo_s) * j as f64 / n as f64;
                    let e = err(&m);
                    if e < best.1 {
                        best = (m, e);
                    }
                }
            }
            let (dl, ds) = ((hi_l - lo_l) / 8.0, (hi_s - lo_s) / 8.0);
            lo_l = (best.0.long_run_vol - dl).max(0.05);
            hi_l = best.0.long_run_vol + dl;
            lo_s = (best.0.iv_scale - ds).max(0.3);
            hi_s = best.0.iv_scale + ds;
        }
        Some((best.0, best.1 * 100.0))
    }

    /// Implied vol for a given strike / spot / maturity (years) / vol-index level (points).
    #[inline]
    pub fn iv(&self, strike: f64, spot: f64, t: f64, vol_index_pts: f64) -> f64 {
        let t = t.max(1.0 / 365.0);
        let atm = self.atm_vol(t, vol_index_pts);
        if !(strike > 0.0 && spot > 0.0) {
            return atm;
        }
        let x = ((strike / spot).ln() / t.sqrt()).clamp(-2.5, 2.5);
        let m = (self.skew_ref_vol / atm).sqrt().clamp(0.7, 1.5);
        // the call wing (x > 0) is flatter than the put wing
        let slope = if x < 0.0 { self.skew_slope } else { self.skew_slope * CALL_WING_FACTOR };
        (atm + m * (-slope * x + self.smile_curv * x * x)).clamp(self.min_vol, self.max_vol)
    }

    /// Model mid price.
    #[allow(clippy::too_many_arguments)]
    pub fn price(&self, kind: OptionKind, spot: f64, strike: f64, t: f64, r: f64, q: f64, vol_index_pts: f64) -> f64 {
        self.greeks(kind, spot, strike, t, r, q, vol_index_pts).0.price
    }

    /// Model Greeks (BS at the surface IV, via [`fast_bs_greeks`]) and the IV used.
    #[inline]
    #[allow(clippy::too_many_arguments)]
    pub fn greeks(&self, kind: OptionKind, spot: f64, strike: f64, t: f64, r: f64, q: f64, vol_index_pts: f64) -> (Greeks, f64) {
        let v = self.iv(strike, spot, t, vol_index_pts);
        (fast_bs_greeks(kind, spot, strike, t, r, q, v), v)
    }

    /// Strike whose delta (at the surface IV of that strike) equals `target_delta`.
    /// Fixed-point iteration on the skewed IV; converges in a handful of steps.
    #[allow(clippy::too_many_arguments)]
    pub fn strike_for_delta(&self, kind: OptionKind, target_delta: f64, spot: f64, t: f64, r: f64, q: f64, vol_index_pts: f64) -> f64 {
        let mut v = self.atm_vol(t, vol_index_pts);
        let mut k = strike_for_delta(kind, target_delta, spot, t, r, q, v);
        for _ in 0..6 {
            let nv = self.iv(k, spot, t, vol_index_pts);
            if (nv - v).abs() < 1e-6 {
                break;
            }
            // damped update for stability on steep skews
            v = 0.5 * v + 0.5 * nv;
            k = strike_for_delta(kind, target_delta, spot, t, r, q, v);
        }
        k
    }
}

/// Fast standard normal CDF (Hart 1968 / West 2005 double-precision algorithm,
/// absolute error ≈ 1e-14). Used in the backtest hot loop instead of the
/// high-precision but iterative `pricing::norm_cdf`.
#[inline]
pub fn fast_norm_cdf(x: f64) -> f64 {
    let xa = x.abs();
    let c = if xa > 37.0 {
        0.0
    } else {
        let e = (-0.5 * xa * xa).exp();
        if xa < 7.071_067_811_865_47 {
            let mut n = 3.526_249_659_989_11e-2 * xa + 0.700_383_064_443_688;
            n = n * xa + 6.373_962_203_531_65;
            n = n * xa + 33.912_866_078_383;
            n = n * xa + 112.079_291_497_871;
            n = n * xa + 221.213_596_169_931;
            n = n * xa + 220.206_867_912_376;
            let mut d = 8.838_834_764_831_84e-2 * xa + 1.755_667_163_182_64;
            d = d * xa + 16.064_177_579_207;
            d = d * xa + 86.780_732_202_946_1;
            d = d * xa + 296.564_248_779_674;
            d = d * xa + 637.333_633_378_831;
            d = d * xa + 793.826_512_519_948;
            d = d * xa + 440.413_735_824_752;
            e * n / d
        } else {
            let mut b = xa + 0.65;
            b = xa + 4.0 / b;
            b = xa + 3.0 / b;
            b = xa + 2.0 / b;
            b = xa + 1.0 / b;
            e / b / 2.506_628_274_631
        }
    };
    if x > 0.0 {
        1.0 - c
    } else {
        c
    }
}

/// Black-Scholes-Merton Greeks with the same conventions as `pricing::bs_greeks`
/// (theta per calendar day, vega/rho per 1 point) but using [`fast_norm_cdf`].
#[inline]
pub fn fast_bs_greeks(kind: OptionKind, s: f64, k: f64, t: f64, r: f64, q: f64, sigma: f64) -> Greeks {
    if t <= 0.0 || sigma <= 0.0 || s <= 0.0 || k <= 0.0 {
        return bs_greeks(kind, s, k, t, r, q, sigma);
    }
    let sqrt_t = t.sqrt();
    let vs = sigma * sqrt_t;
    let d1 = ((s / k).ln() + (r - q + 0.5 * sigma * sigma) * t) / vs;
    let d2 = d1 - vs;
    let dq = (-q * t).exp();
    let dr = (-r * t).exp();
    let pdf = (-0.5 * d1 * d1).exp() * 0.398_942_280_401_432_7;
    let gamma = dq * pdf / (s * vs);
    let vega = s * dq * pdf * sqrt_t * 0.01;
    let decay = -s * dq * pdf * sigma / (2.0 * sqrt_t);
    match kind {
        OptionKind::Call => {
            let n1 = fast_norm_cdf(d1);
            let n2 = fast_norm_cdf(d2);
            let price = s * dq * n1 - k * dr * n2;
            let theta = decay - r * k * dr * n2 + q * s * dq * n1;
            Greeks { price, delta: dq * n1, gamma, theta: theta / 365.0, vega, rho: k * t * dr * n2 * 0.01 }
        }
        OptionKind::Put => {
            let n1 = fast_norm_cdf(-d1);
            let n2 = fast_norm_cdf(-d2);
            let price = k * dr * n2 - s * dq * n1;
            let theta = decay + r * k * dr * n2 - q * s * dq * n1;
            Greeks { price, delta: -dq * n1, gamma, theta: theta / 365.0, vega, rho: -k * t * dr * n2 * 0.01 }
        }
    }
}

/// Transaction-cost model for option trades.
///
/// A buy fills at `mid + half_spread + slippage·mid`, a sell at
/// `mid − half_spread − slippage·mid`, where
/// `half_spread = max(half_spread_pct·mid, min_half_spread) · m(vol)` and the crisis
/// multiplier `m(vol) = (vol / crisis_ref_vol)^crisis_exp` when the vol index is above
/// `crisis_ref_vol` (1 otherwise; `crisis_ref_vol = 0` disables widening). Commission is
/// charged per contract on every leg.
///
/// Default half spreads (calm markets, working limit orders near mid) for long-dated
/// 0.7–0.9Δ calls: SPY 0.6%, QQQ 0.75%, SPX 0.4%. Rationale: SPY/QQQ Jan LEAPS on a
/// $100–200 premium typically quote $1–3 wide (0.3–1% half spread); the only real long
/// dated chain available for validation (DOV, an illiquid single stock) showed 4.4%,
/// which is an upper bound, not representative of SPY/QQQ. Use
/// [`CostModel::conservative`] (1% + 0.25% slippage, stronger crisis widening) for
/// sensitivity checks.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct CostModel {
    /// Half of the bid/ask spread as a fraction of the option mid (0.005 = 0.5%).
    pub half_spread_pct: f64,
    /// Minimum half spread in dollars per share (tick-size floor).
    pub min_half_spread: f64,
    /// Commission + exchange fees per contract (dollars).
    pub commission_per_contract: f64,
    /// Extra adverse fill as a fraction of mid (market impact / not getting mid-ish fills).
    pub slippage: f64,
    /// Vol-index level (points) above which spreads widen; 0 disables widening.
    #[serde(default)]
    pub crisis_ref_vol: f64,
    /// Exponent of the widening: half spread × (vol/ref)^exp above the reference.
    #[serde(default)]
    pub crisis_exp: f64,
}

impl Default for CostModel {
    fn default() -> Self {
        Self::spy()
    }
}

impl CostModel {
    /// SPY LEAPS: 0.6% half spread, widening linearly with VIX above 25.
    pub fn spy() -> Self {
        Self { half_spread_pct: 0.006, min_half_spread: 0.05, commission_per_contract: 0.65, slippage: 0.0, crisis_ref_vol: 25.0, crisis_exp: 1.0 }
    }
    /// QQQ LEAPS: 0.75% half spread, widening linearly with VXN above 30 (VXN ≈ 1.17×VIX).
    pub fn qqq() -> Self {
        Self { half_spread_pct: 0.0075, min_half_spread: 0.05, commission_per_contract: 0.65, slippage: 0.0, crisis_ref_vol: 30.0, crisis_exp: 1.0 }
    }
    /// SPX LEAPS: wide in dollars but tighter in percent; higher per-contract fees.
    pub fn spx() -> Self {
        Self { half_spread_pct: 0.004, min_half_spread: 0.10, commission_per_contract: 1.50, slippage: 0.0, crisis_ref_vol: 25.0, crisis_exp: 1.0 }
    }
    /// Zero costs (for research / invariance tests).
    pub fn zero() -> Self {
        Self { half_spread_pct: 0.0, min_half_spread: 0.0, commission_per_contract: 0.0, slippage: 0.0, crisis_ref_vol: 0.0, crisis_exp: 0.0 }
    }
    /// Conservative: 1% half spread plus 0.25% slippage, spreads ∝ (vol/25)^1.5 in crises.
    pub fn conservative() -> Self {
        Self { half_spread_pct: 0.01, min_half_spread: 0.05, commission_per_contract: 0.65, slippage: 0.0025, crisis_ref_vol: 25.0, crisis_exp: 1.5 }
    }
    /// Copy without crisis widening (legacy behaviour).
    pub fn without_crisis_widening(mut self) -> Self {
        self.crisis_ref_vol = 0.0;
        self
    }

    /// Spread multiplier for a vol-index level (points); 1 in calm markets.
    #[inline]
    pub fn crisis_mult(&self, vol_index_pts: f64) -> f64 {
        if self.crisis_ref_vol > 0.0 && vol_index_pts > self.crisis_ref_vol {
            (vol_index_pts / self.crisis_ref_vol).powf(self.crisis_exp)
        } else {
            1.0
        }
    }
    /// Half spread in dollars per share for a given mid (no crisis widening).
    #[inline]
    pub fn half_spread(&self, mid: f64) -> f64 {
        if mid <= 0.0 {
            return 0.0;
        }
        (self.half_spread_pct * mid).max(self.min_half_spread)
    }
    /// Half spread including crisis widening at vol-index level `vol_index_pts`.
    #[inline]
    pub fn half_spread_at(&self, mid: f64, vol_index_pts: f64) -> f64 {
        self.half_spread(mid) * self.crisis_mult(vol_index_pts)
    }
    /// Fill price when buying (no crisis widening).
    #[inline]
    pub fn buy_price(&self, mid: f64) -> f64 {
        mid + self.half_spread(mid) + self.slippage * mid
    }
    /// Fill price when selling (never negative, no crisis widening).
    #[inline]
    pub fn sell_price(&self, mid: f64) -> f64 {
        (mid - self.half_spread(mid) - self.slippage * mid).max(0.0)
    }
    /// Fill price when buying at vol-index level `vol_index_pts`.
    #[inline]
    pub fn buy_price_at(&self, mid: f64, vol_index_pts: f64) -> f64 {
        mid + self.half_spread_at(mid, vol_index_pts) + self.slippage * mid
    }
    /// Fill price when selling at vol-index level `vol_index_pts` (never negative).
    #[inline]
    pub fn sell_price_at(&self, mid: f64, vol_index_pts: f64) -> f64 {
        (mid - self.half_spread_at(mid, vol_index_pts) - self.slippage * mid).max(0.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn term_structure_reverts() {
        let m = SynthVolModel::spy();
        // high vol: long-dated IV below short-dated
        assert!(m.atm_vol(2.0, 40.0) < m.atm_vol(0.08, 40.0));
        // low vol: long-dated above
        assert!(m.atm_vol(2.0, 11.0) > m.atm_vol(0.08, 11.0));
        // short end ~ index * scale
        assert!((m.atm_vol(1.0 / 365.0, 20.0) - 0.18).abs() < 0.002);
    }

    #[test]
    fn two_factor_crisis_long_end() {
        let m = SynthVolModel::spy();
        let (y1, y15) = (m.atm_vol(1.0, 82.7), m.atm_vol(1.5, 82.7));
        assert!((0.37..0.43).contains(&y1), "{y1}");
        assert!((0.33..0.39).contains(&y15), "{y15}");
        let old = m.with_term_model(TermModel::SingleKappa);
        assert!(old.atm_vol(1.0, 82.7) > 0.52);
        // calm: contango, VIX 20: roughly flat
        let r = |v: f64| m.atm_vol(0.25, v) / m.atm_vol(1.0 / 12.0, v);
        assert!(r(12.0) > 1.08 && (0.97..1.05).contains(&r(20.0)) && r(82.7) < 0.9, "{} {} {}", r(12.0), r(20.0), r(82.7));
    }

    /// Calibration check against CBOE VIX3M/VIX (bundled data; skipped if absent).
    #[test]
    fn two_factor_fits_vix3m_better() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../data");
        let read = |f: &str| -> Option<std::collections::HashMap<chrono::NaiveDate, f64>> {
            let txt = std::fs::read_to_string(dir.join(f)).ok()?;
            Some(
                txt.lines()
                    .skip(1)
                    .filter_map(|l| {
                        let c: Vec<&str> = l.split(',').collect();
                        let d = chrono::NaiveDate::parse_from_str(c.first()?.trim(), "%m/%d/%Y").ok()?;
                        Some((d, c.get(4)?.trim().parse::<f64>().ok()?))
                    })
                    .collect(),
            )
        };
        let (Some(vix), Some(v3)) = (read("VIX_History.csv"), read("VIX3M_History.csv")) else {
            return;
        };
        let pairs: Vec<(f64, f64)> = v3.iter().filter_map(|(d, b)| vix.get(d).map(|a| (*a, *b))).collect();
        assert!(pairs.len() > 3000);
        let buckets = [(0.0, 15.0), (15.0, 20.0), (20.0, 30.0), (30.0, 45.0), (45.0, 200.0)];
        let sse = |m: &SynthVolModel| {
            let (mut e, mut worst) = (0.0, 0.0f64);
            for (lo, hi) in buckets {
                let b: Vec<&(f64, f64)> = pairs.iter().filter(|p| p.0 >= lo && p.0 < hi).collect();
                let actual = b.iter().map(|p| p.1 / p.0).sum::<f64>() / b.len() as f64;
                let vbar = b.iter().map(|p| p.0).sum::<f64>() / b.len() as f64;
                let model = m.atm_vol(0.25, vbar) / m.atm_vol(1.0 / 12.0, vbar);
                worst = worst.max((model - actual).abs());
                e += (model - actual).powi(2);
            }
            (e, worst)
        };
        let two = SynthVolModel::spy();
        let one = two.with_term_model(TermModel::SingleKappa);
        let ((e2, w2), (e1, _)) = (sse(&two), sse(&one));
        assert!(e2 < e1 * 0.8, "{e2} vs {e1}");
        assert!(w2 < 0.11, "worst bucket error {w2}");
    }

    #[test]
    fn skew_lower_strikes_higher_iv() {
        let m = SynthVolModel::spy();
        let lo = m.iv(85.0, 100.0, 1.5, 16.0);
        let atm = m.iv(100.0, 100.0, 1.5, 16.0);
        let hi = m.iv(110.0, 100.0, 1.5, 16.0);
        assert!(lo > atm && atm > hi, "{lo} {atm} {hi}");
        // ~ +3..6 vol points for a 0.85 K/S 18m strike
        assert!((lo - atm) > 0.03 && (lo - atm) < 0.07, "{}", lo - atm);
    }

    #[test]
    fn delta_strike_consistent_with_surface() {
        let m = SynthVolModel::spy();
        let (s, t, r, q) = (500.0, 1.5, 0.04, 0.013);
        let k = m.strike_for_delta(OptionKind::Call, 0.80, s, t, r, q, 17.0);
        let (g, _) = m.greeks(OptionKind::Call, s, k, t, r, q, 17.0);
        assert!((g.delta - 0.80).abs() < 0.005, "{}", g.delta);
        assert!(k < s);
    }

    #[test]
    fn leaps_extrinsic_realistic() {
        // 0.80-delta 18-month SPY call: extrinsic ≈ 3–8% of spot per year at normal IV/rates.
        let m = SynthVolModel::spy();
        let (s, t, r, q) = (100.0, 1.5, 0.04, 0.013);
        let k = m.strike_for_delta(OptionKind::Call, 0.80, s, t, r, q, 17.0);
        let p = m.price(OptionKind::Call, s, k, t, r, q, 17.0);
        let ext = (p - (s - k).max(0.0)) / s / t;
        assert!(ext > 0.03 && ext < 0.08, "extrinsic/yr = {ext}");
    }

    #[test]
    #[allow(clippy::excessive_precision)]
    fn fast_greeks_match_reference() {
        // reference values: 0.5·erfc(−x/√2) in IEEE double (Python math.erfc)
        for (x, want) in [(-0.8, 0.21185539858339669), (-0.75, 0.22662735237686821), (-1.0, 0.15865525393145707), (-2.0, 0.02275013194817922), (0.3, 0.61791142218895256), (-8.0, 6.22096057427178e-16)] {
            let got = fast_norm_cdf(x);
            assert!((got - want).abs() < 1e-14 * want.max(1e-2), "x={x}: {got} vs {want}");
            assert!((fast_norm_cdf(-x) - (1.0 - want)).abs() < 1e-14);
        }
        for kind in [OptionKind::Call, OptionKind::Put] {
            for &k in &[50.0, 80.0, 100.0, 120.0, 200.0] {
                for &t in &[0.02, 0.5, 1.5, 3.0] {
                    let a = fast_bs_greeks(kind, 100.0, k, t, 0.04, 0.012, 0.23);
                    let b = bs_greeks(kind, 100.0, k, t, 0.04, 0.012, 0.23);
                    for (x, y) in [(a.price, b.price), (a.delta, b.delta), (a.gamma, b.gamma), (a.theta, b.theta), (a.vega, b.vega), (a.rho, b.rho)] {
                        // pricing::norm_cdf has ~1e-11 error near |x|≈0.75
                        assert!((x - y).abs() < 1e-8, "{kind:?} k={k} t={t}: {x} vs {y}");
                    }
                }
            }
        }
    }

    #[test]
    fn cost_model_fills() {
        let c = CostModel::spy();
        assert!(c.buy_price(100.0) > 100.0 && c.sell_price(100.0) < 100.0);
        assert!((c.half_spread(1.0) - 0.05).abs() < 1e-12);
        // crisis widening
        assert_eq!(c.half_spread_at(100.0, 20.0), c.half_spread(100.0));
        assert!((c.half_spread_at(100.0, 50.0) - 2.0 * c.half_spread(100.0)).abs() < 1e-12);
        assert!(c.sell_price_at(100.0, 80.0) < c.sell_price(100.0));
        assert_eq!(c.without_crisis_widening().half_spread_at(100.0, 80.0), c.half_spread(100.0));
        assert_eq!(CostModel::zero().buy_price(10.0), 10.0);
        assert_eq!(c.sell_price(0.01), 0.0);
    }
}


#[cfg(test)]
mod calibration_tests {
    use super::*;

    #[test]
    fn calibration_recovers_known_parameters() {
        let truth = SynthVolModel { long_run_vol: 0.235, iv_scale: 0.93, ..SynthVolModel::qqq() };
        let vix = 19.0;
        let pts: Vec<(i64, f64)> = [7, 30, 60, 91, 182, 273, 365, 547, 730]
            .iter()
            .map(|&d| (d as i64, truth.atm_vol(d as f64 / 365.0, vix)))
            .collect();
        let (fit, rms) = SynthVolModel::qqq().calibrate_term_structure(&pts, vix).unwrap();
        assert!(rms < 0.05, "rms {rms} vol pts");
        assert!((fit.long_run_vol - 0.235).abs() < 0.01, "{}", fit.long_run_vol);
        assert!((fit.iv_scale - 0.93).abs() < 0.03, "{}", fit.iv_scale);
        assert!(SynthVolModel::qqq().calibrate_term_structure(&pts[..1], vix).is_none());
    }
}
