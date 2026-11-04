"""Task 2a (part): same-close execution and crisis spread widening (Python engine only,
these knobs do not exist in RollConfig)."""
import sys
import pandas as pd
sys.path.insert(0, '.')
from optlab_ref import *
rows = []
for data, q, vm, hs, ref in [('qqq', 0.007, VolModel.qqq(), 0.006, 25.0), ('spy', 0.017, VolModel.spy(), 0.005, 20.0)]:
    df = load_inputs(f'../out/inputs_{data}.csv', q)
    bh = df.set_index('date').adj
    for ex in [1.0, 1.5, 2.0]:
        for name, kw in [('base', {}),
                         ('次日收盤成交 (lag 1)', dict(trade_lag=1)),
                         ('危機價差 ∝ vol/ref', dict(costs=Costs(hs, crisis_ref_vol=ref, crisis_exp=1.0))),
                         ('危機價差 ∝ (vol/ref)^2', dict(costs=Costs(hs, crisis_ref_vol=ref, crisis_exp=2.0))),
                         ('lag1 + 危機價差^1', dict(trade_lag=1, costs=Costs(hs, crisis_ref_vol=ref, crisis_exp=1.0)))]:
            cfg = Cfg(exposure=ex, vol=vm, costs=kw.get('costs', Costs(hs)), trade_lag=kw.get('trade_lag', 0))
            for per, (s, e) in {'full': (None, None), 'covid': ('2020-02-19', '2020-12-31')}.items():
                c, w = backtest(df, cfg, q, s, e)
                b = bh[c.index[0]:c.index[-1]]
                st, sb = stats(c), stats(b)
                rows.append(dict(sym=data, exp=ex, per=per, var=name, cagr=st['cagr'] * 100, bh=sb['cagr'] * 100,
                                 excess=(st['cagr'] - sb['cagr']) * 100, mdd=st['mdd'] * 100, tot=(st['final'] - 1) * 100, bh_tot=(sb['final'] - 1) * 100, wiped=w))
                print(rows[-1], flush=True)
pd.DataFrame(rows).to_csv('../out/robust_py.csv', index=False)
