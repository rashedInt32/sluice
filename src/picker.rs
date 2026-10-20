//! Full-screen pickers. fzf owns the tty while it runs; we only hand it lines on
//! stdin and read the choice back on stdout, exactly like the user's shell
//! scripts do. The look is our own: boxed input and list, prism's colours.

use crate::zmx::Group;
use std::io::Write;
use std::process::{Command, Stdio};

// glass's prism palette (glass/hooks/palette.ts), copied by role. Orange marks
// focus: the list box in nav mode, the prompt and its label in search mode.
const ORANGE: &str = "#ff9f1a"; // heading3
const BLUE: &str = "#4d96ff"; // cmd: keys in the hints
const CYAN: &str = "#4eb4d3"; // str: search matches
const GREEN: &str = "#32ff7e"; // ok: the session you are in
const TEXT: &str = "#dcdcdc"; // calloutText: session names
const WHITE: &str = "#ffffff"; // bold: the row under the cursor
const META: &str = "#9a9a9a"; // paths, counts
const NUM: &str = "#8a8a8a"; // orderedNum: row numbers
const FAINT: &str = "#6c6c6c"; // idle input box, hint words
const RULE: &str = "#565656"; // preview box
const ROW_BG: &str = "#0e2a3a"; // rowHover

/// `s` in a 24-bit colour, then reset.
fn paint(hex: &str, bold: bool, s: &str) -> String {
    let ch = |i: usize| u8::from_str_radix(&hex[1 + 2 * i..3 + 2 * i], 16).unwrap_or(0);
    let b = if bold { "1;" } else { "" };
    format!("\x1b[{b}38;2;{};{};{}m{s}\x1b[0m", ch(0), ch(1), ch(2))
}

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
///
/// A line with a NUL is no choice at all: no path or argument can hold one.
pub fn parse_choice(line: &str) -> Option<Choice> {
    let line = line.trim_end_matches(['\r', '\n']);
    if line.trim().is_empty() || line.contains('\0') {
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
            let mark = match s.name == current {
                true => paint(GREEN, false, "●"),
                false => " ".into(),
            };
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
                0 => "   ".to_string(),
                1 => " 1 ".to_string(),
                c => format!("{c:>2} "),
            };
            // pad before painting: escape codes would count toward the width
            format!(
                "{} {mark} {}  {}{}\t{}",
                paint(NUM, false, &n),
                paint(TEXT, false, &format!("{:<w$}", s.name)),
                paint(META, false, &format!("{windows}{clients} ")),
                paint(META, false, &s.cwd),
                s.name
            )
        })
        .collect()
}

/// Open the picker with the cursor on row `index` (1-based), the session you
/// are in, the way tmux's own session list does: you see where you are, j/k
/// move from there, and a stray Enter keeps you put.
///
/// `--sync` holds the first draw until the list is in, so `start:pos` lands on
/// a real row instead of being clamped against an empty one. `--track` then
/// keeps the cursor on the same session when ctrl-x or ctrl-d redraws the list
/// and the rows above it shift.
pub fn focus_binds(index: Option<usize>) -> Vec<String> {
    match index {
        Some(n) if n >= 1 => vec![
            "--sync".into(),
            "--track".into(),
            format!("--bind=start:pos({n})"),
        ],
        _ => Vec::new(),
    }
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

/// The current session reaches the reload through `$SLUICE_CURRENT`, never
/// spelled into the bind: a name can come from a git branch, and a `)` or `;`
/// in it would be read by fzf's action parser or by sh.
fn session_binds_with(exe: &str, current: &str) -> Vec<String> {
    let reload = match current.is_empty() {
        true => format!("{exe} list --rows"),
        false => format!("{exe} list --rows --current \"$SLUICE_CURRENT\""),
    };
    vec![
        format!("--bind=ctrl-x:execute-silent({exe} kill {{2}})+reload(sleep 0.2; {reload})"),
        format!("--bind=ctrl-d:execute-silent({exe} detach --session {{2}})+reload(sleep 0.2; {reload})"),
        format!("--bind=ctrl-l:reload({reload})"),
    ]
}

/// Run fzf over `lines`; returns the hidden field (after the last TAB) of the pick.
/// `label` titles the list box; `hints` is `key word · key word`, shown under it.
pub fn fzf(lines: &[String], label: &str, hints: &str, preview: Option<&str>) -> Option<String> {
    fzf_with(lines, label, hints, preview, &[], &[])
}

/// The session picker: one row per session, the screen of the one under the
/// cursor below, kill and detach keys. `current` is the session this pane
/// shows, "" for none; the cursor starts on it.
pub fn pick_session(groups: &[Group], current: &str, hints: &str) -> Option<String> {
    let lines = session_lines(groups, current);
    let here = groups.iter().position(|g| g.name == current).map(|i| i + 1);
    let mut extra = session_binds(current);
    extra.extend(focus_binds(here));
    fzf_with(
        &lines,
        "sessions",
        hints,
        Some("zmx history {2} --vt --screen"),
        &extra,
        &[("SLUICE_CURRENT", current)],
    )
}

/// As `fzf`, plus extra arguments, and variables for fzf and its binds.
fn fzf_with(
    lines: &[String],
    label: &str,
    hints: &str,
    preview: Option<&str>,
    extra: &[String],
    vars: &[(&str, &str)],
) -> Option<String> {
    let mut cmd = Command::new("fzf");
    cmd.envs(vars.iter().copied());
    cmd.args([
        "--delimiter=\t",
        "--with-nth=1",
        "--nth=1",
        "--ansi",
        "--layout=reverse",
        "--no-sort",
        "--cycle",
        "--info=inline-right",
        "--tiebreak=index",
        "--margin=1,2",
        "--input-border=rounded",
        "--list-border=rounded",
        "--highlight-line",
        "--pointer=▌",
        // fzf draws ▌ down the gutter of every row by default
        "--gutter= ",
        "--ghost=i search",
    ]);
    cmd.arg(format!(
        "--color=bg+:{ROW_BG},fg+:{WHITE}:bold,gutter:-1,pointer:{ORANGE},\
         list-border:{ORANGE},list-label:{ORANGE}:bold,\
         input-border:{FAINT},input-label:{FAINT},prompt:{FAINT},\
         preview-border:{RULE},preview-label:{NUM},\
         query:{TEXT},ghost:{RULE},info:{META},hl:{CYAN},hl+:{CYAN}:bold"
    ));
    cmd.arg(format!("--prompt={NAV_PROMPT}"));
    cmd.arg(format!("--list-label= {label} "));
    cmd.args(nav_binds(label));
    cmd.args(extra);
    if !hints.is_empty() {
        cmd.arg(format!("--footer={}", footer(hints)));
        // inside the list box, under a rule, rather than on a line of its own
        cmd.arg("--footer-border=inline");
    }
    if let Some(p) = preview {
        cmd.arg(format!("--preview={p}"));
        // List on top at full width, the session's screen below it. Side by
        // side, the list lost its directories to the preview's half.
        cmd.arg("--preview-window=down,50%,border-rounded");
        cmd.arg("--bind=focus:transform-preview-label:printf ' %s ' {2}");
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

/// `j/k move · q back` as blue keys and faint words.
fn footer(hints: &str) -> String {
    hints
        .split(" · ")
        .map(|h| match h.split_once(' ') {
            Some((key, word)) => format!("{} {}", paint(BLUE, true, key), paint(FAINT, false, word)),
            None => paint(BLUE, true, h),
        })
        .collect::<Vec<_>>()
        .join("  ")
}

const NAV_KEYS: &str = "j,k,q,i,a,/,1,2,3,4,5,6,7,8,9";
/// Plain on purpose: Esc tells the modes apart by comparing `$FZF_PROMPT`.
const NAV_PROMPT: &str = "❯ ";

/// Open in a vim-style nav mode rather than fzf's filter mode: the lists here
/// are short, so moving beats typing. j/k move, 1-9 pick that row outright,
/// q or Esc close. i, a or / switch to filtering with the keys freed up for
/// typing; Esc there returns to nav. Same shape as the user's tmux pickers.
///
/// fzf cannot recolour a border once it is drawn, so focus moves through the
/// text instead: search mode paints the prompt and an input label orange and
/// dims the list label. Nav mode's plain text takes the `--color` defaults.
fn nav_binds(label: &str) -> Vec<String> {
    let to_search = format!(
        "unbind({NAV_KEYS})+change-prompt({})+change-input-label( {} )+change-list-label( {} )+change-ghost()",
        paint(ORANGE, true, "/ "),
        paint(ORANGE, true, "search"),
        paint(FAINT, false, label),
    );
    let to_nav = format!(
        "rebind({NAV_KEYS})+change-prompt({NAV_PROMPT})+change-input-label()+change-list-label( {label} )+change-ghost(i search)"
    );
    let mut v = vec![
        "--bind=j:down".to_string(),
        "--bind=k:up".to_string(),
        "--bind=q:abort".to_string(),
        format!("--bind=i:{to_search}"),
        format!("--bind=a:{to_search}"),
        format!("--bind=/:{to_search}"),
        // Esc closes in nav mode, returns to nav mode from search mode.
        format!(
            "--bind=esc:transform:[ \"$FZF_PROMPT\" = \"{NAV_PROMPT}\" ] && echo abort || echo \"{to_nav}\""
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
        assert!(parse_choice("x\0y\t/tmp").is_none(), "NUL in the name");
        assert!(parse_choice("x\t/tm\0p").is_none(), "NUL in the directory");
        assert!(parse_choice("x\t/tmp\tsh -c\0").is_none(), "NUL in the command");
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
        let b = nav_binds("sessions").join("\n");
        for needle in [
            "--bind=j:down",
            "--bind=k:up",
            "--bind=q:abort",
            "--bind=i:unbind(j,k,q,i,a,/,1,2,3,4,5,6,7,8,9)+change-prompt(\x1b[1;38;2;255;159;26m/ \x1b[0m)",
            "+change-input-label( \x1b[1;38;2;255;159;26msearch\x1b[0m )",
            "+change-list-label( \x1b[38;2;108;108;108msessions\x1b[0m )+change-ghost()",
            "--bind=/:unbind(",
            "--bind=5:pos(5)+accept",
            "--bind=9:pos(9)+accept",
            "--bind=esc:transform:[ \"$FZF_PROMPT\" = \"❯ \" ] && echo abort || echo \"rebind(j,k,q,i,a,/,1,2,3,4,5,6,7,8,9)+change-prompt(❯ )+change-input-label()+change-list-label( sessions )+change-ghost(i search)\"",
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
            b.contains("reload(sleep 0.2; S list --rows --current \"$SLUICE_CURRENT\")"),
            "{b}"
        );
        // with no current session the marker argument is left off entirely
        let b = session_binds_with("S", "").join("\n");
        assert!(b.contains("reload(sleep 0.2; S list --rows)"), "{b}");
        assert!(!b.contains("--current"), "{b}");
    }

    #[test]
    fn a_hostile_session_name_never_reaches_the_binds() {
        let name = "x)+execute(touch${IFS}pwned);$(id)'q";
        let b = session_binds_with("S", name).join("\n");
        assert!(!b.contains("pwned"), "{b}");
        // what the reload runs gets the whole name back as one argument
        let out = std::process::Command::new("sh")
            .arg("-c")
            .arg("printf %s \"$SLUICE_CURRENT\"")
            .envs([("SLUICE_CURRENT", name)])
            .output()
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&out.stdout), name);
    }

    #[test]
    fn the_picker_opens_on_the_current_session() {
        assert_eq!(
            focus_binds(Some(3)),
            ["--sync", "--track", "--bind=start:pos(3)"]
        );
        // no current session (the startup picker): plain top-of-list
        assert!(focus_binds(None).is_empty());
        assert!(focus_binds(Some(0)).is_empty(), "there is no row 0");
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
        let lines: Vec<String> = session_lines(&[s("main", 1, 1), s("glass", 0, 3)], "glass")
            .iter()
            .map(|l| strip_ansi(l))
            .collect();
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
        assert!(lines[0].starts_with("1   main"), "{}", lines[0]);
        assert!(lines[1].starts_with("2 ● glass"), "{}", lines[1]);
        assert!(lines[1].ends_with("~/x\tglass"));
        // padding happens before painting, so columns still line up
        let col = |l: &str| l.split("~/x").next().unwrap().chars().count();
        assert_eq!(col(&lines[0]), col(&lines[1]));
    }

    fn strip_ansi(s: &str) -> String {
        let mut out = String::new();
        let mut chars = s.chars();
        while let Some(c) = chars.next() {
            if c == '\x1b' {
                chars.by_ref().find(|&c| c == 'm');
            } else {
                out.push(c);
            }
        }
        out
    }

    #[test]
    fn footer_paints_keys_and_words() {
        let f = strip_ansi(&footer("j/k move · enter switch · q back"));
        assert_eq!(f, "j/k move  enter switch  q back");
        assert!(footer("q back").starts_with("\x1b[1;38;2;77;150;255mq\x1b[0m"));
    }
}
