use anyhow::{bail, Context, Result};
use axum::{
    extract::State,
    http::StatusCode,
    routing::{get, post},
    Json, Router,
};
use championvibe_licensing_core::{
    ActivateRequest, ApiEndpoints, CreateQuoteRequest, LicensePayload, LicenseSigner,
    PaymentVerifier, Plan, PriceOracle, Quote, SignedLicense, Treasury,
};
use chrono::{Months, TimeZone, Utc};
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use serde_json::json;
use std::{
    env,
    net::SocketAddr,
    path::PathBuf,
    sync::{Arc, Mutex},
};
use tokio::net::TcpListener;
use uuid::Uuid;

const DB_DEFAULT: &str = "issuer-data/licenses.sqlite";

#[derive(Clone)]
struct AppState {
    database: Arc<Mutex<Connection>>,
    oracle: PriceOracle,
    verifier: PaymentVerifier,
    treasury: Treasury,
    signer: Arc<LicenseSigner>,
}

type ApiError = (StatusCode, String);

#[tokio::main]
async fn main() -> Result<()> {
    if env::args().any(|argument| argument == "--print-public-key") {
        let signer = load_signer()?;
        println!("{}", signer.public_key_hex());
        return Ok(());
    }

    let signer = Arc::new(load_signer()?);
    let endpoints = ApiEndpoints::from_env();
    let treasury = Treasury::from_env();
    let oracle = PriceOracle::new(endpoints.price_oracle_rpc.clone())?;
    let verifier = PaymentVerifier::new(endpoints, treasury.clone())?;
    let database_path = env::var("CHAMPIONVIBE_ISSUER_DB").unwrap_or_else(|_| DB_DEFAULT.to_owned());
    let database = open_database(PathBuf::from(database_path))?;
    let state = AppState {
        database: Arc::new(Mutex::new(database)),
        oracle,
        verifier,
        treasury,
        signer,
    };

    let app = Router::new()
        .route("/health", get(health))
        .route("/v1/quotes", post(create_quote))
        .route("/v1/activate", post(activate))
        .with_state(state);

    let address: SocketAddr = env::var("CHAMPIONVIBE_ISSUER_BIND")
        .unwrap_or_else(|_| "127.0.0.1:8080".to_owned())
        .parse()
        .context("CHAMPIONVIBE_ISSUER_BIND must be a valid socket address")?;
    let listener = TcpListener::bind(address)
        .await
        .with_context(|| format!("could not bind license issuer to {address}"))?;
    println!("ChampionVibe issuer listening on http://{address}");
    axum::serve(listener, app).await.context("issuer HTTP server failed")?;
    Ok(())
}

fn load_signer() -> Result<LicenseSigner> {
    let seed = env::var("CHAMPIONVIBE_SIGNING_KEY_HEX")
        .context("set CHAMPIONVIBE_SIGNING_KEY_HEX on the issuer host")?;
    LicenseSigner::from_seed_hex(&seed)
}

fn open_database(path: PathBuf) -> Result<Connection> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("could not create issuer data directory {}", parent.display()))?;
    }
    let connection = Connection::open(&path)
        .with_context(|| format!("could not open issuer database {}", path.display()))?;
    connection.pragma_update(None, "journal_mode", "WAL")?;
    connection.pragma_update(None, "synchronous", "NORMAL")?;
    connection.pragma_update(None, "foreign_keys", "ON")?;
    connection.busy_timeout(std::time::Duration::from_secs(5))?;
    connection.execute_batch(
        "CREATE TABLE IF NOT EXISTS payment_quotes (
            id TEXT PRIMARY KEY NOT NULL,
            payload TEXT NOT NULL,
            created_at INTEGER NOT NULL,
            expires_at INTEGER NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_payment_quotes_expires
            ON payment_quotes(expires_at);
        CREATE TABLE IF NOT EXISTS subscriptions (
            device_binding TEXT PRIMARY KEY NOT NULL,
            expires_at INTEGER NOT NULL
        );
        CREATE TABLE IF NOT EXISTS activations (
            transaction_key TEXT PRIMARY KEY NOT NULL,
            quote_id TEXT NOT NULL UNIQUE,
            device_binding TEXT NOT NULL,
            transaction_hash TEXT NOT NULL,
            license_json TEXT NOT NULL,
            expires_at INTEGER NOT NULL
        );",
    )?;
    Ok(connection)
}

async fn health() -> Json<serde_json::Value> {
    Json(json!({ "ok": true, "service": "championvibe-license-issuer" }))
}

async fn create_quote(
    State(state): State<AppState>,
    Json(request): Json<CreateQuoteRequest>,
) -> Result<Json<Quote>, ApiError> {
    let quote = state
        .oracle
        .create_quote(request.chain, request.plan, &state.treasury)
        .await
        .map_err(|error| internal_error("could not create a price quote", error))?;
    let payload = serde_json::to_string(&quote)
        .map_err(|error| internal_error("could not serialize price quote", error))?;
    let database = state
        .database
        .lock()
        .map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, "issuer database lock poisoned".to_owned()))?;
    database
        .execute(
            "INSERT INTO payment_quotes (id, payload, created_at, expires_at) VALUES (?1, ?2, ?3, ?4)",
            params![quote.id, payload, quote.created_at, quote.expires_at],
        )
        .map_err(|error| internal_error("could not store price quote", error))?;
    Ok(Json(quote))
}

async fn activate(
    State(state): State<AppState>,
    Json(request): Json<ActivateRequest>,
) -> Result<Json<SignedLicense>, ApiError> {
    let binding = normalize_device_binding(&request.device_binding)?;
    if request.quote_id.len() > 80 || request.quote_id.trim().is_empty() {
        return Err((StatusCode::BAD_REQUEST, "invalid quote id".to_owned()));
    }

    let quote_json: Option<String> = {
        let database = state
            .database
            .lock()
            .map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, "issuer database lock poisoned".to_owned()))?;
        database
            .query_row(
                "SELECT payload FROM payment_quotes WHERE id = ?1",
                [&request.quote_id],
                |row| row.get(0),
            )
            .optional()
            .map_err(|error| internal_error("could not load payment quote", error))?
    };
    let quote: Quote = quote_json
        .as_deref()
        .ok_or_else(|| (StatusCode::NOT_FOUND, "payment quote not found".to_owned()))
        .and_then(|json| {
            serde_json::from_str(json)
                .map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, "stored payment quote is invalid".to_owned()))
        })?;

    let transaction_hash = normalize_transaction_hash(quote.chain, &request.transaction_hash)?;
    let transaction_key = format!("{}:{transaction_hash}", quote.chain.as_str());

    // Return an identical previous response if a client retries after a lost HTTP reply.
    if let Some(existing) = lookup_activation(&state, &transaction_key)? {
        if existing.device_binding == binding
            && existing.quote_id == quote.id
            && existing.transaction_hash == transaction_hash
        {
            return Ok(Json(existing.license));
        }
        return Err((StatusCode::CONFLICT, "transaction has already been used".to_owned()));
    }

    if let Some(existing) = lookup_quote_activation(&state, &quote.id)? {
        if existing.device_binding == binding && existing.transaction_hash == transaction_hash {
            return Ok(Json(existing.license));
        }
        return Err((StatusCode::CONFLICT, "quote has already been used".to_owned()));
    }

    state
        .verifier
        .verify(&quote, &transaction_hash)
        .await
        .map_err(|error| (StatusCode::UNPROCESSABLE_ENTITY, format!("payment verification failed: {error:#}")))?;

    let issued_at = Utc::now().timestamp();
    let mut database = state
        .database
        .lock()
        .map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, "issuer database lock poisoned".to_owned()))?;
    let transaction = database
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|error| internal_error("could not start activation transaction", error))?;

    // Check replay constraints again while holding SQLite's immediate write lock.
    if let Some(existing) = lookup_activation_in(&transaction, &transaction_key)? {
        transaction
            .rollback()
            .map_err(|error| internal_error("could not close activation transaction", error))?;
        if existing.device_binding == binding
            && existing.quote_id == quote.id
            && existing.transaction_hash == transaction_hash
        {
            return Ok(Json(existing.license));
        }
        return Err((StatusCode::CONFLICT, "transaction has already been used".to_owned()));
    }
    if lookup_quote_activation_in(&transaction, &quote.id)?.is_some() {
        transaction
            .rollback()
            .map_err(|error| internal_error("could not close activation transaction", error))?;
        return Err((StatusCode::CONFLICT, "quote has already been used".to_owned()));
    }

    let now = Utc::now();
    let previous_expiry: Option<i64> = transaction
        .query_row(
            "SELECT expires_at FROM subscriptions WHERE device_binding = ?1",
            [&binding],
            |row| row.get(0),
        )
        .optional()
        .map_err(|error| internal_error("could not load subscription", error))?;
    let start = previous_expiry
        .filter(|expires_at| *expires_at > now.timestamp())
        .and_then(|expires_at| Utc.timestamp_opt(expires_at, 0).single())
        .unwrap_or(now);
    let expires_at = start
        .checked_add_months(Months::new(quote.plan.duration_months()))
        .ok_or_else(|| (StatusCode::INTERNAL_SERVER_ERROR, "could not calculate subscription expiry".to_owned()))?
        .timestamp();

    let license = state
        .signer
        .sign(LicensePayload {
            version: 1,
            license_id: Uuid::new_v4().to_string(),
            subject_binding: binding.clone(),
            plan: quote.plan,
            chain: quote.chain,
            transaction_hash: transaction_hash.clone(),
            issued_at,
            expires_at,
            features: vec!["record".to_owned(), "save".to_owned()],
        })
        .map_err(|error| internal_error("could not sign license", error))?;
    let license_json = serde_json::to_string(&license)
        .map_err(|error| internal_error("could not serialize signed license", error))?;

    transaction
        .execute(
            "INSERT INTO activations
                (transaction_key, quote_id, device_binding, transaction_hash, license_json, expires_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![transaction_key, quote.id, binding, transaction_hash, license_json, expires_at],
        )
        .map_err(|error| internal_error("could not store activation", error))?;
    transaction
        .execute(
            "INSERT INTO subscriptions (device_binding, expires_at) VALUES (?1, ?2)
             ON CONFLICT(device_binding) DO UPDATE SET expires_at = excluded.expires_at",
            params![binding, expires_at],
        )
        .map_err(|error| internal_error("could not update subscription", error))?;
    transaction
        .commit()
        .map_err(|error| internal_error("could not commit activation", error))?;

    Ok(Json(license))
}

#[derive(Debug)]
struct StoredActivation {
    device_binding: String,
    quote_id: String,
    transaction_hash: String,
    license: SignedLicense,
}

fn lookup_activation(state: &AppState, transaction_key: &str) -> Result<Option<StoredActivation>, ApiError> {
    let database = state
        .database
        .lock()
        .map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, "issuer database lock poisoned".to_owned()))?;
    load_activation_by_tx(&database, transaction_key)
}

fn lookup_quote_activation(state: &AppState, quote_id: &str) -> Result<Option<StoredActivation>, ApiError> {
    let database = state
        .database
        .lock()
        .map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, "issuer database lock poisoned".to_owned()))?;
    load_activation_by_quote(&database, quote_id)
}

fn lookup_activation_in(
    database: &Connection,
    transaction_key: &str,
) -> Result<Option<StoredActivation>, ApiError> {
    load_activation_by_tx(database, transaction_key)
}

fn lookup_quote_activation_in(
    database: &Connection,
    quote_id: &str,
) -> Result<Option<StoredActivation>, ApiError> {
    load_activation_by_quote(database, quote_id)
}

fn load_activation_by_tx(database: &Connection, transaction_key: &str) -> Result<Option<StoredActivation>, ApiError> {
    load_activation(
        database,
        "SELECT device_binding, quote_id, transaction_hash, license_json
         FROM activations WHERE transaction_key = ?1",
        transaction_key,
    )
}

fn load_activation_by_quote(database: &Connection, quote_id: &str) -> Result<Option<StoredActivation>, ApiError> {
    load_activation(
        database,
        "SELECT device_binding, quote_id, transaction_hash, license_json
         FROM activations WHERE quote_id = ?1",
        quote_id,
    )
}

fn load_activation(
    database: &Connection,
    sql: &str,
    key: &str,
) -> Result<Option<StoredActivation>, ApiError> {
    let row: Option<(String, String, String, String)> = database
        .query_row(sql, [key], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)))
        .optional()
        .map_err(|error| internal_error("could not check activation replay", error))?;
    row.map(|(device_binding, quote_id, transaction_hash, json)| {
        let license = serde_json::from_str(&json)
            .map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, "stored signed license is invalid".to_owned()))?;
        Ok(StoredActivation {
            device_binding,
            quote_id,
            transaction_hash,
            license,
        })
    })
    .transpose()
}

fn normalize_device_binding(binding: &str) -> Result<String, ApiError> {
    let binding = binding.trim();
    let decoded = hex::decode(binding)
        .map_err(|_| (StatusCode::BAD_REQUEST, "device binding must be SHA-256 hex".to_owned()))?;
    if decoded.len() != 32 {
        return Err((StatusCode::BAD_REQUEST, "device binding must be 32 bytes".to_owned()));
    }
    Ok(binding.to_ascii_lowercase())
}

fn normalize_transaction_hash(chain: championvibe_licensing_core::Chain, hash: &str) -> Result<String, ApiError> {
    let hash = hash.trim();
    if hash.len() > 100 {
        return Err((StatusCode::BAD_REQUEST, "transaction hash is too long".to_owned()));
    }
    match chain {
        championvibe_licensing_core::Chain::Bitcoin => Ok(hash.to_ascii_lowercase()),
        championvibe_licensing_core::Chain::Ethereum => {
            let body = hash.strip_prefix("0x").or_else(|| hash.strip_prefix("0X")).unwrap_or(hash);
            Ok(format!("0x{}", body.to_ascii_lowercase()))
        }
        championvibe_licensing_core::Chain::Solana => Ok(hash.to_owned()),
    }
}

fn internal_error(context: &str, error: impl std::fmt::Display) -> ApiError {
    eprintln!("{context}: {error}");
    (StatusCode::INTERNAL_SERVER_ERROR, context.to_owned())
}
