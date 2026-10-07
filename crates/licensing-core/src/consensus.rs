//! Cross-node consensus for payment verification.
//!
//! One RPC node is a single point of failure — and a single point of trust. The issuer therefore
//! asks several independent nodes the same question and only acts when [`CONSENSUS_NODES`] of them
//! report the same on-chain fact. Every attempt is recorded in node order, so a client that has to
//! retry receives an ordered, replayable signal instead of an open-ended wait.

use std::fmt;
use std::time::Duration;

use crate::budget::{TimeBudget, RETRY_AFTER_MS};

/// Nodes that must report the same on-chain fact before the issuer acts on it.
///
/// Applied as `min(CONSENSUS_NODES, configured nodes)`, so a deliberately single-node deployment
/// still works while the default three-node list needs two agreeing nodes.
pub const CONSENSUS_NODES: usize = 2;

/// Confirm-time difference that still counts as the same settlement across nodes.
pub const CONFIRMED_AT_TOLERANCE_SECONDS: i64 = 2;

/// Permanent reason a node gives for refusing a submitted transfer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RejectionCode {
    /// The transfer settled but moved less than the quoted minimum.
    BelowMinimum,
    /// The transaction exists but pays a different recipient.
    WrongRecipient,
    /// The transaction failed on chain.
    FailedOnChain,
    /// The transaction contains no transfer to the configured treasury.
    NoTransfer,
}

impl RejectionCode {
    /// Stable machine-readable code used in issuer responses.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::BelowMinimum => "below-minimum",
            Self::WrongRecipient => "wrong-recipient",
            Self::FailedOnChain => "failed-on-chain",
            Self::NoTransfer => "no-transfer",
        }
    }

    /// Human-readable explanation for logs and API responses.
    pub fn describe(self) -> &'static str {
        match self {
            Self::BelowMinimum => "confirmed transfer is below the quoted minimum amount",
            Self::WrongRecipient => "transfer recipient does not match the configured treasury",
            Self::FailedOnChain => "transaction failed on chain",
            Self::NoTransfer => "transaction contains no transfer to the configured treasury",
        }
    }
}

/// The on-chain fact a node reported about a submitted payment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NodeObservation {
    /// The transfer settled into the treasury.
    Settled {
        received_units: u128,
        confirmed_at: i64,
    },
    /// The transaction is on chain but cannot pay this quote.
    Invalid { code: RejectionCode },
}

impl NodeObservation {
    /// Whether two nodes reported the same fact.
    pub fn agrees_with(&self, other: &Self) -> bool {
        match (self, other) {
            (
                Self::Settled {
                    received_units: left,
                    confirmed_at: left_at,
                },
                Self::Settled {
                    received_units: right,
                    confirmed_at: right_at,
                },
            ) => left == right && (left_at - right_at).abs() <= CONFIRMED_AT_TOLERANCE_SECONDS,
            (Self::Invalid { code: left }, Self::Invalid { code: right }) => left == right,
            _ => false,
        }
    }
}

/// Outcome of one node attempt. Only [`Self::Settled`] and [`Self::Invalid`] are decision facts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttemptState {
    /// The node confirmed a settlement.
    Settled,
    /// The node refused the transaction for a permanent reason.
    Invalid,
    /// The node does not know the transaction yet or it needs more confirmations.
    Pending,
    /// The node could not be reached.
    Unreachable,
    /// The node consumed its slice of the verification budget without answering.
    TimedOut,
    /// The budget was spent before this node was contacted;
    Skipped,
}

impl AttemptState {
    /// Whether the state carries a decision fact that may form a consensus.
    pub fn is_fact(self) -> bool {
        matches!(self, Self::Settled | Self::Invalid)
    }

    /// Stable machine-readable label used in responses and logs.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Settled => "settled",
            Self::Invalid => "invalid",
            Self::Pending => "pending",
            Self::Unreachable => "unreachable",
            Self::TimedOut => "timed-out",
            Self::Skipped => "skipped",
        }
    }
}

impl fmt::Display for AttemptState {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Why the issuer could not reach a decision inside the budget.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetryReason {
    /// Nodes answered, but fewer than [`CONSENSUS_NODES`] agreed.
    NoConsensus,
    /// No node produced a decision fact at all.
    NodesUnavailable,
    /// The hard watchdog fired before the chain returned.
    BudgetExpired,
}

impl RetryReason {
    /// Stable machine-readable label used in responses and logs.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NoConsensus => "no-consensus",
            Self::NodesUnavailable => "nodes-unavailable",
            Self::BudgetExpired => "budget-expired",
        }
    }
}

impl fmt::Display for RetryReason {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Opaque, ordered node label. Clients never learn which provider a step refers to.
pub fn node_label(order: usize) -> String {
    format!("node-{}", order + 1)
}

/// One node attempt in the order it was made.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeAttempt {
    /// Zero-based position in the configured node order.
    pub order: usize,
    /// Opaque label, e.g. `node-2`.
    pub label: String,
    /// What the attempt produced.
    pub state: AttemptState,
    /// Wall-clock time the attempt consumed.
    pub elapsed_ms: u64,
    /// Operator-facing detail. Server logs only — never sent to a client.
    pub detail: Option<String>,
}

impl NodeAttempt {
    pub fn new(
        order: usize,
        state: AttemptState,
        elapsed: Duration,
        detail: Option<String>,
    ) -> Self {
        Self {
            order,
            label: node_label(order),
            state,
            elapsed_ms: milliseconds(elapsed),
            detail,
        }
    }

    /// One deterministic log line per attempt.
    pub fn log_line(&self) -> String {
        match self.detail.as_deref() {
            Some(detail) => format!(
                "{} {} ({} ms): {detail}",
                self.label, self.state, self.elapsed_ms
            ),
            None => format!("{} {} ({} ms)", self.label, self.state, self.elapsed_ms),
        }
    }
}

/// One ordered step of the signal a client retries with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetryStep {
    /// Zero-based position in the configured node order.
    pub order: usize,
    /// Opaque label, e.g. `node-1`.
    pub label: String,
    /// Attempt state, e.g. `timed-out`.
    pub state: String,
    /// Wall-clock time the step consumed.
    pub elapsed_ms: u64,
}

/// Ordered retry-later signal returned instead of blocking on a slow chain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetrySignal {
    /// Why no decision was reached.
    pub reason: RetryReason,
    /// Delay the client should observe before retrying the same request.
    pub retry_after_ms: u64,
    /// Budget that applied to this verification.
    pub budget_ms: u64,
    /// Budget time already consumed.
    pub elapsed_ms: u64,
    /// Node order, so a retry walks the same sequence.
    pub steps: Vec<RetryStep>,
    /// Operator-facing detail lines. Server logs only — never sent to a client.
    pub details: Vec<String>,
}

impl RetrySignal {
    /// Delay for an HTTP `Retry-After` response header.
    pub fn retry_after_seconds(&self) -> u64 {
        self.retry_after_ms.div_ceil(1_000)
    }

    /// Ordered signal built from the attempts of a completed chain.
    fn from_attempts(
        reason: RetryReason,
        budget: Duration,
        elapsed: Duration,
        attempts: &[NodeAttempt],
    ) -> Self {
        let steps = attempts
            .iter()
            .map(|attempt| RetryStep {
                order: attempt.order,
                label: attempt.label.clone(),
                state: attempt.state.as_str().to_owned(),
                elapsed_ms: attempt.elapsed_ms,
            })
            .collect();
        let details = attempts
            .iter()
            .map(NodeAttempt::log_line)
            .collect::<Vec<_>>();
        Self {
            reason,
            retry_after_ms: RETRY_AFTER_MS,
            budget_ms: milliseconds(budget),
            elapsed_ms: milliseconds(elapsed),
            steps,
            details,
        }
    }

    /// Signal for the hard watchdog path, where no attempt list survives.
    ///
    /// The node order is still reported so the client retries the same sequence.
    pub fn watchdog(budget: &TimeBudget, nodes: usize) -> Self {
        let steps = (0..nodes)
            .map(|order| RetryStep {
                order,
                label: node_label(order),
                state: "unfinished".to_owned(),
                elapsed_ms: 0,
            })
            .collect();
        Self {
            reason: RetryReason::BudgetExpired,
            retry_after_ms: RETRY_AFTER_MS,
            budget_ms: budget.total_ms(),
            elapsed_ms: budget.elapsed_ms(),
            steps,
            details: vec![format!(
                "the {} ms verification watchdog expired before any node reported a decision",
                budget.total_ms()
            )],
        }
    }
}

/// A fact that enough nodes agreed on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Consensus {
    /// The agreed fact.
    pub observation: NodeObservation,
    /// How many nodes reported it.
    pub agreements: usize,
    /// Positions of the agreeing nodes, in attempt order.
    pub nodes: Vec<usize>,
    /// Every attempt that was made, in node order.
    pub attempts: Vec<NodeAttempt>,
}

/// Accumulates node attempts until [`CONSENSUS_NODES`] of them agree.
#[derive(Debug, Clone)]
pub struct ConsensusTracker {
    required: usize,
    observations: Vec<(usize, NodeObservation)>,
    attempts: Vec<NodeAttempt>,
}

impl ConsensusTracker {
    /// A tracker that needs `required` agreeing nodes. `required` is clamped to at least one.
    pub fn new(required: usize) -> Self {
        Self {
            required: required.max(1),
            observations: Vec::new(),
            attempts: Vec::new(),
        }
    }

    /// How many agreeing nodes this tracker waits for.
    pub fn required(&self) -> usize {
        self.required
    }

    /// Records a decision fact reported by the node at `order`.
    pub fn record_observation(
        &mut self,
        order: usize,
        observation: NodeObservation,
        elapsed: Duration,
    ) {
        let state = match observation {
            NodeObservation::Settled { .. } => AttemptState::Settled,
            NodeObservation::Invalid { .. } => AttemptState::Invalid,
        };
        self.attempts
            .push(NodeAttempt::new(order, state, elapsed, None));
        self.observations.push((order, observation));
    }

    /// Records an attempt that produced no decision fact.
    pub fn record_state(
        &mut self,
        order: usize,
        state: AttemptState,
        elapsed: Duration,
        detail: Option<String>,
    ) {
        // A fact recorded here would let a caller claim consensus without an observation.
        let state = if state.is_fact() {
            AttemptState::Unreachable
        } else {
            state
        };
        self.attempts
            .push(NodeAttempt::new(order, state, elapsed, detail));
    }

    /// Every attempt so far, in node order.
    pub fn attempts(&self) -> &[NodeAttempt] {
        &self.attempts
    }

    /// The first fact that [`Self::required`] nodes agreed on.
    pub fn consensus(&self) -> Option<Consensus> {
        for (index, (order, observation)) in self.observations.iter().enumerate() {
            let mut nodes = vec![*order];
            for (other_order, other) in self.observations.iter().skip(index + 1) {
                if observation.agrees_with(other) {
                    nodes.push(*other_order);
                }
            }
            if nodes.len() >= self.required {
                return Some(Consensus {
                    observation: observation.clone(),
                    agreements: nodes.len(),
                    nodes,
                    attempts: self.attempts.clone(),
                });
            }
        }
        None
    }

    /// Ordered retry-later signal describing everything that was attempted.
    pub fn retry_signal(
        &self,
        reason: RetryReason,
        budget: Duration,
        elapsed: Duration,
    ) -> RetrySignal {
        RetrySignal::from_attempts(reason, budget, elapsed, &self.attempts)
    }
}

fn milliseconds(value: Duration) -> u64 {
    value.as_millis().min(u128::from(u64::MAX)) as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settled(received_units: u128, confirmed_at: i64) -> NodeObservation {
        NodeObservation::Settled {
            received_units,
            confirmed_at,
        }
    }

    #[test]
    fn two_agreeing_nodes_form_a_consensus_and_one_node_does_not() {
        let mut tracker = ConsensusTracker::new(CONSENSUS_NODES);
        tracker.record_observation(0, settled(1_000, 1_700_000_000), Duration::from_millis(90));
        assert!(tracker.consensus().is_none());
        tracker.record_observation(1, settled(1_000, 1_700_000_000), Duration::from_millis(120));
        let consensus = tracker.consensus().expect("two matching nodes agree");
        assert_eq!(consensus.agreements, 2);
        assert_eq!(consensus.nodes, vec![0, 1]);
        assert_eq!(consensus.attempts.len(), 2);
        assert_eq!(consensus.attempts[0].label, "node-1");
        assert_eq!(consensus.attempts[1].state, AttemptState::Settled);
    }

    #[test]
    fn a_single_node_deployment_still_reaches_consensus() {
        let mut tracker = ConsensusTracker::new(1);
        tracker.record_observation(0, settled(7, 1_700_000_000), Duration::from_millis(40));
        assert_eq!(tracker.required(), 1);
        assert_eq!(
            tracker.consensus().expect("one node is enough").agreements,
            1
        );
        assert!(ConsensusTracker::new(0).required() >= 1);
    }

    #[test]
    fn disagreement_and_confirm_time_drift_are_bounded() {
        let reference = settled(1_000, 1_700_000_000);
        assert!(reference.agrees_with(&settled(1_000, 1_700_000_000)));
        assert!(reference.agrees_with(&settled(1_000, 1_700_000_002)));
        assert!(reference.agrees_with(&settled(1_000, 1_699_999_998)));
        assert!(!reference.agrees_with(&settled(1_000, 1_699_999_997)));
        assert!(!reference.agrees_with(&settled(999, 1_700_000_000)));
        assert!(!reference.agrees_with(&NodeObservation::Invalid {
            code: RejectionCode::NoTransfer
        }));
        assert!(NodeObservation::Invalid {
            code: RejectionCode::BelowMinimum
        }
        .agrees_with(&NodeObservation::Invalid {
            code: RejectionCode::BelowMinimum
        }));
    }

    #[test]
    fn a_disagreeing_node_neither_completes_nor_breaks_a_consensus() {
        let mut tracker = ConsensusTracker::new(CONSENSUS_NODES);
        tracker.record_observation(0, settled(1_000, 1_700_000_000), Duration::from_millis(50));
        tracker.record_observation(1, settled(900, 1_700_000_000), Duration::from_millis(60));
        assert!(tracker.consensus().is_none());
        tracker.record_observation(2, settled(1_000, 1_700_000_000), Duration::from_millis(70));
        let consensus = tracker.consensus().expect("the first agreeing pair wins");
        assert_eq!(consensus.nodes, vec![0, 2]);
        assert_eq!(consensus.observation, settled(1_000, 1_700_000_000));
    }

    #[test]
    fn recorded_states_cannot_fake_a_decision_fact() {
        let mut tracker = ConsensusTracker::new(CONSENSUS_NODES);
        tracker.record_state(
            0,
            AttemptState::Settled,
            Duration::from_millis(10),
            Some("not a real observation".to_owned()),
        );
        tracker.record_state(1, AttemptState::Settled, Duration::from_millis(10), None);
        assert!(tracker.consensus().is_none());
        assert_eq!(tracker.attempts()[0].state, AttemptState::Unreachable);
        assert!(AttemptState::Settled.is_fact());
        assert!(AttemptState::Invalid.is_fact());
        assert!(!AttemptState::Pending.is_fact());
        assert!(!AttemptState::Unreachable.is_fact());
        assert!(!AttemptState::TimedOut.is_fact());
        assert!(!AttemptState::Skipped.is_fact());
    }

    #[test]
    fn the_retry_signal_keeps_attempt_order_including_skipped_nodes() {
        let mut tracker = ConsensusTracker::new(CONSENSUS_NODES);
        tracker.record_state(
            0,
            AttemptState::TimedOut,
            Duration::from_millis(500),
            Some("request timed out".to_owned()),
        );
        tracker.record_state(
            1,
            AttemptState::Pending,
            Duration::from_millis(120),
            Some("needs six confirmations".to_owned()),
        );
        tracker.record_state(2, AttemptState::Skipped, Duration::ZERO, None);
        let signal = tracker.retry_signal(
            RetryReason::NoConsensus,
            Duration::from_millis(1_500),
            Duration::from_millis(620),
        );
        assert_eq!(signal.reason, RetryReason::NoConsensus);
        assert_eq!(signal.budget_ms, 1_500);
        assert_eq!(signal.elapsed_ms, 620);
        assert_eq!(signal.retry_after_ms, RETRY_AFTER_MS);
        assert_eq!(signal.retry_after_seconds(), 1);
        assert_eq!(
            signal
                .steps
                .iter()
                .map(|step| format!("{}:{}", step.label, step.state))
                .collect::<Vec<_>>(),
            vec!["node-1:timed-out", "node-2:pending", "node-3:skipped"]
        );
        assert_eq!(signal.details.len(), 3);
        assert!(signal.details[0].contains("node-1 timed-out"));
        assert!(signal.details[1].contains("needs six confirmations"));
    }

    #[test]
    fn reason_and_state_labels_are_stable() {
        assert_eq!(RetryReason::NoConsensus.as_str(), "no-consensus");
        assert_eq!(RetryReason::NodesUnavailable.as_str(), "nodes-unavailable");
        assert_eq!(RetryReason::BudgetExpired.as_str(), "budget-expired");
        assert_eq!(RejectionCode::BelowMinimum.as_str(), "below-minimum");
        assert_eq!(RejectionCode::WrongRecipient.as_str(), "wrong-recipient");
        assert_eq!(RejectionCode::FailedOnChain.as_str(), "failed-on-chain");
        assert_eq!(RejectionCode::NoTransfer.as_str(), "no-transfer");
        assert_eq!(node_label(0), "node-1");
        assert!(RejectionCode::NoTransfer.describe().contains("treasury"));
    }
}
