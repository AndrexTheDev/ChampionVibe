//! Monotonic time budget shared by every RPC call of one verification chain.
//!
//! An issuer must answer inside a fixed wall-clock window: a node that never replies is not allowed
//! to keep a client — or an HTTP worker — waiting. A [`TimeBudget`] is created once per request,
//! handed to every node attempt and used to derive the timeout of each individual network call, so
//! the attempts of the whole chain can never add up to more than the configured window.

use std::time::{Duration, Instant};

/// Milliseconds the whole payment/oracle verification chain may consume across all RPC nodes.
///
/// This replaces the per-attempt-only limit: individual requests are now capped by the *remaining*
/// part of this window instead of a fixed timeout of their own.
pub const VERIFICATION_BUDGET_MS: u64 = 1_500;

/// Smallest slice handed to a single node. A node that cannot answer inside this slice is reported
/// as unavailable instead of being given a deadline that cannot be met anyway.
pub const MIN_NODE_SLICE_MS: u64 = 150;

/// Hard cap for one node attempt, so a single slow node cannot consume the entire budget.
pub const MAX_NODE_ATTEMPT_MS: u64 = 700;

/// Delay asked of a client whose request reached no consensus inside the budget.
pub const RETRY_AFTER_MS: u64 = 1_000;

/// Milliseconds the quote/pricing chain may consume.
///
/// Pricing reads four Chainlink rounds over one Ethereum RPC, so it gets a wider window than a
/// payment, which is bounded by [`VERIFICATION_BUDGET_MS`].
pub const QUOTE_BUDGET_MS: u64 = 4_000;

/// A monotonic deadline shared by every network call of one verification.
#[derive(Debug, Clone, Copy)]
pub struct TimeBudget {
    started: Instant,
    total: Duration,
}

impl TimeBudget {
    pub fn new(total: Duration) -> Self {
        Self {
            started: Instant::now(),
            total,
        }
    }

    /// The production budget for payment and price-oracle verification.
    pub fn verification() -> Self {
        Self::from_millis(VERIFICATION_BUDGET_MS)
    }

    pub fn from_millis(milliseconds: u64) -> Self {
        Self::new(Duration::from_millis(milliseconds))
    }

    /// The production budget for quote pricing over the Chainlink feeds.
    pub fn quote() -> Self {
        Self::from_millis(QUOTE_BUDGET_MS)
    }

    pub fn total(&self) -> Duration {
        self.total
    }

    /// The configured window in milliseconds.
    pub fn total_ms(&self) -> u64 {
        duration_ms(self.total)
    }

    /// Time already spent, never reported as more than the configured window.
    pub fn elapsed(&self) -> Duration {
        self.started.elapsed().min(self.total)
    }

    pub fn elapsed_ms(&self) -> u64 {
        duration_ms(self.elapsed())
    }

    /// Time left before the chain has to give up.
    pub fn remaining(&self) -> Duration {
        self.total.saturating_sub(self.started.elapsed())
    }

    pub fn remaining_ms(&self) -> u64 {
        duration_ms(self.remaining())
    }

    pub fn expired(&self) -> bool {
        self.remaining().is_zero()
    }

    /// Longest a single network attempt may take right now, or `None` when the budget is spent.
    pub fn attempt_timeout(&self) -> Option<Duration> {
        let remaining = self.remaining();
        if remaining.is_zero() {
            return None;
        }
        Some(remaining.min(Duration::from_millis(MAX_NODE_ATTEMPT_MS)))
    }

    /// Slice for one of `nodes_left` remaining nodes, or `None` when what is left cannot complete a
    /// request and the attempt should be skipped instead.
    pub fn node_slice(&self, nodes_left: usize) -> Option<Duration> {
        if nodes_left == 0 {
            return None;
        }
        let share = self.remaining_ms() / nodes_left as u64;
        let slice = share.min(MAX_NODE_ATTEMPT_MS);
        (slice >= MIN_NODE_SLICE_MS).then(|| Duration::from_millis(slice))
    }
}

fn duration_ms(value: Duration) -> u64 {
    value.as_millis().min(u128::from(u64::MAX)) as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fresh_budget_reports_the_configured_window() {
        let budget = TimeBudget::verification();
        assert_eq!(
            budget.total(),
            Duration::from_millis(VERIFICATION_BUDGET_MS)
        );
        assert!(!budget.expired());
        assert!(budget.remaining_ms() <= VERIFICATION_BUDGET_MS);
        assert!(budget.remaining_ms() > VERIFICATION_BUDGET_MS - 500);
        assert!(budget.elapsed_ms() < 500);
    }

    #[test]
    fn the_quote_budget_is_wider_than_payment_verification() {
        let quote = TimeBudget::quote();
        assert_eq!(quote.total(), Duration::from_millis(QUOTE_BUDGET_MS));
        assert!(quote.total() > TimeBudget::verification().total());
        assert!(!quote.expired());
    }

    #[test]
    fn an_exhausted_budget_reports_expiry_and_no_attempt_timeout() {
        let budget = TimeBudget::from_millis(0);
        assert!(budget.expired());
        assert_eq!(budget.remaining_ms(), 0);
        assert_eq!(budget.elapsed(), Duration::ZERO);
        assert!(budget.attempt_timeout().is_none());
        assert!(budget.node_slice(3).is_none());
    }

    #[test]
    fn node_slices_share_the_remaining_budget_and_respect_the_caps() {
        let budget = TimeBudget::from_millis(VERIFICATION_BUDGET_MS);
        let slice = budget.node_slice(3).expect("three nodes share the budget");
        assert!(slice >= Duration::from_millis(400));
        assert!(slice <= Duration::from_millis(MAX_NODE_ATTEMPT_MS));
        assert_eq!(budget.node_slice(0), None);
        // 300 ms cannot give three nodes a slice that could complete a request.
        assert_eq!(TimeBudget::from_millis(300).node_slice(3), None);
        // A single attempt never exceeds the hard cap, however much budget is left.
        assert_eq!(
            TimeBudget::from_millis(VERIFICATION_BUDGET_MS).attempt_timeout(),
            Some(Duration::from_millis(MAX_NODE_ATTEMPT_MS))
        );
    }
}
