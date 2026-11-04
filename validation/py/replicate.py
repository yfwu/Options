"""Task 1b: independent Python backtest vs run_backtest (same rules) on QQQ 1999-2026 and SPY 1993-2026."""
import sys, json, time
import numpy as np, pandas as pd
sys.path.insert(0, '.')
from optlab_ref import *

OUT = '../out'
res = []
for data, q, vm, cm in [('qqq', 0.007, VolModel.qqq(), Costs(0.006)), ('spy', 0.017, VolModel.spy(), Costs(0.005))]:
    df = load_inputs(f'{OUT}/inputs_{data}.csv', q)
    for name, cfg in [(f'{data}_d80_1.0', Cfg(exposure=1.0, vol=vm, costs=cm)),
                      (f'{data}_d80_1.5', Cfg(exposure=1.5, vol=vm, costs=cm)),
                      (f'{data}_d80_2.0', Cfg(exposure=2.0, vol=vm, costs=cm)),
                      (f'{data}_d60_1.5', Cfg(delta=0.6, exposure=1.5, vol=vm, costs=cm)),
                      (f'{data}_d80_1.0_zero', Cfg(exposure=1.0, vol=vm, costs=Costs(0, 0, 0, 0)))]:
        t0 = time.time()
        py, wiped = backtest(df, cfg, q)
        rs = pd.read_csv(f'{OUT}/curves/{name}.csv', parse_dates=['date']).set_index('date').equity
        a, b = stats(py), stats(rs)
        rel = (py / rs - 1).abs()
        res.append(dict(name=name, py_cagr=a['cagr'], rust_cagr=b['cagr'], d_cagr_bp=(a['cagr'] - b['cagr']) * 1e4,
                        py_mdd=a['mdd'], rust_mdd=b['mdd'], max_rel_dev=rel.max(), median_rel_dev=rel.median(), secs=time.time() - t0))
        py.to_csv(f'{OUT}/curves/{name}_py.csv')
        print(res[-1], flush=True)
pd.DataFrame(res).to_csv(f'{OUT}/replication.csv', index=False)
