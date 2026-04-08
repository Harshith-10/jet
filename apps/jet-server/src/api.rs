use std::{
    collections::HashMap,
    future::Future,
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
    time::{SystemTime, UNIX_EPOCH},
};

use apalis::prelude::TaskSink;
use apalis_redis::RedisStorage;
use axum::{
    Json, Router,
    body::{Body, to_bytes},
    middleware::{self, Next},
    extract::{Path, State},
    http::{HeaderMap, Method, Request, StatusCode},
    response::Response,
    routing::{get, post},
};
use deadpool_redis::{Pool, redis::AsyncCommands};
use hmac::{Hmac, Mac};
use jet_core::models::JobRequest;
use jet_pack::{VersionResolver, manifest::RuntimeManifest};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tower_governor::{
    GovernorLayer,
    errors::GovernorError,
    governor::GovernorConfigBuilder,
    key_extractor::{KeyExtractor, SmartIpKeyExtractor},
};
use tracing::{info, warn};
use url::form_urlencoded;
use uuid::Uuid;

use crate::{
    counters::{saturating_decrement, try_increment_with_limit},
    path_safety::validate_job_id,
    queue::{JobStateRecord, QueuedJob, job_state_key},
};

pub trait RuntimeResolver: Send + Sync {
    fn canonical_language(&self, name: &str) -> String;
    fn resolve(&self, language: &str, requested: &str) -> jet_pack::JetPackResult<Option<String>>;
}

impl<S> RuntimeResolver for VersionResolver<S>
where
    S: jet_pack::resolver::VersionStore + Send + Sync,
{
    fn canonical_language(&self, name: &str) -> String {
        VersionResolver::canonical_language(self, name).to_string()
    }

    fn resolve(&self, language: &str, requested: &str) -> jet_pack::JetPackResult<Option<String>> {
        VersionResolver::resolve(self, language, requested)
    }
}

pub trait JobQueue: Send + Sync {
    fn push_job<'a>(
        &'a self,
        job: QueuedJob,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>>;
}

impl JobQueue for tokio::sync::Mutex<RedisStorage<QueuedJob>> {
    fn push_job<'a>(
        &'a self,
        job: QueuedJob,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>> {
        Box::pin(async move {
            self.lock()
                .await
                .push(job)
                .await
                .map_err(|source| source.to_string())
        })
    }
}

pub type SharedResolver = Arc<dyn RuntimeResolver>;
pub type SharedStorage = Arc<dyn JobQueue>;
pub type SharedManifests = Arc<HashMap<String, RuntimeManifest>>;

#[derive(Clone)]
pub struct ApiState {
    pub resolver: SharedResolver,
    pub manifests: SharedManifests,
    pub storage: SharedStorage,
    pub redis_pool: Pool,
    pub job_state_prefix: String,
    pub start_time: Instant,
    pub jobs_submitted: Arc<AtomicU64>,
    pub jobs_completed: Arc<AtomicU64>,
    pub jobs_failed: Arc<AtomicU64>,
    pub worker_concurrency: usize,
    pub jobs_in_flight: Arc<AtomicU64>,
    pub max_queue_depth: u64,
    /// Per-category in-flight counters for MLP stats.
    pub compile_in_flight: Arc<AtomicU64>,
    pub execute_in_flight: Arc<AtomicU64>,
    /// Concurrency limits for stats reporting.
    pub compile_concurrency: usize,
    pub execute_concurrency: usize,
    /// Max queue wait time before shedding (seconds).
    pub max_queue_wait_secs: u64,
    /// Key identifier for HMAC v2 request signing.
    pub rate_limit_hmac_key_id: Option<String>,
    /// Shared secret used to validate signed user identity headers
    /// for per-user rate limiting.
    pub rate_limit_hmac_secret: Option<Arc<[u8]>>,
    /// Max accepted absolute timestamp skew for signed rate-limit headers.
    pub rate_limit_timestamp_tolerance_secs: i64,
    /// Nonce replay protection TTL in seconds.
    pub rate_limit_nonce_ttl_secs: u64,
    /// Strict limiter: one token replenished every N seconds.
    pub strict_rate_limit_token_interval_secs: u64,
    pub strict_rate_limit_burst: u32,
    /// General limiter: one token replenished every N milliseconds.
    pub general_rate_limit_token_interval_ms: u64,
    pub general_rate_limit_burst: u32,
    /// Poll limiter: one token replenished every N milliseconds.
    pub poll_rate_limit_token_interval_ms: u64,
    pub poll_rate_limit_burst: u32,
}

const MAX_FILES: usize = 10;
const MAX_TESTCASES: usize = 1000;
const MAX_TOTAL_FILE_SIZE: usize = 5 * 1024 * 1024; // 5 MB
const MAX_IDEMPOTENCY_KEY_LEN: usize = 128;
const IDEMPOTENCY_HEADER: &str = "Idempotency-Key";
const AUTH_VERSION_HEADER: &str = "x-jet-auth-version";
const AUTH_VERSION_V2: &str = "2";
const AUTH_KEY_ID_HEADER: &str = "x-jet-key-id";
const RATE_LIMIT_USER_HEADER: &str = "x-jet-user-id";
const RATE_LIMIT_TIMESTAMP_HEADER: &str = "x-jet-timestamp";
const RATE_LIMIT_NONCE_HEADER: &str = "x-jet-nonce";
const RATE_LIMIT_CONTENT_SHA256_HEADER: &str = "x-jet-content-sha256";
const RATE_LIMIT_SIGNATURE_HEADER: &str = "x-jet-signature";
const HMAC_V2_CANONICAL_PREFIX: &str = "jet-hmac-v2";
const MAX_NONCE_LEN: usize = 128;
const NONCE_REPLAY_KEY_PREFIX: &str = "jet:auth:nonce";
const MAX_AUTH_BODY_BYTES: usize = 50 * 1024 * 1024;

/// TTL for job state keys set from the API side.
const JOB_STATE_TTL_SECS: i64 = 3600; // 1 hour

type HmacSha256 = Hmac<Sha256>;

#[derive(Clone, Debug)]
struct AuthenticatedUserId(String);

#[derive(Clone, Copy, Debug)]
enum SignedHeaderError {
    MissingAuthVersion,
    UnsupportedAuthVersion,
    MissingKeyId,
    UnknownKeyId,
    MissingSecret,
    MissingUserId,
    EmptyUserId,
    MissingTimestamp,
    InvalidTimestamp,
    TimestampOutOfRange,
    MissingNonce,
    InvalidNonce,
    MissingContentSha256,
    InvalidContentSha256,
    MissingContentType,
    MissingSignature,
    InvalidSignatureEncoding,
    SignatureMismatch,
    BodyHashMismatch,
    NonceReplay,
    BodyReadFailed,
    RedisUnavailable,
    ClockUnavailable,
    HmacInitialization,
}

impl SignedHeaderError {
    fn reason_code(self) -> &'static str {
        match self {
            SignedHeaderError::MissingAuthVersion => "missing_auth_version",
            SignedHeaderError::UnsupportedAuthVersion => "unsupported_auth_version",
            SignedHeaderError::MissingKeyId => "missing_key_id",
            SignedHeaderError::UnknownKeyId => "unknown_key_id",
            SignedHeaderError::MissingSecret => "missing_secret",
            SignedHeaderError::MissingUserId => "missing_user_id",
            SignedHeaderError::EmptyUserId => "empty_user_id",
            SignedHeaderError::MissingTimestamp => "missing_timestamp",
            SignedHeaderError::InvalidTimestamp => "invalid_timestamp",
            SignedHeaderError::TimestampOutOfRange => "timestamp_out_of_range",
            SignedHeaderError::MissingNonce => "missing_nonce",
            SignedHeaderError::InvalidNonce => "invalid_nonce",
            SignedHeaderError::MissingContentSha256 => "missing_content_sha256",
            SignedHeaderError::InvalidContentSha256 => "invalid_content_sha256",
            SignedHeaderError::MissingContentType => "missing_content_type",
            SignedHeaderError::MissingSignature => "missing_signature",
            SignedHeaderError::InvalidSignatureEncoding => "invalid_signature_encoding",
            SignedHeaderError::SignatureMismatch => "signature_mismatch",
            SignedHeaderError::BodyHashMismatch => "body_hash_mismatch",
            SignedHeaderError::NonceReplay => "nonce_replay",
            SignedHeaderError::BodyReadFailed => "body_read_failed",
            SignedHeaderError::RedisUnavailable => "redis_unavailable",
            SignedHeaderError::ClockUnavailable => "clock_unavailable",
            SignedHeaderError::HmacInitialization => "hmac_initialization",
        }
    }
}

#[derive(Clone, Debug)]
struct VerifiedIdentity {
    user_id: String,
    key_id: String,
    nonce: String,
    content_sha256: String,
}

#[derive(Clone)]
struct JetRateLimitKeyExtractor {
    key_id: Option<String>,
    shared_secret: Option<Arc<[u8]>>,
    max_skew_secs: i64,
}

impl JetRateLimitKeyExtractor {
    fn signed_user_key_detailed<T>(
        &self,
        req: &Request<T>,
    ) -> Result<VerifiedIdentity, SignedHeaderError> {
        let key_id = self
            .key_id
            .as_deref()
            .ok_or(SignedHeaderError::MissingKeyId)?;
        let secret = self
            .shared_secret
            .as_ref()
            .ok_or(SignedHeaderError::MissingSecret)?;
        verify_v2_identity(req, key_id, secret.as_ref(), self.max_skew_secs)
    }

    fn signed_user_key<T>(&self, req: &Request<T>) -> Option<String> {
        self.signed_user_key_detailed(req).ok().map(|v| v.user_id)
    }
}

impl KeyExtractor for JetRateLimitKeyExtractor {
    type Key = String;

    fn extract<T>(&self, req: &Request<T>) -> Result<Self::Key, GovernorError> {
        if let Some(AuthenticatedUserId(user_id)) = req.extensions().get::<AuthenticatedUserId>() {
            return Ok(format!("user:{user_id}"));
        }

        if let Some(user_id) = self.signed_user_key(req) {
            return Ok(format!("user:{user_id}"));
        }

        // Protected routes should already be auth-gated. Keep a single fallback
        // bucket for unauthenticated attempts to avoid IP fan-in surprises.
        Ok("user:unauthenticated".to_string())
    }
}

#[derive(Clone, Copy)]
struct GeneralIpKeyExtractor;

impl KeyExtractor for GeneralIpKeyExtractor {
    type Key = String;

    fn extract<T>(&self, req: &Request<T>) -> Result<Self::Key, GovernorError> {
        let ip = SmartIpKeyExtractor.extract(req)?;
        Ok(format!("ip:{ip}"))
    }
}

async fn require_signed_identity(
    State(state): State<ApiState>,
    req: Request<Body>,
    next: Next,
) -> Result<Response, (StatusCode, String)> {
    let extractor = JetRateLimitKeyExtractor {
        key_id: state.rate_limit_hmac_key_id.clone(),
        shared_secret: state.rate_limit_hmac_secret.clone(),
        max_skew_secs: state.rate_limit_timestamp_tolerance_secs,
    };

    let verified = match extractor.signed_user_key_detailed(&req) {
        Ok(verified) => verified,
        Err(reason) => {
            warn!(reason = reason.reason_code(), "rejecting protected request: invalid signed identity headers");
            return Err((
                StatusCode::UNAUTHORIZED,
                format!("invalid signed identity headers ({})", reason.reason_code()),
            ));
        }
    };

    let (parts, body) = req.into_parts();
    let method = parts.method.clone();
    let body_bytes = to_bytes(body, MAX_AUTH_BODY_BYTES).await.map_err(|_| {
        (
            StatusCode::UNAUTHORIZED,
            format!(
                "invalid signed identity headers ({})",
                SignedHeaderError::BodyReadFailed.reason_code()
            ),
        )
    })?;

    if method == Method::POST || method == Method::PUT || method == Method::PATCH {
        let actual_sha = sha256_hex(&body_bytes);
        if actual_sha != verified.content_sha256 {
            return Err((
                StatusCode::UNAUTHORIZED,
                format!(
                    "invalid signed identity headers ({})",
                    SignedHeaderError::BodyHashMismatch.reason_code()
                ),
            ));
        }
    }

    let mut conn = state.redis_pool.get().await.map_err(|_| {
        (
            StatusCode::UNAUTHORIZED,
            format!(
                "invalid signed identity headers ({})",
                SignedHeaderError::RedisUnavailable.reason_code()
            ),
        )
    })?;
    let nonce_key = format!(
        "{}:{}:{}",
        NONCE_REPLAY_KEY_PREFIX, verified.key_id, verified.nonce
    );
    let inserted = conn
        .set_nx::<_, _, bool>(&nonce_key, "1")
        .await
        .map_err(|_| {
            (
                StatusCode::UNAUTHORIZED,
                format!(
                    "invalid signed identity headers ({})",
                    SignedHeaderError::RedisUnavailable.reason_code()
                ),
            )
        })?;
    if !inserted {
        return Err((
            StatusCode::UNAUTHORIZED,
            format!(
                "invalid signed identity headers ({})",
                SignedHeaderError::NonceReplay.reason_code()
            ),
        ));
    }
    if let Err(source) = conn
        .expire::<_, ()>(&nonce_key, state.rate_limit_nonce_ttl_secs as i64)
        .await
    {
        warn!(key = %nonce_key, error = %source, "failed to set nonce expiry");
    }

    let mut req = Request::from_parts(parts, Body::from(body_bytes));
    req.extensions_mut()
        .insert(AuthenticatedUserId(verified.user_id));
    Ok(next.run(req).await)
}

fn verify_v2_identity<T>(
    req: &Request<T>,
    expected_key_id: &str,
    secret: &[u8],
    max_skew_secs: i64,
) -> Result<VerifiedIdentity, SignedHeaderError> {
    let headers = req.headers();

    let auth_version = header_value(headers, AUTH_VERSION_HEADER)
        .ok_or(SignedHeaderError::MissingAuthVersion)?;
    if auth_version != AUTH_VERSION_V2 {
        return Err(SignedHeaderError::UnsupportedAuthVersion);
    }

    let key_id = header_value(headers, AUTH_KEY_ID_HEADER).ok_or(SignedHeaderError::MissingKeyId)?;
    if key_id != expected_key_id {
        return Err(SignedHeaderError::UnknownKeyId);
    }

    let user_id = header_value(headers, RATE_LIMIT_USER_HEADER)
        .ok_or(SignedHeaderError::MissingUserId)?;
    if user_id.is_empty() {
        return Err(SignedHeaderError::EmptyUserId);
    }

    let ts_raw = header_value(headers, RATE_LIMIT_TIMESTAMP_HEADER)
        .ok_or(SignedHeaderError::MissingTimestamp)?;
    let timestamp = ts_raw
        .parse::<i64>()
        .map_err(|_| SignedHeaderError::InvalidTimestamp)?;

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| SignedHeaderError::ClockUnavailable)?
        .as_secs() as i64;
    if (now - timestamp).abs() > max_skew_secs {
        return Err(SignedHeaderError::TimestampOutOfRange);
    }

    let nonce = header_value(headers, RATE_LIMIT_NONCE_HEADER)
        .ok_or(SignedHeaderError::MissingNonce)?;
    if nonce.is_empty()
        || nonce.len() > MAX_NONCE_LEN
        || !nonce
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return Err(SignedHeaderError::InvalidNonce);
    }

    let content_sha256 = header_value(headers, RATE_LIMIT_CONTENT_SHA256_HEADER)
        .ok_or(SignedHeaderError::MissingContentSha256)?;
    if content_sha256.len() != 64 || !content_sha256.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(SignedHeaderError::InvalidContentSha256);
    }

    if (req.method() == Method::POST || req.method() == Method::PUT || req.method() == Method::PATCH)
        && canonical_content_type(headers).is_empty()
    {
        return Err(SignedHeaderError::MissingContentType);
    }

    let sig_hex = header_value(headers, RATE_LIMIT_SIGNATURE_HEADER)
        .ok_or(SignedHeaderError::MissingSignature)?;
    let sig_bytes =
        hex::decode(sig_hex).map_err(|_| SignedHeaderError::InvalidSignatureEncoding)?;

    let canonical = canonical_request_v2(
        req.method(),
        req.uri().path(),
        req.uri().query(),
        &content_sha256,
        &canonical_content_type(headers),
        &user_id,
        &ts_raw,
        &nonce,
        &key_id,
    );

    let mut mac =
        HmacSha256::new_from_slice(secret).map_err(|_| SignedHeaderError::HmacInitialization)?;
    mac.update(canonical.as_bytes());
    if mac.verify_slice(&sig_bytes).is_err() {
        return Err(SignedHeaderError::SignatureMismatch);
    }

    Ok(VerifiedIdentity {
        user_id,
        key_id,
        nonce,
        content_sha256,
    })
}

fn header_value(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.trim().to_string())
}

fn canonical_content_type(headers: &HeaderMap) -> String {
    let Some(raw) = headers
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
    else {
        return String::new();
    };

    raw.split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase()
}

fn canonical_query_string(raw_query: Option<&str>) -> String {
    let Some(raw) = raw_query else {
        return String::new();
    };

    let mut pairs: Vec<(String, String)> = form_urlencoded::parse(raw.as_bytes())
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
    pairs.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));

    pairs
        .into_iter()
        .map(|(k, v)| {
            let key = form_urlencoded::byte_serialize(k.as_bytes()).collect::<String>();
            let value = form_urlencoded::byte_serialize(v.as_bytes()).collect::<String>();
            format!("{key}={value}")
        })
        .collect::<Vec<_>>()
        .join("&")
}

fn canonical_request_v2(
    method: &Method,
    path: &str,
    raw_query: Option<&str>,
    content_sha256: &str,
    content_type: &str,
    user_id: &str,
    timestamp_raw: &str,
    nonce: &str,
    key_id: &str,
) -> String {
    let query = canonical_query_string(raw_query);
    format!(
        "{HMAC_V2_CANONICAL_PREFIX}\n{key_id}\n{timestamp_raw}\n{nonce}\n{}\n{path}\n{query}\n{content_sha256}\n{content_type}\n{user_id}",
        method.as_str().to_ascii_uppercase(),
    )
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    hex::encode(digest)
}

fn validate_job_request(req: &JobRequest) -> Result<(), (StatusCode, String)> {
    if req.files.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            "at least one file is required".to_string(),
        ));
    }

    if req.files.len() > MAX_FILES {
        return Err((
            StatusCode::BAD_REQUEST,
            format!("too many files: {} (max: {})", req.files.len(), MAX_FILES),
        ));
    }

    let total_size: usize = req.files.iter().map(|f| f.content.len()).sum();
    if total_size > MAX_TOTAL_FILE_SIZE {
        return Err((
            StatusCode::PAYLOAD_TOO_LARGE,
            format!(
                "total file size too large: {} bytes (max: {})",
                total_size, MAX_TOTAL_FILE_SIZE
            ),
        ));
    }

    if let Some(testcases) = &req.testcases {
        if testcases.len() > MAX_TESTCASES {
            return Err((
                StatusCode::BAD_REQUEST,
                format!(
                    "too many testcases: {} (max: {})",
                    testcases.len(),
                    MAX_TESTCASES
                ),
            ));
        }

        for (i, tc) in testcases.iter().enumerate() {
            if tc.input.len() > MAX_TOTAL_FILE_SIZE / 10 {
                return Err((
                    StatusCode::BAD_REQUEST,
                    format!("testcase {} input too large", i),
                ));
            }
        }
    }

    Ok(())
}

#[derive(Debug, Serialize)]
pub struct SubmitJobResponse {
    pub job_id: String,
    pub status: String,
    pub resolved_version: String,
}

#[derive(Serialize)]
pub struct RuntimeInfo {
    pub version: String,
    pub aliases: Vec<String>,
    pub architectures: Vec<String>,
    pub compiled: bool,
}

#[derive(Serialize)]
pub struct RuntimesResponse {
    pub total: usize,
    pub languages: HashMap<String, Vec<RuntimeInfo>>,
}

#[derive(Serialize)]
pub struct StatsResponse {
    pub uptime_seconds: u64,
    pub jobs_submitted: u64,
    pub jobs_completed: u64,
    pub jobs_failed: u64,
    pub jobs_in_flight: u64,
    pub compile_in_flight: u64,
    pub execute_in_flight: u64,
    pub max_queue_depth: u64,
    pub installed_runtimes: usize,
    pub supported_languages: Vec<String>,
    pub worker_concurrency: usize,
    pub compile_concurrency: usize,
    pub execute_concurrency: usize,
    pub max_queue_wait_secs: u64,
    pub host_arch: String,
    pub strict_rate_limit_token_interval_secs: u64,
    pub strict_rate_limit_burst: u32,
    pub general_rate_limit_token_interval_ms: u64,
    pub general_rate_limit_burst: u32,
    pub poll_rate_limit_token_interval_ms: u64,
    pub poll_rate_limit_burst: u32,
}

pub fn router(state: ApiState) -> Router {
    // Strict rate limiting: 1 request per second, burst of 3.
    // Keying strategy: signed `x-jet-user-id` first, then fallback to smart IP.
    //
    // NOTE: GovernorConfigBuilder::per_second(n) means "replenish 1 token
    // every n seconds", NOT "n tokens per second".
    let key_extractor = JetRateLimitKeyExtractor {
        key_id: state.rate_limit_hmac_key_id.clone(),
        shared_secret: state.rate_limit_hmac_secret.clone(),
        max_skew_secs: state.rate_limit_timestamp_tolerance_secs,
    };

    let strict_conf = GovernorConfigBuilder::default()
        .per_second(state.strict_rate_limit_token_interval_secs)
        .burst_size(state.strict_rate_limit_burst)
        .key_extractor(key_extractor.clone())
        .finish()
        .unwrap();

    // Background cleanup of expired rate-limit entries.
    let strict_limiter = strict_conf.limiter().clone();
    let interval = Duration::from_secs(60);
    std::thread::spawn(move || {
        loop {
            std::thread::sleep(interval);
            strict_limiter.retain_recent();
        }
    });

    // General rate limiting: 5 requests per second, burst of 10.
    // per_millisecond(200) = 1 token every 200ms = 5 tokens/second.
    let general_conf = GovernorConfigBuilder::default()
        .per_millisecond(state.general_rate_limit_token_interval_ms)
        .burst_size(state.general_rate_limit_burst)
        .key_extractor(GeneralIpKeyExtractor)
        .finish()
        .unwrap();

    let poll_conf = GovernorConfigBuilder::default()
        .per_millisecond(state.poll_rate_limit_token_interval_ms)
        .burst_size(state.poll_rate_limit_burst)
        .key_extractor(key_extractor)
        .finish()
        .unwrap();

    // Background cleanup of expired rate-limit entries.
    let general_limiter = general_conf.limiter().clone();
    let interval = Duration::from_secs(60);
    std::thread::spawn(move || {
        loop {
            std::thread::sleep(interval);
            general_limiter.retain_recent();
        }
    });

    let poll_limiter = poll_conf.limiter().clone();
    let interval = Duration::from_secs(60);
    std::thread::spawn(move || {
        loop {
            std::thread::sleep(interval);
            poll_limiter.retain_recent();
        }
    });

    // Define the "General" routes
    let general_routes = Router::new()
        .route("/health", get(health))
        .route("/runtimes", get(list_runtimes))
        .route("/runtimes/{language}", get(get_language_runtimes))
        .route("/stats", get(get_stats))
        .layer(GovernorLayer::new(general_conf));

    // Define the "Strict" routes
    let submit_routes = Router::new()
        .route("/jobs", post(submit_job))
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            require_signed_identity,
        ))
        .layer(GovernorLayer::new(strict_conf));

    let poll_routes = Router::new()
        .route("/jobs/{id}", get(get_job))
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            require_signed_identity,
        ))
        .layer(GovernorLayer::new(poll_conf));

    // Merge them together
    Router::new()
        .merge(general_routes)
        .merge(submit_routes)
        .merge(poll_routes)
        .with_state(state)
}

async fn health() -> &'static str {
    "ok"
}

async fn submit_job(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Json(mut request): Json<JobRequest>,
) -> Result<(StatusCode, Json<SubmitJobResponse>), (StatusCode, String)> {
    validate_job_request(&request).map_err(|e| {
        warn!(language = %request.language, reason = %e.1, "job submission rejected: validation");
        e
    })?;

    let request_signature = build_idempotency_signature(&request).map_err(internal_error)?;
    let idempotency_key = extract_idempotency_key(&headers)?;

    let requested = request
        .version
        .clone()
        .ok_or((StatusCode::BAD_REQUEST, "version is required".to_string()))?;

    let resolved = state
        .resolver
        .resolve(&request.language, &requested)
        .map_err(internal_error)?
        .ok_or((
            StatusCode::BAD_REQUEST,
            format!(
                "runtime not installed or unsupported: {}:{}",
                request.language, requested
            ),
        ))?;

    let canonical_lang = state.resolver.canonical_language(&request.language);
    let manifest_key = format!("{}:{}", canonical_lang, resolved);
    let manifest = state.manifests.get(&manifest_key).ok_or((
        StatusCode::BAD_REQUEST,
        format!("runtime is not installed: {}", manifest_key),
    ))?;

    if let Some(key) = idempotency_key.as_deref() {
        if let Some(existing) = read_idempotency_record(&state, key).await? {
            if existing.request_signature != request_signature {
                return Err((
                    StatusCode::CONFLICT,
                    "idempotency key was already used with a different request".to_string(),
                ));
            }

            return Ok((
                StatusCode::ACCEPTED,
                Json(SubmitJobResponse {
                    job_id: existing.job_id,
                    status: "queued".to_string(),
                    resolved_version: existing.resolved_version,
                }),
            ));
        }
    }

    // Determine job type based on whether the runtime has a compile step.
    let job_type = if manifest.compile.is_some() {
        crate::queue::JobType::Compile
    } else {
        crate::queue::JobType::Execute
    };

    if !try_increment_with_limit(&state.jobs_in_flight, state.max_queue_depth) {
        let in_flight = state.jobs_in_flight.load(Ordering::Relaxed);
        warn!(
            in_flight = in_flight,
            max = state.max_queue_depth,
            "rejecting job: queue full"
        );
        return Err((
            StatusCode::TOO_MANY_REQUESTS,
            format!(
                "server is overloaded: {} jobs in flight (max: {})",
                in_flight, state.max_queue_depth
            ),
        ));
    }

    let client_job_id = request.job_id.take();
    let job_id = Uuid::new_v4().to_string();
    if let Some(client_job_id) = client_job_id {
        warn!(
            provided_job_id = %client_job_id,
            server_job_id = %job_id,
            "ignoring client-supplied job id"
        );
    }

    let language = request.language.clone();
    request.job_id = Some(job_id.clone());
    request.version = Some(resolved.clone());

    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;

    let queued_state = JobStateRecord {
        job_id: job_id.clone(),
        status: "queued".to_string(),
        language,
        version: resolved.clone(),
        result: None,
        error: None,
        terminal_reason: None,
        queue_wait_ms: None,
    };

    if let Err(err) = write_api_job_state(&state, &queued_state).await {
        saturating_decrement(&state.jobs_in_flight);
        return Err(internal_error(err));
    }

    let job = QueuedJob {
        id: job_id.clone(),
        language: queued_state.language.clone(),
        version: resolved.clone(),
        request,
        enqueued_at: now_ms,
        job_type,
    };

    if let Err(err) = state.storage.push_job(job).await {
        saturating_decrement(&state.jobs_in_flight);
        reconcile_enqueue_failure_state(&state, &job_id, &queued_state.language, &resolved, &err)
            .await;
        return Err(internal_error(err));
    }

    state.jobs_submitted.fetch_add(1, Ordering::Relaxed);

    if let Some(key) = idempotency_key.as_deref() {
        let record = IdempotencyRecord {
            job_id: job_id.clone(),
            resolved_version: resolved.clone(),
            request_signature,
        };
        if let Err(source) = write_idempotency_record(&state, key, &record).await {
            warn!(
                idempotency_key = %key,
                job_id = %job_id,
                error = %source,
                "failed to persist idempotency record"
            );
        }
    }

    info!(
        job_id = %job_id,
        language = %queued_state.language,
        version = %resolved,
        "job submitted"
    );

    Ok((
        StatusCode::ACCEPTED,
        Json(SubmitJobResponse {
            job_id,
            status: "queued".to_string(),
            resolved_version: resolved,
        }),
    ))
}

async fn get_job(
    Path(id): Path<String>,
    State(state): State<ApiState>,
) -> Result<(StatusCode, Json<JobStateRecord>), (StatusCode, String)> {
    validate_job_id(&id).map_err(|reason| (StatusCode::BAD_REQUEST, reason))?;

    let key = job_state_key(&state.job_state_prefix, &id);
    let mut conn = state.redis_pool.get().await.map_err(internal_error)?;
    let raw: Option<String> = conn
        .get::<_, Option<String>>(&key)
        .await
        .map_err(internal_error)?;

    let Some(raw) = raw else {
        return Err((StatusCode::NOT_FOUND, format!("job not found: {}", id)));
    };

    let parsed: JobStateRecord = serde_json::from_str(&raw).map_err(internal_error)?;
    Ok((StatusCode::OK, Json(parsed)))
}

async fn list_runtimes(State(state): State<ApiState>) -> Json<RuntimesResponse> {
    let languages = build_runtime_map(&state.manifests);
    let total: usize = languages.values().map(|v| v.len()).sum();
    Json(RuntimesResponse { total, languages })
}

async fn get_language_runtimes(
    Path(language): Path<String>,
    State(state): State<ApiState>,
) -> Result<Json<RuntimesResponse>, (StatusCode, String)> {
    let all = build_runtime_map(&state.manifests);
    let filtered: HashMap<String, Vec<RuntimeInfo>> = all
        .into_iter()
        .filter(|(lang, _)| lang == &language)
        .collect();

    if filtered.is_empty() {
        return Err((
            StatusCode::NOT_FOUND,
            format!("no installed runtimes for language: {}", language),
        ));
    }

    let total: usize = filtered.values().map(|v| v.len()).sum();
    Ok(Json(RuntimesResponse {
        total,
        languages: filtered,
    }))
}

async fn get_stats(State(state): State<ApiState>) -> Json<StatsResponse> {
    let uptime = state.start_time.elapsed();
    let mut langs: Vec<String> = state
        .manifests
        .values()
        .map(|m| m.language.clone())
        .collect::<std::collections::HashSet<_>>()
        .into_iter()
        .collect();
    langs.sort();

    Json(StatsResponse {
        uptime_seconds: uptime.as_secs(),
        jobs_submitted: state.jobs_submitted.load(Ordering::Relaxed),
        jobs_completed: state.jobs_completed.load(Ordering::Relaxed),
        jobs_failed: state.jobs_failed.load(Ordering::Relaxed),
        jobs_in_flight: state.jobs_in_flight.load(Ordering::Relaxed),
        compile_in_flight: state.compile_in_flight.load(Ordering::Relaxed),
        execute_in_flight: state.execute_in_flight.load(Ordering::Relaxed),
        max_queue_depth: state.max_queue_depth,
        installed_runtimes: state.manifests.len(),
        supported_languages: langs,
        worker_concurrency: state.worker_concurrency,
        compile_concurrency: state.compile_concurrency,
        execute_concurrency: state.execute_concurrency,
        max_queue_wait_secs: state.max_queue_wait_secs,
        host_arch: std::env::consts::ARCH.to_string(),
        strict_rate_limit_token_interval_secs: state.strict_rate_limit_token_interval_secs,
        strict_rate_limit_burst: state.strict_rate_limit_burst,
        general_rate_limit_token_interval_ms: state.general_rate_limit_token_interval_ms,
        general_rate_limit_burst: state.general_rate_limit_burst,
        poll_rate_limit_token_interval_ms: state.poll_rate_limit_token_interval_ms,
        poll_rate_limit_burst: state.poll_rate_limit_burst,
    })
}

fn build_runtime_map(manifests: &SharedManifests) -> HashMap<String, Vec<RuntimeInfo>> {
    let mut map: HashMap<String, Vec<RuntimeInfo>> = HashMap::new();
    for manifest in manifests.values() {
        let mut archs: Vec<String> = manifest.runtimes.keys().cloned().collect();
        archs.sort();
        let info = RuntimeInfo {
            version: manifest.version.clone(),
            aliases: manifest.aliases.clone(),
            architectures: archs,
            compiled: manifest.compile.is_some(),
        };
        map.entry(manifest.language.clone()).or_default().push(info);
    }

    // Sort versions within each language for consistent output
    for versions in map.values_mut() {
        versions.sort_by(|a, b| a.version.cmp(&b.version));
    }

    map
}

fn internal_error<E: std::fmt::Display>(err: E) -> (StatusCode, String) {
    (StatusCode::INTERNAL_SERVER_ERROR, err.to_string())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct IdempotencyRecord {
    job_id: String,
    resolved_version: String,
    request_signature: String,
}

fn build_idempotency_signature(request: &JobRequest) -> Result<String, serde_json::Error> {
    let mut normalized = request.clone();
    normalized.job_id = None;
    serde_json::to_string(&normalized)
}

fn extract_idempotency_key(headers: &HeaderMap) -> Result<Option<String>, (StatusCode, String)> {
    let Some(value) = headers.get(IDEMPOTENCY_HEADER) else {
        return Ok(None);
    };

    let raw = value.to_str().map_err(|_| {
        (
            StatusCode::BAD_REQUEST,
            "Idempotency-Key must be valid ASCII".to_string(),
        )
    })?;

    let key = raw.trim();
    if key.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            "Idempotency-Key cannot be empty".to_string(),
        ));
    }

    if key.len() > MAX_IDEMPOTENCY_KEY_LEN {
        return Err((
            StatusCode::BAD_REQUEST,
            format!("Idempotency-Key is too long (max: {MAX_IDEMPOTENCY_KEY_LEN})"),
        ));
    }

    if !key
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return Err((
            StatusCode::BAD_REQUEST,
            "Idempotency-Key contains invalid characters".to_string(),
        ));
    }

    Ok(Some(key.to_string()))
}

fn idempotency_state_key(prefix: &str, idempotency_key: &str) -> String {
    format!("{}:idempotency:{}", prefix, idempotency_key)
}

async fn read_idempotency_record(
    state: &ApiState,
    idempotency_key: &str,
) -> Result<Option<IdempotencyRecord>, (StatusCode, String)> {
    let key = idempotency_state_key(&state.job_state_prefix, idempotency_key);
    let mut conn = state.redis_pool.get().await.map_err(internal_error)?;
    let raw: Option<String> = conn
        .get::<_, Option<String>>(&key)
        .await
        .map_err(internal_error)?;

    raw.map(|payload| serde_json::from_str::<IdempotencyRecord>(&payload).map_err(internal_error))
        .transpose()
}

async fn write_idempotency_record(
    state: &ApiState,
    idempotency_key: &str,
    record: &IdempotencyRecord,
) -> Result<(), String> {
    let key = idempotency_state_key(&state.job_state_prefix, idempotency_key);
    let payload = serde_json::to_string(record).map_err(|source| source.to_string())?;
    let mut conn = state
        .redis_pool
        .get()
        .await
        .map_err(|source| source.to_string())?;

    let _: () = conn
        .set::<_, _, ()>(&key, &payload)
        .await
        .map_err(|source| source.to_string())?;

    if let Err(source) = conn.expire::<_, ()>(&key, JOB_STATE_TTL_SECS).await {
        warn!(key = %key, error = %source, "failed to set idempotency-key expiry");
    }

    Ok(())
}

async fn write_api_job_state(state: &ApiState, record: &JobStateRecord) -> Result<(), String> {
    let state_key = job_state_key(&state.job_state_prefix, &record.job_id);
    let payload = serde_json::to_string(record).map_err(|source| source.to_string())?;
    let mut conn = state
        .redis_pool
        .get()
        .await
        .map_err(|source| source.to_string())?;

    let _: () = conn
        .set::<_, _, ()>(&state_key, &payload)
        .await
        .map_err(|source| source.to_string())?;

    if let Err(source) = conn.expire::<_, ()>(&state_key, JOB_STATE_TTL_SECS).await {
        warn!(state_key = %state_key, error = %source, "failed to set queued job-state expiry");
    }

    Ok(())
}

async fn reconcile_enqueue_failure_state(
    state: &ApiState,
    job_id: &str,
    language: &str,
    version: &str,
    error: &str,
) {
    let failed_state = JobStateRecord {
        job_id: job_id.to_string(),
        status: "failed".to_string(),
        language: language.to_string(),
        version: version.to_string(),
        result: None,
        error: Some(format!("job was not enqueued: {error}")),
        terminal_reason: Some("enqueue_failed".to_string()),
        queue_wait_ms: None,
    };

    if let Err(source) = write_api_job_state(state, &failed_state).await {
        warn!(
            job_id = %job_id,
            error = %source,
            "failed to reconcile queued state after enqueue failure"
        );
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::{HashMap, VecDeque},
        pin::Pin,
        sync::{Arc, atomic::AtomicU64},
        time::Instant,
        time::{SystemTime, UNIX_EPOCH},
    };

    use axum::{
        extract::Path,
        extract::State,
        http::{HeaderMap, HeaderValue, Request, StatusCode},
    };
    use deadpool_redis::{Config as PoolConfig, Runtime as PoolRuntime, redis::AsyncCommands};
    use hmac::{Hmac, Mac};
    use jet_core::models::{FileRequest, JobRequest};
    use jet_pack::{InMemoryVersionStore, RuntimeArchive, manifest::ExecutionTemplate};
    use mini_redis::server;
    use sha2::Sha256;
    use tokio::{
        net::TcpListener,
        sync::{Mutex, oneshot},
    };
    use tower_governor::key_extractor::KeyExtractor;

    use super::{
        ApiState, IDEMPOTENCY_HEADER, JetRateLimitKeyExtractor, JobQueue, get_job, submit_job,
    };
    use crate::queue::{JobStateRecord, QueuedJob, job_state_key};
    use jet_pack::{VersionResolver, manifest::RuntimeManifest};

    type TestHmacSha256 = Hmac<Sha256>;

    fn sign_user_header_v2(
        secret: &[u8],
        key_id: &str,
        user_id: &str,
        timestamp: i64,
        nonce: &str,
        method: &str,
        path: &str,
        query: Option<&str>,
        body: &[u8],
        content_type: &str,
    ) -> String {
        let body_hash = super::sha256_hex(body);
        let canonical = super::canonical_request_v2(
            &axum::http::Method::from_bytes(method.as_bytes()).expect("valid method"),
            path,
            query,
            &body_hash,
            content_type,
            user_id,
            &timestamp.to_string(),
            nonce,
            key_id,
        );
        let mut mac = TestHmacSha256::new_from_slice(secret).expect("valid hmac key");
        mac.update(canonical.as_bytes());
        hex::encode(mac.finalize().into_bytes())
    }

    #[derive(Default)]
    struct MockQueue {
        jobs: Mutex<Vec<QueuedJob>>,
        errors: Mutex<VecDeque<String>>,
        attempted_job_ids: Mutex<Vec<String>>,
    }

    impl MockQueue {
        async fn push_error(&self, error: impl Into<String>) {
            self.errors.lock().await.push_back(error.into());
        }

        async fn len(&self) -> usize {
            self.jobs.lock().await.len()
        }

        async fn attempted_job_ids(&self) -> Vec<String> {
            self.attempted_job_ids.lock().await.clone()
        }
    }

    impl JobQueue for MockQueue {
        fn push_job<'a>(
            &'a self,
            job: QueuedJob,
        ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>> {
            Box::pin(async move {
                self.attempted_job_ids.lock().await.push(job.id.clone());

                if let Some(err) = self.errors.lock().await.pop_front() {
                    return Err(err);
                }

                self.jobs.lock().await.push(job);
                Ok(())
            })
        }
    }

    async fn start_test_redis() -> (String, oneshot::Sender<()>) {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind test redis listener");
        let addr = listener.local_addr().expect("local addr");
        let redis_url = format!("redis://{}/", addr);

        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
        tokio::spawn(async move {
            let _ = server::run(listener, async move {
                let _ = shutdown_rx.await;
            })
            .await;
        });

        (redis_url, shutdown_tx)
    }

    fn test_manifest(language: &str, version: &str) -> RuntimeManifest {
        RuntimeManifest {
            language: language.to_string(),
            version: version.to_string(),
            aliases: vec!["latest".to_string()],
            runtimes: HashMap::from([(
                "x86_64".to_string(),
                RuntimeArchive {
                    url: "file:///tmp/runtime.tar.gz".to_string(),
                    sha256: None,
                },
            )]),
            execute: ExecutionTemplate {
                command: "python3".to_string(),
                args: Some(vec!["main.py".to_string()]),
                jvm_flags: None,
            },
            compile: None,
            starter_code: None,
        }
    }

    async fn setup_state() -> (ApiState, Arc<MockQueue>, oneshot::Sender<()>) {
        let (redis_url, shutdown_tx) = start_test_redis().await;

        let manifest = test_manifest("python", "3.14.3");
        let mut resolver = VersionResolver::new(InMemoryVersionStore::default());
        resolver
            .initialize_from_manifests(std::slice::from_ref(&manifest))
            .expect("resolver init");

        // Create a deadpool-redis connection pool for tests.
        let pool_cfg = PoolConfig::from_url(&redis_url);
        let pool = pool_cfg
            .create_pool(Some(PoolRuntime::Tokio1))
            .expect("pool creation");

        let queue = Arc::new(MockQueue::default());

        let state = ApiState {
            resolver: Arc::new(resolver),
            manifests: Arc::new(HashMap::from([(
                format!("{}:{}", manifest.language, manifest.version),
                manifest,
            )])),
            storage: queue.clone(),
            redis_pool: pool,
            job_state_prefix: "jet:test:jobs".to_string(),
            start_time: Instant::now(),
            jobs_submitted: Arc::new(AtomicU64::new(0)),
            jobs_completed: Arc::new(AtomicU64::new(0)),
            jobs_failed: Arc::new(AtomicU64::new(0)),
            worker_concurrency: 1,
            jobs_in_flight: Arc::new(AtomicU64::new(0)),
            max_queue_depth: 100,
            compile_in_flight: Arc::new(AtomicU64::new(0)),
            execute_in_flight: Arc::new(AtomicU64::new(0)),
            compile_concurrency: 1,
            execute_concurrency: 1,
            max_queue_wait_secs: 30,
            rate_limit_hmac_key_id: Some("test-kid".to_string()),
            rate_limit_hmac_secret: None,
            rate_limit_timestamp_tolerance_secs: 300,
            rate_limit_nonce_ttl_secs: 300,
            strict_rate_limit_token_interval_secs: 1,
            strict_rate_limit_burst: 3,
            general_rate_limit_token_interval_ms: 200,
            general_rate_limit_burst: 10,
            poll_rate_limit_token_interval_ms: 50,
            poll_rate_limit_burst: 60,
        };

        (state, queue, shutdown_tx)
    }

    #[tokio::test]
    async fn get_job_returns_saved_state() {
        let (state, _, shutdown_tx) = setup_state().await;

        let record = JobStateRecord {
            job_id: "job-1".to_string(),
            status: "completed".to_string(),
            language: "python".to_string(),
            version: "3.14.3".to_string(),
            result: None,
            error: None,
            terminal_reason: Some("success".to_string()),
            queue_wait_ms: None,
        };
        let payload = serde_json::to_string(&record).expect("serialize record");
        let key = job_state_key(&state.job_state_prefix, &record.job_id);
        {
            let mut conn = state.redis_pool.get().await.expect("pool conn");
            let _: () = conn
                .set::<_, _, ()>(&key, &payload)
                .await
                .expect("seed redis");
        }

        let (status, body) = get_job(Path("job-1".to_string()), State(state.clone()))
            .await
            .expect("get job should succeed");

        assert_eq!(status, StatusCode::OK);
        assert_eq!(body.job_id, "job-1");
        assert_eq!(body.status, "completed");

        let _ = shutdown_tx.send(());
    }

    #[test]
    fn extractor_prefers_valid_signed_user_id_key() {
        let secret: Arc<[u8]> = Arc::from(b"test-secret".to_vec().into_boxed_slice());
        let extractor = JetRateLimitKeyExtractor {
            key_id: Some("kid-1".to_string()),
            shared_secret: Some(secret.clone()),
            max_skew_secs: 300,
        };

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("valid clock")
            .as_secs() as i64;
        let nonce = "nonce-123";
        let method = "GET";
        let path = "/jobs/job-1";
        let query = Some("a=2&a=1&b=%20");
        let body = b"";
        let content_type = "";
        let sig = sign_user_header_v2(
            secret.as_ref(),
            "kid-1",
            "user-123",
            now,
            nonce,
            method,
            path,
            query,
            body,
            content_type,
        );

        let req = Request::builder()
            .method(method)
            .uri("/jobs/job-1?b=%20&a=1&a=2")
            .header("x-jet-auth-version", "2")
            .header("x-jet-key-id", "kid-1")
            .header("x-jet-user-id", "user-123")
            .header("x-jet-timestamp", now.to_string())
            .header("x-jet-nonce", nonce)
            .header("x-jet-content-sha256", super::sha256_hex(body))
            .header("x-jet-signature", sig)
            .body(())
            .expect("request build");

        let key = extractor.extract(&req).expect("key extraction should work");
        assert_eq!(key, "user:user-123");
    }

    #[test]
    fn extractor_uses_unauthenticated_bucket_when_signature_invalid() {
        let extractor = JetRateLimitKeyExtractor {
            key_id: Some("kid-1".to_string()),
            shared_secret: Some(Arc::from(b"test-secret".to_vec().into_boxed_slice())),
            max_skew_secs: 300,
        };

        let req = Request::builder()
            .method("GET")
            .uri("/jobs/job-1")
            .header("x-jet-auth-version", "2")
            .header("x-jet-key-id", "kid-1")
            .header("x-jet-user-id", "user-123")
            .header("x-jet-timestamp", "9999999999")
            .header("x-jet-nonce", "nonce-123")
            .header("x-jet-content-sha256", super::sha256_hex(b""))
            .header("x-jet-signature", "deadbeef")
            .body(())
            .expect("request build");

        let key = extractor
            .extract(&req)
            .expect("unauthenticated fallback should work");
        assert_eq!(key, "user:unauthenticated");
    }

    #[tokio::test]
    async fn get_job_returns_not_found_for_unknown_id() {
        let (state, _, shutdown_tx) = setup_state().await;

        let err = get_job(Path("missing-job".to_string()), State(state))
            .await
            .expect_err("missing job should fail");

        assert_eq!(err.0, StatusCode::NOT_FOUND);

        let _ = shutdown_tx.send(());
    }

    #[tokio::test]
    async fn submit_job_ignores_client_supplied_job_id() {
        let (state, queue, shutdown_tx) = setup_state().await;

        let request = JobRequest {
            job_id: Some("../../escape".to_string()),
            language: "python".to_string(),
            version: Some("3.14".to_string()),
            files: vec![FileRequest {
                name: Some("main.py".to_string()),
                content: "print('ok')".to_string(),
                encoding: None,
            }],
            testcases: None,
            args: None,
            stdin: None,
            run_timeout: None,
            compile_timeout: None,
            run_memory_limit: None,
            compile_memory_limit: None,
            run_output_limit: None,
            compile_output_limit: None,
        };

        let (status, response) =
            submit_job(State(state.clone()), HeaderMap::new(), axum::Json(request))
                .await
                .expect("submit should succeed");

        assert_eq!(status, StatusCode::ACCEPTED);
        assert_ne!(response.job_id, "../../escape");
        assert_eq!(queue.len().await, 1);

        let queued_raw: Option<String> = state
            .redis_pool
            .get()
            .await
            .expect("pool conn")
            .get(job_state_key(&state.job_state_prefix, &response.job_id))
            .await
            .expect("state lookup");
        assert!(queued_raw.is_some());

        let _ = shutdown_tx.send(());
    }

    #[tokio::test]
    async fn submit_job_releases_slot_and_cleans_state_when_enqueue_fails() {
        let (state, queue, shutdown_tx) = setup_state().await;
        queue.push_error("queue unavailable").await;

        let request = JobRequest {
            job_id: None,
            language: "python".to_string(),
            version: Some("3.14".to_string()),
            files: vec![FileRequest {
                name: Some("main.py".to_string()),
                content: "print('ok')".to_string(),
                encoding: None,
            }],
            testcases: None,
            args: None,
            stdin: None,
            run_timeout: None,
            compile_timeout: None,
            run_memory_limit: None,
            compile_memory_limit: None,
            run_output_limit: None,
            compile_output_limit: None,
        };

        let err = submit_job(State(state.clone()), HeaderMap::new(), axum::Json(request))
            .await
            .expect_err("submit should fail");

        assert_eq!(err.0, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(
            state
                .jobs_in_flight
                .load(std::sync::atomic::Ordering::Relaxed),
            0
        );
        assert_eq!(queue.len().await, 0);

        let attempted_job_ids = queue.attempted_job_ids().await;
        assert_eq!(attempted_job_ids.len(), 1);

        let mut conn = state.redis_pool.get().await.expect("pool conn");
        let stored: Option<String> = conn
            .get(job_state_key(
                &state.job_state_prefix,
                &attempted_job_ids[0],
            ))
            .await
            .expect("state lookup");
        let stored = stored.expect("failed enqueue state should remain queryable");
        let stored: JobStateRecord = serde_json::from_str(&stored).expect("decode state");
        assert_eq!(stored.status, "failed");
        assert!(
            stored
                .error
                .expect("failed state should include error")
                .contains("job was not enqueued")
        );

        let _ = shutdown_tx.send(());
    }

    #[tokio::test]
    async fn submit_job_reuses_existing_job_for_same_idempotency_key() {
        let (state, queue, shutdown_tx) = setup_state().await;

        let request = JobRequest {
            job_id: None,
            language: "python".to_string(),
            version: Some("3.14".to_string()),
            files: vec![FileRequest {
                name: Some("main.py".to_string()),
                content: "print('ok')".to_string(),
                encoding: None,
            }],
            testcases: None,
            args: None,
            stdin: None,
            run_timeout: None,
            compile_timeout: None,
            run_memory_limit: None,
            compile_memory_limit: None,
            run_output_limit: None,
            compile_output_limit: None,
        };

        let mut headers = HeaderMap::new();
        headers.insert(IDEMPOTENCY_HEADER, HeaderValue::from_static("retry_key_1"));

        let (_, first) = submit_job(
            State(state.clone()),
            headers.clone(),
            axum::Json(request.clone()),
        )
        .await
        .expect("first submit should succeed");

        let (_, second) = submit_job(State(state.clone()), headers, axum::Json(request))
            .await
            .expect("second submit should reuse existing job");

        assert_eq!(first.job_id, second.job_id);
        assert_eq!(queue.len().await, 1);

        let _ = shutdown_tx.send(());
    }

    #[tokio::test]
    async fn submit_job_rejects_idempotency_key_reuse_with_different_request() {
        let (state, _queue, shutdown_tx) = setup_state().await;

        let first_request = JobRequest {
            job_id: None,
            language: "python".to_string(),
            version: Some("3.14".to_string()),
            files: vec![FileRequest {
                name: Some("main.py".to_string()),
                content: "print('ok')".to_string(),
                encoding: None,
            }],
            testcases: None,
            args: None,
            stdin: None,
            run_timeout: None,
            compile_timeout: None,
            run_memory_limit: None,
            compile_memory_limit: None,
            run_output_limit: None,
            compile_output_limit: None,
        };

        let second_request = JobRequest {
            files: vec![FileRequest {
                name: Some("main.py".to_string()),
                content: "print('different')".to_string(),
                encoding: None,
            }],
            ..first_request.clone()
        };

        let mut headers = HeaderMap::new();
        headers.insert(IDEMPOTENCY_HEADER, HeaderValue::from_static("retry_key_2"));

        let _ = submit_job(
            State(state.clone()),
            headers.clone(),
            axum::Json(first_request),
        )
        .await
        .expect("first submit should succeed");

        let err = submit_job(State(state), headers, axum::Json(second_request))
            .await
            .expect_err("second submit should conflict");

        assert_eq!(err.0, StatusCode::CONFLICT);

        let _ = shutdown_tx.send(());
    }
}
