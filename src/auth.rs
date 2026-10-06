//! Credential-based auth against the Adventures mobile API.
//!
//! Reversed from `com.groundspeak.react.adventures` v1.71.0 (Hermes bundle):
//!   POST /v1/public/accounts/login               {username, password}
//!     -> {accessToken, expiresIn, refreshToken, user}
//!   POST /v1/public/accounts/refreshaccesstoken  {refreshToken}
//!     -> {accessToken, expiresIn, ...}
//!   GET  /v1/public/accounts/me  (bearer)
//!     -> profile incl. publicGuid — the md5 answer-hash salt.
//!
//! The whole login is plain JSON-over-HTTPS — no web OAuth flow is needed
//! for the Geocaching provider (that flow exists for account *linking*).
//! State lives in `meta` as JSON so a restarted process keeps its tokens.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::Mutex;
use tracing::{info, warn};

use crate::db::Db;

pub const ACCOUNTS_BASE: &str =
    "https://api.groundspeak.com/adventuresmobile/v1/public/accounts";
const META_KEY: &str = "auth";
/// Refresh this many seconds before the stated expiry.
const EXPIRY_SKEW_SECS: i64 = 120;
const AUTH_TIMEOUT: Duration = Duration::from_secs(60);

/// Persisted credential + token state.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AuthState {
    pub username: String,
    /// Kept so a dead refresh token can be re-exchanged without
    /// prompting. Lives only in the local db.
    pub password: String,
    pub access_token: String,
    pub refresh_token: String,
    /// Unix seconds when the access token stops being valid.
    pub expires_at: i64,
    /// The account's publicGuid — salt for md5 answer cracking.
    #[serde(default)]
    pub public_guid: Option<String>,
}

impl AuthState {
    pub fn is_fresh(&self) -> bool {
        now_secs() < self.expires_at - EXPIRY_SKEW_SECS
    }
}

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

fn parse_token_body(v: &Value) -> Result<(String, String, i64)> {
    let access = v
        .get("accessToken")
        .and_then(|t| t.as_str())
        .or_else(|| v.get("access_token").and_then(|t| t.as_str()))
        .context("login response missing accessToken")?;
    let refresh = v
        .get("refreshToken")
        .and_then(|t| t.as_str())
        .unwrap_or("");
    let expires_in = v
        .get("expiresIn")
        .or_else(|| v.get("expires_in"))
        .and_then(|e| e.as_i64())
        .unwrap_or(3600);
    Ok((
        access.to_string(),
        refresh.to_string(),
        now_secs() + expires_in,
    ))
}

/// username+password -> fresh token set. 403 = bad credentials.
pub async fn do_login(
    http: &reqwest::Client,
    username: &str,
    password: &str,
) -> Result<AuthState> {
    let resp = http
        .post(format!("{ACCOUNTS_BASE}/login"))
        .json(&serde_json::json!({ "username": username, "password": password }))
        .send()
        .await?;
    let status = resp.status();
    let body: Value = resp.json().await.unwrap_or(Value::Null);
    if !status.is_success() {
        let msg = body
            .get("errorMessage")
            .and_then(|m| m.as_str())
            .unwrap_or("login failed");
        return Err(anyhow!("login http {}: {msg}", status.as_u16()));
    }
    let (access, refresh, expires_at) = parse_token_body(&body)?;
    if refresh.is_empty() {
        warn!("login response had no refreshToken — token will not be renewable");
    }
    let mut st = AuthState {
        username: username.to_string(),
        password: password.to_string(),
        access_token: access,
        refresh_token: refresh,
        expires_at,
        public_guid: None,
    };
    st.public_guid = fetch_public_guid(http, &st.access_token).await;
    Ok(st)
}

/// Exchange refresh_token for a new access token. Falls back to a full
/// re-login when the refresh token itself is rejected.
async fn renew(http: &reqwest::Client, st: &mut AuthState) -> Result<()> {
    if !st.refresh_token.is_empty() {
        let resp = http
            .post(format!("{ACCOUNTS_BASE}/refreshaccesstoken"))
            .json(&serde_json::json!({ "refreshToken": st.refresh_token }))
            .send()
            .await;
        if let Ok(r) = resp {
            let status = r.status();
            let body: Value = r.json().await.unwrap_or(Value::Null);
            if status.is_success() {
                if let Ok((access, refresh, expires_at)) = parse_token_body(&body) {
                    st.access_token = access;
                    if !refresh.is_empty() {
                        st.refresh_token = refresh;
                    }
                    st.expires_at = expires_at;
                    info!("access token refreshed (expires in {}s)", st.expires_at - now_secs());
                    return Ok(());
                }
            }
            warn!("refresh token rejected (http {}), re-logging in", status.as_u16());
        }
    }
    let fresh = do_login(http, &st.username, &st.password).await?;
    *st = fresh;
    info!("re-logged in as {}", st.username);
    Ok(())
}

/// GET /me — grab the account's publicGuid (answer-hash salt).
pub async fn fetch_public_guid(http: &reqwest::Client, token: &str) -> Option<String> {
    let resp = http
        .get(format!("{ACCOUNTS_BASE}/me"))
        .bearer_auth(token)
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let v: Value = resp.json().await.ok()?;
    for key in ["publicGuid", "publicGUID", "referenceCode", "guid", "id"] {
        if let Some(g) = v.get(key).and_then(|g| g.as_str()) {
            return Some(g.to_string());
        }
    }
    None
}

/// Shared, auto-renewing auth context. `token()` is the only public
/// surface: it returns a valid bearer string, renewing as needed.
pub struct Auth {
    http: reqwest::Client,
    db: Db,
    state: Mutex<AuthState>,
}

impl Auth {
    pub fn new(db: Db, state: AuthState, headers: reqwest::header::HeaderMap) -> Result<Self> {
        let http = reqwest::Client::builder()
            .user_agent(crate::api::USER_AGENT)
            .default_headers(headers)
            .timeout(AUTH_TIMEOUT)
            .build()?;
        Ok(Self {
            http,
            db,
            state: Mutex::new(state),
        })
    }

    /// Load persisted auth from `meta` (None when never logged in).
    pub async fn load(db: &Db, headers: reqwest::header::HeaderMap) -> Result<Option<Self>> {
        let Some(raw) = db.get_meta(META_KEY).await? else {
            return Ok(None);
        };
        let state: AuthState = serde_json::from_str(&raw).context("bad auth blob in meta")?;
        Ok(Some(Self::new(db.clone(), state, headers)?))
    }

    pub async fn username(&self) -> String {
        self.state.lock().await.username.clone()
    }

    pub async fn public_guid(&self) -> Option<String> {
        self.state.lock().await.public_guid.clone()
    }

    /// A valid bearer token; refreshes / re-logs-in under the mutex so
    /// concurrent callers single-flight. Persists rotated tokens.
    pub async fn token(&self) -> Result<String> {
        let mut st = self.state.lock().await;
        if st.is_fresh() {
            return Ok(st.access_token.clone());
        }
        renew(&self.http, &mut st).await?;
        let raw = serde_json::to_string(&*st)?;
        self.db.set_meta(META_KEY, raw).await?;
        Ok(st.access_token.clone())
    }
}

/// Bare HTTP client for one-shot auth calls (login/status from the CLI).
pub fn bare_client(headers: reqwest::header::HeaderMap) -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .user_agent(crate::api::USER_AGENT)
        .default_headers(headers)
        .timeout(AUTH_TIMEOUT)
        .build()?)
}
