use anyhow::{bail, Context, Result};
use chrono::Utc;
use reqwest::Client;
use serde_json::{json, Value};
use std::{
    env, fmt,
    time::{Duration, Instant},
};

use crate::{
    budget::{TimeBudget, MAX_NODE_ATTEMPT_MS},
    consensus::{
        AttemptState, Consensus, ConsensusTracker, NodeAttempt, NodeObservation, RejectionCode,
        RetryReason, RetrySignal, CONSENSUS_NODES,
    },
    model::{Chain, Quote, VerifiedPayment},
};

const SOLANA_RPC_DEFAULT: &str = "https://api.mainnet-beta.solana.com";
const BITCOIN_API_DEFAULT: &str = "https://blockstream.info/api";
const ETHEREUM_RPC_DEFAULT: &str = "https://cloudflare-eth.com";
const SOLANA_RECEIVER_DEFAULT: &str = "79KsqtJJdhKFJ9woxnYgtf3nq7HxQveafWBCtC3mxWi8";
const BITCOIN_RECEIVER_DEFAULT: &str = "bc1qeqzrlfg3edrydk4s0hecakc82gp26n5p7hkc7f";
const ETHEREUM_RECEIVER_DEFAULT: &str = "0xBC3fab34f69bc9f6661608C3FB36dDdC313C42F7";
const BTC_CONFIRMATIONS_REQUIRED: u64 = 6;
/// Independent providers asked for the same chain fact, so one lying or broken node cannot decide a
/// payment. The matching `CHAMPIONVIBE_*_NODES` variable replaces a whole list.
const SOLANA_RPC_MIRRORS: [&str; 2] = [
    "https://solana-rpc.publicnode.com",
    "https://solana-mainnet.public.blastapi.io",
];
const BITCOIN_API_MIRRORS: [&str; 2] = ["https://mempool.space/api", "https://mempool.emzy.de/api"];
const ETHEREUM_RPC_MIRRORS: [&str; 2] = [
    "https://ethereum-rpc.publicnode.com",
    "https://eth-mainnet.public.blastapi.io",
];
const QUOTE_SETTLEMENT_WINDOW_SECONDS: i64 = 24 * 60 * 60;

#[derive(Debug, Clone)]
pub struct ApiEndpoints {
    pub solana_rpc: String,
    pub bitcoin_api: String,
    pub ethereum_rpc: String,
    pub price_oracle_rpc: String,
}

impl Default for ApiEndpoints {
    fn default() -> Self {
        Self {
            solana_rpc: SOLANA_RPC_DEFAULT.to_owned(),
            bitcoin_api: BITCOIN_API_DEFAULT.to_owned(),
            ethereum_rpc: ETHEREUM_RPC_DEFAULT.to_owned(),
            price_oracle_rpc: ETHEREUM_RPC_DEFAULT.to_owned(),
        }
    }
}

impl ApiEndpoints {
    /// Optional overrides are useful for self-hosted nodes and integration tests.
    pub fn from_env() -> Self {
        let mut endpoints = Self::default();
        endpoints.solana_rpc = env_value("CHAMPIONVIBE_SOLANA_RPC", endpoints.solana_rpc);
        endpoints.bitcoin_api = env_value("CHAMPIONVIBE_BITCOIN_API", endpoints.bitcoin_api);
        endpoints.ethereum_rpc = env_value("CHAMPIONVIBE_ETHEREUM_RPC", endpoints.ethereum_rpc);
        endpoints.price_oracle_rpc =
            env_value("CHAMPIONVIBE_PRICE_ORACLE_RPC", endpoints.price_oracle_rpc);
        endpoints
    }

    /// Ordered nodes consulted for `chain`, primary first.
    ///
    /// `CHAMPIONVIBE_SOLANA_RPC_NODES`, `CHAMPIONVIBE_BITCOIN_API_NODES` and
    /// `CHAMPIONVIBE_ETHEREUM_RPC_NODES` replace a whole list when set (comma separated); the
    /// single-endpoint variables still choose the primary node.
    pub fn nodes(&self, chain: Chain) -> Vec<String> {
        let (primary, mirrors, override_name) = match chain {
            Chain::Solana => (
                &self.solana_rpc,
                SOLANA_RPC_MIRRORS,
                "CHAMPIONVIBE_SOLANA_RPC_NODES",
            ),
            Chain::Bitcoin => (
                &self.bitcoin_api,
                BITCOIN_API_MIRRORS,
                "CHAMPIONVIBE_BITCOIN_API_NODES",
            ),
            Chain::Ethereum => (
                &self.ethereum_rpc,
                ETHEREUM_RPC_MIRRORS,
                "CHAMPIONVIBE_ETHEREUM_RPC_NODES",
            ),
        };
        if let Some(configured) = env_list(override_name) {
            return configured;
        }
        let mut nodes = vec![primary.clone()];
        for mirror in mirrors {
            if !nodes.iter().any(|node| node == mirror) {
                nodes.push((*mirror).to_owned());
            }
        }
        nodes
    }
}

/// Comma-separated node list from the environment, ignoring blank entries.
fn env_list(name: &str) -> Option<Vec<String>> {
    let raw = env::var(name).ok()?;
    let nodes: Vec<String> = raw
        .split(',')
        .map(str::trim)
        .filter(|node| !node.is_empty())
        .map(str::to_owned)
        .collect();
    (!nodes.is_empty()).then_some(nodes)
}

#[derive(Debug, Clone)]
pub struct Treasury {
    pub solana: String,
    pub bitcoin: String,
    pub ethereum: String,
}

impl Default for Treasury {
    fn default() -> Self {
        Self {
            solana: SOLANA_RECEIVER_DEFAULT.to_owned(),
            bitcoin: BITCOIN_RECEIVER_DEFAULT.to_owned(),
            ethereum: ETHEREUM_RECEIVER_DEFAULT.to_owned(),
        }
    }
}

impl Treasury {
    pub fn from_env() -> Self {
        let defaults = Self::default();
        Self {
            solana: env_value("CHAMPIONVIBE_SOLANA_RECEIVER", defaults.solana),
            bitcoin: env_value("CHAMPIONVIBE_BITCOIN_RECEIVER", defaults.bitcoin),
            ethereum: env_value("CHAMPIONVIBE_ETHEREUM_RECEIVER", defaults.ethereum),
        }
    }

    pub fn recipient(&self, chain: Chain) -> &str {
        match chain {
            Chain::Solana => &self.solana,
            Chain::Bitcoin => &self.bitcoin,
            Chain::Ethereum => &self.ethereum,
        }
    }

    fn validate(&self) -> Result<()> {
        let solana = bs58::decode(&self.solana)
            .into_vec()
            .context("configured Solana receiver is not valid base58")?;
        if solana.len() != 32 {
            bail!("configured Solana receiver must decode to 32 bytes");
        }
        if self.bitcoin.len() != 42
            || !self.bitcoin.starts_with("bc1q")
            || !self
                .bitcoin
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
        {
            bail!("configured Bitcoin receiver is not a supported mainnet bech32 address");
        }
        let ethereum = self.ethereum.strip_prefix("0x").unwrap_or(&self.ethereum);
        if ethereum.len() != 40 || !ethereum.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            bail!("configured Ethereum receiver must be a 20-byte hex address");
        }
        Ok(())
    }
}

#[derive(Clone)]
pub struct PaymentVerifier {
    client: Client,
    endpoints: ApiEndpoints,
    treasury: Treasury,
}

/// What one node attempt produced.
enum NodeReport {
    /// The node confirmed a settlement.
    Settled {
        received_units: u128,
        confirmed_at: i64,
    },
    /// The node refuses the transaction for a permanent reason.
    Invalid { code: RejectionCode },
    /// The node does not know the transaction yet or it is not final.
    Pending { detail: String },
}

/// Why a payment could not be accepted.
#[derive(Debug)]
pub enum VerificationError {
    /// Nodes agreed that the submitted payment cannot settle this quote.
    Rejected {
        /// Stable machine-readable code, e.g. `below-minimum`.
        code: &'static str,
        /// Human-readable explanation.
        detail: String,
        /// Every node attempt that led to the agreement. Server logs only.
        attempts: Vec<NodeAttempt>,
    },
    /// No decision inside the budget: retry later, walking the same ordered steps.
    ///
    /// The client is never left waiting; the error carries the ordered node signal instead.
    RetryLater(RetrySignal),
}

impl VerificationError {
    /// Stable machine-readable code for API responses.
    pub fn code(&self) -> &'static str {
        match self {
            Self::Rejected { code, .. } => code,
            Self::RetryLater(signal) => signal.reason.as_str(),
        }
    }

    /// The ordered retry signal, when the failure is retryable.
    pub fn retry_signal(&self) -> Option<&RetrySignal> {
        match self {
            Self::Rejected { .. } => None,
            Self::RetryLater(signal) => Some(signal),
        }
    }
}

impl fmt::Display for VerificationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Rejected { code, detail, .. } => write!(formatter, "{code}: {detail}"),
            Self::RetryLater(signal) => write!(
                formatter,
                "{}: no decision after {} ms of a {} ms budget",
                signal.reason, signal.elapsed_ms, signal.budget_ms
            ),
        }
    }
}

impl std::error::Error for VerificationError {}

/// Deadline shared by every request of one node attempt, so a single node cannot stretch the chain.
#[derive(Debug, Clone, Copy)]
struct AttemptClock {
    deadline: Instant,
}

impl AttemptClock {
    fn new(slice: Duration) -> Self {
        Self {
            deadline: Instant::now() + slice,
        }
    }

    /// Time left for the next request within this attempt.
    fn request_timeout(&self) -> anyhow::Result<Duration> {
        self.deadline
            .checked_duration_since(Instant::now())
            .filter(|left| !left.is_zero())
            .map_err(|_| anyhow::Error::new(AttemptTimeout))
    }
}

/// Marker error for a node attempt that consumed its slice of the verification budget.
#[derive(Debug)]
struct AttemptTimeout;

impl fmt::Display for AttemptTimeout {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("node attempt consumed its slice of the verification budget")
    }
}

impl std::error::Error for AttemptTimeout {}

impl PaymentVerifier {
    pub fn new(endpoints: ApiEndpoints, treasury: Treasury) -> Result<Self> {
        treasury.validate()?;
        let client = Client::builder()
            // Backstop only: every attempt is additionally capped by the remaining part of the
            // verification budget, so no single request can hold the chain open.
            .timeout(Duration::from_millis(MAX_NODE_ATTEMPT_MS))
            .user_agent("ChampionVibe-License-Issuer/0.1")
            .build()
            .context("could not initialize payment RPC HTTP client")?;
        Ok(Self {
            client,
            endpoints,
            treasury,
        })
    }

    /// Number of ordered nodes consulted for `chain`.
    pub fn node_count(&self, chain: Chain) -> usize {
        self.endpoints.nodes(chain).len()
    }

    /// Checks a native-coin transfer against a stored, time-limited quote.
    /// This code belongs in the issuer; a client-only result is not trusted.
    pub async fn verify(
        &self,
        quote: &Quote,
        transaction_hash: &str,
    ) -> anyhow::Result<VerifiedPayment> {
        let budget = TimeBudget::verification();
        self.verify_with_budget(quote, transaction_hash, &budget)
            .await
            .map_err(anyhow::Error::new)
    }

    /// Checks a native-coin transfer inside one shared time budget.
    ///
    /// Nodes are contacted in a fixed order; the budget is divided by the attempts that remain, so
    /// the whole chain, across every node, stays inside `budget`. Without a consensus of
    /// [`CONSENSUS_NODES`] the call returns [`VerificationError::RetryLater`] with the ordered
    /// node signal instead of waiting for the slowest node.
    pub async fn verify_with_budget(
        &self,
        quote: &Quote,
        transaction_hash: &str,
        budget: &TimeBudget,
    ) -> Result<VerifiedPayment, VerificationError> {
        let transaction_hash = Self::validate_submission(quote.chain, transaction_hash)?;
        let minimum_units = match quote.minimum_units.parse::<u128>() {
            Ok(minimum_units) if minimum_units > 0 => minimum_units,
            _ => {
                return Err(rejected(
                    "invalid-quote",
                    "stored quote contains an invalid amount".to_owned(),
                    Vec::new(),
                ))
            }
        };

        let nodes = self.endpoints.nodes(quote.chain);
        let mut tracker = ConsensusTracker::new(CONSENSUS_NODES.min(nodes.len()));
        for (order, node) in nodes.iter().enumerate() {
            let attempts_left = nodes.len() - order;
            let Some(slice) = budget.node_slice(attempts_left) else {
                tracker.record_state(order, AttemptState::Skipped, Duration::ZERO, None);
                continue;
            };
            let started = Instant::now();
            match self
                .observe(quote.chain, node, &transaction_hash, slice)
                .await
            {
                Ok(NodeReport::Settled {
                    received_units,
                    confirmed_at,
                }) => {
                    tracker.record_observation(
                        order,
                        NodeObservation::Settled {
                            received_units,
                            confirmed_at,
                        },
                        started.elapsed(),
                    );
                }
                Ok(NodeReport::Invalid { code }) => {
                    tracker.record_observation(
                        order,
                        NodeObservation::Invalid { code },
                        started.elapsed(),
                    );
                }
                Ok(NodeReport::Pending { detail }) => {
                    tracker.record_state(
                        order,
                        AttemptState::Pending,
                        started.elapsed(),
                        Some(detail),
                    );
                }
                Err(error) => {
                    tracker.record_state(
                        order,
                        attempt_state(&error),
                        started.elapsed(),
                        Some(error.to_string()),
                    );
                }
            }
            if let Some(consensus) = tracker.consensus() {
                return self.settle(quote, &transaction_hash, minimum_units, consensus);
            }
        }

        let reason = if tracker
            .attempts()
            .iter()
            .any(|attempt| attempt.state.is_fact())
        {
            RetryReason::NoConsensus
        } else {
            RetryReason::NodesUnavailable
        };
        Err(VerificationError::RetryLater(tracker.retry_signal(
            reason,
            budget.total(),
            budget.elapsed(),
        )))
    }

    /// Canonicalises and validates a submitted transaction hash before any network access.
    ///
    /// Public so an operator can reject an obvious client error, and so the issuer can refuse it
    /// without spending the verification budget on a hash that no node would ever know.
    pub fn validate_submission(
        chain: Chain,
        transaction_hash: &str,
    ) -> Result<String, VerificationError> {
        let transaction_hash = canonical_transaction_hash(chain, transaction_hash.trim());
        if validate_transaction_hash(chain, &transaction_hash).is_err() {
            return Err(rejected(
                "invalid-transaction-hash",
                format!("submitted {} transaction hash is malformed", chain.as_str()),
                Vec::new(),
            ));
        }
        Ok(transaction_hash)
    }

    /// Applies the quote policy to a fact that enough nodes agreed on.
    fn settle(
        &self,
        quote: &Quote,
        transaction_hash: &str,
        minimum_units: u128,
        consensus: Consensus,
    ) -> Result<VerifiedPayment, VerificationError> {
        let attempts = consensus.attempts;
        let (received_units, confirmed_at) = match consensus.observation {
            NodeObservation::Invalid { code } => {
                return Err(rejected(
                    code.as_str(),
                    code.describe().to_owned(),
                    attempts,
                ))
            }
            NodeObservation::Settled {
                received_units,
                confirmed_at,
            } => (received_units, confirmed_at),
        };
        if received_units < minimum_units {
            return Err(rejected(
                RejectionCode::BelowMinimum.as_str(),
                RejectionCode::BelowMinimum.describe().to_owned(),
                attempts,
            ));
        }
        if confirmed_at < quote.created_at.saturating_sub(30)
            || confirmed_at > quote.expires_at.saturating_add(30)
        {
            return Err(rejected(
                "outside-quote-window",
                "payment was not made during the quote validity window".to_owned(),
                attempts,
            ));
        }
        let now = Utc::now().timestamp();
        if now
            > quote
                .expires_at
                .saturating_add(QUOTE_SETTLEMENT_WINDOW_SECONDS)
        {
            return Err(rejected(
                "quote-expired",
                "quote is too old to settle".to_owned(),
                attempts,
            ));
        }

        Ok(VerifiedPayment {
            chain: quote.chain,
            transaction_hash: transaction_hash.to_owned(),
            recipient: self.treasury.recipient(quote.chain).to_owned(),
            asset: quote.chain.asset().to_owned(),
            received_units: received_units.to_string(),
            confirmed_at,
            finalized: true,
        })
    }

    /// Asks one node about the submitted transfer, inside the attempt's slice of the budget.
    async fn observe(
        &self,
        chain: Chain,
        node: &str,
        transaction_hash: &str,
        slice: Duration,
    ) -> anyhow::Result<NodeReport> {
        let clock = AttemptClock::new(slice);
        match chain {
            Chain::Solana => self.solana_observation(node, transaction_hash, clock).await,
            Chain::Bitcoin => {
                self.bitcoin_observation(node, transaction_hash, clock)
                    .await
            }
            Chain::Ethereum => {
                self.ethereum_observation(node, transaction_hash, clock)
                    .await
            }
        }
    }

    async fn solana_observation(
        &self,
        node: &str,
        signature: &str,
        clock: AttemptClock,
    ) -> anyhow::Result<NodeReport> {
        let body = self
            .client
            .post(node)
            .timeout(clock.request_timeout()?)
            .json(&json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "getTransaction",
                "params": [signature, {
                    "encoding": "jsonParsed",
                    "commitment": "finalized",
                    "maxSupportedTransactionVersion": 0
                }]
            }))
            .send()
            .await
            .context("could not reach the configured Solana RPC")?
            .error_for_status()
            .context("Solana RPC returned an unsuccessful HTTP status")?
            .json::<Value>()
            .await
            .context("Solana RPC returned invalid JSON")?;
        if let Some(error) = body.get("error") {
            bail!("Solana RPC error: {error}");
        }
        let Some(result) = body.get("result").filter(|value| !value.is_null()) else {
            return Ok(NodeReport::Pending {
                detail: "transaction is not finalized or does not exist on this node".to_owned(),
            });
        };
        let meta = result
            .get("meta")
            .context("Solana transaction has no metadata")?;
        if !meta.get("err").unwrap_or(&Value::Null).is_null() {
            return Ok(NodeReport::Invalid {
                code: RejectionCode::FailedOnChain,
            });
        }
        let returned_signature = result
            .pointer("/transaction/signatures/0")
            .and_then(Value::as_str)
            .context("Solana response does not contain the transaction signature")?;
        if returned_signature != signature {
            bail!("Solana RPC returned a different transaction signature");
        }
        let confirmed_at = result
            .get("blockTime")
            .and_then(Value::as_i64)
            .context("Solana transaction has no confirmed block time")?;
        let instructions = result
            .pointer("/transaction/message/instructions")
            .and_then(Value::as_array)
            .context("Solana transaction has no parsed instructions")?;
        let mut received = 0_u128;
        for instruction in instructions {
            if instruction.get("program").and_then(Value::as_str) != Some("system") {
                continue;
            }
            let Some(parsed) = instruction.get("parsed") else {
                continue;
            };
            if parsed.get("type").and_then(Value::as_str) != Some("transfer") {
                continue;
            }
            let Some(info) = parsed.get("info") else {
                continue;
            };
            if info.get("destination").and_then(Value::as_str)
                != Some(self.treasury.solana.as_str())
            {
                continue;
            }
            let source = info
                .get("source")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if source.is_empty() || source == self.treasury.solana {
                continue;
            }
            let lamports = value_as_u128(info.get("lamports"))
                .context("Solana transfer instruction has invalid lamports")?;
            received = received
                .checked_add(lamports)
                .context("Solana transfer amount overflow")?;
        }
        if received == 0 {
            return Ok(NodeReport::Invalid {
                code: RejectionCode::NoTransfer,
            });
        }
        Ok(NodeReport::Settled {
            received_units: received,
            confirmed_at,
        })
    }

    async fn bitcoin_observation(
        &self,
        node: &str,
        txid: &str,
        clock: AttemptClock,
    ) -> anyhow::Result<NodeReport> {
        let base = node.trim_end_matches('/');
        let response = self
            .client
            .get(format!("{base}/tx/{txid}"))
            .timeout(clock.request_timeout()?)
            .send()
            .await
            .context("could not reach the configured Bitcoin explorer API")?;
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(NodeReport::Pending {
                detail: "transaction is unknown to this explorer".to_owned(),
            });
        }
        let transaction: Value = response
            .error_for_status()
            .context("Bitcoin explorer returned an unsuccessful HTTP status")?
            .json()
            .await
            .context("Bitcoin explorer returned invalid transaction JSON")?;
        if transaction.get("txid").and_then(Value::as_str) != Some(txid) {
            bail!("Bitcoin explorer returned a different transaction id");
        }
        let status = transaction
            .get("status")
            .context("Bitcoin transaction has no confirmation status")?;
        if status.get("confirmed").and_then(Value::as_bool) != Some(true) {
            return Ok(NodeReport::Pending {
                detail: "transaction is not confirmed".to_owned(),
            });
        }
        let height = status
            .get("block_height")
            .and_then(Value::as_u64)
            .context("confirmed Bitcoin transaction has no block height")?;
        let confirmed_at = status
            .get("block_time")
            .and_then(Value::as_i64)
            .context("confirmed Bitcoin transaction has no block time")?;
        let tip_text = self
            .client
            .get(format!("{base}/blocks/tip/height"))
            .timeout(clock.request_timeout()?)
            .send()
            .await
            .context("could not read Bitcoin chain tip")?
            .error_for_status()
            .context("Bitcoin explorer returned an unsuccessful tip response")?
            .text()
            .await
            .context("could not read Bitcoin chain tip body")?;
        let tip = tip_text
            .trim()
            .parse::<u64>()
            .context("invalid Bitcoin tip height")?;
        let confirmations = tip.saturating_sub(height).saturating_add(1);
        if confirmations < BTC_CONFIRMATIONS_REQUIRED {
            return Ok(NodeReport::Pending {
                detail: format!(
                    "transaction has {confirmations} of {BTC_CONFIRMATIONS_REQUIRED} confirmations"
                ),
            });
        }
        let outputs = transaction
            .get("vout")
            .and_then(Value::as_array)
            .context("Bitcoin transaction has no outputs")?;
        let mut received = 0_u128;
        for output in outputs {
            if output.get("scriptpubkey_address").and_then(Value::as_str)
                != Some(self.treasury.bitcoin.as_str())
            {
                continue;
            }
            let sats = output
                .get("value")
                .and_then(Value::as_u64)
                .context("Bitcoin output has an invalid satoshi value")?;
            received = received
                .checked_add(u128::from(sats))
                .context("Bitcoin transfer amount overflow")?;
        }
        if received == 0 {
            return Ok(NodeReport::Invalid {
                code: RejectionCode::NoTransfer,
            });
        }
        Ok(NodeReport::Settled {
            received_units: received,
            confirmed_at,
        })
    }

    async fn ethereum_observation(
        &self,
        node: &str,
        tx_hash: &str,
        clock: AttemptClock,
    ) -> anyhow::Result<NodeReport> {
        let chain_id = self
            .ethereum_rpc(node, "eth_chainId", json!([]), clock)
            .await?;
        if parse_hex_quantity(&chain_id)? != 1 {
            bail!("configured Ethereum RPC is not connected to Ethereum mainnet");
        }
        let transaction = self
            .ethereum_rpc(node, "eth_getTransactionByHash", json!([tx_hash]), clock)
            .await?;
        if transaction.is_null() {
            return Ok(NodeReport::Pending {
                detail: "transaction is unknown to this node".to_owned(),
            });
        }
        let returned_hash = transaction
            .get("hash")
            .and_then(Value::as_str)
            .context("Ethereum RPC transaction has no hash")?;
        if !returned_hash.eq_ignore_ascii_case(tx_hash) {
            bail!("Ethereum RPC returned a different transaction hash");
        }
        if !transaction
            .get("to")
            .and_then(Value::as_str)
            .is_some_and(|address| address.eq_ignore_ascii_case(&self.treasury.ethereum))
        {
            return Ok(NodeReport::Invalid {
                code: RejectionCode::WrongRecipient,
            });
        }
        let received = parse_hex_quantity(
            transaction
                .get("value")
                .context("Ethereum transaction has no value")?,
        )?;
        let receipt = self
            .ethereum_rpc(node, "eth_getTransactionReceipt", json!([tx_hash]), clock)
            .await?;
        if receipt.is_null() {
            return Ok(NodeReport::Pending {
                detail: "transaction has no receipt yet".to_owned(),
            });
        }
        if parse_hex_quantity(
            receipt
                .get("status")
                .context("Ethereum receipt has no status")?,
        )? != 1
        {
            return Ok(NodeReport::Invalid {
                code: RejectionCode::FailedOnChain,
            });
        }
        let tx_block = parse_hex_quantity(
            receipt
                .get("blockNumber")
                .context("Ethereum receipt has no block number")?,
        )?;
        let finalized_block = self
            .ethereum_rpc(
                node,
                "eth_getBlockByNumber",
                json!(["finalized", false]),
                clock,
            )
            .await?;
        let finalized_number = parse_hex_quantity(
            finalized_block
                .get("number")
                .context("Ethereum RPC did not return a finalized block")?,
        )?;
        if tx_block > finalized_number {
            return Ok(NodeReport::Pending {
                detail: "transaction is not finalized yet".to_owned(),
            });
        }
        let block_tag = format!("0x{tx_block:x}");
        let block = self
            .ethereum_rpc(
                node,
                "eth_getBlockByNumber",
                json!([block_tag, false]),
                clock,
            )
            .await?;
        let confirmed_at = parse_hex_quantity(
            block
                .get("timestamp")
                .context("Ethereum block has no timestamp")?,
        )?;
        let confirmed_at: i64 = confirmed_at
            .try_into()
            .context("Ethereum block timestamp is out of range")?;
        Ok(NodeReport::Settled {
            received_units: received,
            confirmed_at,
        })
    }

    async fn ethereum_rpc(
        &self,
        node: &str,
        method: &str,
        params: Value,
        clock: AttemptClock,
    ) -> anyhow::Result<Value> {
        let response: Value = self
            .client
            .post(node)
            .timeout(clock.request_timeout()?)
            .json(&json!({
                "jsonrpc": "2.0",
                "id": 17,
                "method": method,
                "params": params
            }))
            .send()
            .await
            .with_context(|| format!("could not call Ethereum RPC method {method}"))?
            .error_for_status()
            .with_context(|| format!("Ethereum RPC method {method} returned an HTTP error"))?
            .json()
            .await
            .with_context(|| format!("Ethereum RPC method {method} returned invalid JSON"))?;
        if let Some(error) = response.get("error") {
            bail!("Ethereum RPC error for {method}: {error}");
        }
        response
            .get("result")
            .cloned()
            .with_context(|| format!("Ethereum RPC response for {method} has no result"))
    }
}

/// Classifies a failed attempt for the consensus log without string matching.
fn attempt_state(error: &anyhow::Error) -> AttemptState {
    let timed_out = error.downcast_ref::<AttemptTimeout>().is_some()
        || error
            .downcast_ref::<reqwest::Error>()
            .is_some_and(reqwest::Error::is_timeout);
    if timed_out {
        AttemptState::TimedOut
    } else {
        AttemptState::Unreachable
    }
}

fn rejected(code: &'static str, detail: String, attempts: Vec<NodeAttempt>) -> VerificationError {
    VerificationError::Rejected {
        code,
        detail,
        attempts,
    }
}

fn canonical_transaction_hash(chain: Chain, transaction_hash: &str) -> String {
    match chain {
        Chain::Solana => transaction_hash.to_owned(),
        Chain::Bitcoin => transaction_hash.to_ascii_lowercase(),
        Chain::Ethereum => {
            let body = transaction_hash
                .strip_prefix("0x")
                .or_else(|| transaction_hash.strip_prefix("0X"))
                .unwrap_or(transaction_hash);
            format!("0x{}", body.to_ascii_lowercase())
        }
    }
}

fn validate_transaction_hash(chain: Chain, transaction_hash: &str) -> Result<()> {
    match chain {
        Chain::Solana => {
            let bytes = bs58::decode(transaction_hash)
                .into_vec()
                .context("Solana transaction signature is not valid base58")?;
            if bytes.len() != 64 {
                bail!("Solana transaction signature must decode to 64 bytes");
            }
        }
        Chain::Bitcoin => {
            if transaction_hash.len() != 64
                || !transaction_hash
                    .bytes()
                    .all(|byte| byte.is_ascii_hexdigit())
            {
                bail!("Bitcoin transaction id must be 64 hexadecimal characters");
            }
        }
        Chain::Ethereum => {
            let hash = transaction_hash
                .strip_prefix("0x")
                .unwrap_or(transaction_hash);
            if hash.len() != 64 || !hash.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                bail!("Ethereum transaction hash must contain 32 bytes of hexadecimal data");
            }
        }
    }
    Ok(())
}

fn value_as_u128(value: Option<&Value>) -> Option<u128> {
    let value = value?;
    value
        .as_u64()
        .map(u128::from)
        .or_else(|| value.as_str()?.parse::<u128>().ok())
}

fn parse_hex_quantity(value: &Value) -> Result<u128> {
    let text = value
        .as_str()
        .context("Ethereum JSON-RPC quantity is not a hex string")?;
    let digits = text.strip_prefix("0x").unwrap_or(text);
    if digits.is_empty() {
        return Ok(0);
    }
    u128::from_str_radix(digits, 16).context("Ethereum quantity is invalid or exceeds u128")
}

fn env_value(name: &str, default: String) -> String {
    env::var(name)
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or(default)
}

#[cfg(test)]
mod tests {
    use super::{parse_hex_quantity, validate_transaction_hash};
    use crate::model::Chain;
    use serde_json::json;

    #[test]
    fn parses_ethereum_quantities_without_floating_point() {
        assert_eq!(parse_hex_quantity(&json!("0x0")).unwrap(), 0);
        assert_eq!(parse_hex_quantity(&json!("0x2a")).unwrap(), 42);
        assert_eq!(
            parse_hex_quantity(&json!("0x16345785d8a0000")).unwrap(),
            100_000_000_000_000_000
        );
    }

    #[test]
    fn rejects_malformed_hashes_before_network_access() {
        assert!(validate_transaction_hash(Chain::Bitcoin, &"a".repeat(64)).is_ok());
        assert!(validate_transaction_hash(Chain::Bitcoin, "xyz").is_err());
        assert!(
            validate_transaction_hash(Chain::Ethereum, &format!("0x{}", "a".repeat(64))).is_ok()
        );
        assert!(validate_transaction_hash(Chain::Solana, "abc").is_err());
    }
}
