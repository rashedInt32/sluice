//! Full-screen pickers. fzf owns the tty while it runs; we only hand it lines on
//! stdin and read the choice back on stdout, exactly like the user's shell
//! scripts do, so FZF_DEFAULT_OPTS and their colours apply unchanged.

use crate::zmx::Group;
use std::io::Write;
use std::process::{Command, Stdio};

pub struct Choice {
    pub name: String,
    pub cwd: Option<String>,
    /// Program to start when the session has to be created. Ignored for a
    /// session that is already running, which is what makes prefix+y both
    /// "launch" and "re-attach".
    pub cmd: Option<Vec<String>>,
}

impl Choice {
    pub fn named(name: impl Into<String>) -> Choice {
        Choice {
            name: name.into(),
            cwd: None,
            cmd: None,
        }
    }
}

/// `name`, `name<TAB>dir`, or `name<TAB>dir<TAB>command args` from a line of
/// script output. The command is split on whitespace: a script that needs
/// quoting should wrap the program in one of its own.
pub fn parse_choice(line: &str) -> Option<Choice> {
    let line = line.trim_end_matches(['\r', '\n']);
    if line.trim().is_empty() {
        return None;
    }
    let mut fields = line.split('\t');
    let name = crate::zmx::sanitize_name(fields.next()?.trim());
    if name.is_empty() {
        return None;
    }
    let cwd = fields
        .next()
        .map(str::trim)
        .filter(|d| !d.is_empty())
        .map(str::to_string);
    let cmd = fields
        .next()
        .map(str::trim)
        .filter(|c| !c.is_empty())
        .map(|c| c.split_whitespace().map(str::to_string).collect::<Vec<_>>());
    Some(Choice { name, cwd, cmd })
}

/// One display line per session: `<n>  name  windows  clients  cwd<TAB>name`.
/// Windows are not listed one by one -- a session is one row, with a count
/// when it has more than one. The hidden trailing field is what we read back.
pub fn session_lines(groups: &[Group], current: &str) -> Vec<String> {
    let w = groups
        .iter()
        .map(|s| s.name.chars().count())
        .max()
        .unwrap_or(4)
        .max(4);
    groups
        .iter()
        .enumerate()
        .map(|(i, s)| {
            let mark = if s.name == current { '*' } else { ' ' };
            let n = if i < 9 {
                (i + 1).to_string()
            } else {
                " ".into()
            };
            let windows = match s.windows.len() {
                0 | 1 => "   ".to_string(),
                k => format!("{k}w "),
            };
            let clients = match s.clients {
                0 => "    ".to_string(),
                1 => " 1 ".to_string(),
                c => format!("{c:>2} "),
            };
            format!(
                "{n:>1}{mark} {:<w$}  {windows}{clients} {}\t{}",
                s.name, s.cwd, s.name
            )
        })
        .collect()
}

/// Extra keys for a picker whose rows are sessions: kill one, or free it from
/// whatever pane is holding it, then redraw the list in place.
///
/// Both act on the hidden name in field 2. The list is reloaded from `sluice
/// list --rows`, so the picker shows the result rather than a stale row.
/// The kill is immediate, takes every window of the session, and there is no
/// undo, which the header says.
pub fn session_binds(current: &str) -> Vec<String> {
    session_binds_with(&self_exe(), current)
}

/// The sluice binary that is running, quoted for the shell fzf hands bind
/// commands to. Calling `sluice` by name would pick up whichever one is first
/// on PATH, which after an upgrade can be an older build without the very
/// subcommand the key needs.
fn self_exe() -> String {
    let exe = std::env::current_exe()
        .ok()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|| "sluice".to_string());
    format!("'{}'", exe.replace('\'', r"'\''"))
}

fn session_binds_with(exe: &str, current: &str) -> Vec<String> {
    let reload = match current.is_empty() {
        true => format!("{exe} list --rows"),
        false => format!("{exe} list --rows --current {current}"),
    };
    vec![
        format!("--bind=ctrl-x:execute-silent({exe} kill {{2}})+reload(sleep 0.2; {reload})"),
        format!("--bind=ctrl-d:execute-silent({exe} detach --session {{2}})+reload(sleep 0.2; {reload})"),
        format!("--bind=ctrl-l:reload({reload})"),
    ]
}

/// Run fzf over `lines`; returns the hidden field (after the last TAB) of the pick.
pub fn fzf(lines: &[String], prompt: &str, header: &str, preview: Option<&str>) -> Option<String> {
    fzf_with(lines, prompt, header, preview, &[])
}

/// As `fzf`, plus extra `--bind` arguments.
pub fn fzf_with(
    lines: &[String],
    prompt: &str,
    header: &str,
    preview: Option<&str>,
    extra: &[String],
) -> Option<String> {
    let mut cmd = Command::new("fzf");
    cmd.args([
        "--delimiter=\t",
        "--with-nth=1",
        "--nth=1",
        "--ansi",
        "--layout=reverse",
        "--no-sort",
        "--cycle",
        "--info=inline",
        "--tiebreak=index",
    ]);
    cmd.arg(format!("--prompt={prompt}"));
    cmd.args(nav_binds(prompt));
    cmd.args(extra);
    if !header.is_empty() {
        cmd.arg(format!("--header={header}"));
    }
    if let Some(p) = preview {
        cmd.arg(format!("--preview={p}"));
        // List on top at full width, the session's screen below it. Side by
        // side, the list lost its directories to the preview's half.
        cmd.arg("--preview-window=down,50%,border-top");
    }
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());
    let mut child = cmd.spawn().ok()?;
    {
        let mut stdin = child.stdin.take()?;
        for l in lines {
            let _ = writeln!(stdin, "{l}");
        }
    }
    let out = child.wait_with_output().ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let line = text.lines().next()?;
    Some(line.rsplit('\t').next().unwrap_or(line).to_string())
}

const NAV_KEYS: &str = "j,k,q,i,a,/,1,2,3,4,5,6,7,8,9";
pub const SEARCH_PROMPT: &str = "search > ";

/// Open in a vim-style nav mode rather than fzf's filter mode: the lists here
/// are short, so moving beats typing. j/k move, 1-9 pick that row outright,
/// q or Esc close. i, a or / switch to filtering with the keys freed up for
/// typing; Esc there returns to nav. Same shape as the user's tmux pickers.
fn nav_binds(nav_prompt: &str) -> Vec<String> {
    let to_search = format!("unbind({NAV_KEYS})+change-prompt({SEARCH_PROMPT})");
    let mut v = vec![
        "--bind=j:down".to_string(),
        "--bind=k:up".to_string(),
        "--bind=q:abort".to_string(),
        format!("--bind=i:{to_search}"),
        format!("--bind=a:{to_search}"),
        format!("--bind=/:{to_search}"),
        // Esc closes in nav mode, returns to nav mode from search mode.
        format!(
            "--bind=esc:transform:[ \"$FZF_PROMPT\" = \"{nav_prompt}\" ] && echo abort || echo \"rebind({NAV_KEYS})+change-prompt({nav_prompt})\""
        ),
    ];
    for n in 1..=9 {
        v.push(format!("--bind={n}:pos({n})+accept"));
    }
    v
}

/// Run a user script with the tty attached; parse its first stdout line.
pub fn run_script(cmd: &str) -> Option<Choice> {
    let out = Command::new("sh")
        .arg("-c")
        .arg(cmd)
        .stdin(Stdio::inherit())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    text.lines().find_map(parse_choice)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn choice_parsing() {
        let c = parse_choice("glass\t/Users/x/glass\n").unwrap();
        assert_eq!(c.name, "glass");
        assert_eq!(c.cwd.as_deref(), Some("/Users/x/glass"));
        assert!(c.cmd.is_none());
        let c = parse_choice("eddy").unwrap();
        assert_eq!(c.name, "eddy");
        assert!(c.cwd.is_none());
        assert!(parse_choice("\n").is_none());
        assert!(
            parse_choice("\t/tmp").is_none(),
            "a row with no name is not a choice"
        );
        assert_eq!(parse_choice("a.b\t").unwrap().name, "a_b");
    }

    #[test]
    fn choice_carries_a_command_for_a_new_session() {
        let c = parse_choice("claude_glass\t/Users/x/glass\tclaude --model opus").unwrap();
        assert_eq!(c.name, "claude_glass");
        assert_eq!(c.cwd.as_deref(), Some("/Users/x/glass"));
        assert_eq!(c.cmd.unwrap(), vec!["claude", "--model", "opus"]);
        // an empty third field is no command at all
        assert!(parse_choice("x\t/tmp\t").unwrap().cmd.is_none());
    }

    #[test]
    fn nav_binds_cover_move_jump_search_and_close() {
        let b = nav_binds("session > ").join("\n");
        for needle in [
            "--bind=j:down",
            "--bind=k:up",
            "--bind=q:abort",
            "--bind=i:unbind(j,k,q,i,a,/,1,2,3,4,5,6,7,8,9)+change-prompt(search > )",
            "--bind=/:unbind(",
            "--bind=5:pos(5)+accept",
            "--bind=9:pos(9)+accept",
            "--bind=esc:transform:[ \"$FZF_PROMPT\" = \"session > \" ] && echo abort || echo \"rebind(j,k,q,i,a,/,1,2,3,4,5,6,7,8,9)+change-prompt(session > )\"",
        ] {
            assert!(b.contains(needle), "missing {needle}\n{b}");
        }
    }

    #[test]
    fn session_binds_act_on_the_hidden_name_and_reload() {
        let b = session_binds_with("S", "main").join("\n");
        assert!(
            b.contains("--bind=ctrl-x:execute-silent(S kill {2})"),
            "{b}"
        );
        assert!(
            b.contains("--bind=ctrl-d:execute-silent(S detach --session {2})"),
            "{b}"
        );
        assert!(
            b.contains("reload(sleep 0.2; S list --rows --current main)"),
            "{b}"
        );
        // with no current session the marker argument is left off entirely
        let b = session_binds_with("S", "").join("\n");
        assert!(b.contains("reload(sleep 0.2; S list --rows)"), "{b}");
        assert!(!b.contains("--current"), "{b}");
    }

    #[test]
    fn binds_call_the_running_binary_quoted() {
        let exe = self_exe();
        assert!(exe.starts_with('\'') && exe.ends_with('\''), "{exe}");
        assert!(session_binds("x").join("\n").contains(&exe));
    }

    #[test]
    fn session_lines_number_and_mark() {
        let s = |n: &str, c: u32, w: usize| Group {
            name: n.into(),
            created: 0,
            clients: c,
            cwd: "~/x".into(),
            dir: "/x".into(),
            windows: (1..=w)
                .map(|k| crate::zmx::window_name(n, k as u32))
                .collect(),
        };
        let lines = session_lines(&[s("main", 1, 1), s("glass", 0, 3)], "glass");
        assert!(
            !lines[0].contains("w "),
            "one window: no count: {}",
            lines[0]
        );
        assert!(
            lines[1].contains("3w"),
            "several windows: a count: {}",
            lines[1]
        );
        assert!(lines[0].starts_with("1  main"), "{}", lines[0]);
        assert!(lines[1].starts_with("2* glass"), "{}", lines[1]);
        assert!(lines[1].ends_with("\tglass"));
    }
}
