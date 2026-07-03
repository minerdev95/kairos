//! Power-efficiency optimizer — squeeze more **profit per watt**, not more hashrate.
//!
//! A GPU mining at its stock power limit is almost never at its most *profitable*
//! point: the last ~25–30 % of its power budget typically buys only ~5 % more
//! hashrate. Because revenue scales with hashrate but electricity scales with power,
//! the profit-maximizing power limit sits **below** stock — and it moves lower as
//! electricity gets more expensive or the coin gets less valuable. This module finds
//! that point.
//!
//!   profit(f) = revenue · relHash(f) − electricity · P_stock · f
//!
//! where `f` is the power fraction (power-limit ÷ stock power). `relHash` is a
//! concave undervolt curve calibrated to real GPU behaviour: ~94 % hashrate at 70 %
//! power, ~82 % at 50 %. We maximize profit over `f ∈ [min_frac, 1]`.

/// Relative hashrate retained when a GPU is power-limited to fraction `frac` of its
/// stock power. Concave: near-flat just under stock, falling off as you undervolt
/// hard. `relHash(1)=1.0`, `relHash(0.7)≈0.937`, `relHash(0.5)≈0.825`.
pub fn rel_hashrate(frac: f64) -> f64 {
    let f = frac.clamp(0.0, 1.0);
    (1.0 - 0.7 * (1.0 - f).powi(2)).clamp(0.0, 1.0)
}

/// The chosen operating point after profit-per-watt optimization.
#[derive(Clone, Copy, Debug)]
pub struct OpPoint {
    /// Power fraction of stock (1.0 = run at stock power).
    pub frac: f64,
    /// Chosen power draw in watts.
    pub power_w: f64,
    /// Resulting hashrate (hash/s).
    pub hashrate: f64,
    /// Net $/day at this optimized point.
    pub net_day: f64,
    /// Net $/day at stock power, for comparison.
    pub net_day_stock: f64,
}

impl OpPoint {
    /// Extra $/day the optimized power limit earns over running at stock.
    pub fn gain_day(&self) -> f64 {
        self.net_day - self.net_day_stock
    }
    /// Watts saved versus stock.
    pub fn watts_saved(&self, stock_power_w: f64) -> f64 {
        (stock_power_w - self.power_w).max(0.0)
    }
}

/// Find the power fraction that maximizes net $/day (revenue after electricity),
/// searching `f ∈ [min_frac, 1]`. `rev_per_s_stock` is USD/second at stock
/// hashrate; `energy_kwh` is $/kWh. For devices that can't be power-limited (CPU,
/// managed ASICs) pass `min_frac = 1.0` so the result is just the stock point.
pub fn optimize(
    rev_per_s_stock: f64,
    stock_power_w: f64,
    energy_kwh: f64,
    stock_hashrate: f64,
    min_frac: f64,
) -> OpPoint {
    let net_day = |f: f64| {
        let rev = rev_per_s_stock * rel_hashrate(f);
        let cost = stock_power_w * f * energy_kwh / 3_600_000.0;
        (rev - cost) * 86_400.0
    };
    let stock = net_day(1.0);
    let lo = min_frac.clamp(0.3, 1.0);
    // Coarse 1%-step sweep — robust for any curve shape, and cheap (≤70 evals).
    let mut best_f = 1.0;
    let mut best = stock;
    let mut f = lo;
    while f <= 1.0 + 1e-9 {
        let n = net_day(f);
        if n > best {
            best = n;
            best_f = f;
        }
        f += 0.01;
    }
    OpPoint {
        frac: best_f,
        power_w: stock_power_w * best_f,
        hashrate: stock_hashrate * rel_hashrate(best_f),
        net_day: best,
        net_day_stock: stock,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn curve_shape_is_concave_and_bounded() {
        assert!((rel_hashrate(1.0) - 1.0).abs() < 1e-9);
        assert!((rel_hashrate(0.7) - 0.937).abs() < 0.01);
        assert!((rel_hashrate(0.5) - 0.825).abs() < 0.01);
        assert!(rel_hashrate(0.0) >= 0.0);
        // strictly increasing over the useful band
        assert!(rel_hashrate(0.6) < rel_hashrate(0.8));
    }

    #[test]
    fn free_power_runs_at_stock() {
        // With zero electricity cost, max hashrate = max profit → run at stock.
        let op = optimize(1e-6, 300.0, 0.0, 1.0e9, 0.5);
        assert!((op.frac - 1.0).abs() < 1e-9);
        assert!(op.gain_day() >= 0.0);
    }

    #[test]
    fn expensive_power_undervolts() {
        // Costly electricity on a modest earner → optimum drops below stock and
        // earns more than running flat-out.
        let rev_per_s = 5.0e-5; // ~$4.3/day gross at stock
        let op = optimize(rev_per_s, 300.0, 0.35, 1.0e9, 0.5);
        assert!(op.frac < 1.0, "should undervolt: frac={}", op.frac);
        assert!(op.net_day >= op.net_day_stock);
        assert!(op.watts_saved(300.0) > 0.0);
    }

    #[test]
    fn undervolt_can_rescue_a_marginal_coin() {
        // A coin that loses money at stock but is positive when undervolted.
        let rev_per_s = 1.2e-5;
        let op = optimize(rev_per_s, 320.0, 0.20, 1.0e9, 0.5);
        // optimized profit strictly beats stock (cutting power faster than revenue)
        assert!(op.net_day > op.net_day_stock);
    }
}
