//! 建議引擎（繁體中文）：長期看多、以滾動 LEAPS 買權取得穩健槓桿 beta。
//!
//! * [`MarketSnapshot::from_history`] — 由價格與波動率指數計算目前市場狀態。
//! * [`advise`] / [`advise_with`] — 依市場狀態、最佳化結果與目前設定給出量化建議。
//! * [`strategy_conclusions`] — 內建「常見算法的綜合策略結論」知識庫。
//! * 輔助計算：[`leaps_carry_table`]、[`stress_test`]、[`kelly_leverage`]、[`expected_log_growth`]。
//!
//! 所有數字均為模型估算（合成 IV 曲面 + Black-Scholes），非投資建議。

use chrono::NaiveDate;
use serde::{Deserialize, Serialize};

use crate::backtest::{RollConfig, StrikeSelection};
use crate::chain::OptionKind;
use crate::history::{PriceSeries, ScalarSeries};
use crate::optimize::OptRow;
use crate::synth::SynthVolModel;

// ---------------------------------------------------------------------------
// Market snapshot
// ---------------------------------------------------------------------------

/// 目前市場狀態。無法計算的欄位為 NaN。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct MarketSnapshot {
    pub symbol: String,
    pub date: NaiveDate,
    /// 最新（調整後）收盤價。
    pub spot: f64,
    /// 最新波動率指數（點數，例如 VIX 18.5）。
    pub vol_now: f64,
    pub vol_mean_1y: f64,
    /// IV Rank（近一年 (目前−最低)/(最高−最低)，0..1）。
    pub iv_rank_1y: f64,
    /// IV 百分位（近一年低於目前值的比例，0..1）。
    pub iv_percentile_1y: f64,
    /// 20/60 日實現波動率（年化，小數）。
    pub rv20: f64,
    pub rv60: f64,
    /// 波動率風險溢酬：vol_now/100 − rv20。
    pub vrp: f64,
    pub sma50: f64,
    pub sma200: f64,
    pub above_sma200: bool,
    /// 200 日均線近 20 日的變化率。
    pub sma200_slope: f64,
    /// 距歷史高點回撤（≤ 0）。
    pub drawdown_from_high: f64,
    /// 距 52 週高點回撤（≤ 0）。
    pub drawdown_from_52w_high: f64,
    pub ret_1m: f64,
    pub ret_3m: f64,
    pub ret_6m: f64,
    pub ret_12m: f64,
    /// 趨勢分數 −1（強空）..+1（強多）。
    pub trend_score: f64,
}

fn sma_last(x: &[f64], n: usize) -> f64 {
    if x.len() < n || n == 0 {
        return f64::NAN;
    }
    x[x.len() - n..].iter().sum::<f64>() / n as f64
}

fn ret_back(x: &[f64], k: usize) -> f64 {
    if x.len() <= k {
        return f64::NAN;
    }
    x[x.len() - 1] / x[x.len() - 1 - k] - 1.0
}

impl MarketSnapshot {
    /// 由價格歷史（使用 adj_close）與波動率指數計算。
    pub fn from_history(prices: &PriceSeries, vol_index: &ScalarSeries) -> MarketSnapshot {
        let mut s = MarketSnapshot { symbol: prices.symbol.clone(), ..Default::default() };
        let px = prices.adj_closes();
        let nan = f64::NAN;
        if px.is_empty() {
            s.spot = nan;
            s.vol_now = vol_index.last().map_or(nan, |p| p.1);
            return s;
        }
        s.date = prices.last_date().unwrap();
        s.spot = *px.last().unwrap();
        s.vol_now = vol_index.value_at(s.date).unwrap_or(nan);
        // vol stats over the trailing year (up to the snapshot date)
        let vpts: Vec<f64> = vol_index.points.iter().filter(|p| p.0 <= s.date).map(|p| p.1).collect();
        let w = &vpts[vpts.len().saturating_sub(252)..];
        if w.len() >= 20 {
            let mn = w.iter().copied().fold(f64::INFINITY, f64::min);
            let mx = w.iter().copied().fold(f64::NEG_INFINITY, f64::max);
            s.iv_rank_1y = if mx > mn { (s.vol_now - mn) / (mx - mn) } else { 0.5 };
            s.iv_percentile_1y = w.iter().filter(|&&v| v < s.vol_now).count() as f64 / (w.len() - 1).max(1) as f64;
            s.vol_mean_1y = w.iter().sum::<f64>() / w.len() as f64;
        } else {
            s.iv_rank_1y = nan;
            s.iv_percentile_1y = nan;
            s.vol_mean_1y = nan;
        }
        s.rv20 = prices.realized_vol(20).unwrap_or(nan);
        s.rv60 = prices.realized_vol(60).unwrap_or(nan);
        s.vrp = s.vol_now / 100.0 - s.rv20;
        s.sma50 = sma_last(&px, 50);
        s.sma200 = sma_last(&px, 200);
        s.above_sma200 = s.sma200.is_finite() && s.spot > s.sma200;
        s.sma200_slope = if px.len() >= 220 {
            let prev = px[px.len() - 220..px.len() - 20].iter().sum::<f64>() / 200.0;
            s.sma200 / prev - 1.0
        } else {
            nan
        };
        let ath = px.iter().copied().fold(f64::NEG_INFINITY, f64::max);
        s.drawdown_from_high = s.spot / ath - 1.0;
        let h52 = px[px.len().saturating_sub(252)..].iter().copied().fold(f64::NEG_INFINITY, f64::max);
        s.drawdown_from_52w_high = s.spot / h52 - 1.0;
        s.ret_1m = ret_back(&px, 21);
        s.ret_3m = ret_back(&px, 63);
        s.ret_6m = ret_back(&px, 126);
        s.ret_12m = ret_back(&px, 252);
        let c = |x: f64| if x.is_finite() { x.clamp(-1.0, 1.0) } else { 0.0 };
        s.trend_score = 0.35 * c((s.spot / s.sma200 - 1.0) / 0.10)
            + 0.25 * c((s.sma50 / s.sma200 - 1.0) / 0.05)
            + 0.25 * c(s.ret_12m / 0.20)
            + 0.15 * c(s.ret_3m / 0.10);
        s
    }
}

// ---------------------------------------------------------------------------
// Advice types & assumptions
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AdviceLevel {
    /// 資訊
    Info,
    /// 建議
    Suggest,
    /// 注意
    Warning,
    /// 風險
    Risk,
}

impl AdviceLevel {
    pub fn label(&self) -> &'static str {
        match self {
            AdviceLevel::Info => "資訊",
            AdviceLevel::Suggest => "建議",
            AdviceLevel::Warning => "注意",
            AdviceLevel::Risk => "風險",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Advice {
    pub level: AdviceLevel,
    pub title: String,
    pub body: String,
}

fn adv(level: AdviceLevel, title: impl Into<String>, body: impl Into<String>) -> Advice {
    Advice { level, title: title.into(), body: body.into() }
}

/// 建議引擎所用的長期假設。
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct AdvisorAssumptions {
    /// 預期股票超額報酬（相對無風險利率，算術年化）。
    pub equity_premium: f64,
    /// 目前無風險利率（小數）。
    pub rate: f64,
    pub dividend_yield: f64,
    /// 長期年化波動率假設（用於 Kelly 的中性估計）。
    pub long_run_vol: f64,
}

impl Default for AdvisorAssumptions {
    fn default() -> Self {
        Self { equity_premium: 0.05, rate: 0.037, dividend_yield: 0.013, long_run_vol: 0.17 }
    }
}

fn is_nasdaq(sym: &str) -> bool {
    let s = sym.to_ascii_uppercase();
    ["QQQ", "NDX", "QLD", "TQQQ", "XND", "NQ", "MNQ", "QQQM"].iter().any(|x| s.contains(x))
}

impl AdvisorAssumptions {
    /// 依標的給預設值：那斯達克 100 類較高波動、較高溢酬與較低股息。
    pub fn for_symbol(sym: &str) -> Self {
        if is_nasdaq(sym) {
            Self { equity_premium: 0.06, rate: 0.037, dividend_yield: 0.006, long_run_vol: 0.23 }
        } else {
            Self::default()
        }
    }
}

// ---------------------------------------------------------------------------
// Quantitative helpers
// ---------------------------------------------------------------------------

/// 連續時間 Kelly 槓桿：f* = (μ − r) / σ²。
pub fn kelly_leverage(excess_return: f64, sigma: f64) -> f64 {
    if sigma <= 0.0 {
        return f64::INFINITY;
    }
    excess_return / (sigma * sigma)
}

/// 固定槓桿 L 的預期對數成長率：r + L·(μ−r) − L²σ²/2（未計成本）。
pub fn expected_log_growth(lev: f64, excess_return: f64, rate: f64, sigma: f64) -> f64 {
    rate + lev * excess_return - 0.5 * lev * lev * sigma * sigma
}

/// LEAPS 成本/槓桿表的一列。
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct CarryRow {
    pub delta: f64,
    pub dte: u32,
    /// 履約價 / 現價。
    pub strike_pct: f64,
    pub iv: f64,
    /// 權利金 / 現價。
    pub price_pct: f64,
    /// 時間價值 / 現價 / 年。
    pub extrinsic_pct_annual: f64,
    /// 時間價值 / (Δ·S) / 年：每單位曝險的年化持有成本。
    pub carry_pct_of_exposure: f64,
    /// −θ·365 / 權利金：權利金的年化時間耗損率（當下速度）。
    pub theta_pct_of_price_annual: f64,
    /// 每 1 vol 點的權利金變化 / 權利金。
    pub vega_pct_of_price: f64,
    /// 槓桿倍數 Δ·S / 權利金（每 1 元權利金帶來的曝險）。
    pub leverage: f64,
}

/// 以合成 IV 曲面計算一組 delta × DTE 的 LEAPS 成本表。
pub fn leaps_carry_table(model: &SynthVolModel, spot: f64, vol_index_pts: f64, rate: f64, q: f64, deltas: &[f64], dtes: &[u32]) -> Vec<CarryRow> {
    let mut out = Vec::new();
    for &d in deltas {
        for &dte in dtes {
            let t = dte as f64 / 365.0;
            let k = model.strike_for_delta(OptionKind::Call, d, spot, t, rate, q, vol_index_pts);
            let (g, iv) = model.greeks(OptionKind::Call, spot, k, t, rate, q, vol_index_pts);
            let price = g.price.max(1e-12);
            let ext = (g.price - (spot - k).max(0.0)).max(0.0);
            out.push(CarryRow {
                delta: g.delta,
                dte,
                strike_pct: k / spot,
                iv,
                price_pct: g.price / spot,
                extrinsic_pct_annual: ext / spot / t,
                carry_pct_of_exposure: ext / (g.delta * spot) / t,
                theta_pct_of_price_annual: -g.theta * 365.0 / price,
                vega_pct_of_price: g.vega / price,
                leverage: g.delta * spot / price,
            });
        }
    }
    out
}

/// 壓力測試結果一列（帳戶權益變化，小數）。
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct StressRow {
    /// 指數跌幅（0.35 = −35%）。
    pub drop: f64,
    /// 衝擊後的波動率指數。
    pub vol_after: f64,
    /// 衝擊發生所經過的天數。
    pub days: u32,
    /// LEAPS 部位（含現金）權益變化。
    pub leaps_change: f64,
    /// 每日再平衡 L× 的近似變化 (1−d)^L − 1（未計波動拖累）。
    pub lev_etf_change: f64,
    /// 不槓桿持有的變化。
    pub unlevered_change: f64,
    /// 衝擊後的實際曝險倍數。
    pub exposure_after: f64,
}

/// 以設定（delta、DTE、目標曝險、權利金上限）在今天建倉後，模擬指數下跌衝擊。
pub fn stress_test(cfg: &RollConfig, spot: f64, vol_now: f64, rate: f64, q: f64, scenarios: &[(f64, f64, u32)]) -> Vec<StressRow> {
    let vm = &cfg.vol_model;
    let r = rate + cfg.option_rate_spread;
    let t = cfg.entry_dte as f64 / 365.0;
    let k = match cfg.strike {
        StrikeSelection::Delta(d) => vm.strike_for_delta(OptionKind::Call, d, spot, t, r, q, vol_now),
        StrikeSelection::Moneyness(m) => m * spot,
    };
    let (g, _) = vm.greeks(OptionKind::Call, spot, k, t, r, q, vol_now);
    let price = g.price.max(1e-12);
    let n = (cfg.target_exposure / (g.delta * spot).max(1e-12)).min(cfg.max_premium_pct / price);
    let cash = 1.0 - n * price;
    scenarios
        .iter()
        .map(|&(drop, vol_after, days)| {
            let s2 = spot * (1.0 - drop);
            let t2 = (t - days as f64 / 365.0).max(1.0 / 365.0);
            let (g2, _) = vm.greeks(OptionKind::Call, s2, k, t2, r, q, vol_after);
            let p2 = g2.price.max(s2 - k).max(0.0);
            let e2 = cash * (rate * days as f64 / 365.0).exp() + n * p2;
            StressRow {
                drop,
                vol_after,
                days,
                leaps_change: e2 - 1.0,
                lev_etf_change: (1.0 - drop).powf(cfg.target_exposure) - 1.0,
                unlevered_change: -drop,
                exposure_after: if e2 > 0.0 { n * g2.delta * s2 / e2 } else { 0.0 },
            }
        })
        .collect()
}

fn pc(x: f64) -> String {
    if x.is_finite() {
        format!("{:.1}%", x * 100.0)
    } else {
        "—".into()
    }
}

fn model_for(sym: &str, current: Option<&RollConfig>) -> SynthVolModel {
    match current {
        Some(c) => c.vol_model,
        None if is_nasdaq(sym) => SynthVolModel::qqq(),
        None => SynthVolModel::spy(),
    }
}

// ---------------------------------------------------------------------------
// advise
// ---------------------------------------------------------------------------

/// 依市場狀態、最佳化最佳列與目前設定產生建議（使用依標的的預設假設）。
pub fn advise(snap: &MarketSnapshot, best: Option<&OptRow>, current: Option<&RollConfig>) -> Vec<Advice> {
    advise_with(snap, best, current, &AdvisorAssumptions::for_symbol(&snap.symbol))
}

/// 同 [`advise`]，可自訂長期假設。
pub fn advise_with(snap: &MarketSnapshot, best: Option<&OptRow>, current: Option<&RollConfig>, a: &AdvisorAssumptions) -> Vec<Advice> {
    use AdviceLevel::*;
    let mut out = Vec::new();
    let sym = if snap.symbol.is_empty() { "標的".to_string() } else { snap.symbol.clone() };
    let nasdaq = is_nasdaq(&sym);
    let vm = model_for(&sym, current);
    let default_cfg = if nasdaq { RollConfig::qqq() } else { RollConfig::spy() };
    let cfg = current.cloned().unwrap_or(default_cfg);
    let vol_ok = snap.vol_now.is_finite() && snap.vol_now > 0.0;
    let vol_now = if vol_ok { snap.vol_now } else if nasdaq { 22.0 } else { 18.0 };
    let spot = if snap.spot.is_finite() && snap.spot > 0.0 { snap.spot } else { 100.0 };
    let r = a.rate;
    let q = a.dividend_yield;

    // 0. 市場狀態摘要
    out.push(adv(
        Info,
        format!("{sym} 市場狀態（{}）", snap.date),
        format!(
            "收盤 {:.2}；波動率指數 {:.1}（一年均值 {:.1}，IV Rank {}，IV 百分位 {}）；實現波動 20 日 {} / 60 日 {}；\
             200 日均線 {:.2}（{}，斜率 {}）；距歷史高點 {}、距 52 週高點 {}；近 1/3/12 個月報酬 {} / {} / {}；趨勢分數 {:+.2}（−1 強空 ~ +1 強多）。",
            snap.spot, snap.vol_now, snap.vol_mean_1y, pc(snap.iv_rank_1y), pc(snap.iv_percentile_1y), pc(snap.rv20), pc(snap.rv60),
            snap.sma200, if snap.above_sma200 { "價格在均線之上" } else { "價格在均線之下" }, pc(snap.sma200_slope),
            pc(snap.drawdown_from_high), pc(snap.drawdown_from_52w_high), pc(snap.ret_1m), pc(snap.ret_3m), pc(snap.ret_12m), snap.trend_score
        ),
    ));

    // 1. 進場時機 vs IV
    let ref_row = leaps_carry_table(&vm, spot, vol_now, r + cfg.option_rate_spread, q, &[0.80], &[540]);
    let rr = ref_row[0];
    let ivr = snap.iv_rank_1y;
    if ivr.is_finite() {
        let vega_txt = format!(
            "以模型估算，0.80Δ、約 18 個月的買權目前 IV ≈ {}、權利金 ≈ 現價的 {}，IV 每變動 1 點權利金約變動 {}；時間價值年化約為現價的 {}（每單位曝險 {}／年）。",
            pc(rr.iv), pc(rr.price_pct), pc(rr.vega_pct_of_price), pc(rr.extrinsic_pct_annual), pc(rr.carry_pct_of_exposure)
        );
        if ivr < 0.25 && snap.vol_now < snap.vol_mean_1y * 1.05 {
            out.push(adv(
                Suggest,
                "IV 處於一年低檔：LEAPS 相對便宜",
                format!(
                    "IV Rank {}。低 IV 時買進長天期買權的 vega 成本最低，適合新建倉或「提前展期」（把即將到展期門檻的部位提早換成新的 18–24 個月合約，鎖住便宜的波動率）。\
                     注意低 IV 也常伴隨市場自滿；可改用較低 delta（0.70–0.75）以較少的權利金取得同樣曝險，保留更多現金緩衝。{vega_txt}",
                    pc(ivr)
                ),
            ));
        } else if ivr > 0.70 || snap.vol_now > 30.0 {
            out.push(adv(
                Warning,
                "IV 偏高：LEAPS 權利金昂貴",
                format!(
                    "IV Rank {}、波動率指數 {:.1}。此時買進的時間價值與 vega 都偏貴，若之後 IV 回落，即使指數不動部位也會虧損。建議：\
                     (1) 分 2–4 批建倉；(2) 選更深價內（0.85–0.90Δ）降低 vega 與時間價值占比；(3) 目前部位若 DTE 仍 > 150 天，可延後展期等 IV 回落；\
                     (4) 依波動目標原則，暫時把目標曝險降低約 {:.2} 倍（σ²拖累與 σ 成正比放大）。{vega_txt}",
                    pc(ivr),
                    snap.vol_now,
                    (1.0 - (snap.vol_mean_1y / snap.vol_now).clamp(0.4, 1.0)) * cfg.target_exposure
                ),
            ));
        } else {
            out.push(adv(Info, "IV 位於一年區間中段", format!("IV Rank {}，無明顯的貴/便宜訊號；照紀律於展期門檻展期即可。{vega_txt}", pc(ivr))));
        }
    }
    if snap.vrp.is_finite() {
        if snap.vrp > 0.06 {
            out.push(adv(
                Info,
                "隱含波動遠高於實現波動",
                format!("波動率指數 {:.1} 比 20 日實現波動 {} 高 {:.1} 點，選擇權相對「貴」：買方長期支付此波動率風險溢酬（VRP），這是 LEAPS 相對融資槓桿的隱性成本之一。", snap.vol_now, pc(snap.rv20), snap.vrp * 100.0),
            ));
        } else if snap.vrp < -0.02 {
            out.push(adv(
                Info,
                "實現波動高於隱含波動",
                format!("20 日實現波動 {} 已高於波動率指數 {:.1}，通常出現在急跌中；此時買方的 gamma 收益可抵銷時間價值，但方向風險極大，請先確認現金緩衝。", pc(snap.rv20), snap.vol_now),
            ));
        }
    }

    // 2. Delta 選擇
    let table = leaps_carry_table(&vm, spot, vol_now, r + cfg.option_rate_spread, q, &[0.60, 0.70, 0.80, 0.90], &[540]);
    let mut lines = String::new();
    for row in &table {
        lines.push_str(&format!(
            "Δ{:.2}: K/S {:.2}、IV {}、權利金 {} 現價、槓桿 {:.1}x、時間價值 {}/年（每單位曝險）、vega {}；",
            row.delta,
            row.strike_pct,
            pc(row.iv),
            pc(row.price_pct),
            row.leverage,
            pc(row.carry_pct_of_exposure),
            pc(row.vega_pct_of_price)
        ));
    }
    out.push(adv(
        Suggest,
        "Delta 選擇：0.70–0.85 為股票替代的甜蜜點",
        format!(
            "{lines} 深價內（0.85–0.90Δ）時間價值與 vega 少、行為接近股票，但每單位曝險要付較多權利金、現金緩衝較少；較低 delta（0.60–0.70）權利金少、凸性大（跌時曝險自動下降、漲時上升），\
             但時間價值與 IV 敏感度高。本引擎 1993–2026 SPY 與 1999–2026 QQQ 的回測中 0.60–0.80Δ 差異不大，0.90Δ 以上反而較差（權利金占用資金多、凸性少）。\
             實務上建議 0.70–0.80Δ，並以「目標曝險」而非「口數」決定部位大小。"
        ),
    ));

    // 3. DTE / 展期
    let dte_table = leaps_carry_table(&vm, spot, vol_now, r + cfg.option_rate_spread, q, &[0.80], &[730, 540, 365, 180, 90, 45]);
    let mut dl = String::new();
    for row in &dte_table {
        dl.push_str(&format!("{} 天：權利金時間耗損 {}/年；", row.dte, pc(row.theta_pct_of_price_annual)));
    }
    out.push(adv(
        Suggest,
        "到期天數與展期時機",
        format!(
            "同為 0.80Δ 買權的當下時間耗損速度（−θ×365/權利金）：{dl} 剩餘 6 個月以內 theta 明顯加速、gamma 上升，股票替代的特性變差。\
             建議建倉 12–24 個月（365–730 天），剩餘 120–240 天時展期到新的 LEAPS；優先選擇 1、6、12 月等成交量大的 LEAPS 月份，以價差單（同時賣舊買新）一次完成。"
        ),
    ));

    // 4. 曝險 vs Kelly 與波動拖累
    let sigma_mkt = vm.atm_vol(1.0, vol_now);
    let sigma = 0.5 * sigma_mkt + 0.5 * a.long_run_vol;
    let kelly = kelly_leverage(a.equity_premium, sigma);
    let mut g_txt = String::new();
    for l in [1.0, 1.5, 2.0, 2.5, 3.0] {
        g_txt.push_str(&format!("{l:.1}x：拖累 {}、預期對數成長 {}；", pc(0.5 * l * l * sigma * sigma), pc(expected_log_growth(l, a.equity_premium, r, sigma))));
    }
    let tgt = cfg.target_exposure;
    let level = if tgt > 1.5 * kelly { Risk } else if tgt > kelly { Warning } else if tgt > 0.5 * kelly { Info } else { Suggest };
    out.push(adv(
        level,
        format!("曝險倍數 vs Kelly：目前目標 {tgt:.2}x，全 Kelly ≈ {kelly:.2}x、半 Kelly ≈ {:.2}x", kelly * 0.5),
        format!(
            "假設超額報酬 μ−r = {}、σ = {}（市場 1 年 IV {} 與長期 {} 平均），Kelly f* = (μ−r)/σ² ≈ {kelly:.2}。波動拖累 ≈ L²σ²/2：{g_txt} \
             超過全 Kelly 後長期複利反而下降，且回撤急遽擴大；參數估計誤差（μ 很難估準）讓實務上多採半 Kelly 或更低。\
             穩健槓桿 beta 建議 {} 曝險約 0.6–0.9 倍 Kelly，即 {:.1}–{:.1}x。",
            pc(a.equity_premium),
            pc(sigma),
            pc(sigma_mkt),
            pc(a.long_run_vol),
            sym,
            (0.6 * kelly).clamp(0.5, 1.5),
            (0.9 * kelly).clamp(0.8, 2.0)
        ),
    ));

    // 5. 回撤存活數學 / 壓力測試
    let scen = [(0.20, (vol_now * 1.6).max(30.0), 60u32), (0.35, (vol_now * 2.3).max(45.0), 120), (0.50, (vol_now * 3.0).max(60.0), 250)];
    let st = stress_test(&cfg, spot, vol_now, r, q, &scen);
    let mut s_txt = String::new();
    for row in &st {
        s_txt.push_str(&format!(
            "指數 −{:.0}%（{} 天內、波動率升至 {:.0}）：LEAPS 帳戶 {}，每日再平衡 {:.1}x 約 {}，未槓桿 {}，衝擊後曝險 {:.2}x；",
            row.drop * 100.0,
            row.days,
            row.vol_after,
            pc(row.leaps_change),
            tgt,
            pc(row.lev_etf_change),
            pc(row.unlevered_change),
            row.exposure_after
        ));
    }
    let worst = st.iter().map(|r| r.leaps_change).fold(0.0, f64::min);
    out.push(adv(
        if worst < -0.6 { Risk } else { Warning },
        "回撤存活壓力測試",
        format!(
            "以目前設定 {}（今天建倉）模擬：{s_txt} （此為建倉後不再調整的情境：買權凸性使損失以權利金為上限、曝險自動下降；若依再平衡區間或向下展期重新加槓桿，之後若續跌損失會更大——回測中的最大回撤即包含這種路徑。）回本所需漲幅 = 1/(1−回撤) − 1：−50% 需 +100%、−75% 需 +300%、−90% 需 +900%。\
             歷史上 2000–2002 年 NDX 跌 −83%、2008 年 SPX 跌 −55%、2022 年 NDX 跌 −35%；長期槓桿的首要目標是「活下來」——請確認最壞情境下仍不會被迫出場。",
            cfg.label()
        ),
    ));

    // 6. 現金緩衝
    let prem_pct = (tgt / rr.leverage).min(cfg.max_premium_pct);
    out.push(adv(
        Suggest,
        "現金緩衝與資金配置",
        format!(
            "以 0.80Δ 18 個月買權取得 {tgt:.2}x 曝險約需投入權益的 {} 作為權利金，其餘 {} 留作現金，請放在短期美國公債/T-Bill（或 SGOV、BIL 等）賺取無風險利率——這是 LEAPS 槓桿的另一半報酬。\
             買權的最大損失就是權利金，不會追繳保證金；但請另外保留 6–12 個月生活費在投資帳戶以外，並避免在 LEAPS 之上再疊加融資。",
            pc(prem_pct),
            pc(1.0 - prem_pct)
        ),
    ));

    // 7. 200 日均線 / 趨勢
    if snap.sma200.is_finite() {
        if snap.above_sma200 {
            out.push(adv(
                Info,
                "多頭格局：價格位於 200 日均線之上",
                format!(
                    "趨勢分數 {:+.2}。歷史上指數在 200 日均線之上時波動較低、平均報酬較高，維持目標曝險即可；若距均線過遠（目前 {}），短線回檔機率上升，可分批而非一次建倉。",
                    snap.trend_score,
                    pc(snap.spot / snap.sma200 - 1.0)
                ),
            ));
        } else {
            out.push(adv(
                Warning,
                "空頭/修正格局：價格跌破 200 日均線",
                format!(
                    "趨勢分數 {:+.2}、距歷史高點 {}。均線之下的期間波動率約為之上的 1.5–2 倍，槓桿的波動拖累與深度回撤大多發生在此區（Gayed & Bilello 2016）。\
                     可考慮把目標曝險降至 1.0x 附近或改用更深價內的買權；但完全出場有錯過 V 型反彈的成本（如 2020 年），且 LEAPS 的買賣價差使頻繁切換代價高，建議以「降低曝險」取代「全部出場」。",
                    snap.trend_score,
                    pc(snap.drawdown_from_high)
                ),
            ));
        }
    }
    if snap.drawdown_from_high.is_finite() && snap.drawdown_from_high < -0.20 {
        out.push(adv(
            Info,
            "已進入熊市區（距高點超過 −20%）",
            "熊市中 IV 高、權利金貴，但長期預期報酬通常較好。若部位 delta 已明顯下降（價外化），應「向下展期」把履約價移到目標 delta，恢復股票替代特性，而非持有低 delta 的價外買權等待。".to_string(),
        ));
    }

    // 8. 流動性與交易執行
    out.push(adv(
        Suggest,
        "流動性與下單執行",
        "優先使用 SPY、QQQ、SPX（或小型 XSP）月選擇權的 LEAPS（每月第三個週五到期，1 月、6 月、12 月未平倉量最大）。深價內 LEAPS 的買賣價差常為權利金的 0.3–1.5%，\
         一律使用限價單從中價開始掛、逐步讓價；展期時用「價差組合單」同時賣舊買新，避免單腳成交風險；避開開盤後與收盤前 15 分鐘。\
         本引擎預設半價差 0.5%（SPY）/0.6%（QQQ），價差每增加 1%，年化報酬約下降 0.5–1 個百分點。"
            .to_string(),
    ));

    // 9. 稅務與合約結構
    out.push(adv(
        Info,
        "SPX/XSP 與 SPY/QQQ 的合約差異與稅務（美國稅務居民）",
        "SPX/XSP/NDX 為歐式、現金交割、無提前履約風險；對美國納稅人適用 Section 1256（60% 長期 / 40% 短期資本利得、年底按市價計稅）。\
         SPY/QQQ 為美式、實物交割，持有深價內買權在除息日前可能被認為應提前履約（僅影響賣方；買方持有不受強制）。\
         合約名目：SPX 約 指數×100（數十萬美元），XSP 為其 1/10，NDX 名目更大；小資金建議 QQQ/SPY/XSP 以便以整數口數接近目標曝險。"
            .to_string(),
    ));
    out.push(adv(
        Info,
        "股息：買權不領股息，但已反映在價格中",
        format!(
            "買權持有人不會收到 {sym} 的股息（目前約 {}），但選擇權以遠期價格定價，股息已讓買權變便宜，因此並非額外成本（本引擎回測驗證：股息率改變對 LEAPS 年化報酬影響很小）。\
             對台灣居民而言反而有一項好處：直接持有美國 ETF 的股息會被預扣 30% 稅，LEAPS 則沒有股息、也就沒有這筆預扣，每年約省下股息率×30% 的拖累（約 {}）。",
            pc(q),
            pc(q * 0.3)
        ),
    ));
    out.push(adv(
        Warning,
        "台灣居民稅務提醒（請以專業意見為準）",
        "一般而言，非美國居民外國人透過券商交易美股選擇權的資本利得，通常不需繳美國所得稅、也不會被預扣（股息才會被預扣 30%）；但需填妥 W-8BEN。\
         台灣方面，海外所得（含海外證券及選擇權交易所得）屬最低稅負制的基本所得額：一申報戶全年海外所得達新台幣 100 萬元起須全數計入，\
         基本所得額超過扣除額（近年約新台幣 750 萬元，依物價調整）的部分按 20% 計算，並與一般所得稅比較擇高。\
         另外，美國遺產稅對非居民僅約 6 萬美元免稅額，美國券商帳戶內的美國資產可能適用，選擇權是否屬美國境內財產有爭議。以上僅為概括說明，請依個人情況諮詢會計師或稅務顧問。"
            .to_string(),
    ));

    // 10. 最佳化結果
    if let Some(b) = best {
        let s = &b.stats;
        out.push(adv(
            Suggest,
            format!("最佳化結果：{}", b.config.label()),
            format!(
                "回測 {} → {}：CAGR {}、年化波動 {}、Sharpe {:.2}、最大回撤 {}（{} 到 {}）、最差年度 {}、展期 {} 次、平均曝險 {:.2}x{}。\
                 提醒：歷史最佳參數容易過度擬合；本引擎的 walk-forward 測試顯示「每期重新最佳化」並不能穩定勝過合理的預設參數。\
                 請優先挑選位於參數「高原」（鄰近參數表現也不錯）的設定，並以 bootstrap 檢查回撤分布與破產機率。",
                s.start,
                s.end,
                pc(s.cagr),
                pc(s.ann_vol),
                s.sharpe,
                pc(s.max_drawdown),
                s.max_dd_start,
                s.max_dd_trough,
                s.worst_year.map_or("—".into(), |(y, r)| format!("{y} 年 {}", pc(r))),
                b.n_rolls,
                b.avg_exposure,
                if b.wiped_out { "（曾歸零！）" } else { "" }
            ),
        ));
        if s.max_drawdown < -0.6 {
            out.push(adv(
                Risk,
                "最佳化設定的歷史最大回撤超過 60%",
                format!("最大回撤 {}，需 +{} 才能回本；多數人在這種回撤中會放棄策略。建議改用「回撤上限下最大化 CAGR」目標或降低曝險。", pc(s.max_drawdown), pc(1.0 / (1.0 + s.max_drawdown) - 1.0)),
            ));
        }
    }

    // 11. 目前設定檢查
    if let Some(c) = current {
        if c.roll_dte < 90 {
            out.push(adv(Warning, "展期門檻太短", format!("roll_dte = {} 天：最後 3 個月 theta 與 gamma 急升，股票替代特性喪失；建議 ≥ 120 天。", c.roll_dte)));
        }
        if c.entry_dte < 300 {
            out.push(adv(Suggest, "建倉天數偏短", format!("entry_dte = {} 天：較短的合約每年需展期更多次（多付價差）且時間耗損較快；建議 365–730 天。", c.entry_dte)));
        }
        if let StrikeSelection::Delta(d) = c.strike {
            if d < 0.6 {
                out.push(adv(Warning, "Delta 偏低", format!("Δ{d:.2} 接近價平：時間價值與 vega 占權利金比重大，IV 下跌或盤整時損耗明顯，較像投機而非股票替代。")));
            } else if d > 0.92 {
                out.push(adv(Info, "Delta 很高", format!("Δ{d:.2}：幾乎等同股票，但每單位曝險占用大量現金，緩衝變少；也可直接考慮期貨或融資。")));
            }
        }
        if c.target_exposure > 2.0 {
            out.push(adv(Risk, "目標曝險超過 2 倍", format!("{:.2}x 曝險下，指數下跌 −35% 可能讓帳戶損失 60% 以上；QQQ 在 2000–2002 年的情境下接近歸零。", c.target_exposure)));
        }
        if c.rebalance_band.is_none_or(|b| b <= 0.0) {
            out.push(adv(Info, "未設定再平衡區間", "只在展期時調整口數：大漲後曝險會因 delta 上升而偏高、大跌後偏低。設定 ±20–30% 的區間可維持穩定的槓桿。".to_string()));
        }
        if c.integer_contracts {
            let per_contract = 0.8 * spot * c.multiplier;
            let n = c.initial_capital * c.target_exposure / per_contract;
            if n < 4.0 {
                out.push(adv(
                    Warning,
                    "口數太少，曝險顆粒度粗",
                    format!("每口 0.80Δ 約帶來 ${per_contract:.0} 曝險，目前資金只能買約 {n:.1} 口；四捨五入會讓實際曝險偏離目標 ±{:.0}% 以上。可改用 SPY/XSP 或增加資金。", 50.0 / n.max(0.5)),
                ));
            }
        }
        if c.costs.half_spread_pct < 0.003 && c.costs.slippage <= 0.0 {
            out.push(adv(Info, "交易成本假設偏樂觀", format!("半價差 {} 低於深價內 LEAPS 常見水準（0.3–1%），回測報酬可能高估。", pc(c.costs.half_spread_pct))));
        }
        if c.max_premium_pct > 0.8 && c.target_exposure > 1.8 {
            out.push(adv(Warning, "權利金上限過高", format!("max_premium_pct = {}：高曝險下幾乎全部資金都變成權利金，失去現金緩衝，崩盤時接近歸零。", pc(c.max_premium_pct))));
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Knowledge base
// ---------------------------------------------------------------------------

/// 常見槓桿多頭算法的綜合策略結論（內建知識庫）。
pub fn strategy_conclusions() -> Vec<Advice> {
    use AdviceLevel::*;
    vec![
        adv(
            Info,
            "總結：目標是「穩健的槓桿 beta」，不是 alpha",
            "長期看多美國大型股時，報酬主要來自股票風險溢酬（歷史上約 5–7%/年超額）。槓桿能放大溢酬，但也以 L² 放大變異數：長期幾何報酬 ≈ r + L(μ−r) − L²σ²/2。\
             因此存在最佳槓桿（Kelly），超過之後報酬反而下降、破產風險急升。所有工具（融資、槓桿 ETF、期貨、LEAPS）本質都是在「以多少成本借錢」與「如何控制曝險路徑」上取捨。\
             本引擎的結論：以 0.70–0.85Δ、12–24 個月 LEAPS 滾動、維持約 1.0–1.5x（SPY）或 0.8–1.3x（QQQ）曝險（約 0.6–0.9 倍 Kelly）、閒置資金放 T-Bill，是成本與風險控制都相對穩健的做法。",
        ),
        adv(
            Info,
            "買進持有（基準）",
            "最簡單、成本最低、沒有展期與流動性問題。SPY 1993–2026 年化約 10.7%、最大回撤 −55%（2008）；QQQ 1999–2026 年化約 10.8%，但 2000–2002 年回撤 −83%、花了約 15 年才回本。\
             任何槓桿方案都應與此比較「風險調整後」的結果（Sharpe、Calmar），並記得：在 1.0x 曝險下 LEAPS 並沒有免費的超額報酬——本引擎 bootstrap 顯示 1.0x LEAPS 勝過買進持有的機率只有約四到五成。",
        ),
        adv(
            Info,
            "券商融資（保證金）槓桿",
            "優點：簡單、可精確控制槓桿、可領股息。缺點：零售融資利率通常為基準利率 +0.5–1.5%（IBKR）或更高，台灣複委託多半不提供；跌深時會追繳保證金、被強制平倉，正好在最低點賣出。\
             Ayres & Nalebuff《Lifecycle Investing》(2010) 主張年輕時以 2:1 槓桿分散時間風險，並建議用深價內 LEAPS 代替融資，原因正是 LEAPS 隱含的借款利率較低且沒有追繳。",
        ),
        adv(
            Warning,
            "槓桿 ETF（QLD/TQQQ/SSO/UPRO）：每日重設與波動耗損",
            "每日重設使報酬 ≈ L·μ − (L−1)(r+利差) − 費用率 − (L²−L)σ²/2（相對於 L 倍指數）。以 QQQ σ≈25%：2x 每年額外耗損約 6%，3x 約 19%；費用率 0.9–0.95% 另計。\
             在低波動、強趨勢期間（2010–2021）報酬驚人，但在高波動盤整或崩盤（2000–2002、2008、2022 TQQQ −79%）會出現難以回復的損失；模擬 TQQQ 於 2000–2002 年跌幅超過 −99%。\
             優點是不會追繳、虧損以本金為限、操作簡單。本引擎中「每日再平衡 L×」基準即代表此類產品，滾動 LEAPS 在相同目標曝險下多數期間勝出約 1–3%/年（無費用率、較低的再平衡頻率）。",
        ),
        adv(
            Suggest,
            "LEAPS 股票替代（深價內買權滾動）",
            "買進 0.70–0.85Δ、12–24 個月的買權，剩 4–8 個月時展期。隱含借款利率接近公債利率（盒式價差利率約 T-Bill +0.2–0.5%），低於多數融資；最大損失為權利金、不會追繳；\
             凸性讓崩盤時曝險自動下降。成本：買賣價差、時間價值中的波動率風險溢酬（IV 通常高於之後的實現波動）、需要展期紀律。\
             本引擎合成定價回測（含 0.5% 半價差、佣金、選擇權利率 +0.3%）：SPY 1993–2026，1.5x 目標曝險 CAGR 約 13.6%（買進持有 10.7%、每日再平衡 1.5x 約 12.2%），最大回撤約 −77%；\
             QQQ 1999–2026 1.5x 約 13%，但 2000–2002 年回撤 −95%。2009 年後 QQQ 1.5x 約 29%/年、回撤 −55%。結論：LEAPS 是取得 1.2–2x 曝險最有效率的方式之一，但無法消除槓桿本身的回撤。",
        ),
        adv(
            Info,
            "PMCC（Poor Man's Covered Call：長 LEAPS + 賣短期價外買權）",
            "賣出 30–45 天價外買權收取時間價值，可收割波動率風險溢酬、降低持有成本，但上檔被封頂。學術與指數證據（CBOE BXM 備兌買權指數）顯示在長期多頭中明顯落後指數，主要優勢是較低波動。\
             PMCC 又多了短權被指派、除息日提前履約、在急漲時需向上展期（買回虧損）的管理成本。若目標是長期槓桿 beta，PMCC 會砍掉最重要的右尾報酬，只適合作為盤整期的收入疊加，不建議作為核心。",
        ),
        adv(
            Warning,
            "合成多頭（買買權 + 賣賣權 / Risk Reversal）與期貨",
            "同履約價買 call、賣 put 的淨權利金接近 0，曝險 ≈ 1 倍名目，隱含利率同樣接近盒式利率，成本最低；但賣出賣權等於承擔完整下跌，需要保證金、會被追繳，失去 LEAPS「最大損失有限」的特性。\
             經濟上等同股指期貨（/MES、/ES、/MNQ）。期貨是成本最低、最透明的槓桿 beta 工具（美國稅務居民適用 1256 條款），但同樣有保證金追繳與每季轉倉；適合紀律嚴格、能維持大量現金緩衝的投資人。",
        ),
        adv(
            Suggest,
            "200 日均線擇時 + 槓桿",
            "Gayed & Bilello（2016,《Leverage for the Long Run》）發現：波動與大幅回撤集中在價格低於 200 日均線時；在均線之上持有槓桿、之下轉為公債，歷史上大幅改善風險調整後報酬並避開 2000–2002、2008 的大部分跌幅。\
             代價：盤整期反覆進出（whipsaw）、錯過 V 型反彈（2020 年 3 月）、交易成本與稅。用 LEAPS 實作時，頻繁全進全出的價差成本很高；建議以「均線之下把曝險降到 1.0x 左右」的漸進方式取代完全出場。",
        ),
        adv(
            Suggest,
            "固定曝險再平衡與波動目標",
            "Moreira & Muir（2017,《Volatility-Managed Portfolios》）顯示依波動率反向調整曝險（波動高時降槓桿）能提升 Sharpe；原因是波動具有群聚性而報酬未同比例上升。\
             每日再平衡的槓桿 ETF 是極端版本（過度交易、在下跌中賣低）；以 ±20–30% 區間再平衡則在控制槓桿與降低交易成本間取得平衡。本引擎預設 ±25% 區間，且在 delta 偏離目標超過 0.15 時改為重設履約價，避免越跌越買進價外買權。",
        ),
        adv(
            Info,
            "Kelly 準則與實務槓桿區間",
            "連續時間最佳槓桿 f* = (μ−r)/σ²。SPY：μ−r≈5–6%、σ≈16–18% ⇒ f*≈1.6–2.3；QQQ：μ−r≈6%、σ≈22–25% ⇒ f*≈1.0–1.2。本引擎 SPY 曝險掃描中 CAGR 約在 2–2.5x 達到高峰後下降，與 Kelly 一致；QQQ 則在 1x–2.5x 之間 CAGR 幾乎持平（約 12–14%），最大回撤卻從 −84% 擴大到 −99%。\
             由於 μ 的估計誤差極大，實務採半 Kelly：SPY 約 1.0–1.5x、QQQ 約 0.8–1.3x。超過全 Kelly 是「承擔更多風險換取更少報酬」。",
        ),
        adv(
            Warning,
            "最佳化與過度擬合",
            "網格最佳化的「最佳」參數高度依賴樣本期間（例如 QQQ 是否包含 2000–2002）。本引擎 walk-forward（訓練 8 年、測試 3 年）顯示：每期重新選參數的樣本外表現與固定的合理預設相近甚至更差。\
             穩健的做法是：選擇參數高原的中央、用 block bootstrap（保留報酬與波動率指數的聯合變動、即槓桿效應）檢查回撤與破產機率，並在不同標的與期間交叉驗證。",
        ),
        adv(
            Risk,
            "常見錯誤",
            "(1) 買短天期價外買權當成「槓桿持股」——那是方向投機，時間價值耗損極快；(2) 把全部資金變成權利金，沒有現金緩衝；(3) IV 高檔時一次重押；(4) 到期前一兩個月才展期，承受最快的 theta；\
             (5) 用市價單交易流動性差的履約價；(6) 跌深後持有已變價外的買權不向下展期，失去股票替代特性；(7) 以為 LEAPS 沒有追繳就沒有風險——QQQ 2000–2002 年 1.5x LEAPS 回撤仍達 −95%。",
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backtest::test_util::gbm_data;
    use crate::optimize::{optimize, Objective, ParamGrid};

    #[test]
    fn snapshot_from_history() {
        let data = gbm_data(3.0, 0.10, 0.2, 20.0, 0.02, 0.0, 3);
        let s = MarketSnapshot::from_history(&data.prices, &data.vol_index);
        assert!(s.spot > 0.0 && s.sma200 > 0.0 && s.rv20 > 0.0);
        assert!((s.vol_now - 20.0).abs() < 1e-12);
        assert!(s.drawdown_from_high <= 0.0);
        assert!((-1.0..=1.0).contains(&s.trend_score));
        let empty = MarketSnapshot::from_history(&PriceSeries::default(), &ScalarSeries::default());
        assert!(empty.spot.is_nan());
    }

    #[test]
    fn advise_produces_quantitative_chinese_advice() {
        let data = gbm_data(4.0, 0.10, 0.2, 20.0, 0.02, 0.0, 5);
        let snap = MarketSnapshot::from_history(&data.prices, &data.vol_index);
        let rows = optimize(&data, &RollConfig::default(), &ParamGrid::small(), Objective::Calmar, None);
        let cur = RollConfig { target_exposure: 2.5, roll_dte: 60, entry_dte: 200, integer_contracts: true, ..RollConfig::default() };
        let a = advise(&snap, rows.first(), Some(&cur));
        assert!(a.len() >= 10);
        assert!(a.iter().any(|x| x.title.contains("Kelly")));
        assert!(a.iter().any(|x| x.level == AdviceLevel::Risk));
        assert!(a.iter().any(|x| x.title.contains("展期門檻太短")));
        assert!(a.iter().all(|x| !x.body.contains("NaN")), "{:?}", a.iter().find(|x| x.body.contains("NaN")));
        let none = advise(&snap, None, None);
        assert!(none.len() >= 8);
    }

    #[test]
    fn helpers() {
        assert!((kelly_leverage(0.05, 0.2) - 1.25).abs() < 1e-12);
        // growth is maximised at the Kelly leverage
        let g = |l| expected_log_growth(l, 0.05, 0.03, 0.2);
        assert!(g(1.25) > g(1.0) && g(1.25) > g(1.5));
        let rows = leaps_carry_table(&SynthVolModel::spy(), 100.0, 17.0, 0.04, 0.013, &[0.7, 0.9], &[365, 90]);
        assert_eq!(rows.len(), 4);
        // shorter-dated ⇒ faster decay
        assert!(rows[1].theta_pct_of_price_annual > rows[0].theta_pct_of_price_annual);
        // deeper ITM ⇒ less leverage
        assert!(rows[2].leverage < rows[0].leverage);
        let st = stress_test(&RollConfig::default(), 100.0, 17.0, 0.04, 0.013, &[(0.35, 45.0, 120)]);
        // convexity: max loss is the premium (~38% of equity at 1.5x), better than a daily 1.5x fund
        assert!(st[0].leaps_change < -0.2 && st[0].leaps_change > st[0].lev_etf_change, "{:?}", st[0]);
        assert!(st[0].exposure_after < 1.0);
        assert!(strategy_conclusions().len() >= 10);
    }
}
