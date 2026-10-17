//! Telling systemd how the hub is doing, so it can restart a hub that is stuck and not only one that has died. systemd starts the hub
//! with `Type=notify` and `WatchdogSec=30`: the hub says `READY=1` once it is serving, then `WATCHDOG=1` every few seconds for as long as
//! its core is answering, and `STOPPING=1` when it is told to stop. If the pings stop (a hung core, a deadlock) systemd kills and starts
//! the hub again within the watchdog time. Without systemd (no `NOTIFY_SOCKET`) every call does nothing, and on Windows too.
//! This is the whole of the protocol, so no crate is needed.

/// Sends one state line to systemd, if it is listening.
fn send(state: &str) {
    if let Some(path) = std::env::var_os("NOTIFY_SOCKET") {
        send_to(&path.to_string_lossy(), state);
    }
}

/// Sends one state line to the datagram socket at `path` (a name starting with `@` is an abstract socket on Linux).
#[cfg(unix)]
pub fn send_to(path: &str, state: &str) {
    use std::os::unix::net::UnixDatagram;
    let Ok(sock) = UnixDatagram::unbound() else {
        return;
    };
    #[cfg(target_os = "linux")]
    if let Some(name) = path.strip_prefix('@') {
        use std::os::linux::net::SocketAddrExt;
        if let Ok(addr) = std::os::unix::net::SocketAddr::from_abstract_name(name.as_bytes()) {
            let _ = sock.send_to_addr(state.as_bytes(), &addr);
        }
        return;
    }
    let _ = sock.send_to(state.as_bytes(), path);
}

#[cfg(not(unix))]
pub fn send_to(_path: &str, _state: &str) {}

/// The hub is up and serving.
pub fn ready() {
    send("READY=1");
}

/// The hub's core just answered: still alive.
pub fn alive() {
    send("WATCHDOG=1");
}

/// The hub has been told to stop.
pub fn stopping() {
    send("STOPPING=1");
}
