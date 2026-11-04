"""Task 2b: is the synthetic term structure realistic in crashes? and model VRP."""
import sys, math
import numpy as np, pandas as pd
sys.path.insert(0, '.')
from optlab_ref import *
from scipy.optimize import minimize_scalar
D = '../../data'
vix = pd.read_csv(f'{D}/VIX_History.csv', parse_dates=['DATE']).set_index('DATE').CLOSE.rename('vix')
v3 = pd.read_csv(f'{D}/VIX3M_History.csv', parse_dates=['DATE']).set_index('DATE').CLOSE.rename('vix3m')
x = pd.concat([vix, v3], axis=1).dropna()
out = []
m = VolModel.spy()
def model_ratio(k, vixv, T=0.25):
    mm = replace(m, kappa=k)
    return np.array([mm.atm(T, v) for v in vixv])
# VIX3M is a 3m variance-swap level; compare like with like: model 3m ATM / model 1m ATM vs VIX3M/VIX
for lo, hi in [(0, 15), (15, 20), (20, 30), (30, 45), (45, 100)]:
    s = x[(x.vix >= lo) & (x.vix < hi)]
    ratio_real = (s.vix3m / s.vix).median()
    r1 = np.median(model_ratio(1.0, s.vix.values) / np.array([m.atm(30/365, v) for v in s.vix.values]))
    out.append(f"VIX∈[{lo},{hi}) n={len(s)}: 實際 VIX3M/VIX 中位 {ratio_real:.3f} | 模型(κ=1) σ3m/σ1m 中位 {r1:.3f}")
def loss(k, s):
    mm = replace(m, kappa=k)
    a = np.array([mm.atm(0.25, v) / mm.atm(30/365, v) for v in s.vix.values])
    return np.mean((a - (s.vix3m / s.vix).values) ** 2)
for lab, s in [('全部', x), ('VIX>30', x[x.vix > 30])]:
    r = minimize_scalar(lambda k: loss(k, s), bounds=(0.05, 30), method='bounded')
    out.append(f"以 VIX3M/VIX 擬合 κ ({lab}): κ*={r.x:.2f}")
# crash-date table
spy = pd.read_csv(f'{D}/VIX_SPY_1993.csv', parse_dates=['Date']).set_index('Date')
qin = pd.read_csv('../out/inputs_qqq.csv', parse_dates=['date']).set_index('date')
qbc = pd.read_csv('../out/inputs_qqq_bc.csv', parse_dates=['date']).set_index('date')
out.append("\n危機日模型 ATM IV (κ=1, 引擎預設) vs 參考值")
out.append("日期        VIX   模型SPY 1y  1.5y | QQQ vol-idx(引擎) 模型QQQ 1y 1.5y | VIX3M")
for dte in ['2000-04-14', '2001-04-04', '2002-07-23', '2008-10-24', '2008-11-20', '2010-05-20', '2011-08-08', '2015-08-24', '2018-02-05', '2018-12-24', '2020-03-16', '2022-06-16', '2025-04-08']:
    t = pd.Timestamp(dte)
    vv = vix.asof(t); qv = qin.vol.asof(t); v3v = v3.asof(t) if t >= v3.index[0] else float('nan')
    mq = VolModel.qqq()
    out.append(f"{dte}  {vv:5.1f}   {m.atm(1, vv):.3f}  {m.atm(1.5, vv):.3f}  |  {qv:5.1f}  {mq.atm(1, qv):.3f}  {mq.atm(1.5, qv):.3f} | {v3v:5.1f}")
out.append("參考 (公開文獻/市場記憶, 非本資料集): 2020-03-16 SPX 1 年 ATM IV 約 30–35%; 2008-11 SPX 1 年 ATM IV 約 40–45%. ")
# what kappa reproduces 1y ATM ~33% at VIX 82.7?
for k in [1, 1.5, 2, 3, 4, 5]:
    mm = replace(m, kappa=k)
    out.append(f"  κ={k}: VIX 82.7 → 1y ATM {mm.atm(1, 82.69):.3f}, 1.5y {mm.atm(1.5, 82.69):.3f}; VIX 12 → 1.5y {mm.atm(1.5, 12):.3f}; VIX 20 → 1.5y {mm.atm(1.5, 20):.3f}")
# model VRP: model 1y ATM implied vs realised over the next year
for name, px, vol, mm in [('SPY', spy.SPY, spy.VIX, VolModel.spy()), ('QQQ', qin.adj, qin.vol, VolModel.qqq())]:
    lr = np.log(px).diff()
    rv_fwd = lr[::-1].rolling(252).std()[::-1].shift(-1) * math.sqrt(252)
    iv1 = vol.apply(lambda v: mm.atm(1.0, v))
    # 0.8-delta 1.5y strike IV (skew included)
    d = pd.concat([iv1.rename('iv'), rv_fwd.rename('rv')], axis=1).dropna()
    out.append(f"\n{name}: 模型 1 年 ATM IV 平均 {d.iv.mean():.3f} vs 其後 1 年實現波動 {d.rv.mean():.3f} → 模型 VRP {100*(d.iv-d.rv).mean():+.1f} vol pts (中位 {100*(d.iv-d.rv).median():+.1f})")
    for a, b in [('1999', '2003'), ('2003', '2009'), ('2009', '2020'), ('2020', '2026')]:
        s = d[a:b]
        out.append(f"   {a}-{b}: IV {s.iv.mean():.3f} RV {s.rv.mean():.3f} 差 {100*(s.iv-s.rv).mean():+.1f}")
open('../out/term_structure.txt', 'w').write("\n".join(out)); print("\n".join(out))
