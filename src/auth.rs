//! Access tokens and roles.

use std::fs::File;
use std::io::{self, Read, Write};
use std::net::{SocketAddr, UdpSocket};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use anyhow::Context;

use serde::Serialize;

use crate::config::Config;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    /// May take control and inject input.
    Control,
    /// Watch only.
    View,
}

/// Every failed authentication (handshake or /api/status) waits this long first, so tokens
/// cannot be guessed quickly.
pub const AUTH_FAIL_DELAY: std::time::Duration = std::time::Duration::from_millis(500);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthError {
    /// Wrong or missing token.
    Denied,
    /// `--token-file` is configured but is missing or holds no token yet.
    NotReady,
}

/// Where valid tokens come from. The token file is re-read on every check, so E2B can inject
/// it after the sandbox is created.
#[derive(Debug, Clone)]
pub struct TokenSource {
    control: Option<String>,
    view: Option<String>,
    file: Option<PathBuf>,
    no_auth: bool,
}

impl TokenSource {
    pub fn from_config(cfg: &Config) -> TokenSource {
        TokenSource {
            control: cfg.token.clone(),
            view: cfg.view_token.clone(),
            file: cfg.token_file.clone(),
            no_auth: cfg.no_auth,
        }
    }

    pub fn no_auth(&self) -> bool {
        self.no_auth
    }

    /// Role for the token presented in `hello` (or as an `Authorization: Bearer` token).
    /// No-auth mode grants Control to everyone. Callers delay failures (brief 4.1).
    pub async fn authenticate(&self, token: Option<&str>) -> Result<Role, AuthError> {
        if self.no_auth {
            return Ok(Role::Control);
        }
        let file_token = match self.file.clone() {
            // Off the runtime's two threads: a read that hangs (a network mount) must not
            // stall every other connection.
            Some(path) => tokio::task::spawn_blocking(move || read_token_file(&path))
                .await
                .unwrap_or_default(),
            None => None,
        };
        self.check(token, file_token.as_deref())
    }

    fn check(&self, token: Option<&str>, file_token: Option<&str>) -> Result<Role, AuthError> {
        let presented = token.unwrap_or("").as_bytes();
        let matches = |t: Option<&str>| t.is_some_and(|t| ct_eq(presented, t.as_bytes()));
        // Non-short-circuiting `|`, so timing does not reveal which candidate matched.
        let control = matches(self.control.as_deref()) | matches(file_token);
        let view = matches(self.view.as_deref());
        if control {
            Ok(Role::Control)
        } else if view {
            Ok(Role::View)
        } else if self.file.is_some() && file_token.is_none() {
            Err(AuthError::NotReady)
        } else {
            Err(AuthError::Denied)
        }
    }
}

// Tokens are short; a bounded read keeps a mistaken path (a log, a device) harmless.
const TOKEN_FILE_MAX: u64 = 4096;

/// The first non-empty line of the token file, trimmed, after any UTF-8 byte order mark (some
/// editors write one); None while the file is missing, unreadable or holds no token (E2B may
/// create it before writing the token).
fn read_token_file(path: &Path) -> Option<String> {
    let mut content = Vec::new();
    let read = std::fs::metadata(path).and_then(|meta| {
        // Opening a FIFO would wait for a writer.
        if !meta.is_file() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "not a regular file",
            ));
        }
        File::open(path)?
            .take(TOKEN_FILE_MAX)
            .read_to_end(&mut content)
    });
    match read {
        Ok(_) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => return None,
        Err(e) => {
            tracing::warn!("token file {}: {e}", path.display());
            return None;
        }
    }
    let content = String::from_utf8_lossy(&content);
    let content = content.strip_prefix('\u{feff}').unwrap_or(&content);
    content
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(str::to_owned)
}

/// Equality whose running time does not depend on where the inputs differ (only on the longer
/// length).
pub fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    let mut diff = (a.len() ^ b.len()) as u64;
    for i in 0..a.len().max(b.len()) {
        let x = a.get(i).copied().unwrap_or(0);
        let y = b.get(i).copied().unwrap_or(0);
        diff |= u64::from(x ^ y);
    }
    std::hint::black_box(diff) == 0
}

/// The control token tilt keeps for itself when given no --token, --token-file or --no-auth,
/// in $XDG_CONFIG_HOME/tilt/token (else ~/.config/tilt/token), made on first use: the file and
/// its token.
pub fn kept_token() -> anyhow::Result<(PathBuf, String)> {
    let dir = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .context("no HOME to keep a token in: pass --token, --token-file or --no-auth")?
        .join("tilt");
    let file = dir.join("token");
    if let Some(token) = read_token_file(&file) {
        return Ok((file, token));
    }
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).map_err(|e| anyhow::anyhow!("no randomness for a token: {e}"))?;
    let token: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&dir)
        .with_context(|| format!("creating {}", dir.display()))?;
    std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&file)
        .and_then(|mut f| writeln!(f, "{token}"))
        .with_context(|| format!("writing {}", file.display()))?;
    Ok((file, token))
}

/// Links that open the viewer with `token`, in control (`remote=1`): for a server on every
/// interface, localhost and this machine's address.
pub fn links(bind: SocketAddr, token: &str) -> Vec<String> {
    let port = bind.port();
    let hosts = if bind.ip().is_unspecified() {
        // Connecting a UDP socket sends nothing; it only picks the outgoing address.
        let own = UdpSocket::bind("0.0.0.0:0")
            .and_then(|s| s.connect("192.0.2.1:9").and(s.local_addr()))
            .ok()
            .map(|a| a.ip().to_string());
        std::iter::once("localhost".to_owned()).chain(own).collect()
    } else {
        vec![bind.ip().to_string()]
    };
    hosts
        .into_iter()
        .map(|h| {
            let h = if h.contains(':') { format!("[{h}]") } else { h };
            format!("http://{h}:{port}/#token={token}&remote=1")
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source(control: Option<&str>, view: Option<&str>, file: Option<PathBuf>) -> TokenSource {
        TokenSource {
            control: control.map(Into::into),
            view: view.map(Into::into),
            file,
            no_auth: false,
        }
    }

    #[test]
    fn links_name_the_bound_address() {
        let bound: SocketAddr = "127.0.0.1:7000".parse().unwrap();
        assert_eq!(
            links(bound, "t"),
            ["http://127.0.0.1:7000/#token=t&remote=1"]
        );
        let everywhere = links("0.0.0.0:6090".parse().unwrap(), "t");
        assert_eq!(everywhere[0], "http://localhost:6090/#token=t&remote=1");
    }

    #[test]
    fn the_kept_token_is_made_once() {
        let dir = std::env::temp_dir().join(format!("tilt-kept-{}", std::process::id()));
        // No other test reads XDG_CONFIG_HOME.
        std::env::set_var("XDG_CONFIG_HOME", &dir);
        let (file, token) = kept_token().unwrap();
        assert_eq!(file, dir.join("tilt/token"));
        assert_eq!(token.len(), 32);
        assert_eq!(kept_token().unwrap().1, token, "the same token next time");
        let mode = std::os::unix::fs::PermissionsExt::mode(
            &std::fs::metadata(&file).unwrap().permissions(),
        );
        assert_eq!(mode & 0o777, 0o600);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn roles_follow_the_token() {
        let t = source(Some("ctl-secret"), Some("view-secret"), None);
        assert_eq!(t.authenticate(Some("ctl-secret")).await, Ok(Role::Control));
        assert_eq!(t.authenticate(Some("view-secret")).await, Ok(Role::View));
        assert_eq!(
            t.authenticate(Some("ctl-secreT")).await,
            Err(AuthError::Denied)
        );
        assert_eq!(
            t.authenticate(Some("ctl-secret ")).await,
            Err(AuthError::Denied)
        );
        assert_eq!(t.authenticate(Some("")).await, Err(AuthError::Denied));
        assert_eq!(t.authenticate(None).await, Err(AuthError::Denied));
        let view_only = source(
            None,
            Some("v"),
            Some(PathBuf::from("/nonexistent/tilt-token")),
        );
        assert_eq!(view_only.authenticate(Some("v")).await, Ok(Role::View));
    }

    #[tokio::test]
    async fn no_auth_grants_control_to_anyone() {
        let t = TokenSource {
            no_auth: true,
            ..source(None, None, None)
        };
        assert!(t.no_auth());
        assert_eq!(t.authenticate(None).await, Ok(Role::Control));
        assert_eq!(t.authenticate(Some("anything")).await, Ok(Role::Control));
    }

    #[tokio::test]
    async fn token_file_is_reread_on_every_check() {
        let path = std::env::temp_dir().join(format!("tilt-token-test-{}", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let t = source(None, Some("view"), Some(path.clone()));

        assert_eq!(
            t.authenticate(Some("first")).await,
            Err(AuthError::NotReady)
        );
        assert_eq!(t.authenticate(Some("view")).await, Ok(Role::View));
        std::fs::write(&path, "").unwrap();
        assert_eq!(t.authenticate(Some("")).await, Err(AuthError::NotReady));
        std::fs::write(&path, "first\r\nsecond line\n").unwrap();
        assert_eq!(t.authenticate(Some("first")).await, Ok(Role::Control));
        assert_eq!(
            t.authenticate(Some("second line")).await,
            Err(AuthError::Denied)
        );
        assert_eq!(t.authenticate(None).await, Err(AuthError::Denied));
        std::fs::write(&path, "  rotated  ").unwrap();
        assert_eq!(t.authenticate(Some("first")).await, Err(AuthError::Denied));
        assert_eq!(t.authenticate(Some("rotated")).await, Ok(Role::Control));
        std::fs::remove_file(&path).unwrap();
        assert_eq!(
            t.authenticate(Some("rotated")).await,
            Err(AuthError::NotReady)
        );
    }

    #[tokio::test]
    async fn static_and_file_tokens_both_grant_control() {
        let path = std::env::temp_dir().join(format!("tilt-token-both-{}", std::process::id()));
        std::fs::write(&path, "from-file\n").unwrap();
        let t = source(Some("static"), None, Some(path.clone()));
        assert_eq!(t.authenticate(Some("static")).await, Ok(Role::Control));
        assert_eq!(t.authenticate(Some("from-file")).await, Ok(Role::Control));
        assert_eq!(t.authenticate(Some("other")).await, Err(AuthError::Denied));
        std::fs::remove_file(&path).unwrap();
    }

    #[tokio::test]
    async fn token_file_skips_a_byte_order_mark_and_blank_lines() {
        let path = std::env::temp_dir().join(format!("tilt-token-edges-{}", std::process::id()));
        let t = source(None, None, Some(path.clone()));
        for content in [
            "secret\n",
            "\u{feff}secret\n",
            "\nsecret\n",
            "secret\r\n",
            "\u{feff}\r\n \t\n secret \nlater\n",
        ] {
            std::fs::write(&path, content).unwrap();
            assert_eq!(
                t.authenticate(Some("secret")).await,
                Ok(Role::Control),
                "{content:?}"
            );
        }
        for content in ["\u{feff}", "\n\r\n  \n"] {
            std::fs::write(&path, content).unwrap();
            assert_eq!(
                t.authenticate(Some("secret")).await,
                Err(AuthError::NotReady),
                "{content:?}"
            );
        }
        std::fs::remove_file(&path).unwrap();
        // Not a regular file: refused without opening it (a FIFO would block the open).
        let dir = source(None, None, Some(std::env::temp_dir()));
        assert_eq!(
            dir.authenticate(Some("secret")).await,
            Err(AuthError::NotReady)
        );
    }

    #[test]
    fn constant_time_compare() {
        assert!(ct_eq(b"", b""));
        assert!(ct_eq(b"token", b"token"));
        assert!(!ct_eq(b"token", b"tokem"));
        assert!(!ct_eq(b"token", b"Token"));
        assert!(!ct_eq(b"token", b"token\0"));
        assert!(!ct_eq(b"tok", b"token"));
        assert!(!ct_eq(b"", b"x"));
        // A prefix padded with the zeros the loop substitutes must still differ by length.
        assert!(!ct_eq(b"ab", b"ab\0\0"));
    }
}
