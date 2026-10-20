//! Talking to zmx through its CLI. No protocol coupling: `zmx list` and
//! `zmx kill` are the only commands we parse, and only their stable fields.

use std::process::Command;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Session {
    pub name: String,
    pub created: u64,
    pub clients: u32,
    /// working directory for display, `$HOME` shortened to `~`
    pub cwd: String,
    /// the same directory as an absolute path, for starting a new window there
    pub dir: String,
}

/// Live sessions in creation order (oldest first), name as tiebreak. That is
/// the order the numbered jumps use, so a new session never renumbers an old one.
pub fn list() -> Vec<Session> {
    let out = Command::new("zmx").arg("list").output();
    let Ok(out) = out else { return Vec::new() };
    parse_list(&String::from_utf8_lossy(&out.stdout))
}

pub fn parse_list(text: &str) -> Vec<Session> {
    let mut v: Vec<Session> = text.lines().filter_map(parse_line).collect();
    v.sort_by(|a, b| a.created.cmp(&b.created).then_with(|| a.name.cmp(&b.name)));
    v
}

fn parse_line(line: &str) -> Option<Session> {
    let mut s = Session {
        name: String::new(),
        created: 0,
        clients: 0,
        cwd: String::new(),
        dir: String::new(),
    };
    for field in line.split('\t') {
        // `zmx list` prefixes the caller's own session with an arrow marker.
        let field = field
            .trim()
            .trim_start_matches(|c: char| !c.is_ascii_alphanumeric());
        let Some((k, v)) = field.split_once('=') else {
            continue;
        };
        match k {
            "name" => s.name = v.to_string(),
            "created" => s.created = v.parse().unwrap_or(0),
            "clients" => s.clients = v.parse().unwrap_or(0),
            "cwd" => {
                s.dir = raw_path(v);
                s.cwd = cwd_path(v);
            }
            _ => {}
        }
    }
    if s.name.is_empty() {
        None
    } else {
        Some(s)
    }
}

/// zmx reports `cwd=file://host/path`; keep just the path.
fn raw_path(v: &str) -> String {
    match v.strip_prefix("file://") {
        Some(rest) => match rest.find('/') {
            Some(i) => rest[i..].to_string(),
            None => rest.to_string(),
        },
        None => v.to_string(),
    }
}

/// The path for display, with $HOME shortened to ~.
fn cwd_path(v: &str) -> String {
    let path = raw_path(v);
    let path = path.as_str();
    if let Ok(home) = std::env::var("HOME") {
        if let Some(rest) = path.strip_prefix(home.as_str()) {
            if rest.is_empty() || rest.starts_with('/') {
                return format!("~{rest}");
            }
        }
    }
    path.to_string()
}

pub fn kill(name: &str) -> bool {
    Command::new("zmx")
        .args(["kill", name])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

pub fn available() -> bool {
    Command::new("zmx")
        .arg("version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

// ---------------------------------------------------------------- windows
//
// A sluice session is a group of zmx sessions, one per window. Window 1 keeps
// the bare session name, so every session that existed before windows did is
// simply a one-window group. Window k is `<session>+<k>`: zmx accepts `+` in a
// name. A name made from a directory never ends that way (see
// `dir_session_name`); a typed `glass+2` means window 2 on purpose.
//
// Only one window is on screen at a time, so switching window is the same
// operation as switching session -- detach one zmx client, attach another --
// and the terminal still gets each shell's raw bytes. That is the difference
// from splits, which would need a screen model to draw several at once.

/// `glass+3` -> ("glass", 3); `glass` -> ("glass", 1).
pub fn window_of(name: &str) -> (&str, u32) {
    if let Some((group, k)) = name.rsplit_once('+') {
        if !group.is_empty() && !k.is_empty() && k.bytes().all(|b| b.is_ascii_digit()) {
            if let Ok(k) = k.parse::<u32>() {
                if k >= 1 {
                    return (group, k);
                }
            }
        }
    }
    (name, 1)
}

/// The zmx name for window `k` of `group`. Window 1 is the bare group name.
pub fn window_name(group: &str, k: u32) -> String {
    if k <= 1 {
        group.to_string()
    } else {
        format!("{group}+{k}")
    }
}

/// True when `name` spells a window explicitly (`glass+1`, `glass+4`) rather
/// than naming a whole session.
pub fn is_window_ref(name: &str) -> bool {
    window_of(name).0.len() != name.len()
}

/// One sluice session: the zmx sessions that are its windows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Group {
    pub name: String,
    /// creation time of its oldest window, so adding a window never renumbers it
    pub created: u64,
    /// clients across all its windows
    pub clients: u32,
    /// window 1's directory, or the oldest window's when window 1 is gone
    pub cwd: String,
    pub dir: String,
    /// zmx names of its windows, in window order
    pub windows: Vec<String>,
}

/// Collapse windows into sessions, in the stable jump order: oldest first.
pub fn groups(list: &[Session]) -> Vec<Group> {
    let mut out: Vec<Group> = Vec::new();
    let mut by_index: Vec<Vec<(u32, &Session)>> = Vec::new();
    for s in list {
        let (g, k) = window_of(&s.name);
        match out.iter().position(|x| x.name == g) {
            Some(i) => by_index[i].push((k, s)),
            None => {
                out.push(Group {
                    name: g.to_string(),
                    created: 0,
                    clients: 0,
                    cwd: String::new(),
                    dir: String::new(),
                    windows: Vec::new(),
                });
                by_index.push(vec![(k, s)]);
            }
        }
    }
    for (g, mut ws) in out.iter_mut().zip(by_index) {
        ws.sort_by_key(|(k, _)| *k);
        g.created = ws.iter().map(|(_, s)| s.created).min().unwrap_or(0);
        g.clients = ws.iter().map(|(_, s)| s.clients).sum();
        let first = ws[0].1;
        g.cwd = first.cwd.clone();
        g.dir = first.dir.clone();
        g.windows = ws.iter().map(|(_, s)| s.name.clone()).collect();
    }
    out.sort_by(|a, b| a.created.cmp(&b.created).then_with(|| a.name.cmp(&b.name)));
    out
}

/// Live sessions, grouped. One `zmx list`.
pub fn list_groups() -> Vec<Group> {
    groups(&list())
}

/// Kill every window of a session, or one window when named explicitly.
pub fn kill_target(name: &str, live: &[Session]) -> usize {
    let targets: Vec<String> = if is_window_ref(name) {
        let (g, k) = window_of(name);
        vec![window_name(g, k)]
    } else {
        groups(live)
            .into_iter()
            .find(|g| g.name == name)
            .map(|g| g.windows)
            .unwrap_or_default()
    };
    targets.iter().filter(|t| kill(t)).count()
}

/// Session names share zmx's socket directory, so `/` is out; tmux also forbade
/// `.` `:` and spaces and the user's scripts already map those to `_`.
/// Control characters go too: a name is drawn in the picker, and an ESC in it
/// would be a terminal escape.
pub fn sanitize_name(raw: &str) -> String {
    raw.chars()
        .map(|c| {
            if matches!(c, '/' | '.' | ':' | ',' | ' ') || c.is_control() {
                '_'
            } else {
                c
            }
        })
        .collect()
}

/// A session name made from a directory name. Sanitized like any name, and
/// a trailing `+<digits>` becomes `_<digits>`: `c++17` would otherwise be
/// window 17 of a session called `c+`. The bundled scripts apply the same
/// rule, so `sluice here` and the sessionizer agree on a directory's name.
pub fn dir_session_name(raw: &str) -> String {
    let mut name = sanitize_name(raw);
    if let Some((g, k)) = name.rsplit_once('+') {
        if !g.is_empty() && !k.is_empty() && k.bytes().all(|b| b.is_ascii_digit()) {
            name = format!("{g}_{k}");
        }
    }
    name
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_sorts_by_creation() {
        let text = "  name=beta\tpid=2\tclients=1\tcreated=200\tcwd=file://host/tmp/b\n\
                    name=alpha\tpid=1\tclients=0\tcreated=100\tcwd=file://host/Users/x/proj\n\
                    garbage line\n\
                    name=gamma\tpid=3\tclients=2\tcreated=200\tcwd=\n";
        let v = parse_list(text);
        let names: Vec<&str> = v.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["alpha", "beta", "gamma"]);
        assert_eq!(v[0].cwd, "/Users/x/proj");
        assert_eq!(v[1].clients, 1);
        assert_eq!(v[2].cwd, "");
    }

    #[test]
    fn current_session_marker_is_not_dropped() {
        let text = "→ name=here\tpid=1\tclients=1\tcreated=5\tcwd=file://h/x\n  name=other\tpid=2\tclients=0\tcreated=6\tcwd=\n";
        let v = parse_list(text);
        assert_eq!(v.len(), 2, "{v:?}");
        assert_eq!(v[0].name, "here");
        assert_eq!(v[0].clients, 1);
    }

    #[test]
    fn empty_or_no_sessions_message() {
        assert!(parse_list("no sessions found in /tmp/zmx-501\n").is_empty());
        assert!(parse_list("").is_empty());
    }

    fn s(name: &str, created: u64, clients: u32) -> Session {
        Session {
            name: name.into(),
            created,
            clients,
            cwd: format!("~/{name}"),
            dir: format!("/h/{name}"),
        }
    }

    #[test]
    fn window_names_round_trip() {
        assert_eq!(window_of("glass"), ("glass", 1));
        assert_eq!(window_of("glass+3"), ("glass", 3));
        assert_eq!(
            window_of("c++"),
            ("c++", 1),
            "a trailing plus with no number is a name"
        );
        assert_eq!(window_of("a+b"), ("a+b", 1));
        assert_eq!(window_of("x+0"), ("x+0", 1), "there is no window 0");
        assert_eq!(window_of("+2"), ("+2", 1), "a window needs a session");
        assert_eq!(window_name("glass", 1), "glass");
        assert_eq!(window_name("glass", 4), "glass+4");
        assert!(is_window_ref("glass+1"));
        assert!(!is_window_ref("glass"));
    }

    #[test]
    fn windows_collapse_into_their_session() {
        let live = vec![
            s("eddy", 50, 0),
            s("glass+2", 90, 1),
            s("glass", 10, 0),
            s("glass+3", 95, 0),
        ];
        let g = groups(&live);
        let names: Vec<&str> = g.iter().map(|x| x.name.as_str()).collect();
        assert_eq!(names, ["glass", "eddy"], "ordered by the oldest window");
        assert_eq!(g[0].windows, ["glass", "glass+2", "glass+3"]);
        assert_eq!(g[0].clients, 1, "clients add up across windows");
        assert_eq!(g[0].dir, "/h/glass", "window 1's directory");
        assert_eq!(g[1].windows, ["eddy"]);
    }

    #[test]
    fn a_new_window_never_renumbers_the_sessions() {
        let before = groups(&[s("a", 10, 0), s("b", 20, 0)]);
        let after = groups(&[s("a", 10, 0), s("b", 20, 0), s("a+2", 99, 0)]);
        let n = |g: &[Group]| g.iter().map(|x| x.name.clone()).collect::<Vec<_>>();
        assert_eq!(n(&before), n(&after));
    }

    #[test]
    fn a_directory_never_names_a_window() {
        assert_eq!(dir_session_name("c++17"), "c+_17");
        assert_eq!(dir_session_name("app+2"), "app_2");
        assert_eq!(dir_session_name("app+0"), "app_0", "same rule as the scripts");
        assert_eq!(dir_session_name("c++"), "c++");
        assert_eq!(dir_session_name("+2"), "+2", "not a window ref either");
        assert_eq!(dir_session_name("my app.v2"), "my_app_v2");
        assert!(!is_window_ref(&dir_session_name("lib+3")));
    }

    #[test]
    fn sanitize_matches_tmux_rules() {
        assert_eq!(sanitize_name("glass/feat.x:y z"), "glass_feat_x_y_z");
        assert_eq!(sanitize_name("plain-name"), "plain-name");
        assert_eq!(sanitize_name("a\tb\x1b[2Jc\0"), "a_b_[2Jc_", "no control bytes");
    }
}
