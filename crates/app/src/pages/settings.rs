//! 設定 Settings: API keys, default provider, rates, data dir; saved via optlab-data Config.

use gpui::{div, prelude::*, px, Context, Entity, SharedString, Window};
use gpui_component::{
    button::{Button, ButtonVariants},
    checkbox::Checkbox,
    h_flex,
    input::{Input, InputState},
    select::{Select, SelectState},
    v_flex, IndexPath, Sizable,
};
use optlab_data::{bundled, Config, Provider};

use crate::data_bridge::{self, Settings};
use crate::state::{AppState, StatusLevel};
use crate::theme::palette;
use crate::widgets::ui::*;

pub struct SettingsPage {
    state: Entity<AppState>,
    tradier: Entity<InputState>,
    polygon: Entity<InputState>,
    provider: Entity<SelectState<Vec<SharedString>>>,
    rate: Entity<InputState>,
    div_yield: Entity<InputState>,
    symbol: Entity<InputState>,
    data_dir: Entity<InputState>,
    max_exp: Entity<InputState>,
    sandbox: bool,
    offline: bool,
    errors: FormErrors,
    saved: Option<String>,
}

const PROVIDERS: [Option<Provider>; 5] = [None, Some(Provider::Cboe), Some(Provider::Yahoo), Some(Provider::Tradier), Some(Provider::Polygon)];

impl SettingsPage {
    pub fn new(state: Entity<AppState>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let s = state.read(cx).settings.clone();
        let c = &s.config;
        let masked = |v: Option<String>, ph: &str, window: &mut Window, cx: &mut Context<Self>| {
            let v = v.unwrap_or_default();
            let ph = ph.to_string();
            cx.new(|cx| InputState::new(window, cx).masked(true).default_value(v).placeholder(ph))
        };
        let labels: Vec<SharedString> = PROVIDERS.iter().map(|p| SharedString::from(p.map(|p| p.display_name()).unwrap_or("自動 (CBOE 優先)"))).collect();
        let idx = PROVIDERS.iter().position(|p| *p == c.default_provider).unwrap_or(0);
        let provider = cx.new(|cx| SelectState::new(labels, Some(IndexPath::default().row(idx)), window, cx));
        Self {
            tradier: masked(c.tradier_token.clone(), "Tradier access token", window, cx),
            polygon: masked(c.polygon_api_key.clone(), "Polygon / Massive API key", window, cx),
            provider,
            rate: new_input_ph(c.risk_free_rate.map(|r| format!("{:.3}", r * 100.0)).unwrap_or_default(), "空白 = 自動 (內建最新 3M T-Bill)", window, cx),
            div_yield: new_input(format!("{:.2}", s.dividend_yield * 100.0), window, cx),
            symbol: new_input(s.default_symbol.clone(), window, cx),
            data_dir: new_input_ph(c.data_dir.as_ref().map(|d| d.display().to_string()).unwrap_or_default(), "空白 = 預設資料夾", window, cx),
            max_exp: new_input_ph(c.max_expiries.map(|m| m.to_string()).unwrap_or_default(), "空白 = 全部到期日", window, cx),
            sandbox: c.tradier_sandbox,
            offline: false,
            errors: FormErrors::default(),
            saved: None,
            state,
        }
    }

    fn save(&mut self, cx: &mut Context<Self>) {
        let mut e = FormErrors::default();
        let opt_text = |inp: &Entity<InputState>, cx: &Context<Self>| {
            let t = input_text(inp, cx);
            (!t.is_empty()).then_some(t)
        };
        let rate = if input_text(&self.rate, cx).is_empty() { None } else { Some(e.num_range("rate", &self.rate, -5.0, 30.0, cx) / 100.0) };
        let dy = e.num_range("div", &self.div_yield, 0.0, 20.0, cx) / 100.0;
        let max_exp = if input_text(&self.max_exp, cx).is_empty() { None } else { Some(e.int("max_exp", &self.max_exp, 1, 500, cx) as usize) };
        let sym = input_text(&self.symbol, cx).to_ascii_uppercase();
        if sym.is_empty() {
            e.errors.push(("symbol", "必填".into()));
        }
        if !e.is_empty() {
            self.errors = e;
            cx.notify();
            return;
        }
        self.errors = e;
        let idx = self.provider.read(cx).selected_index(cx).map(|i| i.row).unwrap_or(0);
        let config = Config {
            tradier_token: opt_text(&self.tradier, cx),
            tradier_sandbox: self.sandbox,
            polygon_api_key: opt_text(&self.polygon, cx),
            default_provider: PROVIDERS.get(idx).copied().flatten(),
            risk_free_rate: rate,
            data_dir: opt_text(&self.data_dir, cx).map(Into::into),
            max_expiries: max_exp,
        };
        let risk_free = rate.or_else(data_bridge::latest_tbill).unwrap_or(0.04);
        let settings = Settings { config, risk_free, dividend_yield: dy, default_symbol: sym };
        let res = data_bridge::save_settings(&settings);
        let offline = self.offline;
        self.state.update(cx, |st, cx| {
            st.apply_settings(settings, cx);
            st.svc.set_offline(offline);
            match &res {
                Ok(p) => st.set_status(StatusLevel::Ok, format!("設定已儲存：{}", p.display()), cx),
                Err(err) => st.set_status(StatusLevel::Error, format!("設定已套用但儲存失敗：{err}"), cx),
            }
        });
        self.saved = Some(match res {
            Ok(p) => format!("已儲存到 {}", p.display()),
            Err(err) => format!("儲存失敗：{err}"),
        });
        cx.notify();
    }
}

impl Render for SettingsPage {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = palette();
        let e = &self.errors;
        let st = self.state.read(cx);
        let svc = st.svc.clone();
        let cfg_path = Config::default_path().map(|p| p.display().to_string()).unwrap_or_else(|e| e.to_string());
        let store_root = svc.store().root().display().to_string();
        let snaps = svc.list_snapshots(None).unwrap_or_default();
        let bundles = bundled::list();
        let rf = st.settings.risk_free;
        let row = || h_flex().gap_2().items_start();
        let form = panel("資料來源與 API 金鑰", cx).w(px(560.0)).flex_none().child(
            v_flex()
                .p_2()
                .gap_2()
                .child(labeled("Tradier token", Input::new(&self.tradier).small().mask_toggle(), cx))
                .child(Checkbox::new("sandbox").label("使用 Tradier sandbox (延遲報價，免費開發者帳號)").checked(self.sandbox).on_click(cx.listener(|this, v: &bool, _, cx| {
                    this.sandbox = *v;
                    cx.notify();
                })))
                .child(labeled("Polygon / Massive API key", Input::new(&self.polygon).small().mask_toggle(), cx))
                .child(labeled("預設期權鏈來源", Select::new(&self.provider).small(), cx))
                .child(row().child(field_err("無風險利率 % (空白=自動)", &self.rate, e.get("rate"), cx).flex_1()).child(field_err("預設股息率 %", &self.div_yield, e.get("div"), cx).flex_1()))
                .child(row().child(field_err("預設標的", &self.symbol, e.get("symbol"), cx).flex_1()).child(field_err("到期日上限 (Yahoo/Tradier)", &self.max_exp, e.get("max_exp"), cx).flex_1()))
                .child(labeled("資料 / 快照資料夾", Input::new(&self.data_dir).small(), cx))
                .child(Checkbox::new("offline").label("離線模式 (不連網，只用快照 / CSV / 內建資料)").checked(self.offline).on_click(cx.listener(|this, v: &bool, _, cx| {
                    this.offline = *v;
                    this.state.update(cx, |st, _| st.svc.set_offline(*v));
                    cx.notify();
                })))
                .child(h_flex().gap_2().child(Button::new("save-settings").primary().small().label("儲存設定").on_click(cx.listener(|this, _, _, cx| this.save(cx)))))
                .when_some(self.saved.clone(), |d, s| d.child(muted_text(s, cx)))
                .when(!e.is_empty(), |d| d.child(div().text_size(px(11.0)).text_color(p.red).child(e.summary())))
                .child(muted_text("金鑰也可用環境變數 TRADIER_TOKEN / POLYGON_API_KEY / OPTLAB_RISK_FREE_RATE 設定；設定 OPTLAB_HOME 可使用可攜模式。", cx)),
        );
        let info = v_flex()
            .flex_1()
            .min_w_0()
            .gap_2()
            .child(
                panel("路徑與狀態", cx).child(
                    panel_body()
                        .child(kv("設定檔", cfg_path, cx))
                        .child(kv("快照資料夾", store_root, cx))
                        .child(kv("目前無風險利率", pct2(rf), cx))
                        .child(kv("內建最新 3M T-Bill", data_bridge::latest_tbill().map(pct2).unwrap_or_else(|| "—".into()), cx))
                        .child(kv("可用即時來源", svc.available_providers().iter().map(|p| p.short_name()).collect::<Vec<_>>().join(", "), cx))
                        .child(kv("離線模式", if svc.is_offline() { "開" } else { "關" }, cx)),
                ),
            )
            .child(panel(format!("本地期權鏈快照 ({} 個)", snaps.len()), cx).child(v_flex().p_2().gap_0p5().children(snaps.iter().rev().take(15).map(|s| {
                kv(format!("{} · {}", s.symbol, s.source), s.as_of.format("%Y-%m-%d %H:%M").to_string(), cx)
            })).when(snaps.is_empty(), |d| d.child(muted_text("尚無快照：即時取得期權鏈或匯入 CSV 時會自動儲存。", cx)))))
            .child(panel(format!("內建離線資料 ({} 組)", bundles.len()), cx).child(v_flex().p_2().gap_0p5().children(bundles.iter().map(|b| {
                kv(format!("{} · {}", b.symbol, b.description), format!("{} → {}", b.first, b.last), cx)
            }))));
        div().id("settings-scroll").size_full().overflow_y_scroll().child(h_flex().items_start().gap_2().p_3().child(form).child(info))
    }
}
