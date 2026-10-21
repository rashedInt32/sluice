mod clients;
mod config;
mod handoff;
mod kbd;
mod keys;
mod picker;
mod runner;
mod sys;
mod title;
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
  sluice list --windows               every window, one zmx session per line
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
  sluice picker-style [--label L] [--accent #rrggbb] [--hints H]
                                      the pickers' fzf look and nav keys as quoted
                                      shell words, for a script's own fzf
  sluice --version | --help

A name that is also a command (a, ls, d, kill, keys, here...) needs
`sluice attach <name>`.

exit codes: 0 detached or session ended, 3 nothing picked, 1 error,
            2 bad arguments

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
        Some("picker-style") => return picker_style_cmd(&args[1..]),
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

    if let Some(s) = zmx_session_env() {
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
        Ok(Some((name, cwd))) => (session_name(&name, &live), cwd, live),
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

/// The value after flag `args[i]`. A missing one is an error rather than
/// None, and so is one that looks like the next flag: `--current --rows`
/// must not name a session `--rows`.
fn flag_value(args: &[String], i: usize) -> Result<String, String> {
    match args.get(i + 1) {
        Some(v) if !v.is_empty() && !v.starts_with('-') => Ok(v.clone()),
        _ => Err(format!("{} needs a value", args[i])),
    }
}

/// The session a typed name means: one that exists under exactly that name
/// (zmx itself takes `a.b`), else the name sanitized for a new session.
fn session_name(raw: &str, live: &[zmx::Session]) -> String {
    let exists = live.iter().any(|s| s.name == raw)
        || zmx::groups(live).iter().any(|g| g.name == raw);
    match exists {
        true => raw.to_string(),
        false => zmx::sanitize_name(raw),
    }
}

/// `$ZMX_SESSION`, unless unset or empty. An empty name would point the
/// client registry at its own root.
fn zmx_session_env() -> Option<String> {
    std::env::var("ZMX_SESSION").ok().filter(|s| !s.is_empty())
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
                current = match flag_value(args, i) {
                    Ok(v) => v,
                    Err(e) => {
                        eprintln!("sluice: list: {e}");
                        return 2;
                    }
                };
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

/// `picker-style [--label L] [--accent #rrggbb] [--hints H]`: the shared fzf
/// look and nav keys as shell words, for a script to `eval` into an array.
/// Every word is quoted, so the output is safe to eval whatever the inputs.
fn picker_style_cmd(args: &[String]) -> i32 {
    let (mut label, mut accent, mut hints) = ("", "#ff9f1a", "");
    let mut i = 0;
    while i < args.len() {
        let val = args.get(i + 1).map(String::as_str);
        match (args[i].as_str(), val) {
            ("--label", Some(v)) => label = v,
            ("--accent", Some(v)) if picker::is_hex_colour(v) => accent = v,
            ("--accent", Some(v)) => {
                eprintln!("sluice: picker-style: --accent wants #rrggbb, not `{v}`");
                return 2;
            }
            ("--hints", Some(v)) => hints = v,
            (other, _) => {
                eprintln!("sluice: picker-style: bad argument {other}");
                return 2;
            }
        }
        i += 2;
    }
    let words: Vec<String> = picker::style_args(label, hints, accent)
        .iter()
        .map(|w| picker::sh_quote(w))
        .collect();
    println!("{}", words.join(" "));
    0
}

/// `kill <name>`: a session with every window, or `<session>+<n>` for one.
fn kill_cmd(args: &[String]) -> i32 {
    let [name] = args else {
        eprintln!("sluice: kill needs one session name");
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
    let mut session = zmx_session_env();
    let mut target: Option<String> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--session" => {
                session = match flag_value(args, i) {
                    Ok(v) => Some(v),
                    Err(e) => {
                        eprintln!("sluice: {verb}: {e}");
                        return 2;
                    }
                };
                i += 2;
            }
            // `-` is a name here: the previous session.
            other if other.starts_with('-') && other != "-" => {
                eprintln!("sluice: {verb}: unknown argument {other}");
                return 2;
            }
            other => {
                if target.is_some() {
                    eprintln!("sluice: {verb}: one name only");
                    return 2;
                }
                target = Some(other.to_string());
                i += 1;
            }
        }
    }
    if verb == "detach" && target.is_some() {
        eprintln!("sluice: detach takes no name; use --session");
        return 2;
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
            let t = &session_name(t, &live);
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
    let live = zmx::list();
    if !zmx::is_window_ref(&session) {
        if let Some(g) = zmx::groups(&live).into_iter().find(|g| g.name == session) {
            for w in g.windows {
                if !targets.contains(&w) {
                    targets.push(w);
                }
            }
        }
    }
    let reached: Vec<(&String, i32)> = targets
        .iter()
        .flat_map(|t| clients::request(t, &cmd).into_iter().map(move |p| (t, p)))
        .collect();
    if reached.is_empty() {
        eprintln!("sluice: no sluice client is showing `{session}`");
        return 1;
    }
    if cmd == "detach" {
        wait_detached(&targets, clients_on(&live, &targets), &reached);
    }
    0
}

fn clients_on(live: &[zmx::Session], targets: &[String]) -> usize {
    live.iter()
        .filter(|s| targets.contains(&s.name))
        .map(|s| s.clients as usize)
        .sum()
}

/// A detach lands asynchronously, in the client. Return once zmx counts
/// the reached clients gone from `targets`, so whoever runs next (the
/// picker's reload) sees the result.
///
/// First watch each client's registration file, which it removes once its
/// attach child is reaped: a stat per poll, no subprocess. Then confirm with
/// `zmx list`, which can trail the reap by a moment, backing off so a client
/// that never lets go costs a handful of spawns rather than a hundred.
fn wait_detached(targets: &[String], before: usize, reached: &[(&String, i32)]) {
    let end = std::time::Instant::now() + std::time::Duration::from_secs(1);
    while reached.iter().any(|(t, p)| clients::showing(t, *p)) {
        if std::time::Instant::now() >= end {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    // About a second in all, as long as the old fixed loop gave it.
    for ms in [0, 5, 10, 20, 40, 80, 160, 320, 400] {
        std::thread::sleep(std::time::Duration::from_millis(ms));
        if clients_on(&zmx::list(), targets) + reached.len() <= before {
            return;
        }
    }
}

fn handoff_cmd(args: &[String]) -> i32 {
    let mut verb = None;
    let mut pid: i32 = unsafe { libc::getppid() };
    let mut session = zmx_session_env();
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
                session = match flag_value(args, i) {
                    Ok(v) => Some(v),
                    Err(e) => {
                        eprintln!("sluice: handoff: {e}");
                        return 2;
                    }
                };
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
                // Sanitized by the caller, which knows what already exists.
                let n = s.trim();
                if zmx::sanitize_name(n).is_empty() {
                    return Err("empty session name".into());
                }
                name = Some(n.to_string());
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

    #[test]
    fn a_flag_needs_a_value_that_is_not_another_flag() {
        assert_eq!(flag_value(&v(&["--session", "x"]), 0).as_deref(), Ok("x"));
        assert!(flag_value(&v(&["--session"]), 0).is_err());
        assert!(flag_value(&v(&["--session", ""]), 0).is_err());
        assert!(flag_value(&v(&["--current", "--rows"]), 0).is_err());
    }

    #[test]
    fn an_empty_name_is_refused() {
        assert!(parse_attach_args(&v(&[""])).is_err());
        assert!(parse_attach_args(&v(&["attach", "  "])).is_err());
        assert_eq!(
            parse_attach_args(&v(&["attach", "a"])),
            Ok(Some(("a".into(), None))),
            "a name that is also a command, through attach"
        );
    }

    #[test]
    fn switch_dash_and_bad_flags_exit_two_or_reach_the_lookup() {
        // No client anywhere: a well-formed request ends in 1 (nobody to
        // reach), a malformed one in 2 before anything is looked up.
        let d = std::env::temp_dir().join(format!("sluice-cli-{}", std::process::id()));
        std::env::set_var("XDG_STATE_HOME", &d);
        assert_eq!(control_cmd("switch", &v(&["-", "--session", "nosuch"])), 1);
        assert_eq!(control_cmd("switch", &v(&["x", "--session"])), 2);
        assert_eq!(control_cmd("detach", &v(&["x", "--session", "y"])), 2);
        assert_eq!(kill_cmd(&v(&["a", "b"])), 2);
        let _ = std::fs::remove_dir_all(&d);
    }

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
    fn a_typed_name_keeps_an_existing_session_or_is_sanitized() {
        let s = |n: &str| zmx::Session {
            name: n.into(),
            created: 0,
            pid: 0,
            clients: 0,
            cwd: String::new(),
            dir: String::new(),
        };
        let live = [s("a.b"), s("g"), s("g+2")];
        assert_eq!(session_name("a.b", &live), "a.b", "zmx made it: keep it");
        assert_eq!(session_name("c.d", &live), "c_d", "new: sanitized");
        assert_eq!(session_name("g+2", &live), "g+2");
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
            Ok(Some(("a.b".into(), Some("/tmp".into()))))
        );
        assert!(parse_attach_args(&v(&["-c"])).is_err());
        assert!(parse_attach_args(&v(&["x", "y"])).is_err());
        assert!(parse_attach_args(&v(&["--bogus"])).is_err());
    }
}
