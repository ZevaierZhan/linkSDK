//! Checked libzt TCP handles. These descriptors belong to lwIP, never the OS.
use crate::network::frame::Frame;
use parking_lot::Mutex;
use std::{
    net::Ipv4Addr,
    os::raw::{c_int, c_void},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};
extern "C" {
    fn zts_get_thread_socket_error() -> c_int;
}
fn bsd_socket_failure(raw: i64) -> bool {
    raw == -1 || cfg!(windows) && raw == i64::from(u32::MAX)
}
/// SDK-owned thread-local slot, captured on the same calling thread. Wrapper
/// SERVICE/ARG statuses do not set BSD errno and must never consume stale data.
pub fn socket_errno(raw: i64) -> i32 {
    if bsd_socket_failure(raw) {
        unsafe { zts_get_thread_socket_error() }
    } else {
        0
    }
}

/// Validate every SDK byte count before using it as a Rust length. The Windows
/// SDK uses a 32-bit signed ssize_t; generated bindings may expose its EAX -1
/// result as zero-extended 4294967295. None means a negative SDK status, never
/// EOF. Some(0) keeps ordinary stream EOF / empty-datagram semantics.
pub fn checked_io_count(raw: i64, capacity: usize) -> std::io::Result<Option<usize>> {
    checked_io_count_abi(raw, capacity, cfg!(windows))
}
fn checked_io_count_abi(
    raw: i64,
    capacity: usize,
    windows: bool,
) -> std::io::Result<Option<usize>> {
    let invalid = || {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "libzt byte count exceeds the requested buffer or native ABI",
        )
    };
    let signed = if windows {
        if raw < i64::from(i32::MIN) || raw > i64::from(u32::MAX) {
            return Err(invalid());
        }
        i64::from(raw as i32)
    } else {
        raw
    };
    if signed < 0 {
        return Ok(None);
    }
    let count = usize::try_from(signed).map_err(|_| invalid())?;
    if count > capacity {
        return Err(invalid());
    }
    Ok(Some(count))
}

pub struct ZtSocket {
    fd: c_int,
    stopped: AtomicBool,
    writer: Mutex<()>,
}
pub struct SocketIo {
    socket: Arc<ZtSocket>,
    deadline: Instant,
}
#[derive(Debug)]
struct SdkIoFailure {
    operation: &'static str,
    raw: i64,
    socket_errno: i32,
    capacity: usize,
}
impl std::fmt::Display for SdkIoFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "sdk operation={} raw={} socketErrno={} errnoSource=SDK_TLS capacity={}",
            self.operation, self.raw, self.socket_errno, self.capacity
        )
    }
}
impl std::error::Error for SdkIoFailure {}
/// Bounded diagnostic fields only: never format TLS payloads/certificates or
/// arbitrary third-party error text. Retry and diagnostics use one SDK TLS
/// error snapshot, never the compatibility process-global zts_errno.
pub fn io_diagnostic(error: &std::io::Error) -> String {
    let mut value = format!(
        "kind={:?} rawOsCode={:?}",
        error.kind(),
        error.raw_os_error()
    );
    if let Some(sdk) = error
        .get_ref()
        .and_then(|e| e.downcast_ref::<SdkIoFailure>())
    {
        value.push_str(&format!(" {sdk}"));
    }
    value
}
fn sdk_io_error(
    operation: &'static str,
    raw: i64,
    socket_errno: i32,
    capacity: usize,
) -> std::io::Error {
    std::io::Error::other(SdkIoFailure {
        operation,
        raw,
        socket_errno,
        capacity,
    })
}
fn sdk_retryable_status(raw: i64, observed_errno: i32) -> bool {
    // Only the BSD -1 status owns an errno. SDK SERVICE(-2)/ARG(-3) return
    // directly without setting it. Windows may zero-extend its 32-bit -1.
    bsd_socket_failure(raw) && retryable(observed_errno)
}
impl std::io::Read for SocketIo {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        loop {
            if self.socket.stopped.load(Ordering::SeqCst) {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::ConnectionAborted,
                    "overlay TLS cancelled",
                ));
            }
            if Instant::now() >= self.deadline {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "overlay TLS deadline",
                ));
            }
            let (n, observed_errno) = unsafe {
                let n =
                    libzt::zts_bsd_read(self.socket.fd, buf.as_mut_ptr().cast(), buf.len() as _);
                (n, socket_errno(n as i64))
            };
            if let Some(count) = checked_io_count(n as i64, buf.len()).map_err(|_| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    SdkIoFailure {
                        operation: "TLS read count",
                        raw: n as i64,
                        socket_errno: observed_errno,
                        capacity: buf.len(),
                    },
                )
            })? {
                return Ok(count);
            }
            if !sdk_retryable_status(n as i64, observed_errno) {
                return Err(sdk_io_error(
                    "TLS read",
                    n as i64,
                    observed_errno,
                    buf.len(),
                ));
            }
        }
    }
}
impl std::io::Write for SocketIo {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        loop {
            if self.socket.stopped.load(Ordering::SeqCst) {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::ConnectionAborted,
                    "overlay TLS cancelled",
                ));
            }
            if Instant::now() >= self.deadline {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "overlay TLS deadline",
                ));
            }
            let (n, observed_errno) = unsafe {
                let n = libzt::zts_bsd_write(self.socket.fd, buf.as_ptr().cast(), buf.len() as _);
                (n, socket_errno(n as i64))
            };
            if let Some(count) = checked_io_count(n as i64, buf.len()).map_err(|_| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    SdkIoFailure {
                        operation: "TLS write count",
                        raw: n as i64,
                        socket_errno: observed_errno,
                        capacity: buf.len(),
                    },
                )
            })? {
                return Ok(count);
            }
            if !sdk_retryable_status(n as i64, observed_errno) {
                return Err(sdk_io_error(
                    "TLS write",
                    n as i64,
                    observed_errno,
                    buf.len(),
                ));
            }
        }
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
impl Drop for ZtSocket {
    fn drop(&mut self) {
        unsafe {
            libzt::zts_bsd_close(self.fd);
        }
    }
}
fn address(ip: Ipv4Addr, port: u16) -> libzt::zts_sockaddr_in {
    libzt::zts_sockaddr_in {
        sin_len: std::mem::size_of::<libzt::zts_sockaddr_in>() as u8,
        sin_family: libzt::ZTS_AF_INET as _,
        sin_port: port.to_be(),
        sin_addr: crate::network::address::encode(ip),
        sin_zero: [0; 8],
    }
}
fn error(op: &str, rc: isize) -> String {
    format!(
        "{op}: raw={rc}, socketErrno={}, errnoSource=SDK_TLS",
        socket_errno(rc as i64)
    )
}
impl ZtSocket {
    pub fn io(self: &Arc<Self>, deadline: Instant) -> SocketIo {
        SocketIo {
            socket: self.clone(),
            deadline,
        }
    }
    fn new() -> Result<Arc<Self>, String> {
        let fd = unsafe {
            libzt::zts_bsd_socket(libzt::ZTS_AF_INET as _, libzt::ZTS_SOCK_STREAM as _, 0)
        };
        if fd < 0 {
            return Err(error("socket", fd as _));
        }
        Self::from_fd(fd)
    }
    fn from_fd(fd: c_int) -> Result<Arc<Self>, String> {
        let s = Arc::new(Self {
            fd,
            stopped: AtomicBool::new(false),
            writer: Mutex::new(()),
        });
        let timeout = libzt::zts_timeval {
            tv_sec: 0,
            tv_usec: 250_000,
        };
        for option in [libzt::ZTS_SO_RCVTIMEO, libzt::ZTS_SO_SNDTIMEO] {
            let rc = unsafe {
                libzt::zts_bsd_setsockopt(
                    fd,
                    libzt::ZTS_SOL_SOCKET as _,
                    option as _,
                    &timeout as *const _ as *const c_void,
                    std::mem::size_of_val(&timeout) as _,
                )
            };
            if rc != 0 {
                return Err(error("timeout", rc as _));
            }
        }
        // Idle shells remain open; TCP probes, not UDP presence TTL, detect
        // a peer process disappearing without sending an orderly FIN.
        for (level, option, value) in [
            (libzt::ZTS_SOL_SOCKET, libzt::ZTS_SO_KEEPALIVE, 1i32),
            (libzt::ZTS_IPPROTO_TCP, libzt::ZTS_TCP_NODELAY, 1),
            (libzt::ZTS_IPPROTO_TCP, libzt::ZTS_TCP_KEEPIDLE, 5),
            (libzt::ZTS_IPPROTO_TCP, libzt::ZTS_TCP_KEEPINTVL, 2),
            (libzt::ZTS_IPPROTO_TCP, libzt::ZTS_TCP_KEEPCNT, 3),
        ] {
            let rc = unsafe {
                libzt::zts_bsd_setsockopt(
                    fd,
                    level as _,
                    option as _,
                    &value as *const _ as *const c_void,
                    std::mem::size_of_val(&value) as _,
                )
            };
            if rc != 0 {
                return Err(error("keepalive", rc as _));
            }
        }
        Ok(s)
    }
    pub fn connect(ip: &str, port: u16) -> Result<Arc<Self>, String> {
        let s = Self::new()?;
        let ip = std::ffi::CString::new(ip).map_err(|e| e.to_string())?;
        let rc = unsafe { libzt::zts_connect(s.fd, ip.as_ptr(), port, 5000) };
        if rc != 0 {
            return Err(error("connect", rc as _));
        }
        Ok(s)
    }
    /// A real wall-clock connect budget. The SDK zts_connect timeout retries
    /// a blocking connect and cannot bound an individual SYN wait.
    pub fn connect_bounded(ip: &str, port: u16, budget: Duration) -> Result<Arc<Self>, String> {
        let s = Self::new()?;
        let a = address(ip.parse().map_err(|_| "invalid IPv4")?, port);
        if unsafe {
            libzt::zts_bsd_fcntl(s.fd, libzt::ZTS_F_SETFL as _, libzt::ZTS_O_NONBLOCK as _)
        } != 0
        {
            return Err(error("nonblocking connect", -1));
        }
        let rc = unsafe {
            libzt::zts_bsd_connect(
                s.fd,
                &a as *const _ as *const libzt::zts_sockaddr,
                std::mem::size_of_val(&a) as _,
            )
        };
        if rc != 0 {
            let e = socket_errno(rc as i64);
            let pending =
                bsd_socket_failure(rc as i64) && e == libzt::zts_errno_t_ZTS_EINPROGRESS as i32;
            #[cfg(windows)]
            let pending = pending || (bsd_socket_failure(rc as i64) && e == libc::EINPROGRESS);
            if !pending {
                return Err(error("connect", rc as _));
            }
            let deadline = Instant::now() + budget;
            loop {
                if Instant::now() >= deadline {
                    return Err("connect deadline exceeded".into());
                }
                let mut poll = libzt::zts_pollfd {
                    fd: s.fd,
                    events: libzt::ZTS_POLLOUT as _,
                    revents: 0,
                };
                let n = unsafe { libzt::zts_bsd_poll(&mut poll, 1, 100) };
                if n < 0 {
                    if sdk_retryable_status(n as i64, socket_errno(n as i64)) {
                        continue;
                    }
                    return Err(error("connect poll", n as _));
                }
                if n == 0 {
                    continue;
                }
                let mut result = 0i32;
                let mut len = std::mem::size_of_val(&result) as _;
                if unsafe {
                    libzt::zts_bsd_getsockopt(
                        s.fd,
                        libzt::ZTS_SOL_SOCKET as _,
                        libzt::ZTS_SO_ERROR as _,
                        &mut result as *mut _ as *mut c_void,
                        &mut len,
                    )
                } != 0
                    || result != 0
                    || poll.revents
                        & (libzt::ZTS_POLLERR | libzt::ZTS_POLLHUP | libzt::ZTS_POLLNVAL) as i16
                        != 0
                {
                    return Err(format!("connect refused: {result}"));
                }
                break;
            }
        }
        if unsafe { libzt::zts_bsd_fcntl(s.fd, libzt::ZTS_F_SETFL as _, 0) } != 0 {
            return Err(error("restore blocking socket", -1));
        }
        Ok(s)
    }
    pub fn listen(ip: &str, port: u16) -> Result<Arc<Self>, String> {
        Self::listen_configured(ip, port, false)
    }
    /// Rebind a fully closed TCP endpoint; never enables concurrent live listeners.
    pub fn listen_reusable_tcp(ip: &str, port: u16) -> Result<Arc<Self>, String> {
        Self::listen_configured(ip, port, true)
    }
    fn listen_configured(ip: &str, port: u16, reuse: bool) -> Result<Arc<Self>, String> {
        let s = Self::new()?;
        if reuse {
            let enabled = 1i32;
            let rc = unsafe {
                libzt::zts_bsd_setsockopt(
                    s.fd,
                    libzt::ZTS_SOL_SOCKET as _,
                    libzt::ZTS_SO_REUSEADDR as _,
                    &enabled as *const _ as *const c_void,
                    std::mem::size_of_val(&enabled) as _,
                )
            };
            if rc != 0 {
                return Err(error("TCP reuse address", rc as _));
            }
        }
        let a = address(ip.parse().map_err(|_| "invalid IPv4")?, port);
        let rc = unsafe {
            libzt::zts_bsd_bind(
                s.fd,
                &a as *const _ as *const libzt::zts_sockaddr,
                std::mem::size_of_val(&a) as _,
            )
        };
        if rc != 0 {
            return Err(error("bind", rc as _));
        }
        let rc = unsafe { libzt::zts_bsd_listen(s.fd, 8) };
        if rc != 0 {
            return Err(error("listen", rc as _));
        }
        Ok(s)
    }
    pub fn accept(&self) -> Result<(Arc<Self>, String), String> {
        loop {
            if self.stopped.load(Ordering::SeqCst) {
                return Err("closed".into());
            }
            let mut a: libzt::zts_sockaddr_in = unsafe { std::mem::zeroed() };
            let mut len = std::mem::size_of_val(&a) as _;
            let fd = unsafe {
                libzt::zts_bsd_accept(
                    self.fd,
                    &mut a as *mut _ as *mut libzt::zts_sockaddr,
                    &mut len,
                )
            };
            if fd >= 0 {
                return Ok((
                    Self::from_fd(fd)?,
                    crate::network::address::decode(&a.sin_addr).to_string(),
                ));
            }
            if !sdk_retryable_status(fd as i64, socket_errno(fd as i64)) {
                return Err(error("accept", fd as _));
            }
        }
    }
    fn read_exact(&self, mut bytes: &mut [u8], deadline: Option<Instant>) -> Result<(), String> {
        while !bytes.is_empty() {
            if self.stopped.load(Ordering::SeqCst) {
                return Err("closed".into());
            }
            if deadline.is_some_and(|d| Instant::now() > d) {
                return Err("handshake timeout".into());
            }
            let (n, observed_errno) = unsafe {
                let n = libzt::zts_bsd_read(
                    self.fd,
                    bytes.as_mut_ptr() as *mut c_void,
                    bytes.len() as _,
                );
                (n, socket_errno(n as i64))
            };
            match checked_io_count(n as i64, bytes.len()).map_err(|error| error.to_string())? {
                Some(0) => return Err("EOF".into()),
                Some(count) => bytes = &mut bytes[count..],
                None if !sdk_retryable_status(n as i64, observed_errno) => {
                    return Err(format!(
                        "read: raw={n}, socketErrno={observed_errno}, errnoSource=SDK_TLS"
                    ))
                }
                None => {}
            }
        }
        Ok(())
    }
    pub fn receive(&self, deadline: Option<Instant>) -> Result<Frame, String> {
        let mut header = [0; 5];
        self.read_exact(&mut header, deadline)?;
        let len = u32::from_be_bytes(header[1..].try_into().unwrap()) as usize;
        if len > crate::network::frame::MAX_FRAME {
            return Err("oversized frame".into());
        }
        let mut data = vec![0; len];
        self.read_exact(&mut data, deadline)?;
        Ok(Frame {
            kind: header[0],
            data,
        })
    }
    pub fn send(&self, frame: Frame) -> Result<(), String> {
        let _guard = self.writer.lock();
        let bytes = frame.encode()?;
        let mut offset = 0;
        let deadline = Instant::now() + Duration::from_secs(5);
        while offset < bytes.len() {
            if self.stopped.load(Ordering::SeqCst) || Instant::now() > deadline {
                return Err("write closed/timeout".into());
            }
            let (n, observed_errno) = unsafe {
                let n = libzt::zts_bsd_write(
                    self.fd,
                    bytes[offset..].as_ptr() as *const c_void,
                    (bytes.len() - offset) as _,
                );
                (n, socket_errno(n as i64))
            };
            match checked_io_count(n as i64, bytes.len() - offset)
                .map_err(|error| error.to_string())?
            {
                Some(0) => return Err("write EOF".into()),
                Some(count) => offset += count,
                None if !sdk_retryable_status(n as i64, observed_errno) => {
                    return Err(format!(
                        "write: raw={n}, socketErrno={observed_errno}, errnoSource=SDK_TLS"
                    ))
                }
                None => {}
            }
        }
        Ok(())
    }
    pub fn close(&self) {
        // lwIP is built without LWIP_NETCONN_FULLDUPLEX. shutdown(SHUT_RDWR)
        // frees the receive mailbox even while another thread is in read(),
        // which aborts on Android (a destroyed pthread mutex). Reads/accepts
        // already have a 250ms timeout; let them observe cancellation and let
        // the last Arc close the descriptor only after every borrower exits.
        self.stopped.store(true, Ordering::SeqCst);
    }
}
pub fn retryable(e: i32) -> bool {
    // lwIP's Win32 arch/cc.h uses MSVC errno from VS 2017 onward.
    // Its EWOULDBLOCK (140) differs from the SDK's POSIX enum (11).
    // Unix lwIP still uses the SDK values, including on macOS.
    if e == libzt::zts_errno_t_ZTS_EAGAIN as i32 || e == libzt::zts_errno_t_ZTS_EINTR as i32 {
        return true;
    }
    #[cfg(windows)]
    if e == libc::EWOULDBLOCK {
        return true;
    }
    false
}

#[cfg(test)]
mod retry_tests {
    #[test]
    fn idle_timeout_keeps_all_libzt_listeners_alive() {
        assert!(super::retryable(11));
        assert!(super::retryable(4));
        #[cfg(windows)]
        assert!(super::retryable(libc::EWOULDBLOCK));
        assert!(!super::retryable(0));
        assert!(!super::retryable(22));
        assert!(!super::retryable(104));
    }
}

#[cfg(test)]
mod io_count_tests {
    #[test]
    fn sdk_wrapper_failures_never_retry_a_stale_bsd_errno() {
        assert!(super::sdk_retryable_status(-1, 11));
        assert!(!super::sdk_retryable_status(-1, 0));
        assert!(!super::sdk_retryable_status(-2, 11));
        assert!(!super::sdk_retryable_status(-3, 11));
        assert!(!super::sdk_retryable_status(0, 11));
    }
    #[test]
    fn tcp_error_diagnostic_retains_one_snapshot_and_no_arbitrary_error_text() {
        let err = super::sdk_io_error("TLS write", -1, 11, 1234);
        let detail = super::io_diagnostic(&err);
        assert!(detail.contains("socketErrno=11"));
        assert!(detail.contains("errnoSource=SDK_TLS"));
        assert!(detail.contains("raw=-1"));
        assert!(detail.contains("capacity=1234"));
        assert!(
            !super::io_diagnostic(&std::io::Error::other("sensitive third-party payload"))
                .contains("sensitive")
        );
    }
    use super::*;
    #[test]
    fn windows_ssize_negative_status_is_not_a_slice_length_or_eof() {
        for capacity in [5, 2048, 16384, 65536] {
            for raw in [-1, 0xffff_ffff, 0xffff_fffe, i64::from(i32::MIN)] {
                assert_eq!(checked_io_count_abi(raw, capacity, true).unwrap(), None);
            }
            assert_eq!(checked_io_count_abi(0, capacity, true).unwrap(), Some(0));
            assert_eq!(
                checked_io_count_abi(capacity as i64, capacity, true).unwrap(),
                Some(capacity)
            );
        }
    }
    #[test]
    fn invalid_sdk_counts_fail_before_buffer_slicing_or_write_progress() {
        for windows in [false, true] {
            for raw in [6, 2048, i64::MAX, 0x1_0000_0001] {
                assert_eq!(
                    checked_io_count_abi(raw, 5, windows).unwrap_err().kind(),
                    std::io::ErrorKind::InvalidData
                );
            }
            // Only validated progress may reach a slice/index, even when the
            // raw SDK value came from the exact observed Windows regression.
            let mut remaining = &[1u8, 2, 3, 4, 5][..];
            let raw = if windows { 0xffff_ffff } else { -1 };
            if let Some(count) = checked_io_count_abi(raw, remaining.len(), windows).unwrap() {
                remaining = &remaining[count..];
            }
            assert_eq!(remaining.len(), 5);
            let count = checked_io_count_abi(3, remaining.len(), windows)
                .unwrap()
                .unwrap();
            remaining = &remaining[count..];
            assert_eq!(remaining, &[4, 5]);
        }
    }
    #[test]
    fn posix_negative_and_positive_counts_keep_native_signed_semantics() {
        assert_eq!(checked_io_count_abi(-1, 2048, false).unwrap(), None);
        assert!(checked_io_count_abi(0xffff_ffff, 2048, false).is_err());
        for count in [0, 1, 1400, 2048] {
            assert_eq!(
                checked_io_count_abi(count, 2048, false).unwrap(),
                Some(count as usize)
            );
        }
    }
}
