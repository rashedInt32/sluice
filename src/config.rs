//! `~/.config/sluice/config`: one directive per line.
//!
//!   prefix C-a
//!   bind f pick
//!   bind s run ~/.config/sluice/bin/sessionizer
//!   bind 1 jump 1
//!
//! Unknown lines are reported and skipped; defaults fill every key not bound.

use crate::keys::{key_name, parse_key_name, Key};
use std::collections::HashMap;
use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// fzf over live sessions
    Pick,
    /// previous session
    Prev,
    /// nth session in creation order (1-based)
    Jump(usize),
    /// detach this client, leave the session running
    Detach,
    /// send the prefix key itself to the session
    Literal,
    /// kill the window on screen after confirmation (the session with it, if
    /// it was the last window)
    Kill,
    /// open a new window in this session, in the same directory
    NewWindow,
    /// next window of this session, wrapping
    NextWindow,
    /// previous window of this session, wrapping
    PrevWindow,
    /// show bindings
    Help,
    /// run a shell command that prints `name[<TAB>dir]`; attach to that session
    Run(String),
}

/// What happens when the session on screen ends (its shell exited or it was
/// killed from elsewhere).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OnEnd {
    /// move to the previous session, else any live one, else exit
    Previous,
    /// exit, which closes the window under the shell snippet
    Close,
}

#[derive(Debug, Clone)]
pub struct Config {
    pub prefix: Key,
    pub binds: HashMap<Key, Action>,
    pub on_end: OnEnd,
    /// What a bare `sluice` runs when no session is live (or the picker is
    /// cancelled): the `sessionizer` directive, else whatever `s` runs.
    pub sessionizer: Option<String>,
}

pub fn config_dir() -> PathBuf {
    if let Ok(x) = std::env::var("XDG_CONFIG_HOME") {
        return PathBuf::from(x).join("sluice");
    }
    PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into()))
        .join(".config")
        .join("sluice")
}

impl Config {
    pub fn defaults() -> Config {
        let bin = config_dir().join("bin");
        let sessionizer = bin.join("sessionizer").display().to_string();
        let mut binds = HashMap::new();
        binds.insert(Key::Char('f'), Action::Pick);
        binds.insert(Key::Char('s'), Action::Run(sessionizer.clone()));
        binds.insert(
            Key::Char('g'),
            Action::Run(bin.join("worktree").display().to_string()),
        );
        binds.insert(
            Key::Char('y'),
            Action::Run(bin.join("claude-here").display().to_string()),
        );
        binds.insert(
            Key::Char('u'),
            Action::Run(bin.join("claude-picker").display().to_string()),
        );
        binds.insert(Key::Char('0'), Action::Prev);
        for n in 1..=9usize {
            binds.insert(Key::Char(char::from(b'0' + n as u8)), Action::Jump(n));
        }
        binds.insert(Key::Char('d'), Action::Detach);
        binds.insert(Key::Char('x'), Action::Kill);
        // tmux's window keys
        binds.insert(Key::Char('c'), Action::NewWindow);
        binds.insert(Key::Char('n'), Action::NextWindow);
        binds.insert(Key::Char('p'), Action::PrevWindow);
        binds.insert(Key::Char('?'), Action::Help);
        binds.insert(Key::Ctrl('a'), Action::Literal);
        Config {
            prefix: Key::Ctrl('a'),
            binds,
            on_end: OnEnd::Previous,
            sessionizer: Some(sessionizer),
        }
    }

    /// Load the user's config over the defaults. Returns the config plus any
    /// warnings about lines it could not understand.
    pub fn load() -> (Config, Vec<String>) {
        let path = config_dir().join("config");
        match std::fs::read_to_string(&path) {
            Ok(text) => Config::parse(&text),
            Err(_) => (Config::defaults(), Vec::new()),
        }
    }

    pub fn parse(text: &str) -> (Config, Vec<String>) {
        let mut cfg = Config::defaults();
        let mut warnings = Vec::new();
        let mut had_prefix = false;
        let mut sessionizer = None;
        for (lineno, raw) in text.lines().enumerate() {
            let line = strip_comment(raw);
            if line.is_empty() {
                continue;
            }
            let mut words = line.splitn(2, char::is_whitespace);
            let directive = words.next().unwrap_or("");
            let rest = words.next().unwrap_or("").trim();
            match directive {
                "prefix" => match parse_key_name(rest) {
                    Some(k) => {
                        // The old prefix's literal binding follows the prefix.
                        if !had_prefix {
                            if let Some(a) = cfg.binds.remove(&cfg.prefix) {
                                cfg.binds.insert(k, a);
                            }
                        }
                        cfg.prefix = k;
                        had_prefix = true;
                    }
                    None => warnings.push(format!("line {}: bad prefix key `{rest}`", lineno + 1)),
                },
                "bind" => {
                    let mut parts = rest.splitn(2, char::is_whitespace);
                    let key_s = parts.next().unwrap_or("");
                    let action_s = parts.next().unwrap_or("").trim();
                    let Some(key) = parse_key_name(key_s) else {
                        warnings.push(format!("line {}: bad key `{key_s}`", lineno + 1));
                        continue;
                    };
                    match parse_action(action_s) {
                        Some(Action::Run(cmd)) if cmd.is_empty() => {
                            warnings.push(format!("line {}: `run` needs a command", lineno + 1))
                        }
                        Some(a) => {
                            cfg.binds.insert(key, a);
                        }
                        None => {
                            warnings.push(format!("line {}: bad action `{action_s}`", lineno + 1))
                        }
                    }
                }
                "on-end" => match rest {
                    "previous" | "prev" => cfg.on_end = OnEnd::Previous,
                    "close" => cfg.on_end = OnEnd::Close,
                    _ => warnings.push(format!(
                        "line {}: on-end wants `previous` or `close`",
                        lineno + 1
                    )),
                },
                "sessionizer" if rest.is_empty() => warnings.push(format!(
                    "line {}: `sessionizer` needs a command",
                    lineno + 1
                )),
                "sessionizer" => sessionizer = Some(rest.to_string()),
                "unbind" => match parse_key_name(rest) {
                    Some(k) => {
                        cfg.binds.remove(&k);
                    }
                    None => warnings.push(format!("line {}: bad key `{rest}`", lineno + 1)),
                },
                _ => warnings.push(format!(
                    "line {}: unknown directive `{directive}`",
                    lineno + 1
                )),
            }
        }
        cfg.sessionizer = sessionizer.or_else(|| match cfg.binds.get(&Key::Char('s')) {
            Some(Action::Run(cmd)) => Some(cmd.clone()),
            _ => None,
        });
        (cfg, warnings)
    }

    /// Bindings as `key  action` lines, sorted, for the help overlay.
    pub fn describe(&self) -> Vec<String> {
        let mut rows: Vec<(String, String)> = self
            .binds
            .iter()
            .map(|(k, a)| (key_name(*k), describe_action(a)))
            .collect();
        rows.sort();
        let w = rows.iter().map(|(k, _)| k.len()).max().unwrap_or(1);
        rows.into_iter()
            .map(|(k, a)| format!("{} {k:<w$}  {a}", key_name(self.prefix)))
            .collect()
    }
}

fn parse_action(s: &str) -> Option<Action> {
    let mut parts = s.splitn(2, char::is_whitespace);
    let verb = parts.next()?;
    let arg = parts.next().unwrap_or("").trim();
    match verb {
        "pick" => Some(Action::Pick),
        "prev" => Some(Action::Prev),
        "jump" => arg
            .parse::<usize>()
            .ok()
            .filter(|n| *n >= 1)
            .map(Action::Jump),
        "detach" => Some(Action::Detach),
        "literal" => Some(Action::Literal),
        "kill" => Some(Action::Kill),
        "new-window" => Some(Action::NewWindow),
        "next-window" => Some(Action::NextWindow),
        "prev-window" => Some(Action::PrevWindow),
        "help" => Some(Action::Help),
        "run" => Some(Action::Run(arg.to_string())),
        _ => None,
    }
}

/// `$HOME/x` as `~/x`, for display: the defaults are absolute paths, while
/// the config the user wrote says `~`.
fn tilde(cmd: &str) -> String {
    match std::env::var("HOME") {
        Ok(h) if !h.is_empty() => match cmd.strip_prefix(h.as_str()) {
            Some(rest) if rest.starts_with('/') => format!("~{rest}"),
            _ => cmd.to_string(),
        },
        _ => cmd.to_string(),
    }
}

fn describe_action(a: &Action) -> String {
    match a {
        Action::Pick => "pick a session".into(),
        Action::Prev => "previous session".into(),
        Action::Jump(n) => format!("session #{n}"),
        Action::Detach => "detach".into(),
        Action::Literal => "send the prefix key".into(),
        Action::Kill => "kill this window".into(),
        Action::NewWindow => "new window".into(),
        Action::NextWindow => "next window".into(),
        Action::PrevWindow => "previous window".into(),
        Action::Help => "this list".into(),
        Action::Run(cmd) => format!("run {}", tilde(cmd)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_cover_the_tmux_habits() {
        let c = Config::defaults();
        assert_eq!(c.prefix, Key::Ctrl('a'));
        assert_eq!(c.binds[&Key::Char('f')], Action::Pick);
        assert_eq!(c.binds[&Key::Char('0')], Action::Prev);
        assert_eq!(c.binds[&Key::Char('9')], Action::Jump(9));
        assert_eq!(c.binds[&Key::Ctrl('a')], Action::Literal);
        assert_eq!(c.binds[&Key::Char('c')], Action::NewWindow);
        assert_eq!(c.binds[&Key::Char('n')], Action::NextWindow);
        assert_eq!(c.binds[&Key::Char('p')], Action::PrevWindow);
    }

    #[test]
    fn parse_overrides_and_warns() {
        let (c, w) = Config::parse(
            "# comment\nprefix C-b\nbind f run my-picker --flag\nbind 5 jump 2\nunbind x\nbogus 1\nbind zz pick\nbind f2 run\n",
        );
        assert_eq!(c.prefix, Key::Ctrl('b'));
        assert_eq!(
            c.binds[&Key::Ctrl('b')],
            Action::Literal,
            "literal follows the prefix"
        );
        assert!(!c.binds.contains_key(&Key::Ctrl('a')));
        assert_eq!(
            c.binds[&Key::Char('f')],
            Action::Run("my-picker --flag".into())
        );
        assert_eq!(c.binds[&Key::Char('5')], Action::Jump(2));
        assert!(!c.binds.contains_key(&Key::Char('x')));
        assert_eq!(w.len(), 3, "{w:?}");
    }

    #[test]
    fn on_end_directive() {
        assert_eq!(Config::defaults().on_end, OnEnd::Previous);
        let (c, w) = Config::parse("on-end close\n");
        assert_eq!(c.on_end, OnEnd::Close);
        assert!(w.is_empty());
        let (c, w) = Config::parse("on-end sideways\n");
        assert_eq!(c.on_end, OnEnd::Previous);
        assert_eq!(w.len(), 1);
    }

    #[test]
    fn the_sessionizer_follows_s_unless_named() {
        let s = |text: &str| Config::parse(text).0.sessionizer;
        assert!(Config::defaults()
            .sessionizer
            .is_some_and(|c| c.ends_with("/bin/sessionizer")));
        assert_eq!(s("bind s run ~/x").as_deref(), Some("~/x"), "follows a rebound s");
        assert_eq!(
            s("bind s run ~/x\nsessionizer ~/y").as_deref(),
            Some("~/y"),
            "the directive wins"
        );
        assert_eq!(s("unbind s"), None, "nothing to run: never some other script");
        assert_eq!(s("bind s pick"), None);
        let (_, w) = Config::parse("sessionizer");
        assert_eq!(w.len(), 1, "{w:?}");
    }

    #[test]
    fn run_commands_show_home_as_tilde() {
        let home = std::env::var("HOME").unwrap();
        assert_eq!(tilde(&format!("{home}/.config/x")), "~/.config/x");
        assert_eq!(tilde("~/y"), "~/y");
        assert_eq!(tilde(&format!("{home}x/z")), format!("{home}x/z"), "not a prefix dir");
    }

    #[test]
    fn describe_is_sorted_and_prefixed() {
        let c = Config::defaults();
        let rows = c.describe();
        assert!(rows.iter().all(|r| r.starts_with("C-a ")));
        assert!(rows.windows(2).all(|p| p[0] <= p[1]));
    }
}

/// Drop `# ...` comments: a whole line, or a trailing one preceded by whitespace
/// so `bind ? help` and a `#` inside a run command's argument survive.
fn strip_comment(raw: &str) -> &str {
    let t = raw.trim();
    if t.starts_with('#') {
        return "";
    }
    let bytes = t.as_bytes();
    for i in 1..bytes.len() {
        if bytes[i] == b'#' && bytes[i - 1].is_ascii_whitespace() {
            return t[..i].trim_end();
        }
    }
    t
}

#[cfg(test)]
mod comment_tests {
    use super::*;

    #[test]
    fn trailing_comments_are_ignored() {
        let (c, w) = Config::parse(
            "bind 5 jump 2   # two\nbind ? help # q\nbind r run echo a#b\n# all\n   # indented\n",
        );
        assert!(w.is_empty(), "{w:?}");
        assert_eq!(c.binds[&Key::Char('5')], Action::Jump(2));
        assert_eq!(c.binds[&Key::Char('?')], Action::Help);
        assert_eq!(c.binds[&Key::Char('r')], Action::Run("echo a#b".into()));
    }

    #[test]
    fn example_config_parses_clean() {
        let text = include_str!("../config.example");
        let (c, w) = Config::parse(text);
        assert!(w.is_empty(), "{w:?}");
        let d = Config::defaults();
        assert_eq!(c.prefix, d.prefix);
        assert_eq!(c.binds.len(), d.binds.len());
        for (k, a) in &d.binds {
            match (a, &c.binds[k]) {
                (Action::Run(_), Action::Run(p)) => {
                    assert!(p.starts_with("~/.config/sluice/bin/"), "{p}")
                }
                (x, y) => assert_eq!(x, y, "key {k:?}"),
            }
        }
    }
}
