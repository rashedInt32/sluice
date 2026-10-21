//! What each window is running, for the pickers. zmx reports the pid each
//! window started with, the shell; its terminal knows which process group is
//! in the foreground, and that group's leader is the program on screen: nvim,
//! claude, a build, or the shell itself at its prompt. Read in-process, no
//! `ps`, so a whole list costs microseconds.
//!
//! A Claude Code agent also says what it is doing in
//! `~/.claude/sessions/<pid>.json`, the file the agent picker reads.

use crate::sys;
use crate::zmx::Session;
use std::collections::HashMap;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Running {
    /// argv as the program was started, argv[0] first
    pub args: Vec<String>,
    /// set when the program is a Claude Code agent that reports its state
    pub agent: Option<Agent>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Agent {
    /// `waiting`, `idle`, `busy`, `shell`, or whatever a later Claude says
    pub status: String,
    /// when the status last changed, ms since the epoch; 0 when unknown
    pub since_ms: u64,
}

impl Running {
    /// The program's short name: `nvim`, `zsh` for a login `-zsh`, `claude`
    /// for an agent however it was started (`node .../cli.js`).
    pub fn name(&self) -> String {
        if self.agent.is_some() {
            return "claude".into();
        }
        base_name(self.args.first().map(String::as_str).unwrap_or(""))
    }

    /// The name and its arguments, `nvim src/main.rs`, cut to `max` chars.
    pub fn command(&self, max: usize) -> String {
        let mut words = vec![self.name()];
        if self.agent.is_none() {
            words.extend(self.args.iter().skip(1).cloned());
        }
        clip(&words.join(" "), max)
    }
}

/// `/opt/homebrew/bin/nvim` -> `nvim`, `-zsh` -> `zsh`.
fn base_name(argv0: &str) -> String {
    let b = argv0.rsplit('/').next().unwrap_or(argv0);
    let b = b.strip_prefix('-').unwrap_or(b);
    if b.is_empty() {
        "?".into()
    } else {
        b.to_string()
    }
}

/// At most `max` chars, the last one `…` when cut.
fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// What the window whose first process is `pid` has in the foreground.
pub fn of(pid: u32) -> Option<Running> {
    if pid == 0 {
        return None;
    }
    let fg = sys::tty_foreground(pid)?;
    // A group outlives its first process (`a | b` after `a` ends): its id
    // then names no process, and there is nothing to show. Another user's
    // process keeps its arguments to itself; its name will do.
    let args = sys::proc_args(fg).or_else(|| sys::proc_name(fg).map(|n| vec![n]))?;
    let agent = match looks_like_claude(&args) {
        true => agent(fg),
        false => None,
    };
    Some(Running { args, agent })
}

/// Each of `windows`, by zmx name. Windows that could not be read are left
/// out.
pub fn of_all<'a>(windows: impl IntoIterator<Item = &'a Session>) -> HashMap<String, Running> {
    windows
        .into_iter()
        .filter_map(|s| of(s.pid).map(|r| (s.name.clone(), r)))
        .collect()
}

/// `claude`, or an install that runs it as `node .../claude-code/cli.js`.
/// Not any command that mentions it: `nvim ~/.claude/settings.json` at the
/// pid of a killed agent would read that agent's stale state file.
fn looks_like_claude(args: &[String]) -> bool {
    let Some(argv0) = args.first() else {
        return false;
    };
    base_name(argv0) == "claude"
        || args
            .iter()
            .take(2)
            .any(|a| a.contains("claude-code/") || a.contains("@anthropic-ai/claude"))
}

/// The state Claude Code keeps for the agent at `pid`.
fn agent(pid: u32) -> Option<Agent> {
    let base = match std::env::var("CLAUDE_CONFIG_DIR") {
        Ok(d) if !d.is_empty() => std::path::PathBuf::from(d),
        _ => crate::clients::home().join(".claude"),
    };
    let text = std::fs::read_to_string(base.join("sessions").join(format!("{pid}.json"))).ok()?;
    parse_agent(&text, pid)
}

/// The top-level `status` of an interactive session file that is about `pid`.
fn parse_agent(text: &str, pid: u32) -> Option<Agent> {
    let fields = top_level_fields(text);
    let get = |k: &str| fields.iter().find(|(key, _)| key == k).map(|(_, v)| v.as_str());
    if get("kind") != Some("interactive") || get("pid") != Some(pid.to_string().as_str()) {
        return None;
    }
    let status = get("status").filter(|s| !s.is_empty())?.to_string();
    let since_ms = get("statusUpdatedAt")
        .or_else(|| get("updatedAt"))
        .and_then(|v| v.parse::<f64>().ok())
        .filter(|v| v.is_finite() && *v > 0.0)
        .map_or(0, |v| v as u64);
    Some(Agent { status, since_ms })
}

/// The string and number members of a JSON object, by key, top level only;
/// nested objects and arrays are stepped over. Enough for the session file
/// without a JSON crate. String values keep simple escapes undone; anything
/// that is not an object yields nothing.
fn top_level_fields(text: &str) -> Vec<(String, String)> {
    let b = text.as_bytes();
    let mut out = Vec::new();
    let mut i = skip_ws(b, 0);
    if b.get(i) != Some(&b'{') {
        return out;
    }
    i += 1;
    loop {
        i = skip_ws(b, i);
        match b.get(i) {
            Some(b'"') => {}
            _ => return out,
        }
        let Some((key, next)) = read_string(b, i) else {
            return out;
        };
        i = skip_ws(b, next);
        if b.get(i) != Some(&b':') {
            return out;
        }
        i = skip_ws(b, i + 1);
        match b.get(i) {
            Some(b'"') => {
                let Some((v, next)) = read_string(b, i) else {
                    return out;
                };
                out.push((key, v));
                i = next;
            }
            Some(b'{' | b'[') => match skip_nested(b, i) {
                Some(next) => i = next,
                None => return out,
            },
            Some(_) => {
                let end = b[i..]
                    .iter()
                    .position(|c| matches!(c, b',' | b'}') || c.is_ascii_whitespace())
                    .map_or(b.len(), |p| i + p);
                out.push((key, text[i..end].to_string()));
                i = end;
            }
            None => return out,
        }
        i = skip_ws(b, i);
        match b.get(i) {
            Some(b',') => i += 1,
            _ => return out,
        }
    }
}

fn skip_ws(b: &[u8], mut i: usize) -> usize {
    while b.get(i).is_some_and(u8::is_ascii_whitespace) {
        i += 1;
    }
    i
}

/// The string starting at the quote `b[i]`, and the index after its close.
fn read_string(b: &[u8], i: usize) -> Option<(String, usize)> {
    let mut s = Vec::new();
    let mut j = i + 1;
    loop {
        match *b.get(j)? {
            b'"' => return Some((String::from_utf8_lossy(&s).into_owned(), j + 1)),
            b'\\' => {
                let c = *b.get(j + 1)?;
                match c {
                    b'n' => s.push(b'\n'),
                    b't' => s.push(b'\t'),
                    b'r' => s.push(b'\r'),
                    b'"' | b'\\' | b'/' => s.push(c),
                    // \uXXXX and the rest kept as written: only the status
                    // words are read, and they are plain
                    _ => s.extend_from_slice(&[b'\\', c]),
                }
                j += 2;
            }
            c => {
                s.push(c);
                j += 1;
            }
        }
    }
}

/// The index after the object or array opening at `b[i]`.
fn skip_nested(b: &[u8], i: usize) -> Option<usize> {
    let mut depth = 0usize;
    let mut j = i;
    while j < b.len() {
        match b[j] {
            b'"' => {
                j = read_string(b, j)?.1;
                continue;
            }
            b'{' | b'[' => depth += 1,
            b'}' | b']' => {
                depth -= 1;
                if depth == 0 {
                    return Some(j + 1);
                }
            }
            _ => {}
        }
        j += 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(args: &[&str]) -> Running {
        Running {
            args: args.iter().map(|s| s.to_string()).collect(),
            agent: None,
        }
    }

    #[test]
    fn names_drop_the_path_and_the_login_dash() {
        assert_eq!(run(&["/opt/homebrew/bin/nvim", "."]).name(), "nvim");
        assert_eq!(run(&["-zsh"]).name(), "zsh");
        assert_eq!(run(&[""]).name(), "?");
        assert_eq!(run(&["/"]).name(), "?");
    }

    #[test]
    fn commands_keep_their_arguments_and_are_cut() {
        assert_eq!(run(&["/usr/bin/nvim", "src/main.rs"]).command(40), "nvim src/main.rs");
        assert_eq!(run(&["cargo", "test", "--release"]).command(10), "cargo tes…");
        let mut a = run(&["node", "/x/claude-code/cli.js", "--resume"]);
        a.agent = Some(Agent {
            status: "idle".into(),
            since_ms: 0,
        });
        assert_eq!(a.command(40), "claude");
    }

    #[test]
    fn claude_is_told_by_the_whole_command() {
        assert!(looks_like_claude(&["claude".into()]));
        assert!(looks_like_claude(&["node".into(), "/x/@anthropic-ai/claude-code/cli.js".into()]));
        assert!(looks_like_claude(&["/opt/homebrew/bin/claude".into(), "--resume".into()]));
        assert!(!looks_like_claude(&["nvim".into(), ".".into()]));
        assert!(!looks_like_claude(&["nvim".into(), "/x/.claude/settings.json".into()]));
        assert!(!looks_like_claude(&["claude-picker".into()]));
        assert!(!looks_like_claude(&[]));
    }

    const FILE: &str = r#"{"pid":46053,"sessionId":"7","cwd":"/x","peerFeatures":["notify_idle","a\"status\":\"x"],"kind":"interactive","nested":{"status":"no"},"status":"busy","updatedAt":1792582037000,"statusUpdatedAt":1792582037894}"#;

    #[test]
    fn the_session_file_gives_status_and_time() {
        assert_eq!(
            parse_agent(FILE, 46053),
            Some(Agent {
                status: "busy".into(),
                since_ms: 1792582037894
            })
        );
    }

    #[test]
    fn a_file_about_another_pid_or_kind_is_not_read() {
        assert_eq!(parse_agent(FILE, 1), None);
        let bg = FILE.replace("interactive", "background");
        assert_eq!(parse_agent(&bg, 46053), None);
    }

    #[test]
    fn nested_members_and_escapes_do_not_leak_to_the_top() {
        let text = r#"{ "pid" : 9 , "x" : {"status":"no","y":[1,{"z":"}"}]}, "kind":"interactive", "status" : "wait\"ing" }"#;
        assert_eq!(parse_agent(text, 9).unwrap().status, "wait\"ing");
        // no status: nothing to show
        assert_eq!(parse_agent(r#"{"pid":9,"kind":"interactive"}"#, 9), None);
    }

    #[test]
    fn broken_files_yield_nothing() {
        for t in ["", "[]", "{", r#"{"pid":9,"kind":"interactive","status":"#, "{\"pid\"", "null"] {
            assert_eq!(parse_agent(t, 9), None, "{t:?}");
        }
    }

    #[test]
    fn a_missing_time_is_zero() {
        let t = r#"{"pid":9,"kind":"interactive","status":"idle"}"#;
        assert_eq!(parse_agent(t, 9).unwrap().since_ms, 0);
    }
}
