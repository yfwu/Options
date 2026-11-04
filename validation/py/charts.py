"""Charts for docs/validation (dark theme, CJK font)."""
import json, glob, sys
import numpy as np, pandas as pd
import matplotlib
matplotlib.use('Agg')
import matplotlib.pyplot as plt
from matplotlib import font_manager as fm
from matplotlib.colors import LinearSegmentedColormap
sys.path.insert(0, 'py')
from optlab_ref import VolModel
from dataclasses import replace

cjk = [f for f in fm.findSystemFonts() if 'NotoSansCJK' in f or 'NotoSansCJK' in f.replace('-', '')]
for f in cjk: fm.fontManager.addfont(f)
SURF, INK, INK2, GRID = '#1a1a19', '#f0efec', '#c3c2b7', '#3a3a37'
C = ['#3987e5', '#d95926', '#199e70', '#c98500', '#d55181', '#9085e9']
plt.rcParams.update({'font.family': ['Noto Sans CJK TC', 'Noto Sans CJK JP', 'DejaVu Sans'], 'figure.facecolor': SURF, 'axes.facecolor': SURF,
                     'axes.edgecolor': GRID, 'axes.labelcolor': INK2, 'xtick.color': INK2, 'ytick.color': INK2, 'text.color': INK,
                     'axes.grid': True, 'grid.color': GRID, 'grid.linewidth': 0.6, 'axes.spines.top': False, 'axes.spines.right': False,
                     'legend.frameon': False, 'lines.linewidth': 2, 'axes.titleweight': 'bold', 'axes.titlesize': 12, 'font.size': 10,
                     'axes.unicode_minus': False})
OUT = '../docs/validation'

# 1. replication
fig, axes = plt.subplots(2, 2, figsize=(12, 6.5), gridspec_kw={'height_ratios': [3, 1.2]}, sharex='col')
for j, sym in enumerate(['qqq', 'spy']):
    ax, ax2 = axes[0, j], axes[1, j]
    for i, name in enumerate([f'{sym}_d80_1.0', f'{sym}_d80_2.0']):
        rs = pd.read_csv(f'out/curves/{name}.csv', parse_dates=['date']).set_index('date')
        py = pd.read_csv(f'out/curves/{name}_py.csv', parse_dates=[0], index_col=0).iloc[:, 0]
        lab = name.split('_')[-1] + 'x'
        ax.plot(rs.index, rs.equity / 1e5, color=C[i], label=f'Rust {lab}')
        ax.plot(py.index, py / 1e5, color=INK, lw=0.8, ls=(0, (3, 3)), label=f'Python {lab}' if i == 0 else None)
        ax2.plot(rs.index, (py / rs.equity - 1) * 100, color=C[i], lw=1)
        if i == 0:
            ax.plot(rs.index, rs.bh / 1e5, color=C[2], lw=1.2, label='買進持有')
    ax.set_yscale('log'); ax.set_title(f'{sym.upper()} 0.80Δ 540/180：Rust run_backtest vs 獨立 Python 實作')
    ax.set_ylabel('權益（倍數，對數）'); ax.legend(loc='upper left', fontsize=9)
    ax2.set_ylabel('Python/Rust −1 (%)'); ax2.axhline(0, color=INK2, lw=0.8)
fig.tight_layout(); fig.savefig(f'{OUT}/fig1_replication.png', dpi=150); plt.close(fig)

# 2. attribution of QQQ 1.0x excess
a = pd.read_csv('out/attr.csv')
rp = pd.read_csv('out/robust_py.csv')
lab = {'base': '引擎預設', 'zero_costs': '零交易成本', 'cost_1pct': '半價差 1% + 滑價 0.25%', 'cost_2pct': '半價差 2% + 滑價 0.25%',
       'no_cash_yield': '閒置現金不計息', 'rate_spread_0.8': '選擇權利率加碼 0.8%', 'no_band': '不做區間再平衡', 'band_10': '再平衡區間 ±10%',
       'kappa2': '期限結構 κ=2', 'kappa3': '期限結構 κ=3', 'const_iv': 'IV 不隨 VXN 變動', 'iv_scale+10%': 'iv_scale +10%',
       'lr_vol+3pt': '長期 vol +3 點', 'dgs3mo': '真實 DGS3MO 利率', 'q=0.004': 'q=0.4%', 'q=0.02': 'q=2.0%', 'real_vxn_2000': '真實 VXN (2000-10 起)',
       'real_vxn+dgs+k3+cost1': '保守組合（真實VXN+DGS+κ3+成本1%）', 'real_vxn+dgs+k2': '真實VXN+DGS+κ2', 'dgs+k3+cost1': '保守組合（DGS+κ3+成本1%）'}
fig, axes = plt.subplots(1, 2, figsize=(13, 7.5))
for ax, sym in zip(axes, ['qqq', 'spy']):
    s = a[(a.sym == sym) & (a.per == 'full')].copy()
    extra = rp[(rp.sym == sym) & (rp.exp == 1.0) & (rp.per == 'full') & (rp['var'].isin(['次日收盤成交 (lag 1)', '危機價差 ∝ (vol/ref)^2']))]
    s = pd.concat([s[['var', 'excess']].assign(lbl=s['var'].map(lab)), extra[['var', 'excess']].assign(lbl=extra['var'] + '（Python）')])
    s = s.iloc[::-1]
    cols = [C[0] if v >= 0 else C[1] for v in s.excess]
    ax.barh(s.lbl, s.excess, color=cols, height=0.6)
    for y, v in enumerate(s.excess):
        ax.text(v + (0.05 if v >= 0 else -0.05), y, f'{v:+.2f}', va='center', ha='left' if v >= 0 else 'right', color=INK2, fontsize=8)
    lo, hi = s.excess.min(), s.excess.max()
    ax.set_xlim(min(lo, 0) - 0.45, max(hi, 0) + 0.45)
    ax.axvline(0, color=INK2, lw=0.8)
    ax.set_title(f'{sym.upper()} 0.80Δ 1.0x：CAGR 減 買進持有 CAGR（%/年，全期）')
    ax.set_xlabel('超額 CAGR (%/年)')
fig.tight_layout(); fig.savefig(f'{OUT}/fig2_attribution.png', dpi=150); plt.close(fig)

# 3. term structure
D = '../data'
vix = pd.read_csv(f'{D}/VIX_History.csv', parse_dates=['DATE']).set_index('DATE').CLOSE
v3 = pd.read_csv(f'{D}/VIX3M_History.csv', parse_dates=['DATE']).set_index('DATE').CLOSE
x = pd.concat([vix.rename('vix'), v3.rename('v3')], axis=1).dropna()
bins = [9, 12, 14, 16, 18, 20, 23, 26, 30, 35, 40, 50, 90]
x['b'] = pd.cut(x.vix, bins)
g = x.groupby('b', observed=True).apply(lambda s: pd.Series(dict(v=s.vix.median(), real=(s.v3 / s.vix).median(), n=len(s))))
m = VolModel.spy()
fig, axes = plt.subplots(1, 2, figsize=(13, 5))
ax = axes[0]
ax.plot(g.v, g.real, color=C[2], marker='o', ms=6, label='實際 VIX3M / VIX（中位數）')
vv = np.linspace(10, 85, 100)
for i, k in enumerate([1.0, 3.0]):
    mm = replace(m, kappa=k)
    ax.plot(vv, [mm.atm(0.25, v) / mm.atm(30 / 365, v) for v in vv], color=C[0] if k == 1 else C[3], label=f'模型 σ(3m)/σ(1m)，κ={k:g}')
ax.axhline(1, color=INK2, lw=0.8)
ax.set_xlabel('VIX'); ax.set_ylabel('3 個月 / 1 個月'); ax.set_title('短端期限結構：模型 vs VIX3M（2009–2026）'); ax.legend()
ax = axes[1]
for i, k in enumerate([1.0, 2.0, 3.0]):
    mm = replace(m, kappa=k)
    ax.plot(vv, [mm.atm(1.5, v) * 100 for v in vv], color=[C[0], C[4], C[3]][i], label=f'模型 1.5 年 ATM IV，κ={k:g}' + ('（引擎預設）' if k == 1 else ''))
ax.axhspan(30, 40, xmin=(78 - 10) / 75, xmax=(86 - 10) / 75, color=C[2], alpha=0.35)
ax.annotate('2020-03-16 SPX 長天期 ATM IV\n參考區間 ~30–40%（VIX 82.7）', xy=(80, 36), xytext=(28, 42), color=INK2, fontsize=9,
            arrowprops=dict(arrowstyle='->', color=INK2))
ax.set_xlabel('VIX'); ax.set_ylabel('IV (%)'); ax.set_title('SPY 長天期 ATM IV 對 VIX 的反應'); ax.legend(loc='upper left')
fig.tight_layout(); fig.savefig(f'{OUT}/fig3_term_structure.png', dpi=150); plt.close(fig)

# 4. survival heatmaps
cm = LinearSegmentedColormap.from_list('seq', ['#1f2a3a', '#2a5ea8', '#3987e5', '#9cc3f2', '#e6f0fc'])
fig, axes = plt.subplots(2, 2, figsize=(12, 8.5))
for i, sym in enumerate(['qqq', 'spy']):
    for j, scen in enumerate(['engine', 'conservative']):
        s = pd.read_csv(f'out/survival_{sym}_{scen}.csv')
        p = s.pivot(index='exp', columns='delta', values='near') * 100
        w = s.pivot(index='exp', columns='delta', values='med_mdd') * 100
        ax = axes[i, j]
        im = ax.imshow(p.values, cmap=cm, vmin=0, vmax=25, aspect='auto')
        for r in range(p.shape[0]):
            for c in range(p.shape[1]):
                v = p.values[r, c]
                ax.text(c, r, f'{v:.0f}%\nMDD中位 {w.values[r, c]:.0f}%', ha='center', va='center', fontsize=8.5, color='#0b0b0b' if v > 15 else INK)
        ax.set_xticks(range(4)); ax.set_xticklabels([f'Δ{d}' for d in p.columns]); ax.set_yticks(range(5)); ax.set_yticklabels([f'{e}x' for e in p.index])
        ax.grid(False)
        ax.set_title(f"{sym.upper()} P(3 年內權益 < 10%)｜{'引擎假設' if scen == 'engine' else '保守假設'}")
        fig.colorbar(im, ax=ax, fraction=0.04, label='%')
fig.suptitle('危機壓力測試：10 次危機 × 4 個起點 × 9 種 DTE 組合（每格 n=360）', color=INK)
fig.tight_layout(); fig.savefig(f'{OUT}/fig4_survival.png', dpi=150); plt.close(fig)

# 5. crash paths
fig, axes = plt.subplots(2, 2, figsize=(13, 8))
for ax, c in zip(axes.flat, ['2000_網路泡沫', '2007_金融海嘯', '2020_COVID', '2022_升息熊市']):
    for i, e in enumerate([1.0, 2.0, 3.0]):
        r = pd.read_csv(f'out/crash_curves/{c}_{e}.csv', parse_dates=['date']).set_index('date')
        ax.plot(r.index, r.equity / r.equity.iloc[0], color=C[i], label=f'LEAPS 0.80Δ {e:g}x')
        if i == 0:
            bh = r.bh / r.bh.iloc[0]
            ax.plot(r.index, bh, color=INK2, lw=1.2, label='買進持有 QQQ')
            ret = bh.pct_change().fillna(0)
            l3 = (1 + 3 * ret - 2 * (0.03 + 0.005) / 252 - 0.0095 / 252).clip(lower=0).cumprod()
            ax.plot(r.index, l3, color=C[3], lw=1.2, ls=(0, (4, 2)), label='3x 每日再平衡（近似）')
    ax.axhline(0.1, color=C[4], lw=0.8, ls=':')
    ax.set_yscale('log'); ax.set_ylim(0.004, 3)
    ax.set_title(f'{c.replace("_", " ")}：自高點建倉')
    ax.set_ylabel('權益倍數（對數）')
axes[0, 0].legend(fontsize=8.5, loc='lower left')
fig.tight_layout(); fig.savefig(f'{OUT}/fig5_crash_paths.png', dpi=150); plt.close(fig)

# 6. bootstrap min-equity CDF
fig, axes = plt.subplots(1, 2, figsize=(13, 5))
for ax, sym in zip(axes, ['qqq', 'spy']):
    recs = [json.loads(l) for l in open(f'out/boot_{sym}_stress.jsonl')]
    series = {'買進持有': [r['bh']['min_eq'] for r in recs], '3x ETF': [r['letf3']['min_eq'] for r in recs]}
    for n in ['d0.8_1.0x', 'd0.8_1.5x', 'd0.8_2.0x', 'd0.8_3.0x']:
        series[f'LEAPS {n[1:4]}Δ {n.split("_")[1]}'] = [next(cs['min_eq'] for nn, cs, w in r['strats'] if nn == n) for r in recs]
    cols = [INK2, C[3], C[2], C[0], C[5], C[1]]
    for (k, v), col in zip(series.items(), cols):
        v = np.sort(v)
        ax.plot(v, np.arange(1, len(v) + 1) / len(v) * 100, color=col, label=k, ls=(0, (4, 2)) if 'ETF' in k else '-')
    ax.axvline(0.1, color=C[4], lw=0.8, ls=':')
    ax.set_xscale('log'); ax.set_xlabel('3 年內最低權益 / 初始資金（對數）'); ax.set_ylabel('累積機率 (%)')
    ax.set_title(f'{sym.upper()} 區塊拔靴（危機加權，n=3000）：最低權益分布')
    ax.legend(fontsize=8.5, loc='upper left')
fig.tight_layout(); fig.savefig(f'{OUT}/fig6_bootstrap.png', dpi=150); plt.close(fig)
print('ok')
