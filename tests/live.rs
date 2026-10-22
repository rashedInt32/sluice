//! Live tests against the real zmx daemon: sluice runs under a pty we own, and we
//! check what the session daemon sees. Skipped (with a note) when zmx is absent.

use std::ffi::CString;
use std::process::Command;
use std::time::{Duration, Instant};

struct Pty {
    pid: libc::pid_t,
    master: i32,
}

fn spawn(args: &[&str], env: &[(&str, &str)]) -> Pty {
    spawn_argv(env!("CARGO_BIN_EXE_sluice"), args, env)
}

/// As `spawn`, for any program: `/bin/sh -c '... exec sluice'` runs sluice
/// under a pid chosen beforehand.
/// Every zmx a test runs, sluice's included, uses a socket directory of the
/// test's own. Sessions of the user's, or of tests running at the same
/// time, are then out of reach: a test whose window ends would otherwise
/// fall back to one of them. libtest runs each test on its own thread.
/// Short, as zmx caps socket path length.
fn zdir() -> String {
    thread_local! {
        static DIR: ZmxDir = {
            static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
            let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let dir = format!("/tmp/sit-{}-{n}", std::process::id());
            std::fs::create_dir_all(&dir).unwrap();
            ZmxDir(dir)
        };
    }
    DIR.with(|d| d.0.clone())
}

/// A test's zmx directory. When the test's thread ends, whatever it left
/// running there (a test that failed before its cleanup) is killed and the
/// directory removed.
struct ZmxDir(String);

impl Drop for ZmxDir {
    fn drop(&mut self) {
        let list = Command::new("zmx")
            .arg("list")
            .env("ZMX_DIR", &self.0)
            .output();
        if let Ok(out) = list {
            for line in String::from_utf8_lossy(&out.stdout).lines() {
                let name = line
                    .split('\t')
                    .next()
                    .and_then(|f| f.trim_start_matches(|c: char| c.is_whitespace() || c == '\u{2192}').strip_prefix("name="));
                if let Some(n) = name {
                    let _ = Command::new("zmx")
                        .args(["kill", n])
                        .env("ZMX_DIR", &self.0)
                        .output();
                }
            }
        }
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// `zmx` with this test's socket directory.
fn zmx() -> Command {
    let mut c = Command::new("zmx");
    c.env("ZMX_DIR", zdir());
    c
}

/// The sluice under test, with this test's socket directory.
fn sluice() -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_sluice"));
    c.env("ZMX_DIR", zdir());
    c
}

fn spawn_argv(exe: &str, args: &[&str], env: &[(&str, &str)]) -> Pty {
    let z = zdir();
    // zmx starts new sessions with $SHELL: a plain sh prompt keeps the tests
    // independent of the user's zsh theme.
    let mut env: Vec<(&str, &str)> = env.to_vec();
    env.push(("SHELL", "/bin/sh"));
    if !env.iter().any(|(k, _)| *k == "ZMX_DIR") {
        env.push(("ZMX_DIR", &z));
    }
    let env = &env[..];
    let mut argv: Vec<CString> = vec![CString::new(exe).unwrap()];
    argv.extend(args.iter().map(|a| CString::new(*a).unwrap()));
    let mut ptrs: Vec<*const libc::c_char> = argv.iter().map(|c| c.as_ptr()).collect();
    ptrs.push(std::ptr::null());
    let envs: Vec<(CString, CString)> = env
        .iter()
        .map(|(k, v)| (CString::new(*k).unwrap(), CString::new(*v).unwrap()))
        .collect();
    let unset = CString::new("ZMX_SESSION").unwrap();
    let mut ws = libc::winsize {
        ws_row: 24,
        ws_col: 100,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    let mut master = -1;
    let pid = unsafe {
        libc::forkpty(
            &mut master,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut ws,
        )
    };
    assert!(pid >= 0);
    if pid == 0 {
        unsafe {
            libc::unsetenv(unset.as_ptr());
            for (k, v) in &envs {
                libc::setenv(k.as_ptr(), v.as_ptr(), 1);
            }
            libc::execv(ptrs[0], ptrs.as_ptr());
            libc::_exit(127);
        }
    }
    Pty { pid, master }
}

impl Pty {
    fn send(&self, b: &[u8]) {
        let n = unsafe { libc::write(self.master, b.as_ptr() as *const _, b.len()) };
        assert_eq!(n as usize, b.len());
    }

    /// Read until `needle` shows up or `timeout` passes. Returns everything read.
    fn expect(&self, needle: &str, timeout: Duration) -> String {
        let mut acc = Vec::new();
        let end = Instant::now() + timeout;
        let mut buf = [0u8; 8192];
        while Instant::now() < end {
            let mut p = libc::pollfd {
                fd: self.master,
                events: libc::POLLIN,
                revents: 0,
            };
            let r = unsafe { libc::poll(&mut p, 1, 100) };
            if r > 0 {
                let n = unsafe { libc::read(self.master, buf.as_mut_ptr() as *mut _, buf.len()) };
                if n <= 0 {
                    break;
                }
                acc.extend_from_slice(&buf[..n as usize]);
                if String::from_utf8_lossy(&acc).contains(needle) {
                    return String::from_utf8_lossy(&acc).into_owned();
                }
            }
        }
        panic!(
            "did not see {needle:?} within {timeout:?}; got: {}",
            String::from_utf8_lossy(&acc)
        );
    }

    fn drain(&self, d: Duration) {
        let end = Instant::now() + d;
        let mut buf = [0u8; 8192];
        while Instant::now() < end {
            let mut p = libc::pollfd {
                fd: self.master,
                events: libc::POLLIN,
                revents: 0,
            };
            if unsafe { libc::poll(&mut p, 1, 50) } > 0 {
                let n = unsafe { libc::read(self.master, buf.as_mut_ptr() as *mut _, buf.len()) };
                if n <= 0 {
                    return;
                }
            }
        }
    }

    /// Read and discard whatever is pending, without blocking.
    fn drain_now(&self) {
        let mut buf = [0u8; 8192];
        loop {
            let mut p = libc::pollfd {
                fd: self.master,
                events: libc::POLLIN,
                revents: 0,
            };
            if unsafe { libc::poll(&mut p, 1, 0) } <= 0 {
                return;
            }
            let n = unsafe { libc::read(self.master, buf.as_mut_ptr() as *mut _, buf.len()) };
            if n <= 0 {
                return;
            }
        }
    }

    /// `clients=` for `name` from `zmx list`, polled until it equals `want`.
    /// Keeps draining the pty meanwhile: a tty whose master is not read blocks
    /// the writer, and with it the writer's exit.
    fn wait_clients(&self, name: &str, want: Option<u32>, timeout: Duration) {
        let end = Instant::now() + timeout;
        while Instant::now() < end {
            self.drain_now();
            if clients(name) == want {
                return;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        panic!(
            "session {name}: clients={:?}, wanted {want:?}",
            clients(name)
        );
    }

    fn wait_exit(&self, timeout: Duration) -> Option<i32> {
        let end = Instant::now() + timeout;
        while Instant::now() < end {
            self.drain_now();
            let mut st = 0;
            let r = unsafe { libc::waitpid(self.pid, &mut st, libc::WNOHANG) };
            if r == self.pid {
                return Some(if libc::WIFEXITED(st) {
                    libc::WEXITSTATUS(st)
                } else {
                    128 + libc::WTERMSIG(st)
                });
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        None
    }
}

impl Drop for Pty {
    fn drop(&mut self) {
        unsafe {
            libc::close(self.master);
            libc::kill(self.pid, libc::SIGKILL);
            let mut st = 0;
            libc::waitpid(self.pid, &mut st, 0);
        }
    }
}

fn zmx_present() -> bool {
    let ok = zmx()
        .arg("version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if !ok {
        eprintln!("zmx not installed; skipping live test");
    }
    ok
}

/// `(clients)` for a session name from `zmx list`, None if absent.
fn clients(name: &str) -> Option<u32> {
    let out = zmx().arg("list").output().unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    for line in text.lines() {
        let mut nm = None;
        let mut cl = None;
        for f in line.split('\t') {
            // the caller's own session carries a leading arrow marker
            let f = f
                .trim()
                .trim_start_matches(|c: char| !c.is_ascii_alphanumeric());
            if let Some(v) = f.strip_prefix("name=") {
                nm = Some(v.to_string());
            }
            if let Some(v) = f.strip_prefix("clients=") {
                cl = v.parse().ok();
            }
        }
        if nm.as_deref() == Some(name) {
            return cl;
        }
    }
    None
}

fn kill_session(name: &str) {
    let _ = zmx().args(["kill", name]).output();
}

/// `clients(name)` once it reads `want`, for counts zmx updates a moment
/// after the fact. Panics after 5 s.
fn wait_count(name: &str, want: Option<u32>) {
    let end = Instant::now() + Duration::from_secs(5);
    while clients(name) != want {
        assert!(Instant::now() < end, "{name}: clients={:?}, wanted {want:?}", clients(name));
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn uniq(tag: &str) -> String {
    format!("sluice-it-{tag}-{}", std::process::id())
}

#[test]
fn attach_echo_detach_leaves_session_running() {
    if !zmx_present() {
        return;
    }
    let name = uniq("a");
    let p = spawn(&[&name], &[]);
    p.expect("$", Duration::from_secs(5));
    p.send(b"echo marker-$((20+3))\r");
    p.expect("marker-23", Duration::from_secs(5));
    p.wait_clients(&name, Some(1), Duration::from_secs(3));

    p.send(b"\x01d"); // prefix + d: detach
    assert_eq!(
        p.wait_exit(Duration::from_secs(5)),
        Some(0),
        "sluice exits 0 on detach"
    );
    p.wait_clients(&name, Some(0), Duration::from_secs(3));
    kill_session(&name);
}

#[test]
fn literal_prefix_reaches_the_program() {
    if !zmx_present() {
        return;
    }
    let name = uniq("b");
    let p = spawn(&[&name], &[]);
    p.expect("$", Duration::from_secs(5));
    p.send(b"stty -echo; cat -v\r");
    p.drain(Duration::from_millis(400));
    p.send(b"\x01\x01\r");
    p.expect("^A", Duration::from_secs(5));
    // kitty-encoded prefix twice replays the prefix exactly as it arrived, so a
    // program that enabled the kitty protocol sees the key it expects.
    p.send(b"\x1b[97;5u\x1b[97;5u\r");
    let out = p.expect("^[[97;5u", Duration::from_secs(5));
    assert_eq!(
        out.matches("^[[97;5u").count(),
        1,
        "one chord, one replayed prefix: {out}"
    );
    p.send(b"\x03");
    p.send(b"\x01d");
    assert_eq!(p.wait_exit(Duration::from_secs(5)), Some(0));
    kill_session(&name);
}

#[test]
fn run_binding_switches_and_prev_switches_back() {
    if !zmx_present() {
        return;
    }
    let a = uniq("c1");
    let b = uniq("c2");
    let dir = std::env::temp_dir().join(format!("sluice-cfg-{}", std::process::id()));
    std::fs::create_dir_all(dir.join("sluice")).unwrap();
    std::fs::write(
        dir.join("sluice/config"),
        format!("bind j run printf '{a}\\n'\n"),
    )
    .unwrap();
    let xdg = dir.to_str().unwrap();

    // create session a, then detach
    let p = spawn(&[&a], &[("XDG_CONFIG_HOME", xdg)]);
    p.expect("$", Duration::from_secs(5));
    p.send(b"\x01d");
    assert_eq!(p.wait_exit(Duration::from_secs(5)), Some(0));

    // attach to b, jump to a through the run binding
    let p = spawn(&[&b], &[("XDG_CONFIG_HOME", xdg)]);
    p.expect("$", Duration::from_secs(5));
    p.wait_clients(&b, Some(1), Duration::from_secs(3));
    p.send(b"\x01j");
    p.wait_clients(&a, Some(1), Duration::from_secs(5));
    p.wait_clients(&b, Some(0), Duration::from_secs(5));
    p.drain(Duration::from_millis(300));
    p.send(b"echo back-in-$ZMX_SESSION\r");
    p.expect(&format!("back-in-{a}"), Duration::from_secs(5));

    // prefix+0 goes back to b
    p.send(b"\x010");
    p.wait_clients(&b, Some(1), Duration::from_secs(5));
    p.wait_clients(&a, Some(0), Duration::from_secs(5));

    p.send(b"\x01d");
    assert_eq!(p.wait_exit(Duration::from_secs(5)), Some(0));
    kill_session(&a);
    kill_session(&b);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn shell_exit_ends_sluice_and_session() {
    if !zmx_present() {
        return;
    }
    let name = uniq("d");
    let p = spawn(&[&name], &[]);
    p.expect("$", Duration::from_secs(5));
    p.send(b"exit\r");
    p.wait_clients(&name, None, Duration::from_secs(8));
    // With `on-end previous` sluice moves to another live session if there is
    // one, otherwise it exits. Either way the ended session is gone.
    let others = zmx().arg("list").output().unwrap();
    let any_left = String::from_utf8_lossy(&others.stdout).contains("name=");
    match p.wait_exit(Duration::from_secs(3)) {
        Some(_) => {} // no other session to move to, or it closed cleanly
        None => {
            assert!(any_left, "sluice stayed alive although no session is left");
            p.send(b"\x01d");
            assert_eq!(p.wait_exit(Duration::from_secs(5)), Some(0));
        }
    }
}

#[test]
fn window_close_detaches() {
    if !zmx_present() {
        return;
    }
    let name = uniq("e");
    let p = spawn(&[&name], &[]);
    p.expect("$", Duration::from_secs(5));
    unsafe { libc::kill(p.pid, libc::SIGHUP) };
    assert_eq!(p.wait_exit(Duration::from_secs(5)), Some(0));
    p.wait_clients(&name, Some(0), Duration::from_secs(3));
    kill_session(&name);
}

fn fzf_present() -> bool {
    let ok = Command::new("fzf")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if !ok {
        eprintln!("fzf not installed; skipping picker test");
    }
    ok
}

#[test]
fn help_overlay_and_picker_draw_then_return() {
    if !zmx_present() || !fzf_present() {
        return;
    }
    let name = uniq("f");
    let p = spawn(&[&name], &[]);
    p.expect("$", Duration::from_secs(5));

    p.send(b"\x01?");
    p.expect("prefix bindings", Duration::from_secs(5));
    p.send(b"\x1b");
    p.drain(Duration::from_millis(500));
    p.send(b"echo back-from-help\r");
    p.expect("back-from-help", Duration::from_secs(5));

    p.send(b"\x01f");
    let out = p.expect("sessions", Duration::from_secs(5));
    assert!(out.contains(&name) || p.expect(&name, Duration::from_secs(3)).contains(&name));
    p.send(b"\x1b");
    p.drain(Duration::from_millis(500));
    p.send(b"echo back-from-picker\r");
    p.expect("back-from-picker", Duration::from_secs(5));

    p.send(b"\x01d");
    assert_eq!(p.wait_exit(Duration::from_secs(5)), Some(0));
    kill_session(&name);
}

#[test]
fn resize_reaches_the_session_and_numbered_jump_works() {
    if !zmx_present() {
        return;
    }
    let a = uniq("g1");
    let b = uniq("g2");
    let p = spawn(&[&a], &[]);
    p.expect("$", Duration::from_secs(5));
    p.send(b"\x01d");
    assert_eq!(p.wait_exit(Duration::from_secs(5)), Some(0));

    let p = spawn(&[&b], &[]);
    p.expect("$", Duration::from_secs(5));

    // resize: the terminal changes the master's size and signals us
    let ws = libc::winsize {
        ws_row: 31,
        ws_col: 91,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    unsafe {
        libc::ioctl(p.master, libc::TIOCSWINSZ, &ws);
        libc::kill(p.pid, libc::SIGWINCH);
    }
    p.drain(Duration::from_millis(300));
    p.send(b"stty size\r");
    p.expect("31 91", Duration::from_secs(5));

    // numbered jump: find a's number from `sluice list`, press it
    let out = sluice()
        .arg("list")
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    let n: usize = text
        .lines()
        .find(|l| l.split('\t').nth(1) == Some(a.as_str()))
        .and_then(|l| l.split('\t').next()?.parse().ok())
        .expect("a is listed");
    if n <= 9 {
        p.send(format!("\x01{n}").as_bytes());
        p.wait_clients(&b, Some(0), Duration::from_secs(5));
        // Other tests create sessions in parallel; one created in the same
        // second with a smaller name shifts a's number between our list and the
        // press. Then the row that is now at n must be the one we landed on.
        let out = sluice()
            .arg("list")
            .output()
            .unwrap();
        let text = String::from_utf8_lossy(&out.stdout);
        let at_n = text
            .lines()
            .find(|l| l.split('\t').next() == Some(&n.to_string()))
            .and_then(|l| l.split('\t').nth(1))
            .unwrap_or("")
            .to_string();
        if at_n == a {
            p.wait_clients(&a, Some(1), Duration::from_secs(5));
        } else {
            eprintln!("session order shifted under the test; landed on {at_n}");
            p.wait_clients(&at_n, Some(1), Duration::from_secs(5));
        }
    } else {
        eprintln!("more than nine live sessions on this machine; jump not exercised");
    }

    p.send(b"\x01d");
    assert_eq!(p.wait_exit(Duration::from_secs(5)), Some(0));
    kill_session(&a);
    kill_session(&b);
}

#[test]
fn kitty_flags_are_neutralised_for_pickers_and_restored_after() {
    if !zmx_present() || !fzf_present() {
        return;
    }
    let name = uniq("k");
    let p = spawn(&[&name], &[]);
    p.expect("$", Duration::from_secs(5));
    // the program in the session enables the kitty keyboard protocol (flags=1)
    p.send(b"printf '\\033[>1u'\r");
    p.expect("\x1b[>1u", Duration::from_secs(5));
    p.drain(Duration::from_millis(300));

    // prefix+? : before fzf draws, sluice must have zeroed the flags
    p.send(b"\x01?");
    let out = p.expect("prefix bindings", Duration::from_secs(5));
    let zero = out
        .find("\x1b[=0;1u")
        .expect("neutral keyboard sequence sent");
    let fzf = out.find("prefix bindings").unwrap();
    assert!(zero < fzf, "neutral sequence precedes the picker");

    // Esc: back to the session, flags=1 re-applied before zmx repaints
    p.send(b"\x1b");
    p.expect("\x1b[=1;1u", Duration::from_secs(5));

    p.send(b"\x01d");
    assert_eq!(p.wait_exit(Duration::from_secs(5)), Some(0));
    kill_session(&name);
}

fn sluice_cmd(args: &[&str], state_dir: &str) -> std::process::Output {
    sluice()
        .args(args)
        .env("XDG_STATE_HOME", state_dir)
        .output()
        .unwrap()
}

#[test]
fn handoff_passes_the_prefix_to_the_program_while_its_pid_lives() {
    if !zmx_present() {
        return;
    }
    let name = uniq("h");
    let state = std::env::temp_dir().join(format!("sluice-state-{}", std::process::id()));
    let xdg = state.to_str().unwrap().to_string();
    let p = spawn(&[&name], &[("XDG_STATE_HOME", &xdg)]);
    p.expect("$", Duration::from_secs(5));
    p.send(b"stty -echo; cat -v\r");
    p.drain(Duration::from_millis(400));

    let me = std::process::id().to_string();
    assert!(
        sluice_cmd(&["handoff", "on", "--session", &name, "--pid", &me], &xdg)
            .status
            .success()
    );

    // prefix+d is no longer a chord: both bytes reach cat
    p.send(b"\x01d\r");
    p.expect("^Ad", Duration::from_secs(5));
    // a real detach leaves the count at 0, so waiting still catches it; an
    // instant read could catch zmx mid-update under the parallel test load
    p.wait_clients(&name, Some(1), Duration::from_secs(3));

    // a dead pid ends the handoff by itself
    sluice_cmd(
        &["handoff", "on", "--session", &name, "--pid", "1999999999"],
        &xdg,
    );
    p.send(b"\x01\x01\r"); // literal chord works again
    p.expect("^A\r\n", Duration::from_secs(5));
    let status = sluice_cmd(&["handoff", "status", "--session", &name], &xdg);
    assert_eq!(String::from_utf8_lossy(&status.stdout).trim(), "off");

    p.send(b"\x03");
    p.drain(Duration::from_millis(300));
    p.send(b"\x01d");
    assert_eq!(p.wait_exit(Duration::from_secs(5)), Some(0));
    kill_session(&name);
    let _ = std::fs::remove_dir_all(&state);
}

#[test]
fn cancelled_picker_exits_three() {
    if !zmx_present() || !fzf_present() {
        return;
    }
    // No name given. Esc on the session picker (if any sessions exist) runs
    // the `s` script, which here is `false`, so nothing gets picked.
    let dir = std::env::temp_dir().join(format!("sluice-cfg3-{}", std::process::id()));
    std::fs::create_dir_all(dir.join("sluice")).unwrap();
    std::fs::write(dir.join("sluice/config"), "bind s run false\nunbind g\n").unwrap();
    let xdg = dir.to_str().unwrap();
    // a session, so the picker opens
    let s = uniq("cp");
    let p = spawn(&[&s], &[("XDG_CONFIG_HOME", xdg)]);
    p.expect("$", Duration::from_secs(5));
    p.send(b"\x01d");
    assert_eq!(p.wait_exit(Duration::from_secs(5)), Some(0));
    let p = spawn(&[], &[("XDG_CONFIG_HOME", xdg)]);
    std::thread::sleep(Duration::from_millis(800));
    p.drain_now();
    p.send(b"\x1b");
    assert_eq!(p.wait_exit(Duration::from_secs(5)), Some(3));
    kill_session(&s);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn ctrl_backslash_reaches_the_program_instead_of_detaching() {
    if !zmx_present() {
        return;
    }
    let name = uniq("bs");
    let p = spawn(&[&name], &[]);
    p.expect("$", Duration::from_secs(5));
    // ctrl+\ is also the tty's QUIT character; raw-mode programs (nvim, loom)
    // never see it as a signal, so turn isig off for cat as well.
    p.send(b"stty -echo -isig; cat -v\r");
    p.drain(Duration::from_millis(400));
    p.send(b"\x1c\r"); // ctrl+\ : zmx's own detach key, disabled by sluice
    p.expect("^\\", Duration::from_secs(5));
    p.wait_clients(&name, Some(1), Duration::from_secs(3)); // still attached after ctrl+\
    p.send(b"\x04"); // EOF ends cat
    p.drain(Duration::from_millis(300));
    p.send(b"\x01d");
    assert_eq!(p.wait_exit(Duration::from_secs(5)), Some(0));
    kill_session(&name);
}

#[test]
fn session_end_moves_to_the_previous_session() {
    if !zmx_present() {
        return;
    }
    let a = uniq("e1");
    let b = uniq("e2");
    let dir = std::env::temp_dir().join(format!("sluice-cfg-end-{}", std::process::id()));
    std::fs::create_dir_all(dir.join("sluice")).unwrap();
    std::fs::write(
        dir.join("sluice/config"),
        format!("bind j run printf '{a}\\n'\n"),
    )
    .unwrap();
    let xdg = dir.to_str().unwrap();

    let p = spawn(&[&a], &[("XDG_CONFIG_HOME", xdg)]);
    p.expect("$", Duration::from_secs(5));
    p.send(b"\x01d");
    assert_eq!(p.wait_exit(Duration::from_secs(5)), Some(0));

    // on b, jump to a (so previous = b), then end a with `exit`
    let p = spawn(&[&b], &[("XDG_CONFIG_HOME", xdg)]);
    p.expect("$", Duration::from_secs(5));
    p.send(b"\x01j");
    p.wait_clients(&a, Some(1), Duration::from_secs(5));
    p.drain(Duration::from_millis(300));
    p.send(b"exit\r");
    p.wait_clients(&a, None, Duration::from_secs(8));
    p.wait_clients(&b, Some(1), Duration::from_secs(8));
    p.drain(Duration::from_millis(300));
    p.send(b"echo landed-$ZMX_SESSION\r");
    p.expect(&format!("landed-{b}"), Duration::from_secs(5));

    p.send(b"\x01d");
    assert_eq!(p.wait_exit(Duration::from_secs(5)), Some(0));
    kill_session(&b);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn on_end_close_exits_instead() {
    if !zmx_present() {
        return;
    }
    let name = uniq("e3");
    let dir = std::env::temp_dir().join(format!("sluice-cfg-close-{}", std::process::id()));
    std::fs::create_dir_all(dir.join("sluice")).unwrap();
    std::fs::write(dir.join("sluice/config"), "on-end close\n").unwrap();
    let p = spawn(&[&name], &[("XDG_CONFIG_HOME", dir.to_str().unwrap())]);
    p.expect("$", Duration::from_secs(5));
    p.send(b"exit\r");
    assert!(p.wait_exit(Duration::from_secs(8)).is_some());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn detach_command_reaches_the_client_in_front() {
    if !zmx_present() {
        return;
    }
    let name = uniq("dc");
    let state = std::env::temp_dir().join(format!("sluice-state-dc-{}", std::process::id()));
    let xdg = state.to_str().unwrap().to_string();
    let p = spawn(&[&name], &[("XDG_STATE_HOME", &xdg)]);
    p.expect("$", Duration::from_secs(5));
    let out = sluice_cmd(&["detach", "--session", &name], &xdg);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    // it returns only once zmx has dropped the client: the picker's reload
    // relies on that instead of a sleep
    assert_eq!(clients(&name), Some(0), "detach returned before it landed");
    assert_eq!(p.wait_exit(Duration::from_secs(5)), Some(0));
    // nobody left to detach
    let out = sluice_cmd(&["detach", "--session", &name], &xdg);
    assert_eq!(out.status.code(), Some(1));
    kill_session(&name);
    let _ = std::fs::remove_dir_all(&state);
}

#[test]
fn switch_command_moves_the_pane_from_outside() {
    if !zmx_present() {
        return;
    }
    let a = uniq("sw1");
    let b = uniq("sw2");
    let state = std::env::temp_dir().join(format!("sluice-state-sw-{}", std::process::id()));
    let xdg = state.to_str().unwrap().to_string();

    // create a, leave it running
    let p = spawn(&[&a], &[("XDG_STATE_HOME", &xdg)]);
    p.expect("$", Duration::from_secs(5));
    p.send(b"\x01d");
    assert_eq!(p.wait_exit(Duration::from_secs(5)), Some(0));

    // sit on b, then drive the pane to a from another process
    let p = spawn(&[&b], &[("XDG_STATE_HOME", &xdg)]);
    p.expect("$", Duration::from_secs(5));
    p.wait_clients(&b, Some(1), Duration::from_secs(3));

    let out = sluice_cmd(&["switch", &a, "--session", &b], &xdg);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    p.wait_clients(&a, Some(1), Duration::from_secs(5));
    p.wait_clients(&b, Some(0), Duration::from_secs(5));
    p.drain(Duration::from_millis(300));
    p.send(b"echo moved-$ZMX_SESSION\r");
    p.expect(&format!("moved-{a}"), Duration::from_secs(5));

    // prefix+0 still knows where we came from
    p.send(b"\x010");
    p.wait_clients(&b, Some(1), Duration::from_secs(5));

    // and so does `sluice switch -`
    let out = sluice_cmd(&["switch", "-", "--session", &b], &xdg);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    p.wait_clients(&a, Some(1), Duration::from_secs(5));
    p.send(b"\x010");
    p.wait_clients(&b, Some(1), Duration::from_secs(5));

    // a name nobody is running is refused, and the pane is untouched
    let out = sluice_cmd(&["switch", "no-such-session", "--session", &b], &xdg);
    assert_eq!(out.status.code(), Some(1));
    p.wait_clients(&b, Some(1), Duration::from_secs(3));

    p.send(b"\x01d");
    assert_eq!(p.wait_exit(Duration::from_secs(5)), Some(0));
    kill_session(&a);
    kill_session(&b);
    let _ = std::fs::remove_dir_all(&state);
}

#[test]
fn back_to_back_switches_are_not_lost() {
    if !zmx_present() {
        return;
    }
    let a = uniq("bb1");
    let b = uniq("bb2");
    let c = uniq("bb3");
    let state = std::env::temp_dir().join(format!("sluice-state-bb-{}", std::process::id()));
    let xdg = state.to_str().unwrap().to_string();
    for s in [&b, &c] {
        let p = spawn(&[s], &[("XDG_STATE_HOME", &xdg)]);
        p.expect("$", Duration::from_secs(5));
        p.send(b"\x01d");
        assert_eq!(p.wait_exit(Duration::from_secs(5)), Some(0));
    }
    let p = spawn(&[&a], &[("XDG_STATE_HOME", &xdg)]);
    p.expect("$", Duration::from_secs(5));
    p.wait_clients(&a, Some(1), Duration::from_secs(3));

    // Hold the client still so both requests are waiting when it wakes: a
    // single command slot keeps only the second, a queue runs both in order.
    unsafe { libc::kill(p.pid, libc::SIGSTOP) };
    let first = sluice_cmd(&["switch", &b, "--session", &a], &xdg);
    let second = sluice_cmd(&["switch", &c, "--session", &a], &xdg);
    unsafe { libc::kill(p.pid, libc::SIGCONT) };
    assert!(first.status.success() && second.status.success());
    p.wait_clients(&c, Some(1), Duration::from_secs(5));
    // prefix+0 returns to the session before the last switch: b, which only
    // holds if the first request ran too.
    p.send(b"\x010");
    p.wait_clients(&b, Some(1), Duration::from_secs(5));
    p.drain(Duration::from_millis(300));
    p.send(b"echo on-$ZMX_SESSION\r");
    p.expect(&format!("on-{b}"), Duration::from_secs(5));

    p.send(b"\x01d");
    assert_eq!(p.wait_exit(Duration::from_secs(5)), Some(0));
    for s in [&a, &b, &c] {
        kill_session(s);
    }
    let _ = std::fs::remove_dir_all(&state);
}

#[test]
fn a_run_script_can_name_the_command_a_new_session_starts() {
    if !zmx_present() {
        return;
    }
    let target = uniq("cmd1");
    let host = uniq("cmd2");
    let dir = std::env::temp_dir().join(format!("sluice-cfg-cmd-{}", std::process::id()));
    std::fs::create_dir_all(dir.join("sluice")).unwrap();
    // third field = the program the session runs when it has to be created
    std::fs::write(
        dir.join("sluice/config"),
        format!("bind j run printf '{target}\\t/tmp\\tsh -c echo-marker-then-wait\\n'\n"),
    )
    .unwrap();
    // a tiny program on PATH, so the command needs no quoting
    let bin = dir.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let prog = bin.join("echo-marker-then-wait");
    std::fs::write(&prog, "#!/bin/sh\necho started-by-run-script\nsleep 30\n").unwrap();
    std::process::Command::new("chmod")
        .arg("+x")
        .arg(&prog)
        .status()
        .unwrap();
    let path = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );

    let p = spawn(
        &[&host],
        &[("XDG_CONFIG_HOME", dir.to_str().unwrap()), ("PATH", &path)],
    );
    p.expect("$", Duration::from_secs(5));
    p.send(b"\x01j");
    p.expect("started-by-run-script", Duration::from_secs(8));
    p.wait_clients(&target, Some(1), Duration::from_secs(5));

    // going away and coming back re-attaches rather than starting a second one
    p.send(b"\x010");
    p.wait_clients(&host, Some(1), Duration::from_secs(5));
    p.send(b"\x01j");
    p.wait_clients(&target, Some(1), Duration::from_secs(5));
    p.drain(Duration::from_millis(400));
    let out = zmx()
        .args(["history", &target, "--screen"])
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert_eq!(
        text.matches("started-by-run-script").count(),
        1,
        "the command ran once, not once per attach: {text}"
    );

    p.send(b"\x01d");
    assert_eq!(p.wait_exit(Duration::from_secs(5)), Some(0));
    kill_session(&target);
    kill_session(&host);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_picker_kills_a_session_and_redraws_without_it() {
    if !zmx_present() || !fzf_present() {
        return;
    }
    let doomed = uniq("kp1");
    let host = uniq("kp2");

    let p = spawn(&[&doomed], &[]);
    p.expect("$", Duration::from_secs(5));
    p.send(b"\x01d");
    assert_eq!(p.wait_exit(Duration::from_secs(5)), Some(0));

    let p = spawn(&[&host], &[]);
    p.expect("$", Duration::from_secs(5));

    p.send(b"\x01f");
    p.expect("sessions", Duration::from_secs(5));
    // search mode, so the row can be named rather than counted to
    p.send(b"i");
    p.drain(Duration::from_millis(300));
    p.send(doomed.as_bytes());
    p.drain(Duration::from_millis(500));
    p.send(b"\x18"); // ctrl-x
    let end = Instant::now() + Duration::from_secs(8);
    while Instant::now() < end && clients(&doomed).is_some() {
        p.drain_now();
        std::thread::sleep(Duration::from_millis(100));
    }
    assert_eq!(
        clients(&doomed),
        None,
        "ctrl-x killed the highlighted session"
    );

    p.send(b"\x1b"); // leave search mode
    p.drain(Duration::from_millis(300));
    p.send(b"\x1b"); // close the picker
    p.drain(Duration::from_millis(600));
    p.send(b"echo still-$ZMX_SESSION\r");
    p.expect(&format!("still-{host}"), Duration::from_secs(5));

    p.send(b"\x01d");
    assert_eq!(p.wait_exit(Duration::from_secs(5)), Some(0));
    kill_session(&host);
}

#[test]
fn killing_the_session_you_are_in_falls_back_to_another() {
    if !zmx_present() || !fzf_present() {
        return;
    }
    let other = uniq("kf1");
    let mine = uniq("kf2");
    let state = std::env::temp_dir().join(format!("sluice-state-kf-{}", std::process::id()));
    let xdg = state.to_str().unwrap().to_string();

    // `mine` has to exist before anything can switch to it.
    let p = spawn(&[&mine], &[("XDG_STATE_HOME", &xdg)]);
    p.expect("$", Duration::from_secs(5));
    p.send(b"\x01d");
    assert_eq!(p.wait_exit(Duration::from_secs(5)), Some(0));

    // Start on `other` and move to `mine`, so the pane has a previous session
    // to fall back to. Without one it would land on whatever else is live,
    // which on a shared machine is not something a test can predict.
    let p = spawn(&[&other], &[("XDG_STATE_HOME", &xdg)]);
    p.expect("$", Duration::from_secs(5));
    let out = sluice_cmd(&["switch", &mine, "--session", &other], &xdg);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    p.wait_clients(&mine, Some(1), Duration::from_secs(5));
    p.drain(Duration::from_millis(300));

    p.send(b"\x01f");
    p.expect("sessions", Duration::from_secs(5));
    p.send(b"i");
    p.drain(Duration::from_millis(300));
    p.send(mine.as_bytes());
    p.drain(Duration::from_millis(500));
    p.send(b"\x18"); // ctrl-x on the session this pane is showing
    let end = Instant::now() + Duration::from_secs(8);
    while Instant::now() < end && clients(&mine).is_some() {
        p.drain_now();
        std::thread::sleep(Duration::from_millis(100));
    }
    assert_eq!(clients(&mine), None, "the current session is gone");

    p.send(b"\x1b");
    p.drain(Duration::from_millis(300));
    p.send(b"\x1b");
    // rather than recreating an empty session under the dead name, sluice
    // lands on a live one
    p.wait_clients(&other, Some(1), Duration::from_secs(8));
    p.drain(Duration::from_millis(400));
    p.send(b"echo landed-$ZMX_SESSION\r");
    p.expect(&format!("landed-{other}"), Duration::from_secs(5));

    p.send(b"\x01d");
    assert_eq!(p.wait_exit(Duration::from_secs(5)), Some(0));
    kill_session(&other);
    let _ = std::fs::remove_dir_all(&state);
}

#[test]
fn the_session_picker_opens_on_the_current_session() {
    if !zmx_present() || !fzf_present() {
        return;
    }
    // A private zmx directory: only this test's sessions exist in it. Without
    // the fix the cursor would sit on row 1, and on a shared machine row 1 is
    // somebody's real session -- a failing test must not be able to land there.
    let zdir = std::env::temp_dir().join(format!("sluice-zdir-{}", std::process::id()));
    std::fs::create_dir_all(&zdir).unwrap();
    let z = zdir.to_str().unwrap().to_string();
    let first = uniq("pf1");
    let second = uniq("pf2");

    let p = spawn(&[&first], &[("ZMX_DIR", &z)]);
    p.expect("$", Duration::from_secs(5));
    p.send(b"\x01d");
    assert_eq!(p.wait_exit(Duration::from_secs(5)), Some(0));
    std::thread::sleep(Duration::from_millis(1100)); // a later creation second, so `second` is row 2

    let p = spawn(&[&second], &[("ZMX_DIR", &z)]);
    p.expect("$", Duration::from_secs(5));

    // open the picker and accept straight away: the cursor must already be on
    // `second`, the session this pane shows, not on `first` at the top
    p.send(b"\x01f");
    p.expect("sessions", Duration::from_secs(5));
    p.drain(Duration::from_millis(400));
    p.send(b"\r");
    p.drain(Duration::from_millis(800));
    assert_eq!(
        pane_window(&p, "pf"),
        second,
        "Enter on open keeps you where you are"
    );

    // and moving one row up from there reaches `first`
    p.send(b"\x01f");
    p.expect("sessions", Duration::from_secs(5));
    p.drain(Duration::from_millis(400));
    p.send(b"k\r");
    p.drain(Duration::from_millis(800));
    assert_eq!(pane_window(&p, "pk"), first, "k moves from the current row");

    p.send(b"\x01d");
    assert_eq!(p.wait_exit(Duration::from_secs(5)), Some(0));
    for s in [&first, &second] {
        let _ = zmx()
            .args(["kill", s])
            .env("ZMX_DIR", &z)
            .output();
    }
    let _ = std::fs::remove_dir_all(&zdir);
}

/// Name of the window the pane is showing, read from inside it.
fn pane_window(p: &Pty, tag: &str) -> String {
    p.drain(Duration::from_millis(300));
    // `TAG:` appears only in the output: the typed line holds `TAG "$...`,
    // so the shell's echo of the command can never be mistaken for it.
    p.send(format!("printf '%s:%s;\\n' {tag} \"$ZMX_SESSION\"\r").as_bytes());
    let marker = format!("{tag}:");
    let out = p.expect(&marker, Duration::from_secs(5));
    let out = if out[out.rfind(&marker).unwrap()..].contains(';') {
        out
    } else {
        out + &p.expect(";", Duration::from_secs(5))
    };
    let start = out.rfind(&marker).unwrap() + marker.len();
    let rest = &out[start..];
    rest[..rest.find(';').unwrap()].to_string()
}

#[test]
fn windows_open_cycle_and_stay_one_session() {
    if !zmx_present() || !fzf_present() {
        return;
    }
    let s = uniq("win");
    let w2 = format!("{s}+2");
    let w3 = format!("{s}+3");
    let p = spawn(&[&s], &[]);
    p.expect("$", Duration::from_secs(5));

    // prefix+c: a second and a third window, each landing on screen
    p.send(b"\x01c");
    p.wait_clients(&w2, Some(1), Duration::from_secs(5));
    p.wait_clients(&s, Some(0), Duration::from_secs(5));
    assert_eq!(pane_window(&p, "a"), w2);
    p.send(b"\x01c");
    p.wait_clients(&w3, Some(1), Duration::from_secs(5));

    // prefix+n wraps from the last window to the first, prefix+p goes back
    p.send(b"\x01n");
    p.wait_clients(&s, Some(1), Duration::from_secs(5));
    assert_eq!(pane_window(&p, "b"), s);
    p.send(b"\x01p");
    p.wait_clients(&w3, Some(1), Duration::from_secs(5));

    // the session list folds the windows into one row with a count
    let out = sluice()
        .args(["list", "--rows"])
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    let rows: Vec<&str> = text
        .lines()
        .filter(|l| l.ends_with(&format!("\t{s}")))
        .collect();
    assert_eq!(rows.len(), 1, "one row per session: {text}");
    assert!(rows[0].contains("3w"), "with its window count: {}", rows[0]);
    assert!(
        !text.contains(&format!("\t{w2}")),
        "windows are not listed as sessions"
    );

    p.send(b"\x01d");
    assert_eq!(p.wait_exit(Duration::from_secs(5)), Some(0));
    let out = sluice()
        .args(["kill", &s])
        .output()
        .unwrap();
    assert!(out.status.success(), "kill takes every window");
    assert_eq!(clients(&s), None);
    assert_eq!(clients(&w2), None);
    assert_eq!(clients(&w3), None);
}

#[test]
fn closing_a_window_lands_on_its_sibling_not_another_session() {
    if !zmx_present() {
        return;
    }
    let s = uniq("wsib");
    let w2 = format!("{s}+2");
    let p = spawn(&[&s], &[]);
    p.expect("$", Duration::from_secs(5));
    p.send(b"\x01c");
    p.wait_clients(&w2, Some(1), Duration::from_secs(5));
    p.drain(Duration::from_millis(300));

    p.send(b"exit\r");
    p.wait_clients(&w2, None, Duration::from_secs(8));
    p.wait_clients(&s, Some(1), Duration::from_secs(8));
    assert_eq!(
        pane_window(&p, "c"),
        s,
        "back on window 1 of the same session"
    );

    p.send(b"\x01d");
    assert_eq!(p.wait_exit(Duration::from_secs(5)), Some(0));
    kill_session(&s);
}

#[test]
fn prefix_x_kills_only_the_window_on_screen() {
    if !zmx_present() || !fzf_present() {
        return;
    }
    let s = uniq("wkill");
    let w2 = format!("{s}+2");
    let p = spawn(&[&s], &[]);
    p.expect("$", Duration::from_secs(5));
    p.send(b"\x01c");
    p.wait_clients(&w2, Some(1), Duration::from_secs(5));

    // a bare Enter keeps the window: the first row is "no"
    p.send(b"\x01x");
    // the row, not the box label: fzf can draw the label before the rows load
    p.expect("yes, kill", Duration::from_secs(5));
    p.send(b"\r");
    p.drain(Duration::from_millis(500));
    // back on the window: one client once zmx has counted the reattach
    p.wait_clients(&w2, Some(1), Duration::from_secs(5));

    p.send(b"\x01x");
    p.expect("yes, kill", Duration::from_secs(5));
    p.send(b"j\r");
    p.wait_clients(&w2, None, Duration::from_secs(8));
    p.wait_clients(&s, Some(1), Duration::from_secs(8));
    assert_eq!(pane_window(&p, "d"), s);

    p.send(b"\x01d");
    assert_eq!(p.wait_exit(Duration::from_secs(5)), Some(0));
    kill_session(&s);
}

#[test]
fn an_armed_prefix_does_not_survive_a_switch() {
    if !zmx_present() {
        return;
    }
    let a = uniq("arm1");
    let b = uniq("arm2");
    let state = std::env::temp_dir().join(format!("sluice-state-arm-{}", std::process::id()));
    let xdg = state.to_str().unwrap().to_string();

    let p = spawn(&[&a], &[("XDG_STATE_HOME", &xdg)]);
    p.expect("$", Duration::from_secs(5));
    p.send(b"\x01d");
    assert_eq!(p.wait_exit(Duration::from_secs(5)), Some(0));

    let p = spawn(&[&b], &[("XDG_STATE_HOME", &xdg)]);
    p.expect("$", Duration::from_secs(5));
    p.wait_clients(&b, Some(1), Duration::from_secs(3));

    // arm the prefix, then move the pane from outside before the chord key
    p.send(b"\x01");
    std::thread::sleep(Duration::from_millis(200));
    let out = sluice_cmd(&["switch", &a, "--session", &b], &xdg);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    p.wait_clients(&a, Some(1), Duration::from_secs(5));
    p.drain(Duration::from_millis(300));

    // `d` is typed into the shell, not taken as prefix+d (which would detach)
    p.send(b"d");
    assert_eq!(
        p.wait_exit(Duration::from_millis(800)),
        None,
        "sluice detached"
    );
    p.send(b"\x15echo still-$ZMX_SESSION\r");
    p.expect(&format!("still-{a}"), Duration::from_secs(5));

    p.send(b"\x01d");
    assert_eq!(p.wait_exit(Duration::from_secs(5)), Some(0));
    kill_session(&a);
    kill_session(&b);
    let _ = std::fs::remove_dir_all(&state);
}

#[test]
fn title_on_shows_the_session_and_keeps_programs_from_changing_it() {
    if !zmx_present() {
        return;
    }
    let s = uniq("title");
    let cfg = std::env::temp_dir().join(format!("sluice-cfg-title-{}", std::process::id()));
    std::fs::create_dir_all(cfg.join("sluice")).unwrap();
    std::fs::write(cfg.join("sluice/config"), "title on\n").unwrap();
    let xdg = cfg.to_str().unwrap().to_string();

    let p = spawn(&[&s], &[("XDG_CONFIG_HOME", &xdg)]);
    let seen = p.expect("$", Duration::from_secs(5));
    assert!(
        seen.contains(&format!("\x1b]2;{s}\x07")),
        "no title set: {seen:?}"
    );

    // the shell sets a title of its own, then a directory report: only the
    // directory gets through, since it draws the folder icon
    p.send(b"printf '\\033]2;from-shell\\007\\033]7;file://h/tmp\\007'; echo done-$((1+1))\r");
    let seen = p.expect("done-2", Duration::from_secs(5));
    assert!(
        !seen.contains("\x1b]2;from-shell"),
        "a program's title leaked: {seen:?}"
    );
    assert!(
        seen.contains("\x1b]7;file://h/tmp\x07"),
        "OSC 7 was dropped: {seen:?}"
    );

    p.send(b"\x01d");
    assert_eq!(p.wait_exit(Duration::from_secs(5)), Some(0));
    kill_session(&s);
    let _ = std::fs::remove_dir_all(&cfg);
}

/// A test's own config and state dirs, and every session it names, killed
/// on drop so a failing test leaves nothing running.
struct Env {
    dir: std::path::PathBuf,
    cfg: String,
    state: String,
    sessions: Vec<String>,
}

impl Env {
    fn new(tag: &str, config: &str) -> Env {
        let dir = std::env::temp_dir().join(format!("sluice-env-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("cfg/sluice")).unwrap();
        std::fs::create_dir_all(dir.join("state")).unwrap();
        std::fs::write(dir.join("cfg/sluice/config"), config).unwrap();
        Env {
            cfg: dir.join("cfg").to_str().unwrap().to_string(),
            state: dir.join("state").to_str().unwrap().to_string(),
            dir,
            sessions: Vec::new(),
        }
    }
    fn session(&mut self, tag: &str) -> String {
        let s = uniq(tag);
        self.sessions.push(s.clone());
        s
    }
    fn spawn(&self, args: &[&str]) -> Pty {
        spawn(args, &[("XDG_CONFIG_HOME", &self.cfg), ("XDG_STATE_HOME", &self.state)])
    }
    /// Start `name` and detach, leaving it running.
    fn make(&self, name: &str) {
        let p = self.spawn(&[name]);
        p.expect("$", Duration::from_secs(5));
        p.send(b"\x01d");
        assert_eq!(p.wait_exit(Duration::from_secs(5)), Some(0));
        wait_count(name, Some(0));
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        for s in &self.sessions {
            kill_session(s);
            for k in 2..6 {
                kill_session(&format!("{s}+{k}"));
            }
        }
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

#[test]
fn ctrl_c_and_ctrl_backslash_in_a_run_script_reach_only_the_script() {
    if !zmx_present() {
        return;
    }
    // The script waits on read: ctrl+c / ctrl+\ end it, and sluice must
    // take the window back rather than end with it.
    let mut env = Env::new("cc", "bind j run read x\n");
    let a = env.session("cc");
    let p = env.spawn(&[&a]);
    p.expect("$", Duration::from_secs(5));
    p.wait_clients(&a, Some(1), Duration::from_secs(3));
    for key in [b"\x03", b"\x1c"] {
        p.send(b"\x01j");
        p.wait_clients(&a, Some(0), Duration::from_secs(3));
        std::thread::sleep(Duration::from_millis(300));
        p.send(key);
        p.wait_clients(&a, Some(1), Duration::from_secs(3));
        assert_eq!(p.wait_exit(Duration::from_millis(500)), None, "{key:?} ended sluice");
    }
    p.send(b"\x01d");
    assert_eq!(p.wait_exit(Duration::from_secs(5)), Some(0));
}

#[test]
fn a_new_window_opens_in_a_directory_with_a_space_in_its_name() {
    if !zmx_present() {
        return;
    }
    let mut env = Env::new("sp", "");
    let s = env.session("sp");
    let dir = env.dir.join("my dir/café");
    std::fs::create_dir_all(&dir).unwrap();
    let dir = dir.canonicalize().unwrap();
    let p = env.spawn(&[&s, "-c", dir.to_str().unwrap()]);
    p.expect("$", Duration::from_secs(5));
    p.send(b"\x01c");
    p.wait_clients(&format!("{s}+2"), Some(1), Duration::from_secs(5));
    p.drain(Duration::from_millis(300));
    p.send(b"echo \"at:$(pwd)\"\r");
    p.expect(&format!("at:{}", dir.display()), Duration::from_secs(5));
    p.send(b"\x01d");
    assert_eq!(p.wait_exit(Duration::from_secs(5)), Some(0));
}

#[test]
fn a_new_client_ignores_an_inbox_left_under_its_pid() {
    if !zmx_present() {
        return;
    }
    let mut env = Env::new("inb", "");
    let a = env.session("inb1");
    let b = env.session("inb2");
    env.make(&a);
    // A dead process with this pid left a detach queued. exec keeps the pid.
    let script = format!(
        "d=\"$XDG_STATE_HOME/sluice/inbox/$$\"; mkdir -p \"$d\" && printf detach > \"$d/1-1\" && exec {} {b}",
        env!("CARGO_BIN_EXE_sluice")
    );
    let p = spawn_argv(
        "/bin/sh",
        &["-c", &script],
        &[("XDG_CONFIG_HOME", &env.cfg), ("XDG_STATE_HOME", &env.state)],
    );
    p.expect("$", Duration::from_secs(5));
    p.wait_clients(&b, Some(1), Duration::from_secs(3));
    let out = sluice_cmd(&["switch", &a, "--session", &b], &env.state);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    p.wait_clients(&a, Some(1), Duration::from_secs(5));
    assert_eq!(p.wait_exit(Duration::from_millis(300)), None, "the old detach ran");
    p.send(b"\x01d");
    assert_eq!(p.wait_exit(Duration::from_secs(5)), Some(0));
}

#[test]
fn prefix_0_does_not_bring_back_a_killed_session() {
    if !zmx_present() {
        return;
    }
    let mut env = Env::new("pd", "");
    let a = env.session("pd1");
    let b = env.session("pd2");
    env.make(&a);
    let p = env.spawn(&[&b]);
    p.expect("$", Duration::from_secs(5));
    p.wait_clients(&b, Some(1), Duration::from_secs(3));
    let out = sluice_cmd(&["switch", &a, "--session", &b], &env.state);
    assert!(out.status.success());
    p.wait_clients(&a, Some(1), Duration::from_secs(5)); // previous is b
    kill_session(&b);
    assert_eq!(clients(&b), None);
    p.send(b"\x010");
    std::thread::sleep(Duration::from_millis(800));
    assert_eq!(clients(&b), None, "prefix+0 made the killed session anew");
    wait_count(&a, Some(1)); // the pane stays where it is
    p.send(b"\x01d");
    assert_eq!(p.wait_exit(Duration::from_secs(5)), Some(0));
}

#[test]
fn a_name_zmx_refuses_is_an_error_not_another_session() {
    if !zmx_present() {
        return;
    }
    // zmx caps a name by its socket path: 46 bytes here, ~100 anywhere.
    let mut env = Env::new("ln", "");
    let other = env.session("ln-other");
    env.make(&other);
    let long = format!("{}-{}", uniq("ln"), "x".repeat(120));
    env.sessions.push(long.clone());
    let p = env.spawn(&[&long]);
    assert_eq!(p.wait_exit(Duration::from_secs(5)), Some(1));
    // zmx's count trails a detach under load: poll, then hold steady
    wait_count(&other, Some(0));
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(clients(&other), Some(0), "landed in another session instead");
}

#[test]
fn a_new_window_zmx_refuses_says_so_and_goes_back() {
    if !zmx_present() {
        return;
    }
    // Window 1 fits zmx's limit and window 2 (`+2`) does not. The limit
    // depends on the socket directory, so find it from zmx's own message.
    let probe = zmx()
        .args(["attach", &"y".repeat(200)])
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    let msg = String::from_utf8_lossy(&probe.stderr).into_owned()
        + &String::from_utf8_lossy(&probe.stdout);
    let Some(max) = msg
        .split("max ")
        .nth(1)
        .and_then(|r| r.split_whitespace().next())
        .and_then(|n| n.parse::<usize>().ok())
    else {
        eprintln!("no cap found in {msg:?}");
        return; // this zmx does not cap names
    };
    let mut env = Env::new("lw", "");
    let mut name = uniq("lw");
    while name.len() < max - 1 {
        name.push('x');
    }
    env.sessions.push(name.clone());
    let p = env.spawn(&[&name]);
    p.expect("$", Duration::from_secs(5));
    p.send(b"\x01c");
    p.expect("zmx did not open", Duration::from_secs(5));
    // it waits for the key: not for the dead child's SIGCHLD, nor a resize
    unsafe { libc::kill(p.pid, libc::SIGWINCH) };
    std::thread::sleep(Duration::from_millis(1000));
    assert_eq!(clients(&name), Some(0), "went back without a key");
    p.send(b"x");
    p.wait_clients(&name, Some(1), Duration::from_secs(5));
    // waiting for that key, sluice still ends on SIGTERM
    p.send(b"\x01c");
    p.expect("zmx did not open", Duration::from_secs(5));
    unsafe { libc::kill(p.pid, libc::SIGTERM) };
    assert_eq!(p.wait_exit(Duration::from_secs(3)), Some(0), "SIGTERM waited for a key");
}

#[test]
fn detach_from_inside_window_1_leaves_window_2s_pane_alone() {
    if !zmx_present() {
        return;
    }
    let mut env = Env::new("dw", "");
    let x = env.session("dw");
    let x2 = format!("{x}+2");
    let t1 = env.spawn(&[&x]);
    t1.expect("$", Duration::from_secs(5));
    let t2 = env.spawn(&[&x2]);
    t2.expect("$", Duration::from_secs(5));
    t2.wait_clients(&x2, Some(1), Duration::from_secs(3));
    t1.drain(Duration::from_millis(200));
    t1.send(
        format!(
            "XDG_STATE_HOME='{}' {} detach\r",
            env.state,
            env!("CARGO_BIN_EXE_sluice")
        )
        .as_bytes(),
    );
    assert_eq!(t1.wait_exit(Duration::from_secs(5)), Some(0));
    assert_eq!(t2.wait_exit(Duration::from_millis(800)), None, "window 2's pane detached too");
    // naming the session reaches every window, as the picker's ctrl-d wants
    let out = sluice_cmd(&["detach", "--session", &x], &env.state);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(t2.wait_exit(Duration::from_secs(5)), Some(0));
}

#[test]
fn a_spelled_out_window_1_is_that_window_alone_and_a_stuck_client_fails_detach() {
    if !zmx_present() {
        return;
    }
    let mut env = Env::new("w1", "");
    let x = env.session("w1");
    let x2 = format!("{x}+2");
    let t1 = env.spawn(&[&x]);
    t1.expect("$", Duration::from_secs(5));
    let t2 = env.spawn(&[&x2]);
    t2.expect("$", Duration::from_secs(5));
    t2.wait_clients(&x2, Some(1), Duration::from_secs(3));
    // `x+1` names window 1, not the session
    let out = sluice_cmd(&["detach", "--session", &format!("{x}+1")], &env.state);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(t1.wait_exit(Duration::from_secs(5)), Some(0));
    assert_eq!(t2.wait_exit(Duration::from_millis(800)), None, "window 2 went too");
    // a client that cannot act on the request: detach says it failed
    unsafe { libc::kill(t2.pid, libc::SIGSTOP) };
    let out = sluice_cmd(&["detach", "--session", &x2], &env.state);
    unsafe { libc::kill(t2.pid, libc::SIGCONT) };
    assert_eq!(out.status.code(), Some(1), "{}", String::from_utf8_lossy(&out.stderr));
    // it detaches once it runs again
    assert_eq!(t2.wait_exit(Duration::from_secs(5)), Some(0));
}

#[test]
fn prefix_0_returns_to_another_window_of_the_same_session() {
    if !zmx_present() {
        return;
    }
    let mut env = Env::new("pw", "");
    let a = env.session("pw");
    let a2 = format!("{a}+2");
    let p = env.spawn(&[&a]);
    p.expect("$", Duration::from_secs(5));
    p.send(b"\x01c");
    p.wait_clients(&a2, Some(1), Duration::from_secs(5));
    p.send(b"\x01p");
    p.wait_clients(&a, Some(1), Duration::from_secs(5));
    // from outside, to window 2: previous is now window 1 of the same session
    let out = sluice_cmd(&["switch", &a2, "--session", &a], &env.state);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    p.wait_clients(&a2, Some(1), Duration::from_secs(5));
    p.send(b"\x010");
    p.wait_clients(&a, Some(1), Duration::from_secs(5));
    p.send(b"\x01d");
    assert_eq!(p.wait_exit(Duration::from_secs(5)), Some(0));
}

#[test]
fn the_startup_sessionizer_runs_its_command() {
    if !zmx_present() || !fzf_present() {
        return;
    }
    let mut env = Env::new("sz", "");
    let t = env.session("sz");
    // a command that shows it ran, then leaves a shell
    let prog = env.dir.join("prog");
    std::fs::write(&prog, "#!/bin/sh\necho sessionizer-ran\nexec /bin/sh\n").unwrap();
    std::fs::set_permissions(&prog, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    let script = env.dir.join("sz");
    std::fs::write(
        &script,
        format!("#!/bin/sh\nprintf '%s\\t/tmp\\t%s\\n' {t} {}\n", prog.display()),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    std::fs::write(
        format!("{}/sluice/config", env.cfg),
        format!("sessionizer {}\n", script.display()),
    )
    .unwrap();
    // a session, so the picker opens; q there goes to the sessionizer
    let other = env.session("sz-other");
    env.make(&other);
    let p = env.spawn(&[]);
    std::thread::sleep(Duration::from_millis(800));
    p.drain_now();
    p.send(b"q");
    p.expect("sessionizer-ran", Duration::from_secs(5));
    p.send(b"\x01d");
    assert_eq!(p.wait_exit(Duration::from_secs(5)), Some(0));
}

#[test]
fn alt_close_bracket_typed_does_not_swallow_the_prefix_after_it() {
    if !zmx_present() {
        return;
    }
    let mut env = Env::new("ab", "");
    let a = env.session("ab");
    let p = env.spawn(&[&a]);
    p.expect("$", Duration::from_secs(5));
    // legacy alt+] is ESC ], the start of an OSC: held, then let go once
    // no rest follows
    p.send(b"\x1b]");
    std::thread::sleep(Duration::from_millis(200));
    p.send(b"\x01d");
    assert_eq!(p.wait_exit(Duration::from_secs(5)), Some(0));
}

#[test]
fn a_held_alt_key_settles_while_the_session_prints() {
    if !zmx_present() {
        return;
    }
    let mut env = Env::new("bz", "");
    let a = env.session("bz");
    let p = env.spawn(&[&a]);
    p.expect("$", Duration::from_secs(5));
    // output every 10 ms keeps sluice's loop waking up
    p.send(b"while :; do printf .; sleep 0.01; done &\r");
    std::thread::sleep(Duration::from_millis(300));
    p.send(b"\x1b]");
    std::thread::sleep(Duration::from_millis(200));
    p.send(b"\x01d");
    assert_eq!(p.wait_exit(Duration::from_secs(5)), Some(0));
}

/// Kitty ctrl+a cut after `ESC[9`, its rest 20 ms later, then `d`. With
/// `block`, the test stops reading sluice's output around the cut, so
/// sluice's own write blocks for ~200 ms while the rest waits on its stdin.
fn cut_prefix_detaches(tag: &str, block: bool) -> Option<i32> {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    let mut env = Env::new(tag, "");
    let a = env.session(tag);
    let p = env.spawn(&[&a]);
    p.expect("$", Duration::from_secs(5));
    p.send(b"yes\r");
    let fd = p.master;
    let draining = Arc::new(AtomicBool::new(true));
    let stop = Arc::new(AtomicBool::new(false));
    let (d, s) = (draining.clone(), stop.clone());
    let t = std::thread::spawn(move || {
        let mut buf = [0u8; 65536];
        while !s.load(Ordering::SeqCst) {
            if !d.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(1));
                continue;
            }
            let mut pf = libc::pollfd {
                fd,
                events: libc::POLLIN,
                revents: 0,
            };
            if unsafe { libc::poll(&mut pf, 1, 5) } > 0 {
                unsafe { libc::read(fd, buf.as_mut_ptr() as *mut _, buf.len()) };
            }
        }
    });
    std::thread::sleep(Duration::from_millis(500));
    p.send(b"\x1b[9");
    std::thread::sleep(Duration::from_millis(5));
    if block {
        draining.store(false, Ordering::SeqCst);
    }
    std::thread::sleep(Duration::from_millis(15));
    p.send(b"7;5ud");
    std::thread::sleep(Duration::from_millis(200));
    draining.store(true, Ordering::SeqCst);
    std::thread::sleep(Duration::from_millis(300));
    stop.store(true, Ordering::SeqCst);
    t.join().unwrap();
    p.wait_exit(Duration::from_secs(3))
}

#[test]
fn a_cut_sequence_is_not_settled_while_its_rest_waits_unread() {
    if !zmx_present() {
        return;
    }
    assert_eq!(cut_prefix_detaches("cuq", false), Some(0), "quiet");
    assert_eq!(cut_prefix_detaches("cub", true), Some(0), "output blocked");
}

#[test]
fn the_window_picker_shows_what_runs_jumps_and_kills_one_window() {
    if !zmx_present() || !fzf_present() {
        return;
    }
    let s = uniq("wpick");
    let w2 = format!("{s}+2");
    let w3 = format!("{s}+3");
    let p = spawn(&[&s], &[]);
    p.expect("$", Duration::from_secs(5));
    p.send(b"\x01c");
    p.wait_clients(&w2, Some(1), Duration::from_secs(5));
    p.send(b"\x01c");
    p.wait_clients(&w3, Some(1), Duration::from_secs(5));
    p.drain(Duration::from_millis(300));
    // window 3 runs something other than its shell
    p.send(b"sleep 297\r");
    p.drain(Duration::from_millis(500));

    // its row names the program, and the picker opens on window 3, so
    // Enter keeps you there
    p.send(b"\x01w");
    p.expect("windows", Duration::from_secs(5));
    p.expect("sleep 297", Duration::from_secs(5));
    p.drain(Duration::from_millis(400));
    p.send(b"\r");
    p.wait_clients(&w3, Some(1), Duration::from_secs(5));

    // a digit jumps to that row: window 1, the bare session name
    p.send(b"\x01w");
    p.expect("windows", Duration::from_secs(5));
    p.drain(Duration::from_millis(400));
    p.send(b"1");
    p.wait_clients(&s, Some(1), Duration::from_secs(5));
    p.wait_clients(&w3, Some(0), Duration::from_secs(5));
    assert_eq!(pane_window(&p, "wa"), s);
    // a window pick is not a session switch: prefix+0 has nowhere to go
    p.send(b"\x010");
    p.drain(Duration::from_millis(500));
    assert_eq!(pane_window(&p, "wz"), s, "previous left alone");

    // from window 2, ctrl-x on row 1 kills window 1 and nothing else
    p.send(b"\x01n");
    p.wait_clients(&w2, Some(1), Duration::from_secs(5));
    p.drain(Duration::from_millis(300));
    p.send(b"\x01w");
    p.expect("windows", Duration::from_secs(5));
    p.drain(Duration::from_millis(400));
    p.send(b"k");
    p.drain(Duration::from_millis(300));
    p.send(b"\x18"); // ctrl-x
    let end = Instant::now() + Duration::from_secs(8);
    while Instant::now() < end && clients(&s).is_some() {
        p.drain_now();
        std::thread::sleep(Duration::from_millis(100));
    }
    assert_eq!(clients(&s), None, "ctrl-x killed window 1");
    p.drain(Duration::from_millis(400));
    p.send(b"\x1b");
    p.wait_clients(&w2, Some(1), Duration::from_secs(5));
    assert_eq!(clients(&w3), Some(0), "the other windows live on");
    assert_eq!(pane_window(&p, "wb"), w2, "Esc goes back where you were");

    p.send(b"\x01d");
    assert_eq!(p.wait_exit(Duration::from_secs(5)), Some(0));
    kill_session(&s);
    kill_session(&w2);
    kill_session(&w3);
}

#[test]
fn a_drag_for_a_button_never_pressed_reaches_the_program_as_motion() {
    if !zmx_present() {
        return;
    }
    // Ghostty holds the right button down after its context menu takes the
    // release, and reports every pointer move as a right drag.
    let s = uniq("mouse");
    let p = spawn(&[&s], &[]);
    p.expect("$", Duration::from_secs(5));
    // cat -v shows what arrives; raw, so the reports are not line-edited
    p.send(b"stty raw -echo; cat -v\r");
    p.drain(Duration::from_millis(500));
    p.send(b"\x1b[<34;7;3M");
    p.expect("^[[<35;7;3M", Duration::from_secs(5));
    // a real drag, press first, goes through as the terminal sent it
    p.send(b"\x1b[<2;7;3M\x1b[<34;8;3M\x1b[<2;8;3m");
    p.expect("^[[<34;8;3M", Duration::from_secs(5));
    p.send(b"\x01d");
    assert_eq!(p.wait_exit(Duration::from_secs(5)), Some(0));
    kill_session(&s);
}

#[test]
fn a_background_left_set_does_not_fill_the_next_window() {
    if !zmx_present() {
        return;
    }
    // nvim and its plugins leave a background colour set. The next window's
    // repaint starts with a clear, which fills with the current background.
    let s = uniq("bce");
    let w2 = format!("{s}+2");
    let p = spawn(&[&s], &[]);
    p.expect("$", Duration::from_secs(5));
    p.send(b"printf '\\033[48;5;17mBG'; sleep 30\r");
    p.expect("\x1b[48;5;17mBG", Duration::from_secs(5));
    p.drain(Duration::from_millis(300));
    p.send(b"\x01c");
    let out = p.expect("\x1b[2J", Duration::from_secs(5));
    let clear = out.rfind("\x1b[2J").unwrap();
    let reset = out[..clear].rfind("\x1b[0m");
    let bg = out[..clear].rfind("\x1b[48;5;17m");
    assert!(
        reset.is_some() && reset > bg,
        "colours reset before the repaint's clear"
    );
    p.wait_clients(&w2, Some(1), Duration::from_secs(5));
    p.send(b"\x01d");
    assert_eq!(p.wait_exit(Duration::from_secs(5)), Some(0));
    kill_session(&s);
    kill_session(&w2);
}
