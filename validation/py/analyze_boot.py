"""Block-bootstrap analysis (3-year paths, mean block 60d, joint return + vol-index changes)."""
import json, os
PRE = os.environ.get('BOOT_PRE', 'boot')
import numpy as np, pandas as pd
md = []
summ = []
for sym in ['qqq', 'spy']:
    for tag in ['uncond', 'stress']:
        recs = [json.loads(l) for l in open(f'out/{PRE}_{sym}_{tag}.jsonl')]
        rows = []
        for r in recs:
            base = dict(bh_mdd=r['bh']['mdd'], bh_min=r['bh']['min_eq'], bh_fm=r['bh']['final_mult'])
            rows.append(dict(cfg='B&H', mdd=r['bh']['mdd'], min_eq=r['bh']['min_eq'], fm=r['bh']['final_mult'], wiped=False, **base))
            rows.append(dict(cfg='2x ETF', mdd=r['letf2']['mdd'], min_eq=r['letf2']['min_eq'], fm=r['letf2']['final_mult'], wiped=False, **base))
            rows.append(dict(cfg='3x ETF', mdd=r['letf3']['mdd'], min_eq=r['letf3']['min_eq'], fm=r['letf3']['final_mult'], wiped=False, **base))
            for n, cs, w in r['strats']:
                rows.append(dict(cfg=n, mdd=cs['mdd'], min_eq=cs['min_eq'], fm=cs['final_mult'], wiped=w, **base))
        d = pd.DataFrame(rows)
        for cond_name, cond in [('全部路徑', d.bh_mdd > -9), ('B&H 3 年內 MDD ≤ −30% 的路徑', d.bh_mdd <= -0.30)]:
            s = d[cond]
            g = s.groupby('cfg', sort=False).agg(n=('mdd', 'size'), p_near=('min_eq', lambda x: (x < 0.1).mean()), p_wipe=('wiped', 'mean'),
                                                  mdd_med=('mdd', 'median'), mdd_p5=('mdd', lambda x: x.quantile(0.05)),
                                                  fm_med=('fm', 'median'), fm_p5=('fm', lambda x: x.quantile(0.05)), p_loss50=('fm', lambda x: (x < 0.5).mean()))
            g['sym'], g['tag'], g['cond'] = sym, tag, cond_name
            summ.append(g.reset_index())
            md.append(f"\n#### {sym.upper()} — {'無條件抽樣' if tag=='uncond' else '危機加權抽樣 (15% 新區塊強制從 vol≥35 的日子開始)'} — {cond_name}（n={int(g.n.iloc[0])}）\n")
            md.append("| 配置 | P(權益<10%) | P(期末<50%) | MDD 中位 | MDD 5% 分位 | 3 年終值中位 | 3 年終值 5% 分位 |")
            md.append("|---|---|---|---|---|---|---|")
            for c, x in g.iterrows():
                md.append(f"| {c} | {x.p_near*100:.1f}% | {x.p_loss50*100:.1f}% | {x.mdd_med*100:.0f}% | {x.mdd_p5*100:.0f}% | {x.fm_med:.2f} | {x.fm_p5:.2f} |")
pd.concat(summ).to_csv(f'out/{PRE}_summary.csv', index=False)
open(f'out/{PRE}_tables.md', 'w').write("\n".join(md))
print("\n".join(md))
