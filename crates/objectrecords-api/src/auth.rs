//! Phase 4.D Step 3 — Auth0 JWT verify middleware (POC).
//!
//! Phase 4.D Step 3
//! ("OR backend に Auth0 JWT verify middleware POC、 valid / invalid /
//! missing token の 3 case test green").
//!
//! Identity SSOT: Option IV — Auth0
//! the Auth0 tenant emits multi-audience tokens (`aud` array contains
//! `https://example.com/api` and OR's audience). This verifier
//! checks that OR's audience is present, regardless of additional
//! audiences in the array.
//!
//! Phase scope:
//! - Phase 4.D Step 3 (this file): RS256 + JwkSet verification, applied
//!   to `GET /records/:id` only. dev stage runs no-auth (the verifier
//!   is `None` on [`crate::AppState`] and the middleware short-circuits).
//! - Phase 4.E Step 5: same middleware extended to every endpoint, plus
//!   401 (auth) vs 403 (provisioning) error split.
//! - Phase 4.E Step 6 / 4 投資: the
//!   [`Claims`] struct already accepts an optional `act` claim so a
//!   future RFC 8693 token-exchange path lands without breaking the
//!   verify schema. `sub` validation is intentionally `client_id`-free
//!   so MCP delegated tokens and end-user tokens flow through the same
//!   gate.

use std::sync::Arc;

use axum::extract::{Request, State};
use axum::http::HeaderName;
use axum::http::header::AUTHORIZATION;
use axum::middleware::Next;
use axum::response::Response;
use jsonwebtoken::jwk::JwkSet;
use jsonwebtoken::{Algorithm, DecodingKey, Validation, decode, decode_header};
use serde::Deserialize;
use thiserror::Error;
use tracing::{info, warn};

/// `X-MCP-Client-Id` — opt-in client identification header for requests
/// proxied through a MCP server. Phase 4.E Step 6 / 4 投資 #2
/// — the API does not gate auth on this
/// header, but extracts it for audit-trail logging so that a future
/// RFC 8693 token-exchange migration has a coarse "this came via MCP"
/// signal without changing the verify schema.
pub const X_MCP_CLIENT_ID: HeaderName = HeaderName::from_static("x-mcp-client-id");

use crate::AppState;
use crate::error::ApiError;

/// Decoded claims from an Auth0-issued JWT.
///
/// `aud` is captured as a raw [`serde_json::Value`] because Auth0 emits
/// **either** a string **or** an array depending on the audience config —
/// the verification logic in [`Auth0Verifier::verify`] hands off the
/// audience check to [`jsonwebtoken`] which handles both shapes.
#[derive(Debug, Clone, Deserialize)]
pub struct Claims {
    /// Subject — Auth0 `sub` claim, i.e. the Auth0 user id (e.g. `auth0|abc123`).
    /// This is the identity SSOT per Option IV:
    /// OR's `or_user.auth0_sub` joins on this value.
    pub sub: String,
    /// Audience claim. Either a single string or an array of strings;
    /// validation that OR's audience is present is performed by
    /// [`jsonwebtoken::Validation::set_audience`].
    pub aud: serde_json::Value,
    /// Issuer — the Auth0 tenant URL (e.g. `https://example.auth0.com/`).
    pub iss: String,
    /// Expiry, seconds since UNIX epoch. Validated by jsonwebtoken.
    pub exp: i64,
    /// Issued-at, seconds since UNIX epoch. Optional in some Auth0 configs.
    pub iat: Option<i64>,
    /// RFC 8693 actor claim (token-exchange / on-behalf-of). Phase 4.E
    /// Step 6 prep: the schema accepts this claim being present without
    /// extra logic, so MCP delegated-vs-OBO can flip later without
    /// breaking verify.
    #[serde(default)]
    pub act: Option<serde_json::Value>,
    /// OAuth2 `scope` claim — Auth0 emits this as a single
    /// space-separated string (e.g. `"objectrecords:read
    /// objectrecords:write"`). Empty by default so the schema parses
    /// tokens that pre-date Phase 4.F scope gating without breaking.
    /// Use [`Claims::has_scope`] to query membership rather than
    /// splitting at the call site.
    #[serde(default)]
    pub scope: String,
}

impl Claims {
    /// Returns true iff `wanted` is one of the space-separated tokens
    /// in [`Self::scope`]. Phase 4.F endpoints use this to gate
    /// reads (`"objectrecords:read"`) vs writes
    /// (`"objectrecords:write"`).
    #[must_use]
    pub fn has_scope(&self, wanted: &str) -> bool {
        self.scope.split_whitespace().any(|s| s == wanted)
    }
}

/// Failure modes for token verification. Mapped to HTTP 401 at the
/// middleware boundary (401 == auth
/// invalid, distinct from 403 == user-record-missing which arrives in
/// Phase 4.E Step 5 via the first-time provisioning flow).
#[derive(Debug, Error)]
pub enum AuthError {
    /// `Authorization` header was not present on the request.
    #[error("missing Authorization header")]
    Missing,
    /// `Authorization` header was present but didn't start with `Bearer `.
    #[error("malformed Authorization header")]
    Malformed,
    /// JWT header decoded but carried no `kid`; Auth0 always sets one,
    /// so this is almost certainly a forged token.
    #[error("token header missing kid")]
    NoKid,
    /// JWT `kid` did not match any key in the configured JWKS. Either
    /// the JWKS is stale (Auth0 rotated keys — Phase 4.E adds refresh)
    /// or the token came from a different tenant.
    #[error("kid not found in JWKS")]
    UnknownKid,
    /// Any other validation failure from jsonwebtoken (bad signature,
    /// wrong `aud`, wrong `iss`, expired, malformed).
    #[error("invalid token: {0}")]
    InvalidToken(#[from] jsonwebtoken::errors::Error),
}

/// JWT verifier configured for a specific Auth0 tenant + audience pair.
///
/// Constructed once at server start (Phase 4.E will add a background
/// JWKS refresh task; Phase 4.D POC uses a static JWKS supplied at
/// construction).
#[derive(Debug)]
pub struct Auth0Verifier {
    jwks: JwkSet,
    audience: String,
    issuer: String,
}

impl Auth0Verifier {
    /// Build a verifier from a JWKS JSON document (typically fetched
    /// once from `https://<tenant>/.well-known/jwks.json`).
    ///
    /// # Errors
    /// Returns [`serde_json::Error`] if the JWKS payload doesn't parse
    /// into a [`JwkSet`].
    pub fn from_jwks_json(
        jwks_json: &str,
        audience: impl Into<String>,
        issuer: impl Into<String>,
    ) -> Result<Self, serde_json::Error> {
        let jwks: JwkSet = serde_json::from_str(jwks_json)?;
        Ok(Self {
            jwks,
            audience: audience.into(),
            issuer: issuer.into(),
        })
    }

    /// Verify a bearer token. Returns the decoded [`Claims`] on success.
    ///
    /// Algorithm is fixed to RS256 — Auth0 default for application APIs.
    /// HS256 / ES256 support is intentionally out of scope; the verifier
    /// is purpose-built for Auth0 tenant.
    ///
    /// # Errors
    /// See [`AuthError`].
    pub fn verify(&self, token: &str) -> Result<Claims, AuthError> {
        let header = decode_header(token)?;
        let kid = header.kid.ok_or(AuthError::NoKid)?;
        let jwk = self.jwks.find(&kid).ok_or(AuthError::UnknownKid)?;
        let key = DecodingKey::from_jwk(jwk)?;
        let mut validation = Validation::new(Algorithm::RS256);
        validation.set_audience(&[&self.audience]);
        validation.set_issuer(&[&self.issuer]);
        let data = decode::<Claims>(token, &key, &validation)?;
        Ok(data.claims)
    }
}

/// Axum middleware that enforces JWT presence + validity on routes it
/// wraps.
///
/// Behaviour matrix:
///
/// | `state.auth` | header        | outcome             |
/// |--------------|---------------|---------------------|
/// | `None`       | (any)         | pass through        |
/// | `Some(v)`    | absent        | 401 `Unauthorized`  |
/// | `Some(v)`    | invalid token | 401 `Unauthorized`  |
/// | `Some(v)`    | valid token   | pass through + attach `Claims` to `req.extensions_mut` |
///
/// Phase 4.D dev stage runs with `state.auth = None` (no-auth public
/// per internal design notes); Phase 4.E live promotion sets
/// `state.auth = Some(_)` to enforce.
pub async fn verify_jwt_middleware(
    State(state): State<AppState>,
    req: Request,
    next: Next,
) -> Result<Response, ApiError> {
    let Some(verifier) = state.auth.as_ref() else {
        return Ok(next.run(req).await);
    };

    let token = extract_bearer(&req).map_err(|e| {
        warn!(?e, "Authorization header rejected");
        ApiError::Unauthorized
    })?;

    let claims = verifier.verify(token).map_err(|e| {
        warn!(?e, "JWT verification failed");
        ApiError::Unauthorized
    })?;

    // Phase 4.E Step 5b — JIT provisioning. Look up or create the
    // or_user row for this Auth0 sub. The lookup is idempotent and the
    // create races on the auth0_sub UNIQUE index, so the call is safe
    // to make on every request (cost = 1 round-trip for the SELECT).
    let or_user = state.repo.upsert_or_user(&claims.sub).await.map_err(|e| {
        warn!(?e, sub = %claims.sub, "or_user upsert failed");
        ApiError::Internal
    })?;

    // Capture X-MCP-Client-Id for audit. Absence is normal (direct
    // user-agent calls); presence flags MCP-proxied requests.
    let mcp_client_id = req
        .headers()
        .get(&X_MCP_CLIENT_ID)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    info!(
        sub = %claims.sub,
        or_user_id = %or_user.id,
        mcp_client_id = ?mcp_client_id,
        has_act = claims.act.is_some(),
        "auth: token verified",
    );

    // Make claims, the resolved or_user, and the optional MCP client id
    // available to downstream handlers via Extension.
    let mut req = req;
    req.extensions_mut().insert(Arc::new(claims));
    req.extensions_mut().insert(Arc::new(or_user));
    if let Some(id) = mcp_client_id {
        req.extensions_mut().insert(McpClientId(id));
    }

    Ok(next.run(req).await)
}

/// Wrapper type for the `X-MCP-Client-Id` value attached to request
/// extensions (Phase 4.E Step 6 / 4 投資 #2). Wrapping in a newtype
/// rather than using a bare `String` lets extension lookups disambiguate
/// from any other `String` carried in the request.
#[derive(Debug, Clone)]
pub struct McpClientId(pub String);

fn extract_bearer(req: &Request) -> Result<&str, AuthError> {
    let header = req
        .headers()
        .get(AUTHORIZATION)
        .ok_or(AuthError::Missing)?
        .to_str()
        .map_err(|_| AuthError::Malformed)?;
    header.strip_prefix("Bearer ").ok_or(AuthError::Malformed)
}

/// Required scope token for read endpoints (`GET /records/:id` etc).
pub const SCOPE_READ: &str = "objectrecords:read";
/// Required scope token for write endpoints
/// (`POST /records`, `PUT /records/:id`, `POST /records/:id/fix`,
/// `POST /assets`, …).
pub const SCOPE_WRITE: &str = "objectrecords:write";

/// Axum middleware that enforces a specific OAuth2 scope on routes
/// it wraps. Phase 4.F — Stage 3 scope gating.
///
/// Behaviour:
///
/// | `state.auth` | scope claim contains `scope` | outcome |
/// |--------------|------------------------------|---------|
/// | `None`       | (any)                        | pass through (no-auth scratch stage) |
/// | `Some(_)`    | yes                          | pass through |
/// | `Some(_)`    | no                           | `403 Forbidden { kind: "Forbidden" }` |
/// | `Some(_)`    | no `Claims` extension        | `500 Internal` (logged: middleware ordering bug) |
///
/// Layered **after** [`verify_jwt_middleware`] so the claims have
/// already been deposited into `req.extensions_mut()`. Layering
/// before would yield 500 because the `Claims` extension is absent.
pub async fn require_scope(
    State(state): State<AppState>,
    scope: &'static str,
    req: Request,
    next: Next,
) -> Result<Response, ApiError> {
    if state.auth.is_none() {
        // No-auth scratch path. Per internal design notes, the
        // scope gate has no opinion in this mode — the deploy already
        // chose `no enforcement`.
        return Ok(next.run(req).await);
    }
    let claims = req
        .extensions()
        .get::<Arc<Claims>>()
        .ok_or_else(|| {
            warn!(
                scope,
                "require_scope invoked without prior verify_jwt_middleware \
                 — fix middleware layering"
            );
            ApiError::Internal
        })?
        .clone();
    if claims.has_scope(scope) {
        Ok(next.run(req).await)
    } else {
        warn!(scope, sub = %claims.sub, "scope check failed");
        Err(ApiError::Forbidden(format!(
            "scope `{scope}` required"
        )))
    }
}

/// Convenience wrapper of [`require_scope`] pinned to
/// [`SCOPE_READ`]. Suitable as a route_layer fn for
/// `axum::middleware::from_fn_with_state`.
pub async fn require_scope_read(
    State(state): State<AppState>,
    req: Request,
    next: Next,
) -> Result<Response, ApiError> {
    require_scope(State(state), SCOPE_READ, req, next).await
}

/// Convenience wrapper of [`require_scope`] pinned to
/// [`SCOPE_WRITE`]. Suitable as a route_layer fn for
/// `axum::middleware::from_fn_with_state`.
pub async fn require_scope_write(
    State(state): State<AppState>,
    req: Request,
    next: Next,
) -> Result<Response, ApiError> {
    require_scope(State(state), SCOPE_WRITE, req, next).await
}

/// Env var holding the Auth0 API identifier — the value OR's tokens
/// must carry in their `aud` claim (`https://api.objectrecords.io`).
pub const ENV_AUTH0_AUDIENCE: &str = "OBJECTRECORDS_AUTH0_AUDIENCE";
/// Env var holding the Auth0 tenant issuer URL, trailing slash included
/// (`https://example.auth0.com/`). See [`ENV_AUTH0_AUDIENCE`].
pub const ENV_AUTH0_ISSUER: &str = "OBJECTRECORDS_AUTH0_ISSUER";
/// Env var holding the absolute JWKS document URL
/// (`https://example.auth0.com/.well-known/jwks.json`).
/// See [`ENV_AUTH0_AUDIENCE`].
pub const ENV_AUTH0_JWKS_URL: &str = "OBJECTRECORDS_AUTH0_JWKS_URL";

/// Auth enforcement mode resolved from the environment.
///
/// The three [`ENV_AUTH0_AUDIENCE`] / [`ENV_AUTH0_ISSUER`] /
/// [`ENV_AUTH0_JWKS_URL`] vars are an all-or-nothing switch: all set =
/// [`AuthEnv::Enabled`] (live stage), all absent = [`AuthEnv::Disabled`]
/// (dev stage public per internal design notes). A *partial* set
/// is a deploy mistake and yields [`AuthEnvError`] rather than silently
/// downgrading to no-auth — fail loud, never weaken security by default.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthEnv {
    /// No JWT enforcement. The verify middleware short-circuits.
    Disabled,
    /// JWT enforcement on, with the Auth0 coordinates to build an
    /// [`Auth0Verifier`].
    Enabled {
        /// API identifier registered in Auth0 (expected `aud` value).
        audience: String,
        /// Auth0 tenant issuer URL (trailing slash included).
        issuer: String,
        /// Absolute URL of the tenant JWKS document.
        jwks_url: String,
    },
}

/// A partial Auth0 configuration: some — but not all — of the three
/// Auth0 env vars were set.
#[derive(Debug, Error)]
#[error(
    "incomplete Auth0 config — set all of [{ENV_AUTH0_AUDIENCE}, \
     {ENV_AUTH0_ISSUER}, {ENV_AUTH0_JWKS_URL}] for live, or none for \
     dev; unset: {missing}"
)]
pub struct AuthEnvError {
    /// Comma-separated names of the env vars left unset.
    pub missing: String,
}

/// Resolves [`AuthEnv`] from a variable-lookup closure.
///
/// `get` is injected rather than reading [`std::env`] directly so the
/// all-or-nothing rule is unit-testable without mutating process-global
/// environment state. `get` should return `None` for both unset *and*
/// empty/whitespace values (the caller normalises).
///
/// # Errors
/// Returns [`AuthEnvError`] when the three Auth0 vars are partially set.
pub fn auth_env_from_vars(
    get: impl Fn(&str) -> Option<String>,
) -> Result<AuthEnv, AuthEnvError> {
    let audience = get(ENV_AUTH0_AUDIENCE);
    let issuer = get(ENV_AUTH0_ISSUER);
    let jwks_url = get(ENV_AUTH0_JWKS_URL);

    match (&audience, &issuer, &jwks_url) {
        (None, None, None) => Ok(AuthEnv::Disabled),
        (Some(audience), Some(issuer), Some(jwks_url)) => Ok(AuthEnv::Enabled {
            audience: audience.clone(),
            issuer: issuer.clone(),
            jwks_url: jwks_url.clone(),
        }),
        _ => {
            let missing = [
                (ENV_AUTH0_AUDIENCE, &audience),
                (ENV_AUTH0_ISSUER, &issuer),
                (ENV_AUTH0_JWKS_URL, &jwks_url),
            ]
            .into_iter()
            .filter_map(|(name, value)| value.is_none().then_some(name))
            .collect::<Vec<_>>()
            .join(", ");
            Err(AuthEnvError { missing })
        }
    }
}

#[cfg(test)]
mod auth_env_tests {
    use super::{
        AuthEnv, ENV_AUTH0_AUDIENCE, ENV_AUTH0_ISSUER, ENV_AUTH0_JWKS_URL, auth_env_from_vars,
    };

    /// Builds a `get` closure backed by a fixed `(name, value)` table.
    fn lookup<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |key| {
            pairs
                .iter()
                .find(|(name, _)| *name == key)
                .map(|(_, value)| (*value).to_owned())
        }
    }

    #[test]
    fn all_unset_resolves_to_disabled() {
        let env = auth_env_from_vars(lookup(&[])).expect("no vars is valid");
        assert_eq!(env, AuthEnv::Disabled);
    }

    #[test]
    fn all_set_resolves_to_enabled() {
        let env = auth_env_from_vars(lookup(&[
            (ENV_AUTH0_AUDIENCE, "https://api.objectrecords.io"),
            (ENV_AUTH0_ISSUER, "https://example.auth0.com/"),
            (
                ENV_AUTH0_JWKS_URL,
                "https://example.auth0.com/.well-known/jwks.json",
            ),
        ]))
        .expect("all three vars is valid");
        assert_eq!(
            env,
            AuthEnv::Enabled {
                audience: "https://api.objectrecords.io".to_owned(),
                issuer: "https://example.auth0.com/".to_owned(),
                jwks_url: "https://example.auth0.com/.well-known/jwks.json".to_owned(),
            }
        );
    }

    #[test]
    fn partial_config_is_rejected_loudly() {
        let err = auth_env_from_vars(lookup(&[(
            ENV_AUTH0_AUDIENCE,
            "https://api.objectrecords.io",
        )]))
        .expect_err("audience-only is a misconfiguration");
        // The audience that *was* set is not reported as missing.
        assert!(!err.missing.contains(ENV_AUTH0_AUDIENCE));
        // The two that were left unset are both named.
        assert!(err.missing.contains(ENV_AUTH0_ISSUER));
        assert!(err.missing.contains(ENV_AUTH0_JWKS_URL));
    }
}

#[cfg(test)]
mod claims_scope_tests {
    //! `Claims::has_scope` unit tests (Phase 4.F / Stage 3).
    //!
    //! The behavioural end-to-end test for `require_scope` (S1-S3) lives
    //! in the Medium tier integration suite (`tests/auth_smoke.rs`)
    //! because constructing a realistic `Request<Body>` with the
    //! `Arc<Claims>` extension pre-loaded mirrors real production
    //! invocation only inside axum's test machinery. These unit tests
    //! cover the pure predicate.

    use super::Claims;

    fn claims_with_scope(s: &str) -> Claims {
        Claims {
            sub: "auth0|test".to_string(),
            aud: serde_json::Value::String("https://api.objectrecords.io".to_string()),
            iss: "https://example.auth0.com/".to_string(),
            exp: 0,
            iat: None,
            act: None,
            scope: s.to_string(),
        }
    }

    #[test]
    fn has_scope_finds_exact_match() {
        let c = claims_with_scope("objectrecords:read objectrecords:write");
        assert!(c.has_scope("objectrecords:read"));
        assert!(c.has_scope("objectrecords:write"));
    }

    #[test]
    fn has_scope_missing_returns_false() {
        let c = claims_with_scope("objectrecords:read");
        assert!(!c.has_scope("objectrecords:write"));
        assert!(!c.has_scope("admin"));
    }

    #[test]
    fn has_scope_empty_returns_false() {
        let c = claims_with_scope("");
        assert!(!c.has_scope("objectrecords:read"));
        assert!(!c.has_scope(""));
    }

    #[test]
    fn has_scope_treats_substring_as_no_match() {
        // "objectrecords:rea" is a prefix of "objectrecords:read" but
        // must not match — naive `contains()` would be wrong.
        let c = claims_with_scope("objectrecords:read");
        assert!(!c.has_scope("objectrecords:rea"));
        assert!(!c.has_scope("objectrecords:read objectrecords:admin"));
    }

    #[test]
    fn has_scope_ignores_extra_whitespace() {
        // Whitespace separators per RFC 6749 — any consecutive whitespace
        // counts as a delimiter.
        let c = claims_with_scope("  objectrecords:read   objectrecords:write  ");
        assert!(c.has_scope("objectrecords:read"));
        assert!(c.has_scope("objectrecords:write"));
        assert!(!c.has_scope(""));
    }

    #[test]
    fn scope_field_deserializes_with_default_empty() {
        // Tokens that pre-date Phase 4.F scope gating omit the field;
        // serde(default) ensures `scope == ""` rather than failing parse.
        let json = r#"{
            "sub": "auth0|legacy",
            "aud": "https://api.objectrecords.io",
            "iss": "https://example.auth0.com/",
            "exp": 0
        }"#;
        let c: Claims = serde_json::from_str(json).expect("missing scope must parse");
        assert!(c.scope.is_empty());
        assert!(!c.has_scope("objectrecords:read"));
    }

    #[test]
    fn scope_field_deserializes_present() {
        let json = r#"{
            "sub": "auth0|user",
            "aud": "https://api.objectrecords.io",
            "iss": "https://example.auth0.com/",
            "exp": 0,
            "scope": "objectrecords:read objectrecords:write"
        }"#;
        let c: Claims = serde_json::from_str(json).expect("scope field must parse");
        assert!(c.has_scope("objectrecords:read"));
        assert!(c.has_scope("objectrecords:write"));
    }
}
