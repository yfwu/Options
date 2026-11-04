"""Generate all harness job files used in the report (except the stress grid: make_stress.py)."""
import json, pandas as pd
O = 'out'
VMQ = dict(iv_scale=0.92, long_run_vol=0.235, kappa=1.0, skew_slope=0.25, smile_curv=0.05, skew_ref_vol=0.22, min_vol=0.08, max_vol=1.8)
VMS = dict(iv_scale=0.90, long_run_vol=0.195, kappa=1.0, skew_slope=0.30, smile_curv=0.05, skew_ref_vol=0.18, min_vol=0.06, max_vol=1.50)
ZERO = dict(half_spread_pct=0, min_half_spread=0, commission_per_contract=0, slippage=0)
def V(b, **k): d = dict(b); d.update(k); return d
def dump(name, obj): json.dump(obj, open(f'{O}/{name}', 'w'), ensure_ascii=False)

# main replication curves
jobs = []
for data in ['qqq', 'spy']:
    for ex in [1.0, 1.5, 2.0]:
        jobs.append(dict(name=f'{data}_d80_{ex}', data=data, cfg=dict(target_exposure=ex), records=f'{O}/curves/{data}_d80_{ex}.csv'))
    jobs.append(dict(name=f'{data}_d60_1.5', data=data, cfg=dict(strike={'Delta': 0.6}), records=f'{O}/curves/{data}_d60_1.5.csv'))
    jobs.append(dict(name=f'{data}_d80_1.0_zero', data=data, cfg=dict(target_exposure=1.0, costs=ZERO), records=f'{O}/curves/{data}_d80_1.0_zero.csv'))
jobs += [dict(name='qqq_k3', data='qqq', cfg=dict(target_exposure=1.0, vol_model=V(VMQ, kappa=3.0)), records=f'{O}/curves/qqq_d80_1.0_k3.csv'),
         dict(name='qqq_const', data='qqq', cfg=dict(target_exposure=1.0, vol_model=V(VMQ, kappa=1000.0)), records=f'{O}/curves/qqq_d80_1.0_const.csv'),
         dict(name='covid', data='qqq', cfg=dict(target_exposure=1.0, start='2020-02-19', end='2020-12-31'), records=f'{O}/curves/qqq_covid_1.0.csv')]
dump('jobs_main.json', jobs)

# attribution variants
jobs = []
for sym, vm in [('qqq', VMQ), ('spy', VMS)]:
    var = [('base', sym, {}), ('zero_costs', sym, dict(costs=ZERO)),
           ('cost_1pct', sym, dict(costs=dict(half_spread_pct=0.01, slippage=0.0025))),
           ('cost_2pct', sym, dict(costs=dict(half_spread_pct=0.02, slippage=0.0025))),
           ('no_cash_yield', sym, dict(cash_yield=False)), ('rate_spread_0.8', sym, dict(option_rate_spread=0.008)),
           ('no_band', sym, dict(rebalance_band=None)), ('band_10', sym, dict(rebalance_band=0.10)),
           ('kappa2', sym, dict(vol_model=V(vm, kappa=2.0))), ('kappa3', sym, dict(vol_model=V(vm, kappa=3.0))),
           ('const_iv', sym, dict(vol_model=V(vm, kappa=1000.0))), ('iv_scale+10%', sym, dict(vol_model=V(vm, iv_scale=vm['iv_scale'] * 1.1))),
           ('lr_vol+3pt', sym, dict(vol_model=V(vm, long_run_vol=vm['long_run_vol'] + 0.03))), ('dgs3mo', sym + ':dgs3mo', {}),
           ('q=0.004' if sym == 'qqq' else 'q=0.02', sym + (':q=0.004' if sym == 'qqq' else ':q=0.02'), {})]
    if sym == 'qqq':
        var += [('real_vxn_2000', 'qqq_bc', {}),
                ('real_vxn+dgs+k3+cost1', 'qqq_bc:dgs3mo', dict(vol_model=V(vm, kappa=3.0), costs=dict(half_spread_pct=0.01, slippage=0.0025))),
                ('real_vxn+dgs+k2', 'qqq_bc:dgs3mo', dict(vol_model=V(vm, kappa=2.0)))]
    else:
        var += [('dgs+k3+cost1', 'spy:dgs3mo', dict(vol_model=V(vm, kappa=3.0), costs=dict(half_spread_pct=0.01, slippage=0.0025)))]
    for per, (s, e) in {'full': (None, None), '2003+': ('2003-01-02', None), '2009-10+': ('2009-10-01', None), 'covid': ('2020-02-19', '2020-12-31'), 'pre2003': (None, '2002-12-31')}.items():
        for nm, data, c in var:
            cfg = dict(target_exposure=1.0); cfg.update(c)
            if s: cfg['start'] = s
            if e: cfg['end'] = e
            jobs.append(dict(name=f'{sym}|{per}|{nm}', data=data, cfg=cfg))
dump('jobs_attr.json', jobs)

# full-period grid
dump('jobs_full_grid.json', [dict(name=f'{s}|{d}|{e}', data=s, cfg=dict(strike={'Delta': d}, target_exposure=e))
                              for s in ['qqq', 'spy'] for d in [0.6, 0.7, 0.8, 0.9] for e in [1.0, 1.25, 1.5, 2.0, 3.0]])

# band variants on the stress starts (needs jobs_stress.json)
st = [j for j in json.load(open(f'{O}/jobs_stress.json')) if '|engine|0.8|' in j['name'] and j['name'].endswith('|540|180')]
out = []
for j in st:
    for band, tag in [(None, 'noband'), (0.10, 'band10'), (0.5, 'band50')]:
        k = json.loads(json.dumps(j)); k['cfg']['rebalance_band'] = band; k['name'] = j['name'] + '|' + tag; out.append(k)
dump('jobs_band.json', out)

# crash curves for fig 5
f = pd.read_csv(f'{O}/stress_flat.csv') if __import__('os').path.exists(f'{O}/stress_flat.csv') else None
peaks = {'2000_網路泡沫': '2000-03-27', '2007_金融海嘯': '2007-10-31', '2020_COVID': '2020-02-19', '2022_升息熊市': '2021-12-27'}
jobs = []
for c, stt in peaks.items():
    end = str((pd.Timestamp(stt) + pd.Timedelta(days=1095 * (2 if c.startswith('2000') else 1))).date())
    for e in [1.0, 2.0, 3.0]:
        jobs.append(dict(name=f'{c}|{e}', data='qqq', cfg=dict(target_exposure=e, start=stt, end=end), records=f'{O}/crash_curves/{c}_{e}.csv'))
dump('jobs_cc.json', jobs)

# bootstrap specs
for sym, vm in [('qqq', VMQ), ('spy', VMS)]:
    cfgs = [(f'd{d}_{e}x', dict(strike={'Delta': d}, target_exposure=e)) for d in [0.6, 0.8, 0.9] for e in [1.0, 1.5, 2.0, 3.0]]
    cfgs += [(f'd0.8_{e}x_k3cost1', dict(target_exposure=e, vol_model=V(vm, kappa=3.0), costs=dict(half_spread_pct=0.01, slippage=0.0025))) for e in [1.5, 2.0, 3.0]]
    for tag, extra in [('uncond', dict(stress_frac=0.0, start_in_stress=False)), ('stress', dict(stress_frac=0.15, start_in_stress=False))]:
        dump(f'boot_{sym}_{tag}.json', dict(data=sym, n=3000, days=756, block=60, seed=11, rate=0.03, configs=cfgs, **extra))
# proxy vs real VXN / rates (§2.5)
dump('jobs_proxy.json', [dict(name=f'{d}|{e}|{p}', data=d, cfg=dict(target_exposure=e, **({'start': a} if a else {}), **({'end': b} if b else {})))
    for d in ['qqq', 'qqq:dgs3mo', 'qqq_bc', 'qqq_bc:dgs3mo', 'qqq_preset', 'qqq_preset:dgs3mo'] for e in [1.0, 1.5, 2.0]
    for p, (a, b) in {'full': (None, None), '1999-2002': (None, '2002-12-31'), 'peak2000+3y': ('2000-03-27', '2003-03-27')}.items()])
print('ok')
