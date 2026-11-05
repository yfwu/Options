//! Shared application state (one `Entity<AppState>` shared by every page).

use std::collections::HashMap;
use std::sync::Arc;

use gpui::{Context, EventEmitter, SharedString};
use optlab_core::{OptionChain, PriceSeries};

use crate::data_bridge::{self, Settings};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Page {
    Studio,
    Dashboard,
    Chain,
    Strategy,
    Backtest,
    Optimizer,
    Guide,
    Settings,
}

impl Page {
    pub const ALL: [Page; 8] = [
        Page::Studio,
        Page::Dashboard,
        Page::Chain,
        Page::Strategy,
        Page::Backtest,
        Page::Optimizer,
        Page::Guide,
        Page::Settings,
    ];

    pub fn title(self) -> &'static str {
        match self {
            Page::Studio => "函數工作室",
            Page::Dashboard => "市場總覽",
            Page::Chain => "期權鏈",
            Page::Strategy => "策略實驗室",
            Page::Backtest => "LEAPS 滾動回測",
            Page::Optimizer => "參數優化",
            Page::Guide => "策略知識庫",
            Page::Settings => "設定",
        }
    }

    pub fn subtitle(self) -> &'static str {
        match self {
            Page::Studio => "Payoff Studio",
            Page::Dashboard => "Dashboard",
            Page::Chain => "Option Chain",
            Page::Strategy => "Strategy Lab",
            Page::Backtest => "Backtest",
            Page::Optimizer => "Optimizer",
            Page::Guide => "Guide",
            Page::Settings => "Settings",
        }
    }

    pub fn icon(self) -> &'static str {
        match self {
            Page::Studio => "icons/function.svg",
            Page::Dashboard => "icons/layout-dashboard.svg",
            Page::Chain => "icons/table.svg",
            Page::Strategy => "icons/flask-conical.svg",
            Page::Backtest => "icons/history.svg",
            Page::Optimizer => "icons/sliders-horizontal.svg",
            Page::Guide => "icons/book-open.svg",
            Page::Settings => "icons/settings.svg",
        }
    }

    pub fn key(self) -> &'static str {
        match self {
            Page::Studio => "studio",
            Page::Dashboard => "dashboard",
            Page::Chain => "chain",
            Page::Strategy => "strategy",
            Page::Backtest => "backtest",
            Page::Optimizer => "optimizer",
            Page::Guide => "guide",
            Page::Settings => "settings",
        }
    }

    pub fn from_key(s: &str) -> Option<Page> {
        let s = s.trim().to_ascii_lowercase();
        Page::ALL.iter().copied().find(|p| p.key() == s || (s == "lab" && *p == Page::Strategy) || ((s == "payoff" || s == "function") && *p == Page::Studio) || (s == "optimize" && *p == Page::Optimizer))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StatusLevel {
    Info,
    Ok,
    Warn,
    Error,
}

#[derive(Clone, Debug)]
pub enum AppEvent {
    Navigate(Page),
    ChainChanged,
    /// Optimiser → backtest: apply these parameters (delta, entry DTE, roll DTE, exposure...).
    ApplyBacktestParams(Box<crate::pages::backtest::BacktestParamsPatch>),
    SettingsChanged,
    /// Payoff Studio → Strategy Lab: (strategy, spot, r, q, label).
    OpenInStrategyLab(Box<(optlab_core::strategy::Strategy, f64, f64, f64, String)>),
}

pub struct AppState {
    pub status: (StatusLevel, SharedString),
    pub busy: Vec<(u64, SharedString)>,
    next_task: u64,
    pub chain: Option<Arc<OptionChain>>,
    pub chain_label: SharedString,
    pub settings: Settings,
    pub svc: Arc<optlab_data::DataService>,
    prices: HashMap<String, Arc<PriceSeries>>,
}

impl EventEmitter<AppEvent> for AppState {}

impl AppState {
    pub fn new() -> Self {
        let settings = data_bridge::load_settings();
        let svc = data_bridge::service(&settings.config);
        Self {
            status: (StatusLevel::Info, "就緒 · 內建離線資料可用".into()),
            busy: vec![],
            next_task: 1,
            chain: None,
            chain_label: "".into(),
            settings,
            svc,
            prices: HashMap::new(),
        }
    }

    pub fn set_status(&mut self, level: StatusLevel, msg: impl Into<SharedString>, cx: &mut Context<Self>) {
        self.status = (level, msg.into());
        cx.notify();
    }

    pub fn begin_task(&mut self, label: impl Into<SharedString>, cx: &mut Context<Self>) -> u64 {
        let id = self.next_task;
        self.next_task += 1;
        self.busy.push((id, label.into()));
        cx.notify();
        id
    }

    pub fn end_task(&mut self, id: u64, cx: &mut Context<Self>) {
        self.busy.retain(|(i, _)| *i != id);
        cx.notify();
    }

    pub fn navigate(&mut self, page: Page, cx: &mut Context<Self>) {
        cx.emit(AppEvent::Navigate(page));
    }

    pub fn set_chain(&mut self, chain: OptionChain, label: impl Into<SharedString>, cx: &mut Context<Self>) {
        self.chain = Some(Arc::new(chain));
        self.chain_label = label.into();
        cx.emit(AppEvent::ChainChanged);
        cx.notify();
    }

    /// Bundled (or cached) price history for a symbol.
    pub fn prices(&mut self, symbol: &str) -> Result<Arc<PriceSeries>, String> {
        let key = symbol.to_ascii_uppercase();
        if let Some(s) = self.prices.get(&key) {
            return Ok(s.clone());
        }
        let s = Arc::new(data_bridge::bundled_prices(&key)?);
        self.prices.insert(key, s.clone());
        Ok(s)
    }

    /// Replace settings (and rebuild the data service).
    pub fn apply_settings(&mut self, settings: Settings, cx: &mut Context<Self>) {
        self.svc = data_bridge::service(&settings.config);
        self.settings = settings;
        cx.emit(AppEvent::SettingsChanged);
        cx.notify();
    }

}

impl Default for AppState {
    fn default() -> Self {
        Self::new()
    }
}
