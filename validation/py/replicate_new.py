"""Cross-check of the fixed engine (>= 6a0d2a3 defaults: two-factor term structure,
crisis spread widening, exec lag 1, new cost presets) against the Python reference."""
import sys, json, time
import numpy as np, pandas as pd
sys.path.insert(0, '.')
from optlab_ref import *
from dataclasses import replace
OUT = '../out'
TF = (4.0, 0.5, 0.75)
eng = {json.loads(l)['name']: json.loads(l) for l in open(f'{OUT}/newrepl.jsonl')}
res = []
for tag, q, vm, cm in [('qqq', 0.007, replace(VolModel.qqq(), two_factor=TF), Costs(0.0075, crisis_ref_vol=30, crisis_exp=1.0)),
                       ('spy', 0.017, replace(VolModel.spy(), two_factor=TF), Costs(0.006, crisis_ref_vol=25, crisis_exp=1.0)),
                       ('qqq_preset_dgs3mo', 0.007, replace(VolModel.qqq(), two_factor=TF), Costs(0.0075, crisis_ref_vol=30, crisis_exp=1.0)),
                       ('spy_dgs3mo', 0.017, replace(VolModel.spy(), two_factor=TF), Costs(0.006, crisis_ref_vol=25, crisis_exp=1.0))]:
    df = load_inputs(f'{OUT}/inputs_{tag}.csv', q)
    for nm, cfg in [('d80_1.0', Cfg(exposure=1.0, vol=vm, costs=cm, trade_lag=1)),
                    ('d80_2.0', Cfg(exposure=2.0, vol=vm, costs=cm, trade_lag=1)),
                    ('d60_1.5', Cfg(delta=0.6, exposure=1.5, vol=vm, costs=cm, trade_lag=1)),
                    ('d80_1.0_lag0', Cfg(exposure=1.0, vol=vm, costs=cm, trade_lag=0))]:
        name = f'{tag}_{nm}'
        py, wiped = backtest(df, cfg, q)
        rs = pd.read_csv(f'{OUT}/curves/new_{name}.csv', parse_dates=['date']).set_index('date').equity
        a, b = stats(py), stats(rs)
        e = eng[name]
        res.append(dict(name=name, py_cagr=a['cagr'] * 100, rust_cagr=b['cagr'] * 100, d_bp=(a['cagr'] - b['cagr']) * 1e4,
                        py_mdd=a['mdd'] * 100, rust_mdd=b['mdd'] * 100, max_rel_dev=(py / rs - 1).abs().max(),
                        py_min_eq=py.min() / 1e5, engine_min_eq=e['engine_min_equity_ratio'], bh=e['bh']['cagr'] * 100,
                        matched=(e['matched_cagr'] or 0) * 100))
        print({k: (round(v, 4) if isinstance(v, float) else v) for k, v in res[-1].items()}, flush=True)
pd.DataFrame(res).to_csv(f'{OUT}/replication_new.csv', index=False)
