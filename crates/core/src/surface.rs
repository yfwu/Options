//! Option-chain analytics ("數值分析"): per-quote implied vols, implied forwards,
//! quadratic smile fits, an interpolated volatility surface, and chain-level
//! statistics (expected move, put/call ratios, max pain, dealer gamma exposure).
//!
//! Conventions: `t` in years (ACT/365), log-moneyness `k = ln(K / F)`,
//! vols as decimals. IVs come from the provider when sane (1%–300%), otherwise
//! they are inverted from the mid price with Black-Scholes.

use crate::chain::{OptionChain, OptionKind, OptionQuote};
use crate::pricing::{bs_greeks, implied_vol, norm_pdf, strike_for_delta, year_frac};
use crate::strategy::sane_iv;
use chrono::NaiveDate;
use serde::{Deserialize, Serialize};

/// Quotes with a relative spread above this are ignored by fits.
pub const MAX_SPREAD_PCT: f64 = 0.50;

/// Time to expiry used for analytics (floored at a few hours so same-day expiries do not blow up).
fn t_of(as_of: NaiveDate, expiry: NaiveDate) -> f64 {
    year_frac(as_of, expiry).max(0.5 / 365.0)
}

/// Implied vol of a quote: provider IV if sane, else inverted from mid.
pub fn quote_iv(quote: &OptionQuote, spot: f64, t: f64, r: f64, q: f64) -> Option<f64> {
    sane_iv(quote.iv).or_else(|| {
        let m = quote.mid();
        if m <= 0.0 {
            return None;
        }
        implied_vol(quote.kind, m, spot, quote.strike, t, r, q).filter(|v| *v > 0.01 && *v < 3.0)
    })
}

/// Per-quote volatility record (for plotting and fitting).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QuoteVol {
    pub expiry: NaiveDate,
    pub strike: f64,
    pub kind: OptionKind,
    pub t: f64,
    pub mid: f64,
    pub iv: f64,
    /// Log-moneyness ln(K/F) against the implied forward of the expiry.
    pub k: f64,
    pub spread_pct: Option<f64>,
    /// BS vega per share per 1 vol point.
    pub vega: f64,
    pub delta: f64,
}

/// Forward for an expiry from put-call parity on (up to 5) near-ATM strikes:
/// `F = K + e^{rT}(C − P)`, median across strikes. Falls back to `S·e^{(r−q)T}`
/// when parity data is missing or inconsistent (>10% from carry forward).
pub fn implied_forward(chain: &OptionChain, expiry: NaiveDate, r: f64, q: f64) -> f64 {
    let t = t_of(chain.as_of_date(), expiry);
    let carry = chain.spot * ((r - q) * t).exp();
    let mut pairs: Vec<(f64, f64)> = chain
        .slice(expiry, OptionKind::Call)
        .into_iter()
        .filter(|c| c.bid > 0.0 && c.ask > 0.0)
        .filter_map(|c| {
            chain
                .find(expiry, c.strike, OptionKind::Put)
                .filter(|p| p.bid > 0.0 && p.ask > 0.0)
                .map(|p| (c.strike, c.strike + (r * t).exp() * (c.mid() - p.mid())))
        })
        .collect();
    if pairs.is_empty() {
        return carry;
    }
    pairs.sort_by(|a, b| (a.0 - chain.spot).abs().total_cmp(&(b.0 - chain.spot).abs()));
    let mut fs: Vec<f64> = pairs.iter().take(5).map(|p| p.1).collect();
    fs.sort_by(f64::total_cmp);
    let med = median_sorted(&fs);
    if med.is_finite() && (med / carry - 1.0).abs() < 0.10 {
        med
    } else {
        carry
    }
}

fn median_sorted(v: &[f64]) -> f64 {
    let n = v.len();
    if n == 0 {
        f64::NAN
    } else if n % 2 == 1 {
        v[n / 2]
    } else {
        0.5 * (v[n / 2 - 1] + v[n / 2])
    }
}

/// Every quote with a usable IV for one expiry (both kinds), with moneyness vs. the implied forward.
pub fn expiry_vols(chain: &OptionChain, expiry: NaiveDate, r: f64, q: f64) -> Vec<QuoteVol> {
    let as_of = chain.as_of_date();
    if expiry <= as_of {
        return Vec::new();
    }
    let t = t_of(as_of, expiry);
    let fwd = implied_forward(chain, expiry, r, q);
    chain
        .quotes
        .iter()
        .filter(|qt| qt.expiry == expiry)
        .filter_map(|qt| {
            let iv = quote_iv(qt, chain.spot, t, r, q)?;
            let g = bs_greeks(qt.kind, chain.spot, qt.strike, t, r, q, iv);
            Some(QuoteVol {
                expiry,
                strike: qt.strike,
                kind: qt.kind,
                t,
                mid: qt.mid(),
                iv,
                k: (qt.strike / fwd).ln(),
                spread_pct: qt.spread_pct(),
                vega: g.vega,
                delta: g.delta,
            })
        })
        .collect()
}

/// Quadratic smile `σ(k) = a + b·k + c·k²` for one expiry.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SmileFit {
    pub expiry: NaiveDate,
    pub t: f64,
    pub forward: f64,
    pub a: f64,
    pub b: f64,
    pub c: f64,
    /// Range of log-moneyness used in the fit; evaluation clamps to it (flat wings).
    pub k_min: f64,
    pub k_max: f64,
    pub n_points: usize,
    /// Weighted RMS error of the fit in vol units.
    pub rmse: f64,
}

impl SmileFit {
    /// Vol at log-moneyness `k` (clamped to the fitted range, floored at 1%).
    pub fn iv_k(&self, k: f64) -> f64 {
        let k = k.clamp(self.k_min, self.k_max);
        (self.a + self.b * k + self.c * k * k).max(0.01)
    }
    /// Vol at a strike.
    pub fn iv(&self, strike: f64) -> f64 {
        self.iv_k((strike / self.forward).ln())
    }
    /// ATM-forward vol (k = 0).
    pub fn atm_iv(&self) -> f64 {
        self.iv_k(0.0)
    }
    /// Total implied variance σ²t at log-moneyness k.
    pub fn total_variance_k(&self, k: f64) -> f64 {
        let v = self.iv_k(k);
        v * v * self.t
    }
}

/// Weighted least-squares quadratic fit of `(x, y, w)` points. Returns (a, b, c).
pub fn weighted_quadratic_fit(points: &[(f64, f64, f64)]) -> Option<(f64, f64, f64)> {
    // Normal equations: Σw·[1 x x²]ᵀ[1 x x²] β = Σw·[1 x x²]ᵀ y
    let mut m = [[0.0f64; 3]; 3];
    let mut v = [0.0f64; 3];
    for &(x, y, w) in points {
        if !(x.is_finite() && y.is_finite() && w.is_finite()) || w <= 0.0 {
            continue;
        }
        let basis = [1.0, x, x * x];
        for i in 0..3 {
            for j in 0..3 {
                m[i][j] += w * basis[i] * basis[j];
            }
            v[i] += w * basis[i] * y;
        }
    }
    solve3(m, v).map(|b| (b[0], b[1], b[2]))
}

/// Gaussian elimination with partial pivoting for a 3×3 system.
fn solve3(mut m: [[f64; 3]; 3], mut v: [f64; 3]) -> Option<[f64; 3]> {
    let scale = m.iter().flatten().fold(0.0f64, |a, x| a.max(x.abs()));
    if scale == 0.0 {
        return None;
    }
    for col in 0..3 {
        let piv = (col..3).max_by(|&a, &b| m[a][col].abs().total_cmp(&m[b][col].abs()))?;
        if m[piv][col].abs() < 1e-14 * scale {
            return None;
        }
        m.swap(col, piv);
        v.swap(col, piv);
        for row in col + 1..3 {
            let f = m[row][col] / m[col][col];
            for c in col..3 {
                m[row][c] -= f * m[col][c];
            }
            v[row] -= f * v[col];
        }
    }
    let mut x = [0.0; 3];
    for i in (0..3).rev() {
        let s: f64 = (i + 1..3).map(|j| m[i][j] * x[j]).sum();
        x[i] = (v[i] - s) / m[i][i];
    }
    x.iter().all(|z| z.is_finite()).then_some(x)
}

/// Fit the smile of one expiry. Uses out-of-the-money quotes (puts below the
/// forward, calls at/above), drops `bid <= 0` and spreads > 50%, and weights by
/// `vega / max(spread%, 1%)`. Falls back to a linear or flat fit with few points.
pub fn fit_smile(chain: &OptionChain, expiry: NaiveDate, r: f64, q: f64) -> Option<SmileFit> {
    let as_of = chain.as_of_date();
    if expiry <= as_of {
        return None;
    }
    let raw = expiry_vols(chain, expiry, r, q);
    let fwd = implied_forward(chain, expiry, r, q);
    let t = t_of(as_of, expiry);
    let bid_of = |qv: &QuoteVol| chain.find(expiry, qv.strike, qv.kind).map_or(0.0, |x| x.bid);
    let pts: Vec<(f64, f64, f64)> = raw
        .iter()
        .filter(|qv| match qv.kind {
            OptionKind::Put => qv.strike < fwd,
            OptionKind::Call => qv.strike >= fwd,
        })
        .filter(|qv| bid_of(qv) > 0.0 && qv.spread_pct.is_some_and(|s| s <= MAX_SPREAD_PCT))
        .map(|qv| (qv.k, qv.iv, qv.vega.max(1e-6) / qv.spread_pct.unwrap_or(1.0).max(0.01)))
        .collect();
    if pts.is_empty() {
        return None;
    }
    let k_min = pts.iter().map(|p| p.0).fold(f64::INFINITY, f64::min);
    let k_max = pts.iter().map(|p| p.0).fold(f64::NEG_INFINITY, f64::max);
    let wsum: f64 = pts.iter().map(|p| p.2).sum();
    let (a, b, c) = if pts.len() >= 3 && k_max - k_min > 1e-6 {
        weighted_quadratic_fit(&pts).unwrap_or_else(|| (pts.iter().map(|p| p.1 * p.2).sum::<f64>() / wsum, 0.0, 0.0))
    } else {
        (pts.iter().map(|p| p.1 * p.2).sum::<f64>() / wsum, 0.0, 0.0)
    };
    let rmse = (pts.iter().map(|p| p.2 * (a + b * p.0 + c * p.0 * p.0 - p.1).powi(2)).sum::<f64>() / wsum).sqrt();
    Some(SmileFit { expiry, t, forward: fwd, a, b, c, k_min, k_max, n_points: pts.len(), rmse })
}

/// Fitted volatility surface: one [`SmileFit`] per expiry, interpolated linearly
/// in total variance at constant log-moneyness, flat-vol extrapolated in time.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VolSurface {
    pub underlying: String,
    pub as_of: NaiveDate,
    pub spot: f64,
    pub r: f64,
    pub q: f64,
    /// Sorted by `t`.
    pub fits: Vec<SmileFit>,
}

impl VolSurface {
    pub fn is_empty(&self) -> bool {
        self.fits.is_empty()
    }

    pub fn fit_for(&self, expiry: NaiveDate) -> Option<&SmileFit> {
        self.fits.iter().find(|f| f.expiry == expiry)
    }

    /// Forward for any `t`, interpolating the implied carry rate between expiries.
    pub fn forward(&self, t: f64) -> f64 {
        let carry = |f: &SmileFit| (f.forward / self.spot).ln() / f.t;
        let c = match self.bracket(t) {
            None => self.r - self.q,
            Some((i, j, w)) => carry(&self.fits[i]) * (1.0 - w) + carry(&self.fits[j]) * w,
        };
        self.spot * (c * t).exp()
    }

    /// (i, j, weight of j) bracketing `t`; clamps outside the fitted range.
    fn bracket(&self, t: f64) -> Option<(usize, usize, f64)> {
        let n = self.fits.len();
        if n == 0 {
            return None;
        }
        if t <= self.fits[0].t {
            return Some((0, 0, 0.0));
        }
        if t >= self.fits[n - 1].t {
            return Some((n - 1, n - 1, 0.0));
        }
        let j = self.fits.partition_point(|f| f.t < t);
        let i = j - 1;
        let w = (t - self.fits[i].t) / (self.fits[j].t - self.fits[i].t);
        Some((i, j, w))
    }

    /// Implied vol at `strike` and time `t` (years). Returns `None` for an empty surface.
    pub fn iv(&self, strike: f64, t: f64) -> Option<f64> {
        let (i, j, w) = self.bracket(t)?;
        let t = t.max(1e-6);
        let k = (strike / self.forward(t)).ln();
        if i == j {
            return Some(self.fits[i].iv_k(k));
        }
        let (fi, fj) = (&self.fits[i], &self.fits[j]);
        let wv = fi.total_variance_k(k) * (1.0 - w) + fj.total_variance_k(k) * w;
        Some((wv.max(1e-10) / t).sqrt())
    }

    /// Implied vol at `strike` for an expiry date.
    pub fn iv_at_expiry(&self, strike: f64, expiry: NaiveDate) -> Option<f64> {
        if let Some(f) = self.fit_for(expiry) {
            return Some(f.iv(strike));
        }
        self.iv(strike, t_of(self.as_of, expiry))
    }

    /// ATM-forward vol per fitted expiry: `(calendar DTE, iv)`.
    pub fn atm_term_structure(&self) -> Vec<(i64, f64)> {
        self.fits.iter().map(|f| ((f.expiry - self.as_of).num_days(), f.atm_iv())).collect()
    }

    /// 25-delta risk reversal: IV(25Δ put) − IV(25Δ call). Positive = put skew.
    pub fn skew_25d(&self, expiry: NaiveDate) -> Option<f64> {
        let f = self.fit_for(expiry)?;
        let solve = |kind: OptionKind, delta: f64| {
            let mut v = f.atm_iv();
            let mut k = self.spot;
            for _ in 0..30 {
                k = strike_for_delta(kind, delta, self.spot, f.t, self.r, self.q, v);
                let nv = f.iv(k);
                if (nv - v).abs() < 1e-10 {
                    break;
                }
                v = nv;
            }
            f.iv(k)
        };
        Some(solve(OptionKind::Put, -0.25) - solve(OptionKind::Call, 0.25))
    }
}

/// Fit every future expiry of a chain into a [`VolSurface`].
pub fn fit_surface(chain: &OptionChain, r: f64, q: f64) -> VolSurface {
    let as_of = chain.as_of_date();
    let mut fits: Vec<SmileFit> = chain
        .expiries()
        .into_iter()
        .filter(|e| *e > as_of)
        .filter_map(|e| fit_smile(chain, e, r, q))
        .collect();
    fits.sort_by(|a, b| a.t.total_cmp(&b.t));
    VolSurface { underlying: chain.underlying.clone(), as_of, spot: chain.spot, r, q, fits }
}

// ---------------------------------------------------------------------------
// Chain statistics
// ---------------------------------------------------------------------------

/// Market-implied move to an expiry from the ATM straddle.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExpectedMove {
    pub expiry: NaiveDate,
    pub dte: i64,
    /// Straddle strike (closest to spot with both legs priced).
    pub strike: f64,
    /// Call mid + put mid (per share). Common rule of thumb: ≈ 1σ move (× 0.85 ≈ E|ΔS|).
    pub straddle: f64,
    pub move_pct: f64,
    pub lower: f64,
    pub upper: f64,
    pub atm_iv: Option<f64>,
    /// S·σ_ATM·√t (1-σ move implied by ATM vol).
    pub iv_move: Option<f64>,
}

/// Expected move for every future expiry.
pub fn expected_moves(chain: &OptionChain, r: f64, q: f64) -> Vec<ExpectedMove> {
    let as_of = chain.as_of_date();
    let s = chain.spot;
    chain
        .expiries()
        .into_iter()
        .filter(|e| *e > as_of)
        .filter_map(|e| {
            let t = t_of(as_of, e);
            let (k, c, p) = chain
                .strikes(e)
                .into_iter()
                .filter_map(|k| {
                    let c = chain.find(e, k, OptionKind::Call).filter(|x| x.mid() > 0.0)?;
                    let p = chain.find(e, k, OptionKind::Put).filter(|x| x.mid() > 0.0)?;
                    Some((k, c, p))
                })
                .min_by(|a, b| (a.0 - s).abs().total_cmp(&(b.0 - s).abs()))?;
            let straddle = c.mid() + p.mid();
            let ivs: Vec<f64> = [c, p].iter().filter_map(|x| quote_iv(x, s, t, r, q)).collect();
            let atm_iv = (!ivs.is_empty()).then(|| ivs.iter().sum::<f64>() / ivs.len() as f64);
            Some(ExpectedMove {
                expiry: e,
                dte: (e - as_of).num_days(),
                strike: k,
                straddle,
                move_pct: straddle / s,
                lower: s - straddle,
                upper: s + straddle,
                atm_iv,
                iv_move: atm_iv.map(|v| s * v * t.sqrt()),
            })
        })
        .collect()
}

/// Put/call volume and open-interest ratios.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PutCallRatio {
    /// `None` = whole chain.
    pub expiry: Option<NaiveDate>,
    pub call_volume: f64,
    pub put_volume: f64,
    pub call_oi: f64,
    pub put_oi: f64,
    /// put volume / call volume.
    pub volume_ratio: Option<f64>,
    /// put OI / call OI.
    pub oi_ratio: Option<f64>,
}

/// Put/call ratio for one expiry, or the whole chain when `expiry` is `None`.
pub fn put_call_ratio(chain: &OptionChain, expiry: Option<NaiveDate>) -> PutCallRatio {
    let (mut cv, mut pv, mut co, mut po) = (0.0, 0.0, 0.0, 0.0);
    for q in chain.quotes.iter().filter(|q| expiry.is_none_or(|e| q.expiry == e)) {
        let (vol, oi) = (q.volume.max(0.0), q.open_interest.max(0.0));
        match q.kind {
            OptionKind::Call => {
                cv += vol;
                co += oi;
            }
            OptionKind::Put => {
                pv += vol;
                po += oi;
            }
        }
    }
    PutCallRatio {
        expiry,
        call_volume: cv,
        put_volume: pv,
        call_oi: co,
        put_oi: po,
        volume_ratio: (cv > 0.0).then(|| pv / cv),
        oi_ratio: (co > 0.0).then(|| po / co),
    }
}

/// Put/call ratios for each expiry.
pub fn put_call_ratios_by_expiry(chain: &OptionChain) -> Vec<PutCallRatio> {
    chain.expiries().into_iter().map(|e| put_call_ratio(chain, Some(e))).collect()
}

/// Max-pain result for one expiry.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MaxPain {
    pub expiry: NaiveDate,
    /// Settlement price (among listed strikes) minimising total holder payout.
    pub strike: f64,
    /// Total intrinsic payout to option holders at `strike` ($, OI × 100).
    pub payout: f64,
    /// (settlement price, total payout $) for every listed strike.
    pub curve: Vec<(f64, f64)>,
}

/// Max pain for an expiry: the strike where Σ OI·intrinsic·100 is minimal.
pub fn max_pain(chain: &OptionChain, expiry: NaiveDate) -> Option<MaxPain> {
    let qs: Vec<&OptionQuote> = chain.quotes.iter().filter(|q| q.expiry == expiry && q.open_interest > 0.0).collect();
    if qs.is_empty() {
        return None;
    }
    let curve: Vec<(f64, f64)> = chain
        .strikes(expiry)
        .into_iter()
        .map(|s| {
            let pay: f64 = qs
                .iter()
                .map(|q| {
                    let intr = match q.kind {
                        OptionKind::Call => (s - q.strike).max(0.0),
                        OptionKind::Put => (q.strike - s).max(0.0),
                    };
                    intr * q.open_interest * 100.0
                })
                .sum();
            (s, pay)
        })
        .collect();
    let &(strike, payout) = curve.iter().min_by(|a, b| a.1.total_cmp(&b.1))?;
    Some(MaxPain { expiry, strike, payout, curve })
}

/// Dealer gamma exposure at one strike ($ change of dealer delta per 1% move).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GexLevel {
    pub strike: f64,
    pub call_gex: f64,
    pub put_gex: f64,
    pub net_gex: f64,
}

/// Dealer gamma exposure profile.
///
/// Convention (the common "naive GEX" assumption): dealers are **long calls and
/// short puts** customers trade, so call gamma counts **+** and put gamma **−**.
/// `GEX = Γ × OI × 100 × S² × 0.01`, i.e. the $ change in dealer delta for a 1%
/// move in the underlying. Positive total GEX → dealers hedge against the move
/// (dampening), negative → they chase it (amplifying).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GexProfile {
    pub spot: f64,
    /// `None` = all expiries.
    pub expiry: Option<NaiveDate>,
    pub by_strike: Vec<GexLevel>,
    pub total_gex: f64,
    /// Spot level where total GEX changes sign (sticky-strike vols, ±25% scan).
    pub gamma_flip: Option<f64>,
}

/// Compute GEX by strike (optionally for one expiry).
pub fn gamma_exposure(chain: &OptionChain, r: f64, q: f64, expiry: Option<NaiveDate>) -> GexProfile {
    let as_of = chain.as_of_date();
    let s = chain.spot;
    // (strike, sign, OI, t, iv, provider gamma)
    let items: Vec<(f64, f64, f64, f64, f64, Option<f64>)> = chain
        .quotes
        .iter()
        .filter(|x| x.expiry > as_of && x.open_interest > 0.0 && expiry.is_none_or(|e| x.expiry == e))
        .filter_map(|x| {
            let t = t_of(as_of, x.expiry);
            let iv = quote_iv(x, s, t, r, q)?;
            Some((x.strike, x.kind.sign(), x.open_interest, t, iv, x.gamma.filter(|g| g.is_finite() && *g >= 0.0)))
        })
        .collect();
    let gex_at = |spot: f64, use_provider: bool| -> Vec<(f64, f64)> {
        items
            .iter()
            .map(|&(k, sign, oi, t, iv, pg)| {
                let g = match (use_provider, pg) {
                    (true, Some(g)) => g,
                    _ => bs_greeks(OptionKind::Call, spot, k, t, r, q, iv).gamma,
                };
                (k, sign * g * oi * 100.0 * spot * spot * 0.01)
            })
            .collect()
    };
    let mut levels: Vec<GexLevel> = Vec::new();
    for (i, (k, g)) in gex_at(s, true).into_iter().enumerate() {
        let is_call = items[i].1 > 0.0;
        match levels.iter_mut().find(|l| (l.strike - k).abs() < 1e-9) {
            Some(l) => {
                if is_call {
                    l.call_gex += g
                } else {
                    l.put_gex += g
                }
                l.net_gex += g;
            }
            None => levels.push(GexLevel {
                strike: k,
                call_gex: if is_call { g } else { 0.0 },
                put_gex: if is_call { 0.0 } else { g },
                net_gex: g,
            }),
        }
    }
    levels.sort_by(|a, b| a.strike.total_cmp(&b.strike));
    let total_gex = levels.iter().map(|l| l.net_gex).sum();
    let total_at = |x: f64| gex_at(x, false).iter().map(|p| p.1).sum::<f64>();
    let gamma_flip = if items.is_empty() {
        None
    } else {
        let grid: Vec<f64> = (0..=100).map(|i| s * (0.75 + 0.5 * i as f64 / 100.0)).collect();
        let vals: Vec<f64> = grid.iter().map(|x| total_at(*x)).collect();
        // Choose the sign change closest to spot.
        (1..grid.len())
            .filter(|&i| (vals[i - 1] > 0.0) != (vals[i] > 0.0))
            .map(|i| {
                let (x0, x1, y0, y1) = (grid[i - 1], grid[i], vals[i - 1], vals[i]);
                x0 + (x1 - x0) * y0 / (y0 - y1)
            })
            .min_by(|a, b| (a - s).abs().total_cmp(&(b - s).abs()))
    };
    GexProfile { spot: s, expiry, by_strike: levels, total_gex, gamma_flip }
}

/// One smile point for plotting.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SmilePoint {
    pub strike: f64,
    /// ln(K/F).
    pub log_moneyness: f64,
    pub call_iv: Option<f64>,
    pub put_iv: Option<f64>,
    /// Fitted quadratic smile value, if a fit exists.
    pub fitted: Option<f64>,
}

/// Call / put IV per strike plus the fitted smile for one expiry.
pub fn smile_points(chain: &OptionChain, expiry: NaiveDate, r: f64, q: f64) -> Vec<SmilePoint> {
    let vols = expiry_vols(chain, expiry, r, q);
    let fit = fit_smile(chain, expiry, r, q);
    let fwd = fit.as_ref().map_or_else(|| implied_forward(chain, expiry, r, q), |f| f.forward);
    chain
        .strikes(expiry)
        .into_iter()
        .map(|k| {
            let get = |kind| vols.iter().find(|v| v.kind == kind && (v.strike - k).abs() < 1e-9).map(|v| v.iv);
            SmilePoint {
                strike: k,
                log_moneyness: (k / fwd).ln(),
                call_iv: get(OptionKind::Call),
                put_iv: get(OptionKind::Put),
                fitted: fit.as_ref().map(|f| f.iv(k)),
            }
        })
        .collect()
}

/// Bundle of every chain statistic, for one-call GUI/CLI consumption.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChainAnalytics {
    pub underlying: String,
    pub as_of: NaiveDate,
    pub spot: f64,
    pub surface: VolSurface,
    /// (DTE, ATM IV).
    pub term_structure: Vec<(i64, f64)>,
    /// (expiry, 25Δ put IV − 25Δ call IV).
    pub skew_25d: Vec<(NaiveDate, f64)>,
    pub expected_moves: Vec<ExpectedMove>,
    pub put_call_total: PutCallRatio,
    pub put_call_by_expiry: Vec<PutCallRatio>,
    pub max_pain: Vec<MaxPain>,
    pub gex: GexProfile,
}

impl ChainAnalytics {
    /// Compute everything for a chain.
    pub fn compute(chain: &OptionChain, r: f64, q: f64) -> Self {
        let surface = fit_surface(chain, r, q);
        let as_of = chain.as_of_date();
        let skew_25d = surface.fits.iter().filter_map(|f| surface.skew_25d(f.expiry).map(|s| (f.expiry, s))).collect();
        ChainAnalytics {
            underlying: chain.underlying.clone(),
            as_of,
            spot: chain.spot,
            term_structure: surface.atm_term_structure(),
            skew_25d,
            expected_moves: expected_moves(chain, r, q),
            put_call_total: put_call_ratio(chain, None),
            put_call_by_expiry: put_call_ratios_by_expiry(chain),
            max_pain: chain.expiries().into_iter().filter(|e| *e > as_of).filter_map(|e| max_pain(chain, e)).collect(),
            gex: gamma_exposure(chain, r, q, None),
            surface,
        }
    }
}

/// Black-Scholes vega per share per vol point (helper for weights elsewhere).
pub fn vega_1pt(spot: f64, strike: f64, t: f64, r: f64, q: f64, sigma: f64) -> f64 {
    if t <= 0.0 || sigma <= 0.0 {
        return 0.0;
    }
    let d1 = ((spot / strike).ln() + (r - q + 0.5 * sigma * sigma) * t) / (sigma * t.sqrt());
    spot * (-q * t).exp() * norm_pdf(d1) * t.sqrt() * 0.01
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chain::ExerciseStyle;
    use crate::strategy::tests::toy_chain;

    fn d(y: i32, m: u32, dd: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, dd).unwrap()
    }

    const R: f64 = 0.04;
    const Q: f64 = 0.01;

    fn quad_vol(spot: f64) -> impl Fn(f64, f64) -> f64 {
        move |k: f64, t: f64| {
            let f = spot * ((R - Q) * t).exp();
            let x = (k / f).ln();
            // Term-dependent level so interpolation is tested too.
            0.18 + 0.02 * t - 0.12 * x + 0.35 * x * x
        }
    }

    #[test]
    fn quadratic_fit_exact() {
        let pts: Vec<(f64, f64, f64)> = (-10..=10).map(|i| {
            let x = i as f64 * 0.05;
            (x, 0.2 - 0.1 * x + 0.3 * x * x, 1.0 + i.abs() as f64)
        }).collect();
        let (a, b, c) = weighted_quadratic_fit(&pts).unwrap();
        assert!((a - 0.2).abs() < 1e-12 && (b + 0.1).abs() < 1e-12 && (c - 0.3).abs() < 1e-12);
        assert!(weighted_quadratic_fit(&[(1.0, 1.0, 1.0)]).is_none());
    }

    #[test]
    fn implied_forward_matches_carry() {
        let as_of = d(2026, 11, 4);
        let e = d(2027, 6, 17);
        let chain = toy_chain(500.0, as_of, &[e], |_, _| 0.2, true);
        let f = implied_forward(&chain, e, R, Q);
        let t = year_frac(as_of, e);
        assert!((f - 500.0 * ((R - Q) * t).exp()).abs() < 1e-6, "{f}");
    }

    #[test]
    fn smile_fit_recovers_known_quadratic() {
        let as_of = d(2026, 11, 4);
        let exps = [d(2026, 12, 18), d(2027, 3, 19), d(2027, 12, 17)];
        for provider_iv in [true, false] {
            let mut chain = toy_chain(500.0, as_of, &exps, quad_vol(500.0), true);
            if !provider_iv {
                chain.quotes.iter_mut().for_each(|q| q.iv = None);
            }
            let surf = fit_surface(&chain, R, Q);
            assert_eq!(surf.fits.len(), 3);
            for f in &surf.fits {
                let a_true = 0.18 + 0.02 * f.t;
                assert!((f.a - a_true).abs() < 1e-4, "a={} vs {a_true} (provider={provider_iv})", f.a);
                assert!((f.b + 0.12).abs() < 2e-3, "b={}", f.b);
                assert!((f.c - 0.35).abs() < 5e-3, "c={}", f.c);
                assert!(f.rmse < 1e-3);
            }
            // At a fitted expiry the surface reproduces the smile.
            let e = exps[1];
            let iv = surf.iv_at_expiry(520.0, e).unwrap();
            let t = year_frac(as_of, e);
            assert!((iv - quad_vol(500.0)(520.0, t)).abs() < 1e-3);
            // Between expiries: total variance interpolation lies between neighbours.
            let tm = 0.5 * (surf.fits[1].t + surf.fits[2].t);
            let ivm = surf.iv(500.0, tm).unwrap();
            let lo = surf.fits[1].atm_iv().min(surf.fits[2].atm_iv()) - 0.01;
            let hi = surf.fits[1].atm_iv().max(surf.fits[2].atm_iv()) + 0.01;
            assert!(ivm > lo && ivm < hi, "{ivm}");
            // Term structure increases (level grows with t).
            let ts = surf.atm_term_structure();
            assert!(ts.windows(2).all(|w| w[1].1 > w[0].1 && w[1].0 > w[0].0));
            // Negative b → put skew positive.
            assert!(surf.skew_25d(e).unwrap() > 0.0);
        }
    }

    #[test]
    fn flat_surface_has_no_skew() {
        let as_of = d(2026, 11, 4);
        let e = d(2027, 3, 19);
        let chain = toy_chain(500.0, as_of, &[e], |_, _| 0.25, false);
        let surf = fit_surface(&chain, R, Q);
        assert!(surf.skew_25d(e).unwrap().abs() < 1e-6);
        assert!((surf.iv(400.0, 2.0).unwrap() - 0.25).abs() < 1e-6);
        let pts = smile_points(&chain, e, R, Q);
        assert!(pts.iter().any(|p| p.call_iv.is_some() && p.put_iv.is_some()));
        assert!(pts.iter().all(|p| p.fitted.is_some_and(|v| (v - 0.25).abs() < 1e-6)));
    }

    fn quote(e: NaiveDate, k: f64, kind: OptionKind, oi: f64, vol: f64) -> OptionQuote {
        OptionQuote {
            contract: String::new(),
            expiry: e,
            strike: k,
            kind,
            bid: 1.0,
            ask: 1.2,
            last: 1.1,
            volume: vol,
            open_interest: oi,
            iv: Some(0.2),
            delta: None,
            gamma: None,
            theta: None,
            vega: None,
        }
    }

    fn mini_chain(quotes: Vec<OptionQuote>) -> OptionChain {
        OptionChain {
            underlying: "T".into(),
            spot: 100.0,
            as_of: d(2026, 11, 4).and_hms_opt(16, 0, 0).unwrap(),
            source: "t".into(),
            style: ExerciseStyle::American,
            quotes,
        }
    }

    #[test]
    fn max_pain_toy_chain() {
        use OptionKind::*;
        let e = d(2026, 11, 20);
        let chain = mini_chain(vec![
            quote(e, 90.0, Put, 500.0, 10.0),
            quote(e, 95.0, Put, 300.0, 20.0),
            quote(e, 100.0, Call, 100.0, 30.0),
            quote(e, 100.0, Put, 100.0, 5.0),
            quote(e, 105.0, Call, 400.0, 40.0),
            quote(e, 110.0, Call, 600.0, 50.0),
        ]);
        // Payout at S: calls Σ oi·(S−K)+, puts Σ oi·(K−S)+  (×100)
        // S=95: puts 100@100 → 500; calls 0 → 500*100 = 50_000
        // S=100: puts 0, calls 0 → 0  ← max pain
        let mp = max_pain(&chain, e).unwrap();
        assert_eq!(mp.strike, 100.0);
        assert_eq!(mp.payout, 0.0);
        let at95 = mp.curve.iter().find(|p| p.0 == 95.0).unwrap().1;
        assert!((at95 - 50_000.0).abs() < 1e-9);
        let at110 = mp.curve.iter().find(|p| p.0 == 110.0).unwrap().1;
        assert!((at110 - (100.0 * 10.0 + 400.0 * 5.0) * 100.0).abs() < 1e-9);

        let pcr = put_call_ratio(&chain, None);
        assert_eq!(pcr.put_oi, 900.0);
        assert_eq!(pcr.call_oi, 1100.0);
        assert!((pcr.volume_ratio.unwrap() - 35.0 / 120.0).abs() < 1e-12);
        assert_eq!(put_call_ratios_by_expiry(&chain).len(), 1);
    }

    #[test]
    fn gex_signs_and_flip() {
        use OptionKind::*;
        let e = d(2026, 12, 18);
        // Heavy put OI below spot, call OI above → negative GEX at low spots, positive higher.
        let chain = mini_chain(vec![quote(e, 90.0, Put, 10_000.0, 0.0), quote(e, 110.0, Call, 10_000.0, 0.0)]);
        let g = gamma_exposure(&chain, R, Q, None);
        assert_eq!(g.by_strike.len(), 2);
        assert!(g.by_strike[0].put_gex < 0.0 && g.by_strike[0].call_gex == 0.0);
        assert!(g.by_strike[1].call_gex > 0.0);
        let flip = g.gamma_flip.unwrap();
        assert!((flip - 100.0).abs() < 3.0, "{flip}");
    }

    #[test]
    fn expected_move_from_straddle() {
        let as_of = d(2026, 11, 4);
        let e = d(2026, 12, 18);
        let chain = toy_chain(500.0, as_of, &[e], |_, _| 0.2, true);
        let em = expected_moves(&chain, R, Q);
        assert_eq!(em.len(), 1);
        let m = &em[0];
        assert_eq!(m.strike, 500.0);
        let t = year_frac(as_of, e);
        // ATM straddle ≈ 0.8·S·σ·√t.
        let approx = 0.7979 * 500.0 * 0.2 * t.sqrt();
        assert!((m.straddle - approx).abs() / approx < 0.03, "{} vs {approx}", m.straddle);
        assert!((m.iv_move.unwrap() - 500.0 * 0.2 * t.sqrt()).abs() < 1e-6);
        let all = ChainAnalytics::compute(&chain, R, Q);
        assert_eq!(all.max_pain.len(), 1);
        assert_eq!(all.term_structure.len(), 1);
        let js = serde_json::to_string(&all).unwrap();
        assert!(js.contains("term_structure"));
    }
}
