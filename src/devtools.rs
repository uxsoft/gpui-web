//! The app's own DevTools connection to the browser: a pipe on fds 3 and 4
//! (Chromium's `--remote-debugging-pipe`), or a localhost websocket.

use std::fs::File;
use std::path::Path;
use std::sync::Mutex;

/// Our ends of the pipe: we write to Chromium's fd 3 and read its fd 4.
static PIPE: Mutex<Option<(File, File)>> = Mutex::new(None);
static RESERVED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Puts a pipe pair on fds 3 (Chromium reads) and 4 (Chromium writes).
#[cfg(unix)]
pub(crate) fn reserve_pipe() {
    use std::os::fd::FromRawFd;
    /// Moves `fd` out of the way of 3 and 4, which `pipe` may hand out,
    /// closed on exec.
    fn high(fd: i32) -> Option<i32> {
        let moved = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 10) };
        unsafe { libc::close(fd) };
        (moved >= 10).then_some(moved)
    }
    let mut to_chrome = [0; 2];
    let mut from_chrome = [0; 2];
    unsafe {
        if libc::pipe(to_chrome.as_mut_ptr()) != 0 {
            return;
        }
        if libc::pipe(from_chrome.as_mut_ptr()) != 0 {
            libc::close(to_chrome[0]);
            libc::close(to_chrome[1]);
            return;
        }
        let fds = [
            high(to_chrome[0]),
            high(to_chrome[1]),
            high(from_chrome[0]),
            high(from_chrome[1]),
        ];
        let [
            Some(chrome_reads),
            Some(we_write),
            Some(we_read),
            Some(chrome_writes),
        ] = fds
        else {
            for fd in fds.into_iter().flatten() {
                libc::close(fd);
            }
            return;
        };
        // dup2 leaves the new fds without close-on-exec; Chromium's
        // launcher closes them in its children anyway.
        let ok = libc::dup2(chrome_reads, 3) == 3 && libc::dup2(chrome_writes, 4) == 4;
        libc::close(chrome_reads);
        libc::close(chrome_writes);
        if !ok {
            libc::close(we_write);
            libc::close(we_read);
            return;
        }
        *PIPE.lock().unwrap_or_else(|e| e.into_inner()) =
            Some((File::from_raw_fd(we_read), File::from_raw_fd(we_write)));
    }
    RESERVED.store(true, std::sync::atomic::Ordering::Relaxed);
}

#[cfg_attr(not(feature = "cef"), allow(dead_code))]
pub(crate) fn pipe_reserved() -> bool {
    RESERVED.load(std::sync::atomic::Ordering::Relaxed)
}

/// Our ends of the DevTools pipe as `(read, write)`, once: messages are JSON
/// terminated by a NUL byte. `None` when the pipe isn't in use. The browser
/// answers once the first web view has started it.
pub fn take_devtools_pipe() -> Option<(File, File)> {
    PIPE.lock().unwrap_or_else(|e| e.into_inner()).take()
}

/// The browser's DevTools websocket when it listens on a port
/// ([`crate::RemoteDebugging::Port`]), once it has started.
pub fn devtools_url(root_cache_dir: &Path) -> Option<String> {
    let text = std::fs::read_to_string(root_cache_dir.join("DevToolsActivePort")).ok()?;
    let mut lines = text.lines();
    let port: u16 = lines.next()?.trim().parse().ok()?;
    let path = lines.next()?.trim();
    Some(format!("ws://127.0.0.1:{port}{path}"))
}
