//! # Payoff language & compiler
//!
//! Options are the instruction set of a small functional language: every payoff
//! function of the terminal price `S` that is piecewise linear can be written exactly as
//!
//! ```text
//! f(S) = a + b·S + Σᵢ wᵢ·(S − Kᵢ)⁺            (cash + stock + calls)
//! ```
//!
//! and, through put–call parity `(S−K)⁺ = (K−S)⁺ + S − K`, with puts below a pivot
//! strike and calls above it (the liquid out-of-the-money side). Smooth payoffs are
//! approximated by their piecewise-linear interpolant on the strike grid (Carr–Madan
//! static replication). This module provides
//!
//! 1. a tiny expression language ([`parse`]) — `S`, numbers, `110%` (= 1.10 × spot),
//!    `+ - * / ^`, variables (`a = 600; call(a) - call(a + 50)`), payoff primitives
//!    `call put digital digitalput pwl` and macros `straddle strangle bull bear butterfly
//!    condor collar protect`, plus `max min abs pos neg clamp sqrt ln exp`;
//! 2. a compiler ([`compile`]) that lowers any expression to cash / stock / calls / puts,
//!    either at the function's own kinks (snapped to a strike step) or on a given list of
//!    listed strikes, and reports the replication error;
//! 3. pricing of the compiled portfolio ([`price`]) on a synthetic smile or a real chain,
//!    with risk-neutral and real-world (Esscher-tilted) distributions of the payoff.

use crate::chain::{OptionChain, OptionKind};
use crate::pricing::{bs_greeks, bs_price, Greeks};
use crate::surface::{fit_surface, VolSurface};
use anyhow::{anyhow, bail, Result};
use chrono::NaiveDate;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

// ---------------------------------------------------------------------------
// AST
// ---------------------------------------------------------------------------

/// Built-in functions of the payoff language.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Func {
    Call,
    Put,
    Digital,
    DigitalPut,
    Pwl,
    Straddle,
    Strangle,
    Bull,
    Bear,
    Butterfly,
    Condor,
    Collar,
    Protect,
    Max,
    Min,
    Abs,
    Pos,
    Neg,
    Clamp,
    Sqrt,
    Ln,
    Exp,
}

impl Func {
    fn from_name(name: &str) -> Option<Func> {
        Some(match name.to_ascii_lowercase().as_str() {
            "call" | "c" => Func::Call,
            "put" | "p" => Func::Put,
            "digital" | "binary" | "dcall" => Func::Digital,
            "digitalput" | "dput" => Func::DigitalPut,
            "pwl" | "points" => Func::Pwl,
            "straddle" => Func::Straddle,
            "strangle" => Func::Strangle,
            "bull" | "callspread" => Func::Bull,
            "bear" | "putspread" => Func::Bear,
            "butterfly" | "fly" => Func::Butterfly,
            "condor" => Func::Condor,
            "collar" => Func::Collar,
            "protect" | "protective" => Func::Protect,
            "max" => Func::Max,
            "min" => Func::Min,
            "abs" => Func::Abs,
            "pos" | "relu" => Func::Pos,
            "neg" => Func::Neg,
            "clamp" => Func::Clamp,
            "sqrt" => Func::Sqrt,
            "ln" | "log" => Func::Ln,
            "exp" => Func::Exp,
            _ => return None,
        })
    }

    /// (min, max) number of arguments; `usize::MAX` = variadic.
    fn arity(self) -> (usize, usize) {
        match self {
            Func::Call | Func::Put | Func::Digital | Func::DigitalPut | Func::Straddle | Func::Protect => (1, 1),
            Func::Abs | Func::Pos | Func::Neg | Func::Sqrt | Func::Ln | Func::Exp => (1, 1),
            Func::Strangle | Func::Bull | Func::Bear | Func::Collar => (2, 2),
            Func::Butterfly | Func::Clamp => (3, 3),
            Func::Condor => (4, 4),
            Func::Max | Func::Min => (2, usize::MAX),
            Func::Pwl => (4, usize::MAX),
        }
    }
}

/// Expression tree.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Expr {
    Num(f64),
    /// Terminal underlying price.
    S,
    /// Current spot (constant at compile time).
    Spot,
    Neg(Box<Expr>),
    Add(Box<Expr>, Box<Expr>),
    Sub(Box<Expr>, Box<Expr>),
    Mul(Box<Expr>, Box<Expr>),
    Div(Box<Expr>, Box<Expr>),
    Pow(Box<Expr>, Box<Expr>),
    Fn(Func, Vec<Expr>),
}

/// Evaluation context.
#[derive(Debug, Clone, Copy)]
pub struct Ctx {
    pub s: f64,
    pub spot: f64,
}

impl Expr {
    /// Evaluate at terminal price `ctx.s`.
    pub fn eval(&self, ctx: Ctx) -> f64 {
        use Expr::*;
        match self {
            Num(v) => *v,
            S => ctx.s,
            Spot => ctx.spot,
            Neg(a) => -a.eval(ctx),
            Add(a, b) => a.eval(ctx) + b.eval(ctx),
            Sub(a, b) => a.eval(ctx) - b.eval(ctx),
            Mul(a, b) => a.eval(ctx) * b.eval(ctx),
            Div(a, b) => {
                let d = b.eval(ctx);
                if d == 0.0 {
                    f64::NAN
                } else {
                    a.eval(ctx) / d
                }
            }
            Pow(a, b) => a.eval(ctx).powf(b.eval(ctx)),
            Fn(f, args) => eval_fn(*f, args, ctx),
        }
    }

    /// True when the expression does not depend on `S` (a constant).
    pub fn is_const(&self) -> bool {
        use Expr::*;
        match self {
            Num(_) | Spot => true,
            S => false,
            Neg(a) => a.is_const(),
            Add(a, b) | Sub(a, b) | Mul(a, b) | Div(a, b) | Pow(a, b) => a.is_const() && b.is_const(),
            Fn(f, args) => {
                !matches!(f, Func::Call | Func::Put | Func::Digital | Func::DigitalPut | Func::Pwl | Func::Straddle | Func::Strangle | Func::Bull | Func::Bear | Func::Butterfly | Func::Condor | Func::Collar | Func::Protect)
                    && args.iter().all(|a| a.is_const())
            }
        }
    }
}

fn eval_fn(f: Func, args: &[Expr], ctx: Ctx) -> f64 {
    let a = |i: usize| args[i].eval(ctx);
    let s = ctx.s;
    let call = |k: f64| (s - k).max(0.0);
    let put = |k: f64| (k - s).max(0.0);
    match f {
        Func::Call => call(a(0)),
        Func::Put => put(a(0)),
        Func::Digital => {
            if s > a(0) {
                1.0
            } else {
                0.0
            }
        }
        Func::DigitalPut => {
            if s < a(0) {
                1.0
            } else {
                0.0
            }
        }
        Func::Straddle => call(a(0)) + put(a(0)),
        Func::Strangle => put(a(0)) + call(a(1)),
        Func::Bull => call(a(0)) - call(a(1)),
        Func::Bear => put(a(1)) - put(a(0)),
        Func::Butterfly => call(a(0)) - 2.0 * call(a(1)) + call(a(2)),
        Func::Condor => call(a(0)) - call(a(1)) - call(a(2)) + call(a(3)),
        Func::Collar => s + put(a(0)) - call(a(1)),
        Func::Protect => s + put(a(0)),
        Func::Max => args.iter().map(|e| e.eval(ctx)).fold(f64::NEG_INFINITY, f64::max),
        Func::Min => args.iter().map(|e| e.eval(ctx)).fold(f64::INFINITY, f64::min),
        Func::Abs => a(0).abs(),
        Func::Pos => a(0).max(0.0),
        Func::Neg => a(0).min(0.0),
        Func::Clamp => a(0).clamp(a(1).min(a(2)), a(2).max(a(1))),
        Func::Sqrt => a(0).max(0.0).sqrt(),
        Func::Ln => a(0).ln(),
        Func::Exp => a(0).exp(),
        Func::Pwl => {
            // pwl(x1, y1, x2, y2, ...) in S, flat outside the first/last point
            let pts: Vec<(f64, f64)> = args.chunks(2).map(|c| (c[0].eval(ctx), c[1].eval(ctx))).collect();
            pwl_eval(&pts, s)
        }
    }
}

fn pwl_eval(pts: &[(f64, f64)], x: f64) -> f64 {
    if pts.is_empty() {
        return f64::NAN;
    }
    let mut p: Vec<(f64, f64)> = pts.to_vec();
    p.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
    if x <= p[0].0 {
        return p[0].1;
    }
    for w in p.windows(2) {
        if x <= w[1].0 {
            let (x0, y0) = w[0];
            let (x1, y1) = w[1];
            return if x1 > x0 { y0 + (y1 - y0) * (x - x0) / (x1 - x0) } else { y1 };
        }
    }
    p[p.len() - 1].1
}

// ---------------------------------------------------------------------------
// Parser
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    Num(f64),
    Ident(String),
    Op(char),
    Sep,
}

fn lex(src: &str) -> Result<Vec<(Tok, usize)>> {
    let c: Vec<char> = src.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < c.len() {
        let ch = c[i];
        if ch == '#' {
            while i < c.len() && c[i] != '\n' {
                i += 1;
            }
        } else if ch == '\n' || ch == ';' {
            out.push((Tok::Sep, i));
            i += 1;
        } else if ch.is_whitespace() {
            i += 1;
        } else if ch.is_ascii_digit() || (ch == '.' && i + 1 < c.len() && c[i + 1].is_ascii_digit()) {
            let st = i;
            while i < c.len() && (c[i].is_ascii_digit() || c[i] == '.' || c[i] == '_') {
                i += 1;
            }
            if i < c.len() && (c[i] == 'e' || c[i] == 'E') && i + 1 < c.len() && (c[i + 1].is_ascii_digit() || c[i + 1] == '-' || c[i + 1] == '+') {
                i += 2;
                while i < c.len() && c[i].is_ascii_digit() {
                    i += 1;
                }
            }
            let s: String = c[st..i].iter().filter(|x| **x != '_').collect();
            let v: f64 = s.parse().map_err(|_| anyhow!("位置 {st}: 無法解析數字「{s}」"))?;
            out.push((Tok::Num(v), st));
        } else if ch.is_alphabetic() || ch == '_' {
            let st = i;
            while i < c.len() && (c[i].is_alphanumeric() || c[i] == '_') {
                i += 1;
            }
            out.push((Tok::Ident(c[st..i].iter().collect()), st));
        } else if "+-*/^(),=%".contains(ch) {
            out.push((Tok::Op(ch), i));
            i += 1;
        } else if ch == '×' || ch == '·' {
            out.push((Tok::Op('*'), i));
            i += 1;
        } else if ch == '−' {
            out.push((Tok::Op('-'), i));
            i += 1;
        } else {
            bail!("位置 {i}: 無法辨識的字元「{ch}」");
        }
    }
    Ok(out)
}

struct Parser<'a> {
    toks: &'a [(Tok, usize)],
    pos: usize,
    vars: HashMap<String, Expr>,
}

impl Parser<'_> {
    fn peek(&self) -> Option<&Tok> {
        self.toks.get(self.pos).map(|t| &t.0)
    }
    fn at(&self) -> usize {
        self.toks.get(self.pos).map_or(usize::MAX, |t| t.1)
    }
    fn eat_op(&mut self, op: char) -> bool {
        if self.peek() == Some(&Tok::Op(op)) {
            self.pos += 1;
            true
        } else {
            false
        }
    }
    fn expect_op(&mut self, op: char) -> Result<()> {
        if self.eat_op(op) {
            Ok(())
        } else {
            bail!("位置 {}: 預期「{op}」", self.at())
        }
    }
    fn expr(&mut self) -> Result<Expr> {
        let mut l = self.term()?;
        loop {
            if self.eat_op('+') {
                l = Expr::Add(Box::new(l), Box::new(self.term()?));
            } else if self.eat_op('-') {
                l = Expr::Sub(Box::new(l), Box::new(self.term()?));
            } else {
                return Ok(l);
            }
        }
    }
    fn term(&mut self) -> Result<Expr> {
        let mut l = self.unary()?;
        loop {
            if self.eat_op('*') {
                l = Expr::Mul(Box::new(l), Box::new(self.unary()?));
            } else if self.eat_op('/') {
                l = Expr::Div(Box::new(l), Box::new(self.unary()?));
            } else if matches!(self.peek(), Some(Tok::Ident(_)) | Some(Tok::Op('('))) {
                // implicit multiplication: 2call(600), 0.5(S - 600)
                l = Expr::Mul(Box::new(l), Box::new(self.unary()?));
            } else {
                return Ok(l);
            }
        }
    }
    fn unary(&mut self) -> Result<Expr> {
        if self.eat_op('-') {
            return Ok(Expr::Neg(Box::new(self.unary()?)));
        }
        if self.eat_op('+') {
            return self.unary();
        }
        self.power()
    }
    fn power(&mut self) -> Result<Expr> {
        let base = self.postfix()?;
        if self.eat_op('^') {
            let exp = self.unary()?;
            return Ok(Expr::Pow(Box::new(base), Box::new(exp)));
        }
        Ok(base)
    }
    fn postfix(&mut self) -> Result<Expr> {
        let mut e = self.atom()?;
        while self.eat_op('%') {
            // x% = x/100 × spot
            e = Expr::Mul(Box::new(Expr::Div(Box::new(e), Box::new(Expr::Num(100.0)))), Box::new(Expr::Spot));
        }
        Ok(e)
    }
    fn atom(&mut self) -> Result<Expr> {
        let at = self.at();
        match self.peek().cloned() {
            Some(Tok::Num(v)) => {
                self.pos += 1;
                Ok(Expr::Num(v))
            }
            Some(Tok::Op('(')) => {
                self.pos += 1;
                let e = self.expr()?;
                self.expect_op(')')?;
                Ok(e)
            }
            Some(Tok::Ident(name)) => {
                self.pos += 1;
                if self.eat_op('(') {
                    let f = Func::from_name(&name).ok_or_else(|| anyhow!("位置 {at}: 未知函數「{name}」"))?;
                    let mut args = Vec::new();
                    if !self.eat_op(')') {
                        loop {
                            args.push(self.expr()?);
                            if self.eat_op(')') {
                                break;
                            }
                            self.expect_op(',')?;
                        }
                    }
                    let (lo, hi) = f.arity();
                    if args.len() < lo || args.len() > hi || (f == Func::Pwl && args.len() % 2 != 0) {
                        bail!("位置 {at}: {name}() 的參數個數不對（收到 {}）", args.len());
                    }
                    return Ok(Expr::Fn(f, args));
                }
                if let Some(v) = self.vars.get(&name) {
                    return Ok(v.clone());
                }
                match name.as_str() {
                    "S" | "s" | "x" | "ST" => Ok(Expr::S),
                    "spot" | "S0" | "F" => Ok(Expr::Spot),
                    _ => bail!("位置 {at}: 未定義的名稱「{name}」（終值價格請用 S，現價用 spot）"),
                }
            }
            Some(Tok::Op(c)) => bail!("位置 {at}: 不預期的「{c}」"),
            Some(Tok::Sep) | None => bail!("位置 {at}: 運算式不完整"),
        }
    }
}

/// A parsed payoff program: variable bindings followed by one payoff expression.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Program {
    pub source: String,
    pub expr: Expr,
}

impl Program {
    pub fn eval(&self, s: f64, spot: f64) -> f64 {
        self.expr.eval(Ctx { s, spot })
    }
}

/// Parse a payoff program. Statements are separated by `;` or new lines; `name = expr`
/// binds a variable (inlined into later statements); the last statement is the payoff.
pub fn parse(src: &str) -> Result<Program> {
    let toks = lex(src)?;
    let mut p = Parser { toks: &toks, pos: 0, vars: HashMap::new() };
    let mut last: Option<Expr> = None;
    while p.pos < toks.len() {
        if p.peek() == Some(&Tok::Sep) {
            p.pos += 1;
            continue;
        }
        // assignment?
        if let (Some(Tok::Ident(name)), Some((Tok::Op('='), _))) = (p.peek().cloned(), toks.get(p.pos + 1)) {
            if matches!(name.as_str(), "S" | "s" | "x" | "spot") || Func::from_name(&name).is_some() {
                bail!("「{name}」是保留字，不能當變數名稱");
            }
            p.pos += 2;
            let e = p.expr()?;
            p.vars.insert(name, e);
            last = None;
        } else {
            last = Some(p.expr()?);
        }
        match p.peek() {
            None | Some(Tok::Sep) => {}
            Some(_) => bail!("位置 {}: 多餘的內容", p.at()),
        }
    }
    let expr = last.ok_or_else(|| anyhow!("沒有 payoff 運算式（最後一行應是 S 的函數）"))?;
    Ok(Program { source: src.to_string(), expr })
}

// ---------------------------------------------------------------------------
// Compiler
// ---------------------------------------------------------------------------

/// Replicating instrument.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum Instrument {
    /// Zero-coupon bond paying `qty` at expiry.
    Cash,
    /// `qty` shares of the underlying.
    Stock,
    Call(f64),
    Put(f64),
}

/// One leg of the replicating portfolio, per **one** unit of the payoff function.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct RepLeg {
    pub instrument: Instrument,
    pub qty: f64,
}

impl RepLeg {
    /// Payoff at expiry.
    pub fn payoff(&self, s: f64) -> f64 {
        self.qty
            * match self.instrument {
                Instrument::Cash => 1.0,
                Instrument::Stock => s,
                Instrument::Call(k) => (s - k).max(0.0),
                Instrument::Put(k) => (k - s).max(0.0),
            }
    }
    pub fn strike(&self) -> Option<f64> {
        match self.instrument {
            Instrument::Call(k) | Instrument::Put(k) => Some(k),
            _ => None,
        }
    }
}

/// Which side of the pivot strike uses puts vs calls.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum LegStyle {
    /// Choose the put/call split that needs the smallest stock position (fewest
    /// instruments; e.g. a bull spread compiles to two calls, a protective put to
    /// bond + call — the parity-equivalent "fiduciary call"). Default.
    #[default]
    Minimal,
    /// Puts below the pivot, calls above (liquid OTM options).
    OutOfTheMoney,
    AllCalls,
    AllPuts,
}

/// Where the compiler may place strikes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum StrikeGrid {
    /// Detect the function's own kinks and round them to multiples of `step`
    /// (falls back to a regular grid with that step for smooth functions).
    Free { step: f64 },
    /// Only these (listed) strikes.
    Listed(Vec<f64>),
}

/// Compiler options.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CompileOptions {
    pub spot: f64,
    pub grid: StrikeGrid,
    pub style: LegStyle,
    /// Pivot between puts and calls (default: spot).
    pub pivot: Option<f64>,
    /// Range used to measure the replication error, as multiples of spot.
    pub error_range: (f64, f64),
    /// Smooth functions on a free grid use at most this many strikes.
    pub max_strikes: usize,
}

impl CompileOptions {
    pub fn free(spot: f64) -> Self {
        Self {
            spot,
            grid: StrikeGrid::Free { step: crate::strategy::strike_step(spot) },
            style: LegStyle::default(),
            pivot: None,
            error_range: (0.5, 1.6),
            max_strikes: 60,
        }
    }
    pub fn listed(spot: f64, strikes: Vec<f64>) -> Self {
        Self { grid: StrikeGrid::Listed(strikes), ..Self::free(spot) }
    }
}

/// Result of compiling a payoff program.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Replication {
    pub legs: Vec<RepLeg>,
    /// Breakpoints of the replicated (piecewise-linear) payoff.
    pub kinks: Vec<f64>,
    /// max |f − g| over the error range.
    pub max_error: f64,
    /// mean |f − g| over the error range (uniform in S).
    pub mean_error: f64,
    /// True when the function was treated as smooth (grid approximation).
    pub approximated: bool,
    /// Strikes moved by snapping (original, snapped).
    pub snapped: Vec<(f64, f64)>,
    pub warnings: Vec<String>,
}

impl Replication {
    /// Payoff of the replicating portfolio at expiry.
    pub fn payoff(&self, s: f64) -> f64 {
        self.legs.iter().map(|l| l.payoff(s)).sum()
    }
    /// Canonical formula, e.g. `12.00 + 0.50·S + 1.00·(S−600)⁺ − 1.00·(550−S)⁺`.
    pub fn formula(&self) -> String {
        let mut out = String::new();
        for (i, l) in self.legs.iter().enumerate() {
            let (sign, q) = if l.qty < 0.0 { ("−", -l.qty) } else { ("+", l.qty) };
            let term = match l.instrument {
                Instrument::Cash => format!("{q:.2}"),
                Instrument::Stock => format!("{q:.3}·S"),
                Instrument::Call(k) => format!("{q:.3}·(S−{k:.2})⁺"),
                Instrument::Put(k) => format!("{q:.3}·({k:.2}−S)⁺"),
            };
            if i == 0 {
                out.push_str(if l.qty < 0.0 { "−" } else { "" });
            } else {
                out.push_str(&format!(" {sign} "));
            }
            out.push_str(&term);
        }
        if out.is_empty() {
            "0".into()
        } else {
            out
        }
    }
}

const EPS_W: f64 = 1e-9;

/// Compile a payoff program to cash + stock + options.
pub fn compile(prog: &Program, opt: &CompileOptions) -> Result<Replication> {
    let spot = opt.spot;
    if spot.is_nan() || spot <= 0.0 {
        bail!("spot must be > 0");
    }
    let f = |s: f64| prog.eval(s, spot);
    // sanity: finite on the working range
    let probe_hi = spot * 4.0;
    for i in 0..=200 {
        let s = probe_hi * i as f64 / 200.0;
        if !f(s).is_finite() {
            bail!("payoff 在 S = {s:.2} 不是有限值（例如 ln(0) 或除以 0）；請用 max/clamp 限制定義域");
        }
    }
    let mut warnings: Vec<String> = Vec::new();
    let mut snapped: Vec<(f64, f64)> = Vec::new();
    let mut approximated = false;

    // breakpoints (strike, weight) of g = f(0) + s0·S + Σ w (S−K)⁺
    let (intercept, s0, mut nodes): (f64, f64, Vec<(f64, f64)>) = match &opt.grid {
        StrikeGrid::Free { step } => {
            let step = if *step > 0.0 { *step } else { crate::strategy::strike_step(spot) };
            match detect_kinks(&f, 0.0, probe_hi) {
                Some((s0, kinks, jumps)) => {
                    let mut nodes: Vec<(f64, f64)> = Vec::new();
                    for (k, w) in kinks {
                        let ks = ((k / step).round() * step).max(step);
                        if (ks - k).abs() > 1e-6 * spot {
                            snapped.push((k, ks));
                        }
                        nodes.push((ks, w));
                    }
                    // a jump of size J at x becomes a call spread J/step·[(S−K)⁺ − (S−K−step)⁺]
                    // on the strike grid, with K the grid point nearest to x − step/2
                    if !jumps.is_empty() {
                        warnings.push("不連續（跳躍）以相鄰履約價的窄價差近似".to_string());
                    }
                    for (x, jsize) in jumps {
                        let k = (((x - 0.5 * step) / step).round() * step).max(step);
                        snapped.push((x, k));
                        nodes.push((k, jsize / step));
                        nodes.push((k + step, -jsize / step));
                    }
                    (f(0.0), s0, nodes)
                }
                None => {
                    approximated = true;
                    // smooth: regular grid over [0.3, 2.5]·spot
                    let (lo, hi) = (0.3 * spot, 2.5 * spot);
                    let mut st = step;
                    while (hi - lo) / st > opt.max_strikes as f64 {
                        st *= 2.0;
                    }
                    let k0 = (lo / st).ceil() * st;
                    let grid: Vec<f64> = (0..).map(|i| k0 + i as f64 * st).take_while(|k| *k <= hi).collect();
                    interpolate_on_grid(&f, &grid)
                }
            }
        }
        StrikeGrid::Listed(ks) => {
            let mut grid: Vec<f64> = ks.iter().copied().filter(|k| k.is_finite() && *k > 0.0).collect();
            grid.sort_by(|a, b| a.partial_cmp(b).unwrap());
            grid.dedup_by(|a, b| (*a - *b).abs() < 1e-9);
            if grid.len() < 2 {
                bail!("可用履約價不足（至少需要 2 個）");
            }
            let (i, s, n) = interpolate_on_grid(&f, &grid);
            // only meaningful when the function actually has curvature on the grid
            approximated = detect_kinks(&f, 0.0, probe_hi).is_none();
            (i, s, n)
        }
    };
    // merge equal strikes, drop zero weights
    nodes.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());
    let mut merged: Vec<(f64, f64)> = Vec::new();
    for (k, w) in nodes {
        match merged.last_mut() {
            Some(last) if (last.0 - k).abs() < 1e-9 => last.1 += w,
            _ => merged.push((k, w)),
        }
    }
    let scale = 1.0 + s0.abs() + merged.iter().map(|n| n.1.abs()).fold(0.0, f64::max);
    merged.retain(|n| n.1.abs() > EPS_W * scale);

    // keep the curve anchored where it matters: after snapping, shift the bond leg so
    // that the replicated payoff equals the target at spot (snapping error moves to the tail)
    let g_at = |x: f64| intercept + s0 * x + merged.iter().map(|(k, w)| w * (x - k).max(0.0)).sum::<f64>();
    let anchor_shift = if rep_needs_anchor(&opt.grid) { f(spot) - g_at(spot) } else { 0.0 };
    let intercept = intercept + anchor_shift;

    // lower to instruments
    let pivot = match (opt.style, opt.pivot) {
        (_, Some(p)) => p,
        (LegStyle::Minimal, None) => {
            // candidate pivots: below all strikes, between consecutive strikes, above all
            let mut cands: Vec<f64> = vec![f64::NEG_INFINITY];
            for w in merged.windows(2) {
                cands.push(0.5 * (w[0].0 + w[1].0));
            }
            cands.push(f64::INFINITY);
            let stock_at = |p: f64| s0 + merged.iter().filter(|(k, _)| *k < p).map(|(_, w)| w).sum::<f64>();
            let cash_at = |p: f64| intercept - merged.iter().filter(|(k, _)| *k < p).map(|(k, w)| w * k).sum::<f64>();
            let dist = |p: f64| if p.is_finite() { (p - spot).abs() } else { spot * 10.0 };
            let tol = 1e-9 * (1.0 + spot);
            cands
                .into_iter()
                .min_by(|a, b| {
                    let (sa, sb) = (stock_at(*a).abs(), stock_at(*b).abs());
                    let (ca, cb) = (cash_at(*a).abs(), cash_at(*b).abs());
                    if (sa - sb).abs() > 1e-9 {
                        sa.partial_cmp(&sb).unwrap()
                    } else if (ca - cb).abs() > tol {
                        ca.partial_cmp(&cb).unwrap()
                    } else {
                        dist(*a).partial_cmp(&dist(*b)).unwrap()
                    }
                })
                .unwrap_or(spot)
        }
        _ => spot,
    };
    let mut cash = intercept;
    let mut stock = s0;
    let mut legs: Vec<RepLeg> = Vec::new();
    for &(k, w) in &merged {
        let as_put = match opt.style {
            LegStyle::AllCalls => false,
            LegStyle::AllPuts => true,
            LegStyle::OutOfTheMoney | LegStyle::Minimal => k < pivot,
        };
        if as_put {
            // w (S−K)⁺ = w (K−S)⁺ + w S − w K
            legs.push(RepLeg { instrument: Instrument::Put(k), qty: w });
            stock += w;
            cash -= w * k;
        } else {
            legs.push(RepLeg { instrument: Instrument::Call(k), qty: w });
        }
    }
    let mut all = Vec::new();
    if cash.abs() > 1e-9 * (1.0 + spot) {
        all.push(RepLeg { instrument: Instrument::Cash, qty: cash });
    }
    if stock.abs() > 1e-9 {
        all.push(RepLeg { instrument: Instrument::Stock, qty: stock });
    }
    all.extend(legs);

    let mut rep = Replication { legs: all, kinks: merged.iter().map(|n| n.0).collect(), max_error: 0.0, mean_error: 0.0, approximated, snapped, warnings: Vec::new() };
    // error
    let (lo, hi) = (opt.error_range.0 * spot, opt.error_range.1 * spot);
    let n = 2000;
    let (mut mx, mut sum) = (0.0_f64, 0.0);
    for i in 0..=n {
        let s = lo + (hi - lo) * i as f64 / n as f64;
        let e = (f(s) - rep.payoff(s)).abs();
        mx = mx.max(e);
        sum += e;
    }
    rep.max_error = mx;
    rep.mean_error = sum / (n + 1) as f64;
    if rep.approximated {
        warnings.push("非分段線性函數：以履約價網格做分段線性近似（Carr–Madan 靜態複製），請參考誤差".into());
    }
    if !rep.snapped.is_empty() {
        warnings.push(format!("{} 個轉折點已對齊到可交易履約價", rep.snapped.len()));
    }
    let right_slope = rep.legs.iter().map(|l| match l.instrument {
        Instrument::Stock => l.qty,
        Instrument::Call(_) => l.qty,
        _ => 0.0,
    });
    let rs: f64 = right_slope.sum();
    if rs < -1e-9 {
        warnings.push(format!("S→∞ 時斜率為 {rs:.2}：上方虧損無上限（等同裸賣買權/放空）"));
    }
    if rep.legs.iter().any(|l| matches!(l.instrument, Instrument::Put(_)) && l.qty < 0.0) {
        warnings.push("含賣出賣權：下方有大額虧損風險".into());
    }
    rep.warnings = warnings;
    Ok(rep)
}

fn rep_needs_anchor(grid: &StrikeGrid) -> bool {
    matches!(grid, StrikeGrid::Free { .. })
}

/// Piecewise-linear interpolation of `f` at grid points: returns
/// (intercept at S=0 of the left extension, left slope, [(K, slope change)]).
fn interpolate_on_grid(f: &dyn Fn(f64) -> f64, grid: &[f64]) -> (f64, f64, Vec<(f64, f64)>) {
    let n = grid.len();
    let gap = (grid[n - 1] - grid[0]) / (n as f64 - 1.0).max(1.0);
    let h = (0.25 * gap).min(grid[0] * 0.5).max(1e-6);
    let y: Vec<f64> = grid.iter().map(|&k| f(k)).collect();
    let s_left = (y[0] - f(grid[0] - h)) / h;
    let s_right = (f(grid[n - 1] + h) - y[n - 1]) / h;
    let mut slopes = Vec::with_capacity(n + 1);
    slopes.push(s_left);
    for i in 0..n - 1 {
        slopes.push((y[i + 1] - y[i]) / (grid[i + 1] - grid[i]));
    }
    slopes.push(s_right);
    let nodes: Vec<(f64, f64)> = (0..n).map(|i| (grid[i], slopes[i + 1] - slopes[i])).collect();
    // g(S) = y0 + s_left (S − K0) + Σ w (S−K)⁺ ⇒ intercept at 0 = y0 − s_left·K0
    (y[0] - s_left * grid[0], s_left, nodes)
}

/// Detect kinks of a piecewise-linear function on [lo, hi]. Returns
/// (slope at the left end, [(kink, slope change)]) or `None` when the function looks
/// smooth (too many slope changes). Jumps are turned into steep ramps of width 0.2% of hi.
/// Kink detection result: (left slope, kinks [(x, Δslope)], jumps [(x, size)]).
type Kinks = (f64, Vec<(f64, f64)>, Vec<(f64, f64)>);

fn detect_kinks(f: &dyn Fn(f64) -> f64, lo: f64, hi: f64) -> Option<Kinks> {
    let n = 8000usize;
    let h = (hi - lo) / n as f64;
    let x: Vec<f64> = (0..=n).map(|i| lo + h * i as f64).collect();
    let y: Vec<f64> = x.iter().map(|&s| f(s)).collect();
    let m: Vec<f64> = (0..n).map(|i| (y[i + 1] - y[i]) / h).collect();
    let mag = 1.0 + m.iter().map(|v| v.abs()).fold(0.0, f64::max).min(1e6);
    let tol = 1e-7 * mag.max(1.0) + 1e-9;
    let mut kinks: Vec<(f64, f64)> = Vec::new();
    let mut jumps: Vec<(f64, f64)> = Vec::new();
    let mut i = 1;
    while i < n {
        if (m[i] - m[i - 1]).abs() > tol {
            // the change region spans segments [i .. j); left slope m[i-1], right slope m[j]
            let mut j = i + 1;
            while j < n && (m[j] - m[j - 1]).abs() > tol && j - i < 3 {
                j += 1;
            }
            let ml = m[i - 1];
            let mr = if j < n { m[j] } else { m[n - 1] };
            let (xa, ya) = (x[i], y[i]);
            let (xb, yb) = (x[j.min(n)], y[j.min(n)]);
            // net jump not explained by the slopes
            if (ml - mr).abs() > tol {
                let k = (yb - ya + ml * xa - mr * xb) / (ml - mr);
                if k >= xa - h && k <= xb + h {
                    kinks.push((k, mr - ml));
                } else {
                    // a jump plus a kink — model as ramp over the change region
                    kinks.push((xa, (yb - ya) / (xb - xa) - ml));
                    kinks.push((xb, mr - (yb - ya) / (xb - xa)));
                }
            } else {
                // pure jump of size J between xa and xb (same slope both sides)
                let jump = yb - ya - ml * (xb - xa);
                if jump.abs() > 1e-9 * (1.0 + ya.abs()) {
                    jumps.push((0.5 * (xa + xb), jump));
                }
            }
            i = j + 1;
            if kinks.len() + jumps.len() > 40 {
                return None;
            }
        } else {
            i += 1;
        }
    }
    Some((m[0], kinks, jumps))
}

// ---------------------------------------------------------------------------
// Pricing
// ---------------------------------------------------------------------------

/// Synthetic market with a parametric equity smile (see [`SyntheticMarket::iv`]).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct SyntheticMarket {
    pub spot: f64,
    /// Years to expiry.
    pub t: f64,
    pub r: f64,
    pub q: f64,
    pub atm_iv: f64,
    /// Put skew: relative IV increase per unit of −ln(K/F)/√T. 0 = flat (Black–Scholes).
    pub skew: f64,
    pub curv: f64,
    /// Half bid/ask spread as a fraction of mid (min $0.05).
    pub half_spread_pct: f64,
}

impl SyntheticMarket {
    pub fn new(spot: f64, t: f64, r: f64, q: f64, atm_iv: f64) -> Self {
        Self { spot, t, r, q, atm_iv, skew: 0.30, curv: 0.03, half_spread_pct: 0.01 }
    }
    pub fn forward(&self) -> f64 {
        self.spot * ((self.r - self.q) * self.t).exp()
    }
    /// Smile `atm·exp(−skew·x·w(x) + curv·x²)` with `x = ln(K/F)/√T` clamped to ±3 and
    /// the call wing 60% flatter (`w = 0.4` for x > 0) — positive and arbitrage-benign in
    /// the wings, unlike a linear skew.
    pub fn iv(&self, k: f64) -> f64 {
        let x = ((k / self.forward()).ln() / self.t.max(1e-6).sqrt()).clamp(-3.0, 3.0);
        let w = if x > 0.0 { 0.4 } else { 1.0 };
        (self.atm_iv * (-self.skew * x * w + self.curv * x * x).exp()).clamp(0.03, 3.0)
    }
}

/// Market used to price a replication.
#[derive(Debug, Clone)]
pub enum Market {
    Synthetic(SyntheticMarket),
    Chain { chain: OptionChain, expiry: NaiveDate, r: f64, q: f64, surface: VolSurface },
}

impl Market {
    pub fn chain(chain: OptionChain, expiry: NaiveDate, r: f64, q: f64) -> Self {
        let surface = fit_surface(&chain, r, q);
        Market::Chain { chain, expiry, r, q, surface }
    }
    pub fn spot(&self) -> f64 {
        match self {
            Market::Synthetic(m) => m.spot,
            Market::Chain { chain, .. } => chain.spot,
        }
    }
    pub fn t(&self) -> f64 {
        match self {
            Market::Synthetic(m) => m.t,
            Market::Chain { chain, expiry, .. } => ((*expiry - chain.as_of_date()).num_days().max(1) as f64) / 365.0,
        }
    }
    pub fn r(&self) -> f64 {
        match self {
            Market::Synthetic(m) => m.r,
            Market::Chain { r, .. } => *r,
        }
    }
    pub fn q(&self) -> f64 {
        match self {
            Market::Synthetic(m) => m.q,
            Market::Chain { q, .. } => *q,
        }
    }
    /// Listed strikes available for compilation (chain) or `None` (synthetic).
    pub fn listed_strikes(&self) -> Option<Vec<f64>> {
        match self {
            Market::Chain { chain, expiry, .. } => Some(chain.strikes(*expiry)),
            _ => None,
        }
    }
    /// Model implied vol at strike `k` (smile).
    pub fn iv(&self, k: f64) -> f64 {
        match self {
            Market::Synthetic(m) => m.iv(k),
            Market::Chain { surface, expiry, chain, .. } => surface
                .iv_at_expiry(k, *expiry)
                .or_else(|| chain.atm_iv(*expiry))
                .filter(|v| v.is_finite() && *v > 0.0)
                .unwrap_or(0.2)
                .clamp(0.03, 3.0),
        }
    }
    /// (mid, bid, ask, iv, from_quote) of one option.
    pub fn option_quote(&self, kind: OptionKind, k: f64) -> (f64, f64, f64, f64, bool) {
        let (s, t, r, q) = (self.spot(), self.t(), self.r(), self.q());
        if let Market::Chain { chain, expiry, .. } = self {
            if let Some(qt) = chain.find(*expiry, k, kind) {
                if qt.bid > 0.0 && qt.ask >= qt.bid {
                    let iv = qt.iv.filter(|v| *v > 0.0).unwrap_or_else(|| self.iv(k));
                    return (qt.mid(), qt.bid, qt.ask, iv, true);
                }
            }
        }
        let iv = self.iv(k);
        let mid = bs_price(kind, s, k, t, r, q, iv);
        let hs = match self {
            Market::Synthetic(m) => (m.half_spread_pct * mid).max(0.05),
            _ => (0.02 * mid).max(0.05),
        };
        (mid, (mid - hs).max(0.0), mid + hs, iv, false)
    }
}

/// Priced leg.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct PricedLeg {
    pub leg: RepLeg,
    /// Per unit of instrument.
    pub mid: f64,
    pub bid: f64,
    pub ask: f64,
    pub iv: f64,
    /// Price came from a real quote (chain mode).
    pub quoted: bool,
    /// qty × mid.
    pub value: f64,
    /// Cost when buying at ask / selling at bid.
    pub exec_value: f64,
    pub greeks: Greeks,
}

/// Distribution of the payoff under one measure.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PayoffStats {
    /// E[payoff].
    pub mean: f64,
    /// P(payoff > cost grown at the risk-free rate) — the trade makes money.
    pub prob_profit: f64,
    pub p05: f64,
    pub p50: f64,
    pub p95: f64,
    /// Expected P&L = E[payoff] − cost·e^{rT}.
    pub expected_pnl: f64,
}

/// Full pricing report.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PricedPayoff {
    pub legs: Vec<PricedLeg>,
    pub spot: f64,
    pub t: f64,
    pub r: f64,
    pub q: f64,
    /// Units of the payoff function bought (e.g. 100 = one option contract per unit qty).
    pub units: f64,
    /// Price of ONE unit at mid.
    pub price_mid: f64,
    /// Price of ONE unit when crossing the spread.
    pub price_exec: f64,
    /// Model fair value of the *target* function: e^{−rT} ∫ f(S) q(S) dS.
    pub fair_target: f64,
    /// Same for the replicated payoff (checks the density against the leg prices).
    pub fair_replicated: f64,
    /// Greeks of one unit (delta in shares, theta $/day, vega $/vol pt).
    pub greeks: Greeks,
    pub risk_neutral: PayoffStats,
    pub real_world: PayoffStats,
    /// Annual drift assumed for the real-world measure.
    pub mu: f64,
    /// Density grid (S, q_Q(S), q_P(S)) for plotting.
    pub density: Vec<(f64, f64, f64)>,
    /// Break-evens of P&L at expiry (payoff = cost·e^{rT}).
    pub breakevens: Vec<f64>,
}

/// Price a replication. `mu` = real-world annual drift of the underlying (e.g. 0.08).
pub fn price(prog: &Program, rep: &Replication, mkt: &Market, units: f64, mu: f64) -> PricedPayoff {
    let (s, t, r, q) = (mkt.spot(), mkt.t(), mkt.r(), mkt.q());
    let df = (-r * t).exp();
    let mut legs = Vec::new();
    let mut g = Greeks::default();
    let (mut pm, mut pe) = (0.0, 0.0);
    for leg in &rep.legs {
        let (mid, bid, ask, iv, quoted, gk) = match leg.instrument {
            Instrument::Cash => (df, df, df, 0.0, false, Greeks { price: df, rho: -t * df * 0.01, ..Default::default() }),
            Instrument::Stock => {
                let p = s * (-q * t).exp();
                (p, p, p, 0.0, false, Greeks { price: p, delta: (-q * t).exp(), ..Default::default() })
            }
            Instrument::Call(k) | Instrument::Put(k) => {
                let kind = if matches!(leg.instrument, Instrument::Call(_)) { OptionKind::Call } else { OptionKind::Put };
                let (mid, bid, ask, iv, quoted) = mkt.option_quote(kind, k);
                let mut gk = bs_greeks(kind, s, k, t, r, q, iv);
                gk.price = mid;
                (mid, bid, ask, iv, quoted, gk)
            }
        };
        let value = leg.qty * mid;
        let exec_value = if leg.qty >= 0.0 { leg.qty * ask } else { leg.qty * bid };
        pm += value;
        pe += exec_value;
        g = g + gk * leg.qty;
        legs.push(PricedLeg { leg: *leg, mid, bid, ask, iv, quoted, value, exec_value, greeks: gk * leg.qty });
    }
    g.price = pm;

    // risk-neutral density from the smile (Breeden–Litzenberger on call prices)
    let atm = mkt.iv(s * ((r - q) * t).exp());
    let sd = atm * t.sqrt();
    let lo = (s * (-8.0 * sd - 0.5 * sd * sd).exp()).max(s * 0.005);
    let hi = s * (8.0 * sd).exp();
    let n = 3000;
    let hk = (hi - lo) / n as f64;
    let call_px = |k: f64| bs_price(OptionKind::Call, s, k, t, r, q, mkt.iv(k));
    let mut xs = Vec::with_capacity(n + 1);
    let mut qd = Vec::with_capacity(n + 1);
    for i in 0..=n {
        let k = lo + hk * i as f64;
        let d2 = (call_px(k + hk) - 2.0 * call_px(k) + call_px((k - hk).max(1e-6))) / (hk * hk);
        xs.push(k);
        qd.push((d2 / df).max(0.0));
    }
    let mass: f64 = qd.iter().sum::<f64>() * hk;
    if mass > 0.0 {
        for v in &mut qd {
            *v /= mass;
        }
    }
    // real-world: Esscher tilt q·(S/F)^θ so that E_P[S] = F·e^{(μ − (r − q))T}
    let fwd = s * ((r - q) * t).exp();
    let target_mean = fwd * ((mu - (r - q)) * t).exp();
    let tilt = |th: f64| -> Vec<f64> {
        let mut p: Vec<f64> = xs.iter().zip(&qd).map(|(x, v)| v * (x / fwd).powf(th)).collect();
        let m: f64 = p.iter().sum::<f64>() * hk;
        if m > 0.0 {
            for v in &mut p {
                *v /= m;
            }
        }
        p
    };
    let mean_of = |p: &[f64]| xs.iter().zip(p).map(|(x, v)| x * v).sum::<f64>() * hk;
    let (mut a, mut b) = (-20.0, 20.0);
    for _ in 0..80 {
        let mid = 0.5 * (a + b);
        if mean_of(&tilt(mid)) < target_mean {
            a = mid;
        } else {
            b = mid;
        }
    }
    let pd = tilt(0.5 * (a + b));

    let ft = |x: f64| prog.eval(x, s);
    let fair_target = df * xs.iter().zip(&qd).map(|(x, v)| ft(*x) * v).sum::<f64>() * hk;
    let fair_replicated = df * xs.iter().zip(&qd).map(|(x, v)| rep.payoff(*x) * v).sum::<f64>() * hk;
    let cost_fv = pm / df;
    let stats = |p: &[f64]| -> PayoffStats {
        let mut pts: Vec<(f64, f64)> = xs.iter().zip(p).map(|(x, w)| (rep.payoff(*x), w * hk)).collect();
        let mean: f64 = pts.iter().map(|(v, w)| v * w).sum();
        let prob_profit: f64 = pts.iter().filter(|(v, _)| *v > cost_fv + 1e-9).map(|(_, w)| w).sum();
        pts.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());
        let q = |p: f64| {
            let mut acc = 0.0;
            for (v, w) in &pts {
                acc += w;
                if acc >= p {
                    return *v;
                }
            }
            pts.last().map_or(0.0, |x| x.0)
        };
        PayoffStats { mean, prob_profit, p05: q(0.05), p50: q(0.5), p95: q(0.95), expected_pnl: mean - cost_fv }
    };
    let risk_neutral = stats(&qd);
    let real_world = stats(&pd);
    // break-evens of P&L at expiry over a wide range
    let mut breakevens = Vec::new();
    let bl = s * 0.05;
    let bh = s * 3.0;
    let nb = 3000;
    let pnl = |x: f64| rep.payoff(x) - cost_fv;
    let mut prev = pnl(bl);
    for i in 1..=nb {
        let x = bl + (bh - bl) * i as f64 / nb as f64;
        let v = pnl(x);
        if prev.signum() != v.signum() && v != 0.0 {
            let x0 = x - (bh - bl) / nb as f64;
            breakevens.push(x0 + (x - x0) * prev.abs() / (prev.abs() + v.abs()));
        }
        prev = v;
    }
    let density = xs.iter().zip(qd.iter().zip(&pd)).step_by(10).map(|(x, (a, b))| (*x, *a, *b)).collect();
    PricedPayoff { legs, spot: s, t, r, q, units, price_mid: pm, price_exec: pe, fair_target, fair_replicated, greeks: g, risk_neutral, real_world, mu, density, breakevens }
}

/// Value of the replicating portfolio before expiry, as a function of the spot, with
/// `t_remaining` years left and each option at its pricing IV (sticky strike).
pub fn value_curve(priced: &PricedPayoff, spots: &[f64], t_remaining: f64) -> Vec<(f64, f64)> {
    let (r, q) = (priced.r, priced.q);
    spots
        .iter()
        .map(|&x| {
            let v: f64 = priced
                .legs
                .iter()
                .map(|pl| {
                    let l = pl.leg;
                    l.qty
                        * match l.instrument {
                            Instrument::Cash => (-r * t_remaining).exp(),
                            Instrument::Stock => x * (-q * t_remaining).exp(),
                            Instrument::Call(k) => bs_price(OptionKind::Call, x, k, t_remaining, r, q, pl.iv),
                            Instrument::Put(k) => bs_price(OptionKind::Put, x, k, t_remaining, r, q, pl.iv),
                        }
                })
                .sum();
            (x, v)
        })
        .collect()
}

/// Built-in example programs (name, source, Chinese explanation).
pub fn examples() -> Vec<(&'static str, &'static str, &'static str)> {
    vec![
        ("保護性賣權", "S + put(95%)", "持股 + 95% 賣權：下跌損失被封頂在 5%（加權利金）。"),
        ("領口", "collar(90%, 115%)", "持股 + 買 90% 賣權 + 賣 115% 買權：用上方換下方保護，常可接近零成本。"),
        ("多頭價差", "bull(100%, 110%)", "只參與 100%–110% 區間的上漲，成本遠低於買權。"),
        ("跨式", "straddle(100%)", "不賭方向，只賭波動大：|S − K|。"),
        ("2 倍上漲、封頂", "2*bull(100%, 115%)", "上漲時 2 倍參與到 +15%，下跌只賠權利金。"),
        ("零成本槓桿多頭", "call(100%) - put(85%)", "風險逆轉：賣 85% 賣權資助 ATM 買權，跌破 85% 才開始賠。"),
        ("數位（二元）", "100 * digital(105%)", "到期高於 105% 拿 100 元，否則 0（以窄價差近似）。"),
        ("緩衝型（Buffer）", "S + bear(90%, 100%) - call(112%)", "結構型商品常見：前 10% 跌幅由賣權價差吸收，上方封頂 12%。"),
        ("自訂折線", "pwl(80%, -10, 100%, 0, 120%, 40, 140%, 50)", "用點直接畫函數：跌到 80% 賠 10、漲到 120% 賺 40、之後平緩。"),
        ("平滑凸函數", "(S/spot)^2 * spot / 2", "非線性：二次曲線，以履約價網格近似（方差交換的核心）。"),
        ("對數報酬", "spot * ln(max(S, 1) / spot)", "對數合約：波動率交換的複製核心；只能近似。"),
        ("變數與組合", "k = 100%\nw = 10%\nbutterfly(k - w, k, k + w) * 10", "可以定義變數：在 ±10% 範圍內的蝶式，押價格停在現價附近。"),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(src: &str) -> Program {
        parse(src).unwrap()
    }

    #[test]
    fn parser_basics() {
        let pr = p("2*call(600) - call(650) + 3");
        assert_eq!(pr.eval(700.0, 600.0), 2.0 * 100.0 - 50.0 + 3.0);
        assert_eq!(p("110%").eval(0.0, 600.0), 660.0);
        assert_eq!(p("a = 600; b = a + 50\nbull(a, b)").eval(640.0, 600.0), 40.0);
        assert_eq!(p("2call(600)").eval(610.0, 600.0), 20.0);
        assert_eq!(p("-S + 2^3").eval(1.0, 1.0), 7.0);
        assert_eq!(p("pwl(0,0, 10,10, 20,0)").eval(15.0, 1.0), 5.0);
        assert_eq!(p("pwl(0,0, 10,10, 20,0)").eval(25.0, 1.0), 0.0);
        assert!(parse("call(1,2)").is_err());
        assert!(parse("foo(1)").is_err());
        assert!(parse("y + 1").is_err());
        assert!(parse("S +").is_err());
        assert!(parse("S = 3").is_err());
    }

    fn check_exact(src: &str, spot: f64) -> Replication {
        let pr = p(src);
        let rep = compile(&pr, &CompileOptions::free(spot)).unwrap();
        for i in 0..400 {
            let s = spot * (0.2 + 2.5 * i as f64 / 400.0);
            let (a, b) = (pr.eval(s, spot), rep.payoff(s));
            assert!((a - b).abs() < 1e-6 * (1.0 + a.abs()), "{src}: S={s} f={a} g={b} legs={:?}", rep.legs);
        }
        assert!(!rep.approximated, "{src}");
        rep
    }

    #[test]
    fn compiles_classic_structures_exactly() {
        let r = check_exact("S + put(570)", 600.0);
        // protective put: 1 stock + 1 put, no cash
        assert_eq!(r.legs.len(), 2, "{:?}", r.legs);
        let r = check_exact("bull(600, 650)", 600.0);
        assert!(r.legs.iter().any(|l| l.instrument == Instrument::Call(650.0) && (l.qty + 1.0).abs() < 1e-9));
        check_exact("straddle(600)", 600.0);
        check_exact("collar(540, 690)", 600.0);
        check_exact("butterfly(550, 600, 650) * 3 - 2", 600.0);
        check_exact("call(600) - put(510)", 600.0);
        check_exact("max(S - 600, 0) + min(S - 500, 0)", 600.0);
        check_exact("clamp(S - 600, -50, 100)", 600.0);
        check_exact("pwl(480, -10, 600, 0, 720, 40, 840, 50)", 600.0);
        check_exact("2*S - 600", 600.0);
    }

    #[test]
    fn otm_style_uses_puts_below_and_calls_above() {
        let r = check_exact("straddle(600)", 600.0);
        assert!(r.legs.iter().any(|l| matches!(l.instrument, Instrument::Call(_))));
        // the kink sits at the pivot; with OTM style K < pivot ⇒ put, K = pivot ⇒ call + stock
        let r2 = compile(&p("straddle(600)"), &CompileOptions { pivot: Some(601.0), style: LegStyle::OutOfTheMoney, ..CompileOptions::free(600.0) }).unwrap();
        assert!(r2.legs.iter().any(|l| matches!(l.instrument, Instrument::Put(_))));
        assert!(r.formula().contains("(S−600.00)⁺"));
    }

    #[test]
    fn minimal_style_picks_natural_legs() {
        let r = check_exact("bull(600, 650)", 600.0);
        assert!(r.legs.iter().all(|l| matches!(l.instrument, Instrument::Call(_))), "{:?}", r.legs);
        let r = check_exact("bear(550, 600)", 600.0);
        assert!(r.legs.iter().all(|l| matches!(l.instrument, Instrument::Put(_))), "{:?}", r.legs);
        // protective put = bond + call (fiduciary call), no stock
        let r = check_exact("S + put(570)", 600.0);
        assert!(!r.legs.iter().any(|l| l.instrument == Instrument::Stock), "{:?}", r.legs);
        // OTM style keeps stock + put
        let r = compile(&p("S + put(570)"), &CompileOptions { style: LegStyle::OutOfTheMoney, ..CompileOptions::free(600.0) }).unwrap();
        assert!(r.legs.iter().any(|l| l.instrument == Instrument::Stock));
        assert!(r.legs.iter().any(|l| l.instrument == Instrument::Put(570.0)));
    }

    #[test]
    fn snapping_and_digital_and_smooth() {
        // kink at 603 snaps to 605 (step 5 for spot 600): exact at and above spot... the
        // anchor at spot keeps g(600)=f(600)=0 and the error is ≤ the snap distance
        let r = compile(&p("call(603)"), &CompileOptions::free(600.0)).unwrap();
        assert_eq!(r.snapped.len(), 1);
        assert!(r.max_error > 1.9 && r.max_error < 2.1, "{}", r.max_error);
        // protective put snapped: error only below the kink, none above
        let r = compile(&p("S + put(95%)"), &CompileOptions::free(685.28)).unwrap();
        assert!((r.payoff(800.0) - 800.0).abs() < 1e-9, "{}", r.payoff(800.0));
        // digital → tight call spread
        let r = compile(&p("100*digital(630)"), &CompileOptions::free(600.0)).unwrap();
        assert!(r.legs.iter().filter(|l| matches!(l.instrument, Instrument::Call(_))).count() >= 2);
        assert!((r.payoff(700.0) - 100.0).abs() < 1e-6 && r.payoff(500.0).abs() < 1e-6);
        // smooth function → approximation with bounded error
        let r = compile(&p("(S/spot)^2 * spot / 2"), &CompileOptions::free(600.0)).unwrap();
        assert!(r.approximated);
        assert!(r.max_error < 15.0, "{}", r.max_error);
    }

    #[test]
    fn listed_strikes_interpolation() {
        let ks: Vec<f64> = (0..=40).map(|i| 400.0 + 10.0 * i as f64).collect();
        let pr = p("bull(600, 650)");
        let r = compile(&pr, &CompileOptions::listed(600.0, ks.clone())).unwrap();
        assert!(r.max_error < 1e-6);
        // kink at 605 between listed 600/610 ⇒ split, small error
        let r = compile(&p("call(605)"), &CompileOptions::listed(600.0, ks)).unwrap();
        assert!(r.max_error > 0.0 && r.max_error <= 5.0 + 1e-9);
    }

    #[test]
    fn pricing_matches_black_scholes_and_parity() {
        let mkt = Market::Synthetic(SyntheticMarket { skew: 0.0, curv: 0.0, ..SyntheticMarket::new(600.0, 1.0, 0.04, 0.01, 0.2) });
        let pr = p("call(600)");
        let rep = compile(&pr, &CompileOptions::free(600.0)).unwrap();
        let px = price(&pr, &rep, &mkt, 100.0, 0.08);
        let bs = bs_price(OptionKind::Call, 600.0, 600.0, 1.0, 0.04, 0.01, 0.2);
        assert!((px.price_mid - bs).abs() < 1e-9, "{} vs {bs}", px.price_mid);
        // density-based fair value agrees with the leg price
        assert!((px.fair_target - bs).abs() / bs < 0.01, "{} vs {bs}", px.fair_target);
        assert!(px.price_exec > px.price_mid);
        // a forward (S − K) priced by parity: S e^{−qT} − K e^{−rT}
        let pr = p("S - 600");
        let rep = compile(&pr, &CompileOptions::free(600.0)).unwrap();
        let px = price(&pr, &rep, &mkt, 1.0, 0.08);
        let want = 600.0 * (-0.01f64).exp() - 600.0 * (-0.04f64).exp();
        assert!((px.price_mid - want).abs() < 1e-9);
        // real-world drift above r-q raises the call's expected payoff
        assert!(px.real_world.mean > px.risk_neutral.mean);
    }

    #[test]
    fn examples_all_compile_and_price() {
        let mkt = Market::Synthetic(SyntheticMarket::new(600.0, 1.0, 0.04, 0.006, 0.22));
        for (name, src, _) in examples() {
            let pr = parse(src).unwrap_or_else(|e| panic!("{name}: {e}"));
            let rep = compile(&pr, &CompileOptions::free(600.0)).unwrap_or_else(|e| panic!("{name}: {e}"));
            let px = price(&pr, &rep, &mkt, 100.0, 0.08);
            assert!(px.price_mid.is_finite(), "{name}");
            assert!(px.risk_neutral.mean.is_finite(), "{name}");
        }
    }

    #[test]
    fn chain_market_uses_listed_quotes() {
        use crate::chain::{ExerciseStyle, OptionQuote};
        let as_of = chrono::NaiveDate::from_ymd_opt(2026, 1, 2).unwrap().and_hms_opt(16, 0, 0).unwrap();
        let exp = chrono::NaiveDate::from_ymd_opt(2027, 1, 15).unwrap();
        let t = (exp - as_of.date()).num_days() as f64 / 365.0;
        let mut quotes = Vec::new();
        for i in 0..=30 {
            let k = 450.0 + 10.0 * i as f64;
            for kind in [OptionKind::Call, OptionKind::Put] {
                let iv = 0.2 + 0.1 * (600.0 / k - 1.0);
                let m = bs_price(kind, 600.0, k, t, 0.04, 0.006, iv);
                quotes.push(OptionQuote { contract: String::new(), expiry: exp, strike: k, kind, bid: m * 0.99, ask: m * 1.01, last: m, volume: 1.0, open_interest: 10.0, iv: Some(iv), delta: None, gamma: None, theta: None, vega: None });
            }
        }
        let chain = OptionChain { underlying: "TEST".into(), spot: 600.0, as_of, source: "test".into(), style: ExerciseStyle::American, quotes };
        let mkt = Market::chain(chain, exp, 0.04, 0.006);
        let pr = p("bull(600, 650)");
        let rep = compile(&pr, &CompileOptions::listed(600.0, mkt.listed_strikes().unwrap())).unwrap();
        let px = price(&pr, &rep, &mkt, 100.0, 0.08);
        assert!(px.legs.iter().filter(|l| l.leg.strike().is_some()).all(|l| l.quoted));
        assert!(px.price_mid > 0.0 && px.price_mid < 50.0);
    }
}
