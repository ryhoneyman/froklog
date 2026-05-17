use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::{ConnectInfo, Request, State};
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use tokio::sync::Mutex;
use tracing::warn;

struct IpState {
    count: u32,
    window_start: Instant,
    banned_until: Option<Instant>,
}

pub struct RateLimiter {
    ips: Mutex<HashMap<String, IpState>>,
    max_requests: u32,
    window: Duration,
    ban_duration: Duration,
}

impl RateLimiter {
    pub fn new(max_requests: u32, window_secs: u64, ban_secs: u64) -> Arc<Self> {
        Arc::new(Self {
            ips: Mutex::new(HashMap::new()),
            max_requests,
            window: Duration::from_secs(window_secs),
            ban_duration: Duration::from_secs(ban_secs),
        })
    }

    /// Returns `true` if the request is allowed, `false` if it should be blocked.
    pub async fn check(&self, ip: &str) -> bool {
        let now = Instant::now();
        let mut ips = self.ips.lock().await;

        let state = ips.entry(ip.to_string()).or_insert(IpState {
            count: 0,
            window_start: now,
            banned_until: None,
        });

        // Still banned?
        if let Some(until) = state.banned_until {
            if until > now {
                return false;
            }
            // Ban expired — reset so they start fresh.
            state.banned_until = None;
            state.count = 0;
            state.window_start = now;
            return true;
        }

        // Reset window if expired.
        if now.duration_since(state.window_start) >= self.window {
            state.count = 0;
            state.window_start = now;
        }

        state.count += 1;

        if state.count > self.max_requests {
            let until = now + self.ban_duration;
            warn!(
                "DoS: blacklisting {} ({} reqs in {}s) for {}s",
                ip,
                state.count,
                self.window.as_secs(),
                self.ban_duration.as_secs(),
            );
            state.banned_until = Some(until);
            return false;
        }

        true
    }

    /// Evict stale entries to prevent unbounded memory growth.
    pub async fn cleanup(&self) {
        let now = Instant::now();
        let mut ips = self.ips.lock().await;
        ips.retain(|_, s| {
            s.banned_until.is_some_and(|u| u > now)
                || now.duration_since(s.window_start) < self.window * 3
        });
    }
}

pub async fn rate_limit_middleware(
    State(state): State<crate::ServerState>,
    request: Request,
    next: Next,
) -> Response {
    let ip = {
        let peer = request
            .extensions()
            .get::<ConnectInfo<SocketAddr>>()
            .map(|ci| ci.0)
            .unwrap_or_else(|| "0.0.0.0:0".parse().unwrap());
        crate::client_ip(request.headers(), peer)
    };

    if !state.rate_limiter.check(&ip).await {
        warn!("Blocked [{ip}]: blacklisted");
        return StatusCode::TOO_MANY_REQUESTS.into_response();
    }

    next.run(request).await
}
