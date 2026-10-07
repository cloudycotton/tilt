//! Full-screen grabs of the root window, through MIT-SHM where the server allows it.

use std::fmt;

use anyhow::{anyhow, bail, ensure, Context};
use tracing::{debug, warn};
use x11rb::connection::Connection;
use x11rb::errors::ReplyError;
use x11rb::protocol::shm::ConnectionExt as _;
use x11rb::protocol::xproto::{ConnectionExt as _, ImageFormat, ImageOrder, Screen, Setup, Window};
use x11rb::protocol::ErrorKind;
use x11rb::rust_connection::RustConnection;

#[derive(Debug)]
pub enum GrabError {
    /// The root changed size under the grab (BadMatch): refresh the geometry and grab again.
    GeometryChanged,
    Other(anyhow::Error),
}

impl fmt::Display for GrabError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            GrabError::GeometryChanged => f.write_str("screen geometry changed during the grab"),
            GrabError::Other(e) => fmt::Display::fmt(e, f),
        }
    }
}

impl std::error::Error for GrabError {
    // Transparent over Other, so error chains do not repeat its message.
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            GrabError::GeometryChanged => None,
            GrabError::Other(e) => e.source(),
        }
    }
}

/// How grabs reach us.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Method {
    /// MIT-SHM: the server writes each grab straight into memory we have mapped.
    Shm(ShmKind),
    /// Core GetImage: every frame is copied through the socket.
    GetImage,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ShmKind {
    /// MIT-SHM 1.2: our memfd, handed over with ShmAttachFd.
    Memfd,
    /// MIT-SHM 1.2: a segment the server allocates and passes back (ShmCreateSegment).
    ServerSegment,
    /// SysV shm, which fails across IPC namespaces and, at mode 0600, across uids.
    SysV,
}

/// Best first.
const METHODS: [Method; 4] = [
    Method::Shm(ShmKind::Memfd),
    Method::Shm(ShmKind::ServerSegment),
    Method::Shm(ShmKind::SysV),
    Method::GetImage,
];

impl Method {
    fn name(self) -> &'static str {
        match self {
            Method::Shm(ShmKind::Memfd) => "shm-memfd",
            Method::Shm(ShmKind::ServerSegment) => "shm-segment",
            Method::Shm(ShmKind::SysV) => "shm-sysv",
            Method::GetImage => "get-image",
        }
    }
}

/// Reads the root window as 32-bpp B,G,R,X rows.
pub struct Grabber {
    screen: usize,
    root: Window,
    width: u32,
    height: u32,
    stride: usize,
    scanline_pad: u8,
    method: Method,
    buf: Buffer,
}

impl Grabber {
    /// Prefers MIT-SHM 1.2 with a memfd (shm_attach_fd), then a server-created segment, then
    /// SysV shm, and falls back to core GetImage.
    pub fn new(conn: &RustConnection, screen: usize) -> anyhow::Result<Grabber> {
        Grabber::with_methods(conn, screen, &METHODS)
    }

    fn with_methods(
        conn: &RustConnection,
        screen: usize,
        methods: &[Method],
    ) -> anyhow::Result<Grabber> {
        let setup = conn.setup();
        let info = setup
            .roots
            .get(screen)
            .with_context(|| format!("X screen {screen} does not exist"))?;
        let scanline_pad = check_format(setup, info)?;
        let root = info.root;
        let (width, height) = root_size(conn, root)?;
        let stride = row_bytes(width, scanline_pad);
        let (method, buf) = Buffer::first_usable(conn, root, stride * height as usize, methods)?;
        Ok(Grabber {
            screen,
            root,
            width,
            height,
            stride,
            scanline_pad,
            method,
            buf,
        })
    }

    /// Grabs the whole root into `pixels()`.
    pub fn grab(&mut self, conn: &RustConnection) -> Result<(), GrabError> {
        self.grab_rows(conn, &[(0, self.height)])
    }

    /// Grabs the full-width row bands `[top, bottom)` of the root into their place in
    /// `pixels()`; the other rows keep what earlier grabs left there. Every request is sent
    /// before the first reply is awaited, so several bands cost one round trip.
    pub fn grab_rows(
        &mut self,
        conn: &RustConnection,
        bands: &[(u32, u32)],
    ) -> Result<(), GrabError> {
        // Lossless: the size came from GetGeometry's u16 fields.
        let width = self.width as u16;
        let stride = self.stride;
        let send_failed = |e: x11rb::errors::ConnectionError| GrabError::Other(e.into());
        let in_range = |&(top, bottom): &(u32, u32)| top < bottom && bottom <= self.height;
        if !bands.iter().all(in_range) {
            return Err(GrabError::Other(anyhow!(
                "row bands {bands:?} are not within the screen's {} rows",
                self.height
            )));
        }
        match &mut self.buf {
            Buffer::Shm(segment) => {
                let mut cookies = Vec::with_capacity(bands.len());
                for &(top, bottom) in bands {
                    let offset = u32::try_from(top as usize * stride)
                        .map_err(|_| GrabError::Other(anyhow!("segment offset out of range")))?;
                    let cookie = conn
                        .shm_get_image(
                            self.root,
                            0,
                            top as i16,
                            width,
                            (bottom - top) as u16,
                            !0,
                            ImageFormat::Z_PIXMAP.into(),
                            segment.id(),
                            offset,
                        )
                        .map_err(send_failed)?;
                    cookies.push((cookie, (bottom - top) as usize * stride));
                }
                for (cookie, len) in cookies {
                    let reply = cookie.reply().map_err(grab_error)?;
                    if reply.size as usize != len {
                        return Err(GrabError::Other(anyhow!(
                            "ShmGetImage wrote {} bytes, expected {len}",
                            reply.size
                        )));
                    }
                }
            }
            Buffer::Image(data) => {
                // Empty after release_pages.
                data.resize(stride * self.height as usize, 0);
                for &(top, bottom) in bands {
                    let len = (bottom - top) as usize * stride;
                    let reply = conn
                        .get_image(
                            ImageFormat::Z_PIXMAP,
                            self.root,
                            0,
                            top as i16,
                            width,
                            (bottom - top) as u16,
                            !0,
                        )
                        .map_err(send_failed)?
                        .reply()
                        .map_err(grab_error)?;
                    if reply.data.len() < len {
                        return Err(GrabError::Other(anyhow!(
                            "GetImage returned {} bytes, expected {len}",
                            reply.data.len()
                        )));
                    }
                    let at = top as usize * stride;
                    data[at..at + len].copy_from_slice(&reply.data[..len]);
                }
            }
        }
        Ok(())
    }

    /// The last grab: `size().1` rows, `stride()` bytes apart.
    pub fn pixels(&self) -> &[u8] {
        let len = self.stride * self.height as usize;
        match &self.buf {
            Buffer::Shm(segment) => &segment.bytes()[..len],
            Buffer::Image(data) => &data[..len],
        }
    }

    pub fn stride(&self) -> usize {
        self.stride
    }

    pub fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// The grab path in use, for logs.
    pub fn method(&self) -> &'static str {
        self.method.name()
    }

    /// Gives the memory the grabs land in back to the system while nobody watches; the next
    /// grab must be a full one.
    pub fn release_pages(&mut self) {
        match &mut self.buf {
            Buffer::Shm(segment) => segment.release_pages(),
            Buffer::Image(data) => *data = Vec::new(),
        }
    }

    /// Re-reads the root geometry; when it changed, re-creates the buffers and returns true.
    /// A shared segment that still fits the new size is kept.
    pub fn refresh_geometry(&mut self, conn: &RustConnection) -> anyhow::Result<bool> {
        let (width, height) = root_size(conn, self.root)?;
        if (width, height) == (self.width, self.height) {
            return Ok(false);
        }
        let stride = row_bytes(width, self.scanline_pad);
        let len = stride * height as usize;
        match &mut self.buf {
            Buffer::Image(data) => data.resize(len, 0),
            Buffer::Shm(segment) if segment.len() >= len => {}
            Buffer::Shm(_) => {
                // Same method if it still works, else the next ones down.
                let at = METHODS.iter().position(|&m| m == self.method).unwrap_or(0);
                let (method, buf) = Buffer::first_usable(conn, self.root, len, &METHODS[at..])?;
                std::mem::replace(&mut self.buf, buf).release(conn);
                self.method = method;
            }
        }
        (self.width, self.height, self.stride) = (width, height, stride);
        Ok(true)
    }

    /// Starts over with method selection at the current geometry, releasing the old buffers on
    /// both sides, for the capture loop's recovery after repeated grab failures. Dropping a
    /// Grabber instead leaves the server's side of its segment attached until the connection
    /// closes. On error `self` is unchanged.
    pub fn rebuild(&mut self, conn: &RustConnection) -> anyhow::Result<()> {
        let fresh = Grabber::new(conn, self.screen)?;
        std::mem::replace(self, fresh).buf.release(conn);
        Ok(())
    }
}

/// Where grabs land.
enum Buffer {
    Shm(shm::Segment),
    /// The last GetImage reply.
    Image(Vec<u8>),
}

impl Buffer {
    /// The first of `methods` that delivers `len`-byte grabs of `root`.
    fn first_usable(
        conn: &RustConnection,
        root: Window,
        len: usize,
        methods: &[Method],
    ) -> anyhow::Result<(Method, Buffer)> {
        let mut failures = Vec::new();
        for &method in methods {
            match Buffer::new(conn, root, method, len) {
                Ok(buf) => {
                    if method == Method::GetImage && !failures.is_empty() {
                        warn!(
                            "MIT-SHM is unusable ({}); grabbing through core GetImage",
                            failures.join("; ")
                        );
                    }
                    return Ok((method, buf));
                }
                Err(e) => {
                    debug!("grab method {} is unusable: {e:#}", method.name());
                    failures.push(format!("{}: {e:#}", method.name()));
                }
            }
        }
        bail!("cannot read the screen: {}", failures.join("; "))
    }

    fn new(
        conn: &RustConnection,
        root: Window,
        method: Method,
        len: usize,
    ) -> anyhow::Result<Buffer> {
        match method {
            Method::Shm(kind) => shm::Segment::new(conn, root, kind, len).map(Buffer::Shm),
            Method::GetImage => {
                // A 1x1 read proves the request works at all.
                conn.get_image(ImageFormat::Z_PIXMAP, root, 0, 0, 1, 1, !0)?
                    .reply()
                    .context("GetImage")?;
                Ok(Buffer::Image(vec![0; len]))
            }
        }
    }

    /// Frees the buffer, telling the server to drop its side of a shared segment.
    fn release(self, conn: &RustConnection) {
        match self {
            Buffer::Shm(segment) => segment.release(conn),
            Buffer::Image(_) => {}
        }
    }
}

/// Checks that the root reads back as 32-bpp little-endian xRGB, i.e. B,G,R,X bytes, and
/// returns the server's scanline pad in bits.
fn check_format(setup: &Setup, screen: &Screen) -> anyhow::Result<u8> {
    let depth = screen.root_depth;
    let format = setup
        .pixmap_formats
        .iter()
        .find(|f| f.depth == depth)
        .with_context(|| format!("the server lists no pixmap format for root depth {depth}"))?;
    let masks = screen
        .allowed_depths
        .iter()
        .flat_map(|d| &d.visuals)
        .find(|v| v.visual_id == screen.root_visual)
        .map(|v| (v.red_mask, v.green_mask, v.blue_mask));
    ensure!(
        format.bits_per_pixel == 32
            && setup.image_byte_order == ImageOrder::LSB_FIRST
            && masks == Some((0xff_0000, 0xff00, 0xff)),
        "unsupported root visual (depth {depth}, {} bpp, {:?}, RGB masks {masks:x?}): tilt needs \
         32-bpp little-endian xRGB, e.g. Xvfb -screen 0 1920x1080x24",
        format.bits_per_pixel,
        setup.image_byte_order,
    );
    Ok(format.scanline_pad)
}

/// Bytes per row of a `width`-pixel, 32-bpp ZPixmap, padded to `scanline_pad` bits like the
/// server pads it.
fn row_bytes(width: u32, scanline_pad: u8) -> usize {
    let pad = usize::from(scanline_pad.max(8));
    (width as usize * 32).div_ceil(pad) * pad / 8
}

fn root_size(conn: &RustConnection, root: Window) -> anyhow::Result<(u32, u32)> {
    let geometry = conn.get_geometry(root)?.reply()?;
    Ok((geometry.width.into(), geometry.height.into()))
}

/// BadMatch means the grab rectangle no longer fits in the root, i.e. the screen shrank.
fn grab_error(e: ReplyError) -> GrabError {
    match e {
        ReplyError::X11Error(x) if x.error_kind == ErrorKind::Match => GrabError::GeometryChanged,
        e => GrabError::Other(e.into()),
    }
}

/// Shared-memory segments: memfd and fd passing make this Linux-only.
#[cfg(target_os = "linux")]
mod shm {
    use std::io;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

    use anyhow::{bail, ensure, Context};
    use x11rb::connection::Connection;
    use x11rb::protocol::shm::{ConnectionExt as _, Seg};
    use x11rb::protocol::xproto::{ImageFormat, Window};
    use x11rb::rust_connection::RustConnection;

    use super::ShmKind;

    /// A segment the server writes grabs into, mapped read-only on our side.
    pub(super) struct Segment {
        id: Seg,
        ptr: *const u8,
        len: usize,
        /// Mapped with shmat rather than mmap.
        sysv: bool,
        /// The memfd behind the segment, to give its pages back while nobody watches.
        fd: Option<OwnedFd>,
    }

    // SAFETY: the mapping belongs to this value alone and is not tied to the creating thread.
    unsafe impl Send for Segment {}

    impl Segment {
        /// A `len`-byte segment made the `kind` way, attached on both sides and proven with a
        /// 1x1 grab of `root`.
        pub(super) fn new(
            conn: &RustConnection,
            root: Window,
            kind: ShmKind,
            len: usize,
        ) -> anyhow::Result<Segment> {
            let segment = match kind {
                ShmKind::Memfd => Segment::memfd(conn, len)?,
                ShmKind::ServerSegment => Segment::from_server(conn, len)?,
                ShmKind::SysV => Segment::sysv(conn, len)?,
            };
            // A successful attach shows the server could map the memory, not that it may
            // write grabs into it.
            let probe = conn
                .shm_get_image(
                    root,
                    0,
                    0,
                    1,
                    1,
                    !0,
                    ImageFormat::Z_PIXMAP.into(),
                    segment.id,
                    0,
                )?
                .reply();
            if let Err(e) = probe {
                segment.release(conn);
                return Err(anyhow::Error::new(e).context("ShmGetImage"));
            }
            Ok(segment)
        }

        fn memfd(conn: &RustConnection, len: usize) -> anyhow::Result<Segment> {
            require_fd_passing(conn)?;
            // SAFETY: a NUL-terminated name; the result is checked before use.
            let fd = unsafe { libc::memfd_create(c"tilt-grab".as_ptr(), libc::MFD_CLOEXEC) };
            if fd < 0 {
                bail!("memfd_create: {}", io::Error::last_os_error());
            }
            // SAFETY: a fresh fd that nothing else owns.
            let fd = unsafe { OwnedFd::from_raw_fd(fd) };
            // SAFETY: resizes the memfd we own.
            if unsafe { libc::ftruncate(fd.as_raw_fd(), libc::off_t::try_from(len)?) } != 0 {
                bail!("ftruncate: {}", io::Error::last_os_error());
            }
            let id = conn.generate_id()?;
            let segment = Segment {
                id,
                ptr: map(&fd, len)?,
                len,
                sysv: false,
                fd: Some(fd.try_clone().context("dup of the memfd")?),
            };
            // x11rb sends the fd and then closes it; both mappings outlive it.
            conn.shm_attach_fd(id, fd, false)?
                .check()
                .context("ShmAttachFd")?;
            Ok(segment)
        }

        fn from_server(conn: &RustConnection, len: usize) -> anyhow::Result<Segment> {
            require_fd_passing(conn)?;
            let size = u32::try_from(len).context("screen too large for ShmCreateSegment")?;
            let id = conn.generate_id()?;
            let reply = conn
                .shm_create_segment(id, size, false)?
                .reply()
                .context("ShmCreateSegment")?;
            match map(&reply.shm_fd, len) {
                Ok(ptr) => Ok(Segment {
                    id,
                    ptr,
                    len,
                    sysv: false,
                    fd: Some(reply.shm_fd),
                }),
                Err(e) => {
                    detach(conn, id);
                    Err(e)
                }
            }
        }

        fn sysv(conn: &RustConnection, len: usize) -> anyhow::Result<Segment> {
            let id = conn.generate_id()?;
            // SAFETY: creates a private segment; the result is checked before use.
            let shmid = unsafe { libc::shmget(libc::IPC_PRIVATE, len, libc::IPC_CREAT | 0o600) };
            if shmid < 0 {
                bail!("shmget: {}", io::Error::last_os_error());
            }
            let segment = Segment::attach_sysv(conn, id, shmid, len);
            // Marked for removal once both sides are attached (or have failed to), the segment
            // vanishes when both detach, even if tilt dies first.
            // SAFETY: IPC_RMID on the segment created above.
            unsafe { libc::shmctl(shmid, libc::IPC_RMID, std::ptr::null_mut()) };
            segment
        }

        fn attach_sysv(
            conn: &RustConnection,
            id: Seg,
            shmid: libc::c_int,
            len: usize,
        ) -> anyhow::Result<Segment> {
            // SAFETY: attaches the segment created by the caller; checked before use.
            let addr = unsafe { libc::shmat(shmid, std::ptr::null(), libc::SHM_RDONLY) };
            if addr as isize == -1 {
                bail!("shmat: {}", io::Error::last_os_error());
            }
            let segment = Segment {
                id,
                ptr: addr.cast(),
                len,
                sysv: true,
                fd: None,
            };
            // Non-negative: shmget succeeded.
            conn.shm_attach(id, shmid as u32, false)?
                .check()
                .context("ShmAttach")?;
            Ok(segment)
        }

        pub(super) fn id(&self) -> Seg {
            self.id
        }

        pub(super) fn len(&self) -> usize {
            self.len
        }

        pub(super) fn bytes(&self) -> &[u8] {
            // SAFETY: the mapping is `len` bytes and lives as long as `self`. The server writes
            // into it only while a grab waits for its reply, which takes `&mut Grabber`, so no
            // write overlaps this borrow.
            unsafe { std::slice::from_raw_parts(self.ptr, self.len) }
        }

        /// Also drops the server's attachment; dropping alone unmaps only our side.
        pub(super) fn release(self, conn: &RustConnection) {
            detach(conn, self.id);
        }

        /// Frees the memory behind a memfd segment (on both sides) until the next grab writes
        /// it again; the mappings stay valid. Does nothing for SysV segments.
        pub(super) fn release_pages(&self) {
            if let Some(fd) = &self.fd {
                let len = libc::off_t::try_from(self.len).unwrap_or(libc::off_t::MAX);
                // SAFETY: punches a hole in a memfd we hold; the size stays, so both mappings
                // stay valid and read zeros until the server writes again.
                unsafe {
                    libc::fallocate(
                        fd.as_raw_fd(),
                        libc::FALLOC_FL_PUNCH_HOLE | libc::FALLOC_FL_KEEP_SIZE,
                        0,
                        len,
                    )
                };
            }
        }
    }

    impl Drop for Segment {
        fn drop(&mut self) {
            // SAFETY: undoes the shmat/mmap that produced `ptr`; no borrow of it outlives `self`.
            unsafe {
                if self.sysv {
                    libc::shmdt(self.ptr.cast());
                } else {
                    libc::munmap(self.ptr.cast_mut().cast(), self.len);
                }
            }
        }
    }

    /// ShmAttachFd and ShmCreateSegment arrived in MIT-SHM 1.2.
    fn require_fd_passing(conn: &RustConnection) -> anyhow::Result<()> {
        let v = conn.shm_query_version()?.reply()?;
        ensure!(
            (v.major_version, v.minor_version) >= (1, 2),
            "MIT-SHM {}.{} cannot pass fds",
            v.major_version,
            v.minor_version
        );
        Ok(())
    }

    /// Maps `len` bytes of `fd` read-only. Pages are faulted in as grabs fill them: an idle
    /// server keeps none.
    fn map(fd: &OwnedFd, len: usize) -> anyhow::Result<*const u8> {
        // SAFETY: a new shared mapping of an fd we hold, checked against MAP_FAILED.
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ,
                libc::MAP_SHARED,
                fd.as_raw_fd(),
                0,
            )
        };
        if ptr == libc::MAP_FAILED {
            bail!("mmap: {}", io::Error::last_os_error());
        }
        Ok(ptr.cast())
    }

    fn detach(conn: &RustConnection, id: Seg) {
        // Best effort: a failure leaves the segment until the connection closes.
        if let Ok(cookie) = conn.shm_detach(id) {
            cookie.ignore_error();
        }
    }
}

/// MIT-SHM capture needs Linux; elsewhere no segment can exist and grabs use GetImage.
#[cfg(not(target_os = "linux"))]
mod shm {
    use x11rb::protocol::shm::Seg;
    use x11rb::protocol::xproto::Window;
    use x11rb::rust_connection::RustConnection;

    use super::ShmKind;

    pub(super) enum Segment {}

    impl Segment {
        pub(super) fn new(
            _conn: &RustConnection,
            _root: Window,
            _kind: ShmKind,
            _len: usize,
        ) -> anyhow::Result<Segment> {
            anyhow::bail!("MIT-SHM capture is only built for Linux")
        }

        pub(super) fn id(&self) -> Seg {
            match *self {}
        }

        pub(super) fn len(&self) -> usize {
            match *self {}
        }

        pub(super) fn bytes(&self) -> &[u8] {
            match *self {}
        }

        pub(super) fn release(self, _conn: &RustConnection) {
            match self {}
        }

        pub(super) fn release_pages(&self) {
            match *self {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::row_bytes;

    #[test]
    fn pads_rows_like_the_server() {
        assert_eq!(row_bytes(1920, 32), 7680);
        assert_eq!(row_bytes(1, 32), 4);
        assert_eq!(row_bytes(3, 64), 16);
        assert_eq!(row_bytes(4, 64), 16);
    }
}

#[cfg(all(test, target_os = "linux"))]
mod x_tests {
    use std::time::Instant;

    use x11rb::protocol::shm::Seg;
    use x11rb::protocol::xproto::Rectangle;

    use super::*;
    use crate::x11::conn::connect;
    use crate::x11::testutil::Xvfb;

    const RED: u32 = 0xff0000;
    const BLUE: u32 = 0x0000ff;

    /// The pixel at (x, y) of the last grab, as 0xRRGGBB.
    fn rgb_at(grabber: &Grabber, x: usize, y: usize) -> u32 {
        let p = &grabber.pixels()[y * grabber.stride() + x * 4..][..4];
        u32::from_le_bytes([p[0], p[1], p[2], 0])
    }

    fn segment_id(grabber: &Grabber) -> Option<Seg> {
        match &grabber.buf {
            Buffer::Shm(segment) => Some(segment.id()),
            Buffer::Image(_) => None,
        }
    }

    /// The server no longer knows segment `id`.
    fn assert_released(conn: &RustConnection, root: Window, id: Seg) {
        let format = ImageFormat::Z_PIXMAP.into();
        let probe = conn.shm_get_image(root, 0, 0, 1, 1, !0, format, id, 0);
        match probe.unwrap().reply() {
            Err(ReplyError::X11Error(e)) => assert_eq!(e.error_kind, ErrorKind::ShmBadSeg),
            other => panic!("segment {id} is still attached: {other:?}"),
        }
    }

    #[test]
    #[ignore = "needs Xvfb and xsetroot; run in tilt-dev with --ignored"]
    fn grabs_the_screen_with_each_method() {
        let x = Xvfb::start("640x480x24", &[]);
        x.run("xsetroot", &["-solid", "#ff0000"]);
        // A blue block over x 100..=119, y 50..=59 pins down orientation and stride.
        let block = Rectangle {
            x: 100,
            y: 50,
            width: 20,
            height: 10,
        };
        x.fill(BLUE, block);
        let (conn, screen) = connect(&x.display).unwrap();
        for method in METHODS {
            let mut grabber = Grabber::with_methods(&conn, screen, &[method]).unwrap();
            let name = grabber.method();
            assert_eq!(name, method.name());
            assert_eq!((grabber.size(), grabber.stride()), ((640, 480), 2560));
            grabber.grab(&conn).unwrap();
            assert_eq!(grabber.pixels().len(), 2560 * 480, "{name}");
            // xsetroot's red as B, G, R, X bytes.
            assert_eq!(grabber.pixels()[..3], [0, 0, 0xff], "{name}");
            for (x, y, rgb) in [
                (0, 0, RED),
                (639, 479, RED),
                (100, 50, BLUE),
                (119, 59, BLUE),
                (99, 50, RED),
                (120, 59, RED),
                (100, 49, RED),
                (119, 60, RED),
            ] {
                assert_eq!(rgb_at(&grabber, x, y), rgb, "{name} at ({x}, {y})");
            }
        }
    }

    #[test]
    #[ignore = "needs Xvfb; run in tilt-dev with --ignored"]
    fn prefers_memfd_and_falls_back_to_get_image_without_mit_shm() {
        let x = Xvfb::start("320x240x24", &[]);
        let (conn, screen) = connect(&x.display).unwrap();
        assert_eq!(Grabber::new(&conn, screen).unwrap().method(), "shm-memfd");

        let x = Xvfb::start("320x240x24", &["-extension", "MIT-SHM"]);
        let (conn, screen) = connect(&x.display).unwrap();
        let mut grabber = Grabber::new(&conn, screen).unwrap();
        assert_eq!(grabber.method(), "get-image");
        grabber.grab(&conn).unwrap();
    }

    #[test]
    #[ignore = "needs Xvfb and xrandr; run in tilt-dev with --ignored"]
    fn follows_xrandr_mode_switches_with_each_method() {
        let x = Xvfb::start("1920x1080x24", &[]);
        x.add_720p_mode();
        let (conn, screen) = connect(&x.display).unwrap();
        let root = conn.setup().roots[screen].root;
        for method in METHODS {
            // Starting small makes the switch to 1080p outgrow the first buffer.
            x.set_mode("tilt-720p");
            let mut grabber = Grabber::with_methods(&conn, screen, &[method]).unwrap();
            let name = grabber.method();
            grabber.grab(&conn).unwrap();
            assert_eq!(grabber.size(), (1280, 720), "{name}");
            assert!(!grabber.refresh_geometry(&conn).unwrap(), "{name}");

            x.set_mode("1920x1080");
            let small = segment_id(&grabber);
            assert!(grabber.refresh_geometry(&conn).unwrap(), "{name}");
            assert_eq!((grabber.size(), grabber.stride()), ((1920, 1080), 7680));
            assert_eq!(grabber.method(), name);
            grabber.grab(&conn).unwrap();
            assert_eq!(grabber.pixels().len(), 7680 * 1080, "{name}");
            if let Some(id) = small {
                assert_released(&conn, root, id);
            }

            // Shrinking turns the full-size grab into BadMatch.
            x.set_mode("tilt-720p");
            assert!(
                matches!(grabber.grab(&conn), Err(GrabError::GeometryChanged)),
                "{name}"
            );
            let large = segment_id(&grabber);
            assert!(grabber.refresh_geometry(&conn).unwrap(), "{name}");
            assert_eq!((grabber.size(), grabber.stride()), ((1280, 720), 5120));
            // The larger segment still fits the smaller screen.
            assert_eq!(segment_id(&grabber), large, "{name}");
            grabber.grab(&conn).unwrap();
            assert_eq!(grabber.pixels().len(), 5120 * 720, "{name}");
        }
    }

    #[test]
    #[ignore = "needs Xvfb; run in tilt-dev with --ignored"]
    fn rebuild_releases_the_old_segment() {
        let x = Xvfb::start("640x480x24", &[]);
        let (conn, screen) = connect(&x.display).unwrap();
        let mut grabber = Grabber::new(&conn, screen).unwrap();
        let old = segment_id(&grabber).unwrap();
        grabber.rebuild(&conn).unwrap();
        assert_released(&conn, grabber.root, old);
        assert_ne!(segment_id(&grabber), Some(old));
        grabber.grab(&conn).unwrap();
        assert_eq!(grabber.size(), (640, 480));
    }

    /// This process's mapping that starts at `addr`, as its /proc/self/maps line.
    fn mapping_at(addr: usize) -> Option<String> {
        let maps = std::fs::read_to_string("/proc/self/maps").unwrap();
        let start = format!("{addr:x}-");
        maps.lines()
            .find(|l| l.starts_with(&start))
            .map(str::to_owned)
    }

    #[test]
    #[ignore = "needs Xvfb; run in tilt-dev with --ignored"]
    fn dropping_a_grabber_unmaps_its_segment() {
        let x = Xvfb::start("320x240x24", &[]);
        let (conn, screen) = connect(&x.display).unwrap();
        for kind in [ShmKind::Memfd, ShmKind::ServerSegment, ShmKind::SysV] {
            let grabber = Grabber::with_methods(&conn, screen, &[Method::Shm(kind)]).unwrap();
            let addr = grabber.pixels().as_ptr() as usize;
            let mapping = mapping_at(addr).unwrap();
            drop(grabber);
            // Whole lines, inode included, so a parallel test's new mapping at the same address
            // does not count.
            assert_ne!(mapping_at(addr), Some(mapping), "{kind:?}");
        }
    }

    /// Prints grab times at 1920x1080 per method; run with --nocapture to see them.
    #[test]
    #[ignore = "needs Xvfb; run in tilt-dev with --ignored --nocapture"]
    fn grab_time_at_1080p() {
        let x = Xvfb::start("1920x1080x24", &[]);
        x.run("xsetroot", &["-solid", "#336699"]);
        let (conn, screen) = connect(&x.display).unwrap();
        for method in METHODS {
            let mut grabber = Grabber::with_methods(&conn, screen, &[method]).unwrap();
            for _ in 0..10 {
                grabber.grab(&conn).unwrap();
            }
            let mut ms: Vec<f64> = (0..100)
                .map(|_| {
                    let start = Instant::now();
                    grabber.grab(&conn).unwrap();
                    start.elapsed().as_secs_f64() * 1e3
                })
                .collect();
            ms.sort_by(f64::total_cmp);
            let mean = ms.iter().sum::<f64>() / ms.len() as f64;
            println!(
                "grab 1920x1080 {:<11} median {:6.2} ms  mean {:6.2} ms  p95 {:6.2} ms  max {:6.2} ms",
                grabber.method(),
                ms[50],
                mean,
                ms[95],
                ms[99]
            );
            assert_eq!(rgb_at(&grabber, 960, 540), 0x336699);
        }
    }
}
