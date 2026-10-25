//! Full-screen pickers. fzf owns the tty while it runs; we only hand it lines on
//! stdin and read the choice back on stdout, exactly like the user's shell
//! scripts do. The look is our own: boxed input and list, prism's colours.

use crate::procs::{self, Running};
use crate::zmx::{Group, Session};
use std::collections::HashMap;
use std::io::Write;
use std::process::{Command, Stdio};

// glass's prism palette (glass/hooks/palette.ts), copied by role. Orange marks
// focus: the list box in nav mode, the prompt and its label in search mode.
pub(crate) const ORANGE: &str = "#ff9f1a"; // heading3
pub(crate) const BLUE: &str = "#4d96ff"; // cmd: keys in the hints, a working agent
pub(crate) const CYAN: &str = "#4eb4d3"; // str: search matches, an agent's shell
pub(crate) const GREEN: &str = "#32ff7e"; // ok: the session you are in, an idle agent
pub(crate) const YELLOW: &str = "#fff200"; // warn: an agent waiting on you
pub(crate) const TEXT: &str = "#dcdcdc"; // calloutText: session names
pub(crate) const WHITE: &str = "#ffffff"; // bold: the row under the cursor
pub(crate) const META: &str = "#9a9a9a"; // paths, counts
pub(crate) const NUM: &str = "#8a8a8a"; // orderedNum: row numbers
pub(crate) const FAINT: &str = "#6c6c6c"; // idle input box, hint words
pub(crate) const RULE: &str = "#565656"; // preview box
pub(crate) const ROW_BG: &str = "#0e2a3a"; // rowHover

/// `s` in a 24-bit colour, then reset.
pub(crate) fn paint(hex: &str, bold: bool, s: &str) -> String {
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
    // As written: the caller sanitizes it unless zmx runs that very name,
    // which a script such as claude-picker hands back.
    let name = fields.next()?.trim().to_string();
    if crate::zmx::sanitize_name(&name).is_empty() {
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

/// One display line per session:
/// `<n>  name  windows  clients  programs  cwd<TAB>name`. Windows are not
/// listed one by one -- a session is one row, with a count when it has more
/// than one, and what each window runs, in window order. `current` is the
/// window this pane shows ("" for none): its session gets the dot, and `2/3w`
/// when it is on window 2 of 3. `running` is `procs::of_all`, by zmx name; a
/// window missing from it shows as `?`. The hidden trailing field is what we
/// read back.
pub fn session_lines(
    groups: &[Group],
    current: &str,
    running: &HashMap<String, Running>,
) -> Vec<String> {
    let w = groups
        .iter()
        .map(|s| printable(&s.name).chars().count())
        .max()
        .unwrap_or(4)
        .max(4);
    let windows: Vec<String> = groups
        .iter()
        .map(|g| match (g.windows.len(), is_current(g, current)) {
            (0 | 1, _) => String::new(),
            (k, true) => match g.windows.iter().position(|x| x == current) {
                Some(at) => format!("{}/{k}w", at + 1),
                None => format!("{k}w"),
            },
            (k, false) => format!("{k}w"),
        })
        .collect();
    let ww = windows.iter().map(String::len).max().unwrap_or(0).max(2);
    let programs: Vec<(String, usize)> = groups.iter().map(|g| programs(g, running)).collect();
    let pw = programs.iter().map(|(_, n)| *n).max().unwrap_or(0);
    groups
        .iter()
        .zip(&windows)
        .zip(&programs)
        .enumerate()
        .map(|(i, ((s, windows), (progs, progs_w)))| {
            let mark = match is_current(s, current) {
                true => paint(GREEN, false, "●"),
                false => " ".into(),
            };
            let n = if i < 9 {
                (i + 1).to_string()
            } else {
                " ".into()
            };
            let clients = match s.clients {
                0 => "   ".to_string(),
                1 => " 1 ".to_string(),
                c => format!("{c:>2} "),
            };
            // pad before painting: escape codes would count toward the width
            format!(
                "{} {mark} {}  {}{progs}{}  {}\t{}",
                paint(NUM, false, &n),
                paint(TEXT, false, &format!("{:<w$}", printable(&s.name))),
                paint(META, false, &format!("{windows:<ww$} {clients} ")),
                " ".repeat(pw - progs_w),
                paint(META, false, &printable(&s.cwd)),
                s.name
            )
        })
        .collect()
}

/// The widest the program list of a session row gets, in columns.
const PROGRAMS_MAX: usize = 32;

/// `nvim · claude✻ · zsh`, painted, and its width on screen. An agent's mark
/// takes its status colour. Cut with `…` past `PROGRAMS_MAX`.
fn programs(g: &Group, running: &HashMap<String, Running>) -> (String, usize) {
    let mut out = String::new();
    let mut width = 0;
    for (i, w) in g.windows.iter().enumerate() {
        let r = running.get(w);
        let name = r.map_or_else(|| "?".to_string(), |r| printable(&r.name()).into_owned());
        let agent = r.and_then(|r| r.agent.as_ref());
        let sep = if i == 0 { "" } else { " · " };
        let piece = sep.chars().count() + name.chars().count() + usize::from(agent.is_some());
        // room for this one, and for a ` · …` after it unless it is the last
        let room = match i + 1 < g.windows.len() {
            true => PROGRAMS_MAX - 4,
            false => PROGRAMS_MAX,
        };
        if width + piece > room {
            out.push_str(&paint(META, false, &format!("{sep}…")));
            width += sep.chars().count() + 1;
            break;
        }
        out.push_str(&paint(META, false, &format!("{sep}{name}")));
        if let Some(a) = agent {
            out.push_str(&paint(agent_colour(&a.status), false, AGENT_MARK));
        }
        width += piece;
    }
    (out, width)
}

/// Claude's own glyph, so an agent's mark never reads as the green dot of the
/// session you are in.
pub(crate) const AGENT_MARK: &str = "✻";

/// The agent picker's colours: waiting needs you, idle is done, working is
/// busy rather than failed.
pub(crate) fn agent_colour(status: &str) -> &'static str {
    match status {
        "waiting" => YELLOW,
        "idle" => GREEN,
        "busy" => BLUE,
        "shell" => CYAN,
        _ => FAINT,
    }
}

/// `waiting`, `idle`, `working` for busy, else as Claude spells it.
pub(crate) fn agent_word(status: &str) -> String {
    match status {
        "busy" => "working".into(),
        s => printable(s).into_owned(),
    }
}

/// `3m`, `2h`, `4d` since `since_ms`; "" when unknown.
fn age(since_ms: u64, now_ms: u64) -> String {
    if since_ms == 0 || since_ms > now_ms {
        return String::new();
    }
    let m = (now_ms - since_ms) / 60_000;
    match m {
        0..=59 => format!("{m}m"),
        60..=1439 => format!("{}h", m / 60),
        _ => format!("{}d", m / 1440),
    }
}

/// The widest a window row's command gets.
const COMMAND_MAX: usize = 40;

/// One display line per window of a session, in window order:
/// `<n>  command  agent  cwd<TAB>zmx name<TAB>window`. Numbered by place, as
/// prefix+n steps through them, so 1-9 picks the row with that number.
/// `current` gets the dot. The zmx name feeds the preview; the window,
/// always spelled `<session>+<k>` so window 1 is not read as the whole
/// session, is what a kill acts on and what is read back.
pub fn window_lines(
    windows: &[&Session],
    current: &str,
    running: &HashMap<String, Running>,
    now_ms: u64,
) -> Vec<String> {
    let commands: Vec<String> = windows
        .iter()
        .map(|s| match running.get(&s.name) {
            Some(r) => printable(&r.command(COMMAND_MAX)).into_owned(),
            None => "?".into(),
        })
        .collect();
    let cw = commands.iter().map(|c| c.chars().count()).max().unwrap_or(0);
    let states: Vec<(String, &str)> = windows
        .iter()
        .map(|s| match running.get(&s.name).and_then(|r| r.agent.as_ref()) {
            Some(a) => {
                let text = [agent_word(&a.status), age(a.since_ms, now_ms)]
                    .join(" ")
                    .trim_end()
                    .to_string();
                (format!("{AGENT_MARK} {text}"), agent_colour(&a.status))
            }
            None => (String::new(), FAINT),
        })
        .collect();
    let sw = states.iter().map(|(t, _)| t.chars().count()).max().unwrap_or(0);
    windows
        .iter()
        .zip(commands.iter().zip(&states))
        .enumerate()
        .map(|(i, (s, (cmd, (state, colour))))| {
            let mark = match s.name == current {
                true => paint(GREEN, false, "●"),
                false => " ".into(),
            };
            let n = if i < 9 {
                (i + 1).to_string()
            } else {
                " ".into()
            };
            let (g, k) = crate::zmx::window_of(&s.name);
            let state = match sw {
                0 => String::new(),
                _ => format!("{}  ", paint(colour, false, &format!("{state:<sw$}"))),
            };
            format!(
                "{} {mark} {}  {state}{}\t{}\t{g}+{k}",
                paint(NUM, false, &n),
                paint(TEXT, false, &format!("{cmd:<cw$}")),
                paint(META, false, &printable(&s.cwd)),
                s.name,
            )
        })
        .collect()
}

/// The windows of `current`'s session, in window order.
pub fn session_windows<'a>(live: &'a [Session], current: &str) -> Vec<&'a Session> {
    let g = crate::zmx::window_of(current).0;
    let mut ws: Vec<(u32, &Session)> = live
        .iter()
        .map(|s| (crate::zmx::window_of(&s.name), s))
        .filter(|((sg, _), _)| *sg == g)
        .map(|((_, k), s)| (k, s))
        .collect();
    ws.sort_by_key(|(k, _)| *k);
    ws.into_iter().map(|(_, s)| s).collect()
}

/// Now, in ms since the epoch, for an agent's age.
pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

/// `current` names a window of `g` (or `g` itself).
/// A name or path from `zmx list` as drawn: zmx takes any name, and fzf
/// would read an escape in it as colour and count it toward the width.
pub(crate) fn printable(s: &str) -> std::borrow::Cow<'_, str> {
    match s.chars().any(char::is_control) {
        true => s.chars().map(|c| if c.is_control() { '_' } else { c }).collect(),
        false => s.into(),
    }
}

fn is_current(g: &Group, current: &str) -> bool {
    !current.is_empty() && crate::zmx::window_of(current).0 == g.name
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
/// list --rows`, so the picker shows the result rather than a stale row. No
/// sleep before it: `zmx kill` returns once the session is gone, and `sluice
/// detach` once zmx has dropped the client.
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
    sh_quote(&exe)
}

/// One shell word, whatever `s` holds.
pub fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
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
        format!("--bind=ctrl-x:execute-silent({exe} kill {{s2}})+reload({reload})"),
        format!("--bind=ctrl-d:execute-silent({exe} detach --session {{s2}})+reload({reload})"),
        format!("--bind=ctrl-l:reload({reload})"),
    ]
}

/// Run fzf over `lines`; returns the hidden field (after the last TAB) of the pick.
/// `label` titles the list box; `hints` is `key word · key word`, shown under it.
pub fn fzf(lines: &[String], label: &str, hints: &str, preview: Option<&str>) -> Option<String> {
    fzf_with(lines, label, hints, preview, &[], &[])
}

/// The session picker: one row per session, the screen of the one under the
/// cursor below, kill and detach keys. `current` is the window this pane
/// shows, "" for none; the cursor starts on its session.
pub fn pick_session(live: &[Session], current: &str, hints: &str) -> Option<String> {
    let groups = crate::zmx::groups(live);
    let lines = session_lines(&groups, current, &procs::of_all(live));
    let here = groups.iter().position(|g| is_current(g, current)).map(|i| i + 1);
    let mut extra = session_binds(current);
    extra.extend(focus_binds(here));
    fzf_with(
        &lines,
        "sessions",
        hints,
        Some("zmx history {s2} --vt"),
        &extra,
        &[("SLUICE_CURRENT", current)],
    )
}

/// The window picker: one row per window of `current`'s session with what it
/// runs, its screen below, a kill key. Opens on `current`. Returns the window
/// as `<session>+<k>`.
pub fn pick_window(live: &[Session], current: &str, hints: &str) -> Option<String> {
    let windows = session_windows(live, current);
    let running = procs::of_all(windows.iter().copied());
    let lines = window_lines(&windows, current, &running, now_ms());
    let here = windows.iter().position(|s| s.name == current).map(|i| i + 1);
    let mut extra = window_binds_with(&self_exe());
    extra.extend(focus_binds(here));
    // A fixed label: nav_binds splices it into fzf actions, where a `)` in
    // a session name would end one. The preview's label names the window.
    fzf_with(
        &lines,
        "windows",
        hints,
        Some("zmx history {s2} --vt"),
        &extra,
        &[("SLUICE_CURRENT", current)],
    )
}

/// Kill the window under the cursor, by its `<session>+<k>` spelling, which
/// never takes the session's other windows with it; redraw from `sluice list
/// --rows --windows`. The session reaches the reload through
/// `$SLUICE_CURRENT`, never spelled into the bind.
fn window_binds_with(exe: &str) -> Vec<String> {
    let reload = format!("{exe} list --rows --windows --current \"$SLUICE_CURRENT\"");
    vec![
        format!("--bind=ctrl-x:execute-silent({exe} kill {{s3}})+reload({reload})"),
        format!("--bind=ctrl-l:reload({reload})"),
    ]
}

/// Begin a synchronized update (DEC mode 2026): the terminal draws nothing
/// until `SYNC_END`, then shows the result as one frame. Terminals without
/// it ignore both.
pub const SYNC_BEGIN: &[u8] = b"\x1b[?2026h";
pub const SYNC_END: &[u8] = b"\x1b[?2026l";

/// Back from the alternate screen a picker drew on. Our fzf runs with
/// `--no-clear` and stays on its last frame when it exits, so the screen
/// under it, the window we left, is never shown on the way to the next
/// one: the caller leaves it inside a synchronized update, and the next
/// window's repaint lands in the same frame.
pub const LEAVE_PICKER: &[u8] = b"\x1b[?1049l";

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
    cmd.args(["--delimiter=\t", "--with-nth=1", "--nth=1", "--no-clear"]);
    cmd.args(style_args(label, hints, ORANGE));
    cmd.args(extra);
    if let Some(p) = preview {
        cmd.arg(format!("--preview={p}"));
        cmd.args(preview_args());
        cmd.arg("--bind=focus:transform-preview-label:printf ' %s ' {s2}");
    }
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            // The tty is ours and cooked here: say why nothing opens, and
            // keep it on screen until it is read. The update held open for
            // fzf's first frame ends here instead.
            let _ = crate::sys::write_all(crate::sys::STDOUT, SYNC_END);
            eprint!("sluice: cannot run fzf ({e}); the pickers need it. Press Enter.");
            let _ = std::io::stdin().read_line(&mut String::new());
            return None;
        }
    };
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

/// The preview's place and size, shared with the agent picker through
/// `sluice picker-style --preview`.
///
/// List on top at full width, the session's screen below it: side by side,
/// the list lost its directories to the preview's half. `zmx history` is
/// the whole scrollback, oldest first, so `follow` shows its end, the
/// screen. The preview takes 60% of the height, so two sessions still get
/// a list of a fair size; when the entries need more rows than the other
/// 40% holds, the list grows and the preview gives way, down to 5 lines.
/// Worked out again on every load (a kill reloads) and resize. 11 is fzf's
/// own lines around them: margins, the input box, the list's border and
/// footer, the preview's border.
pub fn preview_args() -> Vec<String> {
    let fit = "p=$((FZF_LINES * 60 / 100)); \
               fit=$((FZF_LINES - FZF_TOTAL_COUNT - 11)); \
               [ \"$fit\" -lt \"$p\" ] && p=$fit; [ \"$p\" -lt 5 ] && p=5; \
               echo \"change-preview-window($p)\"";
    vec![
        "--preview-window=down,60%,border-rounded,follow".to_string(),
        format!("--bind=load:transform:{fit}"),
        format!("--bind=resize:transform:{fit}"),
    ]
}

/// The look and the nav keys every sluice picker shares, `accent` marking
/// focus. Also printed by `sluice picker-style` for the bundled scripts, so
/// the agent picker cannot drift from this one. Fields and preview are the
/// caller's.
pub fn style_args(label: &str, hints: &str, accent: &str) -> Vec<String> {
    let mut v: Vec<String> = [
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
        // The binds' commands are POSIX sh; $SHELL may be fish or nu.
        "--with-shell=sh -c",
    ]
    .map(String::from)
    .to_vec();
    v.push(format!(
        "--color=bg+:{ROW_BG},fg+:{WHITE}:bold,gutter:-1,pointer:{accent},\
         list-border:{accent},list-label:{accent}:bold,\
         input-border:{FAINT},input-label:{FAINT},prompt:{FAINT},\
         preview-border:{RULE},preview-label:{NUM},\
         query:{TEXT},ghost:{RULE},info:{META},hl:{CYAN},hl+:{CYAN}:bold"
    ));
    v.push(format!("--prompt={NAV_PROMPT}"));
    v.push(format!("--list-label= {label} "));
    v.extend(nav_binds(label, accent));
    if !hints.is_empty() {
        v.push(format!("--footer={}", footer(hints)));
        // inside the list box, under a rule, rather than on a line of its own
        v.push("--footer-border=inline".into());
    }
    v
}

/// `#rrggbb`, the only colour form `paint` and fzf's `--color` share.
pub fn is_hex_colour(s: &str) -> bool {
    s.len() == 7 && s.starts_with('#') && s[1..].bytes().all(|b| b.is_ascii_hexdigit())
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
/// text instead: search mode paints the prompt and an input label in the
/// accent and dims the list label. Nav mode's plain text takes the `--color`
/// defaults.
fn nav_binds(label: &str, accent: &str) -> Vec<String> {
    let to_search = format!(
        "unbind({NAV_KEYS})+change-prompt({})+change-input-label( {} )+change-list-label( {} )+change-ghost()",
        paint(accent, true, "/ "),
        paint(accent, true, "search"),
        paint(FAINT, false, label),
    );
    // Back in nav mode the query goes: with it, row 1 of what is shown may
    // be drawn as "3", and the digits would pick by place, not by number.
    let to_nav = format!(
        "rebind({NAV_KEYS})+clear-query+change-prompt({NAV_PROMPT})+change-input-label()+change-list-label( {label} )+change-ghost(i search)"
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
    // fzf clamps pos(): on a list of 3, `7` would take row 3. Only a row
    // that is there is picked.
    for n in 1..=9 {
        v.push(format!(
            "--bind={n}:transform:[ \"$FZF_MATCH_COUNT\" -ge {n} ] && echo \"pos({n})+accept\""
        ));
    }
    v
}

/// Run a user script with the tty attached; parse its first stdout line.
/// `$SLUICE` names this binary, so a script can call back into the sluice
/// that ran it rather than whichever is first on PATH.
pub fn run_script(cmd: &str) -> Option<Choice> {
    let exe = std::env::current_exe().unwrap_or_else(|_| "sluice".into());
    let run = |mut c: Command| {
        c.envs([("SLUICE", &exe)])
            .stdin(Stdio::inherit())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .output()
    };
    // A bare path skips `sh -c`: the shim and the shell behind it are two
    // execs, ~10 ms of a picker's start on a CPU woken from idle. Anything
    // the kernel will not run, a script with no #! say, goes to sh as before.
    let direct = bare_path(cmd).and_then(|p| run(Command::new(p)).ok());
    let out = match direct {
        Some(o) => o,
        None => {
            let mut sh = Command::new("sh");
            sh.arg("-c").arg(cmd);
            run(sh).ok()?
        }
    };
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    text.lines().find_map(parse_choice)
}

/// `cmd` as a path sh would run unchanged: one word with a `/`, nothing sh
/// would expand or split, and a leading `~/` spelled out as sh would. A word
/// with no `/` may be a builtin or a keyword, so it stays with sh.
fn bare_path(cmd: &str) -> Option<std::path::PathBuf> {
    let cmd = cmd.trim();
    let plain = |s: &str| {
        s.contains('/')
            && s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"/._+-".contains(&b))
    };
    match cmd.strip_prefix("~/") {
        Some(rest) if plain(&format!("/{rest}")) => Some(crate::clients::home().join(rest)),
        Some(_) => None,
        None if plain(cmd) => Some(cmd.into()),
        None => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_plain_path_skips_the_shell() {
        let home = crate::clients::home();
        assert_eq!(
            bare_path("/opt/bin/sessionizer"),
            Some("/opt/bin/sessionizer".into())
        );
        assert_eq!(bare_path(" ./pick-it_2.sh "), Some("./pick-it_2.sh".into()));
        assert_eq!(
            bare_path("~/.config/sluice/bin/sessionizer"),
            Some(home.join(".config/sluice/bin/sessionizer"))
        );
        for sh in [
            "sessionizer",          // PATH, or a builtin
            "printf 'main\\n'",     // arguments
            "/opt/bin/pick --flag", // arguments
            "$HOME/bin/pick",       // expansion
            "~other/bin/pick",      // another user's home
            "/opt/bin/pick;rm",     // a list
            "/opt/bin/p*",          // a glob
            "FOO=1/bin",            // an assignment
            "~",                    // home itself
        ] {
            assert_eq!(bare_path(sh), None, "{sh}");
        }
    }

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
        assert_eq!(parse_choice("a.b\t").unwrap().name, "a.b", "the caller sanitizes");
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
        let b = nav_binds("sessions", ORANGE).join("\n");
        for needle in [
            "--bind=j:down",
            "--bind=k:up",
            "--bind=q:abort",
            "--bind=i:unbind(j,k,q,i,a,/,1,2,3,4,5,6,7,8,9)+change-prompt(\x1b[1;38;2;255;159;26m/ \x1b[0m)",
            "+change-input-label( \x1b[1;38;2;255;159;26msearch\x1b[0m )",
            "+change-list-label( \x1b[38;2;108;108;108msessions\x1b[0m )+change-ghost()",
            "--bind=/:unbind(",
            "--bind=5:transform:[ \"$FZF_MATCH_COUNT\" -ge 5 ] && echo \"pos(5)+accept\"",
            "--bind=9:transform:[ \"$FZF_MATCH_COUNT\" -ge 9 ] && echo \"pos(9)+accept\"",
            "--bind=esc:transform:[ \"$FZF_PROMPT\" = \"❯ \" ] && echo abort || echo \"rebind(j,k,q,i,a,/,1,2,3,4,5,6,7,8,9)+clear-query+change-prompt(❯ )+change-input-label()+change-list-label( sessions )+change-ghost(i search)\"",
        ] {
            assert!(b.contains(needle), "missing {needle}\n{b}");
        }
    }

    #[test]
    fn session_binds_act_on_the_hidden_name_and_reload() {
        let b = session_binds_with("S", "main").join("\n");
        assert!(
            b.contains("--bind=ctrl-x:execute-silent(S kill {s2})"),
            "{b}"
        );
        assert!(
            b.contains("--bind=ctrl-d:execute-silent(S detach --session {s2})"),
            "{b}"
        );
        assert!(
            b.contains("reload(S list --rows --current \"$SLUICE_CURRENT\")"),
            "{b}"
        );
        // with no current session the marker argument is left off entirely
        let b = session_binds_with("S", "").join("\n");
        assert!(b.contains("reload(S list --rows)"), "{b}");
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
            pid: 0,
            clients: c,
            cwd: "~/x".into(),
            dir: "/x".into(),
            windows: (1..=w)
                .map(|k| crate::zmx::window_name(n, k as u32))
                .collect(),
        };
        let lines: Vec<String> = session_lines(&[s("main", 1, 1), s("glass", 0, 3)], "glass", &none())
            .iter()
            .map(|l| strip_ansi(l))
            .collect();
        assert!(
            !lines[0].contains("w "),
            "one window: no count: {}",
            lines[0]
        );
        assert!(
            lines[1].contains("1/3w"),
            "the session you are in: which window of how many: {}",
            lines[1]
        );
        assert!(lines[0].starts_with("1   main"), "{}", lines[0]);
        assert!(lines[1].starts_with("2 ● glass"), "{}", lines[1]);
        assert!(lines[1].ends_with("~/x\tglass"));
        // padding happens before painting, so columns still line up
        let col = |l: &str| l.split("~/x").next().unwrap().chars().count();
        assert_eq!(col(&lines[0]), col(&lines[1]));

        // on window 3, elsewhere: just the count
        let lines: Vec<String> =
            session_lines(&[s("main", 1, 2), s("glass", 0, 3)], "glass+3", &none())
                .iter()
                .map(|l| strip_ansi(l))
                .collect();
        assert!(lines[1].contains("3/3w") && lines[1].contains('●'), "{}", lines[1]);
        assert!(lines[0].contains("  2w ") && !lines[0].contains("/2w"), "{}", lines[0]);
        assert_eq!(col(&lines[0]), col(&lines[1]), "2w pads to 3/3w");
    }

    #[test]
    fn session_lines_draw_control_characters_as_underscores() {
        let g = Group {
            name: "a\x1b[31mb".into(),
            created: 0,
            pid: 0,
            clients: 0,
            cwd: "~/\x07x".into(),
            dir: "/x".into(),
            windows: vec!["a\x1b[31mb".into()],
        };
        let line = &session_lines(&[g], "", &none())[0];
        let shown = line.split('\t').next().unwrap();
        assert!(strip_ansi(shown).contains("a_[31mb"), "{line:?}");
        assert!(strip_ansi(shown).contains("~/_x"), "{line:?}");
        assert!(line.ends_with("\ta\x1b[31mb"), "the key stays the real name");
    }

    fn none() -> HashMap<String, Running> {
        HashMap::new()
    }

    fn ran(args: &[&str], status: Option<&str>, since_ms: u64) -> Running {
        Running {
            args: args.iter().map(|a| a.to_string()).collect(),
            agent: status.map(|s| procs::Agent {
                status: s.into(),
                since_ms,
            }),
        }
    }

    fn win(name: &str) -> Session {
        Session {
            name: name.into(),
            created: 0,
            pid: 1,
            clients: 0,
            cwd: "~/g".into(),
            dir: "/g".into(),
        }
    }

    fn group(name: &str, windows: &[&str]) -> Group {
        Group {
            name: name.into(),
            created: 0,
            pid: 0,
            clients: 0,
            cwd: "~/x".into(),
            dir: "/x".into(),
            windows: windows.iter().map(|w| w.to_string()).collect(),
        }
    }

    #[test]
    fn session_rows_list_what_each_window_runs() {
        let running: HashMap<String, Running> = [
            ("glass".to_string(), ran(&["-zsh"], None, 0)),
            ("glass+2".to_string(), ran(&["/usr/bin/nvim", "."], None, 0)),
            ("glass+4".to_string(), ran(&["claude"], Some("waiting"), 0)),
            ("main".to_string(), ran(&["-zsh"], None, 0)),
        ]
        .into();
        let gs = [
            group("main", &["main"]),
            group("glass", &["glass", "glass+2", "glass+3", "glass+4"]),
        ];
        let raw = session_lines(&gs, "glass+2", &running);
        let lines: Vec<String> = raw.iter().map(|l| strip_ansi(l)).collect();
        assert!(lines[0].contains(" zsh "), "{}", lines[0]);
        // window order; a window that could not be read is `?`
        assert!(lines[1].contains("zsh · nvim · ? · claude✻"), "{}", lines[1]);
        // the agent's mark takes its status colour, yellow for waiting
        assert!(raw[1].contains("\x1b[38;2;255;242;0m✻"), "{:?}", raw[1]);
        let col = |l: &str| l.split("~/x").next().unwrap().chars().count();
        assert_eq!(col(&lines[0]), col(&lines[1]), "programs pad to the widest");
    }

    #[test]
    fn a_long_program_list_is_cut() {
        let names: Vec<String> = (1..=12).map(|k| crate::zmx::window_name("g", k)).collect();
        let running: HashMap<String, Running> = names
            .iter()
            .map(|n| (n.clone(), ran(&["longprogramname"], None, 0)))
            .collect();
        let refs: Vec<&str> = names.iter().map(String::as_str).collect();
        let (shown, width) = programs(&group("g", &refs), &running);
        let plain = strip_ansi(&shown);
        assert!(plain.ends_with(" · …"), "{plain}");
        assert_eq!(plain.chars().count(), width);
        assert!(width <= PROGRAMS_MAX, "{width}");
        // the last one fits exactly without a `…`
        let (shown, _) = programs(&group("g", &["g"]), &running);
        assert_eq!(strip_ansi(&shown), "longprogramname");
        // a long first one leaves room for ` · …` when more follow
        let wide: HashMap<String, Running> = [
            ("g".to_string(), ran(&["a".repeat(PROGRAMS_MAX - 1).as_str()], None, 0)),
            ("g+2".to_string(), ran(&["b"], None, 0)),
        ]
        .into();
        let (shown, width) = programs(&group("g", &["g", "g+2"]), &wide);
        assert_eq!(strip_ansi(&shown), "…");
        assert_eq!(width, 1);
        let (_, width) = programs(&group("g", &["g"]), &wide);
        assert_eq!(width, PROGRAMS_MAX - 1, "alone it fits");
    }

    #[test]
    fn window_rows_carry_the_command_the_agent_and_both_names() {
        let ws = [win("glass"), win("glass+3"), win("glass+4")];
        let refs: Vec<&Session> = ws.iter().collect();
        let now = 10 * 60_000;
        let running: HashMap<String, Running> = [
            ("glass".to_string(), ran(&["-zsh"], None, 0)),
            ("glass+3".to_string(), ran(&["nvim", "src/\x1bmain.rs"], None, 0)),
            ("glass+4".to_string(), ran(&["claude"], Some("busy"), 7 * 60_000)),
        ]
        .into();
        let lines: Vec<String> = window_lines(&refs, "glass+3", &running, now)
            .iter()
            .map(|l| strip_ansi(l))
            .collect();
        // numbered by place, as prefix+n steps; the dot on the current one
        assert!(lines[0].starts_with("1   zsh "), "{}", lines[0]);
        assert!(lines[1].starts_with("2 ● nvim src/_main.rs"), "{}", lines[1]);
        assert!(lines[2].starts_with("3   claude"), "{}", lines[2]);
        assert!(lines[2].contains("✻ working 3m"), "{}", lines[2]);
        // zmx name for the preview, then the window spelled out for the kill
        assert!(lines[0].ends_with("~/g\tglass\tglass+1"), "{}", lines[0]);
        assert!(lines[1].ends_with("\tglass+3\tglass+3"), "{}", lines[1]);
        let col = |l: &str| l.split("~/g").next().unwrap().chars().count();
        assert_eq!(col(&lines[0]), col(&lines[2]));
        assert_eq!(col(&lines[1]), col(&lines[2]));
    }

    #[test]
    fn window_rows_without_agents_have_no_empty_column() {
        let ws = [win("a"), win("a+2")];
        let refs: Vec<&Session> = ws.iter().collect();
        let lines: Vec<String> = window_lines(&refs, "a", &none(), 0)
            .iter()
            .map(|l| strip_ansi(l))
            .collect();
        assert_eq!(lines[0], "1 ● ?  ~/g\ta\ta+1");
    }

    #[test]
    fn ages_round_down_and_unknown_is_blank() {
        let m = 60_000;
        assert_eq!(age(0, 5 * m), "");
        assert_eq!(age(5 * m, m), "", "a clock behind the file");
        assert_eq!(age(m, m + 59_999), "0m");
        assert_eq!(age(m, 61 * m), "1h");
        assert_eq!(age(m, 3 * 1440 * m + m), "3d");
    }

    #[test]
    fn session_windows_keep_window_order_and_their_own_session() {
        let live = [win("g+10"), win("other"), win("g"), win("g+2"), win("gg+2")];
        let names: Vec<&str> = session_windows(&live, "g+2")
            .iter()
            .map(|s| s.name.as_str())
            .collect();
        assert_eq!(names, ["g", "g+2", "g+10"]);
    }

    #[test]
    fn window_binds_kill_the_spelled_window_and_reload_from_the_env() {
        let b = window_binds_with("S").join("\n");
        assert!(b.contains("--bind=ctrl-x:execute-silent(S kill {s3})"), "{b}");
        assert!(
            b.contains("reload(S list --rows --windows --current \"$SLUICE_CURRENT\")"),
            "{b}"
        );
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
    fn the_shared_style_takes_the_accent_and_leaves_fields_alone() {
        let v = style_args("✻ agents", "q back", "#8c6ff0");
        let all = v.join("\n");
        assert!(all.contains("pointer:#8c6ff0,list-border:#8c6ff0"), "{all}");
        assert!(all.contains("\x1b[1;38;2;140;111;240m/ "), "search prompt in the accent");
        assert!(all.contains("--list-label= ✻ agents "));
        assert!(all.contains("echo \"pos(5)+accept\""), "digits jump in every picker");
        assert!(all.contains("--footer="));
        assert!(
            !v.iter().any(|a| a.starts_with("--nth") || a.starts_with("--with-nth") || a.starts_with("--delimiter")),
            "fields belong to the caller"
        );
        assert!(is_hex_colour("#8c6ff0") && !is_hex_colour("red") && !is_hex_colour("#12345g"));
    }

    #[test]
    fn footer_paints_keys_and_words() {
        let f = strip_ansi(&footer("j/k move · enter switch · q back"));
        assert_eq!(f, "j/k move  enter switch  q back");
        assert!(footer("q back").starts_with("\x1b[1;38;2;77;150;255mq\x1b[0m"));
    }
}
