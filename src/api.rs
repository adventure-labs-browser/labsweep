//! Groundspeak Adventures mobile API client.
//!
//! One client is shared by all workers; a global fixed-interval limiter
//! (leaky bucket) caps the *aggregate* request rate — an upgrade over the
//! old per-worker `sleep(delay)` which scaled with concurrency.

use std::fmt;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use reqwest::header::{HeaderMap, HeaderValue};
use serde_json::{json, Value};
use tracing::warn;

use crate::auth::Auth;
use crate::geo::Cell;

pub const SEARCH_ENDPOINT: &str =
    "https://api.groundspeak.com/adventuresmobile/v1/public/adventures/search";
pub const DETAIL_ENDPOINT: &str =
    "https://api.groundspeak.com/adventuresmobile/v1/public/adventures";
/// Consumer key + UA lifted from the Android app
/// (com.groundspeak.react.adventures v1.71.0 build 5197, prod config).
/// The old iOS key also worked, but a consistent app fingerprint is cleaner.
pub const CONSUMER_KEY: &str = "A01A9CA1-29E0-46BD-A270-9D894A527B91";
pub const USER_AGENT: &str = "Adventures/1.71.0 (5197) (android/36)";

/// Server hard cap on `Take`.
pub const TAKE: usize = 500;
/// Empirical cliff: `Skip` past ~9500 returns 0 items + totalCount=None.
pub const MAX_SKIP: usize = 9500;
/// A cell with totalCount above this cannot be fully paginated.
pub const PAGINATION_LIMIT: u64 = 10_000;

/// RadiusInMeters has no cap — one giant query returns the API's
/// claimed global total. (It's `Skip` that cliffs at ~9500.)
pub const GLOBAL_RADIUS_M: f64 = 40_000_000.0;

const RETRIES: u32 = 6;
const BACKOFF: f64 = 1.7;
const TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Debug)]
pub enum ApiError {
    /// Permanent HTTP failure (non-retriable status).
    Status(u16, String),
    /// All retry attempts exhausted.
    Retries(String),
}

impl fmt::Display for ApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ApiError::Status(c, s) => write!(f, "http {c}: {s}"),
            ApiError::Retries(m) => write!(f, "max retries exceeded: {m}"),
        }
    }
}

impl std::error::Error for ApiError {}

pub struct SearchResp {
    pub total_count: u64,
    pub items: Vec<Value>,
}

pub struct ReviewsResp {
    pub total_count: u64,
    pub items: Vec<Value>,
    pub error: Option<String>,
}

/// Detail fetch never panics/raises: errors come back as data so the
/// caller can persist an error row and keep going.
pub struct Detail {
    pub status: u16,
    pub json: Option<Value>,
    pub error: Option<String>,
}

fn retriable(status: u16) -> bool {
    matches!(status, 429 | 500 | 502 | 503 | 504)
}

fn snippet(s: &str, n: usize) -> String {
    s.chars().take(n).collect::<String>().replace('\n', " ")
}

/// Headers every request carries (also used by the auth client).
pub fn base_headers() -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert("x-consumer-key", HeaderValue::from_static(CONSUMER_KEY));
    headers.insert("accept", HeaderValue::from_static("application/json"));
    headers
}

/// Where the bearer token comes from.
enum Cred {
    /// No auth — public endpoints only, no answer-hash fields.
    None,
    /// A raw token pasted via --bearer (no auto-renewal).
    Static(String),
    /// Stored credentials — auto-refresh + re-login as needed.
    Auth(Arc<Auth>),
}

pub struct Client {
    http: reqwest::Client,
    next_slot: Mutex<Instant>,
    interval: Duration,
    cred: Cred,
}

impl Client {
    pub fn new(rate_per_sec: f64, bearer: Option<String>) -> anyhow::Result<Self> {
        Self::build(rate_per_sec, bearer, None)
    }

    /// Authed client: bearer comes from stored credentials and
    /// auto-renews via refreshaccesstoken / re-login.
    pub fn authed(rate_per_sec: f64, auth: Arc<Auth>) -> anyhow::Result<Self> {
        Self::build(rate_per_sec, None, Some(auth))
    }

    fn build(
        rate_per_sec: f64,
        bearer: Option<String>,
        auth: Option<Arc<Auth>>,
    ) -> anyhow::Result<Self> {
        let http = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .default_headers(base_headers())
            .timeout(TIMEOUT)
            .build()?;
        let cred = if let Some(a) = auth {
            Cred::Auth(a)
        } else if let Some(t) = bearer.as_deref().filter(|t| !t.is_empty()) {
            Cred::Static(t.to_string())
        } else {
            Cred::None
        };
        Ok(Self {
            http,
            next_slot: Mutex::new(Instant::now()),
            interval: if rate_per_sec > 0.0 {
                Duration::from_secs_f64(1.0 / rate_per_sec)
            } else {
                Duration::ZERO
            },
            cred,
        })
    }

    /// True when responses should include answer-hash fields.
    pub fn has_bearer(&self) -> bool {
        !matches!(self.cred, Cred::None)
    }

    /// Current bearer token (None when unauthenticated). Auth renewal
    /// failures degrade to None — the missing-hash warning in fetch
    /// still flags rows for a later authed pass.
    async fn bearer(&self) -> Option<String> {
        match &self.cred {
            Cred::None => None,
            Cred::Static(t) => Some(t.clone()),
            Cred::Auth(a) => match a.token().await {
                Ok(t) => Some(t),
                Err(e) => {
                    warn!("auth renewal failed, request going out unauthed: {e}");
                    None
                }
            },
        }
    }

    /// Reserve the next rate-limit slot and sleep until it arrives.
    async fn acquire(&self) {
        if self.interval.is_zero() {
            return;
        }
        let wait = {
            let mut next = self.next_slot.lock().unwrap();
            let now = Instant::now();
            let slot = (*next).max(now);
            *next = slot + self.interval;
            slot.saturating_duration_since(now)
        };
        if !wait.is_zero() {
            tokio::time::sleep(wait).await;
        }
    }

    /// POST /search for one page of a cell.
    pub async fn search(
        &self,
        cell: &Cell,
        take: usize,
        skip: usize,
    ) -> Result<SearchResp, ApiError> {
        let body = json!({
            "Origin": { "Latitude": cell.lat, "Longitude": cell.lon },
            "RadiusInMeters": cell.radius.round() as i64,
            "Take": take,
            "Skip": skip,
            "CompletionStatuses": [],
            "OnlyHighlyRecommended": false,
            "AdventureTypes": [],
            "MedianCompletionTimes": [],
            "Themes": [],
            "ExcludeOwned": false,
        });
        let mut last_err = String::new();
        for attempt in 0..RETRIES {
            self.acquire().await;
            let mut req = self.http.post(SEARCH_ENDPOINT).json(&body);
            if let Some(t) = self.bearer().await {
                req = req.bearer_auth(t);
            }
            let resp = match req.send().await {
                Ok(r) => r,
                Err(e) => {
                    last_err = format!("network: {e}");
                    tokio::time::sleep(Duration::from_secs_f64(BACKOFF.powi(attempt as i32))).await;
                    continue;
                }
            };
            let status = resp.status().as_u16();
            if status == 200 {
                return match resp.json::<Value>().await {
                    Ok(v) => Ok(SearchResp {
                        total_count: v.get("totalCount").and_then(|t| t.as_u64()).unwrap_or(0),
                        items: v
                            .get("items")
                            .and_then(|i| i.as_array())
                            .cloned()
                            .unwrap_or_default(),
                    }),
                    Err(e) => {
                        last_err = format!("json: {e}");
                        tokio::time::sleep(Duration::from_secs_f64(BACKOFF.powi(attempt as i32)))
                            .await;
                        continue;
                    }
                };
            }
            if retriable(status) {
                let wait = resp
                    .headers()
                    .get("retry-after")
                    .and_then(|h| h.to_str().ok())
                    .and_then(|s| s.parse::<f64>().ok())
                    .unwrap_or_else(|| BACKOFF.powi(attempt as i32));
                last_err = format!("http {status}: {}", snippet(&resp.text().await.unwrap_or_default(), 200));
                tokio::time::sleep(Duration::from_secs_f64(wait)).await;
                continue;
            }
            return Err(ApiError::Status(status, snippet(&resp.text().await.unwrap_or_default(), 300)));
        }
        Err(ApiError::Retries(last_err))
    }

    /// Global coverage check: totalCount for a planet-sized query.
    pub async fn global_total(&self) -> Result<u64, ApiError> {
        let cell = Cell::new(0.0, 0.0, GLOBAL_RADIUS_M);
        Ok(self.search(&cell, 1, 0).await?.total_count)
    }

    /// GET /adventures/{guid}. Errors are returned, not raised.
    pub async fn detail(&self, guid: &str) -> Detail {
        let url = format!("{DETAIL_ENDPOINT}/{guid}");
        let mut last_err = String::new();
        for attempt in 0..RETRIES {
            self.acquire().await;
            let mut req = self.http.get(&url);
            if let Some(t) = self.bearer().await {
                req = req.bearer_auth(t);
            }
            let resp = match req.send().await {
                Ok(r) => r,
                Err(e) => {
                    last_err = format!("network: {e}");
                    tokio::time::sleep(Duration::from_secs_f64(BACKOFF.powi(attempt as i32))).await;
                    continue;
                }
            };
            let status = resp.status().as_u16();
            if status == 200 {
                return match resp.json::<Value>().await {
                    Ok(v) => Detail { status: 200, json: Some(v), error: None },
                    Err(e) => {
                        last_err = format!("json: {e}");
                        tokio::time::sleep(Duration::from_secs_f64(BACKOFF.powi(attempt as i32)))
                            .await;
                        continue;
                    }
                };
            }
            if retriable(status) {
                let wait = resp
                    .headers()
                    .get("retry-after")
                    .and_then(|h| h.to_str().ok())
                    .and_then(|s| s.parse::<f64>().ok())
                    .unwrap_or_else(|| BACKOFF.powi(attempt as i32));
                last_err = format!("http {status}: {}", snippet(&resp.text().await.unwrap_or_default(), 200));
                tokio::time::sleep(Duration::from_secs_f64(wait)).await;
                continue;
            }
            return Detail {
                status,
                json: None,
                error: Some(format!("http {status}: {}", snippet(&resp.text().await.unwrap_or_default(), 200))),
            };
        }
        Detail { status: 0, json: None, error: Some(format!("max retries exceeded: {last_err}")) }
    }

    /// POST /public/adventures/{guid}/reviews/search?skip&take —
    /// one page of an adventure's reviews. Errors returned as data.
    pub async fn reviews(&self, guid: &str, skip: usize, take: usize) -> ReviewsResp {
        let url = format!(
            "{DETAIL_ENDPOINT}/{guid}/reviews/search?skip={skip}&take={take}"
        );
        let mut last_err = String::new();
        for attempt in 0..RETRIES {
            self.acquire().await;
            let mut req = self.http.post(&url).json(&json!({}));
            if let Some(t) = self.bearer().await {
                req = req.bearer_auth(t);
            }
            let resp = match req.send().await {
                Ok(r) => r,
                Err(e) => {
                    last_err = format!("network: {e}");
                    tokio::time::sleep(Duration::from_secs_f64(BACKOFF.powi(attempt as i32))).await;
                    continue;
                }
            };
            let status = resp.status().as_u16();
            if status == 200 {
                return match resp.json::<Value>().await {
                    Ok(v) => ReviewsResp {
                        total_count: v
                            .get("totalCount")
                            .and_then(|t| t.as_u64())
                            .unwrap_or(0),
                        items: v
                            .get("items")
                            .and_then(|i| i.as_array())
                            .cloned()
                            .unwrap_or_default(),
                        error: None,
                    },
                    Err(e) => {
                        last_err = format!("json: {e}");
                        tokio::time::sleep(Duration::from_secs_f64(BACKOFF.powi(attempt as i32)))
                            .await;
                        continue;
                    }
                };
            }
            if retriable(status) {
                let wait = resp
                    .headers()
                    .get("retry-after")
                    .and_then(|h| h.to_str().ok())
                    .and_then(|s| s.parse::<f64>().ok())
                    .unwrap_or_else(|| BACKOFF.powi(attempt as i32));
                last_err =
                    format!("http {status}: {}", snippet(&resp.text().await.unwrap_or_default(), 200));
                tokio::time::sleep(Duration::from_secs_f64(wait)).await;
                continue;
            }
            return ReviewsResp {
                total_count: 0,
                items: vec![],
                error: Some(format!(
                    "http {status}: {}",
                    snippet(&resp.text().await.unwrap_or_default(), 200)
                )),
            };
        }
        ReviewsResp {
            total_count: 0,
            items: vec![],
            error: Some(format!("max retries exceeded: {last_err}")),
        }
    }
}
