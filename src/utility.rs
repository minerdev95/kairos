//! The operator utility — section 4 of the spec.
//!
//! The objective is not "maximize profit". It is maximize the expected utility
//! of the return distribution: a risk-averse utility, minus a drawdown penalty,
//! subject to a hard ruin constraint.
//!
//! ```text
//! U(R) = E[u(R)] − λ_dd · Drawdown(R)      subject to   P(ruin) ≤ ε
//! ```
//!
//! One operator "risk word" sets (γ, λ_dd, ε); every layer optimizes this single
//! object. Reliability (Part IV) is a term inside it, not an afterthought.

use crate::model::RiskWord;
use serde::{Deserialize, Serialize};

/// Concrete utility parameters. Derived from the risk word, overridable in TOML.
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct OperatorUtility {
    /// CRRA/CARA-style risk-aversion coefficient γ ≥ 0. Penalizes revenue
    /// variance in the certainty-equivalent.
    pub gamma: f64,
    /// Drawdown weight λ_dd ≥ 0. Multiplies a peak-to-trough penalty.
    pub lambda_dd: f64,
    /// Ruin tolerance ε ∈ (0,1). Tighter ε ⇒ smaller committed fraction.
    pub ruin_eps: f64,
    /// Fraction of Kelly to bet (always < 1; full Kelly is too aggressive).
    pub kelly_fraction: f64,
    pub word: RiskWord,
}

impl OperatorUtility {
    /// Map the single risk word to the three knobs. These are deliberately
    /// moderate: even "aggressive" keeps a hard ruin guard.
    pub fn from_risk(word: RiskWord) -> Self {
        match word {
            // Leveraged public miner: high risk aversion, tiny ruin tolerance.
            RiskWord::Conservative => OperatorUtility {
                gamma: 3.0,
                lambda_dd: 1.5,
                ruin_eps: 0.005,
                kelly_fraction: 0.25,
                word,
            },
            RiskWord::Balanced => OperatorUtility {
                gamma: 1.2,
                lambda_dd: 0.7,
                ruin_eps: 0.02,
                kelly_fraction: 0.5,
                word,
            },
            // Cash-rich private farm: low risk aversion, larger drawdown tolerance.
            RiskWord::Aggressive => OperatorUtility {
                gamma: 0.4,
                lambda_dd: 0.25,
                ruin_eps: 0.08,
                kelly_fraction: 0.85,
                word,
            },
        }
    }

    /// Certainty-equivalent of a per-second revenue flow with mean `mu` and
    /// standard deviation `sigma` (both USD/s). Mean-variance approximation of a
    /// CRRA/CARA utility: CE = μ − ½ γ σ². This is what the opportunity scorer
    /// maximizes instead of raw expected revenue, so a high-variance coin must
    /// pay a premium to win.
    pub fn certainty_equivalent(&self, mu: f64, sigma: f64) -> f64 {
        mu - 0.5 * self.gamma * sigma * sigma
    }

    /// Drawdown-penalized value: subtract λ_dd times a realized drawdown (USD).
    /// Used by the portfolio allocator and the regret ledger.
    pub fn drawdown_penalized(&self, value: f64, drawdown: f64) -> f64 {
        value - self.lambda_dd * drawdown
    }

    /// The maximum fraction of the fleet's power budget the allocator may pour
    /// into a *single* opportunity, from a fractional-Kelly rule clamped by the
    /// ruin tolerance. Aggressive ⇒ concentrate; conservative ⇒ diversify.
    ///
    /// `edge` is the opportunity's risk-adjusted edge as a fraction of its own
    /// revenue (≥ 0); `odds_var` is its relative revenue variance (> 0).
    pub fn kelly_cap(&self, edge: f64, odds_var: f64) -> f64 {
        if odds_var <= 0.0 {
            return self.ruin_eps.max(0.0).min(1.0).max(0.05);
        }
        let kelly = (edge / odds_var).max(0.0);
        let sized = self.kelly_fraction * kelly;
        // Never expose more than the ruin tolerance lets us, never 0 (always
        // keep the fleet earning), never more than 1.
        sized.clamp(0.02, 1.0 - self.ruin_eps.min(0.5))
    }

    /// Hard cap on the share of total fleet hashpower committed to any one chain,
    /// part of the swarm/concentration safety story (never threaten a chain, and
    /// never over-concentrate the operator's own risk). Tighter when conservative.
    pub fn max_single_chain_share(&self) -> f64 {
        match self.word {
            RiskWord::Conservative => 0.5,
            RiskWord::Balanced => 0.7,
            RiskWord::Aggressive => 0.9,
        }
    }
}

impl Default for OperatorUtility {
    fn default() -> Self {
        OperatorUtility::from_risk(RiskWord::Balanced)
    }
}
