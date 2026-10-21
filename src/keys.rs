//! Input scanner: finds the prefix key in the raw byte stream the terminal
//! sends us, in any of the three encodings a modern terminal may use, and
//! decodes the key that follows. Everything that is not a prefix chord is
//! forwarded byte-for-byte, so the program inside the session sees exactly
//! what the terminal sent.
//!
//! Encodings handled:
//!   legacy            0x01 for ctrl+a
//!   kitty CSI u       ESC [ 97 ; 5 u        (ESC [ code[:shifted[:base]] ; mods[:event] [; text] u)
//!   modifyOtherKeys   ESC [ 27 ; 5 ; 97 ~
//!
//! The scanner always consumes whole sequences. A sequence split across two
//! reads is held until the rest arrives, and a sequence that is not a key
//! press (key release, focus change, mouse report) is passed through intact
//! rather than being picked apart. Getting this wrong leaks the tail of an
//! escape sequence into the program as literal text.

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Key {
    Char(char),
    Ctrl(char),
    Esc,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Event {
    /// Forward these bytes to the session untouched.
    Bytes(Vec<u8>),
    /// A complete prefix + key chord. `raw` is how the prefix itself arrived,
    /// so a `literal` binding can replay it faithfully.
    Chord { key: Key, raw_prefix: Vec<u8> },
}

/// One decoded item from the input stream.
#[derive(Debug, PartialEq, Eq)]
enum Scan {
    /// A key press. `exact` is false when extra modifiers rule it out as the
    /// prefix (ctrl+alt+a is not ctrl+a) while still naming a usable chord key.
    Key { key: Key, exact: bool, len: usize },
    /// A key release report. Dropped while the prefix waits, so the release of
    /// the prefix itself never reaches the program.
    Release(usize),
    /// A complete sequence that is not a key press: focus, mouse, a reply.
    Other(usize),
    /// The buffer ends part-way through a sequence.
    Incomplete,
}

/// Parsed CSI key report, shared by the kitty and modifyOtherKeys forms.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct KeyReport {
    key: Key,
    /// Modifier bitmask without the lock bits: shift=1 alt=2 ctrl=4 super=8 hyper=16 meta=32
    mods: u32,
    /// true for press/repeat, false for release
    pressed: bool,
}

const MOD_SHIFT: u32 = 1;
const MOD_CTRL: u32 = 4;
const LOCK_BITS: u32 = 64 | 128;

/// An unterminated sequence longer than this is treated as noise and flushed,
/// so a malformed byte cannot stall input for ever.
const MAX_PENDING: usize = 512;

pub struct Scanner {
    prefix: Key,
    /// Bitset over bytes 0-127: those that are a whole key on their own and
    /// not the prefix. They go through as they are, so runs of them (most of
    /// a paste) skip the decoder.
    plain: [u64; 2],
    waiting: Option<Vec<u8>>,
    /// Tail of the last chunk that ended mid-sequence.
    pending: Vec<u8>,
    /// Inside a bracketed paste: everything up to its end marker is text for
    /// the program, so a prefix byte in it is not the prefix.
    in_paste: bool,
}

const PASTE_START: &[u8] = b"\x1b[200~";
const PASTE_END: &[u8] = b"\x1b[201~";

impl Scanner {
    pub fn new(prefix: Key) -> Scanner {
        let mut plain = [0u64; 2];
        for b in 0..0x80u8 {
            let alone = match scan_one(&[b]) {
                Scan::Key { key, exact, len: 1 } => !(exact && key == prefix),
                _ => false,
            };
            if alone && b != 0x1b {
                plain[b as usize / 64] |= 1 << (b % 64);
            }
        }
        Scanner {
            prefix,
            plain,
            waiting: None,
            pending: Vec::new(),
            in_paste: false,
        }
    }

    #[cfg(test)]
    pub fn feed(&mut self, buf: &[u8]) -> Vec<Event> {
        self.feed_with(buf, || false)
    }

    /// As `feed`, but `handed_off` is asked each time the prefix turns up.
    /// While it says yes the prefix goes through as typed: a program in the
    /// session has asked for it. Asking only then keeps the check off every
    /// other keystroke.
    pub fn feed_with(&mut self, buf: &[u8], mut handed_off: impl FnMut() -> bool) -> Vec<Event> {
        let joined;
        let data: &[u8] = if self.pending.is_empty() {
            buf
        } else {
            let mut d = std::mem::take(&mut self.pending);
            d.extend_from_slice(buf);
            joined = d;
            &joined
        };
        let plain = |b: u8| b < 0x80 && self.plain[b as usize / 64] & (1 << (b % 64)) != 0;

        // Bytes that go through are copied out once per run, not per item:
        // `run` is where the run waiting to be copied starts.
        let mut out = Vec::new();
        let mut run = 0;
        let flush = |out: &mut Vec<Event>, from: usize, to: usize| {
            if from < to {
                out.push(Event::Bytes(data[from..to].to_vec()));
            }
        };
        let mut i = 0;
        while i < data.len() {
            if self.in_paste {
                if let Some(j) = find(&data[i..], PASTE_END) {
                    i += j + PASTE_END.len();
                    self.in_paste = false;
                    continue;
                }
                // Hold back a tail that could be the start of the end marker.
                let keep = (1..PASTE_END.len())
                    .rev()
                    .find(|&k| data[i..].ends_with(&PASTE_END[..k]))
                    .unwrap_or(0);
                let end = data.len() - keep;
                flush(&mut out, run, end);
                self.pending = data[end..].to_vec();
                return out;
            }
            if self.waiting.is_none() && plain(data[i]) {
                i += 1;
                while i < data.len() && plain(data[i]) {
                    i += 1;
                }
                continue;
            }
            match scan_one(&data[i..]) {
                Scan::Incomplete => {
                    let rest = &data[i..];
                    if rest.len() <= MAX_PENDING {
                        flush(&mut out, run, i);
                        self.pending = rest.to_vec();
                        return out;
                    }
                    // Not a sequence after all: let one byte through and resync.
                    i += 1;
                }
                Scan::Release(len) => {
                    // While the prefix waits, the release of the prefix key
                    // itself would be a stray event for the program.
                    if self.waiting.is_some() {
                        flush(&mut out, run, i);
                        run = i + len;
                    }
                    i += len;
                }
                Scan::Other(len) => {
                    if &data[i..i + len] == PASTE_START {
                        // A paste cancels an armed prefix: its text is not
                        // a chord key.
                        self.in_paste = true;
                        self.waiting = None;
                    }
                    i += len;
                }
                Scan::Key { key, exact, len } => {
                    if let Some(raw_prefix) = self.waiting.take() {
                        flush(&mut out, run, i);
                        out.push(Event::Chord { key, raw_prefix });
                        run = i + len;
                    } else if exact && key == self.prefix && !handed_off() {
                        flush(&mut out, run, i);
                        self.waiting = Some(data[i..i + len].to_vec());
                        run = i + len;
                    }
                    i += len;
                }
            }
        }
        flush(&mut out, run, data.len());
        out
    }

    #[cfg(test)]
    pub fn is_waiting(&self) -> bool {
        self.waiting.is_some()
    }

    /// Forget an armed prefix and any half-read sequence. Called on every
    /// detach: neither belongs to the window that comes next.
    pub fn reset(&mut self) {
        self.waiting = None;
        self.pending.clear();
        self.in_paste = false;
    }

}

/// Where `needle` first starts in `hay`.
fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    let mut at = 0;
    while let Some(off) = hay[at..].iter().position(|&b| b == needle[0]) {
        let j = at + off;
        if hay[j..].starts_with(needle) {
            return Some(j);
        }
        at = j + 1;
    }
    None
}

/// Decode the next item at the head of `buf`.
fn scan_one(buf: &[u8]) -> Scan {
    let b = buf[0];
    if b != 0x1b {
        if b < 0x20 {
            return Scan::Key {
                key: Key::Ctrl(legacy_ctrl(b)),
                exact: true,
                len: 1,
            };
        }
        if b == 0x7f {
            return Scan::Key {
                key: Key::Ctrl('?'),
                exact: true,
                len: 1,
            };
        }
        let len = utf8_len(b);
        // A byte that cannot continue the character ends it now: an 8-bit
        // meta key or Latin-1 byte is not held waiting for a tail.
        if buf[1..len.min(buf.len())].iter().any(|&c| c & 0xc0 != 0x80) {
            return Scan::Other(1);
        }
        if buf.len() < len {
            return Scan::Incomplete;
        }
        return match std::str::from_utf8(&buf[..len])
            .ok()
            .and_then(|s| s.chars().next())
        {
            Some(c) => Scan::Key {
                key: Key::Char(c),
                exact: true,
                len,
            },
            None => Scan::Other(1),
        };
    }
    // A lone ESC at the end of a read is the Escape key. Terminals write a key
    // sequence in one go, so holding it instead would only delay Escape.
    if buf.len() == 1 {
        return Scan::Key {
            key: Key::Esc,
            exact: true,
            len: 1,
        };
    }
    // Likewise ESC plus `[`, `O` or `]` and nothing more is alt+[ / alt+O /
    // alt+], typed: held as the start of a sequence, it would swallow the
    // keys typed after it, the prefix included.
    if buf.len() == 2 && matches!(buf[1], b'[' | b'O' | b']') {
        return Scan::Other(2);
    }
    match buf[1] {
        b'[' => match csi_len(buf) {
            Csi::Incomplete => Scan::Incomplete,
            // Not a sequence after all: alt+[, then whatever follows.
            Csi::Invalid => Scan::Other(2),
            // X10 mouse: `ESC [ M` and three raw bytes, any of which may look
            // like a key.
            Csi::Done(3) if buf[2] == b'M' => match buf.len() {
                6.. => Scan::Other(6),
                _ => Scan::Incomplete,
            },
            Csi::Done(len) => match parse_csi_key(&buf[2..len - 1], buf[len - 1]) {
                Some(rep) if rep.pressed => {
                    let (key, exact) = report_key(rep);
                    Scan::Key { key, exact, len }
                }
                Some(_) => Scan::Release(len),
                None => Scan::Other(len),
            },
        },
        // SS3: arrow and function keys in application mode, maybe with a
        // modifier first (`ESC O 2 P`, `ESC O 1;5P`). Anything else after
        // ESC O means this was alt+O.
        b'O' => {
            for (i, &b) in buf.iter().enumerate().skip(2) {
                match b {
                    0x40..=0x7e => return Scan::Other(i + 1),
                    b'0'..=b'9' | b';' => {}
                    _ => return Scan::Other(2),
                }
            }
            Scan::Incomplete
        }
        // OSC: a reply such as a clipboard read, terminated by BEL or ST.
        b']' => match osc_len(buf) {
            Some(len) => Scan::Other(len),
            None => Scan::Incomplete,
        },
        // DCS, SOS, PM, APC: replies such as XTVERSION or a kitty graphics
        // status, ended by ST. Taken whole only when the end is in this read:
        // ESC P alone is alt+P as typed.
        b'P' | b'X' | b'^' | b'_' => match find(&buf[2..], b"\x1b\\") {
            Some(j) => Scan::Other(j + 4),
            None => Scan::Other(2),
        },
        // ESC + byte: alt-modified key and friends.
        _ => Scan::Other(2),
    }
}

/// The chord key a report names, and whether its modifiers allow it to be the
/// prefix. ctrl+alt+a can still be a chord key, but it is not ctrl+a.
fn report_key(rep: KeyReport) -> (Key, bool) {
    match rep.key {
        Key::Char(c) if rep.mods & MOD_CTRL != 0 => {
            (Key::Ctrl(ctrl_name(c.to_ascii_lowercase())), rep.mods == MOD_CTRL)
        }
        Key::Char(c) => (Key::Char(c), rep.mods & !MOD_SHIFT == 0),
        k => (k, true),
    }
}

/// The key a legacy control byte stands for. Letters are ctrl plus the
/// letter (0x01 is C-a); the five after Esc are ctrl plus punctuation, and
/// 0x00 is what C-Space and C-@ send.
fn legacy_ctrl(b: u8) -> char {
    match b {
        0x01..=0x1a => (b + 0x60) as char,
        _ => ctrl_name((b + 0x40) as char),
    }
}

/// One name for ctrl+space, however it arrives: legacy 0x00 cannot tell
/// C-Space from C-@, so both are C-@.
fn ctrl_name(c: char) -> char {
    match c {
        ' ' => '@',
        c => c,
    }
}

fn utf8_len(b: u8) -> usize {
    match b {
        0x00..=0x7f => 1,
        0xc0..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf7 => 4,
        _ => 1,
    }
}

/// Length of the CSI sequence at buf[0] == ESC, buf[1] == '[', if complete.
enum Csi {
    Done(usize),
    Incomplete,
    /// A byte no CSI may hold: what came before was not a sequence.
    Invalid,
}

fn csi_len(buf: &[u8]) -> Csi {
    for (i, &b) in buf.iter().enumerate().skip(2) {
        match b {
            0x40..=0x7e => return Csi::Done(i + 1),
            0x20..=0x3f => {}
            _ => return Csi::Invalid,
        }
    }
    Csi::Incomplete
}

/// Length of the OSC string at buf[0] == ESC, buf[1] == ']', if complete.
fn osc_len(buf: &[u8]) -> Option<usize> {
    let mut i = 2;
    while i < buf.len() {
        if buf[i] == 0x07 {
            return Some(i + 1);
        }
        if buf[i] == 0x1b && buf.get(i + 1) == Some(&b'\\') {
            return Some(i + 2);
        }
        i += 1;
    }
    None
}

/// Parse a kitty `CSI ... u` or xterm `CSI 27;mods;code ~` key report.
fn parse_csi_key(body: &[u8], final_byte: u8) -> Option<KeyReport> {
    let body = std::str::from_utf8(body).ok()?;
    let params: Vec<&str> = body.split(';').collect();
    match final_byte {
        b'u' => {
            let codes: Vec<&str> = params.first()?.split(':').collect();
            let code: u32 = codes.first()?.parse().ok()?;
            let shifted: Option<u32> = codes.get(1).and_then(|s| s.parse().ok());
            let (mods, event) = parse_mods(params.get(1).copied());
            let text: Option<u32> = params
                .get(2)
                .and_then(|t| t.split(':').next())
                .and_then(|t| t.parse().ok());
            let effective = text.or(shifted).unwrap_or(code);
            let key = csi_u_key(effective)?;
            Some(KeyReport {
                key,
                mods,
                pressed: event != 3,
            })
        }
        b'~' if params.first() == Some(&"27") => {
            let (mods, event) = parse_mods(params.get(1).copied());
            let code: u32 = params.get(2)?.parse().ok()?;
            let key = csi_u_key(code)?;
            Some(KeyReport {
                key,
                mods,
                pressed: event != 3,
            })
        }
        _ => None,
    }
}

fn parse_mods(field: Option<&str>) -> (u32, u32) {
    let Some(f) = field else { return (0, 1) };
    let mut parts = f.split(':');
    let m: u32 = parts.next().and_then(|m| m.parse().ok()).unwrap_or(1);
    let event: u32 = parts.next().and_then(|e| e.parse().ok()).unwrap_or(1);
    let mods = m.saturating_sub(1) & !LOCK_BITS;
    (mods, event)
}

/// The key a kitty key code names. Enter, Tab, Backspace and the keypad are
/// the keys their legacy bytes decode to. Kitty's other functional keys
/// (modifiers, arrows, F-keys) live in a private-use block and are no chord
/// key: with "report all keys", the shift pressed to type `?` arrives
/// first and must not stand in for the `?`.
fn csi_u_key(code: u32) -> Option<Key> {
    match code {
        27 => Some(Key::Esc),
        9 | 13 => Some(Key::Ctrl(legacy_ctrl(code as u8))),
        127 => Some(Key::Ctrl('?')),
        // The keypad: the keys a legacy keypad sends.
        57399..=57408 => char::from_digit(code - 57399, 10).map(Key::Char),
        57409..=57415 => Some(match code {
            57409 => Key::Char('.'),
            57410 => Key::Char('/'),
            57411 => Key::Char('*'),
            57412 => Key::Char('-'),
            57413 => Key::Char('+'),
            57414 => Key::Ctrl('m'),
            _ => Key::Char('='),
        }),
        57344..=63743 => None,
        0x20..=0x10ffff => char::from_u32(code).map(Key::Char),
        _ => None,
    }
}

/// Parse a key name from the config file: `a`, `?`, `C-a`, `Esc`, `Space`.
pub fn parse_key_name(s: &str) -> Option<Key> {
    match s {
        "Esc" | "Escape" => Some(Key::Esc),
        "Space" => Some(Key::Char(' ')),
        "C-Space" | "c-Space" | "C-space" => Some(Key::Ctrl('@')),
        _ if s.len() >= 3 && (s.starts_with("C-") || s.starts_with("c-")) => {
            let c = s[2..].chars().next()?;
            if s[2..].chars().count() == 1 {
                Some(Key::Ctrl(ctrl_name(c.to_ascii_lowercase())))
            } else {
                None
            }
        }
        _ if s.chars().count() == 1 => s.chars().next().map(Key::Char),
        _ => None,
    }
}

pub fn key_name(k: Key) -> String {
    match k {
        Key::Char(' ') => "Space".into(),
        Key::Char(c) => c.to_string(),
        Key::Ctrl('@') => "C-Space".into(),
        Key::Ctrl(c) => format!("C-{c}"),
        Key::Esc => "Esc".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chord(ev: &Event) -> Option<Key> {
        match ev {
            Event::Chord { key, .. } => Some(*key),
            _ => None,
        }
    }

    #[test]
    fn kitty_modifier_presses_are_not_chord_keys() {
        let mut s = Scanner::new(Key::Ctrl('a'));
        s.feed(b"\x01");
        // left shift down, then `?` (code 47, shifted 63)
        let ev = s.feed(b"\x1b[57441;2u\x1b[47:63;2u");
        assert_eq!(ev[0], Event::Bytes(b"\x1b[57441;2u".to_vec()), "the shift goes through");
        assert_eq!(chord(&ev[1]), Some(Key::Char('?')));
        // kitty Enter, Tab, Backspace are the keys legacy bytes decode to
        for (seq, byte) in [(&b"\x1b[13u"[..], b'\r'), (b"\x1b[9u", b'\t'), (b"\x1b[127u", 0x7f)] {
            let mut k = Scanner::new(Key::Ctrl('a'));
            k.feed(b"\x01");
            let kitty = chord(&k.feed(seq)[0]);
            let mut l = Scanner::new(Key::Ctrl('a'));
            l.feed(b"\x01");
            assert_eq!(kitty, chord(&l.feed(&[byte])[0]), "{seq:?}");
        }
    }

    #[test]
    fn replies_and_mouse_reports_are_not_picked_apart_while_armed() {
        let armed = || {
            let mut s = Scanner::new(Key::Ctrl('a'));
            s.feed(b"\x01");
            s
        };
        // X10 mouse: ESC [ M and three raw bytes
        let mut s = armed();
        assert_eq!(s.feed(b"\x1b[M !!"), vec![Event::Bytes(b"\x1b[M !!".to_vec())]);
        assert!(s.is_waiting());
        // a DCS reply, XTVERSION
        let mut s = armed();
        let dcs = b"\x1bP>|kitty(0.35)\x1b\\";
        assert_eq!(s.feed(dcs), vec![Event::Bytes(dcs.to_vec())]);
        assert!(s.is_waiting());
        // ESC P alone is alt+P, typed: it is the chord key's business, not
        // the start of a reply that swallows input
        let mut s = Scanner::new(Key::Ctrl('a'));
        assert_eq!(s.feed(b"\x1bP"), vec![Event::Bytes(b"\x1bP".to_vec())]);
    }

    #[test]
    fn alt_o_alt_bracket_and_8_bit_bytes_are_not_held() {
        // alt+O, alt+[ and alt+] typed: sent at once, and the prefix typed
        // next is the prefix, not the end of a sequence
        for alt in [&b"\x1bO"[..], b"\x1b[", b"\x1b]"] {
            let mut s = Scanner::new(Key::Ctrl('a'));
            assert_eq!(s.feed(alt), vec![Event::Bytes(alt.to_vec())]);
            let ev = s.feed(b"\x01d");
            assert!(matches!(ev[..], [Event::Chord { key: Key::Char('d'), .. }]), "{alt:?}");
        }
        // with the same keys arriving one read each, alt+[ then `200~`
        // typed is text, not a paste that never ends
        let mut s = Scanner::new(Key::Ctrl('a'));
        for k in [&b"\x1b["[..], b"2", b"0", b"0", b"~", b"x"] {
            s.feed(k);
        }
        assert!(matches!(s.feed(b"\x01d")[..], [Event::Chord { key: Key::Char('d'), .. }]));
        // SS3 with a modifier passes whole while armed
        let mut s = Scanner::new(Key::Ctrl('a'));
        s.feed(b"\x01");
        assert_eq!(s.feed(b"\x1bO2P"), vec![Event::Bytes(b"\x1bO2P".to_vec())]);
        assert!(s.is_waiting());
        // kitty keypad 1 after the prefix is the chord key `1`
        let mut s = Scanner::new(Key::Ctrl('a'));
        s.feed(b"\x1b[97;5u");
        assert_eq!(chord(&s.feed(b"\x1b[57400u")[0]), Some(Key::Char('1')));
        // 8-bit meta alt+a is one byte, and what follows is not held behind it
        let mut s = Scanner::new(Key::Ctrl('a'));
        let mut got = Vec::new();
        for c in [&b"\xe1"[..], b"x"] {
            for e in s.feed(c) {
                if let Event::Bytes(b) = e {
                    got.extend(b);
                }
            }
        }
        assert_eq!(got, b"\xe1x".to_vec());
    }

    #[test]
    fn a_prefix_inside_a_bracketed_paste_is_text() {
        let bytes = |ev: Vec<Event>| -> Vec<u8> {
            ev.into_iter()
                .flat_map(|e| match e {
                    Event::Bytes(b) => b,
                    e => panic!("not text: {e:?}"),
                })
                .collect()
        };
        let mut s = Scanner::new(Key::Ctrl('a'));
        let paste = b"\x1b[200~a\x01bc\x1b[201~";
        assert_eq!(bytes(s.feed(paste)), paste.to_vec());
        // a printable prefix and a code fence, the end marker split
        let mut s = Scanner::new(Key::Char('`'));
        let mut got = bytes(s.feed(b"\x1b[200~```rust\nfn x(){}\n```\n\x1b[20"));
        got.extend(bytes(s.feed(b"1~")));
        assert_eq!(got, b"\x1b[200~```rust\nfn x(){}\n```\n\x1b[201~".to_vec());
        // after the paste the prefix is the prefix again
        assert!(matches!(s.feed(b"`d")[..], [Event::Chord { key: Key::Char('d'), .. }]));
        // a paste cancels an armed prefix
        let mut s = Scanner::new(Key::Ctrl('a'));
        s.feed(b"\x01");
        assert_eq!(bytes(s.feed(b"\x1b[200~xy\x1b[201~")), b"\x1b[200~xy\x1b[201~".to_vec());
        assert!(!s.is_waiting());
    }

    #[test]
    fn a_printable_prefix_is_found_inside_a_run() {
        let mut s = Scanner::new(Key::Char('`'));
        let ev = s.feed(b"echo hi\n`dls\n");
        assert_eq!(ev[0], Event::Bytes(b"echo hi\n".to_vec()));
        assert!(matches!(ev[1], Event::Chord { key: Key::Char('d'), .. }));
        assert_eq!(ev[2], Event::Bytes(b"ls\n".to_vec()));
    }

    #[test]
    fn a_split_read_changes_nothing() {
        // Forwarded bytes joined, chords in place. A split right after an ESC
        // is left out: a lone ESC ending a read is the Escape key by design.
        fn flat(evs: Vec<Event>, into: &mut Vec<Result<u8, Key>>) {
            for e in evs {
                match e {
                    Event::Bytes(b) => into.extend(b.into_iter().map(Ok)),
                    Event::Chord { key, .. } => into.push(Err(key)),
                }
            }
        }
        let input: &[u8] =
            b"ab\tc\x01d\x1b[97;5u\x1b[97;5:3uq\x1b[Ax\xc3\xa9\x01\x1b[O\x1b[<0;9;3Mf\r\x7f\x1b[200~p\x01q\x1b[201~\x01z";
        let mut whole = Vec::new();
        flat(Scanner::new(Key::Ctrl('a')).feed(input), &mut whole);
        for k in 1..input.len() {
            // ESC, or ESC plus `[` / `O` / `]`, ending a read is a key by
            // design, not a sequence's start.
            if input[k - 1] == 0x1b || (k >= 2 && input[k - 2] == 0x1b) {
                continue;
            }
            let mut s = Scanner::new(Key::Ctrl('a'));
            let mut split = Vec::new();
            flat(s.feed(&input[..k]), &mut split);
            flat(s.feed(&input[k..]), &mut split);
            assert_eq!(whole, split, "split at {k}");
        }
    }

    #[test]
    fn a_handed_off_prefix_goes_through_and_only_the_prefix_asks() {
        let mut s = Scanner::new(Key::Ctrl('a'));
        let mut asked = 0;
        let ev = s.feed_with(b"ls\x1b[A\r", || {
            asked += 1;
            true
        });
        assert_eq!(ev, vec![Event::Bytes(b"ls\x1b[A\r".to_vec())]);
        assert_eq!(asked, 0, "no prefix, no question");
        let ev = s.feed_with(b"\x01d\x1b[97;5u", || {
            asked += 1;
            true
        });
        assert_eq!(ev, vec![Event::Bytes(b"\x01d\x1b[97;5u".to_vec())]);
        assert_eq!(asked, 2, "once per prefix");
        assert!(!s.is_waiting());
        // handed back: the prefix is ours again
        let ev = s.feed_with(b"\x01d", || false);
        assert!(matches!(ev[..], [Event::Chord { key: Key::Char('d'), .. }]));
    }

    #[test]
    fn plain_bytes_pass_through_untouched() {
        let mut s = Scanner::new(Key::Ctrl('a'));
        let input = b"hello \x1b[A world \xe2\x9c\x93";
        assert_eq!(s.feed(input), vec![Event::Bytes(input.to_vec())]);
    }

    #[test]
    fn legacy_prefix_then_key_in_one_chunk() {
        let mut s = Scanner::new(Key::Ctrl('a'));
        let ev = s.feed(b"ab\x01fxyz");
        assert_eq!(ev[0], Event::Bytes(b"ab".to_vec()));
        assert_eq!(chord(&ev[1]), Some(Key::Char('f')));
        assert_eq!(ev[2], Event::Bytes(b"xyz".to_vec()));
    }

    #[test]
    fn prefix_and_key_split_across_reads() {
        let mut s = Scanner::new(Key::Ctrl('a'));
        assert_eq!(s.feed(b"\x01"), vec![]);
        assert!(s.is_waiting());
        let ev = s.feed(b"3");
        assert_eq!(chord(&ev[0]), Some(Key::Char('3')));
        assert!(!s.is_waiting());
    }

    #[test]
    fn kitty_encoded_prefix_with_lock_modifier() {
        let mut s = Scanner::new(Key::Ctrl('a'));
        // ctrl (4) + capslock (64) + 1 = 69
        let ev = s.feed(b"\x1b[97;69u");
        assert!(s.is_waiting());
        assert_eq!(ev, vec![]);
        let ev = s.feed(b"\x1b[102u"); // kitty 'f' with report-all-keys
        assert_eq!(chord(&ev[0]), Some(Key::Char('f')));
    }

    #[test]
    fn kitty_release_events_are_ignored() {
        let mut s = Scanner::new(Key::Ctrl('a'));
        // ctrl+a release must not arm the prefix
        assert_eq!(
            s.feed(b"\x1b[97;5:3u"),
            vec![Event::Bytes(b"\x1b[97;5:3u".to_vec())]
        );
        // press arms; the release of ctrl goes to the program, which had its
        // press, and does not stand in for the chord key; then 'd'
        s.feed(b"\x1b[97;5:1u");
        let ev = s.feed(b"\x1b[57442;1:3ud");
        assert_eq!(ev[0], Event::Bytes(b"\x1b[57442;1:3u".to_vec()));
        assert_eq!(chord(&ev[1]), Some(Key::Char('d')));
    }

    #[test]
    fn modify_other_keys_prefix() {
        let mut s = Scanner::new(Key::Ctrl('a'));
        s.feed(b"\x1b[27;5;97~");
        assert!(s.is_waiting());
        let ev = s.feed(b"0");
        assert_eq!(chord(&ev[0]), Some(Key::Char('0')));
    }

    #[test]
    fn literal_chord_keeps_raw_prefix_bytes() {
        let mut s = Scanner::new(Key::Ctrl('a'));
        let ev = s.feed(b"\x1b[97;5u\x1b[97;5u");
        match &ev[0] {
            Event::Chord { key, raw_prefix } => {
                assert_eq!(*key, Key::Ctrl('a'));
                assert_eq!(raw_prefix, b"\x1b[97;5u");
            }
            other => panic!("unexpected {other:?}"),
        }
        let mut s = Scanner::new(Key::Ctrl('a'));
        let ev = s.feed(b"\x01\x01");
        assert_eq!(chord(&ev[0]), Some(Key::Ctrl('a')));
    }

    #[test]
    fn shifted_question_mark_in_kitty_mode() {
        let mut s = Scanner::new(Key::Ctrl('a'));
        s.feed(b"\x01");
        // '/' with shift, shifted alternate '?' reported
        let ev = s.feed(b"\x1b[47:63;2u");
        assert_eq!(chord(&ev[0]), Some(Key::Char('?')));
        s.feed(b"\x01");
        // text field form
        let ev = s.feed(b"\x1b[47;2;63u");
        assert_eq!(chord(&ev[0]), Some(Key::Char('?')));
    }

    #[test]
    fn escape_cancels() {
        let mut s = Scanner::new(Key::Ctrl('a'));
        s.feed(b"\x01");
        let ev = s.feed(b"\x1b");
        assert_eq!(chord(&ev[0]), Some(Key::Esc));
    }

    #[test]
    fn other_ctrl_prefix() {
        let mut s = Scanner::new(Key::Ctrl('b'));
        let ev = s.feed(b"\x01\x02f");
        assert_eq!(ev[0], Event::Bytes(b"\x01".to_vec()));
        assert_eq!(chord(&ev[1]), Some(Key::Char('f')));
    }

    #[test]
    fn release_after_the_prefix_is_dropped_not_shredded() {
        // Ghostty with event reporting sends press then release. The release
        // used to be taken apart one byte at a time, and its tail reached the
        // program as literal text ("97;5:3u" showing up in nvim).
        let mut s = Scanner::new(Key::Ctrl('a'));
        let ev = s.feed(b"\x1b[97;5u\x1b[97;5:3u");
        assert_eq!(ev, vec![], "nothing leaks to the program");
        assert!(s.is_waiting(), "still armed after the release");
        let ev = s.feed(b"f");
        assert_eq!(chord(&ev[0]), Some(Key::Char('f')));
        assert_eq!(ev.len(), 1);
    }

    #[test]
    fn a_sequence_split_across_reads_is_held_whole() {
        let mut s = Scanner::new(Key::Ctrl('a'));
        assert_eq!(s.feed(b"\x1b[97"), vec![], "no partial bytes forwarded");
        assert_eq!(s.feed(b";5u"), vec![]);
        assert!(s.is_waiting(), "the two halves made one prefix");
        let ev = s.feed(b"\x1b[102");
        assert_eq!(ev, vec![]);
        let ev = s.feed(b"u");
        assert_eq!(chord(&ev[0]), Some(Key::Char('f')));
    }

    #[test]
    fn non_key_sequences_pass_through_while_armed() {
        let mut s = Scanner::new(Key::Ctrl('a'));
        s.feed(b"\x01");
        // focus-out, a mouse report and an SS3 arrow all belong to the program
        let ev = s.feed(b"\x1b[O\x1b[<0;9;3M\x1bOA");
        assert_eq!(ev, vec![Event::Bytes(b"\x1b[O\x1b[<0;9;3M\x1bOA".to_vec())]);
        assert!(s.is_waiting(), "none of that counted as the chord key");
        let ev = s.feed(b"d");
        assert_eq!(chord(&ev[0]), Some(Key::Char('d')));
    }

    #[test]
    fn unterminated_noise_does_not_stall_input() {
        let mut s = Scanner::new(Key::Ctrl('a'));
        s.feed(b"\x1b[");
        let big = vec![b'0'; MAX_PENDING + 8];
        let ev = s.feed(&big);
        let forwarded: usize = ev
            .iter()
            .map(|e| match e {
                Event::Bytes(b) => b.len(),
                _ => 0,
            })
            .sum();
        assert!(forwarded > 0, "input resyncs instead of being swallowed");
    }

    #[test]
    fn ctrl_modified_chord_keys_still_resolve() {
        let mut s = Scanner::new(Key::Ctrl('a'));
        s.feed(b"\x01");
        let ev = s.feed(b"\x1b[100;5u"); // ctrl+d in kitty form
        assert_eq!(chord(&ev[0]), Some(Key::Ctrl('d')));
        // ctrl+alt+a is a chord key, never the prefix
        let mut s = Scanner::new(Key::Ctrl('a'));
        let ev = s.feed(b"\x1b[97;7u");
        assert_eq!(ev, vec![Event::Bytes(b"\x1b[97;7u".to_vec())]);
        assert!(!s.is_waiting());
    }

    #[test]
    fn reset_disarms_the_prefix_and_drops_a_held_tail() {
        let mut s = Scanner::new(Key::Ctrl('a'));
        s.feed(b"\x01");
        s.reset();
        assert!(!s.is_waiting());
        assert_eq!(s.feed(b"d"), vec![Event::Bytes(b"d".to_vec())], "d is typed");

        s.feed(b"\x1b[9");
        s.reset();
        assert_eq!(s.feed(b"x"), vec![Event::Bytes(b"x".to_vec())]);
    }

    #[test]
    fn legacy_and_kitty_ctrl_punctuation_agree() {
        // byte, config name, kitty code
        for (byte, name, code) in [
            (0x00u8, "C-Space", 32u32),
            (0x00, "C-@", 64),
            (0x1c, "C-\\", 92),
            (0x1d, "C-]", 93),
            (0x1e, "C-^", 94),
            (0x1f, "C-_", 95),
        ] {
            let k = parse_key_name(name).unwrap_or_else(|| panic!("{name}"));
            let mut s = Scanner::new(k);
            assert_eq!(s.feed(&[byte]), vec![], "{name}: legacy {byte:#x} arms");
            assert!(s.is_waiting(), "{name}");
            let mut s = Scanner::new(k);
            s.feed(format!("\x1b[{code};5u").as_bytes());
            assert!(s.is_waiting(), "{name}: kitty {code}");
        }
        assert_eq!(key_name(parse_key_name("C-Space").unwrap()), "C-Space");
    }

    #[test]
    fn key_names_round_trip() {
        for k in [
            Key::Ctrl('a'),
            Key::Char('?'),
            Key::Char(' '),
            Key::Esc,
            Key::Char('9'),
        ] {
            assert_eq!(parse_key_name(&key_name(k)), Some(k));
        }
        assert_eq!(parse_key_name("C-"), None);
        assert_eq!(parse_key_name("ab"), None);
    }
}

