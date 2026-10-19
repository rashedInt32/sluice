//! Keyboard-mode bookkeeping. zmx replays most terminal modes when a client
//! reattaches (mouse, bracketed paste, alternate screen, modifyOtherKeys) but
//! not the kitty keyboard protocol stack. So a Ghostty that was showing nvim or
//! Claude keeps sending CSI u encoded keys to whatever comes next: fzf in our
//! picker, or a plain zsh in the session we jump to.
//!
//! sluice watches the output it forwards for the handful of sequences that
//! change that state, keeps the result per session, resets the terminal to a
//! neutral keyboard between sessions, and re-applies a session's flags when it
//! comes back. Nothing here touches the bytes; it only reads them.

/// Mouse, focus and paste reporting off, modifyOtherKeys off, kitty flags
/// zeroed on both screens, main screen, cursor keys normal, cursor visible.
/// zmx re-enables what the next session actually uses.
pub const NEUTRAL: &[u8] = b"\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1004l\x1b[?2004l\x1b[>4;0m\x1b[?1l\x1b[?1049l\x1b[=0;1u\x1b[?1049h\x1b[=0;1u\x1b[?1049l\x1b[?25h";

const FLAG_MASK: u32 = 0b11111;
const MAX_STACK: usize = 64;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KittyState {
    main: Vec<u32>,
    alt: Vec<u32>,
    on_alt: bool,
}

impl Default for KittyState {
    fn default() -> Self {
        KittyState {
            main: vec![0],
            alt: vec![0],
            on_alt: false,
        }
    }
}

impl KittyState {
    pub fn flags(&self) -> (u32, u32) {
        (
            *self.main.last().unwrap_or(&0),
            *self.alt.last().unwrap_or(&0),
        )
    }

    /// Bytes that put a terminal (already in the NEUTRAL state) back to this
    /// session's kitty flags on both screens, ending on the main screen.
    /// Empty when both screens are at zero.
    pub fn restore(&self) -> Vec<u8> {
        let (m, a) = self.flags();
        let mut out = Vec::new();
        if m != 0 {
            out.extend_from_slice(format!("\x1b[={m};1u").as_bytes());
        }
        if a != 0 {
            out.extend_from_slice(format!("\x1b[?1049h\x1b[={a};1u\x1b[?1049l").as_bytes());
        }
        out
    }

    /// Scan forwarded output for kitty push/pop/set, screen switches and resets.
    pub fn feed(&mut self, buf: &[u8]) {
        let mut i = 0;
        while let Some(off) = memchr(0x1b, &buf[i..]) {
            let esc = i + off;
            i = esc + 1;
            match buf.get(esc + 1) {
                Some(b'c') => {
                    *self = KittyState::default();
                    i = esc + 2;
                }
                Some(b'[') => {
                    if let Some(len) = csi_len(&buf[esc..]) {
                        self.csi(&buf[esc + 2..esc + len - 1], buf[esc + len - 1]);
                        i = esc + len;
                    }
                }
                _ => {}
            }
        }
    }

    fn stack(&mut self) -> &mut Vec<u32> {
        if self.on_alt {
            &mut self.alt
        } else {
            &mut self.main
        }
    }

    fn csi(&mut self, body: &[u8], fin: u8) {
        match fin {
            b'u' => match body.first() {
                Some(b'>') => {
                    let flags = num(&body[1..]).unwrap_or(0) & FLAG_MASK;
                    let st = self.stack();
                    if st.len() < MAX_STACK {
                        st.push(flags);
                    }
                }
                Some(b'<') => {
                    let n = num(&body[1..]).unwrap_or(1).max(1) as usize;
                    let st = self.stack();
                    for _ in 0..n {
                        if st.len() > 1 {
                            st.pop();
                        } else {
                            st[0] = 0;
                        }
                    }
                }
                Some(b'=') => {
                    let mut parts = body[1..].split(|&b| b == b';');
                    let flags = parts.next().and_then(num).unwrap_or(0) & FLAG_MASK;
                    let mode = parts.next().and_then(num).unwrap_or(1);
                    let st = self.stack();
                    let top = st.last_mut().expect("stack never empty");
                    match mode {
                        2 => *top |= flags,
                        3 => *top &= !flags,
                        _ => *top = flags,
                    }
                }
                _ => {}
            },
            b'h' | b'l' if body.first() == Some(&b'?') => {
                for p in body[1..].split(|&b| b == b';') {
                    if matches!(num(p), Some(1049) | Some(1047) | Some(47)) {
                        self.on_alt = fin == b'h';
                    }
                }
            }
            b'p' if body == b"!" => {
                self.main = vec![0];
                self.alt = vec![0];
            }
            _ => {}
        }
    }
}

fn num(b: &[u8]) -> Option<u32> {
    if b.is_empty() || b.len() > 9 || !b.iter().all(u8::is_ascii_digit) {
        return None;
    }
    std::str::from_utf8(b).ok()?.parse().ok()
}

/// Length of the CSI sequence at buf[0] == ESC, buf[1] == '[', if complete.
fn csi_len(buf: &[u8]) -> Option<usize> {
    for (i, &b) in buf.iter().enumerate().skip(2) {
        if (0x40..=0x7e).contains(&b) {
            return Some(i + 1);
        }
        if i > 64 {
            return None;
        }
    }
    None
}

fn memchr(needle: u8, hay: &[u8]) -> Option<usize> {
    hay.iter().position(|&b| b == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_output_changes_nothing() {
        let mut k = KittyState::default();
        k.feed(b"hello \x1b[31mred\x1b[0m \x1b[2J\x1b[H");
        assert_eq!(k.flags(), (0, 0));
        assert!(k.restore().is_empty());
    }

    #[test]
    fn push_pop_set_on_main() {
        let mut k = KittyState::default();
        k.feed(b"\x1b[>1u");
        assert_eq!(k.flags(), (1, 0));
        k.feed(b"\x1b[>5u");
        assert_eq!(k.flags(), (5, 0));
        k.feed(b"\x1b[<u");
        assert_eq!(k.flags(), (1, 0));
        k.feed(b"\x1b[=8;2u"); // add bit
        assert_eq!(k.flags(), (9, 0));
        k.feed(b"\x1b[=1;3u"); // clear bit
        assert_eq!(k.flags(), (8, 0));
        k.feed(b"\x1b[=3;1u"); // set
        assert_eq!(k.flags(), (3, 0));
        k.feed(b"\x1b[<9u"); // over-pop lands at zero, never panics
        assert_eq!(k.flags(), (0, 0));
    }

    #[test]
    fn screens_have_separate_stacks() {
        let mut k = KittyState::default();
        k.feed(b"\x1b[>1u\x1b[?1049h\x1b[>31u");
        assert_eq!(k.flags(), (1, 31));
        k.feed(b"\x1b[?1049l\x1b[<u");
        assert_eq!(k.flags(), (0, 31));
        assert_eq!(k.restore(), b"\x1b[?1049h\x1b[=31;1u\x1b[?1049l".to_vec());
    }

    #[test]
    fn restore_covers_both_screens_and_ends_on_main() {
        let mut k = KittyState::default();
        k.feed(b"\x1b[>7u\x1b[?1049h\x1b[>1u");
        assert_eq!(
            k.restore(),
            b"\x1b[=7;1u\x1b[?1049h\x1b[=1;1u\x1b[?1049l".to_vec()
        );
    }

    #[test]
    fn resets_clear_everything() {
        let mut k = KittyState::default();
        k.feed(b"\x1b[>7u\x1b[?1049h\x1b[>1u");
        k.feed(b"\x1b[!p");
        assert_eq!(k.flags(), (0, 0));
        k.feed(b"\x1b[>7u\x1bc");
        assert_eq!(k.flags(), (0, 0));
        assert!(!k.on_alt);
    }

    #[test]
    fn sequence_split_across_chunks_is_simply_missed_not_misread() {
        let mut k = KittyState::default();
        k.feed(b"\x1b[>");
        k.feed(b"1u");
        assert_eq!(k.flags(), (0, 0));
        k.feed(b"\x1b[>1u");
        assert_eq!(k.flags(), (1, 0));
    }

    #[test]
    fn queries_and_other_csi_are_ignored() {
        let mut k = KittyState::default();
        k.feed(b"\x1b[?u\x1b[?1000h\x1b[>4;2m\x1b[1;5u");
        assert_eq!(k.flags(), (0, 0));
    }
}
