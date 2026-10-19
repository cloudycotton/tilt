//! Self-update: every start runs the newest release. Before anything else, tilt asks GitHub for
//! the latest release; when it is newer, tilt downloads it, checks it against the release's
//! SHA256SUMS, puts it in place of its own executable and runs it with the same arguments.
//! Any failure (offline, no write access) leaves the current version running.
//!
//! Only release builds do this (TILT_SELF_UPDATE set at build time, by docker/Dockerfile), so a
//! development build never replaces itself. TILT_NO_UPDATE=1 turns it off at run time.

use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::time::Duration;

use anyhow::{bail, ensure, Context};

const ENABLED: bool = option_env!("TILT_SELF_UPDATE").is_some();
const RELEASES: &str = "https://github.com/cloudycotton/tilt/releases";
const CHECK_TIMEOUT: Duration = Duration::from_secs(3);
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(120);
const MAX_BINARY: u64 = 64 * 1024 * 1024;
const TRIPLE: &str = if cfg!(target_arch = "aarch64") {
    "aarch64-unknown-linux-musl"
} else {
    "x86_64-unknown-linux-musl"
};

/// Replaces this process with the latest release when there is a newer one; returns otherwise.
pub fn run() {
    if !ENABLED || std::env::var_os("TILT_NO_UPDATE").is_some_and(|v| !v.is_empty()) {
        return;
    }
    let releases = std::env::var("TILT_RELEASES").unwrap_or_else(|_| RELEASES.to_owned());
    // Before the swap: afterwards Linux names the running executable "<path> (deleted)".
    let exe = match std::env::current_exe().and_then(|e| e.canonicalize()) {
        Ok(exe) => exe,
        Err(e) => return eprintln!("tilt: not updated: cannot find my executable: {e}"),
    };
    match update(&releases, env!("CARGO_PKG_VERSION"), &exe) {
        Ok(None) => {}
        Ok(Some(version)) => {
            eprintln!("tilt: updated to {version}");
            // Only returns on failure. The new binary does not check again.
            let e = std::process::Command::new(&exe)
                .args(std::env::args_os().skip(1))
                .env("TILT_NO_UPDATE", "1")
                .exec();
            eprintln!("tilt: cannot run {}: {e}", exe.display());
            std::process::exit(1);
        }
        Err(e) => eprintln!("tilt: not updated: {e:#}"),
    }
}

/// Installs the latest release over the running executable when it is newer than `current`;
/// Some(its version) when it did.
fn update(releases: &str, current: &str, exe: &Path) -> anyhow::Result<Option<String>> {
    let latest = latest(releases)?;
    if version(&latest) <= version(current) {
        return Ok(None);
    }
    eprintln!("tilt: downloading {latest}...");
    let agent = agent(DOWNLOAD_TIMEOUT, 10);
    let base = format!("{releases}/download/v{latest}");
    let name = format!("tilt-{TRIPLE}");
    let get = |file: &str| -> anyhow::Result<Vec<u8>> {
        let mut res = agent
            .get(format!("{base}/{file}"))
            .call()
            .with_context(|| format!("downloading {file}"))?;
        ensure!(res.status() == 200, "{file}: HTTP {}", res.status());
        Ok(res
            .body_mut()
            .with_config()
            .limit(MAX_BINARY)
            .read_to_vec()?)
    };
    let sums = String::from_utf8(get("SHA256SUMS")?).context("SHA256SUMS is not text")?;
    let want = sums
        .lines()
        .filter_map(|l| l.split_once(char::is_whitespace))
        .find(|(_, file)| file.trim() == name)
        .map(|(sum, _)| sum.to_owned())
        .with_context(|| format!("SHA256SUMS has no {name}"))?;
    let bin = get(&name)?;
    let got: String = ring::digest::digest(&ring::digest::SHA256, &bin)
        .as_ref()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    ensure!(got == want, "{name}: checksum mismatch");
    replace(exe, &bin).with_context(|| format!("cannot replace {}", exe.display()))?;
    Ok(Some(latest))
}

/// The latest release's version, from where GitHub's /releases/latest redirects.
fn latest(releases: &str) -> anyhow::Result<String> {
    let res = agent(CHECK_TIMEOUT, 0)
        .get(format!("{releases}/latest"))
        .call()
        .context("checking for a new release")?;
    let location = res
        .headers()
        .get("location")
        .and_then(|l| l.to_str().ok())
        .unwrap_or("");
    let tag = location.rsplit('/').next().unwrap_or("");
    let v = tag.strip_prefix('v').unwrap_or(tag);
    if version(v).is_none() {
        bail!("no release version in the redirect of {releases}/latest");
    }
    Ok(v.to_owned())
}

fn agent(timeout: Duration, redirects: u32) -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(timeout))
        .max_redirects(redirects)
        .http_status_as_error(false)
        .build()
        .into()
}

/// `x.y.z` as numbers; None for anything else.
fn version(v: &str) -> Option<(u64, u64, u64)> {
    let mut parts = v.split('.').map(|p| p.parse::<u64>().ok());
    match (parts.next(), parts.next(), parts.next(), parts.next()) {
        (Some(Some(a)), Some(Some(b)), Some(Some(c)), None) => Some((a, b, c)),
        _ => None,
    }
}

/// Writes `bin` next to `exe` and renames it over `exe`, so the swap is atomic.
fn replace(exe: &Path, bin: &[u8]) -> anyhow::Result<()> {
    let tmp = exe.with_file_name(format!(".tilt-update.{}", std::process::id()));
    let result = (|| {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(bin)?;
        f.set_permissions(std::fs::Permissions::from_mode(0o755))?;
        f.sync_all()?;
        std::fs::rename(&tmp, exe)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    Ok(result?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader};
    use std::net::TcpListener;

    #[test]
    fn versions_compare_as_numbers() {
        assert!(version("0.10.0") > version("0.9.9"));
        assert!(version("1.0.0") > version("0.99.99"));
        assert_eq!(version("0.2.0"), Some((0, 2, 0)));
        for bad in ["", "1.2", "1.2.3.4", "1.2.x", "v1.2.3"] {
            assert_eq!(version(bad), None, "{bad}");
        }
    }

    /// A release server: /latest redirects to v`latest`, and the download serves `bin` and
    /// SHA256SUMS listing `sum` for it.
    fn serve(latest: &'static str, bin: &'static [u8], sum: String) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let mut stream = stream.unwrap();
                let mut line = String::new();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                reader.read_line(&mut line).unwrap();
                loop {
                    let mut h = String::new();
                    if reader.read_line(&mut h).unwrap() <= 2 {
                        break;
                    }
                }
                let path = line.split_whitespace().nth(1).unwrap_or("").to_owned();
                let (head, body): (String, Vec<u8>) = if path == "/latest" {
                    (format!("302 Found\r\nlocation: /tag/v{latest}"), vec![])
                } else if path.ends_with("/SHA256SUMS") {
                    (
                        "200 OK".into(),
                        format!("{sum}  tilt-{TRIPLE}\n").into_bytes(),
                    )
                } else if path.ends_with(&format!("/tilt-{TRIPLE}")) {
                    ("200 OK".into(), bin.to_vec())
                } else {
                    ("404 Not Found".into(), vec![])
                };
                let _ = write!(
                    stream,
                    "HTTP/1.1 {head}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(&body);
            }
        });
        base
    }

    fn sha256(b: &[u8]) -> String {
        ring::digest::digest(&ring::digest::SHA256, b)
            .as_ref()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }

    #[test]
    fn finds_the_latest_version_and_only_moves_forward() {
        let base = serve("0.3.1", b"", String::new());
        assert_eq!(latest(&base).unwrap(), "0.3.1");
        // Not newer: nothing is downloaded or replaced.
        let exe = Path::new("/nonexistent/tilt");
        assert_eq!(update(&base, "0.3.1", exe).unwrap(), None);
        assert_eq!(update(&base, "0.4.0", exe).unwrap(), None);
    }

    #[test]
    fn a_bad_checksum_replaces_nothing() {
        let base = serve("9.0.0", b"new tilt", sha256(b"other"));
        let e = update(&base, "0.1.0", Path::new("/nonexistent/tilt")).unwrap_err();
        assert!(format!("{e:#}").contains("checksum mismatch"), "{e:#}");
    }

    #[test]
    fn replace_swaps_the_file_atomically() {
        let dir = std::env::temp_dir().join(format!("tilt-update-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let exe = dir.join("tilt");
        std::fs::write(&exe, b"old").unwrap();
        replace(&exe, b"new").unwrap();
        assert_eq!(std::fs::read(&exe).unwrap(), b"new");
        let mode = std::fs::metadata(&exe).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o755);
        assert_eq!(
            std::fs::read_dir(&dir).unwrap().count(),
            1,
            "no temporary file left"
        );
        std::fs::remove_dir_all(&dir).unwrap();
        // A directory it cannot write to: an error, and nothing changes.
        assert!(replace(&dir.join("gone/tilt"), b"new").is_err());
    }
}
