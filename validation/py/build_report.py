"""Assemble docs/validation/REPORT.md from report_template.md + generated tables."""
import re, shutil
import pandas as pd
t = open('report_template.md').read()
r = pd.read_csv('out/replication.csv')
t = t.replace('{REPL}', "\n".join(f"| {x['name']} | {x.rust_cagr*100:.2f}% | {x.py_cagr*100:.2f}% | {x.d_cagr_bp:+.1f} | {x.rust_mdd*100:.1f}% | {x.py_mdd*100:.1f}% |" for _, x in r.iterrows()))
md = open('out/stress_tables.md').read()
secs = {s.split('\n')[0]: s for s in re.split(r'\n(?=### )', md) if s.strip()}
pick = lambda pref: [v.replace('### ', '#### ', 1) for k, v in secs.items() if k.startswith(pref)]
t = t.replace('{PEAK_TABLES}', "格式：3 年內 MDD / 最低權益（倍數）/ 回本年數（💀 = 引擎 wipe-out）\n\n" + "\n\n".join(pick('### QQQ：於高點') + pick('### SPY：於高點')))
t = t.replace('{SURVIVAL_TABLES}', "\n\n".join(pick('### QQQ 存活表') + pick('### SPY 存活表')))
b = pd.read_csv('out/boot_summary.csv')
names = {'B&H': '買進持有', '2x ETF': '2x ETF', '3x ETF': '3x ETF', 'd0.8_1.0x': 'LEAPS 0.8Δ 1.0x', 'd0.8_1.5x': 'LEAPS 0.8Δ 1.5x', 'd0.8_2.0x': 'LEAPS 0.8Δ 2.0x',
         'd0.8_3.0x': 'LEAPS 0.8Δ 3.0x', 'd0.8_2.0x_k3cost1': 'LEAPS 0.8Δ 2.0x（保守）', 'd0.8_3.0x_k3cost1': 'LEAPS 0.8Δ 3.0x（保守）'}
rows = []
for tag, cond, lab in [('uncond', '全部路徑', '無條件，全部'), ('stress', 'B&H 3 年內 MDD ≤ −30% 的路徑', '危機加權，B&H MDD≤−30%')]:
    s = b[(b.sym == 'qqq') & (b.tag == tag) & (b.cond == cond)]
    for c, n in names.items():
        x = s[s.cfg == c].iloc[0]
        rows.append(f"| {lab}（n={x.n}） | {n} | {x.p_near*100:.1f}% | {x.p_loss50*100:.1f}% | {x.mdd_med*100:.0f}% | {x.fm_med:.2f} |")
t = t.replace('{BOOT}', "\n".join(rows))
open('../docs/validation/REPORT.md', 'w').write(t)
shutil.copy('out/stress_tables.md', '../docs/validation/stress_tables.md')
shutil.copy('out/boot_tables.md', '../docs/validation/boot_tables.md')
print('REPORT.md', len(t))
