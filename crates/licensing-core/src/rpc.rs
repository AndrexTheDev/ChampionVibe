use anyhow::{bail, Context, Result};
use chrono::Utc;
use reqwest::Client;
use serde_json::{json, Value};
use std::{env, time::Duration};

use crate::model::{Chain, Quote, VerifiedPayment};

const SOLANA_RPC_DEFAULT: &str = "https://api.mainnet-beta.solana.com";
const BITCOIN_API_DEFAULT: &str = "https://blockstream.info/api";
const ETHEREUM_RPC_DEFAULT: &str = "https://cloudflare-eth.com";
const SOLANA_RECEIVER_DEFAULT: &str = "79KsqtJJdhKFJ9woxnYgtf3nq7HxQveafWBCtC3mxWi8";
const BITCOIN_RECEIVER_DEFAULT: &str = "bc1qeqzrlfg3edrydk4s0hecakc82gp26n5p7hkc7f";
const ETHEREUM_RECEIVER_DEFAULT: &str = "0xBC3fab34f69bc9f6661608C3FB36dDdC313C42F7";
const BTC_CONFIRMATIONS_REQUIRED: u64 = 6;
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

impl PaymentVerifier {
    pub fn new(endpoints: ApiEndpoints, treasury: Treasury) -> Result<Self> {
        treasury.validate()?;
        let client = Client::builder()
            .timeout(Duration::from_secs(20))
            .user_agent("ChampionVibe-License-Issuer/0.1")
            .build()
            .context("could not initialize payment RPC HTTP client")?;
        Ok(Self {
            client,
            endpoints,
            treasury,
        })
    }

    /// Checks a native-coin transfer against a stored, time-limited quote.
    /// This code belongs in the issuer; a client-only result is not trusted.
    pub async fn verify(&self, quote: &Quote, transaction_hash: &str) -> Result<VerifiedPayment> {
        let transaction_hash = canonical_transaction_hash(quote.chain, transaction_hash.trim());
        validate_transaction_hash(quote.chain, &transaction_hash)?;
        let minimum_units = quote
            .minimum_units
            .parse::<u128>()
            .context("stored quote contains an invalid amount")?;
        if minimum_units == 0 {
            bail!("stored quote has a zero payment amount");
        }

        let (received_units, confirmed_at) = match quote.chain {
            Chain::Solana => self.verify_solana(&transaction_hash).await?,
            Chain::Bitcoin => self.verify_bitcoin(&transaction_hash).await?,
            Chain::Ethereum => self.verify_ethereum(&transaction_hash).await?,
        };
        if received_units < minimum_units {
            bail!("confirmed transfer is below the quoted minimum amount");
        }
        if confirmed_at < quote.created_at.saturating_sub(30)
            || confirmed_at > quote.expires_at.saturating_add(30)
        {
            bail!("payment was not made during the quote validity window");
        }
        let now = Utc::now().timestamp();
        if now
            > quote
                .expires_at
                .saturating_add(QUOTE_SETTLEMENT_WINDOW_SECONDS)
        {
            bail!("quote is too old to settle");
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

    async fn verify_solana(&self, signature: &str) -> Result<(u128, i64)> {
        let body = self
            .client
            .post(&self.endpoints.solana_rpc)
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
            .context("could not reach the public Solana RPC")?
            .error_for_status()
            .context("Solana RPC returned an unsuccessful HTTP status")?
            .json::<Value>()
            .await
            .context("Solana RPC returned invalid JSON")?;
        if let Some(error) = body.get("error") {
            bail!("Solana RPC error: {error}");
        }
        let result = body
            .get("result")
            .filter(|value| !value.is_null())
            .context("Solana transaction is not finalized or does not exist")?;
        let meta = result
            .get("meta")
            .context("Solana transaction has no metadata")?;
        if !meta.get("err").unwrap_or(&Value::Null).is_null() {
            bail!("Solana transaction failed on chain");
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
            bail!("Solana transaction contains no direct SOL transfer to the configured receiver");
        }
        Ok((received, confirmed_at))
    }

    async fn verify_bitcoin(&self, txid: &str) -> Result<(u128, i64)> {
        let base = self.endpoints.bitcoin_api.trim_end_matches('/');
        let transaction: Value = self
            .client
            .get(format!("{base}/tx/{txid}"))
            .send()
            .await
            .context("could not reach the public Bitcoin explorer API")?
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
            bail!("Bitcoin transaction is not confirmed");
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
            bail!("Bitcoin transaction needs at least six confirmations");
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
            bail!("Bitcoin transaction contains no output to the configured receiver");
        }
        Ok((received, confirmed_at))
    }

    async fn verify_ethereum(&self, tx_hash: &str) -> Result<(u128, i64)> {
        let chain_id = self.ethereum_rpc("eth_chainId", json!([])).await?;
        if parse_hex_quantity(&chain_id)? != 1 {
            bail!("configured Ethereum RPC is not connected to Ethereum mainnet");
        }
        let transaction = self
            .ethereum_rpc("eth_getTransactionByHash", json!([tx_hash]))
            .await?;
        if transaction.is_null() {
            bail!("Ethereum transaction was not found");
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
            bail!("Ethereum transaction recipient does not match the configured receiver");
        }
        let received = parse_hex_quantity(
            transaction
                .get("value")
                .context("Ethereum transaction has no value")?,
        )?;
        let receipt = self
            .ethereum_rpc("eth_getTransactionReceipt", json!([tx_hash]))
            .await?;
        if receipt.is_null() {
            bail!("Ethereum transaction does not have a receipt");
        }
        if parse_hex_quantity(
            receipt
                .get("status")
                .context("Ethereum receipt has no status")?,
        )? != 1
        {
            bail!("Ethereum transaction failed on chain");
        }
        let tx_block = parse_hex_quantity(
            receipt
                .get("blockNumber")
                .context("Ethereum receipt has no block number")?,
        )?;
        let finalized_block = self
            .ethereum_rpc("eth_getBlockByNumber", json!(["finalized", false]))
            .await?;
        let finalized_number = parse_hex_quantity(
            finalized_block
                .get("number")
                .context("Ethereum RPC did not return a finalized block")?,
        )?;
        if tx_block > finalized_number {
            bail!("Ethereum transaction is not finalized yet");
        }
        let block_tag = format!("0x{tx_block:x}");
        let block = self
            .ethereum_rpc("eth_getBlockByNumber", json!([block_tag, false]))
            .await?;
        let confirmed_at = parse_hex_quantity(
            block
                .get("timestamp")
                .context("Ethereum block has no timestamp")?,
        )?;
        let confirmed_at: i64 = confirmed_at
            .try_into()
            .context("Ethereum block timestamp is out of range")?;
        Ok((received, confirmed_at))
    }

    async fn ethereum_rpc(&self, method: &str, params: Value) -> Result<Value> {
        let response: Value = self
            .client
            .post(&self.endpoints.ethereum_rpc)
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
