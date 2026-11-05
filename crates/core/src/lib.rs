//! # optlab-core
//!
//! Quantitative engine of OptLab: option pricing, chain model, strategy analysis,
//! LEAPS roll backtesting, Monte Carlo simulation, optimisation and advice.

pub mod chain;
pub mod history;
pub mod payoff;
pub mod pricing;
pub mod strategy;
pub mod surface;
pub mod montecarlo;
pub mod synth;
pub mod stats;
pub mod backtest;
pub mod optimize;
pub mod advisor;

pub use chain::{ExerciseStyle, OptionChain, OptionKind, OptionQuote};
pub use history::{Bar, PriceSeries, ScalarSeries};
pub use pricing::Greeks;
