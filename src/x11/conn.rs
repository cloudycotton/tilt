//! Opening an X connection, including Xvfb's abstract socket when its filesystem socket is off.

use anyhow::anyhow;
use x11rb::rust_connection::RustConnection;

/// Connects to `display` (e.g. ":0") and returns the connection with its default screen number.
/// On Linux, when that fails for `:N` or `unix:N`, retries the abstract socket
/// `@/tmp/.X11-unix/XN`, because E2B runs Xvfb with `-nolisten unix`.
pub fn connect(display: &str) -> anyhow::Result<(RustConnection, usize)> {
    let err = match x11rb::connect(Some(display)) {
        Ok(found) => return Ok(found),
        Err(e) => e,
    };
    #[cfg(target_os = "linux")]
    if let Some(local) = local_display(display) {
        return match connect_local(local) {
            Ok(conn) => Ok((conn, local.screen)),
            Err(e) => Err(anyhow!("cannot open X display {display}: {err}; {e:#}")),
        };
    }
    Err(anyhow!("cannot open X display {display}: {err}"))
}

/// A display on this machine's unix sockets: `:N[.S]` or `unix:N[.S]`.
#[cfg(any(target_os = "linux", test))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct LocalDisplay {
    number: u16,
    screen: usize,
    /// Written as `unix:N`, which x11rb reads as a socket path and so never tries.
    unix_prefix: bool,
}

#[cfg(any(target_os = "linux", test))]
fn local_display(display: &str) -> Option<LocalDisplay> {
    let (rest, unix_prefix) = match display.strip_prefix("unix:") {
        Some(rest) => (rest, true),
        None => (display.strip_prefix(':')?, false),
    };
    let (number, screen) = match rest.split_once('.') {
        Some((number, screen)) => (number.parse().ok()?, screen.parse().ok()?),
        None => (rest.parse().ok()?, 0),
    };
    Some(LocalDisplay {
        number,
        screen,
        unix_prefix,
    })
}

/// The abstract socket, then for `unix:N` also the filesystem socket that x11rb skipped.
#[cfg(target_os = "linux")]
fn connect_local(display: LocalDisplay) -> anyhow::Result<RustConnection> {
    use std::os::linux::net::SocketAddrExt;
    use std::os::unix::net::{SocketAddr, UnixStream};

    use anyhow::Context;
    use x11rb::reexports::x11rb_protocol::xauth::get_auth;
    use x11rb::rust_connection::DefaultStream;

    let path = format!("/tmp/.X11-unix/X{}", display.number);
    let stream = match UnixStream::connect_addr(&SocketAddr::from_abstract_name(&path)?) {
        Ok(stream) => stream,
        Err(abstract_err) if display.unix_prefix => UnixStream::connect(&path)
            .map_err(|e| anyhow!("abstract socket @{path}: {abstract_err}; {path}: {e}"))?,
        Err(e) => return Err(anyhow!("abstract socket @{path}: {e}")),
    };
    let (stream, (family, address)) = DefaultStream::from_unix_stream(stream)?;
    // Like x11rb::connect: without a matching Xauthority entry (e.g. Xvfb -ac), send no cookie.
    let (name, data) = get_auth(family, &address, display.number)
        .ok()
        .flatten()
        .unwrap_or_default();
    RustConnection::connect_to_stream_with_auth_info(stream, display.screen, name, data)
        .with_context(|| format!("X handshake over @{path}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognises_local_display_names() {
        let local = |number, screen, unix_prefix| {
            Some(LocalDisplay {
                number,
                screen,
                unix_prefix,
            })
        };
        assert_eq!(local_display(":0"), local(0, 0, false));
        assert_eq!(local_display(":12.1"), local(12, 1, false));
        assert_eq!(local_display("unix:3"), local(3, 0, true));
        assert_eq!(local_display("unix:3.2"), local(3, 2, true));
        for remote in [
            "",
            ":",
            ":x",
            ":1.y",
            "localhost:0",
            "host:10.0",
            "/tmp/.X11-unix/X0",
        ] {
            assert_eq!(local_display(remote), None, "{remote:?}");
        }
    }
}

#[cfg(all(test, target_os = "linux"))]
mod x_tests {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;

    use x11rb::connection::Connection;
    use x11rb::protocol::xproto::ConnectionExt as _;

    use super::connect;
    use crate::x11::testutil::Xvfb;

    #[test]
    #[ignore = "needs Xvfb; run in tilt-dev with --ignored"]
    fn connects_through_the_filesystem_socket() {
        // -nolisten local leaves only the filesystem socket. With no abstract socket to collide
        // on, -displayfd's search would settle on :0 and take over another server's socket
        // file, so this server gets a number of its own. Only that search recovers when a
        // server starting in parallel wins the race to create the socket directory, so the
        // directory is made first.
        let dir = Path::new("/tmp/.X11-unix");
        if fs::create_dir(dir).is_ok() {
            fs::set_permissions(dir, fs::Permissions::from_mode(0o1777)).unwrap();
        }
        let x = Xvfb::start("640x480x24", &[":4320", "-nolisten", "local"]);
        x11rb::connect(Some(&x.display)).unwrap();
        // x11rb reads unix:N as a socket path and gives up, so this one is the fallback's.
        let unix = format!("unix{}", x.display);
        assert!(x11rb::connect(Some(&unix)).is_err());

        for display in [x.display.clone(), unix] {
            let (conn, screen) = connect(&display).unwrap();
            assert_eq!(screen, 0);
            assert_eq!(conn.setup().roots[0].width_in_pixels, 640);
            conn.get_input_focus().unwrap().reply().unwrap();
        }
    }

    #[test]
    #[ignore = "needs Xvfb; run in tilt-dev with --ignored"]
    fn falls_back_to_the_abstract_socket() {
        let x = Xvfb::start("640x480x24", &["-nolisten", "unix"]);
        // x11rb alone only knows the filesystem socket.
        assert!(x11rb::connect(Some(&x.display)).is_err());

        for display in [x.display.clone(), format!("unix{}", x.display)] {
            let (conn, screen) = connect(&display).unwrap();
            assert_eq!(conn.setup().roots[screen].height_in_pixels, 480);
            // A round trip proves the handshake left a working connection.
            conn.get_input_focus().unwrap().reply().unwrap();
        }
    }

    #[test]
    fn reports_both_failures() {
        let err = format!("{:#}", connect(":4321").unwrap_err());
        assert!(err.starts_with("cannot open X display :4321: "), "{err}");
        assert!(
            err.contains("abstract socket @/tmp/.X11-unix/X4321"),
            "{err}"
        );
    }
}
