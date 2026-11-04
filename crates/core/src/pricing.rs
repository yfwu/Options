//! Option pricing: Black-Scholes-Merton (continuous dividend yield), Greeks,
//! implied-volatility inversion and a Cox-Ross-Rubinstein binomial tree for
//! American exercise.
//!
//! Conventions used throughout the crate:
//! * `t` is time to expiry in **years** (ACT/365).
//! * `r` is the continuously compounded risk-free rate (0.045 = 4.5%).
//! * `q` is the continuous dividend yield (0.013 = 1.3%).
//! * `sigma` is annualised volatility (0.20 = 20%).
//! * Theta is reported **per calendar day**, vega and rho **per 1 vol/rate point** (0.01).

use crate::chain::OptionKind;
use serde::{Deserialize, Serialize};
use std::f64::consts::{PI, SQRT_2};

/// Standard normal probability density.
pub fn norm_pdf(x: f64) -> f64 {
    (-0.5 * x * x).exp() / (2.0 * PI).sqrt()
}

/// Standard normal cumulative distribution (via erfc, ~1e-15 accuracy).
pub fn norm_cdf(x: f64) -> f64 {
    0.5 * erfc(-x / SQRT_2)
}

/// Complementary error function (W. J. Cody rational approximations).
pub fn erfc(x: f64) -> f64 {
    // Numerical Recipes erfc (Chebyshev), fractional error < 1.2e-7 is not enough for
    // IV inversion of deep OTM options, so use a high precision series/continued fraction.
    if x < 0.0 {
        return 2.0 - erfc(-x);
    }
    if x < 0.5 {
        return 1.0 - erf_series(x);
    }
    if x > 27.0 {
        return 0.0;
    }
    // Continued fraction (Lentz) for erfc, converges well for x >= 0.5.
    let x2 = x * x;
    let tiny = 1e-300;
    let mut f = x;
    let mut c = x;
    let mut d = 0.0;
    // erfc(x) = exp(-x^2)/sqrt(pi) * 1/(x + (1/2)/(x + 1/(x + (3/2)/(x + 2/(x + ...)))))
    for n in 1..300 {
        let an = n as f64 * 0.5;
        d = x + an * d;
        if d.abs() < tiny {
            d = tiny;
        }
        c = x + an / c;
        if c.abs() < tiny {
            c = tiny;
        }
        d = 1.0 / d;
        let delta = c * d;
        f *= delta;
        if (delta - 1.0).abs() < 1e-16 {
            break;
        }
    }
    (-x2).exp() / (PI.sqrt() * f)
}

fn erf_series(x: f64) -> f64 {
    // erf(x) = 2/sqrt(pi) * sum_{n} (-1)^n x^(2n+1) / (n! (2n+1))
    let mut sum = x;
    let mut term = x;
    let x2 = x * x;
    for n in 1..60 {
        term *= -x2 / n as f64;
        let add = term / (2 * n + 1) as f64;
        sum += add;
        if add.abs() < 1e-17 * sum.abs() {
            break;
        }
    }
    2.0 / PI.sqrt() * sum
}

/// Inputs for a single-option valuation.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct PricingInput {
    pub kind: OptionKind,
    pub spot: f64,
    pub strike: f64,
    /// Years to expiry.
    pub t: f64,
    pub r: f64,
    pub q: f64,
    pub sigma: f64,
}

/// Full set of Black-Scholes Greeks.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct Greeks {
    pub price: f64,
    pub delta: f64,
    pub gamma: f64,
    /// Per calendar day.
    pub theta: f64,
    /// Per 1 vol point (0.01).
    pub vega: f64,
    /// Per 1 rate point (0.01).
    pub rho: f64,
}

impl std::ops::Add for Greeks {
    type Output = Greeks;
    fn add(self, o: Greeks) -> Greeks {
        Greeks {
            price: self.price + o.price,
            delta: self.delta + o.delta,
            gamma: self.gamma + o.gamma,
            theta: self.theta + o.theta,
            vega: self.vega + o.vega,
            rho: self.rho + o.rho,
        }
    }
}

impl std::ops::Mul<f64> for Greeks {
    type Output = Greeks;
    fn mul(self, k: f64) -> Greeks {
        Greeks {
            price: self.price * k,
            delta: self.delta * k,
            gamma: self.gamma * k,
            theta: self.theta * k,
            vega: self.vega * k,
            rho: self.rho * k,
        }
    }
}

fn intrinsic(kind: OptionKind, s: f64, k: f64) -> f64 {
    match kind {
        OptionKind::Call => (s - k).max(0.0),
        OptionKind::Put => (k - s).max(0.0),
    }
}

/// d1, d2 of Black-Scholes-Merton.
pub fn d1_d2(spot: f64, strike: f64, t: f64, r: f64, q: f64, sigma: f64) -> (f64, f64) {
    let vsqrt = sigma * t.sqrt();
    let d1 = ((spot / strike).ln() + (r - q + 0.5 * sigma * sigma) * t) / vsqrt;
    (d1, d1 - vsqrt)
}

/// European Black-Scholes-Merton price.
pub fn bs_price(kind: OptionKind, spot: f64, strike: f64, t: f64, r: f64, q: f64, sigma: f64) -> f64 {
    if t <= 0.0 || sigma <= 0.0 {
        // At/after expiry or zero vol: discounted forward intrinsic.
        let t = t.max(0.0);
        let fwd = spot * ((r - q) * t).exp();
        return (-r * t).exp() * intrinsic(kind, fwd, strike);
    }
    let (d1, d2) = d1_d2(spot, strike, t, r, q, sigma);
    let dq = (-q * t).exp();
    let dr = (-r * t).exp();
    match kind {
        OptionKind::Call => spot * dq * norm_cdf(d1) - strike * dr * norm_cdf(d2),
        OptionKind::Put => strike * dr * norm_cdf(-d2) - spot * dq * norm_cdf(-d1),
    }
}

/// European Black-Scholes-Merton Greeks.
pub fn bs_greeks(kind: OptionKind, spot: f64, strike: f64, t: f64, r: f64, q: f64, sigma: f64) -> Greeks {
    if t <= 0.0 || sigma <= 0.0 {
        let price = bs_price(kind, spot, strike, t, r, q, sigma);
        let itm = match kind {
            OptionKind::Call => spot > strike,
            OptionKind::Put => spot < strike,
        };
        return Greeks {
            price,
            delta: if itm { kind.sign() } else { 0.0 },
            ..Default::default()
        };
    }
    let (d1, d2) = d1_d2(spot, strike, t, r, q, sigma);
    let dq = (-q * t).exp();
    let dr = (-r * t).exp();
    let sqrt_t = t.sqrt();
    let pdf = norm_pdf(d1);
    let price = bs_price(kind, spot, strike, t, r, q, sigma);
    let gamma = dq * pdf / (spot * sigma * sqrt_t);
    let vega = spot * dq * pdf * sqrt_t * 0.01;
    let (delta, theta_y, rho) = match kind {
        OptionKind::Call => {
            let delta = dq * norm_cdf(d1);
            let theta = -spot * dq * pdf * sigma / (2.0 * sqrt_t) - r * strike * dr * norm_cdf(d2)
                + q * spot * dq * norm_cdf(d1);
            let rho = strike * t * dr * norm_cdf(d2) * 0.01;
            (delta, theta, rho)
        }
        OptionKind::Put => {
            let delta = -dq * norm_cdf(-d1);
            let theta = -spot * dq * pdf * sigma / (2.0 * sqrt_t) + r * strike * dr * norm_cdf(-d2)
                - q * spot * dq * norm_cdf(-d1);
            let rho = -strike * t * dr * norm_cdf(-d2) * 0.01;
            (delta, theta, rho)
        }
    };
    Greeks { price, delta, gamma, theta: theta_y / 365.0, vega, rho }
}

impl PricingInput {
    pub fn price(&self) -> f64 {
        bs_price(self.kind, self.spot, self.strike, self.t, self.r, self.q, self.sigma)
    }
    pub fn greeks(&self) -> Greeks {
        bs_greeks(self.kind, self.spot, self.strike, self.t, self.r, self.q, self.sigma)
    }
}

/// Cox-Ross-Rubinstein binomial tree price. `american = true` allows early exercise.
pub fn binomial_price(
    kind: OptionKind,
    spot: f64,
    strike: f64,
    t: f64,
    r: f64,
    q: f64,
    sigma: f64,
    steps: usize,
    american: bool,
) -> f64 {
    if t <= 0.0 || sigma <= 0.0 {
        return intrinsic(kind, spot, strike);
    }
    let n = steps.max(1);
    let dt = t / n as f64;
    let u = (sigma * dt.sqrt()).exp();
    let d = 1.0 / u;
    let disc = (-r * dt).exp();
    let p = ((((r - q) * dt).exp()) - d) / (u - d);
    let p = p.clamp(0.0, 1.0);
    let mut values: Vec<f64> = (0..=n)
        .map(|i| intrinsic(kind, spot * u.powi(i as i32) * d.powi((n - i) as i32), strike))
        .collect();
    for step in (0..n).rev() {
        for i in 0..=step {
            let cont = disc * (p * values[i + 1] + (1.0 - p) * values[i]);
            values[i] = if american {
                let s = spot * u.powi(i as i32) * d.powi((step - i) as i32);
                cont.max(intrinsic(kind, s, strike))
            } else {
                cont
            };
        }
    }
    values[0]
}

/// Price honouring exercise style: European → BSM, American → binomial (calls on
/// low-dividend underlyings are effectively European, so BSM is used when q≈0 for calls).
pub fn price_with_style(
    kind: OptionKind,
    spot: f64,
    strike: f64,
    t: f64,
    r: f64,
    q: f64,
    sigma: f64,
    american: bool,
) -> f64 {
    if !american || (kind == OptionKind::Call && q <= 1e-9) {
        bs_price(kind, spot, strike, t, r, q, sigma)
    } else {
        let steps = ((t * 365.0) as usize).clamp(50, 400);
        binomial_price(kind, spot, strike, t, r, q, sigma, steps, true)
            .max(bs_price(kind, spot, strike, t, r, q, sigma))
    }
}

/// Solve for the Black-Scholes implied volatility that reproduces `price`.
/// Uses Newton-Raphson with a bisection fallback. Returns `None` when the price is
/// outside no-arbitrage bounds.
pub fn implied_vol(kind: OptionKind, price: f64, spot: f64, strike: f64, t: f64, r: f64, q: f64) -> Option<f64> {
    if !(price.is_finite()) || price <= 0.0 || t <= 0.0 || spot <= 0.0 || strike <= 0.0 {
        return None;
    }
    let dq = (-q * t).exp();
    let dr = (-r * t).exp();
    let lower = match kind {
        OptionKind::Call => (spot * dq - strike * dr).max(0.0),
        OptionKind::Put => (strike * dr - spot * dq).max(0.0),
    };
    let upper = match kind {
        OptionKind::Call => spot * dq,
        OptionKind::Put => strike * dr,
    };
    if price < lower - 1e-9 || price > upper + 1e-9 {
        return None;
    }
    let (mut lo, mut hi) = (1e-4_f64, 5.0_f64);
    let f = |s: f64| bs_price(kind, spot, strike, t, r, q, s) - price;
    if f(lo) > 0.0 {
        return Some(lo);
    }
    if f(hi) < 0.0 {
        return None;
    }
    // Brenner-Subrahmanyam style initial guess.
    let mut sigma = ((2.0 * PI / t).sqrt() * price / spot).clamp(0.05, 2.0);
    for _ in 0..100 {
        let diff = f(sigma);
        if diff.abs() < 1e-10 {
            return Some(sigma);
        }
        if diff > 0.0 {
            hi = sigma;
        } else {
            lo = sigma;
        }
        let (d1, _) = d1_d2(spot, strike, t, r, q, sigma);
        let vega = spot * dq * norm_pdf(d1) * t.sqrt();
        let newton = sigma - diff / vega;
        sigma = if vega > 1e-12 && newton > lo && newton < hi { newton } else { 0.5 * (lo + hi) };
        if hi - lo < 1e-12 {
            return Some(sigma);
        }
    }
    Some(sigma)
}

/// Strike that gives a target Black-Scholes delta (e.g. 0.80 call, -0.25 put).
pub fn strike_for_delta(kind: OptionKind, target_delta: f64, spot: f64, t: f64, r: f64, q: f64, sigma: f64) -> f64 {
    // Call delta: e^{-qT} N(d1) = D  =>  d1 = N^{-1}(D e^{qT})
    let dq = (-q * t).exp();
    let p = match kind {
        OptionKind::Call => (target_delta / dq).clamp(1e-6, 1.0 - 1e-6),
        OptionKind::Put => (1.0 + target_delta / dq).clamp(1e-6, 1.0 - 1e-6),
    };
    let d1 = norm_inv(p);
    spot * (-(d1 * sigma * t.sqrt()) + (r - q + 0.5 * sigma * sigma) * t).exp()
}

/// Inverse standard normal CDF (Acklam's algorithm with one Halley refinement).
pub fn norm_inv(p: f64) -> f64 {
    if p <= 0.0 {
        return f64::NEG_INFINITY;
    }
    if p >= 1.0 {
        return f64::INFINITY;
    }
    const A: [f64; 6] = [-3.969683028665376e+01, 2.209460984245205e+02, -2.759285104469687e+02, 1.383577518672690e+02, -3.066479806614716e+01, 2.506628277459239e+00];
    const B: [f64; 5] = [-5.447609879822406e+01, 1.615858368580409e+02, -1.556989798598866e+02, 6.680131188771972e+01, -1.328068155288572e+01];
    const C: [f64; 6] = [-7.784894002430293e-03, -3.223964580411365e-01, -2.400758277161838e+00, -2.549732539343734e+00, 4.374664141464968e+00, 2.938163982698783e+00];
    const D: [f64; 4] = [7.784695709041462e-03, 3.224671290700398e-01, 2.445134137142996e+00, 3.754408661907416e+00];
    let plow = 0.02425;
    let x = if p < plow {
        let q = (-2.0 * p.ln()).sqrt();
        (((((C[0] * q + C[1]) * q + C[2]) * q + C[3]) * q + C[4]) * q + C[5]) / ((((D[0] * q + D[1]) * q + D[2]) * q + D[3]) * q + 1.0)
    } else if p <= 1.0 - plow {
        let q = p - 0.5;
        let r = q * q;
        (((((A[0] * r + A[1]) * r + A[2]) * r + A[3]) * r + A[4]) * r + A[5]) * q / (((((B[0] * r + B[1]) * r + B[2]) * r + B[3]) * r + B[4]) * r + 1.0)
    } else {
        let q = (-2.0 * (1.0 - p).ln()).sqrt();
        -(((((C[0] * q + C[1]) * q + C[2]) * q + C[3]) * q + C[4]) * q + C[5]) / ((((D[0] * q + D[1]) * q + D[2]) * q + D[3]) * q + 1.0)
    };
    // Halley refinement
    let e = norm_cdf(x) - p;
    let u = e * (2.0 * PI).sqrt() * (x * x / 2.0).exp();
    x - u / (1.0 + x * u / 2.0)
}

/// Year fraction between two dates, ACT/365.
pub fn year_frac(from: chrono::NaiveDate, to: chrono::NaiveDate) -> f64 {
    (to - from).num_days() as f64 / 365.0
}

#[cfg(test)]
mod tests {
    use super::*;
    use OptionKind::*;

    #[test]
    fn cdf_values() {
        assert!((norm_cdf(0.0) - 0.5).abs() < 1e-15);
        assert!((norm_cdf(1.96) - 0.9750021048517795).abs() < 1e-12);
        assert!((norm_cdf(-3.0) - 0.0013498980316301).abs() < 1e-13);
        assert!((norm_cdf(-8.0) - 6.22096057427178e-16).abs() < 1e-24);
        for p in [1e-6, 0.01, 0.3, 0.5, 0.77, 0.999] {
            assert!((norm_cdf(norm_inv(p)) - p).abs() < 1e-12, "p={p}");
        }
    }

    #[test]
    fn hull_reference_prices() {
        // Hull, Options Futures & Other Derivatives: S=42,K=40,r=10%,sigma=20%,T=0.5
        let c = bs_price(Call, 42.0, 40.0, 0.5, 0.10, 0.0, 0.20);
        let p = bs_price(Put, 42.0, 40.0, 0.5, 0.10, 0.0, 0.20);
        assert!((c - 4.7594).abs() < 1e-3, "{c}");
        assert!((p - 0.8086).abs() < 1e-3, "{p}");
    }

    #[test]
    fn put_call_parity_with_dividends() {
        let (s, k, t, r, q, v) = (5800.0, 6000.0, 1.3, 0.042, 0.013, 0.18);
        let c = bs_price(Call, s, k, t, r, q, v);
        let p = bs_price(Put, s, k, t, r, q, v);
        let parity = s * (-q * t).exp() - k * (-r * t).exp();
        assert!((c - p - parity).abs() < 1e-8);
    }

    #[test]
    fn greeks_match_finite_differences() {
        let (s, k, t, r, q, v) = (100.0, 95.0, 0.75, 0.04, 0.01, 0.25);
        for kind in [Call, Put] {
            let g = bs_greeks(kind, s, k, t, r, q, v);
            let h = 1e-3;
            let fd_delta = (bs_price(kind, s + h, k, t, r, q, v) - bs_price(kind, s - h, k, t, r, q, v)) / (2.0 * h);
            let fd_gamma = (bs_price(kind, s + h, k, t, r, q, v) - 2.0 * g.price + bs_price(kind, s - h, k, t, r, q, v)) / (h * h);
            let fd_vega = (bs_price(kind, s, k, t, r, q, v + 1e-4) - bs_price(kind, s, k, t, r, q, v - 1e-4)) / 2e-4 * 0.01;
            let fd_theta = (bs_price(kind, s, k, t - 1.0 / 365.0, r, q, v) - g.price);
            let fd_rho = (bs_price(kind, s, k, t, r + 1e-4, q, v) - bs_price(kind, s, k, t, r - 1e-4, q, v)) / 2e-4 * 0.01;
            assert!((g.delta - fd_delta).abs() < 1e-6);
            assert!((g.gamma - fd_gamma).abs() < 1e-4);
            assert!((g.vega - fd_vega).abs() < 1e-6);
            assert!((g.theta - fd_theta).abs() < 2e-4, "{} vs {}", g.theta, fd_theta);
            assert!((g.rho - fd_rho).abs() < 1e-6);
        }
    }

    #[test]
    fn iv_roundtrip() {
        for kind in [Call, Put] {
            for &k in &[60.0, 90.0, 100.0, 110.0, 160.0] {
                for &v in &[0.08, 0.2, 0.55, 1.2] {
                    let price = bs_price(kind, 100.0, k, 1.5, 0.045, 0.012, v);
                    if price < 1e-6 {
                        continue;
                    }
                    let iv = implied_vol(kind, price, 100.0, k, 1.5, 0.045, 0.012).unwrap();
                    let repriced = bs_price(kind, 100.0, k, 1.5, 0.045, 0.012, iv);
                    // Deep ITM / low vega options are ill-conditioned in vol space, so
                    // check the price is reproduced and the vol is close.
                    assert!((repriced - price).abs() < 1e-8, "kind={kind:?} k={k} v={v} iv={iv}");
                    assert!((iv - v).abs() < 1e-3, "kind={kind:?} k={k} v={v} iv={iv}");
                }
            }
        }
        assert!(implied_vol(Call, 200.0, 100.0, 100.0, 1.0, 0.0, 0.0).is_none());
    }

    #[test]
    fn binomial_converges_to_bs_and_american_put_premium() {
        let euro = binomial_price(Put, 100.0, 100.0, 1.0, 0.05, 0.0, 0.2, 500, false);
        let bs = bs_price(Put, 100.0, 100.0, 1.0, 0.05, 0.0, 0.2);
        assert!((euro - bs).abs() < 0.01);
        let amer = binomial_price(Put, 100.0, 100.0, 1.0, 0.05, 0.0, 0.2, 500, true);
        assert!(amer > bs);
        // Reference American put ~6.09 (CRR 500 steps)
        assert!((amer - 6.09).abs() < 0.02, "{amer}");
    }

    #[test]
    fn strike_for_delta_roundtrip() {
        let (s, t, r, q, v) = (600.0, 1.5, 0.04, 0.006, 0.22);
        let k = strike_for_delta(Call, 0.8, s, t, r, q, v);
        let g = bs_greeks(Call, s, k, t, r, q, v);
        assert!((g.delta - 0.8).abs() < 1e-9, "{}", g.delta);
        let kp = strike_for_delta(Put, -0.25, s, t, r, q, v);
        let gp = bs_greeks(Put, s, kp, t, r, q, v);
        assert!((gp.delta + 0.25).abs() < 1e-9);
    }
}
