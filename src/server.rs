//! HTTP: the embedded client, /healthz, /api/status and /transport, which tells the page how to
//! reach the WebTransport stream (src/wt.rs).

use std::future::Future;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use axum::extract::State;
use axum::http::header::{AUTHORIZATION, CACHE_CONTROL, WWW_AUTHENTICATE};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde::Serialize;
use tokio::sync::watch;
use tracing::{info, warn};

use crate::assets;
use crate::auth::{AuthError, Role, TokenSource, AUTH_FAIL_DELAY};
use crate::config::Config;
use crate::control::ControlState;
use crate::hub::FrameHub;
use crate::input::InputHandle;
use crate::protocol::ScreenSize;
use crate::session::{SessionStatus, Sessions};
use crate::wt;
use crate::x11::cursor::CursorState;

/// Everything the handlers, sessions and encoder workers share.
pub struct AppState {
    pub cfg: Arc<Config>,
    pub tokens: TokenSource,
    pub hub: Arc<FrameHub>,
    pub control: ControlState,
    pub input: InputHandle,
    pub cursor: watch::Receiver<CursorState>,
    pub sessions: Sessions,
    /// The CPUs tilt may use, for `--profile auto`.
    pub cpus: usize,
    /// What GET /transport offers, once the WebTransport endpoint is up.
    wt: Mutex<Option<wt::Advert>>,
}

/// What main builds before the server: the parts of AppState that are not made here.
pub struct AppParts {
    pub cfg: Arc<Config>,
    pub tokens: TokenSource,
    pub hub: Arc<FrameHub>,
    pub input: InputHandle,
    pub cursor: watch::Receiver<CursorState>,
    pub sessions: Sessions,
    pub cpus: usize,
}

impl AppState {
    pub fn new(p: AppParts) -> Arc<AppState> {
        Arc::new(AppState {
            cfg: p.cfg,
            tokens: p.tokens,
            hub: p.hub,
            control: ControlState::new(p.input.clone()),
            input: p.input,
            cursor: p.cursor,
            sessions: p.sessions,
            cpus: p.cpus,
            wt: Mutex::new(None),
        })
    }

    pub fn wt_advert(&self) -> MutexGuard<'_, Option<wt::Advert>> {
        self.wt.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// The GET /api/status body.
#[derive(Debug, Serialize)]
struct Status {
    version: &'static str,
    screen: ScreenSize,
    viewers: usize,
    controller: bool,
    sessions: Vec<SessionStatus>,
}

/// How long `serve` waits for sessions to close after the shutdown signal.
const SESSION_DRAIN: Duration = Duration::from_secs(3);

pub fn router(state: Arc<AppState>) -> Router {
    let mut router = Router::new()
        .route("/healthz", get(healthz))
        .route("/api/status", get(status))
        .route("/transport", get(transport));
    for (i, asset) in assets::ASSETS.iter().enumerate() {
        router = router.route(
            asset.path,
            get(move |headers: HeaderMap| async move { assets::respond(i, &headers) }),
        );
    }
    router.with_state(state)
}

async fn healthz() -> impl IntoResponse {
    (
        [(CACHE_CONTROL, HeaderValue::from_static("no-store"))],
        "ok",
    )
}

async fn status(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    let token = headers
        .get(AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(bearer_token);
    match state.tokens.authenticate(token).await {
        Ok(Role::Control) => {}
        Ok(Role::View) => {
            return (StatusCode::FORBIDDEN, "the control token is required\n").into_response();
        }
        Err(e) => {
            tokio::time::sleep(AUTH_FAIL_DELAY).await;
            return match e {
                AuthError::Denied => (
                    StatusCode::UNAUTHORIZED,
                    [(WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"))],
                    "a valid Authorization: Bearer <token> is required\n",
                )
                    .into_response(),
                AuthError::NotReady => (
                    StatusCode::SERVICE_UNAVAILABLE,
                    "the token file is missing or empty\n",
                )
                    .into_response(),
            };
        }
    }
    let (w, h) = state.hub.screen_size();
    let holder = state.control.holder();
    let body = Status {
        version: env!("CARGO_PKG_VERSION"),
        screen: ScreenSize { w, h },
        viewers: state.sessions.count(),
        controller: holder.is_some(),
        sessions: state.sessions.snapshot(holder),
    };
    (
        [(CACHE_CONTROL, HeaderValue::from_static("no-store"))],
        Json(body),
    )
        .into_response()
}

/// GET /transport: `{"wt":{..}}` (see wt::Advert), how to reach the stream. No auth: other
/// sites' pages cannot read it, so its ticket shows a session came from this page.
async fn transport(State(state): State<Arc<AppState>>) -> Response {
    let Some(advert) = state.wt_advert().clone() else {
        return (StatusCode::SERVICE_UNAVAILABLE, "not listening yet\n").into_response();
    };
    let body = serde_json::json!({ "wt": advert });
    (
        [(CACHE_CONTROL, HeaderValue::from_static("no-store"))],
        Json(body),
    )
        .into_response()
}

/// The token of `Bearer <token>` (scheme case-insensitive, RFC 9110 11.1).
fn bearer_token(value: &str) -> Option<&str> {
    let (scheme, token) = value.trim().split_once(' ')?;
    scheme.eq_ignore_ascii_case("bearer").then(|| token.trim())
}

/// Binds `cfg.bind` for HTTP (TCP) and WebTransport (UDP, same address, on --udp-port if
/// given) and serves until
/// `shutdown` resolves; then tells every session to close (1001) and waits up to 3 s for them.
pub async fn serve(
    state: Arc<AppState>,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> anyhow::Result<()> {
    let bind = state.cfg.bind;
    let listener = tokio::net::TcpListener::bind(bind)
        .await
        .map_err(|e| anyhow::anyhow!("cannot listen on {bind}: {e}"))?;
    let local = listener.local_addr()?;
    let udp = std::net::SocketAddr::new(local.ip(), state.cfg.udp_port.unwrap_or(local.port()));
    let endpoint = wt::bind(&state, udp)?;
    info!("listening on http://{local} and udp {udp}");
    let accepting = tokio::spawn(wt::run(Arc::clone(&endpoint), Arc::clone(&state)));

    let closing = Arc::clone(&state);
    axum::serve(listener, router(Arc::clone(&state)))
        .with_graceful_shutdown(async move {
            shutdown.await;
            closing.sessions.close_all();
        })
        .await?;
    if tokio::time::timeout(SESSION_DRAIN, state.sessions.drained())
        .await
        .is_err()
    {
        warn!(
            "{} session(s) still open after {:?}",
            state.sessions.count(),
            SESSION_DRAIN
        );
    }
    accepting.abort();
    endpoint.close(wtransport::VarInt::from_u32(0), b"shutting down");
    Ok(())
}

/// An AppState for tests: `args` after `tilt`, no X server, input going nowhere.
#[cfg(test)]
pub fn test_state(args: &[&str]) -> Arc<AppState> {
    let cfg = Arc::new(
        Config::try_load_from(std::iter::once("tilt").chain(args.iter().copied())).unwrap(),
    );
    let (hub, _capture_wake) = FrameHub::new();
    let (input, _input_rx) = InputHandle::detached();
    AppState::new(AppParts {
        tokens: TokenSource::from_config(&cfg),
        hub,
        input,
        cursor: watch::channel(CursorState::default()).1,
        sessions: Sessions::new(cfg.max_viewers),
        cpus: 4,
        cfg,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bearer_tokens() {
        assert_eq!(bearer_token("Bearer abc"), Some("abc"));
        assert_eq!(bearer_token("bearer  abc "), Some("abc"));
        assert_eq!(bearer_token("BEARER abc"), Some("abc"));
        assert_eq!(bearer_token("Basic abc"), None);
        assert_eq!(bearer_token("Bearer"), None);
        assert_eq!(bearer_token(""), None);
    }
}
