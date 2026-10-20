//! Thin, audited wrappers over libc. Everything `unsafe` in sluice lives here.

use std::ffi::CString;
use std::io;
use std::os::unix::io::RawFd;
use std::sync::atomic::{AtomicI32, Ordering};

pub const STDIN: RawFd = libc::STDIN_FILENO;
pub const STDOUT: RawFd = libc::STDOUT_FILENO;

fn errno() -> io::Error {
    io::Error::last_os_error()
}

// ---------------------------------------------------------------- termios

/// Puts the controlling tty into raw mode and restores it on drop.
pub struct RawMode {
    saved: libc::termios,
    active: bool,
}

impl RawMode {
    pub fn enter() -> io::Result<RawMode> {
        let mut saved: libc::termios = unsafe { std::mem::zeroed() };
        if unsafe { libc::tcgetattr(STDIN, &mut saved) } != 0 {
            return Err(errno());
        }
        let mut raw = saved;
        unsafe { libc::cfmakeraw(&mut raw) };
        if unsafe { libc::tcsetattr(STDIN, libc::TCSANOW, &raw) } != 0 {
            return Err(errno());
        }
        Ok(RawMode {
            saved,
            active: true,
        })
    }

    /// Temporarily hand the tty back in its original (cooked) state, e.g. for fzf.
    pub fn suspend(&mut self) {
        if self.active {
            unsafe { libc::tcsetattr(STDIN, libc::TCSANOW, &self.saved) };
            self.active = false;
        }
    }

    pub fn resume(&mut self) {
        if !self.active {
            let mut raw = self.saved;
            unsafe { libc::cfmakeraw(&mut raw) };
            unsafe { libc::tcsetattr(STDIN, libc::TCSANOW, &raw) };
            self.active = true;
        }
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        self.suspend();
    }
}

// ---------------------------------------------------------------- winsize

pub fn winsize(fd: RawFd) -> libc::winsize {
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    if unsafe { libc::ioctl(fd, libc::TIOCGWINSZ, &mut ws) } != 0 || ws.ws_col == 0 {
        ws.ws_col = 80;
        ws.ws_row = 24;
    }
    ws
}

pub fn set_winsize(fd: RawFd, ws: &libc::winsize) {
    unsafe { libc::ioctl(fd, libc::TIOCSWINSZ, ws) };
}

// ---------------------------------------------------------------- pty child

pub struct Child {
    pub pid: libc::pid_t,
    pub master: RawFd,
}

/// Fork a child on a fresh pty running `argv[0]` with `argv`, optionally in `cwd`,
/// with `ZMX_SESSION` removed so zmx attaches instead of switching.
pub fn spawn_pty(argv: &[&str], cwd: Option<&str>, ws: &libc::winsize) -> io::Result<Child> {
    let nul = |_| io::Error::new(io::ErrorKind::InvalidInput, "a NUL byte in a name or path");
    let c_args: Vec<CString> = argv
        .iter()
        .map(|a| CString::new(*a))
        .collect::<Result<_, _>>()
        .map_err(nul)?;
    let mut ptrs: Vec<*const libc::c_char> = c_args.iter().map(|c| c.as_ptr()).collect();
    ptrs.push(std::ptr::null());
    let c_cwd = cwd.map(CString::new).transpose().map_err(nul)?;
    let zmx_session = CString::new("ZMX_SESSION").unwrap();
    // zmx's own ctrl+\ detach would end the client, and with it sluice and the
    // window. sluice has prefix+d; programs get ctrl+\ (loom.nvim opens on it).
    let no_detach_key = CString::new("ZMX_NO_DETACH_KEY").unwrap();
    let one = CString::new("1").unwrap();

    let mut master: RawFd = -1;
    let pid = unsafe {
        {
            let mut w = *ws;
            libc::forkpty(
                &mut master,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut w,
            )
        }
    };
    if pid < 0 {
        return Err(errno());
    }
    if pid == 0 {
        // Child: only async-signal-safe calls from here to exec.
        unsafe {
            if let Some(d) = &c_cwd {
                libc::chdir(d.as_ptr());
            }
            libc::unsetenv(zmx_session.as_ptr());
            libc::setenv(no_detach_key.as_ptr(), one.as_ptr(), 1);
            // Between fork and exec the child still carries the parent's signal
            // handlers; a SIGHUP meant to detach would be swallowed by them.
            for sig in [
                libc::SIGWINCH,
                libc::SIGCHLD,
                libc::SIGHUP,
                libc::SIGTERM,
                libc::SIGINT,
                libc::SIGUSR1,
            ] {
                libc::signal(sig, libc::SIG_DFL);
            }
            let mut set: libc::sigset_t = std::mem::zeroed();
            libc::sigemptyset(&mut set);
            libc::sigprocmask(libc::SIG_SETMASK, &set, std::ptr::null_mut());
            libc::execvp(ptrs[0], ptrs.as_ptr());
            libc::_exit(127);
        }
    }
    Ok(Child { pid, master })
}

/// Non-blocking reap. Returns `Some(exit_code)` once the child is gone.
pub fn try_wait(pid: libc::pid_t) -> Option<i32> {
    let mut status: libc::c_int = 0;
    let r = unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
    if r == pid {
        Some(exit_code(status))
    } else {
        None
    }
}

/// Blocking reap.
pub fn wait(pid: libc::pid_t) -> i32 {
    let mut status: libc::c_int = 0;
    loop {
        let r = unsafe { libc::waitpid(pid, &mut status, 0) };
        if r == pid {
            return exit_code(status);
        }
        if r < 0 && errno().raw_os_error() != Some(libc::EINTR) {
            return 1;
        }
    }
}

/// SIGHUP then reap; a child that has not gone within `grace` gets SIGKILL,
/// which zmx also treats as a detach (its socket closes).
///
/// Sleeps on the signal pipe, so the child's SIGCHLD wakes it the moment it
/// exits. Other signals read off the pipe meanwhile are put back for the
/// event loop.
pub fn hangup_and_wait(pid: libc::pid_t, grace: std::time::Duration, sig_fd: RawFd) -> i32 {
    kill(pid, libc::SIGHUP);
    let end = std::time::Instant::now() + grace;
    let mut other: Vec<libc::c_int> = Vec::new();
    let code = loop {
        if let Some(code) = try_wait(pid) {
            break Some(code);
        }
        let left = end.saturating_duration_since(std::time::Instant::now());
        if left.is_zero() {
            break None;
        }
        let mut p = libc::pollfd {
            fd: sig_fd,
            events: libc::POLLIN,
            revents: 0,
        };
        // Rounded up, so the last stretch is not a spin of zero timeouts.
        let ms = left.as_micros().div_ceil(1000) as libc::c_int;
        unsafe { libc::poll(&mut p, 1, ms) };
        for s in drain_signals(sig_fd) {
            if s != libc::SIGCHLD && !other.contains(&s) {
                other.push(s);
            }
        }
    };
    for s in other {
        on_signal(s);
    }
    code.unwrap_or_else(|| {
        kill(pid, libc::SIGKILL);
        wait(pid)
    })
}

fn exit_code(status: libc::c_int) -> i32 {
    if libc::WIFEXITED(status) {
        libc::WEXITSTATUS(status)
    } else if libc::WIFSIGNALED(status) {
        128 + libc::WTERMSIG(status)
    } else {
        1
    }
}

pub fn kill(pid: libc::pid_t, sig: libc::c_int) {
    unsafe { libc::kill(pid, sig) };
}

pub fn close(fd: RawFd) {
    unsafe { libc::close(fd) };
}

// ---------------------------------------------------------------- io

pub fn read(fd: RawFd, buf: &mut [u8]) -> io::Result<usize> {
    loop {
        let n = unsafe { libc::read(fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
        if n >= 0 {
            return Ok(n as usize);
        }
        let e = errno();
        if e.raw_os_error() != Some(libc::EINTR) {
            return Err(e);
        }
    }
}

pub fn write_all(fd: RawFd, mut buf: &[u8]) -> io::Result<()> {
    while !buf.is_empty() {
        let n = unsafe { libc::write(fd, buf.as_ptr() as *const libc::c_void, buf.len()) };
        if n >= 0 {
            buf = &buf[n as usize..];
            continue;
        }
        let e = errno();
        match e.raw_os_error() {
            Some(libc::EINTR) | Some(libc::EAGAIN) => continue,
            _ => return Err(e),
        }
    }
    Ok(())
}

/// Wait until any of `fds` is readable. Returns a bitmask of readable indexes.
/// Index bit is also set on HUP/ERR so the caller's read observes the condition.
pub fn poll_readable<const N: usize>(fds: [RawFd; N]) -> io::Result<u32> {
    let mut pfds = fds.map(|fd| libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    });
    loop {
        let r = unsafe { libc::poll(pfds.as_mut_ptr(), pfds.len() as libc::nfds_t, -1) };
        if r >= 0 {
            break;
        }
        if errno().raw_os_error() != Some(libc::EINTR) {
            return Err(errno());
        }
    }
    let mut mask = 0u32;
    for (i, p) in pfds.iter().enumerate() {
        if p.revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0 {
            mask |= 1 << i;
        }
    }
    Ok(mask)
}

// ---------------------------------------------------------------- signals

static SIG_PIPE_W: AtomicI32 = AtomicI32::new(-1);

extern "C" fn on_signal(sig: libc::c_int) {
    let fd = SIG_PIPE_W.load(Ordering::Relaxed);
    if fd >= 0 {
        let b = sig as u8;
        unsafe { libc::write(fd, &b as *const u8 as *const libc::c_void, 1) };
    }
}

/// Installs handlers for WINCH, CHLD, HUP, TERM, INT that write the signal number
/// into a self-pipe. Returns the read end for polling.
pub fn signal_pipe() -> io::Result<RawFd> {
    let mut fds = [0 as RawFd; 2];
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        return Err(errno());
    }
    for fd in fds {
        unsafe {
            libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC);
            let fl = libc::fcntl(fd, libc::F_GETFL);
            libc::fcntl(fd, libc::F_SETFL, fl | libc::O_NONBLOCK);
        }
    }
    SIG_PIPE_W.store(fds[1], Ordering::Relaxed);
    for sig in [
        libc::SIGWINCH,
        libc::SIGCHLD,
        libc::SIGHUP,
        libc::SIGTERM,
        libc::SIGINT,
        libc::SIGUSR1,
    ] {
        let mut sa: libc::sigaction = unsafe { std::mem::zeroed() };
        sa.sa_sigaction = on_signal as usize;
        sa.sa_flags = libc::SA_RESTART;
        unsafe {
            libc::sigemptyset(&mut sa.sa_mask);
            libc::sigaction(sig, &sa, std::ptr::null_mut());
        }
    }
    Ok(fds[0])
}

/// Drains the signal pipe and returns the set of signals seen.
pub fn drain_signals(fd: RawFd) -> Vec<libc::c_int> {
    let mut seen = Vec::new();
    let mut buf = [0u8; 64];
    loop {
        let n = unsafe { libc::read(fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
        if n <= 0 {
            break;
        }
        for &b in &buf[..n as usize] {
            let s = b as libc::c_int;
            if !seen.contains(&s) {
                seen.push(s);
            }
        }
    }
    seen
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[allow(clippy::zombie_processes)] // hangup_and_wait reaps it
    fn hangup_and_wait_reaps_at_once_and_keeps_other_signals() {
        let fd = signal_pipe().unwrap();
        let child = std::process::Command::new("sleep")
            .arg("10")
            .spawn()
            .unwrap();
        unsafe { libc::raise(libc::SIGWINCH) };
        let t = std::time::Instant::now();
        let code = hangup_and_wait(child.id() as i32, std::time::Duration::from_secs(5), fd);
        assert_eq!(code, 128 + libc::SIGHUP);
        assert!(
            t.elapsed() < std::time::Duration::from_millis(500),
            "{:?}",
            t.elapsed()
        );
        assert!(drain_signals(fd).contains(&libc::SIGWINCH), "put back");
    }
}
