"""Task 1a: compare pricing.rs / synth.rs grids against the scipy reference."""
import math, sys
import numpy as np, pandas as pd
sys.path.insert(0, '.')
from optlab_ref import bs_price, bs_greeks, implied_vol, strike_for_call_delta, VolModel, call_pd
from scipy.stats import norm

OUT = '../out'
g = pd.read_csv(f'{OUT}/rust_pricing.csv')
rows = []
for _, x in g.iterrows():
    p, de, ga, th, ve, rh = bs_greeks(x.kind, x.s, x.k, x.t, x.r, x.q, x.sigma)
    rows.append(dict(p=p, de=de, ga=ga, th=th, ve=ve, rh=rh))
ref = pd.DataFrame(rows)
res = {}
for col, rc in [('p', 'price'), ('de', 'delta'), ('ga', 'gamma'), ('th', 'theta'), ('ve', 'vega'), ('rh', 'rho')]:
    err = (g[rc] - ref[col]).abs()
    res[rc] = (err.max(), (err / ref[col].abs().clip(lower=1e-12)).where(ref[col].abs() > 1e-6).max())
# IV round trip: only where vega meaningful (price > 1e-8 and vega > 1e-6)
mask = (ref.ve > 1e-4) & (ref.p > 1e-6)
iv_err = (g.iv - g.sigma).abs()[mask[:len(g)]]
lines = ["# 定價核對 (pricing.rs vs scipy)", f"格點數: {len(g)}"]
for k, (a, r) in res.items():
    lines.append(f"{k:6s} max|abs err| = {a:.2e}   max rel err (|ref|>1e-6) = {r:.2e}")
lines.append(f"IV 反解 (vega>1e-4 per pt): max |iv - sigma| = {iv_err.max():.2e}, NaN 數 = {int(g.iv[mask].isna().sum())}")
# deep ITM / tiny-vega IV cases
bad = g[mask & ((g.iv - g.sigma).abs() > 1e-6)]
lines.append(f"  |iv-sigma|>1e-6 的格點: {len(bad)}")
if len(bad):
    lines.append(bad[['kind','k','t','r','q','sigma','price','iv']].head(8).to_string())
# strike_for_delta
calls = g[g.kind == 'Call']
kd_err = []
for _, x in calls.iterrows():
    k = x.k_for_delta80
    if not np.isfinite(k): continue
    d = call_pd(x.s, k, x.t, x.r, x.q, x.sigma)[1]
    kd_err.append(abs(d - 0.8))
lines.append(f"strike_for_delta(0.80) 反算 delta 誤差 max = {max(kd_err):.2e}")
# Hull textbook
hull = g.tail(4)
lines.append("\nHull 教科書數值:")
lines.append(hull[['kind','s','k','t','r','sigma','price','delta','gamma','theta','vega','rho']].to_string(index=False))
lines.append("Hull 例 15.6: c=4.76, p=0.81；例 19.x (S=49,K=50,T=20/52,r=5%,σ=20%): c≈2.40, Δ=0.522, Γ=0.066, Θ=-4.31/年 (=-0.0118/日), vega=12.1/1.00 (=0.121/vol pt), ρ=8.91/1.00 (=0.0891/pt)")
# American put binomial sanity: done in rust tests already
# synthetic surface
sy = pd.read_csv(f'{OUT}/rust_synth.csv')
mx_iv = mx_atm = mx_k = mx_d = 0
for _, x in sy.iterrows():
    m = VolModel.spy() if x.model == 'spy' else VolModel.qqq()
    q = 0.017 if x.model == 'spy' else 0.007
    mx_atm = max(mx_atm, abs(m.atm(x.t, x.vix) - x.atm))
    mx_iv = max(mx_iv, abs(m.iv(100 * x.k_over_s, 100, x.t, x.vix) - x.iv))
    kref = m.strike_for_delta(0.8, 100, x.t, 0.03, q, x.vix)
    mx_k = max(mx_k, abs(kref - x.kd80))
    mx_d = max(mx_d, abs(x.delta_kd80 - 0.8))
lines.append(f"\n合成曲面: max|ATM 差| = {mx_atm:.1e}, max|IV 差| = {mx_iv:.1e}")
lines.append(f"strike_for_delta(曲面): Rust 阻尼不動點 vs 精確求根 max|ΔK| = {mx_k:.4f} (S=100), Rust 履約價的實際 delta 偏差 max = {mx_d:.4f}")
worst = sy.assign(dd=(sy.delta_kd80 - 0.8).abs()).sort_values('dd', ascending=False).drop_duplicates(['model','vix','t']).head(6)
lines.append(worst[['model','vix','t','kd80','delta_kd80']].to_string(index=False))
open(f'{OUT}/pricing_check.txt', 'w').write("\n".join(lines))
print("\n".join(lines))
