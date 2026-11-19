//! Loading and sealing of the issuer signing seed.
//!
//! The Ed25519 signing seed is the only secret that can issue licenses. It must not sit in a
//! plaintext environment variable of a long-running service: this module therefore reads
//! `ISSUER_SECRET_KEY`, which normally holds an **encrypted envelope** (sealed with ChaCha20-Poly1305
//! from a PBKDF2-HMAC-SHA256 key) whose passphrase comes from a second environment variable. A
//! plaintext seed is refused unless the operator explicitly opts in on a loopback development host.
//!
//! Envelope format:
//!
//! ```text
//! enc:v1:pbkdf2-hmac-sha256:<iterations>:<salt-hex>:<nonce-hex>:<ciphertext-and-tag-hex>
//! ```
//!
//! The authenticated additional data is the envelope header
//! (`enc:v1:pbkdf2-hmac-sha256:<iterations>:<salt-hex>`), so altering the KDF parameters, salt or
//! algorithm makes the envelope fail to open rather than silently weakening it.

use anyhow::{anyhow, bail, Context, Result};
use championvibe_licensing_core::LicenseSigner;
use ring::{
    aead::{Aad, LessSafeKey, Nonce, UnboundKey, CHACHA20_POLY1305, NONCE_LEN},
    pbkdf2,
    rand::{SecureRandom, SystemRandom},
};
use std::{
    env, fs,
    io::Read,
    num::NonZeroU32,
    path::{Path, PathBuf},
};

/// Encrypted secret variable. Holds an envelope, or a plaintext seed only with the opt-in below.
pub const SECRET_ENV: &str = "ISSUER_SECRET_KEY";
/// Alternative source for the envelope, for secret stores that mount a file.
pub const SECRET_FILE_ENV: &str = "ISSUER_SECRET_KEY_FILE";
/// Passphrase that opens the envelope.
pub const PASSPHRASE_ENV: &str = "ISSUER_SECRET_KEY_PASSPHRASE";
/// File holding the passphrase, for secret stores that mount a file.
pub const PASSPHRASE_FILE_ENV: &str = "ISSUER_SECRET_KEY_PASSPHRASE_FILE";
/// Legacy plaintext variable kept for local development only.
pub const LEGACY_SECRET_ENV: &str = "CHAMPIONVIBE_SIGNING_KEY_HEX";
/// Explicit opt-in for a plaintext seed, intended for loopback development hosts.
pub const PLAINTEXT_OPT_IN_ENV: &str = "CHAMPIONVIBE_ALLOW_PLAINTEXT_SECRET";

const ENVELOPE_PREFIX: &str = "enc:v1:";
const ENVELOPE_ALGORITHM: &str = "pbkdf2-hmac-sha256";
const SALT_LEN: usize = 16;
const TAG_LEN: usize = 16;
const SEED_LEN: usize = 32;
const WRAP_KEY_LEN: usize = 32;
/// PBKDF2-HMAC-SHA256 iterations used when sealing a new envelope.
pub const DEFAULT_ITERATIONS: u32 = 600_000;
/// Lower bound accepted when opening an envelope; a lower value is treated as tampering.
pub const MIN_ITERATIONS: u32 = 100_000;
/// Upper bound accepted when opening an envelope, so a hostile envelope cannot burn CPU.
pub const MAX_ITERATIONS: u32 = 4_000_000;

/// Loads the issuer signer from the encrypted environment.
///
/// Returns a signer whose seed buffer is wiped before this function returns.
pub fn load_signer() -> Result<LicenseSigner> {
    let (value, source) = secret_value()?;
    if !is_envelope(&value) {
        ensure_plaintext_allowed(&source)?;
        let mut seed = decode_plaintext_seed(&value)
            .with_context(|| format!("{source} does not contain a valid 32-byte seed"))?;
        let signer = LicenseSigner::from_seed_bytes(&seed).context("invalid signing seed")?;
        wipe(&mut seed);
        return Ok(signer);
    }

    let passphrase = passphrase()?;
    let mut seed = open(&value, passphrase.as_bytes())
        .with_context(|| format!("could not open the sealed secret in {source}"))?;
    let signer = LicenseSigner::from_seed_bytes(&seed).context("invalid signing seed")?;
    wipe(&mut seed);
    Ok(signer)
}

/// Seals a plaintext seed into an envelope. Exposed for the `--seal-secret-key` command and tests.
pub fn seal(seed: &[u8], passphrase: &[u8], iterations: u32) -> Result<String> {
    if seed.len() != SEED_LEN {
        bail!("the signing seed must be exactly {SEED_LEN} bytes");
    }
    if passphrase.is_empty() {
        bail!("the sealing passphrase must not be empty");
    }
    let iterations = iterations.clamp(MIN_ITERATIONS, MAX_ITERATIONS);
    let mut salt = [0_u8; SALT_LEN];
    let mut nonce = [0_u8; NONCE_LEN];
    let random = SystemRandom::new();
    random
        .fill(&mut salt)
        .map_err(|_| anyhow!("the operating system random generator failed"))?;
    random
        .fill(&mut nonce)
        .map_err(|_| anyhow!("the operating system random generator failed"))?;

    let header = header(iterations, &salt);
    let key = derive_key(passphrase, &salt, iterations)?;
    let mut buffer = seed.to_vec();
    let tag = key
        .seal_in_place_separate_tag(
            Nonce::assume_unique_for_key(nonce),
            Aad::from(header.as_bytes()),
            &mut buffer,
        )
        .map_err(|_| anyhow!("could not seal the signing seed"))?;
    buffer.extend_from_slice(tag.as_ref());
    Ok(format!(
        "{header}:{}:{}",
        hex::encode(nonce),
        hex::encode(&buffer)
    ))
}

/// Opens an envelope produced by [`seal`].
pub fn open(envelope: &str, passphrase: &[u8]) -> Result<Vec<u8>> {
    if passphrase.is_empty() {
        bail!("the sealing passphrase must not be empty");
    }
    let rest = envelope
        .trim()
        .strip_prefix(ENVELOPE_PREFIX)
        .with_context(|| format!("envelope must start with {ENVELOPE_PREFIX}"))?;
    let parts: Vec<&str> = rest.split(':').collect();
    if parts.len() != 5 {
        bail!("envelope must be {ENVELOPE_PREFIX}{ENVELOPE_ALGORITHM}:<iterations>:<salt>:<nonce>:<ciphertext>");
    }
    let (algorithm, iterations, salt_hex, nonce_hex, ciphertext_hex) =
        (parts[0], parts[1], parts[2], parts[3], parts[4]);
    if algorithm != ENVELOPE_ALGORITHM {
        bail!("envelope algorithm {algorithm} is not supported");
    }
    let iterations: u32 = iterations
        .parse()
        .context("envelope iteration count is not a number")?;
    if !(MIN_ITERATIONS..=MAX_ITERATIONS).contains(&iterations) {
        bail!("envelope iteration count {iterations} is outside the accepted range");
    }
    // Require the canonical spelling so the header reconstructed below is byte-identical.
    if iterations.to_string() != parts[1] {
        bail!("envelope iteration count must be written without leading zeros");
    }
    let salt = hex::decode(salt_hex).context("envelope salt is not hexadecimal")?;
    if salt.len() != SALT_LEN {
        bail!("envelope salt must be {SALT_LEN} bytes");
    }
    let nonce = hex::decode(nonce_hex).context("envelope nonce is not hexadecimal")?;
    let nonce: [u8; NONCE_LEN] = nonce
        .try_into()
        .map_err(|_| anyhow!("envelope nonce must be {NONCE_LEN} bytes"))?;
    let mut buffer =
        hex::decode(ciphertext_hex).context("envelope ciphertext is not hexadecimal")?;
    if buffer.len() != SEED_LEN + TAG_LEN {
        bail!("envelope ciphertext must hold a {SEED_LEN}-byte seed and a {TAG_LEN}-byte tag");
    }

    let header = header(iterations, &salt);
    let key = derive_key(passphrase, &salt, iterations)?;
    let plain = key
        .open_in_place(
            Nonce::assume_unique_for_key(nonce),
            Aad::from(header.as_bytes()),
            &mut buffer,
        )
        .map_err(|_| {
            anyhow!("could not open the envelope: wrong passphrase, tampered header or damage")
        })?;
    if plain.len() != SEED_LEN {
        bail!("opened envelope did not hold a {SEED_LEN}-byte seed");
    }
    let seed = plain.to_vec();
    wipe(&mut buffer);
    Ok(seed)
}

/// Reads a plaintext seed (hex) from stdin and prints the sealed envelope on stdout.
///
/// The seed is never passed as an argument or stored in the environment, so it stays out of the
/// shell history and the process list.
pub fn seal_command() -> Result<()> {
    let passphrase = passphrase()?;
    eprintln!(
        "Reading the {SEED_LEN}-byte signing seed as hex from stdin; the printed envelope belongs in {SECRET_ENV}."
    );
    let mut input = String::new();
    std::io::stdin()
        .read_to_string(&mut input)
        .context("could not read the signing seed from stdin")?;
    let mut seed =
        decode_plaintext_seed(input.trim()).context("stdin does not hold a valid seed")?;
    let envelope = seal(&seed, passphrase.as_bytes(), DEFAULT_ITERATIONS);
    wipe(&mut seed);
    println!("{}", envelope?);
    eprintln!(
        "Sealed. Keep {PASSPHRASE_ENV} in a different secret store from {SECRET_ENV}; both are needed to start the issuer."
    );
    Ok(())
}

/// Whether a value is a sealed envelope rather than a bare seed.
pub fn is_envelope(value: &str) -> bool {
    value.trim().starts_with(ENVELOPE_PREFIX)
}

fn secret_value() -> Result<(String, String)> {
    if let Some(value) = env_value(SECRET_ENV) {
        return Ok((value, SECRET_ENV.to_owned()));
    }
    if let Some(path) = env_value(SECRET_FILE_ENV) {
        let source = format!("{SECRET_FILE_ENV} ({path})");
        let value = read_secret_file(PathBuf::from(path.trim()))?;
        return Ok((value, source));
    }
    if let Some(value) = env_value(LEGACY_SECRET_ENV) {
        eprintln!(
            "deprecation: {LEGACY_SECRET_ENV} is a plaintext seed variable; seal the seed and use {SECRET_ENV}"
        );
        return Ok((value, LEGACY_SECRET_ENV.to_owned()));
    }
    bail!(
        "set {SECRET_ENV} to the sealed signing envelope. Create one with \
         `cargo run -p championvibe-issuer -- --seal-secret-key`, which reads the plaintext seed from stdin"
    )
}

fn passphrase() -> Result<String> {
    if let Some(value) = env_value(PASSPHRASE_ENV) {
        return Ok(value);
    }
    if let Some(path) = env_value(PASSPHRASE_FILE_ENV) {
        let value = read_secret_file(PathBuf::from(path.trim()))
            .with_context(|| format!("could not read {PASSPHRASE_FILE_ENV}"))?;
        if value.is_empty() {
            bail!("{PASSPHRASE_FILE_ENV} is empty");
        }
        return Ok(value);
    }
    bail!(
        "set {PASSPHRASE_ENV} (or {PASSPHRASE_FILE_ENV}) to the passphrase that opens {SECRET_ENV}"
    )
}

fn ensure_plaintext_allowed(source: &str) -> Result<()> {
    if env_value(PLAINTEXT_OPT_IN_ENV).is_some_and(|value| value.trim() == "1") {
        eprintln!(
            "warning: {source} holds a plaintext signing seed; {PLAINTEXT_OPT_IN_ENV}=1 is for a loopback development host only"
        );
        return Ok(());
    }
    bail!(
        "{source} holds a plaintext signing seed. Seal it with \
         `cargo run -p championvibe-issuer -- --seal-secret-key` and set {SECRET_ENV} to the printed \
         envelope, or set {PLAINTEXT_OPT_IN_ENV}=1 for a loopback development host only"
    )
}

fn read_secret_file(path: PathBuf) -> Result<String> {
    let metadata = fs::metadata(&path)
        .with_context(|| format!("could not stat the secret file {}", path.display()))?;
    if !metadata.is_file() {
        bail!("the secret file {} is not a regular file", path.display());
    }
    ensure_private_permissions(&path, &metadata)?;
    let value = fs::read_to_string(&path)
        .with_context(|| format!("could not read the secret file {}", path.display()))?;
    let value = value.trim().to_owned();
    if value.is_empty() {
        bail!("the secret file {} is empty", path.display());
    }
    Ok(value)
}

#[cfg(unix)]
fn ensure_private_permissions(path: &Path, metadata: &fs::Metadata) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let mode = metadata.permissions().mode();
    if mode & 0o077 != 0 {
        bail!(
            "the secret file {} is readable by other users (mode {:04o}); use mode 0600",
            path.display(),
            mode & 0o777
        );
    }
    Ok(())
}

#[cfg(not(unix))]
fn ensure_private_permissions(_path: &Path, _metadata: &fs::Metadata) -> Result<()> {
    Ok(())
}

fn env_value(name: &str) -> Option<String> {
    env::var(name)
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

fn header(iterations: u32, salt: &[u8]) -> String {
    format!(
        "{ENVELOPE_PREFIX}{ENVELOPE_ALGORITHM}:{iterations}:{}",
        hex::encode(salt)
    )
}

fn derive_key(passphrase: &[u8], salt: &[u8], iterations: u32) -> Result<LessSafeKey> {
    let iterations = NonZeroU32::new(iterations).context("PBKDF2 iterations must not be zero")?;
    let mut key = [0_u8; WRAP_KEY_LEN];
    pbkdf2::derive(
        pbkdf2::PBKDF2_HMAC_SHA256,
        iterations,
        salt,
        passphrase,
        &mut key,
    );
    let unbound = UnboundKey::new(&CHACHA20_POLY1305, &key)
        .map_err(|_| anyhow!("could not initialize the envelope cipher"))?;
    wipe(&mut key);
    Ok(LessSafeKey::new(unbound))
}

fn decode_plaintext_seed(value: &str) -> Result<Vec<u8>> {
    let bytes = hex::decode(value.trim()).context("seed is not hexadecimal")?;
    if bytes.len() != SEED_LEN {
        bail!(
            "seed must be {SEED_LEN} bytes ({} hex characters)",
            SEED_LEN * 2
        );
    }
    Ok(bytes)
}

/// Best-effort wipe of secret bytes.
///
/// Rust may already have copied the value elsewhere and an allocator may reuse the buffer, so this
/// is hygiene rather than a guarantee.
fn wipe(bytes: &mut [u8]) {
    for byte in bytes.iter_mut() {
        *byte = 0;
    }
    let _ = std::hint::black_box(bytes);
}

#[cfg(test)]
mod tests {
    use super::{
        is_envelope, open, seal, DEFAULT_ITERATIONS, ENVELOPE_PREFIX, MAX_ITERATIONS,
        MIN_ITERATIONS,
    };

    const SEED: [u8; 32] = [7_u8; 32];

    #[test]
    fn sealed_envelopes_round_trip_and_are_never_plaintext() {
        let envelope = seal(&SEED, b"correct horse battery staple", MIN_ITERATIONS).unwrap();
        assert!(is_envelope(&envelope));
        assert!(envelope.starts_with(ENVELOPE_PREFIX));
        assert!(!envelope.contains("0707070707"));
        let opened = open(&envelope, b"correct horse battery staple").unwrap();
        assert_eq!(opened, SEED);
    }

    #[test]
    fn a_wrong_passphrase_or_tampered_envelope_never_opens() {
        let envelope = seal(&SEED, b"passphrase", MIN_ITERATIONS).unwrap();
        assert!(open(&envelope, b"wrong passphrase").is_err());
        let parts: Vec<&str> = envelope.split(':').collect();
        // Tamper with the KDF parameters, the salt, the nonce and the ciphertext in turn.
        let other_salt = "00".repeat(16);
        let other_nonce = "11".repeat(12);
        let other_ciphertext = "00".repeat(48);
        for (index, replacement) in [
            (3, "400000"),
            (4, other_salt.as_str()),
            (5, other_nonce.as_str()),
            (6, other_ciphertext.as_str()),
        ] {
            let mut tampered = parts.clone();
            tampered[index] = replacement;
            assert!(
                open(&tampered.join(":"), b"passphrase").is_err(),
                "envelope part {index} must be authenticated"
            );
        }
    }

    #[test]
    fn malformed_envelopes_are_rejected_with_an_actionable_error() {
        assert!(open("not-an-envelope", b"passphrase").is_err());
        assert!(open("enc:v1:pbkdf2-hmac-sha256:600000:aa:bb", b"passphrase").is_err());
        assert!(open("enc:v1:scrypt:600000:aa:bb:cc", b"passphrase").is_err());
        assert!(open("enc:v1:pbkdf2-hmac-sha256:1:00:11:22", b"passphrase").is_err());
        assert!(open(
            &format!(
                "enc:v1:pbkdf2-hmac-sha256:0600000:{}:11:22",
                "00".repeat(16)
            ),
            b"p"
        )
        .is_err());
        assert!(open("", b"passphrase").is_err());
        assert!(seal(&[0_u8; 31], b"passphrase", MIN_ITERATIONS).is_err());
        assert!(seal(&SEED, b"", MIN_ITERATIONS).is_err());
        assert!(!is_envelope(
            "0707070707070707070707070707070707070707070707070707070707070707"
        ));
    }

    #[test]
    fn iteration_bounds_are_clamped_and_never_disable_the_kdf() {
        let envelope = seal(&SEED, b"passphrase", 1).unwrap();
        let parts: Vec<&str> = envelope.split(':').collect();
        assert_eq!(parts[3], MIN_ITERATIONS.to_string());
        let envelope = seal(&SEED, b"passphrase", u32::MAX).unwrap();
        let parts: Vec<&str> = envelope.split(':').collect();
        assert_eq!(parts[3], MAX_ITERATIONS.to_string());
        assert!(open(&envelope, b"passphrase").is_ok());
    }

    #[test]
    fn two_seals_of_the_same_seed_differ() {
        let first = seal(&SEED, b"passphrase", MIN_ITERATIONS).unwrap();
        let second = seal(&SEED, b"passphrase", MIN_ITERATIONS).unwrap();
        assert_ne!(first, second, "salt and nonce must be fresh for every seal");
        assert_eq!(open(&first, b"passphrase").unwrap(), SEED);
        assert_eq!(open(&second, b"passphrase").unwrap(), SEED);
        assert_eq!(DEFAULT_ITERATIONS, 600_000);
    }
}
