//! Integration coverage for the issuer's global verification budget and retry-later contract.
//!
//! The issuer replaces a per-attempt network timeout with one window shared by every node of a
//! payment chain: three nodes must share it, a slow node may not extend it, and a chain that runs
//! out of time returns an ordered signal instead of waiting.

use championvibe_licensing_core::{
    ApiEndpoints, AttemptState, Chain, ConsensusTracker, NodeObservation, PaymentVerifier,
    RetryReason, RetrySignal, TimeBudget, Treasury, VerificationError, CONSENSUS_NODES,
    MAX_NODE_ATTEMPT_MS, MIN_NODE_SLICE_MS, RETRY_AFTER_MS, VERIFICATION_BUDGET_MS,
};
use std::time::{Duration, Instant};

fn settled(received_units: u128, confirmed_at: i64) -> NodeObservation {
    NodeObservation::Settled {
        received_units,
        confirmed_at,
    }
}

#[test]
fn a_malformed_hash_is_rejected_before_any_network_access() {
    match PaymentVerifier::validate_submission(Chain::Solana, "not-a-signature") {
        Err(VerificationError::Rejected { code, attempts, .. }) => {
            assert_eq!(code, "invalid-transaction-hash");
            assert!(
                attempts.is_empty(),
                "no node may be contacted for a bad hash"
            );
        }
        other => panic!("a malformed hash must be a terminal rejection, got {other:?}"),
    }
    let well_formed = format!("0x{}", "ab".repeat(32));
    assert_eq!(
        PaymentVerifier::validate_submission(Chain::Ethereum, &well_formed).unwrap(),
        well_formed
    );
    assert!(PaymentVerifier::validate_submission(Chain::Bitcoin, &"a".repeat(64)).is_ok());
    assert!(PaymentVerifier::validate_submission(Chain::Bitcoin, &"a".repeat(63)).is_err());
    assert!(PaymentVerifier::validate_submission(Chain::Solana, "").is_err());
}

#[test]
fn the_window_is_a_hard_maximum_of_one_and_a_half_seconds() {
    assert_eq!(VERIFICATION_BUDGET_MS, 1_500);
    let budget = TimeBudget::verification();
    assert_eq!(
        budget.total(),
        Duration::from_millis(VERIFICATION_BUDGET_MS)
    );
    assert_eq!(budget.total_ms(), VERIFICATION_BUDGET_MS);

    let mut planned = Duration::ZERO;
    for nodes_left in (1..=3_usize).rev() {
        let slice = budget
            .node_slice(nodes_left)
            .expect("a fresh window can afford three nodes");
        assert!(slice <= Duration::from_millis(MAX_NODE_ATTEMPT_MS));
        assert!(slice <= budget.remaining());
        planned += slice;
    }
    assert!(
        planned <= budget.total() + Duration::from_millis(MAX_NODE_ATTEMPT_MS),
        "three planned slices must stay within the window, planned {planned:?}"
    );
    assert_eq!(budget.node_slice(0), None, "no attempt without a node");
    assert!(budget
        .attempt_timeout()
        .is_some_and(|timeout| timeout <= Duration::from_millis(MAX_NODE_ATTEMPT_MS)));
    // A window that cannot give every node a usable slice is refused instead of being spent.
    assert_eq!(
        TimeBudget::from_millis(MIN_NODE_SLICE_MS * 2).node_slice(3),
        None
    );
}

#[test]
fn a_three_node_chain_that_honours_its_slices_answers_inside_the_window() {
    let budget = TimeBudget::from_millis(900);
    let mut tracker = ConsensusTracker::new(CONSENSUS_NODES);
    let started = Instant::now();
    for order in 0..3 {
        let slice = budget
            .node_slice(3 - order)
            .expect("three nodes fit inside the window");
        assert!(slice >= Duration::from_millis(MIN_NODE_SLICE_MS));
        std::thread::sleep(slice / 2);
        tracker.record_state(
            order,
            AttemptState::Pending,
            slice / 2,
            Some("transaction is not confirmed yet".to_owned()),
        );
    }
    assert!(
        started.elapsed() < budget.total(),
        "a three-node chain must answer inside its window, took {:?}",
        started.elapsed()
    );
    assert!(!budget.expired(), "the window must not be spent on time");

    let signal = tracker.retry_signal(
        RetryReason::NodesUnavailable,
        budget.total(),
        budget.elapsed(),
    );
    assert_eq!(signal.reason.as_str(), "nodes-unavailable");
    assert_eq!(
        signal.steps.len(),
        3,
        "every configured node is reported once"
    );
    assert!(signal.steps.iter().all(|step| step.state == "pending"));
    assert_eq!(signal.details.len(), 3, "server logs keep every attempt");
    assert_eq!(signal.retry_after_seconds(), 1);
}

#[test]
fn nodes_that_overrun_their_slice_cannot_extend_the_window() {
    let budget = TimeBudget::from_millis(600);
    let mut tracker = ConsensusTracker::new(CONSENSUS_NODES);
    let started = Instant::now();
    // Emulate nodes that keep sitting on a socket for longer than their slice allows.
    for order in 0..3 {
        match budget.node_slice(3 - order) {
            Some(slice) => {
                std::thread::sleep(slice + Duration::from_millis(120));
                tracker.record_state(
                    order,
                    AttemptState::TimedOut,
                    slice,
                    Some("node overran its slice".to_owned()),
                );
            }
            None => tracker.record_state(order, AttemptState::Skipped, Duration::ZERO, None),
        }
    }
    let elapsed = started.elapsed();
    assert!(
        elapsed < Duration::from_millis(1_300),
        "an overrunning chain must be cut short, took {elapsed:?}"
    );
    assert!(budget.expired(), "the overrun must consume the window");
    assert!(
        tracker.consensus().is_none(),
        "a timeout is never a consensus"
    );

    let signal = tracker.retry_signal(RetryReason::NoConsensus, budget.total(), budget.elapsed());
    assert_eq!(signal.retry_after_ms, RETRY_AFTER_MS);
    assert_eq!(signal.retry_after_seconds(), 1);
    assert_eq!(signal.steps.len(), 3);
    assert_eq!(
        signal
            .steps
            .iter()
            .map(|step| step.label.as_str())
            .collect::<Vec<_>>(),
        vec!["node-1", "node-2", "node-3"],
        "the client retries the same ordered sequence"
    );
    assert_eq!(signal.steps[0].state, "timed-out");
    assert_eq!(
        signal.steps[1].state, "skipped",
        "a node whose share of the window is too small is dropped, not granted more time"
    );
    assert!(signal.steps.iter().all(|step| step.order < 3));
}

#[test]
fn consensus_needs_agreement_and_a_single_node_deployment_still_works() {
    let mut tracker = ConsensusTracker::new(CONSENSUS_NODES);
    tracker.record_observation(0, settled(1_000, 1_700_000_000), Duration::from_millis(80));
    assert!(tracker.consensus().is_none(), "one node is not a consensus");
    tracker.record_observation(1, settled(1_000, 1_700_000_001), Duration::from_millis(90));
    let consensus = tracker.consensus().expect("two agreeing nodes are enough");
    assert_eq!(consensus.agreements, 2);

    let mut single = ConsensusTracker::new(CONSENSUS_NODES.min(1));
    single.record_observation(2, settled(1_000, 1_700_000_000), Duration::from_millis(70));
    assert_eq!(
        single.consensus().expect("one node is enough").nodes,
        vec![2]
    );
}

#[test]
fn the_watchdog_signal_keeps_the_node_order_for_a_client_retry() {
    let budget = TimeBudget::verification();
    let signal = RetrySignal::watchdog(&budget, 3);
    assert_eq!(signal.reason.as_str(), "budget-expired");
    assert_eq!(signal.budget_ms, VERIFICATION_BUDGET_MS);
    assert_eq!(signal.retry_after_seconds(), 1);
    assert_eq!(signal.steps.len(), 3);
    assert!(signal.steps.iter().all(|step| step.state == "unfinished"));
    assert_eq!(signal.steps[2].label, "node-3");
}

#[test]
fn the_default_deployment_consults_independent_nodes_primary_first() {
    let verifier = PaymentVerifier::new(ApiEndpoints::default(), Treasury::default())
        .expect("shipped endpoints and treasury are valid");
    let endpoints = ApiEndpoints::default();
    let chains = [
        (Chain::Solana, endpoints.solana_rpc.clone()),
        (Chain::Bitcoin, endpoints.bitcoin_api.clone()),
        (Chain::Ethereum, endpoints.ethereum_rpc.clone()),
    ];
    for (chain, primary) in chains {
        // The shipped configuration lists three independent providers per chain; an operator may
        // configure a shorter list, but consensus still needs two agreeing nodes.
        assert!(
            verifier.node_count(chain) >= CONSENSUS_NODES,
            "{chain:?} needs at least two nodes to reach consensus"
        );
        let nodes = endpoints.nodes(chain);
        let mut unique = nodes.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(
            unique.len(),
            nodes.len(),
            "node lists must not repeat a node"
        );
        assert_eq!(
            nodes[0], primary,
            "{chain:?} must try its primary node first"
        );
    }
}
