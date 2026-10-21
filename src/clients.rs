//! Which sluice processes are showing which session, so `sluice detach` from
//! inside a session can reach the client in front of it. One file per client
//! pid under `<state>/clients/<session>/`, flock-ed by that client for as long
//! as it shows the session. The lock dies with the process, so a stale file
//! is told apart from a live client even once its pid is reused, which
//! matters because the client is sent SIGUSR1 and SIGUSR1 kills by default.
//! Commands for a client queue in `<state>/inbox/<pid>/`.

use std::os::fd::AsRawFd;
use std::path::PathBuf;

/// `$XDG_STATE_HOME`, or `~/.local/state`. The spec says to ignore a
/// relative or empty value: state would land in whatever directory sluice
/// was started from.
pub fn base() -> PathBuf {
    match std::env::var("XDG_STATE_HOME") {
        Ok(x) if x.starts_with('/') => PathBuf::from(x),
        _ => home().join(".local/state"),
    }
}

/// `$HOME`, or the user's home from the password database when it is unset,
/// empty or relative, so state never lands under the current directory.
pub fn home() -> PathBuf {
    if let Ok(h) = std::env::var("HOME") {
        if h.starts_with('/') {
            return PathBuf::from(h);
        }
    }
    let pw = unsafe { libc::getpwuid(libc::getuid()) };
    if !pw.is_null() {
        let dir = unsafe { std::ffi::CStr::from_ptr((*pw).pw_dir) };
        if let Ok(d) = dir.to_str() {
            if d.starts_with('/') {
                return PathBuf::from(d);
            }
        }
    }
    PathBuf::from("/")
}

/// A session name as a file name: `%` and `/` escaped, and a leading `.`.
/// One to one, unlike `sanitize_name`, so `a.b` and `a_b` keep apart.
pub fn file_key(session: &str) -> String {
    let mut out = String::with_capacity(session.len());
    for (i, c) in session.chars().enumerate() {
        match c {
            '%' => out.push_str("%25"),
            '/' => out.push_str("%2F"),
            '.' if i == 0 => out.push_str("%2E"),
            c => out.push(c),
        }
    }
    out
}

fn dir(base: &std::path::Path, session: &str) -> PathBuf {
    base.join("sluice/clients").join(file_key(session))
}

/// Commands queue in a per-client inbox, one file each, named so they sort in
/// the order they were sent. Keyed by pid rather than session: a client that
/// switches away still owns what was queued for it, and a second request
/// cannot overwrite the first. Its own tree, because `pids()` sweeps anything
/// in a client directory whose name is not a live pid.
fn inbox(base: &std::path::Path, pid: i32) -> PathBuf {
    base.join("sluice/inbox").join(pid.to_string())
}

/// A client's registration on one session. Holds the lock that marks it
/// live; dropping it unregisters.
pub struct Registration {
    path: PathBuf,
    _lock: std::fs::File,
}

/// The session directory stays: removing it here would race another
/// client registering there. `sweep` clears empty ones at start.
impl Drop for Registration {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

pub fn register(session: &str, pid: i32) -> Option<Registration> {
    register_in(&base(), session, pid)
}

/// Lock a file under a temporary name, then rename it into place: a reader
/// never sees the file unlocked and takes it for stale. The directory can
/// vanish under us (`sweep` at another client's start), so retry.
fn register_in(base: &std::path::Path, session: &str, pid: i32) -> Option<Registration> {
    if session.is_empty() {
        return None;
    }
    let d = dir(base, session);
    let path = d.join(pid.to_string());
    let tmp = d.join(format!(".{pid}"));
    for _ in 0..50 {
        let _ = std::fs::create_dir_all(&d);
        let Ok(f) = std::fs::File::create(&tmp) else {
            continue;
        };
        if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX) } != 0 {
            let _ = std::fs::remove_file(&tmp);
            return None;
        }
        if std::fs::rename(&tmp, &path).is_ok() {
            return Some(Registration { path, _lock: f });
        }
    }
    None
}

#[derive(PartialEq)]
enum Lock {
    /// A client holds it.
    Held,
    /// Nobody does: the file is stale.
    Free,
    /// The probe failed (no flock on this filesystem, say): no telling.
    Unknown,
}

/// Who holds the lock on `path`. Clients lock exclusive and a probe shared,
/// so two probes at once never take each other for a client.
fn probe(path: &std::path::Path) -> Lock {
    let f = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Lock::Free,
        Err(_) => return Lock::Unknown,
    };
    if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_SH | libc::LOCK_NB) } == 0 {
        return Lock::Free;
    }
    match std::io::Error::last_os_error().raw_os_error() == Some(libc::EWOULDBLOCK) {
        true => Lock::Held,
        false => Lock::Unknown,
    }
}

/// Only a held lock makes a client: a pid that is not provably one must not
/// get SIGUSR1, which kills by default.
fn held(path: &std::path::Path) -> bool {
    probe(path) == Lock::Held
}

fn pids_in(base: &std::path::Path, session: &str) -> Vec<i32> {
    if session.is_empty() {
        return Vec::new();
    }
    let d = dir(base, session);
    let Ok(rd) = std::fs::read_dir(&d) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') {
            continue; // a registration being made
        }
        match name.parse::<i32>() {
            // Only a file proven stale goes: a probe that failed may have
            // been looking at a live client.
            Ok(pid) => match probe(&e.path()) {
                Lock::Held => out.push(pid),
                Lock::Free => {
                    let _ = std::fs::remove_file(e.path());
                }
                Lock::Unknown => {}
            },
            Err(_) => {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }
    out.sort_unstable();
    out
}

/// Leave `cmd` for every sluice client showing `session`, then nudge them.
/// Returns the pids of the clients reached.
pub fn request(session: &str, cmd: &str) -> Vec<i32> {
    request_in(&base(), session, cmd)
}

/// The commands left for this process, oldest first. Consumed on read.
pub fn take_requests(pid: i32) -> Vec<String> {
    take_requests_in(&base(), pid)
}

/// Drop this process's inbox. Run once, on the way out.
pub fn close_inbox(pid: i32) {
    let _ = std::fs::remove_dir_all(inbox(&base(), pid));
}

/// Whether client `pid` is still registered on `session`. An open and a
/// lock attempt, so cheap enough to poll.
pub fn showing(session: &str, pid: i32) -> bool {
    held(&dir(&base(), session).join(pid.to_string()))
}

fn request_in(base: &std::path::Path, session: &str, cmd: &str) -> Vec<i32> {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    // Sender pid breaks a tie between two senders in the same nanosecond.
    let name = format!("{stamp:024}-{}", std::process::id());
    let mut sent = Vec::new();
    for pid in pids_in(base, session) {
        if post(&inbox(base, pid), &name, cmd).is_ok() {
            // SIGUSR1 rather than a socket: the client already has a signal
            // self-pipe, and a signal cannot be missed the way a connect to a
            // busy listener can.
            if unsafe { libc::kill(pid, libc::SIGUSR1) } == 0 {
                sent.push(pid);
            }
        }
    }
    sent
}

/// Write under a dot name, then rename: the reader skips dot names, so it
/// never sees a command half written.
fn post(dir: &std::path::Path, name: &str, cmd: &str) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let tmp = dir.join(format!(".{name}"));
    std::fs::write(&tmp, cmd)?;
    std::fs::rename(&tmp, dir.join(name))
}

fn take_requests_in(base: &std::path::Path, pid: i32) -> Vec<String> {
    let Ok(rd) = std::fs::read_dir(inbox(base, pid)) else {
        return Vec::new();
    };
    let mut files: Vec<PathBuf> = rd
        .flatten()
        .filter(|e| !e.file_name().to_string_lossy().starts_with('.'))
        .map(|e| e.path())
        .collect();
    files.sort();
    files
        .into_iter()
        .filter_map(|f| {
            let text = std::fs::read_to_string(&f).ok();
            let _ = std::fs::remove_file(&f);
            let t = text?.trim().to_string();
            (!t.is_empty()).then_some(t)
        })
        .collect()
}

/// Clear what dead clients left behind: a SIGKILLed client never
/// unregisters, and a session nobody asks about again is never swept by
/// `pids_in`. Removes only registrations nobody holds, inboxes of dead pids,
/// and empty directories, under the trees sluice owns. Run once per start.
pub fn sweep() {
    sweep_in(&base())
}

fn sweep_in(base: &std::path::Path) {
    if let Ok(sessions) = std::fs::read_dir(base.join("sluice/clients")) {
        for d in sessions.flatten() {
            let Ok(files) = std::fs::read_dir(d.path()) else {
                continue;
            };
            // A registration still being made is locked before it is
            // renamed into place; if this removes one in the instant before,
            // its rename fails and the register retries.
            for f in files.flatten() {
                if probe(&f.path()) == Lock::Free {
                    let _ = std::fs::remove_file(f.path());
                }
            }
            let _ = std::fs::remove_dir(d.path()); // only succeeds when empty
        }
    }
    // Where sluice kept commands before the inbox.
    let _ = std::fs::remove_dir_all(base.join("sluice/cmd"));
    let Ok(inboxes) = std::fs::read_dir(base.join("sluice/inbox")) else {
        return;
    };
    for d in inboxes.flatten() {
        let dead = d
            .file_name()
            .to_str()
            .and_then(|n| n.parse::<i32>().ok())
            .is_some_and(|pid| !alive(pid));
        if dead {
            let _ = std::fs::remove_dir_all(d.path());
        }
    }
}

/// A process of ours with this pid exists. One we may not signal belongs to
/// someone else, so it is not ours: EPERM counts as gone.
pub fn alive(pid: i32) -> bool {
    pid > 0 && unsafe { libc::kill(pid, 0) } == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A file left by a client that died without unregistering.
    fn stale(base: &std::path::Path, session: &str, pid: i32) {
        std::fs::create_dir_all(dir(base, session)).unwrap();
        std::fs::write(dir(base, session).join(pid.to_string()), "").unwrap();
    }

    #[test]
    fn a_request_reaches_live_clients_only() {
        // The only live "client" available here is the test process itself, and
        // the default action for SIGUSR1 is to kill it.
        unsafe { libc::signal(libc::SIGUSR1, libc::SIG_IGN) };
        let d = std::env::temp_dir().join(format!("sluice-req-{}", std::process::id()));
        let me = std::process::id() as i32;
        assert!(request_in(&d, "s", "switch other").is_empty(), "nobody attached");
        let _r = register_in(&d, "s", me).unwrap();
        stale(&d, "s", 1_999_999_999);
        assert_eq!(
            request_in(&d, "s", "switch other"),
            vec![me],
            "only the live client"
        );
        assert_eq!(take_requests_in(&d, me), vec!["switch other"]);
        assert!(take_requests_in(&d, me).is_empty(), "consumed on read");
        assert!(!inbox(&d, 1_999_999_999).exists(), "nothing for the dead");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_stale_registration_under_a_reused_pid_is_not_signalled() {
        let d = std::env::temp_dir().join(format!("sluice-reuse-{}", std::process::id()));
        // A live process that is not a sluice client, at the pid a dead
        // client left behind. SIGUSR1 would kill it.
        let mut other = std::process::Command::new("sleep").arg("10").spawn().unwrap();
        stale(&d, "s", other.id() as i32);
        assert!(request_in(&d, "s", "detach").is_empty());
        assert!(other.try_wait().unwrap().is_none(), "the bystander lives");
        assert!(!dir(&d, "s").join(other.id().to_string()).exists(), "and the file goes");
        other.kill().unwrap();
        other.wait().unwrap();
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn back_to_back_requests_all_arrive_in_order() {
        unsafe { libc::signal(libc::SIGUSR1, libc::SIG_IGN) };
        let d = std::env::temp_dir().join(format!("sluice-queue-{}", std::process::id()));
        let me = std::process::id() as i32;
        let r = register_in(&d, "a", me).unwrap();
        request_in(&d, "a", "switch b");
        request_in(&d, "a", "switch c");
        // The client moved on before reading: what was queued is still its.
        drop(r);
        let _r = register_in(&d, "b", me).unwrap();
        assert_eq!(take_requests_in(&d, me), vec!["switch b", "switch c"]);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn sweep_clears_dead_clients_and_keeps_live_ones() {
        let d = std::env::temp_dir().join(format!("sluice-sweep-{}", std::process::id()));
        let me = std::process::id() as i32;
        stale(&d, "gone", 1_999_999_999);
        let _r = register_in(&d, "here", me).unwrap();
        post(&inbox(&d, 1_999_999_999), "1", "detach").unwrap();
        post(&inbox(&d, me), "1", "detach").unwrap();
        std::fs::write(dir(&d, "here").join("notes"), "").unwrap();
        sweep_in(&d);
        assert!(!dir(&d, "gone").exists(), "dead client dir removed");
        assert!(!dir(&d, "here").join("notes").exists(), "an unlocked stray too");
        assert!(!inbox(&d, 1_999_999_999).exists(), "its stale command too");
        assert!(inbox(&d, me).exists(), "a live client keeps its inbox");
        assert_eq!(pids_in(&d, "here"), vec![me], "a live client stays");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn register_list_unregister() {
        let d = std::env::temp_dir().join(format!("sluice-clients-{}", std::process::id()));
        let me = std::process::id() as i32;
        let r = register_in(&d, "s", me).unwrap();
        stale(&d, "s", 1_999_999_999);
        assert_eq!(pids_in(&d, "s"), vec![me], "dead pid swept");
        drop(r);
        assert!(pids_in(&d, "s").is_empty());
        sweep_in(&d);
        assert!(!dir(&d, "s").exists(), "empty dir swept");
        assert!(register_in(&d, "", me).is_none(), "no empty session");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn names_that_sanitize_alike_keep_apart() {
        let d = std::env::temp_dir().join(format!("sluice-key-{}", std::process::id()));
        let me = std::process::id() as i32;
        let _r = register_in(&d, "a.b", me).unwrap();
        assert!(pids_in(&d, "a_b").is_empty());
        assert_eq!(pids_in(&d, "a.b"), vec![me]);
        assert_ne!(file_key("a/b"), file_key("a%2Fb"));
        assert_eq!(file_key(".x"), "%2Ex", "never a dot file");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn two_clients_coming_and_going_never_lose_a_registration() {
        // Each unregister used to remove the session directory when it looked
        // empty, under the other client's feet.
        let d = std::env::temp_dir().join(format!("sluice-race-{}", std::process::id()));
        let run = |d: std::path::PathBuf, pid: i32| {
            std::thread::spawn(move || {
                (0..2000)
                    .filter(|_| !register_in(&d, "s", pid).is_some_and(|r| r.path.exists()))
                    .count()
            })
        };
        let me = std::process::id() as i32;
        let (a, b) = (run(d.clone(), me), run(d.clone(), me + 1));
        assert_eq!(a.join().unwrap() + b.join().unwrap(), 0, "registrations lost");
        let _ = std::fs::remove_dir_all(&d);
    }
}
