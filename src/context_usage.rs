//! Context-window fill indicator model (desktop-assistant#341).
//!
//! Mirrors the daemon's `SignalEvent::ContextUsage` (token COUNTS only, never
//! content) and turns it into a glanceable, read-only `used / budget (pct%)`
//! readout plus a colour bucket keyed to the proactive-compaction line. The
//! daemon owns the numbers; this never recomputes a budget — it only formats.

/// The proactive-compaction ratio the daemon uses (`COMPACTION_TOKEN_RATIO`).
/// Kept in sync deliberately: this is the colour-threshold contract.
const COMPACTION_RATIO: f64 = 0.85;

/// Colour bucket for the fill indicator. Green below 0.85 of budget, amber
/// from 0.85 up to budget, red at/over budget (overflow).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContextFillLevel {
    Green,
    Amber,
    Red,
}

impl ContextFillLevel {
    /// CSS class applied to the indicator label for this level. Styled in
    /// `style.css` / `style-light.css`.
    pub fn css_class(self) -> &'static str {
        match self {
            ContextFillLevel::Green => "context-fill-green",
            ContextFillLevel::Amber => "context-fill-amber",
            ContextFillLevel::Red => "context-fill-red",
        }
    }

    /// All classes, so the executor can clear the others before adding one.
    pub fn all_classes() -> [&'static str; 3] {
        [
            "context-fill-green",
            "context-fill-amber",
            "context-fill-red",
        ]
    }
}

/// Per-conversation context-window fill. Token COUNTS only.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContextUsageView {
    pub used_tokens: u64,
    pub budget_tokens: u64,
    pub compaction_active: bool,
}

impl ContextUsageView {
    /// Fraction of budget consumed (0.0..). May exceed 1.0 on overflow.
    /// Returns 0.0 for a zero/unknown budget — no divide-by-zero, renders
    /// neutrally rather than implying false precision.
    pub fn fraction(&self) -> f64 {
        if self.budget_tokens == 0 {
            0.0
        } else {
            self.used_tokens as f64 / self.budget_tokens as f64
        }
    }

    pub fn level(&self) -> ContextFillLevel {
        let f = self.fraction();
        if f >= 1.0 {
            ContextFillLevel::Red
        } else if f >= COMPACTION_RATIO {
            // Inclusive at exactly 0.85: AT the line is the moment to warn.
            ContextFillLevel::Amber
        } else {
            ContextFillLevel::Green
        }
    }

    /// Compact readout, e.g. `12k / 32k (38%)`; a trailing `⟳` marks an
    /// active windowing/compaction pass.
    pub fn readout(&self) -> String {
        let pct = (self.fraction() * 100.0).round() as u64;
        let mut s = format!(
            "{} / {} ({pct}%)",
            abbrev_tokens(self.used_tokens),
            abbrev_tokens(self.budget_tokens),
        );
        if self.compaction_active {
            s.push_str(" ⟳");
        }
        s
    }
}

/// Abbreviate a token count for the narrow status bar: `12000 -> "12k"`,
/// `512 -> "512"`.
fn abbrev_tokens(n: u64) -> String {
    if n >= 1_000 {
        format!("{}k", n / 1_000)
    } else {
        n.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn usage(used: u64, budget: u64, compaction: bool) -> ContextUsageView {
        ContextUsageView {
            used_tokens: used,
            budget_tokens: budget,
            compaction_active: compaction,
        }
    }

    #[test]
    fn readout_formats_used_over_budget_with_percent() {
        assert_eq!(usage(12_000, 32_000, false).readout(), "12k / 32k (38%)");
        assert_eq!(usage(500, 8_000, false).readout(), "500 / 8k (6%)");
    }

    #[test]
    fn readout_marks_active_compaction() {
        assert!(usage(30_000, 32_000, true).readout().ends_with(" ⟳"));
        assert!(!usage(30_000, 32_000, false).readout().contains('⟳'));
    }

    #[test]
    fn zero_used_at_turn_start_is_green_zero_percent() {
        let u = usage(0, 32_000, false);
        assert_eq!(u.level(), ContextFillLevel::Green);
        assert_eq!(u.readout(), "0 / 32k (0%)");
    }

    #[test]
    fn below_threshold_is_green() {
        assert_eq!(
            usage(26_880, 32_000, false).level(),
            ContextFillLevel::Green
        );
    }

    #[test]
    fn exactly_at_0_85_is_amber_inclusive() {
        // 0.85 * 32_000 == 27_200.
        assert_eq!(
            usage(27_200, 32_000, false).level(),
            ContextFillLevel::Amber
        );
    }

    #[test]
    fn between_threshold_and_budget_is_amber() {
        assert_eq!(
            usage(30_000, 32_000, false).level(),
            ContextFillLevel::Amber
        );
    }

    #[test]
    fn at_budget_is_red() {
        assert_eq!(usage(32_000, 32_000, false).level(), ContextFillLevel::Red);
    }

    #[test]
    fn over_budget_overflow_is_red() {
        let u = usage(40_000, 32_000, false);
        assert_eq!(u.level(), ContextFillLevel::Red);
        assert_eq!(u.readout(), "40k / 32k (125%)");
    }

    #[test]
    fn zero_budget_renders_neutrally_without_panic() {
        let u = usage(5_000, 0, false);
        assert_eq!(u.fraction(), 0.0);
        assert_eq!(u.level(), ContextFillLevel::Green);
        assert_eq!(u.readout(), "5k / 0 (0%)");
    }
}
