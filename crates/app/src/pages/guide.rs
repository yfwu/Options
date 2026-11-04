//! 策略知識庫 Guide: built-in conclusions, current advice, LEAPS rolling playbook,
//! carry table (delta × DTE), crash stress test and every template's description.

use gpui::{div, prelude::*, px, Context, Entity, SharedString, Window};
use gpui_component::{h_flex, v_flex, ActiveTheme};
use optlab_core::advisor::{advise, leaps_carry_table, strategy_conclusions, stress_test, Advice, CarryRow, MarketSnapshot, StressRow};
use optlab_core::backtest::RollConfig;
use optlab_core::strategy::ALL_TEMPLATES;

use crate::data_bridge;
use crate::state::AppState;
use crate::theme::palette;
use crate::widgets::ui::*;

const PLAYBOOK: &[(&str, &str)] = &[
    ("1. 選標的", "以流動性最好的 QQQ / SPY（美式、ETF）或 SPX / NDX（歐式、現金交割、60/40 稅制對美國人有利；對台灣投資人主要差異在合約名目較大與無提前履約）。科技權重高者用 VXN 判斷 IV 貴賤，大盤用 VIX。"),
    ("2. 選 Delta 與天期", "股票替代以 0.70–0.85Δ、12–24 個月（DTE 365–730）為主：Delta 越高時間價值越少、越像股票；天期越長每年的 theta 成本越低、但 vega 越大。到「策略知識庫 → 持有成本表」比較每單位曝險的年化成本。"),
    ("3. 控制曝險倍數", "以「Delta × 標的價 × 100 × 口數 ÷ 帳戶淨值」計算實際曝險，而不是只看權利金。長期幾何報酬 ≈ r + L(μ−r) − L²σ²/2，曝險過高反而降低長期報酬並大幅提高歸零風險；SPY 約 1.0–1.5x、QQQ 約 0.8–1.3x 是較穩健的區間。"),
    ("4. 滾動規則", "剩餘 DTE 降到 120–240 天時滾到新的遠月（避開 theta 加速區）；大漲使 Delta > 0.90–0.95 時可上移履約價鎖利（roll-up）；大跌使 Delta < 0.5 時檢討是否降槓桿而非攤平。"),
    ("5. 再平衡帶", "以 ±20–30% 的曝險帶再平衡（超出才調整口數），兼顧交易成本與風險控制。閒置現金放短天期國庫券（T-Bill）賺無風險利率，這是 LEAPS 隱含融資的「另一半」。"),
    ("6. 進場時機與 IV", "IV Rank 高（> 70%）時權利金昂貴：可分批建倉、選更深價內（0.85–0.90Δ）降低 vega，或暫時降低目標曝險。IV 低時延長天期鎖住便宜的波動率。"),
    ("7. 成本與流動性", "選擇未平倉量大、價差窄的月選/LEAPS（一月與六月、十二月到期最活躍），以中間價掛限價單；回測頁可調整半價差與滑價，檢查策略對成本的敏感度。"),
    ("8. 風險管理", "槓桿部位在 2000–2002、2008、2022 類型的空頭可能回撤 60–90%。務必先在「參數優化 → 區塊拔靴」檢查歸零機率與回撤分佈，並以壓力測試表確認可承受的最壞情境；必要時搭配少量遠價外賣權作為災難保險。"),
];

pub struct GuidePage {
    state: Entity<AppState>,
    conclusions: Vec<Advice>,
    advice: Vec<Advice>,
    snap_label: String,
    carry: Vec<CarryRow>,
    stress: Vec<StressRow>,
    stress_label: String,
    loaded: bool,
}

impl GuidePage {
    pub fn new(state: Entity<AppState>, _window: &mut Window, _cx: &mut Context<Self>) -> Self {
        Self { state, conclusions: strategy_conclusions(), advice: vec![], snap_label: String::new(), carry: vec![], stress: vec![], stress_label: String::new(), loaded: false }
    }

    fn ensure_loaded(&mut self, cx: &mut Context<Self>) {
        if self.loaded {
            return;
        }
        self.loaded = true;
        let sym = self.state.read(cx).settings.default_symbol.to_ascii_uppercase();
        let r = self.state.read(cx).settings.risk_free;
        let Ok(prices) = self.state.update(cx, |st, _| st.prices(&sym)) else { return };
        let Ok((vname, vol)) = data_bridge::default_vol_for(&sym) else { return };
        let snap = MarketSnapshot::from_history(&prices, &vol);
        self.advice = advise(&snap, None, None);
        let spot = prices.bars.last().map(|b| b.close).unwrap_or(snap.spot);
        let q = data_bridge::default_dividend_yield(&sym);
        let model = data_bridge::vol_model_for(&sym);
        self.snap_label = format!("{sym} {} · 收盤 {spot:.2} · {vname} {:.1} · r {:.2}%", snap.date, snap.vol_now, r * 100.0);
        self.carry = leaps_carry_table(&model, spot, snap.vol_now, r, q, &[0.6, 0.7, 0.8, 0.9], &[180, 365, 540, 730]);
        let cfg = RollConfig { vol_model: model, ..if sym == "SPY" { RollConfig::spy() } else { RollConfig::qqq() } };
        self.stress_label = format!("{} · {}", sym, cfg.label());
        let scen = [(0.10, snap.vol_now * 1.3, 10), (0.20, snap.vol_now * 1.7, 30), (0.35, snap.vol_now * 2.2, 60), (0.50, snap.vol_now * 2.5, 180)];
        self.stress = stress_test(&cfg, spot, snap.vol_now, r, q, &scen);
    }
}

impl Render for GuidePage {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.ensure_loaded(cx);
        let p = palette();
        let muted = cx.theme().muted_foreground;
        let carry_cols = vec![
            Col::new("Δ", 48.0),
            Col::new("DTE", 50.0),
            Col::new("K/S", 56.0),
            Col::new("IV", 56.0),
            Col::new("權利金%S", 70.0),
            Col::new("時間價值/年", 80.0),
            Col::new("成本/年 (每曝險)", 108.0),
            Col::new("Θ 耗損/年", 76.0),
            Col::new("Vega%", 60.0),
            Col::new("槓桿", 56.0),
        ];
        let stress_cols = vec![
            Col::new("跌幅", 60.0),
            Col::new("波動指數", 70.0),
            Col::new("天數", 50.0),
            Col::new("LEAPS 權益", 90.0),
            Col::new("槓桿 ETF", 80.0),
            Col::new("不槓桿", 70.0),
            Col::new("衝擊後曝險", 84.0),
        ];
        let left = v_flex()
            .flex_1()
            .min_w_0()
            .gap_2()
            .child(panel("LEAPS 滾動操作手冊 Playbook", cx).child(v_flex().p_2().gap_2().children(PLAYBOOK.iter().map(|(t, b)| {
                v_flex().gap_0p5().child(div().text_size(px(12.5)).font_weight(gpui::FontWeight::SEMIBOLD).text_color(p.blue).child(*t)).child(div().text_size(px(12.0)).text_color(muted).child(*b))
            }))))
            .child(panel("策略結論 (內建知識庫 strategy_conclusions)", cx).child(v_flex().p_2().gap_2().children(self.conclusions.iter().map(|a| advice_card(a, cx)))))
            .child(panel(format!("目前市況建議 · {}", self.snap_label), cx).child(v_flex().p_2().gap_2().children(self.advice.iter().map(|a| advice_card(a, cx)))));
        let right = v_flex()
            .w(px(800.0))
            .flex_none()
            .gap_2()
            .child(
                panel(format!("LEAPS 持有成本表 (Call, 合成 IV 曲面) · {}", self.snap_label), cx).child(
                    v_flex().child(table_header(&carry_cols, cx)).children(self.carry.iter().enumerate().map(|(i, c)| {
                        table_row(
                            SharedString::from(format!("carry-{i}")),
                            &carry_cols,
                            vec![
                                Cell::new(f2(c.delta)),
                                Cell::new(c.dte.to_string()),
                                Cell::new(f3(c.strike_pct)),
                                Cell::new(pct(c.iv)).color(p.purple),
                                Cell::new(pct(c.price_pct)),
                                Cell::new(pct2(c.extrinsic_pct_annual)).color(p.orange),
                                Cell::new(pct2(c.carry_pct_of_exposure)).color(p.yellow),
                                Cell::new(pct(c.theta_pct_of_price_annual)),
                                Cell::new(pct2(c.vega_pct_of_price)),
                                Cell::new(format!("{:.2}x", c.leverage)).color(p.blue),
                            ],
                            i % 2 == 1,
                            cx,
                        )
                    })),
                ),
            )
            .child(
                panel(format!("崩盤壓力測試 · {}", self.stress_label), cx).child(
                    v_flex().child(table_header(&stress_cols, cx)).children(self.stress.iter().enumerate().map(|(i, s)| {
                        table_row(
                            SharedString::from(format!("stress-{i}")),
                            &stress_cols,
                            vec![
                                Cell::new(format!("-{:.0}%", s.drop * 100.0)).color(p.red),
                                Cell::new(f1(s.vol_after)),
                                Cell::new(s.days.to_string()),
                                Cell::new(pct(s.leaps_change)).color(sign_color(s.leaps_change)),
                                Cell::new(pct(s.lev_etf_change)).color(sign_color(s.lev_etf_change)),
                                Cell::new(pct(s.unlevered_change)).color(sign_color(s.unlevered_change)),
                                Cell::new(format!("{:.2}x", s.exposure_after)),
                            ],
                            i % 2 == 1,
                            cx,
                        )
                    })),
                ),
            )
            .child(panel("內建策略範本 (★ = 長期槓桿做多適配度)", cx).child(v_flex().p_2().gap_3().children(ALL_TEMPLATES.iter().map(|t| {
                v_flex()
                    .gap_1()
                    .child(
                        h_flex()
                            .gap_2()
                            .child(div().text_size(px(13.0)).font_weight(gpui::FontWeight::SEMIBOLD).child(format!("{} · {}", t.name_zh(), t.name_en())))
                            .child(badge(t.outlook().label_zh(), p.cyan))
                            .child(div().text_size(px(11.0)).text_color(p.yellow).child("★".repeat(t.bull_leverage_fit() as usize))),
                    )
                    .child(div().text_size(px(12.0)).text_color(muted).child(t.description_zh()))
            }))));
        div().id("guide-scroll").size_full().overflow_y_scroll().child(h_flex().items_start().gap_2().p_3().child(left).child(right))
    }
}
