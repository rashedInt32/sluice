//! Talking to zmx through its CLI. No protocol coupling: `zmx list` and
//! `zmx kill` are the only commands we parse, and only their stable fields.

use std::process::Command;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Session {
    pub name: String,
    /// whole seconds, so two sessions can share it
    pub created: u64,
    /// the pid zmx reports, the process the session started with. Pids are
    /// handed out in increasing order, so it breaks a tie in `created` the
    /// way the sessions were actually made.
    pub pid: u32,
    pub clients: u32,
    /// working directory for display, `$HOME` shortened to `~`
    pub cwd: String,
    /// the same directory as an absolute path, for starting a new window there
    pub dir: String,
}

/// Live sessions in creation order, oldest first. That is the order the
/// numbered jumps use, so a new session never renumbers an old one.
pub fn list() -> Vec<Session> {
    try_list().unwrap_or_default()
}

/// As `list`, but a zmx that cannot be run is an error rather than no
/// sessions. Startup uses it in place of a separate `zmx version` probe.
pub fn try_list() -> std::io::Result<Vec<Session>> {
    let out = Command::new("zmx").arg("list").output()?;
    Ok(parse_list(&String::from_utf8_lossy(&out.stdout)))
}

pub fn parse_list(text: &str) -> Vec<Session> {
    let mut v: Vec<Session> = text.lines().filter_map(parse_line).collect();
    v.sort_by_key(|s| (s.created, s.pid));
    v
}

fn parse_line(line: &str) -> Option<Session> {
    let mut s = Session {
        name: String::new(),
        created: 0,
        pid: 0,
        clients: 0,
        cwd: String::new(),
        dir: String::new(),
    };
    // zmx prints its own fields first and the session's labels after, and a
    // label may reuse a field's key (`created=1`): the first one wins.
    let mut seen: Vec<&str> = Vec::new();
    for (i, field) in line.split('\t').enumerate() {
        // `zmx list` indents each line and marks the caller's own session
        // with an arrow; only the first field carries either.
        let field = match i {
            0 => field.trim_start_matches(|c: char| c.is_whitespace() || c == '→'),
            _ => field,
        };
        let Some((k, v)) = field.split_once('=') else {
            continue;
        };
        if seen.contains(&k) {
            continue;
        }
        seen.push(k);
        match k {
            "name" => s.name = v.to_string(),
            "created" => s.created = v.parse().unwrap_or(0),
            "pid" => s.pid = v.parse().unwrap_or(0),
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

/// zmx reports `cwd=file://host/path`, percent-encoded; keep just the path,
/// decoded. A URL with no path after the host has no usable directory.
fn raw_path(v: &str) -> String {
    match v.strip_prefix("file://") {
        Some(rest) => match rest.find('/') {
            Some(i) => percent_decode(&rest[i..]),
            None => String::new(),
        },
        None => v.to_string(),
    }
}

/// `%XX` escapes back to bytes. A `%` not followed by two hex digits is kept
/// as it is.
fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let hex = |c: u8| (c as char).to_digit(16).map(|d| d as u8);
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let digit = |k: usize| b.get(k).and_then(|&c| hex(c));
        if let (b'%', Some(h), Some(l)) = (b[i], digit(i + 1), digit(i + 2)) {
            out.push((h << 4) | l);
            i += 3;
            continue;
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The path for display, with $HOME shortened to ~.
fn cwd_path(v: &str) -> String {
    let path = raw_path(v);
    let path = path.as_str();
    if let Ok(home) = std::env::var("HOME") {
        // `HOME=/Users/x/` is still /Users/x; `HOME=/` shortens nothing.
        let home = home.trim_end_matches('/');
        if home.is_empty() {
            return path.to_string();
        }
        if let Some(rest) = path.strip_prefix(home) {
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
        // Only the number as window_name spells it: `x+01` or `x+0` is a
        // name of its own, not another way to say `x`.
        let canonical = !k.starts_with('0') && k.bytes().all(|b| b.is_ascii_digit());
        if !group.is_empty() && canonical {
            if let Ok(k @ 1..=MAX_WINDOW) = k.parse::<u32>() {
                return (group, k);
            }
        }
    }
    (name, 1)
}

/// The highest window number. Far past any real use, and it keeps
/// `max + 1` from overflowing.
pub const MAX_WINDOW: u32 = 9999;

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
    /// that window's pid, the tiebreak for `created`
    pub pid: u32,
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
                    pid: 0,
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
        (g.created, g.pid) = ws
            .iter()
            .map(|(_, s)| (s.created, s.pid))
            .min()
            .unwrap_or((0, 0));
        g.clients = ws.iter().map(|(_, s)| s.clients).sum();
        let first = ws[0].1;
        g.cwd = first.cwd.clone();
        g.dir = first.dir.clone();
        g.windows = ws.iter().map(|(_, s)| s.name.clone()).collect();
    }
    out.sort_by_key(|g| (g.created, g.pid));
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

/// The session a typed name means: one that exists under exactly that name
/// (zmx itself takes `a.b`), else the name sanitized for a new session. A
/// window spelling (`a.b+2`) keeps its session part the same way.
pub fn typed_name(raw: &str, live: &[Session]) -> String {
    if live.iter().any(|s| s.name == raw) {
        return raw.to_string();
    }
    let (g, k) = window_of(raw);
    let g = match groups(live).iter().any(|x| x.name == g) {
        true => g.to_string(),
        false => sanitize_name(g),
    };
    match is_window_ref(raw) {
        true => format!("{g}+{k}"),
        false => g,
    }
}

/// Session names share zmx's socket directory, so `/` is out; tmux also forbade
/// `.` `:` and spaces and the user's scripts already map those to `_`.
/// Control characters go too, so names sluice makes draw cleanly. Names
/// read back from `zmx list` may still hold them: the picker cleans those
/// where it draws them. A leading `-` goes too: `sluice detach --session
/// --x` could not tell such a name from a flag.
pub fn sanitize_name(raw: &str) -> String {
    raw.chars()
        .enumerate()
        .map(|(i, c)| {
            if matches!(c, '/' | '.' | ':' | ',' | ' ') || c.is_control() || (i == 0 && c == '-') {
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
    fn labels_do_not_override_zmx_fields() {
        // what zmx prints after `zmx set s _name=evil created=1 .clients=9`
        let v = parse_list(
            "  name=s\tpid=5\tclients=0\tcreated=1792545152\tcwd=file://h/x\t.clients=9\t_name=evil\tcreated=1\n",
        );
        assert_eq!(v[0].name, "s");
        assert_eq!(v[0].created, 1792545152);
        assert_eq!(v[0].clients, 0);
    }

    #[test]
    fn cwd_is_percent_decoded() {
        // what zmx 0.8.1 prints for /private/tmp/sluice audit/café
        let v = parse_list(
            "  name=a\tpid=1\tclients=0\tcreated=1\tcwd=file://192.168.1.13/private/tmp/sluice%20audit/caf%C3%A9\n",
        );
        assert_eq!(v[0].dir, "/private/tmp/sluice audit/café");
        assert_eq!(percent_decode("/a%2"), "/a%2", "a short escape stays");
        assert_eq!(percent_decode("/a%zz%41"), "/a%zzA");
        assert_eq!(raw_path("file://host"), "", "no path, no directory");
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
            pid: created as u32,
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
        assert_eq!(window_of("x+01"), ("x+01", 1), "not another spelling of x");
        assert_eq!(window_of("x+4294967295"), ("x+4294967295", 1), "past the last window");
        assert_eq!(window_of("x+9999"), ("x", 9999));
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
    fn sessions_made_in_the_same_second_keep_their_order() {
        // zz was made first; a later `aa` in the same second sorts after it
        let text = "  name=aa\tpid=501\tclients=0\tcreated=7\n  name=zz\tpid=500\tclients=0\tcreated=7\n";
        let names: Vec<String> = parse_list(text).into_iter().map(|s| s.name).collect();
        assert_eq!(names, ["zz", "aa"]);
        let g = groups(&parse_list(text));
        assert_eq!(g[0].name, "zz");
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
        assert_eq!(sanitize_name("--x"), "_-x", "never read as a flag");
        assert_eq!(sanitize_name("a\tb\x1b[2Jc\0"), "a_b_[2Jc_", "no control bytes");
    }
}
