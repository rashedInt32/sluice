//! Prefix handoff: a program inside a session (loom.nvim) asks sluice to send
//! the prefix key through instead of acting on it, the way tmux's `@loom` pane
//! option did. The marker is a file per session holding the asking process's
//! pid, so a crashed program never leaves the prefix stuck in passthrough.

use std::path::PathBuf;

fn dir() -> PathBuf {
    crate::clients::base().join("sluice/handoff")
}

fn path_in(dir: &std::path::Path, session: &str) -> PathBuf {
    dir.join(crate::zmx::sanitize_name(session))
}

pub fn set(session: &str, pid: i32) -> std::io::Result<()> {
    set_in(&dir(), session, pid)
}

pub fn clear(session: &str) -> std::io::Result<()> {
    clear_in(&dir(), session)
}

/// True while the marker exists and its process is alive. A dead pid removes
/// the marker on the way out.
pub fn active(session: &str) -> bool {
    active_in(&dir(), session)
}

fn set_in(dir: &std::path::Path, session: &str, pid: i32) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    std::fs::write(path_in(dir, session), format!("{pid}\n"))
}

fn clear_in(dir: &std::path::Path, session: &str) -> std::io::Result<()> {
    match std::fs::remove_file(path_in(dir, session)) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        r => r,
    }
}

fn active_in(dir: &std::path::Path, session: &str) -> bool {
    let Ok(text) = std::fs::read_to_string(path_in(dir, session)) else {
        return false;
    };
    let Ok(pid) = text.trim().parse::<i32>() else {
        let _ = clear_in(dir, session);
        return false;
    };
    if pid_alive(pid) {
        true
    } else {
        let _ = clear_in(dir, session);
        false
    }
}

fn pid_alive(pid: i32) -> bool {
    if pid <= 0 {
        return false;
    }
    let r = unsafe { libc::kill(pid, 0) };
    r == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with_dir<T>(tag: &str, f: impl FnOnce(&std::path::Path) -> T) -> T {
        let d = std::env::temp_dir().join(format!("sluice-handoff-{tag}-{}", std::process::id()));
        let r = f(&d);
        let _ = std::fs::remove_dir_all(&d);
        r
    }

    #[test]
    fn live_pid_is_active_dead_pid_clears() {
        with_dir("a", |d| {
            set_in(d, "s1", std::process::id() as i32).unwrap();
            assert!(active_in(d, "s1"));
            set_in(d, "s2", 2_000_000_000).unwrap();
            assert!(!active_in(d, "s2"));
            assert!(!path_in(d, "s2").exists(), "stale marker removed");
            clear_in(d, "s1").unwrap();
            assert!(!active_in(d, "s1"));
            clear_in(d, "s1").unwrap(); // idempotent
        });
    }

    #[test]
    fn garbage_marker_is_removed() {
        with_dir("b", |d| {
            std::fs::create_dir_all(d).unwrap();
            std::fs::write(path_in(d, "s3"), "nope").unwrap();
            assert!(!active_in(d, "s3"));
            assert!(!path_in(d, "s3").exists());
        });
    }
}
