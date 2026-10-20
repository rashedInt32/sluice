mod clients;
mod config;
mod handoff;
mod kbd;
mod keys;
mod picker;
mod runner;
mod sys;
mod zmx;

use config::Config;

const USAGE: &str = "\
sluice - tmux-style prefix keys on top of zmx, with raw PTY passthrough

usage:
  sluice [attach] <name> [-c <dir>]   attach to <name>, creating it in <dir>
  sluice here [-c <dir>]              a shell here: the session named after this
                                      directory, reusing an idle one or numbering
                                      a fresh one. What a new split runs.
  sluice                              pick a session; run the sessionizer if none
  sluice list [--rows [--current S]]  numbered sessions, in jump order;
                                      --rows prints them as the picker draws them
  sluice keys                         show the active prefix bindings
  sluice kill <name>                  kill a session with all its windows, or one
                                      window named as <session>+<n>
  sluice switch <name>|- [--session S] move the pane showing this session to <name>,
                                      or back to the previous one with `-`
  sluice detach [--session S]         detach the client showing this session
                                      (the window closes under the shell snippet)
  sluice handoff on|off|status [--pid N] [--session S]
                                      pass the prefix key through to the program
                                      (for loom.nvim); session defaults to $ZMX_SESSION,
                                      pid to the caller; a dead pid ends the handoff
  sluice --version | --help

exit codes: 0 detached or session ended, 3 nothing picked, 1 error

config: ~/.config/sluice/config   (sluice keys shows the result)
";

fn main() {
    // Rust ignores SIGPIPE, so `sluice list | head` would panic on the first
    // write after head exits. A CLI should just stop.
    unsafe { libc::signal(libc::SIGPIPE, libc::SIG_DFL) };
    let args: Vec<String> = std::env::args().skip(1).collect();
    let code = real_main(&args);
    std::process::exit(code);
}

fn real_main(args: &[String]) -> i32 {
    let first = args.first().map(String::as_str);
    match first {
        Some("--help") | Some("-h") | Some("help") => {
            print!("{USAGE}");
            return 0;
        }
        Some("--version") | Some("-V") | Some("version") => {
            println!("sluice {}", env!("CARGO_PKG_VERSION"));
            return 0;
        }
        Some("list") | Some("ls") => return list_cmd(&args[1..]),
        Some("handoff") => return handoff_cmd(&args[1..]),
        Some("detach") | Some("d") => return control_cmd("detach", &args[1..]),
        Some("switch") => return control_cmd("switch", &args[1..]),
        Some("kill") => return kill_cmd(&args[1..]),
        Some("keys") => {
            let (cfg, warnings) = Config::load();
            for w in &warnings {
                eprintln!("sluice: config: {w}");
            }
            for line in cfg.describe() {
                println!("{line}");
            }
            return 0;
        }
        _ => {}
    }

    if let Ok(s) = std::env::var("ZMX_SESSION") {
        eprintln!("sluice: already inside zmx session `{s}`; use the prefix keys instead");
        return 1;
    }
    if unsafe { libc::isatty(libc::STDIN_FILENO) } == 0 {
        eprintln!("sluice: stdin is not a terminal");
        return 1;
    }
    // The session's screen goes to stdout, so it must be the terminal too.
    if unsafe { libc::isatty(libc::STDOUT_FILENO) } == 0 {
        eprintln!("sluice: stdout is not a terminal");
        return 1;
    }
    // One `zmx list` both proves zmx runs and serves the attach below.
    let live = match zmx::try_list() {
        Ok(v) => v,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            eprintln!("sluice: zmx not found on PATH (brew install neurosnap/tap/zmx)");
            return 1;
        }
        Err(e) => {
            eprintln!("sluice: cannot run zmx: {e}");
            return 1;
        }
    };

    let (cfg, warnings) = Config::load();
    for w in &warnings {
        eprintln!("sluice: config: {w}");
    }

    if first == Some("here") {
        let cwd = match parse_here_args(&args[1..]) {
            Ok(d) => d,
            Err(msg) => {
                eprintln!("sluice: {msg}");
                return 2;
            }
        };
        let cwd = cwd.unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
        let home = std::env::var("HOME").ok().map(std::path::PathBuf::from);
        let name = here_name(&cwd, home.as_deref(), &zmx::groups(&live));
        let dir = cwd.to_string_lossy().into_owned();
        return match runner::Runner::start(cfg, &name, Some(&dir), &live) {
            Ok(r) => r.run(),
            Err(e) => {
                eprintln!("sluice: {e}");
                1
            }
        };
    }

    let (name, cwd, live) = match parse_attach_args(args) {
        Ok(Some((name, cwd))) => (name, cwd, live),
        // The picker and the sessionizer can take a while, and the picker
        // can kill sessions: list again after them.
        Ok(None) => match pick_or_sessionize(&cfg, &live) {
            Some(c) => (c.name, c.cwd, zmx::list()),
            None => return 3,
        },
        Err(msg) => {
            eprintln!("sluice: {msg}");
            eprint!("{USAGE}");
            return 2;
        }
    };

    match runner::Runner::start(cfg, &name, cwd.as_deref(), &live) {
        Ok(r) => r.run(),
        Err(e) => {
            eprintln!("sluice: {e}");
            1
        }
    }
}

/// `switch <name>` and `detach` both act on the sluice client that is showing a
/// session, so nvim or a script can drive the pane it is sitting in.
/// `list` prints one session per line. The default is a machine format --
/// index, name, clients, created, cwd, tab separated, because a session name
/// cannot contain a tab and a path should not. `--rows` instead prints the
/// lines the picker draws, which is what fzf reloads after a kill.
fn list_cmd(args: &[String]) -> i32 {
    let mut rows = false;
    let mut windows = false;
    let mut current = String::new();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--rows" => {
                rows = true;
                i += 1;
            }
            "--windows" => {
                windows = true;
                i += 1;
            }
            "--current" => {
                current = args.get(i + 1).cloned().unwrap_or_default();
                i += 2;
            }
            other => {
                eprintln!("sluice: list: unknown argument {other}");
                return 2;
            }
        }
    }
    let live = zmx::list();
    if windows {
        // every zmx session, one per window: name, clients, created, cwd
        for s in &live {
            println!("{}\t{}\t{}\t{}", s.name, s.clients, s.created, s.cwd);
        }
        return 0;
    }
    let groups = zmx::groups(&live);
    if rows {
        for line in picker::session_lines(&groups, &current) {
            println!("{line}");
        }
    } else {
        // One row per session, windows folded in: index, name, clients,
        // created, windows (comma separated, in order), cwd. cwd stays last
        // so a path is never split; names cannot contain a comma or a tab.
        for (i, g) in groups.iter().enumerate() {
            println!(
                "{}\t{}\t{}\t{}\t{}\t{}",
                i + 1,
                g.name,
                g.clients,
                g.created,
                g.windows.join(","),
                g.cwd
            );
        }
    }
    0
}

/// `kill <name>`: a session with every window, or `<session>+<n>` for one.
fn kill_cmd(args: &[String]) -> i32 {
    let Some(name) = args.first() else {
        eprintln!("sluice: kill needs a session name");
        return 2;
    };
    let live = zmx::list();
    if zmx::kill_target(name, &live) == 0 {
        eprintln!("sluice: no session named `{name}`");
        return 1;
    }
    0
}

fn control_cmd(verb: &str, args: &[String]) -> i32 {
    let mut session = std::env::var("ZMX_SESSION").ok();
    let mut target: Option<String> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--session" => {
                session = args.get(i + 1).cloned();
                i += 2;
            }
            other if other.starts_with('-') => {
                eprintln!("sluice: {verb}: unknown argument {other}");
                return 2;
            }
            other => {
                if target.is_some() {
                    eprintln!("sluice: {verb}: one name only");
                    return 2;
                }
                target = Some(zmx::sanitize_name(other));
                i += 1;
            }
        }
    }
    let Some(session) = session else {
        eprintln!("sluice: {verb}: not inside a zmx session and no --session given");
        return 2;
    };
    let cmd = match (verb, &target) {
        ("switch", Some(t)) if t == "-" => "prev".to_string(),
        ("switch", Some(t)) => {
            // A session (any of its windows alive) or one explicit window.
            let live = zmx::list();
            let exists = if zmx::is_window_ref(t) {
                let (g, k) = zmx::window_of(t);
                let w = zmx::window_name(g, k);
                live.iter().any(|s| s.name == w)
            } else {
                zmx::groups(&live).iter().any(|g| &g.name == t)
            };
            if !exists {
                eprintln!("sluice: no session named `{t}`");
                return 1;
            }
            format!("switch {t}")
        }
        ("switch", None) => {
            eprintln!("sluice: switch needs a session name");
            return 2;
        }
        _ => "detach".to_string(),
    };
    // `--session` may name the window a client is on (what ZMX_SESSION
    // holds) or a whole session; a session reaches every one of its windows.
    let mut targets = vec![session.clone()];
    if !zmx::is_window_ref(&session) {
        if let Some(g) = zmx::list_groups().into_iter().find(|g| g.name == session) {
            for w in g.windows {
                if !targets.contains(&w) {
                    targets.push(w);
                }
            }
        }
    }
    let reached: usize = targets.iter().map(|t| clients::request(t, &cmd)).sum();
    if reached == 0 {
        eprintln!("sluice: no sluice client is showing `{session}`");
        return 1;
    }
    0
}

fn handoff_cmd(args: &[String]) -> i32 {
    let mut verb = None;
    let mut pid: i32 = unsafe { libc::getppid() };
    let mut session = std::env::var("ZMX_SESSION").ok();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--pid" => {
                pid = match args.get(i + 1).and_then(|p| p.parse().ok()) {
                    Some(p) => p,
                    None => {
                        eprintln!("sluice: --pid needs a number");
                        return 2;
                    }
                };
                i += 2;
            }
            "--session" => {
                session = args.get(i + 1).cloned();
                i += 2;
            }
            v @ ("on" | "off" | "status") => {
                verb = Some(v.to_string());
                i += 1;
            }
            other => {
                eprintln!("sluice: handoff: unknown argument {other}");
                return 2;
            }
        }
    }
    let Some(session) = session else {
        eprintln!("sluice: handoff: not inside a zmx session and no --session given");
        return 2;
    };
    let r = match verb.as_deref() {
        Some("on") => handoff::set(&session, pid),
        Some("off") => handoff::clear(&session),
        Some("status") => {
            println!(
                "{}",
                if handoff::active(&session) {
                    "on"
                } else {
                    "off"
                }
            );
            return 0;
        }
        _ => {
            eprintln!("sluice: handoff needs on, off or status");
            return 2;
        }
    };
    match r {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("sluice: handoff: {e}");
            1
        }
    }
}

fn parse_here_args(args: &[String]) -> Result<Option<std::path::PathBuf>, String> {
    let mut dir = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "-c" | "--cwd" => {
                dir = Some(std::path::PathBuf::from(
                    args.get(i + 1).ok_or("-c needs a directory")?,
                ));
                i += 2;
            }
            other => return Err(format!("here: unknown argument {other}")),
        }
    }
    Ok(dir)
}

/// The session for "give me a shell here".
///
/// Named after the directory, so a split lands beside its project rather than
/// in a picker. A session of that name that nobody is attached to is reused --
/// that is the one you left behind. One that *is* on screen somewhere is left
/// alone and a numbered sibling is used instead: two panes sharing a session
/// would fight over its size, since zmx resizes to whichever attached last.
fn here_name(
    cwd: &std::path::Path,
    home: Option<&std::path::Path>,
    sessions: &[zmx::Group],
) -> String {
    let base = match cwd.file_name() {
        // $HOME is everyone's default landing place; it gets the name the
        // shell snippet has always used rather than the user's login name.
        _ if Some(cwd) == home => "main".to_string(),
        Some(n) => zmx::dir_session_name(&n.to_string_lossy()),
        None => "main".to_string(),
    };
    let base = if base.is_empty() {
        "main".to_string()
    } else {
        base
    };
    let busy = |n: &str| sessions.iter().any(|s| s.name == n && s.clients > 0);
    if !busy(&base) {
        return base;
    }
    (2..=99)
        .map(|i| format!("{base}-{i}"))
        .find(|n| !busy(n))
        .unwrap_or(base)
}

/// `[attach] <name> [-c dir]` -> Some((name, cwd)); no args -> None.
fn parse_attach_args(args: &[String]) -> Result<Option<(String, Option<String>)>, String> {
    let mut rest: Vec<&str> = args.iter().map(String::as_str).collect();
    if rest.first() == Some(&"attach") || rest.first() == Some(&"a") {
        rest.remove(0);
    }
    if rest.is_empty() {
        return Ok(None);
    }
    let mut name: Option<String> = None;
    let mut cwd: Option<String> = None;
    let mut i = 0;
    while i < rest.len() {
        match rest[i] {
            "-c" | "--cwd" => {
                cwd = Some(rest.get(i + 1).ok_or("-c needs a directory")?.to_string());
                i += 2;
            }
            s if s.starts_with('-') => return Err(format!("unknown flag {s}")),
            s => {
                if name.is_some() {
                    return Err("only one session name".into());
                }
                name = Some(zmx::sanitize_name(s));
                i += 1;
            }
        }
    }
    match name {
        Some(n) => Ok(Some((n, cwd))),
        None => Err("missing session name".into()),
    }
}

/// No name given: fzf over live sessions; with none (or on cancel) fall through
/// to the sessionizer so a fresh terminal can start a project directly.
fn pick_or_sessionize(cfg: &Config, live: &[zmx::Session]) -> Option<picker::Choice> {
    let sessions = zmx::groups(live);
    if !sessions.is_empty() {
        let header = "j/k move · 1-9 jump · enter attach · ctrl-x kill · ctrl-d detach · i search · q new project";
        if let Some(name) = picker::pick_session(&sessions, "", header) {
            return Some(picker::Choice::named(name));
        }
    }
    picker::run_script(cfg.sessionizer.as_deref()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    fn sess(name: &str, clients: u32) -> zmx::Group {
        zmx::Group {
            name: name.into(),
            created: 0,
            pid: 0,
            clients,
            cwd: String::new(),
            dir: String::new(),
            windows: vec![name.into()],
        }
    }

    #[test]
    fn here_names_the_session_after_the_directory() {
        use std::path::Path;
        let home = Path::new("/Users/x");

        // nothing running: plain directory name
        assert_eq!(
            here_name(Path::new("/Users/x/code/glass"), Some(home), &[]),
            "glass"
        );
        // home itself keeps the name the shell snippet always used
        assert_eq!(here_name(home, Some(home), &[]), "main");
        assert_eq!(here_name(Path::new("/"), Some(home), &[]), "main");
        // a dot or space in the directory name is not a session name
        assert_eq!(
            here_name(Path::new("/tmp/my app.v2"), Some(home), &[]),
            "my_app_v2"
        );
        // nor one that would read as a window of another session
        assert_eq!(here_name(Path::new("/tmp/app+2"), Some(home), &[]), "app_2");
    }

    #[test]
    fn here_reuses_an_idle_session_and_numbers_around_a_busy_one() {
        use std::path::Path;
        let dir = Path::new("/Users/x/code/glass");

        // left behind, nobody looking at it: step back into it
        assert_eq!(here_name(dir, None, &[sess("glass", 0)]), "glass");
        // already on screen in another pane: take a sibling instead
        assert_eq!(here_name(dir, None, &[sess("glass", 1)]), "glass-2");
        assert_eq!(
            here_name(dir, None, &[sess("glass", 1), sess("glass-2", 2)]),
            "glass-3"
        );
        // the sibling is reused too once its pane goes away
        assert_eq!(
            here_name(dir, None, &[sess("glass", 1), sess("glass-2", 0)]),
            "glass-2"
        );
    }

    #[test]
    fn attach_args() {
        assert_eq!(parse_attach_args(&v(&[])), Ok(None));
        assert_eq!(parse_attach_args(&v(&["attach"])), Ok(None));
        assert_eq!(
            parse_attach_args(&v(&["glass"])),
            Ok(Some(("glass".into(), None)))
        );
        assert_eq!(
            parse_attach_args(&v(&["attach", "a.b", "-c", "/tmp"])),
            Ok(Some(("a_b".into(), Some("/tmp".into()))))
        );
        assert!(parse_attach_args(&v(&["-c"])).is_err());
        assert!(parse_attach_args(&v(&["x", "y"])).is_err());
        assert!(parse_attach_args(&v(&["--bogus"])).is_err());
    }
}
