use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Chain {
    Solana,
    Bitcoin,
    Ethereum,
}

impl Chain {
    pub fn asset(self) -> &'static str {
        match self {
            Self::Solana => "SOL",
            Self::Bitcoin => "BTC",
            Self::Ethereum => "ETH",
        }
    }

    pub fn decimals(self) -> u32 {
        match self {
            Self::Solana => 9,
            Self::Bitcoin => 8,
            Self::Ethereum => 18,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Solana => "solana",
            Self::Bitcoin => "bitcoin",
            Self::Ethereum => "ethereum",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Plan {
    Monthly,
    Annual,
}

impl Plan {
    pub fn price_eur(self) -> u32 {
        match self {
            Self::Monthly => 19,
            Self::Annual => 150,
        }
    }

    pub fn duration_months(self) -> u32 {
        match self {
            Self::Monthly => 1,
            Self::Annual => 12,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Monthly => "monthly",
            Self::Annual => "annual",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateQuoteRequest {
    pub chain: Chain,
    pub plan: Plan,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Quote {
    pub id: String,
    pub chain: Chain,
    pub plan: Plan,
    pub amount_eur: u32,
    pub asset: String,
    pub recipient: String,
    /// Minimum amount in the chain's smallest unit (lamports, satoshis, or wei).
    /// A string avoids precision loss in JavaScript for large wei values.
    pub minimum_units: String,
    /// Human-readable amount rounded to the chain's native precision.
    pub display_amount: String,
    pub asset_usd: String,
    pub eur_usd: String,
    pub price_published_at: i64,
    pub created_at: i64,
    pub expires_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActivateRequest {
    pub quote_id: String,
    pub transaction_hash: String,
    /// SHA-256 application-scoped digest; raw platform identifiers never leave the device.
    pub device_binding: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LicensePayload {
    pub version: u8,
    pub license_id: String,
    pub subject_binding: String,
    pub plan: Plan,
    pub chain: Chain,
    pub transaction_hash: String,
    pub issued_at: i64,
    pub expires_at: i64,
    pub features: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SignedLicense {
    pub payload: LicensePayload,
    /// Base64-encoded 64-byte Ed25519 signature over the domain-separated payload.
    pub signature: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerifiedPayment {
    pub chain: Chain,
    pub transaction_hash: String,
    pub recipient: String,
    pub asset: String,
    pub received_units: String,
    pub confirmed_at: i64,
    pub finalized: bool,
}
