//! What this test program itself holds, read from the operating system: open files, threads and resident memory. The capacity and leak
//! tests compare these before and after, so a leak shows as a number and not as a feeling.

/// Tests that read this process's memory take the write side; tests that hold tens of megabytes at once (big files, a device that stops
/// reading) take the read side. They run in one process, so without this a heavy test running alongside a measurement shows up as a leak.
pub static BIG_BUFFERS: tokio::sync::RwLock<()> = tokio::sync::RwLock::const_new(());

/// Open file descriptors of this process.
pub fn fds() -> usize {
    std::fs::read_dir(if cfg!(target_os = "linux") {
        "/proc/self/fd"
    } else {
        "/dev/fd"
    })
    .map_or(0, |d| d.count())
}

/// Threads of this process (0 where it cannot be told).
pub fn threads() -> usize {
    let pid = std::process::id().to_string();
    let args: &[&str] = if cfg!(target_os = "linux") {
        &["-o", "nlwp=", "-p"]
    } else {
        &["-M", "-p"]
    };
    let Ok(out) = std::process::Command::new("ps")
        .args(args)
        .arg(&pid)
        .output()
    else {
        return 0;
    };
    let text = String::from_utf8_lossy(&out.stdout);
    if cfg!(target_os = "linux") {
        text.trim().parse().unwrap_or(0)
    } else {
        text.lines().count().saturating_sub(1)
    }
}

/// Resident memory of this process in kilobytes.
pub fn rss_kb() -> usize {
    let out = std::process::Command::new("ps")
        .args(["-o", "rss=", "-p", &std::process::id().to_string()])
        .output();
    out.ok()
        .and_then(|o| String::from_utf8_lossy(&o.stdout).trim().parse().ok())
        .unwrap_or(0)
}
