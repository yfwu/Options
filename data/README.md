# Bundled historical data

These CSV files are compiled into the `optlab-data` binary (`crates/data/src/bundled.rs`,
via `include_str!`) so OptLab works offline. They were collected from **public
third-party GitHub mirrors** of Yahoo Finance, CBOE, Barchart and FRED data — not
from the original vendors — and spot-checked against known values. Treat them as a
convenient fallback: on your own machine refresh with the live providers
(`DataService::fetch_history`, `fetch_vol_index`, `fetch_rate`), which are tried first.
Vendor terms of use apply to the underlying data.

Last bundled trading day overall: **2026-11-03**.

## Prices

| File | Coverage | Columns | Source |
|---|---|---|---|
| `QQQ_10y.csv`, `NVDA_10y.csv`, `MSFT_10y.csv`, `GOOG_10y.csv`, `AMZN_10y.csv`, `META_10y.csv`, `AVGO_10y.csv`, `TSM_10y.csv`, `VTI_10y.csv`, `SOXX_10y.csv` | 2016-11-03 .. 2026-11-03 (2513 rows each) | `date,open,high,low,close,volume,div,split` | <https://github.com/yennanliu/finance_data> (Yahoo Finance) |
| `QQQ_adj_1999.csv` | 1999-03-10 .. 2026-09-18 | `Date,adj_close` | <https://github.com/iqueipopg/backtest-overfitting> (Yahoo adjusted close) |
| `SPY_2019.csv` | 2019-01-02 .. 2026-11-03 | `Date,Adj Close,Close,High,Low,Open,Volume` | <https://github.com/fja05680/brownbear> (Yahoo) up to 2026-08-28; rows 2026-08-31 .. 2026-11-03 appended from <https://github.com/B4L1/daily-notes> (`data/prices/SPY.csv`, OHLCV) and <https://github.com/mano000may/Hackathon-Project> (`data/SPY.csv`, adjusted close) |
| `VIX_SPY_1993.csv` | 1993-01-29 .. 2026-04-24 | `Date,VIX,SPY` (SPY = adjusted close) | <https://github.com/vivek-v-rao/Conditional-Skew> |

Adjustments and caveats:

* **10y files**: OHLC and volume are already **split-adjusted** (e.g. NVDA 4:1 in 2021 and
  10:1 in 2024 show no jump; the `split` column only marks the event). `close` is **not**
  dividend-adjusted; `div` holds the cash dividend (split-adjusted units) on the ex-date.
  `bundled.rs` rebuilds `adj_close` Yahoo-style (all earlier bars × `1 − div/prev_close`).
  The rebuilt QQQ adjusted close matches the independent `QQQ_adj_1999.csv` to within
  0.5 % over 2016–2026 (tested).
* **QQQ** (`bundled::prices("QQQ")`): `QQQ_adj_1999.csv` spliced in front of `QQQ_10y.csv`
  at 2016-11-03. Before the seam only an adjusted close exists, so bars are synthetic:
  `adj_close` = long series scaled to join the rebuilt adjusted close, `open=high=low=close`
  = long series scaled to join the unadjusted close, `volume = 0`. Pre-2016 `close` is
  therefore a dividend-adjusted proxy (≈10 % below the true split-adjusted price in 1999);
  returns are exact, absolute levels are not.
* **SPY** (`bundled::prices("SPY")`): the `SPY` column of `VIX_SPY_1993.csv` spliced in
  front of `SPY_2019.csv` at 2019-01-02 the same way (pre-2019 `close` is a proxy, about
  40 % below the true 1993 price because of 26 years of dividends). The appended rows
  were verified: B4L1 closes equal brownbear closes on all 1925 overlapping days and
  mano000may adjusted closes equal brownbear `Adj Close` to 1e-6. Known check: unadjusted
  close 2020-03-23 = 222.95.

## Volatility indices (vol points)

| File | Coverage | Columns | Source |
|---|---|---|---|
| `VIX_History.csv` | 1990-01-02 .. 2026-10-30 | CBOE format `DATE (m/d/Y),OPEN,HIGH,LOW,CLOSE` | <https://github.com/sabsab0808-IA/trade-data> (`data_ext/cboe/VIX_History.csv`, a copy of CBOE's file). Replaces the earlier copy from GeorgiosLoulakis/Adv-Business-Analytics (ended 2026-04-17), which is identical on all 9166 overlapping days |
| `VIX_close_recent.csv` | 2026-01-02 .. 2026-11-03 | `Date,VIX` | <https://github.com/KKOO245/OPTION-ALERT> (`data/vix_history/VIX.csv`, Yahoo ^VIX closes); used only for days after the CBOE file |
| `VXN_History.csv` | 2009-09-14 .. 2026-10-20 | CBOE format | <https://github.com/rellis3/MacroFXModel> (CBOE file) |
| `VXN_barchart_2000.csv` | 2000-10-10 .. 2026-11-03 | Barchart export `Time,Open,High,Low,Last,Change,%Chg,Volume` | <https://github.com/smallfishmacro-Git/market-dashboard> (`data/barchart/Nasdaq_100_VIX_$VXN.csv`) |
| `VIX3M_History.csv` | 2009-09-18 .. 2026-10-30 | CBOE format | <https://github.com/sabsab0808-IA/trade-data> |

* `bundled::vol_index("VIX")` = CBOE close, holes/recency filled from the `VIX` column of
  `VIX_SPY_1993.csv` and `VIX_close_recent.csv`. Checks: 2020-03-16 = 82.69, 2008-11-20 = 80.86.
* `bundled::vol_index("VXN")` = CBOE VXN close, with Barchart closes for 2000-10-10 .. 2009-09-11
  and after 2026-10-20 (Barchart agrees with CBOE on 4288 of 4296 overlapping days).
  Pre-2009 values may follow CBOE's original (VXO-style) VXN methodology. The Barchart
  record has gaps (2000-11-06 .. 2001-01-23, 2006-05-31 .. 2006-06-29).
* **VXN proxy** (`bundled::default_vol_index_for("QQQ")`, `vxn_with_proxy`, `proxy_vxn_from_vix`):
  before 2000-10-10 VXN = VIX × `VXN_VIX_RATIO_EARLY` = **2.13**, the median VXN/VIX ratio over
  2000-10-10 .. 2002-12-31 (502 days; dot-com era, the regime closest to 1999–2000). Gaps
  longer than 7 days are filled with VIX × the median ratio of the 20 preceding VXN days.
  For reference the median ratio is 2.54 over the first year (2000-10..2001-10), 1.34 over
  2003–2008, **1.17** over 2009-09 .. 2026-10 (`VXN_VIX_RATIO_FULL`) and **1.24** over
  2016 .. 2026 (`VXN_VIX_RATIO_RECENT`). The proxy is an approximation; prefer real VXN.

## Rates

| File | Coverage | Columns | Source |
|---|---|---|---|
| `DGS3MO.csv` | 1981-09-01 .. 2026-11-02 | FRED `observation_date,DGS3MO` (percent; empty cells on holidays) | <https://github.com/achl0902-web/back-testing> (`qld/data/macro/DGS3MO.csv`), identical to two other FRED mirrors on all overlapping days |

`bundled::rate("DGS3MO")` returns percent (4.21 = 4.21 %); `DataService::risk_free_rate()`
returns a decimal. Checks: 2020-04-01 = 0.09, 2023-07-31 = 5.55.
