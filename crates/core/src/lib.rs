//! # optlab-core
//!
//! Quantitative engine of OptLab: option pricing, chain model, strategy analysis,
//! LEAPS roll backtesting, Monte Carlo simulation, optimisation and advice.

pub mod chain;
pub mod history;
pub mod pricing;
pub mod strategy;
pub mod surface;
pub mod montecarlo;
pub mod synth;
pub mod stats;
pub mod backtest;

pub use chain::{ExerciseStyle, OptionChain, OptionKind, OptionQuote};
pub use history::{Bar, PriceSeries, ScalarSeries};
pub use pricing::Greeks;
