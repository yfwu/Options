"""Task 3 analysis: tables (markdown) + charts from out/stress.jsonl."""
import json, sys
import numpy as np, pandas as pd
rows = []
for l in open('out/stress.jsonl'):
    o = json.loads(l)
    sym, crash, off, scen, d, e, ed, rd = o['name'].split('|')
    h = o['h']
    rows.append(dict(sym=sym, crash=crash, off=off, scen=scen, delta=float(d), exp=float(e), entry=int(ed), roll=int(rd),
                     start=o['start'], mdd=h[0]['mdd'], min_eq=h[0]['min_eq'], end3=h[0]['end_mult'], rec=h[0]['days_to_recover_initial'],
                     bh_mdd=h[1]['mdd'], bh_min=h[1]['min_eq'], bh_rec=h[1]['days_to_recover_initial'],
                     l2_mdd=h[2]['mdd'], l2_min=h[2]['min_eq'], l2_rec=h[2]['days_to_recover_initial'],
                     l3_mdd=h[3]['mdd'], l3_min=h[3]['min_eq'], l3_rec=h[3]['days_to_recover_initial'],
                     wiped=o['wiped'], data_end=o['end']))
df = pd.DataFrame(rows)
df.to_csv('out/stress_flat.csv', index=False)
CR = list(dict.fromkeys(df.crash))
def yrs(x):
    return '未回本' if pd.isna(x) else f"{x/365.25:.1f}"
md = []
# Table 1: per crash, peak start, 0.8Δ 540/180 engine & conservative, by exposure; vs B&H/2x/3x
for sym in ['qqq', 'spy']:
    md.append(f"\n### {sym.upper()}：於高點建倉，0.80Δ 540/180 天，3 年內最大回撤 / 最低權益 / 回本年數\n")
    md.append("| 危機 | 起點 | B&H | 2x ETF | 3x ETF | LEAPS 1.0x | 1.5x | 2.0x | 3.0x | 3.0x (保守假設) |")
    md.append("|---|---|---|---|---|---|---|---|---|---|")
    for c in CR:
        s = df[(df.sym == sym) & (df.crash == c) & (df.off == '高點') & (df.delta == 0.8) & (df.entry == 540) & (df.roll == 180)]
        if s.empty: continue
        b = s.iloc[0]
        cell = lambda m, mn, r: f"{m*100:.0f}% / {mn:.2f} / {yrs(r)}"
        row = [c, b.start, cell(b.bh_mdd, b.bh_min, b.bh_rec), cell(b.l2_mdd, b.l2_min, b.l2_rec), cell(b.l3_mdd, b.l3_min, b.l3_rec)]
        for e in [1.0, 1.5, 2.0, 3.0]:
            x = s[(s.exp == e) & (s.scen == 'engine')].iloc[0]
            row.append(cell(x.mdd, x.min_eq, x.rec) + (' 💀' if x.wiped else ''))
        x = s[(s.exp == 3.0) & (s.scen == 'conservative')].iloc[0]
        row.append(cell(x.mdd, x.min_eq, x.rec) + (' 💀' if x.wiped else ''))
        md.append("| " + " | ".join(row) + " |")
# Table 2: survival by delta x exposure
for sym in ['qqq', 'spy']:
    for scen in ['engine', 'conservative']:
        s = df[(df.sym == sym) & (df.scen == scen)]
        g = s.groupby(['exp', 'delta']).agg(near=('min_eq', lambda x: (x < 0.10).mean()), wipe=('wiped', 'mean'),
                                            worst_mdd=('mdd', 'min'), med_mdd=('mdd', 'median'), p_dd70=('mdd', lambda x: (x < -0.7).mean()),
                                            unrec=('rec', lambda x: x.isna().mean()))
        g.to_csv(f'out/survival_{sym}_{scen}.csv')
        md.append(f"\n### {sym.upper()} 存活表（{ '引擎預設假設' if scen=='engine' else ('保守假設：真實 VXN(2000-10 起)/DGS3MO/κ=3/半價差 1%+滑價 0.25%' if sym=='qqq' else '保守假設：DGS3MO/κ=3/半價差 1%+滑價 0.25%') }）\n")
        md.append(f"樣本 = {len(CR) if sym=='qqq' else len(CR)} 次危機 × 4 個起點 × 3 entry DTE × 3 roll DTE（每格 n={int(len(s)/20)}）。格式：P(權益<10%) / 最差 3 年 MDD / 中位 MDD / P(到資料結束仍未回本)\n")
        md.append("| 曝險 \\ Δ | 0.6 | 0.7 | 0.8 | 0.9 |")
        md.append("|---|---|---|---|---|")
        for e in [1.0, 1.25, 1.5, 2.0, 3.0]:
            cells = []
            for d in [0.6, 0.7, 0.8, 0.9]:
                x = g.loc[(e, d)]
                cells.append(f"{x.near*100:.0f}% / {x.worst_mdd*100:.0f}% / {x.med_mdd*100:.0f}% / {x.unrec*100:.0f}%")
            md.append(f"| {e}x | " + " | ".join(cells) + " |")
        bm = s.drop_duplicates(['crash', 'off'])
        md.append(f"\n對照（同樣 {len(bm)} 個起點）：B&H P(<10%) {np.mean(bm.bh_min<0.1)*100:.0f}% 最差 MDD {bm.bh_mdd.min()*100:.0f}% 中位 {bm.bh_mdd.median()*100:.0f}% 未回本 {bm.bh_rec.isna().mean()*100:.0f}%；"
                  f"2x：{np.mean(bm.l2_min<0.1)*100:.0f}% / {bm.l2_mdd.min()*100:.0f}% / {bm.l2_mdd.median()*100:.0f}% / {bm.l2_rec.isna().mean()*100:.0f}%；"
                  f"3x：{np.mean(bm.l3_min<0.1)*100:.0f}% / {bm.l3_mdd.min()*100:.0f}% / {bm.l3_mdd.median()*100:.0f}% / {bm.l3_rec.isna().mean()*100:.0f}%")
# Table 3: DTE effect (QQQ, engine, excluding dot-com and incl.)
s = df[(df.sym == 'qqq') & (df.scen == 'engine')]
md.append("\n### QQQ：entry/roll DTE 對危機表現的影響（中位 3 年 MDD，所有 Δ、危機、起點；引擎假設）\n")
t = s.groupby(['exp', 'entry', 'roll']).mdd.median().unstack(['entry', 'roll']) * 100
md.append(t.round(0).to_markdown())
t2 = s[s.crash != '2000 網路泡沫'].groupby(['exp', 'entry', 'roll']).mdd.median().unstack(['entry', 'roll']) * 100
md.append("\n（排除 2000 網路泡沫）\n")
md.append(t2.round(0).to_markdown())
# Table 4: does start offset matter (QQQ 2000 & 2008)
md.append("\n### QQQ 起點敏感度（0.80Δ 540/180，引擎假設）：3 年 MDD / 最低權益\n")
md.append("| 危機 | 起點 | 1.0x | 1.5x | 2.0x | 3.0x | B&H | 3x ETF |")
md.append("|---|---|---|---|---|---|---|---|")
for c in CR:
    for o in ['前6月', '前3月', '前1月', '高點']:
        x = s[(s.crash == c) & (s.off == o) & (s.delta == 0.8) & (s.entry == 540) & (s.roll == 180)]
        if x.empty: continue
        r = [c, f"{o} ({x.start.iloc[0]})"] + [f"{x[x.exp==e].mdd.iloc[0]*100:.0f}% / {x[x.exp==e].min_eq.iloc[0]:.2f}" for e in [1.0, 1.5, 2.0, 3.0]]
        r += [f"{x.bh_mdd.iloc[0]*100:.0f}% / {x.bh_min.iloc[0]:.2f}", f"{x.l3_mdd.iloc[0]*100:.0f}% / {x.l3_min.iloc[0]:.2f}"]
        md.append("| " + " | ".join(r) + " |")
open('out/stress_tables.md', 'w').write("\n".join(md))
print("\n".join(md[:60]))
