"""Jobs for the 修正後 (after-fix, engine >= 6a0d2a3) re-runs."""
import json, itertools
import pandas as pd
O = 'out'
def dump(n, o): json.dump(o, open(f'{O}/{n}', 'w'), ensure_ascii=False)
LEG = lambda sym: dict(vol_model=dict(term_model="SingleKappa"), costs=dict(half_spread_pct=0.006 if sym == 'qqq' else 0.005, crisis_ref_vol=0.0, crisis_exp=0.0), exec_lag_days=0)
CONS = dict(costs=dict(half_spread_pct=0.01, min_half_spread=0.05, commission_per_contract=0.65, slippage=0.0025, crisis_ref_vol=25.0, crisis_exp=1.5))
def merge(a, b):
    a = json.loads(json.dumps(a))
    for k, v in b.items():
        a[k] = merge(a[k], v) if isinstance(v, dict) and isinstance(a.get(k), dict) else v
    return a
# 1. fix-by-fix attribution
jobs = []
for sym in ['qqq', 'spy']:
    leg = LEG(sym)
    newdata = 'qqq_preset:dgs3mo' if sym == 'qqq' else 'spy:dgs3mo'
    var = [('A 舊引擎（修正前）', sym, leg),
           ('A + 雙因子期限結構', sym, merge(leg, dict(vol_model=dict(term_model={"TwoFactor": {"fast_kappa": 4.0, "slow_kappa": 0.5, "fast_weight": 0.75}})))),
           ('A + 新成本預設（含危機放大）', sym, merge(leg, dict(costs=dict(half_spread_pct=0.0075 if sym == 'qqq' else 0.006, crisis_ref_vol=30.0 if sym == 'qqq' else 25.0, crisis_exp=1.0)))),
           ('A + 隔日成交', sym, merge(leg, dict(exec_lag_days=1))),
           ('B 新引擎預設（舊資料）', sym, {}),
           ('C 新引擎 + 新資料（真實VXN/DGS3MO）', newdata, {}),
           ('D = C + 保守成本', newdata, CONS)]
    for e in [1.0, 1.5, 2.0]:
        for per, (s, en) in {'full': (None, None), 'covid': ('2020-02-19', '2020-12-31'), '2009-10+': ('2009-10-01', None)}.items():
            for nm, data, c in var:
                cfg = merge(c, dict(target_exposure=e))
                if s: cfg['start'] = s
                if en: cfg['end'] = en
                jobs.append(dict(name=f'{sym}|{per}|{e}|{nm}', data=data, cfg=cfg))
dump('jobs_after_attr.json', jobs)
# 2. crash grid with the new engine: 'engine' = new defaults + new data, 'conservative' = + conservative costs
st = json.load(open(f'{O}/jobs_stress.json'))
out = []
for j in st:
    sym, crash, off, scen, d, e, ed, rd = j['name'].split('|')
    base = {k: j['cfg'][k] for k in ['strike', 'target_exposure', 'entry_dte', 'roll_dte', 'start']}
    data = 'qqq_preset:dgs3mo' if sym == 'qqq' else 'spy:dgs3mo'
    out.append(dict(name=j['name'], data=data, cfg=merge(base, CONS) if scen == 'conservative' else base, horizon_days=1095))
dump('jobs_stress_after.json', out)
# 3. bootstrap with the new defaults (vol path from the new data)
for sym in ['qqq', 'spy']:
    data = 'qqq_preset:dgs3mo' if sym == 'qqq' else 'spy:dgs3mo'
    cfgs = [(f'd{d}_{e}x', dict(strike={'Delta': d}, target_exposure=e)) for d in [0.6, 0.8, 0.9] for e in [1.0, 1.5, 2.0, 3.0]]
    cfgs += [(f'd0.8_{e}x_k3cost1', merge(CONS, dict(target_exposure=e))) for e in [1.5, 2.0, 3.0]]  # label kept for table compatibility: = conservative costs
    for tag, extra in [('uncond', dict(stress_frac=0.0, start_in_stress=False)), ('stress', dict(stress_frac=0.15, start_in_stress=False))]:
        dump(f'boot_after_{sym}_{tag}.json', dict(data=data, n=3000, days=756, block=60, seed=11, rate=0.03, configs=cfgs, **extra))
print(len(jobs), len(out))
