"""Greek P&L attribution of the engine's daily records (QQQ 0.80Δ 1.0x)."""
import sys, math
import numpy as np, pandas as pd
sys.path.insert(0, 'py')
from optlab_ref import *
from scipy.stats import norm
inp = pd.read_csv('out/inputs_qqq.csv', parse_dates=['date']).set_index('date')
q = 0.007
res = []
for tag, f in [('κ=1 (引擎預設)', 'qqq_d80_1.0'), ('κ=3', 'qqq_d80_1.0_k3'), ('IV 不隨 VXN 變動', 'qqq_d80_1.0_const')]:
    r = pd.read_csv(f'out/curves/{f}.csv', parse_dates=['date']).set_index('date')
    r['rate'] = inp.rate.reindex(r.index) + 0.003
    tr = pd.read_csv(f'out/curves/{f}_trades.csv', parse_dates=['date', 'expiry'])
    exp_by_day = {}
    # expiry of the position held at end of each day: from last opening trade
    opens = tr[tr.contracts > 0][['date', 'expiry', 'strike']].drop_duplicates('date', keep='last').set_index('date')
    r['expiry'] = opens.expiry.reindex(r.index).ffill()
    comp = dict(delta=0., gamma=0., theta=0., vega=0., resid=0., cash=0., trade=0.)
    yrs = (r.index[-1] - r.index[0]).days / 365.25
    prev = None
    for d, x in r.iterrows():
        if prev is not None and prev.contracts > 0 and prev.equity > 0:
            n = prev.contracts * 100
            T0 = (prev.expiry - pd_prev_date).days / 365
            S0, S1, K = prev.spot, x.spot, prev.strike
            T1 = (prev.expiry - d).days / 365
            if T1 > 0:
                v0 = prev.iv
                st = v0 * math.sqrt(T0)
                d1 = (math.log(S0 / K) + (prev.rate - q + .5 * v0 * v0) * T0) / st
                gam = math.exp(-q * T0) * norm.pdf(d1) / (S0 * st)
                vega = S0 * math.exp(-q * T0) * norm.pdf(d1) * math.sqrt(T0)
                vm = VolModel.qqq()
                if 'k3' in f: vm.kappa = 3.0
                if 'const' in f: vm.kappa = 1000.0
                v1 = vm.iv(K, S1, T1, inp.vol.get(d))
                p0 = call_pd(S0, K, T0, prev.rate, q, v0)[0]; p0 = max(p0, S0 - K)
                p1 = call_pd(S1, K, T1, x.rate, q, v1)[0]; p1 = max(p1, S1 - K)
                dS = S1 - S0; dt = (d - pd_prev_date).days
                parts = dict(delta=prev.delta * dS, gamma=.5 * gam * dS * dS, theta=prev.theta / prev.contracts / 100 * dt if prev.contracts else 0, vega=vega * (v1 - v0))
                tot = p1 - p0
                parts['resid'] = tot - sum(parts.values())
                for k2, v in parts.items():
                    comp[k2] += n * v / prev.equity
                comp['cash'] += prev.cash * (math.exp(inp.rate.get(pd_prev_date) * dt / 365) - 1) / prev.equity
        prev = x; pd_prev_date = d
    # trading costs from trades
    eq = r.equity
    tc = tr.groupby('date').cost.sum()
    comp['trade'] = -(tc / eq.shift(1).reindex(tc.index).fillna(1e5)).sum()
    out = {k: v / yrs * 100 for k, v in comp.items()}
    out['strategy CAGR'] = ((eq.iloc[-1] / eq.iloc[0]) ** (1 / yrs) - 1) * 100
    bh = r.bh; out['B&H CAGR'] = ((bh.iloc[-1] / bh.iloc[0]) ** (1 / yrs) - 1) * 100
    out['avg exposure'] = r.exposure[r.contracts > 0].mean()
    res.append(pd.Series(out, name=tag))
t = pd.concat(res, axis=1).round(2)
txt = "QQQ 0.80Δ 540/180 1.0x ±25%，1999-2026 每日損益歸因 (Σ 每日分項/前日權益, 年化 %/yr; 算術加總, 非幾何)\n" + t.to_string()
open('out/attribution.txt', 'w').write(txt); print(txt)
