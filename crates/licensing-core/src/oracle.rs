use anyhow::{bail, Context, Result};
use chrono::Utc;
use reqwest::Client;
use rust_decimal::{prelude::ToPrimitive, Decimal};
use serde_json::{json, Value};
use std::time::Duration;
use uuid::Uuid;

use crate::model::{Chain, Plan, Quote};
use crate::rpc::Treasury;

const ETHEREUM_RPC_DEFAULT: &str = "https://cloudflare-eth.com";
const BTC_USD_FEED: &str = "0xf4030086522a5beea4988f8ca5b36dbc97bee88c";
const ETH_USD_FEED: &str = "0x5f4ec3df9cbd43714fe2740f5e3616155c5b8419";
const SOL_USD_FEED: &str = "0x4ffc43a60e009b551865a93d232e33fce9f01507";
const EUR_USD_FEED: &str = "0xb49f677943bc038e9857d61e7d053caa2c1734c1";
const LATEST_ROUND_DATA_SELECTOR: &str = "0xfeaf968c";
const DECIMALS_SELECTOR: &str = "0x313ce567";
const EUR_MAX_AGE_SECONDS: i64 = 72 * 60 * 60;

#[derive(Clone)]
pub struct PriceOracle {
    client: Client,
    endpoint: String,
}

#[derive(Debug, Clone)]
struct FeedPrice {
    value: Decimal,
    updated_at: i64,
}

impl PriceOracle {
    pub fn new(endpoint: impl Into<String>) -> Result<Self> {
        let client = Client::builder()
            .timeout(Duration::from_secs(12))
            .user_agent("ChampionVibe/0.1")
            .build()
            .context("could not initialize HTTP client for Chainlink RPC")?;
        Ok(Self {
            client,
            endpoint: endpoint.into(),
        })
    }

    pub fn public_default() -> Result<Self> {
        Self::new(ETHEREUM_RPC_DEFAULT)
    }

    pub async fn create_quote(
        &self,
        chain: Chain,
        plan: Plan,
        treasury: &Treasury,
    ) -> Result<Quote> {
        let chain_id = self.rpc("eth_chainId", json!([])).await?;
        if parse_hex_quantity(&chain_id)? != 1 {
            bail!("configured oracle RPC is not connected to Ethereum mainnet");
        }

        let asset_feed = match chain {
            Chain::Bitcoin => BTC_USD_FEED,
            Chain::Ethereum => ETH_USD_FEED,
            Chain::Solana => SOL_USD_FEED,
        };
        let asset_price = self.read_feed(asset_feed).await?;
        let eur_usd = self.read_feed(EUR_USD_FEED).await?;
        let now = Utc::now().timestamp();
        validate_feed("asset/USD", &asset_price, now, asset_max_age(chain))?;
        validate_feed("EUR/USD", &eur_usd, now, EUR_MAX_AGE_SECONDS)?;

        // Chainlink's on-chain EUR/USD feed is USD for one EUR. Convert the EUR
        // plan price to USD, divide by the native-asset/USD feed, then round up
        // to the next smallest on-chain unit. No API key or paid price service.
        let target_usd = Decimal::from(plan.price_eur())
            .checked_mul(eur_usd.value)
            .context("price calculation overflow")?;
        let native_units = target_usd
            .checked_div(asset_price.value)
            .and_then(|amount| amount.checked_mul(Decimal::from(10_u64.pow(chain.decimals()))))
            .context("native token amount calculation failed")?
            .ceil();
        let minimum_units = native_units
            .to_u128()
            .filter(|value| *value > 0)
            .context("calculated payment amount is outside the supported range")?;
        let scale = 10_u128.pow(chain.decimals());
        let display_amount = format_units(minimum_units, scale, chain.decimals());

        Ok(Quote {
            id: Uuid::new_v4().to_string(),
            chain,
            plan,
            amount_eur: plan.price_eur(),
            asset: chain.asset().to_owned(),
            recipient: treasury.recipient(chain).to_owned(),
            minimum_units: minimum_units.to_string(),
            display_amount,
            asset_usd: asset_price.value.normalize().to_string(),
            eur_usd: eur_usd.value.normalize().to_string(),
            price_published_at: asset_price.updated_at.min(eur_usd.updated_at),
            created_at: now,
            expires_at: now + 15 * 60,
        })
    }

    async fn read_feed(&self, address: &str) -> Result<FeedPrice> {
        let price_data = self
            .eth_call(address, LATEST_ROUND_DATA_SELECTOR)
            .await
            .with_context(|| format!("could not read Chainlink latestRoundData for {address}"))?;
        let decimals_data = self
            .eth_call(address, DECIMALS_SELECTOR)
            .await
            .with_context(|| format!("could not read Chainlink decimals for {address}"))?;
        if price_data.len() < 32 * 5 {
            bail!("Chainlink latestRoundData returned fewer than five ABI words");
        }
        let round_id = word_u128(&price_data, 0)?;
        let answer = word_u128(&price_data, 1)?;
        let updated_at: i64 = word_u128(&price_data, 3)?
            .try_into()
            .context("Chainlink update timestamp is out of range")?;
        let answered_in_round = word_u128(&price_data, 4)?;
        let decimals: u32 = word_u128(&decimals_data, 0)?
            .try_into()
            .context("Chainlink decimals are out of range")?;
        if decimals > 18 || answer == 0 || answered_in_round < round_id {
            bail!("Chainlink feed returned an invalid round, answer or precision");
        }
        let answer: i128 = answer
            .try_into()
            .context("Chainlink price answer is outside the supported range")?;
        let value = Decimal::from_i128_with_scale(answer, decimals);
        Ok(FeedPrice { value, updated_at })
    }

    async fn eth_call(&self, address: &str, data: &str) -> Result<Vec<u8>> {
        let result = self
            .rpc("eth_call", json!([{"to": address, "data": data}, "latest"]))
            .await?;
        let encoded = result
            .as_str()
            .context("Ethereum eth_call did not return a hex string")?;
        hex::decode(encoded.strip_prefix("0x").unwrap_or(encoded))
            .context("Ethereum eth_call returned malformed ABI hex")
    }

    async fn rpc(&self, method: &str, params: Value) -> Result<Value> {
        let response: Value = self
            .client
            .post(&self.endpoint)
            .json(&json!({
                "jsonrpc": "2.0",
                "id": 91,
                "method": method,
                "params": params
            }))
            .send()
            .await
            .with_context(|| format!("could not call oracle Ethereum RPC method {method}"))?
            .error_for_status()
            .with_context(|| format!("oracle Ethereum RPC method {method} returned an HTTP error"))?
            .json()
            .await
            .with_context(|| {
                format!("oracle Ethereum RPC method {method} returned invalid JSON")
            })?;
        if let Some(error) = response.get("error") {
            bail!("oracle Ethereum RPC error for {method}: {error}");
        }
        response
            .get("result")
            .cloned()
            .with_context(|| format!("oracle Ethereum RPC response for {method} has no result"))
    }
}

fn asset_max_age(chain: Chain) -> i64 {
    match chain {
        Chain::Bitcoin => 2 * 60 * 60,
        Chain::Ethereum => 2 * 60 * 60,
        // The on-chain SOL/USD feed updates on a longer heartbeat than BTC/ETH.
        Chain::Solana => 30 * 60 * 60,
    }
}

fn validate_feed(name: &str, price: &FeedPrice, now: i64, max_age: i64) -> Result<()> {
    let age = now.saturating_sub(price.updated_at);
    if age > max_age || age < -30 {
        bail!("{name} Chainlink round is stale or from the future");
    }
    if price.value <= Decimal::ZERO {
        bail!("{name} Chainlink answer is not positive");
    }
    // Reject implausibly tiny values before converting the quote. Chainlink's
    // reporting itself is a decentralized consensus feed.
    if price.value < Decimal::new(1, 8) {
        bail!("{name} Chainlink answer is implausibly small");
    }
    Ok(())
}

fn word_u128(data: &[u8], index: usize) -> Result<u128> {
    let start = index.checked_mul(32).context("ABI word index overflow")?;
    let word = data
        .get(start..start + 32)
        .context("Chainlink ABI response is missing a word")?;
    if word[..16].iter().any(|byte| *byte != 0) {
        bail!("Chainlink ABI integer exceeds u128");
    }
    let mut low = [0_u8; 16];
    low.copy_from_slice(&word[16..]);
    Ok(u128::from_be_bytes(low))
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

fn format_units(units: u128, scale: u128, decimals: u32) -> String {
    let whole = units / scale;
    let remainder = units % scale;
    if remainder == 0 {
        return whole.to_string();
    }
    let fractional = format!("{:0width$}", remainder, width = decimals as usize);
    let fractional = fractional.trim_end_matches('0');
    format!("{whole}.{fractional}")
}

#[cfg(test)]
mod tests {
    use super::{format_units, word_u128};

    #[test]
    fn formats_native_units_without_float_rounding() {
        assert_eq!(format_units(1_230_000_000, 1_000_000_000, 9), "1.23");
        assert_eq!(format_units(12_000, 100_000_000, 8), "0.00012");
        assert_eq!(
            format_units(7, 1_000_000_000_000_000_000, 18),
            "0.000000000000000007"
        );
        assert_eq!(format_units(100, 100, 2), "1");
    }

    #[test]
    fn decodes_chainlink_abi_words_as_big_endian_integers() {
        let mut data = vec![0_u8; 160];
        data[56..64].copy_from_slice(&42_u64.to_be_bytes());
        assert_eq!(word_u128(&data, 1).unwrap(), 42);
        assert!(word_u128(&data[..32], 1).is_err());
    }
}
