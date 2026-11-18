//! systemd `NOTIFY_SOCKET` datagrams.
//!
//! `READY=1` is sent once, after the RPC listen socket is bound. The datagram
//! socket stays open for a later watchdog ping. An unset `NOTIFY_SOCKET` does
//! nothing, so a shell start is unchanged.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

static READY_SENT: AtomicBool = AtomicBool::new(false);
static NOTIFY_LOCK: Mutex<()> = Mutex::new(());
static HEARTBEAT_AT: Mutex<Option<Instant>> = Mutex::new(None);

/// Same 30 seconds as the main-loop shutdown poll in `blvm/src/bin/main.rs`.
const HEARTBEAT_PERIOD: Duration = Duration::from_secs(30);

#[cfg(unix)]
static SOCK: OnceLock<std::os::unix::net::UnixDatagram> = OnceLock::new();

/// Tell systemd the process can accept RPC connections.
///
/// A second call does not send another datagram. The socket opened for the
/// first send stays open.
pub fn notify_ready() {
    let Ok(name) = std::env::var("NOTIFY_SOCKET") else {
        return;
    };
    if name.is_empty() {
        return;
    }
    send_ready(&name);
}

#[cfg(unix)]
fn send_ready(name: &str) {
    let _guard = NOTIFY_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if READY_SENT.load(Ordering::Acquire) {
        return;
    }
    match send_line(name, b"READY=1\n") {
        Ok(()) => READY_SENT.store(true, Ordering::Release),
        Err(error) => {
            tracing::warn!("systemd notify READY=1 failed: {error}");
        }
    }
}

#[cfg(not(unix))]
fn send_ready(_name: &str) {}

/// Record that the tokio runtime just polled this task.
pub fn touch_runtime_heartbeat() {
    if let Ok(mut slot) = HEARTBEAT_AT.lock() {
        *slot = Some(Instant::now());
    }
}

/// Start the runtime heartbeat and the watchdog thread when systemd set
/// `WATCHDOG_USEC`. A shell start does not set it, so this returns without
/// a thread. The thread sends `WATCHDOG=1` only while the heartbeat is
/// younger than that interval.
pub fn start_runtime_watchdog() {
    #[cfg(unix)]
    {
        if watchdog_usec().is_none() {
            return;
        }
        touch_runtime_heartbeat();
        tokio::spawn(async {
            let mut tick = tokio::time::interval(HEARTBEAT_PERIOD);
            loop {
                tick.tick().await;
                touch_runtime_heartbeat();
            }
        });
        spawn_watchdog_thread();
    }
}

#[cfg(unix)]
fn watchdog_usec() -> Option<u64> {
    let raw = std::env::var("WATCHDOG_USEC").ok()?;
    let usec: u64 = raw.parse().ok()?;
    if usec == 0 { None } else { Some(usec) }
}

#[cfg(unix)]
fn heartbeat_is_fresh(limit: Duration) -> bool {
    let Ok(slot) = HEARTBEAT_AT.lock() else {
        return false;
    };
    match *slot {
        Some(at) => at.elapsed() < limit,
        None => false,
    }
}

/// Send one `WATCHDOG=1` if the runtime heartbeat is younger than
/// `WATCHDOG_USEC`. Returns whether a datagram was sent.
#[cfg(unix)]
pub fn watchdog_ping_once() -> bool {
    let Some(usec) = watchdog_usec() else {
        return false;
    };
    if !heartbeat_is_fresh(Duration::from_micros(usec)) {
        return false;
    }
    let Ok(name) = std::env::var("NOTIFY_SOCKET") else {
        return false;
    };
    if name.is_empty() {
        return false;
    }
    let _guard = NOTIFY_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    match send_line(&name, b"WATCHDOG=1\n") {
        Ok(()) => true,
        Err(error) => {
            tracing::warn!("systemd notify WATCHDOG=1 failed: {error}");
            false
        }
    }
}

#[cfg(unix)]
fn spawn_watchdog_thread() {
    let Some(usec) = watchdog_usec() else {
        return;
    };
    // Half the supervisor interval, and never a spin if the value is tiny.
    let period = Duration::from_micros(usec / 2).max(Duration::from_secs(1));
    if let Err(error) = std::thread::Builder::new()
        .name("sd-watchdog".to_string())
        .spawn(move || {
            loop {
                std::thread::sleep(period);
                let _ = watchdog_ping_once();
            }
        })
    {
        tracing::warn!("systemd watchdog thread did not start: {error}");
    }
}

#[cfg(unix)]
fn send_line(name: &str, payload: &[u8]) -> std::io::Result<()> {
    let Some(sock) = datagram() else {
        return Err(std::io::Error::other("systemd notify socket was not opened"));
    };
    if let Some(abstract_name) = name.strip_prefix('@') {
        send_abstract(sock, abstract_name, payload)
    } else {
        sock.send_to(payload, name).map(|_| ())
    }
}

#[cfg(unix)]
fn datagram() -> Option<&'static std::os::unix::net::UnixDatagram> {
    if let Some(sock) = SOCK.get() {
        return Some(sock);
    }
    match std::os::unix::net::UnixDatagram::unbound() {
        Ok(sock) => Some(SOCK.get_or_init(|| sock)),
        Err(error) => {
            tracing::warn!("systemd notify socket was not opened: {error}");
            None
        }
    }
}

#[cfg(all(unix, feature = "libc"))]
fn send_abstract(
    sock: &std::os::unix::net::UnixDatagram,
    name: &str,
    payload: &[u8],
) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;
    let bytes = name.as_bytes();
    if bytes.len() + 1 > 108 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "NOTIFY_SOCKET abstract name is longer than sockaddr_un",
        ));
    }
    let mut addr: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    addr.sun_family = libc::AF_UNIX as libc::sa_family_t;
    addr.sun_path[0] = 0;
    for (index, byte) in bytes.iter().enumerate() {
        addr.sun_path[index + 1] = *byte as libc::c_char;
    }
    let len = (std::mem::size_of::<libc::sa_family_t>() + 1 + bytes.len()) as libc::socklen_t;
    let rc = unsafe {
        libc::sendto(
            sock.as_raw_fd(),
            payload.as_ptr().cast::<libc::c_void>(),
            payload.len(),
            libc::MSG_NOSIGNAL,
            (&addr as *const libc::sockaddr_un).cast::<libc::sockaddr>(),
            len,
        )
    };
    if rc < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(all(unix, not(feature = "libc")))]
fn send_abstract(
    _sock: &std::os::unix::net::UnixDatagram,
    _name: &str,
    _payload: &[u8],
) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "abstract NOTIFY_SOCKET needs the libc feature",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::ErrorKind;
    use std::os::unix::net::UnixDatagram;
    use std::sync::MutexGuard;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn lock() -> MutexGuard<'static, ()> {
        static LOCK: Mutex<()> = Mutex::new(());
        LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    #[test]
    fn ready_is_one_datagram_and_unset_sends_nothing() {
        let _guard = lock();
        let previous = std::env::var("NOTIFY_SOCKET").ok();
        // The sent flag is process-wide. This test is the only writer.
        READY_SENT.store(false, Ordering::Release);

        let path = format!(
            "/tmp/blvm-sd-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        );
        let _ = std::fs::remove_file(&path);
        let rx = UnixDatagram::bind(&path).expect("bind");
        rx.set_nonblocking(true).expect("nonblocking");

        unsafe { std::env::remove_var("NOTIFY_SOCKET") };
        notify_ready();
        let unset = rx.recv(&mut [0u8; 64]);
        assert!(
            unset.is_err_and(|error| error.kind() == ErrorKind::WouldBlock),
            "unset NOTIFY_SOCKET must not send"
        );

        unsafe { std::env::set_var("NOTIFY_SOCKET", &path) };
        notify_ready();
        let mut buf = [0u8; 64];
        let n = rx.recv(&mut buf).expect("READY datagram");
        assert_eq!(&buf[..n], b"READY=1\n");
        assert!(SOCK.get().is_some(), "notify socket stays open");

        notify_ready();
        let second = rx.recv(&mut [0u8; 64]);
        assert!(
            second.is_err_and(|error| error.kind() == ErrorKind::WouldBlock),
            "a second ready write is ignored"
        );

        match previous {
            Some(value) => unsafe { std::env::set_var("NOTIFY_SOCKET", value) },
            None => unsafe { std::env::remove_var("NOTIFY_SOCKET") },
        }
        let _ = std::fs::remove_file(&path);
    }

    #[cfg(unix)]
    #[test]
    fn watchdog_pings_while_the_heartbeat_moves_and_stops_when_it_does_not() {
        let _guard = lock();
        let previous_socket = std::env::var("NOTIFY_SOCKET").ok();
        let previous_watchdog = std::env::var("WATCHDOG_USEC").ok();
        let path = format!(
            "/tmp/blvm-wd-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        );
        let _ = std::fs::remove_file(&path);
        let rx = UnixDatagram::bind(&path).expect("bind");
        rx.set_nonblocking(true).expect("nonblocking");

        unsafe { std::env::remove_var("WATCHDOG_USEC") };
        unsafe { std::env::set_var("NOTIFY_SOCKET", &path) };
        touch_runtime_heartbeat();
        assert!(
            !watchdog_ping_once(),
            "unset WATCHDOG_USEC must not ping"
        );

        unsafe { std::env::set_var("WATCHDOG_USEC", "5000000") };
        touch_runtime_heartbeat();
        assert!(watchdog_ping_once(), "a fresh heartbeat sends WATCHDOG=1");
        let mut buf = [0u8; 64];
        let n = rx.recv(&mut buf).expect("watchdog datagram");
        assert_eq!(&buf[..n], b"WATCHDOG=1\n");
        assert!(SOCK.get().is_some(), "notify socket stays open");

        if let Ok(mut slot) = HEARTBEAT_AT.lock() {
            *slot = Instant::now().checked_sub(Duration::from_secs(6));
        }
        assert!(
            !watchdog_ping_once(),
            "a stale heartbeat withholds the ping"
        );
        let stale = rx.recv(&mut [0u8; 64]);
        assert!(stale.is_err_and(|error| error.kind() == ErrorKind::WouldBlock));

        match previous_socket {
            Some(value) => unsafe { std::env::set_var("NOTIFY_SOCKET", value) },
            None => unsafe { std::env::remove_var("NOTIFY_SOCKET") },
        }
        match previous_watchdog {
            Some(value) => unsafe { std::env::set_var("WATCHDOG_USEC", value) },
            None => unsafe { std::env::remove_var("WATCHDOG_USEC") },
        }
        let _ = std::fs::remove_file(&path);
    }
}
