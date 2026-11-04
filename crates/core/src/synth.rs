//! Synthetic option pricing for historical backtests.
//!
//! Free historical option chains do not exist, so the LEAPS backtester prices
//! options with Black-Scholes using an implied-volatility *surface* reconstructed
//! from a volatility index (VIX for SPY/SPX, VXN for QQQ/NDX):
//!
//! ```text
//! σ_short       = vol_index/100 · iv_scale                       (≈ 1-month ATM IV)
//! σ_ATM(T)      = σ_LR + (σ_short − σ_LR) · (1 − e^{−κT}) / (κT)  (mean-reverting term structure)
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

/// Implied-volatility surface driven by a volatility index. See module docs.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct SynthVolModel {
    /// Ratio of short-dated ATM option IV to the vol index (VIX ≈ 1.1× ATM IV ⇒ ~0.9).
    pub iv_scale: f64,
    /// Long-run ATM implied volatility the term structure reverts to (decimal).
    pub long_run_vol: f64,
    /// Term-structure mean-reversion speed per year (higher ⇒ long-dated IV hugs σ_LR).
    pub kappa: f64,
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

    /// ATM implied vol for maturity `t` (years) given the vol index in points (e.g. 18.5).
    #[inline]
    pub fn atm_vol(&self, t: f64, vol_index_pts: f64) -> f64 {
        let t = t.max(1.0 / 365.0);
        let short = (vol_index_pts / 100.0 * self.iv_scale).max(0.0);
        let kt = self.kappa * t;
        let w = if kt < 1e-8 { 1.0 } else { (1.0 - (-kt).exp()) / kt };
        (self.long_run_vol + (short - self.long_run_vol) * w).clamp(self.min_vol, self.max_vol)
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
/// `mid − half_spread − slippage·mid`, where `half_spread = max(half_spread_pct·mid,
/// min_half_spread)`. Commission is charged per contract on every leg.
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
}

impl Default for CostModel {
    fn default() -> Self {
        Self::spy()
    }
}

impl CostModel {
    /// SPY LEAPS: deep ITM 1–2y calls typically quote $0.6–2.0 wide on a $100–200
    /// option ⇒ ~0.5% half spread when working limit orders near mid.
    pub fn spy() -> Self {
        Self { half_spread_pct: 0.005, min_half_spread: 0.05, commission_per_contract: 0.65, slippage: 0.0 }
    }
    /// QQQ LEAPS: slightly wider than SPY.
    pub fn qqq() -> Self {
        Self { half_spread_pct: 0.006, min_half_spread: 0.05, commission_per_contract: 0.65, slippage: 0.0 }
    }
    /// SPX LEAPS: wide in dollars but tighter in percent; higher per-contract fees.
    pub fn spx() -> Self {
        Self { half_spread_pct: 0.003, min_half_spread: 0.10, commission_per_contract: 1.50, slippage: 0.0 }
    }
    /// Zero costs (for research / invariance tests).
    pub fn zero() -> Self {
        Self { half_spread_pct: 0.0, min_half_spread: 0.0, commission_per_contract: 0.0, slippage: 0.0 }
    }
    /// Conservative: 1% half spread plus 0.25% slippage (illiquid strikes, market orders).
    pub fn conservative() -> Self {
        Self { half_spread_pct: 0.01, min_half_spread: 0.05, commission_per_contract: 0.65, slippage: 0.0025 }
    }

    /// Half spread in dollars per share for a given mid.
    #[inline]
    pub fn half_spread(&self, mid: f64) -> f64 {
        if mid <= 0.0 {
            return 0.0;
        }
        (self.half_spread_pct * mid).max(self.min_half_spread)
    }
    /// Fill price when buying.
    #[inline]
    pub fn buy_price(&self, mid: f64) -> f64 {
        mid + self.half_spread(mid) + self.slippage * mid
    }
    /// Fill price when selling (never negative).
    #[inline]
    pub fn sell_price(&self, mid: f64) -> f64 {
        (mid - self.half_spread(mid) - self.slippage * mid).max(0.0)
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
        assert_eq!(CostModel::zero().buy_price(10.0), 10.0);
        assert_eq!(c.sell_price(0.01), 0.0);
    }
}
