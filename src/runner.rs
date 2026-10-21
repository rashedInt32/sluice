//! The event loop: one `zmx attach` child on a pty, raw bytes copied both ways,
//! prefix chords intercepted on the way in. Idle cost is one blocked poll(2).
//!
//! `current` always names one window: a zmx session. A sluice session is the
//! group of windows sharing its name (see `zmx::window_of`). Switching session
//! and switching window are the same mechanics -- detach one client, attach
//! another -- and differ only in what `previous` remembers: prefix+0 goes back
//! to the previous *session*, so moving between windows leaves it alone.

use crate::clients;
use crate::config::{Action, Config, OnEnd};
use crate::handoff;
use crate::kbd::{KittyState, NEUTRAL};
use crate::keys::{Event, Key, Scanner};
use crate::picker::{self, Choice};
use crate::sys::{self, Child, RawMode, STDIN, STDOUT};
use crate::title;
use crate::zmx::{self, Session};
use std::collections::HashMap;

const BUF: usize = 64 * 1024;

/// How long a held sequence start waits for its rest before it counts as
/// typed. The rest of a sequence a read cut follows within microseconds;
/// the next typed key takes tens of milliseconds.
const SETTLE: std::time::Duration = std::time::Duration::from_millis(50);

pub struct Runner {
    cfg: Config,
    raw: RawMode,
    sig_fd: i32,
    scanner: Scanner,
    /// the window on screen: a zmx session name
    current: String,
    /// the window we were on before the last *session* switch
    previous: Option<String>,
    /// per session, the window last shown, so returning lands where you left
    last_window: HashMap<String, String>,
    child: Option<Child>,
    /// kitty keyboard flags of the window on screen, read from its output
    kbd: KittyState,
    /// the same for windows we have shown before and may come back to
    kbd_saved: HashMap<String, KittyState>,
    /// drops the programs' own window titles, with `title on`
    titles: title::Filter,
    /// this client's entry in the registry, for the window on screen
    registration: Option<clients::Registration>,
    /// when the last read from the terminal came in, for settling a held
    /// sequence start: output and signals do not reset it
    last_input: std::time::Instant,
    /// how many times a window was attached; the first is the one asked for
    attaches: u32,
}

impl Drop for Runner {
    fn drop(&mut self) {
        clients::close_inbox(std::process::id() as i32);
    }
}

enum Flow {
    Continue,
    Exit(i32),
}

/// Run fzf once, in the background, so the first picker opens at once.
/// macOS checks a binary on its first run after a boot or an upgrade: fzf's
/// first `--version` took 965 ms, the next 16. Spawned before the first
/// attach, so it cannot inherit the pty; a thread reaps it, and the event
/// loop's SIGCHLD only ever waits on the attach client's pid.
fn warm_fzf() {
    use std::process::{Command, Stdio};
    let child = Command::new("fzf")
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    if let Ok(mut c) = child {
        std::thread::spawn(move || c.wait());
    }
}

impl Runner {
    /// `live` is a fresh `zmx list`, taken by the caller anyway.
    pub fn start(
        cfg: Config,
        name: &str,
        cwd: Option<&str>,
        cmd: Option<&[String]>,
        live: &[Session],
    ) -> std::io::Result<Runner> {
        clients::sweep();
        // An inbox under our pid is from a process that had it before us.
        clients::close_inbox(std::process::id() as i32);
        let sig_fd = sys::signal_pipe()?;
        let raw = RawMode::enter()?;
        let scanner = Scanner::new(cfg.prefix);
        warm_fzf();
        let target = Self::resolve(name, live, &HashMap::new());
        let mut r = Runner {
            cfg,
            raw,
            sig_fd,
            scanner,
            current: target.clone(),
            previous: None,
            last_window: HashMap::new(),
            child: None,
            kbd: KittyState::default(),
            kbd_saved: HashMap::new(),
            titles: title::Filter::default(),
            registration: None,
            attaches: 0,
            last_input: std::time::Instant::now(),
        };
        r.attach(&target, cwd, cmd)?;
        Ok(r)
    }

    fn group(&self) -> String {
        zmx::window_of(&self.current).0.to_string()
    }

    /// Spawn the attach client. The terminal is in the NEUTRAL keyboard state
    /// here (fresh start, or after `detach`), so re-apply what this window had.
    fn attach(
        &mut self,
        name: &str,
        cwd: Option<&str>,
        cmd: Option<&[String]>,
    ) -> std::io::Result<()> {
        self.kbd = self.kbd_saved.remove(name).unwrap_or_default();
        let restore = self.kbd.restore();
        if !restore.is_empty() {
            let _ = sys::write_all(STDOUT, &restore);
        }
        let ws = sys::winsize(STDOUT);
        // zmx only uses the command when it has to create the session, so an
        // existing one is simply re-attached.
        let mut argv: Vec<&str> = vec!["zmx", "attach", name];
        if let Some(c) = cmd {
            argv.extend(c.iter().map(String::as_str));
        }
        let child = sys::spawn_pty(&argv, cwd, &ws)?;
        self.child = Some(child);
        self.attaches += 1;
        if self.cfg.title {
            let _ = sys::write_all(STDOUT, &title::set(zmx::window_of(name).0));
        }
        self.registration = clients::register(name, std::process::id() as i32);
        self.last_window
            .insert(zmx::window_of(name).0.to_string(), name.to_string());
        Ok(())
    }

    /// SIGHUP the attach client and reap it. zmx treats a vanished client as a
    /// detach, so the session keeps running. Then put the terminal's keyboard
    /// back to neutral: zmx will re-enable the modes the next window uses,
    /// and `attach` re-applies kitty flags, which zmx does not replay.
    fn detach(&mut self) {
        self.scanner.reset();
        self.titles.reset();
        if let Some(c) = self.child.take() {
            sys::hangup_and_wait(c.pid, std::time::Duration::from_millis(500), self.sig_fd);
            sys::close(c.master);
        }
        let _ = sys::write_all(STDOUT, NEUTRAL);
        // Requests still queued stay: a switch detaches too, and a second
        // `sluice switch` sent while the first runs must not be lost.
        self.registration = None;
        let mut state = std::mem::take(&mut self.kbd);
        state.cut();
        self.kbd_saved.insert(self.current.clone(), state);
    }

    /// The attach client went away on its own. If the window is still alive
    /// someone detached us (`zmx detach`, `sluice detach`): exit. If the
    /// window ended, move on like tmux with detach-on-destroy off -- another
    /// window of the same session first, then another session -- unless the
    /// config says to close.
    fn after_child_exit(&mut self, code: i32) -> Flow {
        self.scanner.reset();
        self.titles.reset();
        let _ = sys::write_all(STDOUT, NEUTRAL);
        self.registration = None;
        let live = zmx::list();
        let alive = live.iter().any(|s| s.name == self.current);
        // zmx attach ends 0 when its window ends or is killed. Not 0 with no
        // window means zmx would not open it, a name too long for its socket
        // directory say, and its message is on screen.
        if code != 0 && !alive {
            let name = self.current.clone();
            self.forget(&name);
            if self.attaches == 1 {
                return Flow::Exit(1);
            }
            let msg = format!("\r\nsluice: zmx did not open `{name}`. Press a key.\r\n");
            let _ = sys::write_all(STDOUT, msg.as_bytes());
            if let Flow::Exit(c) = self.wait_for_key() {
                return Flow::Exit(c);
            }
            // The wait has no end: list again rather than land on a window
            // killed meanwhile, which zmx would make anew.
            return self.land_after_loss(&zmx::list(), 1);
        }
        if alive || self.cfg.on_end == OnEnd::Close {
            return Flow::Exit(code);
        }
        self.forget(&self.current.clone());
        self.land_after_loss(&live, code)
    }

    /// The window on screen is gone (exited or killed). Go to a sibling window,
    /// else to another session, else exit with `code`.
    fn land_after_loss(&mut self, live: &[Session], code: i32) -> Flow {
        if let Some(w) = Self::sibling_window(live, &self.current) {
            self.current = w.clone();
            return self.attach_or_exit(&w, None, None);
        }
        match Self::fallback_target(live, self.previous.as_deref(), &self.last_window) {
            Some(t) => {
                self.previous = None;
                self.current = t.clone();
                self.attach_or_exit(&t, None, None)
            }
            None => Flow::Exit(code),
        }
    }

    /// Wait for a key, still heeding signals: a hangup or SIGTERM ends
    /// sluice, and a `sluice switch` or `detach` (SIGUSR1) ends the wait and
    /// is handled once a window is on screen again. Others (the dead
    /// child's SIGCHLD, a resize) keep for later and the wait goes on.
    fn wait_for_key(&mut self) -> Flow {
        let mut kept: Vec<libc::c_int> = Vec::new();
        let flow = loop {
            let ready = match sys::poll_readable([STDIN, self.sig_fd], -1) {
                Ok(m) => m,
                Err(_) => break Flow::Exit(1),
            };
            if ready & 0b10 != 0 {
                let sigs = sys::drain_signals(self.sig_fd);
                let quit = [libc::SIGHUP, libc::SIGTERM, libc::SIGINT, libc::SIGQUIT];
                if sigs.iter().any(|s| quit.contains(s)) {
                    break Flow::Exit(0);
                }
                for s in sigs {
                    if !kept.contains(&s) {
                        kept.push(s);
                    }
                }
                if kept.contains(&libc::SIGUSR1) {
                    break Flow::Continue;
                }
                continue;
            }
            if ready & 0b01 != 0 {
                let mut key = [0u8; 64];
                break match sys::read(STDIN, &mut key) {
                    Ok(0) | Err(_) => Flow::Exit(0),
                    Ok(_) => Flow::Continue,
                };
            }
        };
        sys::repost_signals(&kept);
        flow
    }

    /// Drop what we remembered about a window that no longer exists.
    fn forget(&mut self, name: &str) {
        self.kbd_saved.remove(name);
        self.kbd = KittyState::default();
        let g = zmx::window_of(name).0.to_string();
        if self.last_window.get(&g).map(String::as_str) == Some(name) {
            self.last_window.remove(&g);
        }
    }

    /// Forget every window `live` no longer has. A picker can kill windows
    /// other than the one on screen, and a name comes back: prefix+c reuses
    /// the highest number, and a dead window's kitty flags must not reach
    /// the fresh shell that takes its name.
    fn prune(&mut self, live: &[Session]) {
        Self::prune_maps(&mut self.kbd_saved, &mut self.last_window, live);
    }

    // ------------------------------------------------------------ pure helpers

    fn prune_maps(
        kbd_saved: &mut HashMap<String, KittyState>,
        last_window: &mut HashMap<String, String>,
        live: &[Session],
    ) {
        let alive = |w: &str| live.iter().any(|s| s.name == w);
        kbd_saved.retain(|w, _| alive(w));
        last_window.retain(|_, w| alive(w));
    }

    /// Turn a target into the window to attach.
    ///
    /// An explicit window (`glass+2`, or `glass+1` for window 1) is taken as
    /// written. A bare session name means "that session": the window last shown
    /// there if it still exists, else its first window, else the name itself so
    /// zmx creates the session.
    fn resolve(name: &str, live: &[Session], last: &HashMap<String, String>) -> String {
        if zmx::is_window_ref(name) {
            let (g, k) = zmx::window_of(name);
            return zmx::window_name(g, k);
        }
        if let Some(w) = last
            .get(name)
            .filter(|w| live.iter().any(|s| &s.name == *w))
        {
            return w.clone();
        }
        zmx::groups(live)
            .into_iter()
            .find(|g| g.name == name)
            .and_then(|g| g.windows.into_iter().next())
            .unwrap_or_else(|| name.to_string())
    }

    /// Another window of `current`'s session to land on when `current` goes:
    /// the nearest one below it, else the nearest above.
    fn sibling_window(live: &[Session], current: &str) -> Option<String> {
        let (g, k) = zmx::window_of(current);
        let mut ws: Vec<(u32, &str)> = live
            .iter()
            .map(|s| (zmx::window_of(&s.name), s.name.as_str()))
            .filter(|((sg, sk), _)| *sg == g && *sk != k)
            .map(|((_, sk), n)| (sk, n))
            .collect();
        ws.sort_by_key(|(sk, _)| *sk);
        ws.iter()
            .rev()
            .find(|(sk, _)| *sk < k)
            .or_else(|| ws.iter().find(|(sk, _)| *sk > k))
            .map(|(_, n)| n.to_string())
    }

    /// The window `delta` steps from `current` within its session, wrapping.
    fn cycle_window(live: &[Session], current: &str, delta: i32) -> Option<String> {
        let g = zmx::window_of(current).0;
        let group = zmx::groups(live).into_iter().find(|x| x.name == g)?;
        if group.windows.len() < 2 {
            return None;
        }
        let n = group.windows.len() as i32;
        let at = group.windows.iter().position(|w| w == current).unwrap_or(0) as i32;
        let next = ((at + delta) % n + n) % n;
        Some(group.windows[next as usize].clone())
    }

    /// Name for a new window in `group`: one past the highest window number.
    fn next_window_name(live: &[Session], group: &str) -> Option<String> {
        let max = live
            .iter()
            .map(|s| zmx::window_of(&s.name))
            .filter(|(g, _)| *g == group)
            .map(|(_, k)| k)
            .max()
            .unwrap_or(0);
        (max < zmx::MAX_WINDOW).then(|| zmx::window_name(group, max + 1))
    }

    /// Where to go when the session on screen is gone: the previous window if
    /// it is still alive, else a session nobody is looking at, else the oldest.
    ///
    /// Preferring an unattached session matters on a machine with several
    /// panes open. Falling straight to the oldest would attach a second client
    /// to whatever another pane is already showing, so two panes would mirror
    /// each other for no reason the user asked for.
    fn fallback_target(
        live: &[Session],
        previous: Option<&str>,
        last: &HashMap<String, String>,
    ) -> Option<String> {
        if let Some(p) = previous.filter(|p| live.iter().any(|s| s.name == *p)) {
            return Some(p.to_string());
        }
        let groups = zmx::groups(live);
        let pick = groups
            .iter()
            .find(|g| g.clients == 0)
            .or_else(|| groups.first())?;
        Some(Self::resolve(&pick.name, live, last))
    }

    // ------------------------------------------------------------ event loop

    pub fn run(mut self) -> i32 {
        let mut buf = vec![0u8; BUF];
        loop {
            let (pid, master) = match &self.child {
                Some(c) => (c.pid, c.master),
                None => return 0,
            };
            // A held start settles SETTLE after the last read from the
            // terminal, however busy the session's output keeps the loop,
            // and only once the poll shows no more input waiting: a loop
            // stalled past SETTLE (a slow terminal) may have its rest unread.
            let timeout = match self.scanner.has_pending() {
                false => -1,
                true => {
                    let left = SETTLE.saturating_sub(self.last_input.elapsed());
                    left.as_micros().div_ceil(1000) as i32
                }
            };
            let ready = match sys::poll_readable([STDIN, master, self.sig_fd], timeout) {
                Ok(m) => m,
                Err(_) => return 1,
            };
            if self.scanner.has_pending()
                && ready & 0b001 == 0
                && self.last_input.elapsed() >= SETTLE
            {
                if let Flow::Exit(code) = self.on_settle() {
                    return code;
                }
                // A chord may have switched windows: poll the new fds.
                continue;
            }
            if ready == 0 {
                continue;
            }

            if ready & 0b100 != 0 {
                if let Flow::Exit(code) = self.on_signals() {
                    return code;
                }
                // A signal can end the child or switch to another window.
                // `ready` then describes a closed fd, whose number the new
                // pty has likely reused: poll again rather than read it.
                if self.child.as_ref().map(|c| c.pid) != Some(pid) {
                    continue;
                }
            }
            if ready & 0b010 != 0 {
                match sys::read(master, &mut buf) {
                    Ok(0) | Err(_) => {
                        // Child side closed: the attach client exited.
                        let code = self.reap_code();
                        match self.after_child_exit(code) {
                            Flow::Exit(c) => return c,
                            Flow::Continue => continue,
                        }
                    }
                    Ok(n) => {
                        self.kbd.feed(&buf[..n]);
                        let out = match self.cfg.title {
                            true => self.titles.filter(&buf[..n]),
                            false => std::borrow::Cow::Borrowed(&buf[..n]),
                        };
                        if sys::write_all(STDOUT, &out).is_err() {
                            self.detach();
                            return 1;
                        }
                    }
                }
            }
            if ready & 0b001 != 0 {
                match sys::read(STDIN, &mut buf) {
                    Ok(0) | Err(_) => {
                        self.detach();
                        return 0;
                    }
                    Ok(n) => {
                        self.last_input = std::time::Instant::now();
                        if let Flow::Exit(code) = self.on_input(&buf[..n]) {
                            return code;
                        }
                    }
                }
            }
        }
    }

    fn on_input(&mut self, chunk: &[u8]) -> Flow {
        // A program inside the session may have asked for the prefix itself
        // (loom.nvim). The marker is read only when the prefix turns up.
        let current = &self.current;
        let events = self
            .scanner
            .feed_with(chunk, || handoff::active(current));
        self.on_events(events)
    }

    /// Input went quiet with a sequence's start held: it was typed.
    fn on_settle(&mut self) -> Flow {
        let current = &self.current;
        let events = self.scanner.settle(|| handoff::active(current));
        self.on_events(events)
    }

    fn on_events(&mut self, events: Vec<Event>) -> Flow {
        for ev in events {
            match ev {
                Event::Bytes(b) => {
                    if let Some(c) = &self.child {
                        let _ = sys::write_all(c.master, &b);
                    }
                }
                Event::Chord { key, raw_prefix } => {
                    if let Flow::Exit(code) = self.on_chord(key, &raw_prefix) {
                        return Flow::Exit(code);
                    }
                }
            }
        }
        Flow::Continue
    }

    fn reap_code(&mut self) -> i32 {
        match self.child.take() {
            Some(c) => {
                let code = sys::wait(c.pid);
                sys::close(c.master);
                code
            }
            None => 0,
        }
    }

    fn on_signals(&mut self) -> Flow {
        let sigs = sys::drain_signals(self.sig_fd);
        for (i, &sig) in sigs.iter().enumerate() {
            match sig {
                libc::SIGWINCH => {
                    if let Some(c) = &self.child {
                        sys::set_winsize(c.master, &sys::winsize(STDOUT));
                    }
                }
                libc::SIGCHLD => {
                    if let Some(c) = &self.child {
                        if let Some(code) = sys::try_wait(c.pid) {
                            let m = c.master;
                            self.child = None;
                            sys::close(m);
                            // The caller polls again after a new attach; what
                            // came in with this SIGCHLD waits for that turn.
                            sys::repost_signals(&sigs[i + 1..]);
                            return self.after_child_exit(code);
                        }
                    }
                }
                libc::SIGUSR1 => {
                    if let Flow::Exit(code) = self.on_request() {
                        return Flow::Exit(code);
                    }
                }
                libc::SIGHUP | libc::SIGTERM | libc::SIGINT | libc::SIGQUIT => {
                    self.detach();
                    return Flow::Exit(0);
                }
                _ => {}
            }
        }
        Flow::Continue
    }

    /// Commands left by `sluice switch` / `sluice detach` in other processes,
    /// run in the order they were sent. Signals coalesce, so one SIGUSR1 may
    /// stand for several.
    fn on_request(&mut self) -> Flow {
        for cmd in clients::take_requests(std::process::id() as i32) {
            if let Flow::Exit(code) = self.run_request(&cmd) {
                return Flow::Exit(code);
            }
        }
        Flow::Continue
    }

    fn run_request(&mut self, cmd: &str) -> Flow {
        let (verb, arg) = match cmd.split_once(char::is_whitespace) {
            Some((v, a)) => (v, a.trim()),
            None => (cmd, ""),
        };
        match verb {
            "switch" if !arg.is_empty() => self.switch_to(Choice::named(arg)),
            "prev" => self.switch_prev(),
            "detach" => {
                self.detach();
                Flow::Exit(0)
            }
            _ => Flow::Continue,
        }
    }

    fn on_chord(&mut self, key: Key, raw_prefix: &[u8]) -> Flow {
        let Some(action) = self.cfg.binds.get(&key).cloned() else {
            return Flow::Continue; // unbound: swallowed, like tmux
        };
        match action {
            Action::Literal => {
                if let Some(c) = &self.child {
                    let _ = sys::write_all(c.master, raw_prefix);
                }
                Flow::Continue
            }
            Action::Detach => {
                self.detach();
                Flow::Exit(0)
            }
            Action::Prev => self.switch_prev(),
            Action::Jump(n) => {
                let live = zmx::list();
                match zmx::groups(&live).get(n - 1) {
                    Some(g) => self.switch_in(&live, Choice::named(g.name.clone())),
                    None => Flow::Continue,
                }
            }
            Action::NewWindow => {
                let live = zmx::list();
                let g = self.group();
                let Some(name) = Self::next_window_name(&live, &g) else {
                    return Flow::Continue;
                };
                // The new window starts where the one on screen is now.
                let dir = live
                    .iter()
                    .find(|s| s.name == self.current)
                    .map(|s| s.dir.clone())
                    .filter(|d| !d.is_empty());
                // The name may be one a killed window had; its flags are not
                // the new shell's.
                self.kbd_saved.remove(&name);
                self.switch_window(&name, dir.as_deref())
            }
            Action::NextWindow | Action::PrevWindow => {
                let delta = if action == Action::NextWindow { 1 } else { -1 };
                match Self::cycle_window(&zmx::list(), &self.current, delta) {
                    Some(w) => self.switch_window(&w, None),
                    None => Flow::Continue,
                }
            }
            Action::Pick => {
                let current = self.current.clone();
                let choice = self.with_tty(|| {
                    let live = zmx::list();
                    if live.is_empty() {
                        return None;
                    }
                    let header = "j/k move · 1-9 jump · enter switch · ctrl-x kill · ctrl-d detach · i search · q back";
                    picker::pick_session(&live, &current, header).map(Choice::named)
                });
                self.finish_switch(choice)
            }
            Action::PickWindow => {
                let current = self.current.clone();
                let pick = self.with_tty(|| {
                    let header = "j/k move · 1-9 jump · enter switch · ctrl-x kill · i search · q back";
                    picker::pick_window(&zmx::list(), &current, header)
                });
                self.finish_window(pick)
            }
            Action::Run(cmd) => {
                let choice = self.with_tty(|| picker::run_script(&cmd));
                self.finish_switch(choice)
            }
            Action::Help => {
                let lines: Vec<String> = self
                    .cfg
                    .describe()
                    .into_iter()
                    .map(|l| format!("{l}\t"))
                    .collect();
                let _ = self.with_tty(|| {
                    picker::fzf(&lines, "prefix bindings", "q/esc back", None)
                });
                self.finish_switch(None)
            }
            Action::Kill => {
                // Kills the window on screen. With one window that is the whole
                // session, same as before windows existed; the picker's ctrl-x
                // is the way to kill a session with all its windows.
                let name = self.current.clone();
                // "no" first, so a reflexive Enter keeps the window, as tmux's
                // kill-window? (y/n) does.
                let lines = vec![
                    "no\tno".to_string(),
                    format!("yes, kill window `{name}`\tyes"),
                ];
                let confirmed =
                    self.with_tty(|| picker::fzf(&lines, "kill?", "", None)) == Some("yes".into());
                if !confirmed {
                    return self.finish_switch(None);
                }
                zmx::kill(&name);
                self.forget(&name);
                let live = zmx::list();
                self.land_after_loss(&live, 0)
            }
        }
    }

    /// Detach, give the tty back in cooked mode, run `f`, take the tty again.
    /// The caller must reattach afterwards.
    fn with_tty<T>(&mut self, f: impl FnOnce() -> T) -> T {
        self.detach();
        self.raw.suspend();
        let out = f();
        self.raw.resume();
        // Out of raw mode, ctrl+c and ctrl+\ signal the whole foreground
        // group, sluice with the script. They were meant for the script.
        sys::discard_signals(self.sig_fd, &[libc::SIGINT, libc::SIGQUIT]);
        out
    }

    /// After `with_tty`: attach to the chosen session, or back to the current
    /// window.
    fn finish_switch(&mut self, choice: Option<Choice>) -> Flow {
        let live = zmx::list();
        self.prune(&live);
        if let Some(c) = choice {
            let name = zmx::typed_name(&c.name, &live);
            let target = Self::resolve(&name, &live, &self.last_window);
            if target != self.current {
                self.previous = Some(std::mem::replace(&mut self.current, target.clone()));
                return self.attach_or_exit(&target, c.cwd.as_deref(), c.cmd.as_deref());
            }
        }
        // The picker can kill sessions, including the one we came from. Going
        // back to a window that no longer exists would have zmx create an
        // empty one under the old name.
        let cur = self.current.clone();
        if live.iter().any(|s| s.name == cur) {
            return self.attach_or_exit(&cur, None, None);
        }
        self.forget(&cur);
        self.land_after_loss(&live, 0)
    }

    /// After `with_tty` for the window picker: attach to the window picked,
    /// as `<session>+<k>`, without touching `previous`, as prefix+n does.
    /// A window killed while the picker was open is not attached, which
    /// would have zmx make it anew, empty; nor is one of another session,
    /// which only a reload racing a new session could show.
    fn finish_window(&mut self, pick: Option<String>) -> Flow {
        if let Some(w) = pick {
            let (g, k) = zmx::window_of(&w);
            let name = zmx::window_name(g, k);
            let live = zmx::list();
            self.prune(&live);
            let ours = g == zmx::window_of(&self.current).0;
            if ours && name != self.current && live.iter().any(|s| s.name == name) {
                self.current = name.clone();
                return self.attach_or_exit(&name, None, None);
            }
        }
        self.finish_switch(None)
    }

    /// Session switch without a picker (jump, prev, `sluice switch`).
    /// Back to the previous session, like tmux's last-session. Its window may
    /// have gone while the session lives on: go to the session. With nothing
    /// left of it, do nothing; attaching would have zmx make it anew, empty.
    fn switch_prev(&mut self) -> Flow {
        let Some(p) = self.previous.clone() else {
            return Flow::Continue;
        };
        let live = zmx::list();
        let (group, k) = zmx::window_of(&p);
        let group = group.to_string();
        if live.iter().any(|s| s.name == p) {
            // Spelled as a window: a bare name is a session, which resolve
            // takes to the window last shown there, maybe the one on screen.
            self.switch_in(&live, Choice::named(format!("{group}+{k}")))
        } else if zmx::groups(&live).iter().any(|g| g.name == group) {
            self.switch_in(&live, Choice::named(group))
        } else {
            self.previous = None;
            Flow::Continue
        }
    }

    fn switch_to(&mut self, c: Choice) -> Flow {
        self.switch_in(&zmx::list(), c)
    }

    /// As `switch_to`, with a `zmx list` the caller already has.
    fn switch_in(&mut self, live: &[Session], c: Choice) -> Flow {
        let target = Self::resolve(&c.name, live, &self.last_window);
        if target == self.current {
            return Flow::Continue;
        }
        self.detach();
        self.previous = Some(std::mem::replace(&mut self.current, target.clone()));
        self.attach_or_exit(&target, c.cwd.as_deref(), c.cmd.as_deref())
    }

    /// Window switch inside the session on screen. `previous` is left alone,
    /// so prefix+0 still means the previous session.
    fn switch_window(&mut self, name: &str, cwd: Option<&str>) -> Flow {
        if name == self.current {
            return Flow::Continue;
        }
        self.detach();
        self.current = name.to_string();
        self.attach_or_exit(name, cwd, None)
    }

    fn attach_or_exit(&mut self, name: &str, cwd: Option<&str>, cmd: Option<&[String]>) -> Flow {
        match self.attach(name, cwd, cmd) {
            Ok(()) => Flow::Continue,
            Err(e) => {
                let _ = sys::write_all(
                    STDOUT,
                    format!("\r\nsluice: cannot start zmx: {e}\r\n").as_bytes(),
                );
                Flow::Exit(1)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sess(name: &str, created: u64, clients: u32) -> Session {
        Session {
            name: name.into(),
            created,
            pid: created as u32,
            clients,
            cwd: String::new(),
            dir: String::new(),
        }
    }

    fn none() -> HashMap<String, String> {
        HashMap::new()
    }

    #[test]
    fn fallback_prefers_previous_then_an_unwatched_session() {
        let alive = vec![
            sess("oldest", 1, 1),
            sess("quiet", 2, 0),
            sess("busy", 3, 2),
        ];

        assert_eq!(
            Runner::fallback_target(&alive, Some("busy"), &none()).as_deref(),
            Some("busy"),
            "the previous session wins while it lives"
        );
        assert_eq!(
            Runner::fallback_target(&alive, Some("gone"), &none()).as_deref(),
            Some("quiet"),
            "a dead previous falls to the session nobody is watching"
        );
        assert_eq!(
            Runner::fallback_target(&alive, None, &none()).as_deref(),
            Some("quiet"),
            "never hijack a session another pane is showing"
        );

        // Everything is being watched: the oldest is the least surprising.
        let watched = vec![sess("oldest", 1, 1), sess("newer", 2, 1)];
        assert_eq!(
            Runner::fallback_target(&watched, None, &none()).as_deref(),
            Some("oldest")
        );
        assert_eq!(Runner::fallback_target(&[], None, &none()), None);
    }

    #[test]
    fn a_session_counts_as_watched_if_any_window_is() {
        // `quiet` is unwatched, but `busy` has a client on its second window.
        let live = vec![
            sess("busy", 1, 0),
            sess("busy+2", 5, 1),
            sess("quiet", 2, 0),
        ];
        assert_eq!(
            Runner::fallback_target(&live, None, &none()).as_deref(),
            Some("quiet")
        );
    }

    #[test]
    fn resolve_lands_on_the_window_you_left() {
        let live = vec![
            sess("glass", 1, 0),
            sess("glass+2", 2, 0),
            sess("glass+3", 3, 0),
        ];
        let mut last = none();

        assert_eq!(
            Runner::resolve("glass", &live, &last),
            "glass",
            "first window by default"
        );
        last.insert("glass".into(), "glass+3".into());
        assert_eq!(
            Runner::resolve("glass", &live, &last),
            "glass+3",
            "where you left it"
        );
        assert_eq!(
            Runner::resolve("glass+2", &live, &last),
            "glass+2",
            "an explicit window wins"
        );
        assert_eq!(
            Runner::resolve("glass+1", &live, &last),
            "glass",
            "window 1 is the bare name"
        );
        assert_eq!(
            Runner::resolve("fresh", &live, &last),
            "fresh",
            "unknown: create it"
        );

        // the remembered window died: back to the first one
        let fewer = vec![sess("glass", 1, 0), sess("glass+2", 2, 0)];
        assert_eq!(Runner::resolve("glass", &fewer, &last), "glass");
    }

    #[test]
    fn windows_cycle_and_wrap_within_their_session() {
        let live = vec![
            sess("glass", 1, 0),
            sess("glass+2", 2, 0),
            sess("glass+4", 3, 0),
            sess("eddy", 4, 0),
        ];
        assert_eq!(
            Runner::cycle_window(&live, "glass", 1).as_deref(),
            Some("glass+2")
        );
        assert_eq!(
            Runner::cycle_window(&live, "glass+2", 1).as_deref(),
            Some("glass+4")
        );
        assert_eq!(
            Runner::cycle_window(&live, "glass+4", 1).as_deref(),
            Some("glass"),
            "wraps"
        );
        assert_eq!(
            Runner::cycle_window(&live, "glass", -1).as_deref(),
            Some("glass+4"),
            "wraps back"
        );
        assert_eq!(
            Runner::cycle_window(&live, "eddy", 1),
            None,
            "one window: nowhere to go"
        );
    }

    #[test]
    fn pruning_forgets_only_windows_that_are_gone() {
        let live = [sess("g", 1, 1), sess("g+2", 2, 0), sess("h", 3, 0)];
        let mut kbd: HashMap<String, KittyState> = ["g", "g+2", "g+3", "x"]
            .iter()
            .map(|w| (w.to_string(), KittyState::default()))
            .collect();
        let mut last: HashMap<String, String> = [("g", "g+2"), ("h", "h+4"), ("x", "x")]
            .iter()
            .map(|(a, b)| (a.to_string(), b.to_string()))
            .collect();
        Runner::prune_maps(&mut kbd, &mut last, &live);
        let mut k: Vec<&str> = kbd.keys().map(String::as_str).collect();
        k.sort();
        assert_eq!(k, ["g", "g+2"]);
        assert_eq!(last.len(), 1);
        assert_eq!(last["g"], "g+2");
    }

    #[test]
    fn a_new_window_takes_the_next_number() {
        let live = vec![
            sess("glass", 1, 0),
            sess("glass+2", 2, 0),
            sess("eddy+5", 3, 0),
        ];
        assert_eq!(Runner::next_window_name(&live, "glass").unwrap(), "glass+3");
        assert_eq!(Runner::next_window_name(&live, "eddy").unwrap(), "eddy+6");
        assert_eq!(
            Runner::next_window_name(&[], "new").unwrap(),
            "new",
            "a first window is the bare name"
        );
        let full = vec![sess("g+9999", 1, 0)];
        assert_eq!(Runner::next_window_name(&full, "g"), None, "no wrap past the last");
    }

    #[test]
    fn losing_a_window_lands_on_a_sibling_first() {
        let live = vec![
            sess("glass", 1, 0),
            sess("glass+3", 2, 0),
            sess("eddy", 3, 0),
        ];
        // glass+2 just died: nearest below is window 1
        assert_eq!(
            Runner::sibling_window(&live, "glass+2").as_deref(),
            Some("glass")
        );
        // window 1 died: nearest above
        let live = vec![sess("glass+2", 1, 0), sess("glass+3", 2, 0)];
        assert_eq!(
            Runner::sibling_window(&live, "glass").as_deref(),
            Some("glass+2")
        );
        // the session's last window: no sibling, the caller falls to sessions
        assert_eq!(Runner::sibling_window(&[sess("eddy", 1, 0)], "glass"), None);
    }
}
