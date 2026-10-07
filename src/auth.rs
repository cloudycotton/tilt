//! Access tokens and roles.

use std::fs::File;
use std::io::{self, Read};
use std::path::{Path, PathBuf};

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
