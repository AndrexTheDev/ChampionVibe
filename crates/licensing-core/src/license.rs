use anyhow::{bail, Context, Result};
use base64::{engine::general_purpose::STANDARD, Engine as _};
use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};

use crate::model::{LicensePayload, SignedLicense};

const LICENSE_DOMAIN: &[u8] = b"ChampionVibe-License\0v1\0";

/// The issuer-only signing key. Never construct this type inside the Tauri client.
pub struct LicenseSigner {
    signing_key: SigningKey,
}

impl LicenseSigner {
    /// Reads a 32-byte Ed25519 seed encoded as hex. The caller must source it from
    /// a secret manager or an environment variable on the issuer host.
    pub fn from_seed_hex(seed_hex: &str) -> Result<Self> {
        let bytes = hex::decode(seed_hex.trim()).context("invalid signing seed hex")?;
        let seed: [u8; 32] = bytes
            .try_into()
            .map_err(|_| anyhow::anyhow!("Ed25519 signing seed must be exactly 32 bytes"))?;
        Ok(Self {
            signing_key: SigningKey::from_bytes(&seed),
        })
    }

    pub fn public_key_hex(&self) -> String {
        hex::encode(self.signing_key.verifying_key().to_bytes())
    }

    pub fn sign(&self, payload: LicensePayload) -> Result<SignedLicense> {
        let message = license_signing_bytes(&payload)?;
        let signature = self.signing_key.sign(&message);
        Ok(SignedLicense {
            payload,
            signature: STANDARD.encode(signature.to_bytes()),
        })
    }
}

pub fn decode_public_key_hex(public_key_hex: &str) -> Result<[u8; 32]> {
    let bytes = hex::decode(public_key_hex.trim()).context("invalid license public key hex")?;
    bytes
        .try_into()
        .map_err(|_| anyhow::anyhow!("Ed25519 public key must be exactly 32 bytes"))
}

/// Verifies issuer authenticity, device binding, entitlement and strict expiry.
/// This is a client-side check; a determined owner of the device can still patch
/// or replace the application binary.
pub fn verify_signed_license(
    signed: &SignedLicense,
    public_key_bytes: &[u8; 32],
    expected_binding: &str,
    now: i64,
) -> Result<()> {
    let public_key = VerifyingKey::from_bytes(public_key_bytes)
        .context("invalid embedded Ed25519 verification key")?;
    let signature_bytes = STANDARD
        .decode(&signed.signature)
        .context("license signature is not valid base64")?;
    let signature =
        Signature::from_slice(&signature_bytes).context("license signature must be 64 bytes")?;
    let message = license_signing_bytes(&signed.payload)?;
    public_key
        .verify_strict(&message, &signature)
        .context("license signature verification failed")?;

    let payload = &signed.payload;
    if payload.version != 1 {
        bail!("unsupported license format version");
    }
    if payload.license_id.is_empty() || payload.transaction_hash.is_empty() {
        bail!("license is missing required identifiers");
    }
    if payload.subject_binding != expected_binding {
        bail!("license belongs to a different device binding");
    }
    if payload.expires_at <= payload.issued_at {
        bail!("license has an invalid validity interval");
    }
    if payload.issued_at > now.saturating_add(300) {
        bail!("license issue time is unexpectedly in the future");
    }
    // No grace interval: an entitlement expires at the signed UTC timestamp.
    if now >= payload.expires_at {
        bail!("license has expired");
    }
    if !payload.features.iter().any(|feature| feature == "record")
        || !payload.features.iter().any(|feature| feature == "save")
    {
        bail!("license does not include recording and saving entitlements");
    }
    Ok(())
}

/// A versioned, length-prefixed representation avoids relying on JSON key order,
/// whitespace or serializer behavior for signature verification.
pub fn license_signing_bytes(payload: &LicensePayload) -> Result<Vec<u8>> {
    let mut bytes = Vec::with_capacity(256);
    bytes.extend_from_slice(LICENSE_DOMAIN);
    bytes.push(payload.version);
    push_string(&mut bytes, &payload.license_id)?;
    push_string(&mut bytes, &payload.subject_binding)?;
    push_string(&mut bytes, payload.plan.as_str())?;
    push_string(&mut bytes, payload.chain.as_str())?;
    push_string(&mut bytes, &payload.transaction_hash)?;
    bytes.extend_from_slice(&payload.issued_at.to_be_bytes());
    bytes.extend_from_slice(&payload.expires_at.to_be_bytes());
    let feature_count: u32 = payload
        .features
        .len()
        .try_into()
        .map_err(|_| anyhow::anyhow!("too many license features"))?;
    bytes.extend_from_slice(&feature_count.to_be_bytes());
    for feature in &payload.features {
        push_string(&mut bytes, feature)?;
    }
    Ok(bytes)
}

fn push_string(bytes: &mut Vec<u8>, value: &str) -> Result<()> {
    let length: u32 = value
        .len()
        .try_into()
        .map_err(|_| anyhow::anyhow!("license field is too large"))?;
    bytes.extend_from_slice(&length.to_be_bytes());
    bytes.extend_from_slice(value.as_bytes());
    Ok(())
}
