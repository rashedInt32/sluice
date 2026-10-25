//! prefix+b: the sessions as a panel floating over the right edge of the
//! screen, herdr's sidebar without the layout shift.
//!
//! sluice has no screen model, and does not need one here: while the panel is
//! open the session's output is held back, so the screen under it stays as it
//! was, and the panel simply draws over those cells. Closing reattaches the
//! window, and zmx's repaint puts back every cell the panel covered, inside
//! one synchronized update, the way a picker leaves.
//!
//! This file is the panel's state, rows and drawing. The runner owns the
//! loop: input, the held-back output, signals, the refresh tick.

use crate::keys::{Key, NavKey};
use crate::picker::{
    agent_colour, agent_word, printable, AGENT_MARK, BLUE, FAINT, GREEN, META, NUM, ORANGE,
    ROW_BG, RULE, TEXT, WHITE, YELLOW,
};
use crate::procs::Running;
use crate::zmx::Group;
use std::collections::HashMap;
use std::fmt::Write;

/// Narrowest and widest the panel gets, border included. Between the two it
/// takes what its longest row needs.
const MIN_WIDTH: usize = 30;
const MAX_WIDTH: usize = 40;

pub const HIDE_CURSOR: &[u8] = b"\x1b[?25l";

/// One session, as the panel lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub group: String,
    /// its prefix+1..9 number: creation order, the same as `jump`
    pub number: Option<usize>,
    pub windows: usize,
    /// the most urgent agent status among its windows
    pub agent: Option<String>,
    pub current: bool,
}

/// waiting needs you, idle is done, working can wait.
fn urgency(status: &str) -> u8 {
    match status {
        "waiting" => 0,
        "idle" => 1,
        "busy" | "shell" => 2,
        _ => 3,
    }
}

/// The rows for `groups` (creation order), sessions with a waiting agent
/// first. A row keeps its creation number wherever it sorts, so `3` still
/// means what prefix+3 means.
pub fn rows(groups: &[Group], current: &str, running: &HashMap<String, Running>) -> Vec<Row> {
    let cur = crate::zmx::window_of(current).0;
    let mut rows: Vec<Row> = groups
        .iter()
        .enumerate()
        .map(|(i, g)| Row {
            group: g.name.clone(),
            number: (i < 9).then_some(i + 1),
            windows: g.windows.len(),
            agent: g
                .windows
                .iter()
                .filter_map(|w| running.get(w)?.agent.as_ref())
                .min_by_key(|a| urgency(&a.status))
                .map(|a| a.status.clone()),
            current: !current.is_empty() && g.name == cur,
        })
        .collect();
    // stable: creation order holds inside each half
    rows.sort_by_key(|r| r.agent.as_deref() != Some("waiting"));
    rows
}

/// Where the panel sits, in 1-based terminal cells.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Geometry {
    /// column of the left border; the column before it is a blank gap
    pub left: usize,
    /// row of the top border
    pub top: usize,
    pub width: usize,
    /// list rows shown
    pub visible: usize,
}

impl Geometry {
    /// Row of the first list entry.
    fn first_row(&self) -> usize {
        self.top + 1
    }
}

/// `● name ◆3`'s name part: the name, and the window count past one.
fn name_text(r: &Row) -> String {
    let name = printable(&r.group).into_owned();
    match r.windows {
        0 | 1 => name,
        k => format!("{name} ◆{k}"),
    }
}

fn tag_text(r: &Row) -> String {
    r.agent
        .as_deref()
        .map(|s| format!("{AGENT_MARK} {}", agent_word(s)))
        .unwrap_or_default()
}

fn tag_width(rows: &[Row]) -> usize {
    rows.iter().map(|r| tag_text(r).chars().count()).max().unwrap_or(0)
}

/// The panel's place on a `cols` x `lines` screen, or `None` when it does not
/// fit: it keeps a blank column on its left, one free column on its right and
/// one free row above and below.
pub fn geometry(rows: &[Row], cols: usize, lines: usize) -> Option<Geometry> {
    let tag = tag_width(rows);
    let name = rows.iter().map(|r| name_text(r).chars().count()).max().unwrap_or(0);
    // pointer, number, mark and their spaces; the tag and its space; a
    // trailing space; the two borders
    let need = 5 + name + if tag > 0 { tag + 1 } else { 0 } + 1 + 2;
    let width = need.clamp(MIN_WIDTH, MAX_WIDTH).min(cols.saturating_sub(3));
    // top border, rule, footer, bottom border, a free row above and below
    let room = lines.saturating_sub(6);
    if width < 20 || room == 0 {
        return None;
    }
    Some(Geometry {
        left: cols - width,
        top: 2,
        width,
        visible: rows.len().max(1).min(room),
    })
}

/// What a key did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    /// nothing to draw
    Stay,
    Redraw,
    /// close, switching to this session if one is named
    Close(Option<String>),
    /// kill this session (confirmed), then refresh
    Kill(String),
}

pub struct Panel {
    rows: Vec<Row>,
    cursor: usize,
    /// first row shown when the list is longer than the panel
    top: usize,
    /// a session `x` asked about, waiting for `y`
    confirm: Option<String>,
    /// the prefix came, the next key may close
    armed: bool,
    prefix: Key,
    /// the keys bound to the sidebar: alone or after the prefix, they close it
    toggles: Vec<Key>,
    /// where the last frame went, for clicks
    geometry: Option<Geometry>,
}

impl Panel {
    /// Open on the first session with a waiting agent, so Enter takes you to
    /// it, else on the session you are in.
    pub fn new(rows: Vec<Row>, prefix: Key, toggles: Vec<Key>) -> Panel {
        let cursor = rows
            .iter()
            .position(|r| r.agent.as_deref() == Some("waiting"))
            .or_else(|| rows.iter().position(|r| r.current))
            .unwrap_or(0);
        Panel {
            rows,
            cursor,
            top: 0,
            confirm: None,
            armed: false,
            prefix,
            toggles,
            geometry: None,
        }
    }

    /// New rows from a refresh; the cursor stays on the same session.
    /// Whether anything changed, so an idle refresh draws nothing.
    pub fn set_rows(&mut self, rows: Vec<Row>) -> bool {
        if rows == self.rows {
            return false;
        }
        let at = self.rows.get(self.cursor).map(|r| r.group.clone());
        self.cursor = at
            .and_then(|g| rows.iter().position(|r| r.group == g))
            .unwrap_or(self.cursor.min(rows.len().saturating_sub(1)));
        if let Some(c) = &self.confirm {
            if !rows.iter().any(|r| &r.group == c) {
                self.confirm = None;
            }
        }
        self.rows = rows;
        true
    }

    fn selected(&self) -> Option<String> {
        self.rows.get(self.cursor).map(|r| r.group.clone())
    }

    fn step(&mut self, delta: isize) -> Step {
        let n = self.rows.len() as isize;
        if n == 0 {
            return Step::Stay;
        }
        self.cursor = (((self.cursor as isize + delta) % n + n) % n) as usize;
        Step::Redraw
    }

    pub fn key(&mut self, k: NavKey) -> Step {
        if let Some(group) = self.confirm.take() {
            return match k {
                NavKey::Key(Key::Char('y' | 'Y')) => Step::Kill(group),
                _ => Step::Redraw,
            };
        }
        if std::mem::take(&mut self.armed) {
            return match k {
                NavKey::Key(key) if self.toggles.contains(&key) => Step::Close(None),
                _ => Step::Stay,
            };
        }
        match k {
            NavKey::Key(key) if key == self.prefix => {
                self.armed = true;
                Step::Stay
            }
            NavKey::Key(key) if self.toggles.contains(&key) => Step::Close(None),
            NavKey::Key(Key::Char('q') | Key::Esc | Key::Ctrl('c')) => Step::Close(None),
            NavKey::Down | NavKey::WheelDown | NavKey::Key(Key::Char('j') | Key::Ctrl('n')) => {
                self.step(1)
            }
            NavKey::Up | NavKey::WheelUp | NavKey::Key(Key::Char('k') | Key::Ctrl('p')) => {
                self.step(-1)
            }
            NavKey::Key(Key::Ctrl('m' | 'j')) => Step::Close(self.selected()),
            NavKey::Key(Key::Char(c @ '1'..='9')) => {
                let n = c as usize - '0' as usize;
                match self.rows.iter().find(|r| r.number == Some(n)) {
                    Some(r) => Step::Close(Some(r.group.clone())),
                    None => Step::Stay,
                }
            }
            NavKey::Key(Key::Char('x')) => {
                self.confirm = self.selected();
                Step::Redraw
            }
            NavKey::Click { col, row } => {
                let Some(g) = self.geometry else {
                    return Step::Stay;
                };
                let inside = col > g.left && col < g.left + g.width - 1;
                let first = g.first_row();
                if inside && row >= first && row < first + g.visible {
                    let i = self.top + row - first;
                    if i < self.rows.len() {
                        self.cursor = i;
                        return Step::Close(self.selected());
                    }
                }
                Step::Stay
            }
            _ => Step::Stay,
        }
    }

    /// The panel for a `cols` x `lines` screen, cursor moves and all; empty
    /// when it does not fit.
    pub fn draw(&mut self, cols: usize, lines: usize) -> Vec<u8> {
        let Some(g) = geometry(&self.rows, cols, lines) else {
            self.geometry = None;
            return Vec::new();
        };
        self.geometry = Some(g);
        if self.cursor < self.top {
            self.top = self.cursor;
        }
        if self.cursor >= self.top + g.visible {
            self.top = self.cursor + 1 - g.visible;
        }
        self.top = self.top.min(self.rows.len().saturating_sub(g.visible));
        let inner = g.width - 2;
        let mut out = String::new();
        let line = |out: &mut String, row: usize, body: String| {
            // the gap column first, so the panel never touches the text
            let _ = write!(out, "\x1b[{row};{}H\x1b[0m {body}\x1b[0m", g.left - 1);
        };
        let b = |s: &str| seg(ORANGE, None, false, s);

        // ╭─ sessions · 1 waiting ───╮
        let waiting = self
            .rows
            .iter()
            .filter(|r| r.agent.as_deref() == Some("waiting"))
            .count();
        let mut label = seg(ORANGE, None, true, " sessions ");
        let mut label_w = 10;
        let extra = format!("· {waiting} waiting ");
        if waiting > 0 && label_w + extra.chars().count() + 2 <= inner {
            label.push_str(&seg(YELLOW, None, false, &extra));
            label_w += extra.chars().count();
        }
        let fill = inner.saturating_sub(1 + label_w);
        line(
            &mut out,
            g.top,
            format!("{}{label}{}", b("╭─"), b(&format!("{}╮", "─".repeat(fill)))),
        );

        let tag_w = tag_width(&self.rows);
        let name_w = inner - 6 - if tag_w > 0 { tag_w + 1 } else { 0 };
        for i in 0..g.visible {
            let body = match self.rows.get(self.top + i) {
                Some(r) => self.row_text(r, self.top + i == self.cursor, name_w, tag_w),
                None => seg(FAINT, None, false, &pad(" no sessions", inner)),
            };
            line(&mut out, g.first_row() + i, format!("{}{body}{}", b("│"), b("│")));
        }

        line(
            &mut out,
            g.first_row() + g.visible,
            format!("{}{}{}", b("├"), seg(RULE, None, false, &"─".repeat(inner)), b("┤")),
        );
        let footer = match &self.confirm {
            Some(c) => {
                let name: String = printable(c).chars().take(inner.saturating_sub(15)).collect();
                seg(YELLOW, None, true, &pad(&format!(" kill {name}? y/n"), inner))
            }
            None => hints(inner),
        };
        line(
            &mut out,
            g.first_row() + g.visible + 1,
            format!("{}{footer}{}", b("│"), b("│")),
        );
        line(
            &mut out,
            g.first_row() + g.visible + 2,
            b(&format!("╰{}╯", "─".repeat(inner))),
        );
        out.into_bytes()
    }

    /// `▌3 ● glass ◆2   ✻ waiting ` in `inner` columns.
    fn row_text(&self, r: &Row, on: bool, name_w: usize, tag_w: usize) -> String {
        let bg = on.then_some(ROW_BG);
        let pointer = match on {
            true => seg(ORANGE, bg, false, "▌"),
            false => seg(TEXT, bg, false, " "),
        };
        let n = r.number.map_or(" ".to_string(), |n| n.to_string());
        let mark = match r.current {
            true => "●",
            false => " ",
        };
        let (name_fg, bold) = if on { (WHITE, true) } else { (TEXT, false) };
        let name = clip(&printable(&r.group), name_w);
        let mut s = format!(
            "{pointer}{}{}{}",
            seg(NUM, bg, false, &n),
            seg(GREEN, bg, false, &format!(" {mark} ")),
            seg(name_fg, bg, bold, &name),
        );
        let mut used = name.chars().count();
        if r.windows > 1 {
            let w = format!(" ◆{}", r.windows);
            if used + w.chars().count() <= name_w {
                used += w.chars().count();
                s.push_str(&seg(META, bg, false, &w));
            }
        }
        s.push_str(&seg(TEXT, bg, false, &" ".repeat(name_w - used)));
        if tag_w > 0 {
            let tag = tag_text(r);
            let colour = r.agent.as_deref().map_or(FAINT, agent_colour);
            s.push_str(&seg(colour, bg, false, &format!(" {tag:<tag_w$}")));
        }
        s.push_str(&seg(TEXT, bg, false, " "));
        s
    }
}

/// `j/k  ⏎ go  x kill  q close`, as many as fit in `inner`.
fn hints(inner: usize) -> String {
    let all = [("j/k", ""), ("⏎", "go"), ("x", "kill"), ("q", "close")];
    let mut s = seg(FAINT, None, false, " ");
    let mut used = 1;
    for (i, (key, word)) in all.iter().enumerate() {
        let gap = if i == 0 { 0 } else { 2 };
        let word = match word.is_empty() {
            true => String::new(),
            false => format!(" {word}"),
        };
        let w = gap + key.chars().count() + word.chars().count();
        if used + w > inner {
            break;
        }
        s.push_str(&" ".repeat(gap));
        s.push_str(&seg(BLUE, None, true, key));
        s.push_str(&seg(FAINT, None, false, &word));
        used += w;
    }
    s.push_str(&seg(FAINT, None, false, &" ".repeat(inner - used)));
    s
}

/// `s` in `fg` on `bg` (the terminal's own background when `None`), starting
/// from plain so no attribute of the program's leaks in.
fn seg(fg: &str, bg: Option<&str>, bold: bool, s: &str) -> String {
    let rgb = |hex: &str| {
        let ch = |i: usize| u8::from_str_radix(&hex[1 + 2 * i..3 + 2 * i], 16).unwrap_or(0);
        format!("{};{};{}", ch(0), ch(1), ch(2))
    };
    let back = match bg {
        Some(h) => format!(";48;2;{}", rgb(h)),
        None => String::new(),
    };
    let weight = if bold { ";1" } else { "" };
    format!("\x1b[0{weight}{back};38;2;{}m{s}", rgb(fg))
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

/// `s` cut or padded to exactly `w` chars.
fn pad(s: &str, w: usize) -> String {
    format!("{:<w$}", clip(s, w))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::procs::Agent;

    fn group(name: &str, windows: &[&str]) -> Group {
        Group {
            name: name.into(),
            created: 0,
            pid: 0,
            clients: 0,
            cwd: String::new(),
            dir: String::new(),
            windows: windows.iter().map(|w| w.to_string()).collect(),
        }
    }

    fn agent(status: &str) -> Running {
        Running {
            args: vec!["claude".into()],
            agent: Some(Agent {
                status: status.into(),
                since_ms: 0,
            }),
        }
    }

    fn row(name: &str, number: usize, agent: Option<&str>) -> Row {
        Row {
            group: name.into(),
            number: Some(number),
            windows: 1,
            agent: agent.map(String::from),
            current: false,
        }
    }

    /// The text a frame puts on screen, row by row: cursor moves become
    /// line breaks, colours go.
    fn visible(frame: &[u8]) -> Vec<String> {
        let s = String::from_utf8_lossy(frame);
        let mut lines = Vec::new();
        let mut cur = String::new();
        let mut chars = s.chars().peekable();
        while let Some(c) = chars.next() {
            if c != '\x1b' {
                cur.push(c);
                continue;
            }
            chars.next(); // [
            let mut fin = ' ';
            for d in chars.by_ref() {
                if d.is_ascii_alphabetic() {
                    fin = d;
                    break;
                }
            }
            if fin == 'H' && !cur.is_empty() {
                lines.push(std::mem::take(&mut cur));
            }
        }
        lines.push(cur);
        lines
    }

    #[test]
    fn waiting_sessions_sort_first_and_keep_their_numbers() {
        let gs = [
            group("main", &["main"]),
            group("glass", &["glass", "glass+2"]),
            group("sluice", &["sluice"]),
        ];
        let running: HashMap<String, Running> = [
            ("glass+2".to_string(), agent("busy")),
            ("sluice".to_string(), agent("waiting")),
        ]
        .into();
        let r = rows(&gs, "glass+2", &running);
        let names: Vec<&str> = r.iter().map(|r| r.group.as_str()).collect();
        assert_eq!(names, ["sluice", "main", "glass"]);
        assert_eq!(r[0].number, Some(3), "prefix+3 is still sluice");
        assert!(r[2].current, "on a window of glass is in glass");
        assert_eq!(r[2].windows, 2);
        assert_eq!(r[2].agent.as_deref(), Some("busy"));
    }

    #[test]
    fn a_session_shows_its_most_urgent_agent() {
        let gs = [group("g", &["g", "g+2", "g+3"])];
        let running: HashMap<String, Running> = [
            ("g".to_string(), agent("busy")),
            ("g+2".to_string(), agent("idle")),
            ("g+3".to_string(), agent("waiting")),
        ]
        .into();
        assert_eq!(rows(&gs, "", &running)[0].agent.as_deref(), Some("waiting"));
        let running: HashMap<String, Running> =
            [("g".to_string(), agent("busy")), ("g+2".to_string(), agent("idle"))].into();
        assert_eq!(rows(&gs, "", &running)[0].agent.as_deref(), Some("idle"));
    }

    #[test]
    fn only_nine_sessions_get_a_number() {
        let gs: Vec<Group> = (0..11).map(|i| group(&format!("s{i}"), &[])).collect();
        let r = rows(&gs, "", &HashMap::new());
        assert_eq!(r[8].number, Some(9));
        assert_eq!(r[9].number, None);
    }

    #[test]
    fn the_panel_floats_on_the_right_and_sizes_to_its_rows() {
        let rs = [row("main", 1, None), row("glass", 2, Some("waiting"))];
        let g = geometry(&rs, 120, 40).unwrap();
        assert_eq!(g.width, MIN_WIDTH, "short rows: the narrowest");
        assert_eq!(g.left + g.width, 120, "one free column on the right");
        assert_eq!(g.top, 2, "one free row above");
        assert_eq!(g.visible, 2);

        let long = [row(&"x".repeat(60), 1, Some("waiting"))];
        assert_eq!(geometry(&long, 120, 40).unwrap().width, MAX_WIDTH);
        assert_eq!(geometry(&long, 30, 40).unwrap().width, 27, "a narrow screen");

        // more sessions than rows: a free row below, the list scrolls
        let many: Vec<Row> = (1..=50).map(|i| row(&format!("s{i}"), i, None)).collect();
        let g = geometry(&many, 120, 20).unwrap();
        assert_eq!(g.visible, 14);
        assert!(g.top + g.visible + 3 < 20, "bottom border above the last row");

        assert_eq!(geometry(&rs, 22, 40), None, "too narrow");
        assert_eq!(geometry(&rs, 120, 6), None, "too short");
    }

    #[test]
    fn every_line_of_the_frame_is_the_same_width() {
        let mut rs = vec![
            row("main", 1, None),
            row("a-very-long-session-name-indeed", 2, Some("waiting")),
            row("glass", 3, Some("busy")),
        ];
        rs[2].windows = 3;
        rs[0].current = true;
        let mut p = Panel::new(rs, Key::Ctrl('a'), vec![Key::Char('b')]);
        let frame = p.draw(100, 30);
        let lines = visible(&frame);
        let g = p.geometry.unwrap();
        assert_eq!(lines.len(), 3 + 4);
        for l in &lines {
            // the gap column, then the box
            assert_eq!(l.chars().count(), g.width + 1, "{l:?}");
        }
        assert!(lines[0].contains("sessions · 1 waiting"), "{}", lines[0]);
        assert!(lines[1].contains(" 1 ● main"), "{}", lines[1]);
        assert!(lines[2].contains("▌2   a-very-long-session"), "opens on the waiting one: {}", lines[2]);
        assert!(lines[2].contains("…"), "a long name is cut: {}", lines[2]);
        assert!(lines[2].contains("✻ waiting"), "{}", lines[2]);
        assert!(lines[3].contains("glass ◆3"), "{}", lines[3]);
        assert!(lines[3].contains("✻ working"), "{}", lines[3]);
        assert!(lines[5].contains("j/k  ⏎ go  x kill  q close"), "all four hints fit: {}", lines[5]);
    }

    #[test]
    fn the_frame_draws_only_inside_its_box() {
        let mut p = Panel::new(vec![row("main", 1, None)], Key::Ctrl('a'), vec![]);
        let frame = String::from_utf8(p.draw(80, 24)).unwrap();
        let g = p.geometry.unwrap();
        // every cursor move lands on the gap column of a row inside the box
        let moves: Vec<(usize, usize)> = frame
            .split("\x1b[")
            .filter_map(|s| {
                let (pos, _) = s.split_once('H')?;
                let (r, c) = pos.split_once(';')?;
                Some((r.parse().ok()?, c.parse().ok()?))
            })
            .collect();
        assert_eq!(moves.len(), 5);
        for (r, c) in moves {
            assert_eq!(c, g.left - 1);
            assert!(r >= g.top && r < g.top + 5, "{r}");
        }
        assert!(!frame.contains('\n') && !frame.contains('\r'), "never scrolls the screen");
    }

    #[test]
    fn keys_move_wrap_jump_and_close() {
        let rs = vec![row("a", 1, None), row("b", 2, None), row("c", 3, None)];
        let mut p = Panel::new(rs, Key::Ctrl('a'), vec![Key::Char('b')]);
        assert_eq!(p.cursor, 0);
        assert_eq!(p.key(NavKey::Up), Step::Redraw);
        assert_eq!(p.cursor, 2, "wraps to the bottom");
        p.key(NavKey::Key(Key::Char('j')));
        assert_eq!(p.cursor, 0, "and back");
        p.key(NavKey::WheelDown);
        assert_eq!(p.key(NavKey::Key(Key::Ctrl('m'))), Step::Close(Some("b".into())));
        assert_eq!(p.key(NavKey::Key(Key::Char('3'))), Step::Close(Some("c".into())));
        assert_eq!(p.key(NavKey::Key(Key::Char('7'))), Step::Stay, "no session 7");
        assert_eq!(p.key(NavKey::Key(Key::Char('q'))), Step::Close(None));
        assert_eq!(p.key(NavKey::Key(Key::Esc)), Step::Close(None));
        assert_eq!(p.key(NavKey::Key(Key::Char('b'))), Step::Close(None), "the toggle alone");
        // prefix then the toggle closes; prefix then anything else is dropped
        assert_eq!(p.key(NavKey::Key(Key::Ctrl('a'))), Step::Stay);
        assert_eq!(p.key(NavKey::Key(Key::Char('b'))), Step::Close(None));
        p.key(NavKey::Key(Key::Ctrl('a')));
        assert_eq!(p.key(NavKey::Key(Key::Char('2'))), Step::Stay, "not a jump");
        assert_eq!(p.key(NavKey::Key(Key::Char('2'))), Step::Close(Some("b".into())));
    }

    #[test]
    fn kill_asks_first() {
        let mut p = Panel::new(vec![row("a", 1, None), row("b", 2, None)], Key::Ctrl('a'), vec![]);
        p.key(NavKey::Down);
        assert_eq!(p.key(NavKey::Key(Key::Char('x'))), Step::Redraw);
        let frame = visible(&p.draw(80, 24));
        assert!(frame.iter().any(|l| l.contains("kill b? y/n")), "{frame:?}");
        assert_eq!(p.key(NavKey::Key(Key::Char('n'))), Step::Redraw, "anything but y keeps it");
        assert_eq!(p.key(NavKey::Key(Key::Char('q'))), Step::Close(None), "and the prompt is gone");
        p.key(NavKey::Key(Key::Char('x')));
        assert_eq!(p.key(NavKey::Key(Key::Char('y'))), Step::Kill("b".into()));
    }

    #[test]
    fn a_click_on_a_row_picks_it() {
        let rs = vec![row("a", 1, None), row("b", 2, None)];
        let mut p = Panel::new(rs, Key::Ctrl('a'), vec![]);
        p.draw(80, 24);
        let g = p.geometry.unwrap();
        let col = g.left + 3;
        assert_eq!(
            p.key(NavKey::Click { col, row: g.top + 2 }),
            Step::Close(Some("b".into()))
        );
        assert_eq!(p.key(NavKey::Click { col, row: g.top }), Step::Stay, "the border");
        assert_eq!(p.key(NavKey::Click { col: 2, row: g.top + 1 }), Step::Stay, "outside");
    }

    #[test]
    fn a_refresh_keeps_the_cursor_on_its_session() {
        let mut p = Panel::new(
            vec![row("a", 1, None), row("b", 2, None), row("c", 3, None)],
            Key::Ctrl('a'),
            vec![],
        );
        p.key(NavKey::Down);
        // c starts waiting and sorts to the top
        p.set_rows(vec![row("c", 3, Some("waiting")), row("a", 1, None), row("b", 2, None)]);
        assert_eq!(p.selected().as_deref(), Some("b"));
        // b is killed elsewhere: the cursor stays in range
        p.set_rows(vec![row("c", 3, None), row("a", 1, None)]);
        assert!(p.cursor < 2);
    }

    #[test]
    fn a_long_list_scrolls_to_the_cursor() {
        let rs: Vec<Row> = (1..=30).map(|i| row(&format!("s{i}"), i, None)).collect();
        let mut p = Panel::new(rs, Key::Ctrl('a'), vec![]);
        for _ in 0..20 {
            p.key(NavKey::Down);
        }
        let lines = visible(&p.draw(80, 14));
        assert!(lines.iter().any(|l| l.contains("▌") && l.contains("s21")), "{lines:?}");
        assert!(!lines.iter().any(|l| l.contains(" s1 ")), "the top scrolled away");
    }

    #[test]
    fn opens_on_the_current_session_when_nobody_waits() {
        let mut rs = vec![row("a", 1, None), row("b", 2, Some("idle"))];
        rs[1].current = true;
        assert_eq!(Panel::new(rs, Key::Ctrl('a'), vec![]).cursor, 1);
    }
}
