"""Task 3: crash stress-test job grid."""
import json, itertools
import pandas as pd
CRASH = {  # name: search window for the pre-crash peak
    '2000 網路泡沫': ('1999-12-01', '2000-04-15'),
    '2007 金融海嘯': ('2007-07-01', '2008-01-15'),
    '2011-08 美債降評': ('2011-04-01', '2011-08-01'),
    '2015-08 人民幣': ('2015-05-01', '2015-08-15'),
    '2016-01': ('2015-11-01', '2016-01-05'),
    '2018-02 Volmageddon': ('2018-01-10', '2018-02-02'),
    '2018-Q4': ('2018-08-15', '2018-10-05'),
    '2020 COVID': ('2020-01-15', '2020-02-25'),
    '2022 升息熊市': ('2021-11-01', '2022-01-10'),
    '2025-02 關稅衝擊': ('2025-01-15', '2025-02-25'),
}
OFFS = {'高點': 0, '前1月': 30, '前3月': 91, '前6月': 182}
vmq = dict(iv_scale=0.92, long_run_vol=0.235, kappa=1.0, skew_slope=0.25, smile_curv=0.05, skew_ref_vol=0.22, min_vol=0.08, max_vol=1.8)
vms = dict(iv_scale=0.90, long_run_vol=0.195, kappa=1.0, skew_slope=0.30, smile_curv=0.05, skew_ref_vol=0.18, min_vol=0.06, max_vol=1.50)
SCEN = {  # engine defaults vs 'conservative' (more realistic) assumptions
    'engine': lambda sym: ('qqq' if sym == 'qqq' else 'spy', {}),
    'conservative': lambda sym: ('qqq_bc:dgs3mo' if sym == 'qqq' else 'spy:dgs3mo',
                                 dict(vol_model=dict(vmq if sym == 'qqq' else vms, kappa=3.0), costs=dict(half_spread_pct=0.01, slippage=0.0025))),
}
jobs, meta = [], []
for sym in ['qqq', 'spy']:
    px = pd.read_csv(f'out/inputs_{sym}.csv', parse_dates=['date']).set_index('date').adj
    for cname, (a, b) in CRASH.items():
        if sym == 'spy' and cname == '2016-01':
            pass
        peak = px[a:b].idxmax()
        for oname, off in OFFS.items():
            start = px.index[px.index.searchsorted(peak - pd.Timedelta(days=off))]
            for scen, f in SCEN.items():
                data, extra = f(sym)
                for d, e, ed, rd in itertools.product([0.6, 0.7, 0.8, 0.9], [1.0, 1.25, 1.5, 2.0, 3.0], [365, 540, 730], [90, 180, 270]):
                    cfg = dict(strike={'Delta': d}, target_exposure=e, entry_dte=ed, roll_dte=rd, start=str(start.date()))
                    cfg.update(extra)
                    jobs.append(dict(name=f'{sym}|{cname}|{oname}|{scen}|{d}|{e}|{ed}|{rd}', data=data, cfg=cfg, horizon_days=1095))
json.dump(jobs, open('out/jobs_stress.json', 'w'))
print(len(jobs))
