//! HTTP: the embedded client, /healthz, /api/status and the /stream WebSocket
//! (brief section 5.11).

use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::extract::ws::WebSocketUpgrade;
use axum::extract::State;
use axum::http::header::{AUTHORIZATION, CACHE_CONTROL, FORWARDED, HOST, ORIGIN, WWW_AUTHENTICATE};
use axum::http::{HeaderMap, HeaderValue, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::serve::ListenerExt;
use axum::{Json, Router};
use serde::Serialize;
use tokio::sync::watch;
use tracing::{debug, info, warn};

use crate::assets;
use crate::auth::{AuthError, Role, TokenSource};
use crate::clock::LogEvery;
use crate::config::Config;
use crate::control::ControlState;
use crate::events::EventStreams;
use crate::governor::Governor;
use crate::hub::FrameHub;
use crate::input::InputHandle;
use crate::protocol::ScreenSize;
use crate::session::{self, SessionStatus, Sessions};
use crate::x11::cursor::{CursorState, CursorWaker};

/// Everything the handlers, sessions and encoder workers share.
pub struct AppState {
    pub cfg: Arc<Config>,
    pub tokens: TokenSource,
    pub hub: Arc<FrameHub>,
    pub control: ControlState,
    pub input: InputHandle,
    pub cursor: watch::Receiver<CursorState>,
    /// Rung when the sessions watching video change.
    #[expect(dead_code, reason = "phase 0 stub: rung by sessions (G4)")]
    pub cursor_waker: CursorWaker,
    pub sessions: Sessions,
    /// The CPU governor all encoder workers share.
    pub governor: Arc<Governor>,
    /// Open /api/events streams.
    #[expect(dead_code, reason = "phase 0 stub: used by /api/events (G4 D3)")]
    pub events: EventStreams,
}

/// What main builds before the server: the parts of AppState that are not made here.
pub struct AppParts {
    pub cfg: Arc<Config>,
    pub tokens: TokenSource,
    pub hub: Arc<FrameHub>,
    pub input: InputHandle,
    pub cursor: watch::Receiver<CursorState>,
    pub cursor_waker: CursorWaker,
    pub sessions: Sessions,
    pub governor: Arc<Governor>,
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
            cursor_waker: p.cursor_waker,
            sessions: p.sessions,
            governor: p.governor,
            events: EventStreams::default(),
        })
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

/// Incoming WebSocket messages are small: input, ACKs, and TEXT, which is cut at 4 KiB
/// (protocol::MAX_TEXT_BYTES). The margin lets a client that sends longer TEXT keep its
/// session.
const MAX_INCOMING_MSG: usize = 64 * 1024;
/// Same delay as a failed WebSocket handshake.
const AUTH_FAIL_DELAY: Duration = Duration::from_millis(500);
/// How long `serve` waits for sessions to close after the shutdown signal.
const SESSION_DRAIN: Duration = Duration::from_secs(3);
const X_FORWARDED_HOST: &str = "x-forwarded-host";

pub fn router(state: Arc<AppState>) -> Router {
    let mut router = Router::new()
        .route("/healthz", get(healthz))
        .route("/api/status", get(status))
        .route("/stream", get(stream));
    for asset in assets::ASSETS {
        router = router.route(asset.path, get(asset_handler));
    }
    router.with_state(state)
}

async fn asset_handler(uri: Uri, headers: HeaderMap) -> Response {
    match assets::find(uri.path()) {
        Some(asset) => assets::respond(asset, &headers),
        None => StatusCode::NOT_FOUND.into_response(),
    }
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

/// The token of `Bearer <token>` (scheme case-insensitive, RFC 9110 11.1).
fn bearer_token(value: &str) -> Option<&str> {
    let (scheme, token) = value.trim().split_once(' ')?;
    scheme.eq_ignore_ascii_case("bearer").then(|| token.trim())
}

async fn stream(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    // With --no-auth, anyone who can reach the port gets control, and browsers apply no
    // same-origin policy to WebSockets: any page the user visits could connect. So refuse
    // browsers on other sites' pages. Clients that send no Origin (probes, native apps) are
    // unaffected, and so are token modes: there the token is the secret, and other origins
    // cannot read it. (Addition to brief 5.11.)
    if state.tokens.no_auth()
        && !same_origin(&headers)
        && !allowed_origin(&state.cfg.allow_origin, &headers)
    {
        static REFUSALS: Mutex<LogEvery> = Mutex::new(LogEvery::new(Duration::from_secs(10)));
        let log = REFUSALS
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .ready(Instant::now());
        if let Some(more) = log {
            warn!(
                origin = ?headers.get(ORIGIN),
                host = ?headers.get(HOST),
                forwarded_host = ?headers.get(X_FORWARDED_HOST),
                forwarded = ?headers.get(FORWARDED),
                "refused a /stream upgrade from another origin, as --no-auth gives everyone \
                 control. If the page is yours (behind a proxy that rewrites Host, say), pass \
                 --allow-origin with its origin, or use a token ({more} more since the last \
                 warning)"
            );
        }
        return (
            StatusCode::FORBIDDEN,
            "cross-origin /stream refused with --no-auth\n",
        )
            .into_response();
    }
    ws.max_message_size(MAX_INCOMING_MSG)
        .max_frame_size(MAX_INCOMING_MSG)
        .on_failed_upgrade(|e| debug!("websocket upgrade failed: {e}"))
        .on_upgrade(move |socket| session::run(socket, state))
}

/// True when the request has no Origin, or one whose host and port are those the request was
/// sent to: the Host header, or the host a proxy that rewrites Host passed on in
/// X-Forwarded-Host or Forwarded (`host=`). Browsers set Origin on every WebSocket handshake
/// and scripts cannot set any of these headers on one. A Host without a port matches either
/// default port, as a TLS proxy may sit in front.
fn same_origin(headers: &HeaderMap) -> bool {
    let Some(origin) = headers.get(ORIGIN) else {
        return true;
    };
    let Some((scheme, authority)) = origin.to_str().ok().and_then(|o| o.split_once("://")) else {
        // "null" (a sandboxed frame, a file: page) or garbage.
        return false;
    };
    let default_port = match scheme.to_ascii_lowercase().as_str() {
        "http" | "ws" => 80,
        "https" | "wss" => 443,
        _ => return false,
    };
    let Some((origin_host, origin_port)) = host_port(authority) else {
        return false;
    };
    let origin_port = origin_port.unwrap_or(default_port);
    let x_forwarded = headers
        .get_all(X_FORWARDED_HOST)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','));
    headers
        .get(HOST)
        .and_then(|v| v.to_str().ok())
        .into_iter()
        .chain(x_forwarded)
        .chain(forwarded_hosts(headers))
        .filter_map(|v| host_port(v.trim()))
        .any(|(host, port)| {
            host == origin_host
                && port.map_or(matches!(origin_port, 80 | 443), |p| p == origin_port)
        })
}

/// The `host=` values of RFC 7239 Forwarded headers, such as
/// `for=192.0.2.1;host="desk.example.com";proto=https, for=10.0.0.1`.
fn forwarded_hosts(headers: &HeaderMap) -> impl Iterator<Item = &str> {
    headers
        .get_all(FORWARDED)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split([',', ';']))
        .filter_map(|pair| {
            let (name, value) = pair.split_once('=')?;
            let host = value.trim().trim_matches('"');
            name.trim().eq_ignore_ascii_case("host").then_some(host)
        })
}

/// True when the request's Origin is one of `--allow-origin`, or that list has `*`: for pages
/// whose origin the Host check cannot confirm, such as behind a proxy that rewrites Host
/// without an X-Forwarded-Host (E2B's edge drops one set in front of it, so a custom domain
/// arrives as the sandbox's own host).
fn allowed_origin(allowed: &[String], headers: &HeaderMap) -> bool {
    let Some(origin) = headers.get(ORIGIN).and_then(|v| v.to_str().ok()) else {
        return false;
    };
    let origin = origin_key(origin);
    allowed
        .iter()
        .any(|a| a.trim() == "*" || origin_key(a) == origin)
}

/// An origin as browsers write it: lowercase, without a trailing slash or a default port.
fn origin_key(origin: &str) -> String {
    let o = origin.trim().trim_end_matches('/').to_ascii_lowercase();
    for (scheme, port) in [("http://", ":80"), ("https://", ":443")] {
        if o.starts_with(scheme) && o.ends_with(port) {
            return o[..o.len() - port.len()].to_owned();
        }
    }
    o
}

/// The lowercased host and the port, if any, of "host", "host:port", "[v6]" or "[v6]:port".
fn host_port(authority: &str) -> Option<(String, Option<u16>)> {
    let (host, port) = if authority.starts_with('[') {
        let end = authority.find(']')? + 1;
        let port = match &authority[end..] {
            "" => None,
            rest => Some(rest.strip_prefix(':')?),
        };
        (&authority[..end], port)
    } else {
        match authority.rsplit_once(':') {
            Some((host, port)) => (host, Some(port)),
            None => (authority, None),
        }
    };
    let port = port.map(str::parse::<u16>).transpose().ok()?;
    let valid =
        !host.is_empty() && !host.contains(|c: char| c == '/' || c == '@' || c.is_whitespace());
    valid.then(|| (host.to_ascii_lowercase(), port))
}

/// Binds `cfg.bind` and serves until `shutdown` resolves. Every accepted socket gets
/// TCP_NODELAY, keepalive and, on Linux, TCP_NOTSENT_LOWAT unless `--notsent-lowat 0`.
///
/// Upgraded WebSockets outlive axum's graceful shutdown, so on `shutdown` every session is
/// told to close (1001) and this waits up to 3 s for them before returning.
pub async fn serve(
    state: Arc<AppState>,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> anyhow::Result<()> {
    let bind = state.cfg.bind;
    let lowat = state.cfg.notsent_lowat;
    let listener = tokio::net::TcpListener::bind(bind)
        .await
        .map_err(|e| anyhow::anyhow!("cannot listen on {bind}: {e}"))?;
    info!("listening on http://{}", listener.local_addr()?);
    let listener = listener.tap_io(move |tcp| tune_socket(tcp, lowat));

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
    Ok(())
}

/// Keepalive probes: the first after this long without traffic, then KEEPALIVE_INTERVAL apart.
const KEEPALIVE_IDLE: Duration = Duration::from_secs(10);
const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(5);
const KEEPALIVE_PROBES: u32 = 3;

fn tune_socket(tcp: &mut tokio::net::TcpStream, lowat: u32) {
    if let Err(e) = tcp.set_nodelay(true) {
        warn!("TCP_NODELAY: {e}");
    }
    // Clients PING every second, so a connection idle for 10 s has a peer that is frozen (a
    // backgrounded page, whose kernel still answers) or gone without a FIN (a phone that
    // lost its network): that one is closed after about 25 s, before the 30 s idle close,
    // freeing its encoder and its --max-viewers place.
    let keepalive = socket2::TcpKeepalive::new()
        .with_time(KEEPALIVE_IDLE)
        .with_interval(KEEPALIVE_INTERVAL)
        .with_retries(KEEPALIVE_PROBES);
    if let Err(e) = socket2::SockRef::from(&*tcp).set_tcp_keepalive(&keepalive) {
        warn!("TCP keepalive: {e}");
    }
    // Keeps unsent data in the kernel small, so a frame that is ready goes out next instead of
    // queueing behind ones the receiver has not taken yet.
    #[cfg(target_os = "linux")]
    if lowat > 0 {
        if let Err(e) = socket2::SockRef::from(&*tcp).set_tcp_notsent_lowat(lowat) {
            warn!("TCP_NOTSENT_LOWAT: {e}");
        }
    }
    #[cfg(not(target_os = "linux"))]
    let _ = lowat;
}

/// An AppState for tests: `args` after `tilt`, no X server, input going nowhere.
#[cfg(test)]
pub fn test_state(args: &[&str]) -> Arc<AppState> {
    let cfg = Arc::new(
        Config::try_load_from(std::iter::once("tilt").chain(args.iter().copied())).unwrap(),
    );
    let (hub, _capture_wake) = FrameHub::new();
    let (input, _input_rx) = InputHandle::detached();
    let cap = crate::sysres::CpuCapacity {
        cpus: 4.0,
        quota_cpus: None,
        affinity: 4,
    };
    AppState::new(AppParts {
        tokens: TokenSource::from_config(&cfg),
        hub,
        input,
        cursor: watch::channel(CursorState::default()).1,
        cursor_waker: CursorWaker::detached(),
        sessions: Sessions::new(cfg.max_viewers),
        governor: Governor::new(&cfg, cap),
        cfg,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(pairs: &[(&'static str, &'static str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for &(name, value) in pairs {
            h.append(name, HeaderValue::from_static(value));
        }
        h
    }

    #[test]
    fn same_origin_compares_host_and_port() {
        let ok = |pairs: &[(&'static str, &'static str)]| same_origin(&headers(pairs));
        assert!(
            ok(&[("host", "localhost:6090")]),
            "no Origin: not a browser"
        );
        assert!(ok(&[
            ("origin", "http://localhost:6090"),
            ("host", "localhost:6090")
        ]));
        assert!(ok(&[
            ("origin", "http://LocalHost:6090"),
            ("host", "localhost:6090")
        ]));
        assert!(ok(&[
            ("origin", "http://[::1]:6090"),
            ("host", "[::1]:6090")
        ]));
        assert!(ok(&[
            ("origin", "http://10.0.0.2"),
            ("host", "10.0.0.2:80")
        ]));
        // Behind a TLS proxy (E2B): the Host has no port, the page is https.
        assert!(ok(&[
            ("origin", "https://6090-abc.e2b.app"),
            ("host", "6090-abc.e2b.app")
        ]));
        // E2B with maskRequestHost: its proxy sets Host to the mask and X-Forwarded-Host to
        // the public host (e2b-dev/infra, shared/pkg/proxy/pool/client.go).
        assert!(ok(&[
            ("origin", "https://6090-abc.e2b.app"),
            ("host", "localhost:6090"),
            ("x-forwarded-host", "6090-abc.e2b.app")
        ]));
        // A proxy that rewrites Host but says what it was.
        assert!(ok(&[
            ("origin", "https://desk.example.com"),
            ("host", "127.0.0.1:6090"),
            ("x-forwarded-host", "desk.example.com")
        ]));
        assert!(ok(&[
            ("origin", "https://desk.example.com"),
            ("host", "127.0.0.1:6090"),
            ("x-forwarded-host", "a.internal, desk.example.com:443")
        ]));
        // The same in RFC 7239 form.
        assert!(ok(&[
            ("origin", "https://desk.example.com"),
            ("host", "127.0.0.1:6090"),
            (
                "forwarded",
                "for=192.0.2.1;Host=desk.example.com;proto=https"
            )
        ]));
        assert!(ok(&[
            ("origin", "https://desk.example.com"),
            ("host", "127.0.0.1:6090"),
            (
                "forwarded",
                "for=10.0.0.1, for=192.0.2.1; host=\"desk.example.com:443\""
            )
        ]));

        assert!(!ok(&[
            ("origin", "https://evil.example"),
            ("host", "localhost:6090")
        ]));
        assert!(!ok(&[
            ("origin", "http://localhost:3000"),
            ("host", "localhost:6090")
        ]));
        assert!(!ok(&[
            ("origin", "http://localhost"),
            ("host", "localhost:6090")
        ]));
        assert!(!ok(&[
            ("origin", "http://desk.example.com:8080"),
            ("host", "desk.example.com")
        ]));
        assert!(!ok(&[("origin", "null"), ("host", "localhost:6090")]));
        assert!(!ok(&[("origin", "file://"), ("host", "localhost:6090")]));
        assert!(
            !ok(&[("origin", "http://localhost:6090")]),
            "no Host to compare with"
        );
        assert!(!ok(&[
            ("origin", "http://[::1:6090"),
            ("host", "[::1:6090")
        ]));
        assert!(!ok(&[
            ("origin", "http://evil.example@localhost:6090"),
            ("host", "localhost:6090")
        ]));
        // A custom domain in front of E2B: its proxy drops the X-Forwarded-Host set in front
        // of it, so only --allow-origin can let this page in.
        assert!(!ok(&[
            ("origin", "https://desk.example.com"),
            ("host", "6090-abc.e2b.app")
        ]));
        assert!(!ok(&[
            ("origin", "https://evil.example"),
            ("host", "127.0.0.1:6090"),
            (
                "forwarded",
                "for=evil.example;proto=https;by=desk.example.com"
            )
        ]));
    }

    #[test]
    fn allowed_origins_match_as_browsers_write_them() {
        let allowed = |list: &[&str], origin: &'static str| {
            let list: Vec<String> = list.iter().map(|s| s.to_string()).collect();
            allowed_origin(&list, &headers(&[("origin", origin)]))
        };
        let desk = ["https://desk.example.com"];
        assert!(allowed(&desk, "https://desk.example.com"));
        assert!(allowed(&desk, "HTTPS://Desk.Example.com"));
        assert!(allowed(
            &["https://desk.example.com:443/"],
            "https://desk.example.com"
        ));
        assert!(allowed(&["http://[::1]:80"], "http://[::1]"));
        assert!(allowed(&["http://a.example:8080"], "http://a.example:8080"));
        assert!(allowed(&["https://a.example", " null "], "null"));
        assert!(allowed(&["*"], "null") && allowed(&["*"], "https://evil.example"));

        assert!(!allowed(&desk, "https://desk.example.com:8443"));
        assert!(!allowed(&desk, "http://desk.example.com"));
        assert!(!allowed(&desk, "https://desk.example.com.evil.example"));
        assert!(!allowed(&desk, "null"));
        assert!(!allowed(&[], "https://desk.example.com"));
        assert!(!allowed(&["http://a.example:80"], "http://a.example:8080"));
        let list = vec!["*".to_owned()];
        assert!(
            !allowed_origin(&list, &HeaderMap::new()),
            "no Origin to allow"
        );
    }

    /// The status line of a WebSocket upgrade of /stream with these extra headers.
    async fn upgrade_status(addr: std::net::SocketAddr, extra: &str) -> String {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        let mut tcp = tokio::net::TcpStream::connect(addr).await.unwrap();
        let request = format!(
            "GET /stream HTTP/1.1\r\nHost: {addr}\r\nConnection: Upgrade\r\nUpgrade: websocket\r\n\
             Sec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n{extra}\r\n"
        );
        tcp.write_all(request.as_bytes()).await.unwrap();
        let mut line = String::new();
        BufReader::new(tcp).read_line(&mut line).await.unwrap();
        line.trim_end().to_owned()
    }

    #[tokio::test]
    async fn no_auth_refuses_browsers_on_other_origins() {
        let switching = "HTTP/1.1 101 Switching Protocols";
        for (args, evil_status) in [
            (&["--no-auth"][..], "HTTP/1.1 403 Forbidden"),
            (&["--token", "secret"][..], switching),
            (
                &[
                    "--no-auth",
                    "--allow-origin",
                    "https://desk.example,https://evil.example",
                ][..],
                switching,
            ),
            (&["--no-auth", "--allow-origin", "*"][..], switching),
            (
                &["--no-auth", "--allow-origin", "https://desk.example"][..],
                "HTTP/1.1 403 Forbidden",
            ),
        ] {
            let state = test_state(args);
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let server = tokio::spawn(async move {
                axum::serve(listener, router(state)).await.unwrap();
            });
            assert_eq!(upgrade_status(addr, "").await, switching, "{args:?}");
            let same = format!("Origin: http://{addr}\r\n");
            assert_eq!(upgrade_status(addr, &same).await, switching, "{args:?}");
            let evil = "Origin: https://evil.example\r\n";
            assert_eq!(upgrade_status(addr, evil).await, evil_status, "{args:?}");
            server.abort();
        }
    }

    #[tokio::test]
    async fn accepted_sockets_get_keepalive() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let _client = tokio::net::TcpStream::connect(addr).await.unwrap();
        let (mut tcp, _) = listener.accept().await.unwrap();
        tune_socket(&mut tcp, 0);
        let sock = socket2::SockRef::from(&tcp);
        assert!(sock.keepalive().unwrap());
        assert_eq!(sock.tcp_keepalive_time().unwrap(), KEEPALIVE_IDLE);
        assert_eq!(sock.tcp_keepalive_interval().unwrap(), KEEPALIVE_INTERVAL);
        assert_eq!(sock.tcp_keepalive_retries().unwrap(), KEEPALIVE_PROBES);
        assert!(tcp.nodelay().unwrap());
    }

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
