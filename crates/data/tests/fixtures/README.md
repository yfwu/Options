# Test fixtures (optlab-data)

All files are **real API responses captured by third parties** and published on
GitHub; they are used only to unit-test the parsers. Nothing here is fabricated.

| File | Provider / endpoint | Captured | Source |
|---|---|---|---|
| `cboe_delayed_DOV_2026-09-10.json` | CBOE `delayed_quotes/options/DOV.json` (454 contracts, 7 expiries) | timestamp 2026-09-10 03:09:50 UTC | `chenyan159/investment` — `备份/20家公司投资优先级_2026-09-10/市场原始核验/DOV_cboe.json` |
| `yahoo_v7_options_QQQ_2025-02-24.json` | Yahoo `v7/finance/options/QQQ` (nearest expiry only, weekend capture: bid/ask = 0) | 2025-02-24 09:24 UTC | `nurulislamz/Stocky` — `stockytests/Integration/YahooMockCalls/options-QQQ.json`; only `response.bodyJson` kept, the request cookies/headers/crumb were dropped |
| `yahoo_v8_chart_SPY_5y_2026-03-24.json` | Yahoo `v8/finance/chart/SPY?range=5y&interval=1d` incl. dividends and a live intraday last row | 2026-03-24 | `46923335x/satvalue` — `data/yahoo/SPY.json` |
| `polygon_snapshot_NVDA_2025-09-29.json` | Polygon `v3/snapshot/options/NVDA?limit=10` (plan without `last_quote`) | 2025-09-29 | `calebndavidson-del/Options-trading-` — `polygon_nvda_snapshot.json` |
| `tradier_chain_PLTR_2024-08-26_rewrapped.json` | Tradier `markets/options/chains` option objects (10 PLTR puts, ORATS greeks) | 2024-08-26 20:00 | `bcdannyboy/STOC-D` — `jspreads.json`. The 10 option objects are verbatim real Tradier objects, **re-wrapped** in the documented `{"options":{"option":[...]}}` envelope (the source stored them inside spread records). |

Doc-derived samples (not captured) exist only inline in `tests/parsers.rs`
(`tradier_doc_shapes`) and are labelled there.
