"""Independent reference implementation (numpy/scipy) of the OptLab engine pieces
under validation: Black-Scholes-Merton price/Greeks/IV, the VIX/VXN-driven
synthetic IV surface, the option cost model and a rolling-LEAPS backtest that
follows the rules documented in crates/core/src/backtest.rs.

Written from the documented rules, not by transliterating the Rust code; the
point is to catch places where the code does not do what the docs say.
"""
from __future__ import annotations

import datetime as dt
import math
from dataclasses import dataclass, field, replace
from typing import Optional

import numpy as np
import pandas as pd
from scipy.optimize import brentq
from scipy.stats import norm

# ---------------------------------------------------------------- Black-Scholes


def bs_price(kind, s, k, t, r, q, sig):
    if t <= 0 or sig <= 0:
        t = max(t, 0.0)
        f = s * math.exp((r - q) * t)
        intr = max(f - k, 0) if kind == "Call" else max(k - f, 0)
        return math.exp(-r * t) * intr
    st = sig * math.sqrt(t)
    d1 = (math.log(s / k) + (r - q + 0.5 * sig * sig) * t) / st
    d2 = d1 - st
    if kind == "Call":
        return s * math.exp(-q * t) * norm.cdf(d1) - k * math.exp(-r * t) * norm.cdf(d2)
    return k * math.exp(-r * t) * norm.cdf(-d2) - s * math.exp(-q * t) * norm.cdf(-d1)


def bs_greeks(kind, s, k, t, r, q, sig):
    """price, delta, gamma, theta/day, vega per 1 vol pt, rho per 1 rate pt"""
    st = sig * math.sqrt(t)
    d1 = (math.log(s / k) + (r - q + 0.5 * sig * sig) * t) / st
    d2 = d1 - st
    dq, dr = math.exp(-q * t), math.exp(-r * t)
    pdf = norm.pdf(d1)
    gamma = dq * pdf / (s * st)
    vega = s * dq * pdf * math.sqrt(t) / 100
    if kind == "Call":
        delta = dq * norm.cdf(d1)
        theta = -s * dq * pdf * sig / (2 * math.sqrt(t)) - r * k * dr * norm.cdf(d2) + q * s * dq * norm.cdf(d1)
        rho = k * t * dr * norm.cdf(d2) / 100
    else:
        delta = -dq * norm.cdf(-d1)
        theta = -s * dq * pdf * sig / (2 * math.sqrt(t)) + r * k * dr * norm.cdf(-d2) - q * s * dq * norm.cdf(-d1)
        rho = -k * t * dr * norm.cdf(-d2) / 100
    return bs_price(kind, s, k, t, r, q, sig), delta, gamma, theta / 365, vega, rho


def implied_vol(kind, price, s, k, t, r, q):
    f = lambda v: bs_price(kind, s, k, t, r, q, v) - price
    lo, hi = 1e-4, 5.0
    if f(lo) > 0 or f(hi) < 0:
        return float("nan")
    return brentq(f, lo, hi, xtol=1e-14, rtol=1e-14, maxiter=500)


# vectorised call price/delta (for the backtest hot loop)
def call_pd(s, k, t, r, q, sig):
    st = sig * np.sqrt(t)
    d1 = (np.log(s / k) + (r - q + 0.5 * sig * sig) * t) / st
    d2 = d1 - st
    p = s * np.exp(-q * t) * norm.cdf(d1) - k * np.exp(-r * t) * norm.cdf(d2)
    return p, np.exp(-q * t) * norm.cdf(d1)


def strike_for_call_delta(D, s, t, r, q, sig):
    d1 = norm.ppf(min(max(D * math.exp(q * t), 1e-6), 1 - 1e-6))
    return s * math.exp(-d1 * sig * math.sqrt(t) + (r - q + 0.5 * sig * sig) * t)


# ---------------------------------------------------------------- synthetic surface


@dataclass
class VolModel:
    iv_scale: float = 0.92
    long_run_vol: float = 0.235
    kappa: float = 1.0
    skew_slope: float = 0.25
    smile_curv: float = 0.05
    skew_ref_vol: float = 0.22
    min_vol: float = 0.08
    max_vol: float = 1.8
    call_wing_factor: float = 0.5
    # two-factor term structure (engine >= 6a0d2a3 default); None = single kappa
    two_factor: Optional[tuple] = None   # (fast_kappa, slow_kappa, fast_weight)

    @staticmethod
    def spy():
        return VolModel(0.90, 0.195, 1.0, 0.30, 0.05, 0.18, 0.06, 1.50)

    @staticmethod
    def qqq():
        return VolModel()

    def atm(self, t, vix):
        t = max(t, 1 / 365)
        short = max(vix / 100 * self.iv_scale, 0)
        rw = lambda k: 1.0 if k * t < 1e-8 else (1 - math.exp(-k * t)) / (k * t)
        if self.two_factor:
            kf, ks, fw = self.two_factor
            w = fw * rw(kf) + (1 - fw) * rw(ks)
        else:
            w = rw(self.kappa)
        return min(max(self.long_run_vol + (short - self.long_run_vol) * w, self.min_vol), self.max_vol)

    def iv(self, k, s, t, vix):
        t = max(t, 1 / 365)
        a = self.atm(t, vix)
        x = min(max(math.log(k / s) / math.sqrt(t), -2.5), 2.5)
        m = min(max(math.sqrt(self.skew_ref_vol / a), 0.7), 1.5)
        slope = self.skew_slope if x < 0 else self.skew_slope * self.call_wing_factor
        return min(max(a + m * (-slope * x + self.smile_curv * x * x), self.min_vol), self.max_vol)

    def strike_for_delta_fixedpoint(self, D, s, t, r, q, vix):
        """the engine's damped fixed point (6 iterations), for path-dependence tests"""
        v = self.atm(t, vix)
        k = strike_for_call_delta(D, s, t, r, q, v)
        for _ in range(6):
            nv = self.iv(k, s, t, vix)
            if abs(nv - v) < 1e-6:
                break
            v = 0.5 * v + 0.5 * nv
            k = strike_for_call_delta(D, s, t, r, q, v)
        return k

    def strike_for_delta(self, D, s, t, r, q, vix):
        if getattr(self, 'fixed_point', False):
            return self.strike_for_delta_fixedpoint(D, s, t, r, q, vix)
        """solve delta(K, iv(K)) = D exactly (root find on K) -- independent of the
        Rust damped fixed point."""
        def f(k):
            v = self.iv(k, s, t, vix)
            return call_pd(s, k, t, r, q, v)[1] - D
        lo, hi = s * 0.05, s * 5
        return brentq(f, lo, hi, xtol=1e-10)


@dataclass
class Costs:
    half_spread_pct: float = 0.006
    min_half_spread: float = 0.05
    commission: float = 0.65
    slippage: float = 0.0
    # crisis widening (validation extension, not in the engine): half spread is
    # multiplied by (vol/ref)^crisis_exp when vol > ref
    crisis_ref_vol: float = 0.0
    crisis_exp: float = 0.0

    def hs(self, mid, vol=None):
        if mid <= 0:
            return 0.0
        h = max(self.half_spread_pct * mid, self.min_half_spread)
        if self.crisis_ref_vol > 0 and vol is not None and vol > self.crisis_ref_vol:
            h *= (vol / self.crisis_ref_vol) ** self.crisis_exp
        return h

    def buy(self, mid, vol=None):
        return mid + self.hs(mid, vol) + self.slippage * mid

    def sell(self, mid, vol=None):
        return max(mid - self.hs(mid, vol) - self.slippage * mid, 0.0)


# ---------------------------------------------------------------- calendar


def third_friday(y, m):
    d = dt.date(y, m, 1)
    off = (4 - d.weekday()) % 7
    return d + dt.timedelta(days=off + 14)


def snap_expiry(as_of: dt.date, dte: int, min_dte: int) -> dt.date:
    target = as_of + dt.timedelta(days=dte)
    mn = as_of + dt.timedelta(days=min_dte)
    cands = []
    for k in (-1, 0, 1):
        idx = target.year * 12 + target.month - 1 + k
        e = third_friday(idx // 12, idx % 12 + 1)
        if e > mn:
            cands.append(e)
    if cands:
        # ties -> earlier candidate (first in order), like a strict '<' scan
        best = cands[0]
        for e in cands[1:]:
            if abs((e - target).days) < abs((best - target).days):
                best = e
        return best
    y, m = mn.year, mn.month
    while True:
        e = third_friday(y, m)
        if e > mn:
            return e
        m += 1
        if m == 13:
            y, m = y + 1, 1


# ---------------------------------------------------------------- backtest


@dataclass
class Cfg:
    delta: float = 0.80
    entry_dte: int = 540
    roll_dte: int = 180
    exposure: float = 1.5
    band: float = 0.25
    max_premium_pct: float = 0.9
    cash_yield: bool = True
    option_rate_spread: float = 0.003
    vol: VolModel = field(default_factory=VolModel.qqq)
    costs: Costs = field(default_factory=Costs)
    restrike_gap: float = 0.15
    initial: float = 100_000.0
    mult: float = 100.0
    # validation knobs (not in the engine)
    trade_lag: int = 0          # 0 = decide & trade at the same close; L = signal at t, fill at close t+L (sizes recomputed then)
    floor_intrinsic: bool = True


def load_inputs(path, q):
    df = pd.read_csv(path, parse_dates=["date"])
    last = df.date.iloc[-1]
    df["spot"] = df.adj * np.exp(q * (last - df.date).dt.days / 365.25)
    return df


def backtest(df: pd.DataFrame, cfg: Cfg, q: float, start=None, end=None, record=False):
    d = df
    if start is not None:
        d = d[d.date >= pd.Timestamp(start)]
    if end is not None:
        d = d[d.date <= pd.Timestamp(end)]
    dates = [x.date() for x in d.date]
    S, V, R = d.spot.values, d.vol.values, d.rate.values
    vm, cm = cfg.vol, cfg.costs
    cash, pos = cfg.initial, None  # pos = [K, expiry, n]
    eq_curve = np.empty(len(dates))
    recs = []
    wipe = cfg.initial * 1e-3
    pending = None
    wiped = False
    for i, day in enumerate(dates):
        s, vix, r = S[i], V[i], R[i]
        ro = r + cfg.option_rate_spread
        if i > 0 and cfg.cash_yield and cash > 0:
            cash *= math.exp(R[i - 1] * (day - dates[i - 1]).days / 365)
        if pos is not None and day >= pos[1]:
            intr = max(s - pos[0], 0)
            cash += intr * pos[2] * cfg.mult - (cm.commission * pos[2] if intr > 0 else 0)
            pos = None

        def mark(K, E):
            t = max((E - day).days / 365, 0)
            v = vm.iv(K, s, max(t, 1 / 365), vix)
            if t <= 0:
                return max(s - K, 0), (1.0 if s > K else 0.0), v
            p, dl = call_pd(s, K, t, ro, q, v)
            if cfg.floor_intrinsic:
                p = max(p, s - K)
            return float(p), float(dl), v

        price = delta = iv = 0.0
        if pos is not None:
            price, delta, iv = mark(pos[0], pos[1])
        equity = cash + (pos[2] * cfg.mult * price if pos else 0.0)
        if equity <= wipe:
            eq_curve[i:] = max(equity, 0)
            wiped = True
            break
        # trigger decision
        act = None
        if pos is None:
            act = "open"
        else:
            dte = (pos[1] - day).days
            expo = delta * s * cfg.mult * pos[2] / equity
            if dte <= cfg.roll_dte:
                act = "roll"
            elif cfg.band > 0 and abs(expo / cfg.exposure - 1) > cfg.band:
                act = "roll" if abs(delta - cfg.delta) > cfg.restrike_gap else "rebal"
        if cfg.trade_lag:
            # signal at today's close, executed at the close of day i+lag; signals seen
            # while an order is pending are dropped
            sig = act
            if pending is None and sig is not None:
                pending = (sig, i + cfg.trade_lag)
            if pending is not None and i >= pending[1]:
                act, pending = pending[0], None
            else:
                act = None
            if pos is None and act in ("roll", "rebal"):
                act = "open"
        if act == "rebal" and pos is not None:
            tgt = cfg.exposure * equity / (delta * s * cfg.mult)
            n = pos[2]
            new = tgt
            buy, sell = cm.buy(price, vix), cm.sell(price, vix)
            if new > n:
                cap = cfg.max_premium_pct * equity / (price * cfg.mult)
                cashn = n + cash / (buy * cfg.mult + cm.commission)
                new = min(new, max(cap, n), cashn)
            dn = new - n
            if abs(dn) > 0.01 * n and (dn > 0 or sell * cfg.mult > cm.commission):
                px = buy if dn > 0 else sell
                cash -= dn * px * cfg.mult + abs(dn) * cm.commission
                pos[2] = new
        elif act in ("roll", "open"):
            if pos is not None:
                sell = cm.sell(price, vix)
                c = cm.commission
                if sell * cfg.mult <= c:
                    sell, c = 0.0, 0.0
                cash += pos[2] * (sell * cfg.mult - c)
                pos = None
            E = snap_expiry(day, cfg.entry_dte, cfg.roll_dte + 14)
            t = (E - day).days / 365
            K = vm.strike_for_delta(cfg.delta, s, t, ro, q, vix)
            mid, dl, v = mark(K, E)
            buy = cm.buy(mid, vix)
            unit = buy * cfg.mult + cm.commission
            if mid > 0 and dl > 1e-6 and cash > 0:
                n = min(cfg.exposure * cash / (dl * s * cfg.mult), cfg.max_premium_pct * cash / unit, cash / unit)
                if n > 0:
                    cash -= n * unit
                    pos = [K, E, n]
                    price, delta, iv = mid, dl, v
        eq = cash + (pos[2] * cfg.mult * price if pos else 0.0)
        eq_curve[i] = eq
        if record:
            recs.append((day, eq, s, pos[2] if pos else 0, pos[0] if pos else 0, delta if pos else 0, iv if pos else 0, cash))
    out = pd.Series(eq_curve, index=pd.to_datetime(dates))
    if record:
        return out, pd.DataFrame(recs, columns=["date", "equity", "spot", "n", "K", "delta", "iv", "cash"]).set_index("date"), wiped
    return out, wiped


def stats(curve: pd.Series):
    v = curve.values
    yrs = (curve.index[-1] - curve.index[0]).days / 365.25
    fm = v[-1] / v[0]
    cagr = fm ** (1 / yrs) - 1 if fm > 0 else -1
    pk = np.maximum.accumulate(v)
    mdd = (v / pk - 1).min()
    return dict(cagr=cagr, mdd=mdd, final=fm, years=yrs)
