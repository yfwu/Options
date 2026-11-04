"""Task 2c: carry (extrinsic) and spreads of real LEAPS (CBOE delayed DOV chain, 2026-09-10)
vs the synthetic model."""
import json, re, sys, math, datetime as dt
import numpy as np, pandas as pd
sys.path.insert(0, '.')
from optlab_ref import *
d = json.load(open('../../crates/data/tests/fixtures/cboe_delayed_DOV_2026-09-10.json'))['data']
S = d['current_price']; asof = dt.date(2026, 9, 10)
r = 0.0408  # DGS3MO 2026-09-09/10 (see data/DGS3MO.csv); filled below if found
for l in open('../../data/DGS3MO.csv'):
    if l.startswith('2026-09-09') or l.startswith('2026-09-10'):
        try: r = float(l.split(',')[1]) / 100
        except: pass
qdiv = 0.011  # DOV trailing yield ~1.0-1.1%
rows = []
for o in d['options']:
    m = re.match(r'DOV(\d{6})([CP])(\d{8})', o['option'])
    exp = dt.datetime.strptime(m.group(1), '%y%m%d').date(); kind = m.group(2); K = int(m.group(3)) / 1000
    if kind != 'C' or o['bid'] <= 0 or o['ask'] <= 0: continue
    t = (exp - asof).days / 365
    mid = 0.5 * (o['bid'] + o['ask'])
    rows.append(dict(exp=exp, t=t, K=K, bid=o['bid'], ask=o['ask'], mid=mid, delta=o['delta'], iv=o['iv'], oi=o['open_interest']))
c = pd.DataFrame(rows)
c['ks'] = c.K / S
c['extr'] = c.mid - (S - c.K).clip(lower=0)
c['extr_pct_spot_yr'] = c.extr / S / c.t
c['carry_pct_dnot_yr'] = c.extr / (c.delta * S) / c.t
c['half_spread_pct'] = (c.ask - c.bid) / 2 / c.mid
out = []
out.append(f"DOV 2026-09-10 S={S}, r={r:.4f}, 假設 q={qdiv}, iv30={d['iv30']}")
leaps = c[(c.t > 0.9)]
out.append(f"到期 >0.9 年: {sorted(leaps.exp.unique())}")
for e, g in leaps.groupby('exp'):
    g = g.iloc[(g.delta - 0.8).abs().argsort()[:3]].sort_values('K')
    out.append(f"\n到期 {e} (T={g.t.iloc[0]:.2f}y)  — 最接近 0.80Δ 的履約價:")
    out.append(g[['K', 'ks', 'bid', 'ask', 'mid', 'delta', 'iv', 'extr', 'extr_pct_spot_yr', 'carry_pct_dnot_yr', 'half_spread_pct', 'oi']].round(4).to_string(index=False))
    # model price at same strike, using the chain's ATM IV as the 'index' level
    atm = g.iv.mean()
# model comparison: SPY-model surface with vol index chosen so that model ATM at 30d = iv30
m = VolModel.spy()
vix_eq = d['iv30'] / m.iv_scale  # so that short IV = iv30
out.append(f"\n模型 (SPY 曲面參數, vol index={vix_eq:.1f} 使 30 天 ATM={d['iv30']}%):")
for e, g in leaps.groupby('exp'):
    t = g.t.iloc[0]
    k = m.strike_for_delta(0.8, S, t, r + 0.003, qdiv, vix_eq)
    v = m.iv(k, S, t, vix_eq)
    p, dl = call_pd(S, k, t, r + 0.003, qdiv, v)
    ext = p - max(S - k, 0)
    # real chain interpolated at model strike
    gg = g.sort_values('K')
    real_mid = np.interp(k, gg.K, gg.mid); real_iv = np.interp(k, gg.K, gg.iv); real_d = np.interp(k, gg.K, gg.delta)
    real_ext = real_mid - max(S - k, 0)
    out.append(f"  {e}: 模型 K={k:.1f} (K/S {k/S:.3f}) IV {v:.3f} 價 {p:.2f} 外在 {ext/S/t*100:.2f}%S/年 | 實際同 K: 中價 {real_mid:.2f} IV {real_iv:.3f} Δ {real_d:.3f} 外在 {real_ext/S/t*100:.2f}%S/年")
# spreads
lc = leaps[(leaps.delta > 0.6) & (leaps.delta < 0.95)]
out.append(f"\n0.6-0.95Δ LEAPS 半價差 (佔中價): 中位數 {lc.half_spread_pct.median()*100:.2f}%  四分位 {lc.half_spread_pct.quantile(.25)*100:.2f}–{lc.half_spread_pct.quantile(.75)*100:.2f}%  (引擎 SPY 0.5%, QQQ 0.6%)")
out.append("注意: DOV 為個股, 流動性遠低於 SPY/QQQ, 價差只能作為上限參考; 引擎價差假設需以 SPY/QQQ 實際 LEAPS 報價驗證 (本環境無法連網取得).")
c.to_csv('../out/dov_chain_calls.csv', index=False)
open('../out/chain_check.txt', 'w').write("\n".join(out)); print("\n".join(out))
