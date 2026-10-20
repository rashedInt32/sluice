//! Which sluice processes are showing which session, so `sluice detach` from
//! inside a session can reach the client in front of it. One file per client
//! pid under `<state>/clients/<session>/`; dead pids are swept on read.

use std::path::PathBuf;

pub fn base() -> PathBuf {
    match std::env::var("XDG_STATE_HOME") {
        Ok(x) => PathBuf::from(x),
        Err(_) => {
            PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into())).join(".local/state")
        }
    }
}

fn dir(base: &std::path::Path, session: &str) -> PathBuf {
    base.join("sluice/clients")
        .join(crate::zmx::sanitize_name(session))
}

/// Commands live in their own tree: `pids()` sweeps anything in the client
/// directory whose name is not a live pid, which would eat them.
fn cmd_dir(base: &std::path::Path, session: &str) -> PathBuf {
    base.join("sluice/cmd")
        .join(crate::zmx::sanitize_name(session))
}

pub fn register(session: &str, pid: i32) {
    register_in(&base(), session, pid)
}

pub fn unregister(session: &str, pid: i32) {
    unregister_in(&base(), session, pid)
}

fn register_in(base: &std::path::Path, session: &str, pid: i32) {
    let d = dir(base, session);
    if std::fs::create_dir_all(&d).is_ok() {
        let _ = std::fs::write(d.join(pid.to_string()), "");
    }
}

fn unregister_in(base: &std::path::Path, session: &str, pid: i32) {
    let d = dir(base, session);
    let _ = std::fs::remove_file(d.join(pid.to_string()));
    let _ = std::fs::remove_dir(&d); // only succeeds when empty
}

fn pids_in(base: &std::path::Path, session: &str) -> Vec<i32> {
    let d = dir(base, session);
    let Ok(rd) = std::fs::read_dir(&d) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        match name.parse::<i32>() {
            Ok(pid) if alive(pid) => out.push(pid),
            _ => {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }
    out.sort_unstable();
    out
}

/// Leave `cmd` for every sluice client showing `session`, then nudge them.
/// Returns how many clients were reached.
pub fn request(session: &str, cmd: &str) -> usize {
    request_in(&base(), session, cmd)
}

/// The command left for this process, if any. Consumed on read.
pub fn take_request(session: &str, pid: i32) -> Option<String> {
    take_request_in(&base(), session, pid)
}

pub fn clear_request(session: &str, pid: i32) {
    let d = cmd_dir(&base(), session);
    let _ = std::fs::remove_file(d.join(pid.to_string()));
    let _ = std::fs::remove_dir(&d); // only succeeds when empty
}

fn request_in(base: &std::path::Path, session: &str, cmd: &str) -> usize {
    let pids = pids_in(base, session);
    let d = cmd_dir(base, session);
    if pids.is_empty() || std::fs::create_dir_all(&d).is_err() {
        return 0;
    }
    let mut sent = 0;
    for pid in pids {
        if std::fs::write(d.join(pid.to_string()), cmd).is_ok() {
            // SIGUSR1 rather than a socket: the client already has a signal
            // self-pipe, and a signal cannot be missed the way a connect to a
            // busy listener can.
            if unsafe { libc::kill(pid, libc::SIGUSR1) } == 0 {
                sent += 1;
            }
        }
    }
    sent
}

fn take_request_in(base: &std::path::Path, session: &str, pid: i32) -> Option<String> {
    let d = cmd_dir(base, session);
    let path = d.join(pid.to_string());
    let text = std::fs::read_to_string(&path).ok()?;
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_dir(&d); // only succeeds when empty
    let t = text.trim();
    if t.is_empty() {
        None
    } else {
        Some(t.to_string())
    }
}

/// Clear what dead clients left behind: a SIGKILLed client never
/// unregisters, and a session nobody asks about again is never swept by
/// `pids_in`. Removes only files named after a dead pid, and only empty
/// directories, under the two trees sluice owns. Run once per start.
pub fn sweep() {
    sweep_in(&base())
}

fn sweep_in(base: &std::path::Path) {
    for tree in ["sluice/clients", "sluice/cmd"] {
        let Ok(sessions) = std::fs::read_dir(base.join(tree)) else {
            continue;
        };
        for d in sessions.flatten() {
            let Ok(files) = std::fs::read_dir(d.path()) else {
                continue;
            };
            for f in files.flatten() {
                let dead = f
                    .file_name()
                    .to_str()
                    .and_then(|n| n.parse::<i32>().ok())
                    .is_some_and(|pid| !alive(pid));
                if dead {
                    let _ = std::fs::remove_file(f.path());
                }
            }
            let _ = std::fs::remove_dir(d.path()); // only succeeds when empty
        }
    }
}

fn alive(pid: i32) -> bool {
    pid > 0
        && (unsafe { libc::kill(pid, 0) } == 0
            || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_request_reaches_live_clients_only() {
        // The only live "client" available here is the test process itself, and
        // the default action for SIGUSR1 is to kill it.
        unsafe { libc::signal(libc::SIGUSR1, libc::SIG_IGN) };
        let d = std::env::temp_dir().join(format!("sluice-req-{}", std::process::id()));
        let me = std::process::id() as i32;
        assert_eq!(request_in(&d, "s", "switch other"), 0, "nobody attached");
        register_in(&d, "s", me);
        register_in(&d, "s", 1_999_999_999);
        assert_eq!(
            request_in(&d, "s", "switch other"),
            1,
            "only the live client"
        );
        assert_eq!(
            take_request_in(&d, "s", me).as_deref(),
            Some("switch other")
        );
        assert_eq!(take_request_in(&d, "s", me), None, "consumed on read");
        assert!(!cmd_dir(&d, "s").exists(), "and its directory with it");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn sweep_clears_dead_clients_and_keeps_live_ones() {
        let d = std::env::temp_dir().join(format!("sluice-sweep-{}", std::process::id()));
        let me = std::process::id() as i32;
        register_in(&d, "gone", 1_999_999_999);
        register_in(&d, "here", me);
        std::fs::create_dir_all(cmd_dir(&d, "gone")).unwrap();
        std::fs::write(cmd_dir(&d, "gone").join("1999999999"), "detach").unwrap();
        std::fs::write(dir(&d, "here").join("notes"), "").unwrap();
        sweep_in(&d);
        assert!(!dir(&d, "gone").exists(), "dead client dir removed");
        assert!(!cmd_dir(&d, "gone").exists(), "its stale command too");
        assert_eq!(pids_in(&d, "here"), vec![me], "a live client stays");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn register_list_unregister_and_sweep() {
        let d = std::env::temp_dir().join(format!("sluice-clients-{}", std::process::id()));
        let me = std::process::id() as i32;
        register_in(&d, "s", me);
        register_in(&d, "s", 1_999_999_999);
        assert_eq!(pids_in(&d, "s"), vec![me], "dead pid swept");
        unregister_in(&d, "s", me);
        assert!(pids_in(&d, "s").is_empty());
        assert!(!dir(&d, "s").exists(), "empty dir removed");
        let _ = std::fs::remove_dir_all(&d);
    }
}
