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
///
/// It starts by resetting the colours. The next window's repaint begins with
/// a clear, and a clear fills with the current background: a background
/// left set by the window we left (nvim's, say) would fill the new window's
/// empty space. Leaving the alternate screen clears it too.
///
/// The screens are switched with mode 1047, not 1049: 1047 carries the
/// cursor across and saves nothing. 1049's reset restores the main screen's
/// saved cursor even when it is already on the main screen, and with no
/// save that is the top left corner, where the cursor then sat until the
/// picker drew. Leaving the alternate screen this way clears it, which the
/// next window's repaint or picker does anyway.
pub const NEUTRAL: &[u8] = b"\x1b[0m\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1004l\x1b[?2004l\x1b[>4;0m\x1b[?1l\x1b[?1047l\x1b[=0;1u\x1b[?1047h\x1b[=0;1u\x1b[?1047l\x1b[?25h";

const FLAG_MASK: u32 = 0b11111;
const MAX_STACK: usize = 64;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KittyState {
    main: Vec<u32>,
    alt: Vec<u32>,
    on_alt: bool,
    /// The start of a sequence the last read cut off.
    held: Vec<u8>,
}

impl Default for KittyState {
    fn default() -> Self {
        KittyState {
            main: vec![0],
            alt: vec![0],
            on_alt: false,
            held: Vec::new(),
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
    /// Empty when both screens are at zero. Mode 1047, as in NEUTRAL, so the
    /// cursor and its saved position are left alone.
    pub fn restore(&self) -> Vec<u8> {
        let (m, a) = self.flags();
        let mut out = Vec::new();
        if m != 0 {
            out.extend_from_slice(format!("\x1b[={m};1u").as_bytes());
        }
        if a != 0 {
            out.extend_from_slice(format!("\x1b[?1047h\x1b[={a};1u\x1b[?1047l").as_bytes());
        }
        out
    }

    /// Scan forwarded output for kitty push/pop/set, screen switches and resets.
    pub fn feed(&mut self, buf: &[u8]) {
        if self.held.is_empty() {
            self.scan(buf);
        } else {
            let mut d = std::mem::take(&mut self.held);
            d.extend_from_slice(buf);
            self.scan(&d);
        }
    }

    /// The output stream ended (the window was detached): a held fragment
    /// would be glued onto another stream's output.
    pub fn cut(&mut self) {
        self.held.clear();
    }

    fn scan(&mut self, buf: &[u8]) {
        let mut i = 0;
        while let Some(off) = memchr(0x1b, &buf[i..]) {
            let esc = i + off;
            i = esc + 1;
            match buf.get(esc + 1) {
                None => self.held = vec![0x1b],
                Some(b'c') => {
                    *self = KittyState::default();
                    i = esc + 2;
                }
                Some(b'[') => match csi_len(&buf[esc..]) {
                    Csi::Done(len) => {
                        self.csi(&buf[esc + 2..esc + len - 1], buf[esc + len - 1]);
                        i = esc + len;
                    }
                    Csi::Incomplete => {
                        self.held = buf[esc..].to_vec();
                        return;
                    }
                    // Scanning goes on from the next ESC.
                    Csi::Broken => {}
                },
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
                    // A full stack evicts its oldest entry, as the kitty spec
                    // asks of the terminal. Entry 0 is the base, not a push.
                    if st.len() >= MAX_STACK {
                        st.remove(1);
                    }
                    st.push(flags);
                }
                Some(b'<') => {
                    let n = num(&body[1..]).unwrap_or(1).max(1) as usize;
                    let st = self.stack();
                    // Popping past the base only zeroes it again, so any
                    // count over the depth is the same as the depth.
                    for _ in 0..n.min(st.len()) {
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

enum Csi {
    /// The sequence at buf[0] is this long.
    Done(usize),
    /// The buffer ends inside it.
    Incomplete,
    /// Cut short by an ESC, or too long to be one: a terminal drops it.
    Broken,
}

/// The CSI sequence at buf[0] == ESC, buf[1] == '['.
fn csi_len(buf: &[u8]) -> Csi {
    for (i, &b) in buf.iter().enumerate().skip(2) {
        // ESC starts another sequence; CAN and SUB cancel this one.
        if matches!(b, 0x1b | 0x18 | 0x1a) || i > 64 {
            return Csi::Broken;
        }
        if (0x40..=0x7e).contains(&b) {
            return Csi::Done(i + 1);
        }
    }
    Csi::Incomplete
}

/// First `needle` in `hay`. Runs over all output, so it tests eight bytes at
/// a time: a byte of `x` is zero where `hay` matched, and `x - LO & !x & HI`
/// sets the top bit of the lowest zero byte. Higher bits can be false hits
/// from the borrow, so only the lowest one is used.
fn memchr(needle: u8, hay: &[u8]) -> Option<usize> {
    const LO: u64 = 0x0101_0101_0101_0101;
    const HI: u64 = 0x8080_8080_8080_8080;
    let rep = LO * needle as u64;
    let mut base = 0;
    while let Some(w) = hay.get(base..base + 8) {
        let x = u64::from_le_bytes(w.try_into().expect("8 bytes")) ^ rep;
        let hit = x.wrapping_sub(LO) & !x & HI;
        if hit != 0 {
            return Some(base + (hit.trailing_zeros() / 8) as usize);
        }
        base += 8;
    }
    hay[base..]
        .iter()
        .position(|&b| b == needle)
        .map(|p| base + p)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memchr_finds_the_first_match_at_any_offset() {
        let mut hay = vec![b'x'; 40];
        assert_eq!(memchr(0x1b, &hay), None);
        for at in 0..hay.len() {
            hay[at] = 0x1b;
            // a later match must not win, nor a byte the borrow touched
            hay[(at + 1) % 40] = 0x1a;
            for start in 0..=at {
                assert_eq!(memchr(0x1b, &hay[start..]), Some(at - start), "at {at} from {start}");
            }
            hay[at] = b'x';
            hay[(at + 1) % 40] = b'x';
        }
        assert_eq!(memchr(0x1b, b"\x1b\x1b"), Some(0));
        assert_eq!(memchr(0x1b, b""), None);
    }

    #[test]
    fn a_huge_pop_count_costs_no_more_than_the_depth() {
        let mut k = KittyState::default();
        k.feed(b"\x1b[>1u\x1b[>3u");
        let t = std::time::Instant::now();
        k.feed(b"\x1b[<999999999u");
        assert!(t.elapsed() < std::time::Duration::from_millis(5), "{:?}", t.elapsed());
        assert_eq!(k.flags(), (0, 0));
    }

    #[test]
    fn a_sequence_split_across_reads_still_counts() {
        let out = b"text\x1b[?1049h\x1b[>5u more\x1b[?1049l\x1b[>1u\x1b[=3;2u end";
        let mut whole = KittyState::default();
        whole.feed(out);
        assert_eq!(whole.flags(), (3, 5));
        for k in 1..out.len() {
            let mut s = KittyState::default();
            s.feed(&out[..k]);
            s.feed(&out[k..]);
            assert_eq!(s.flags(), whole.flags(), "split at {k}");
        }
    }

    #[test]
    fn an_esc_ends_a_broken_csi_so_the_next_one_counts() {
        let mut k = KittyState::default();
        k.feed(b"\x1b[\x1b[>1u");
        assert_eq!(k.flags(), (1, 0));
    }

    #[test]
    fn can_or_sub_cancels_a_csi() {
        let mut k = KittyState::default();
        k.feed(b"\x1b[>5u\x1b[>1\x18u\x1b[<u");
        assert_eq!(k.flags(), (0, 0), "the cancelled push was not counted");
    }

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
    fn a_full_stack_drops_its_oldest_push_not_the_newest() {
        let mut k = KittyState::default();
        for _ in 0..MAX_STACK {
            k.feed(b"\x1b[>1u");
        }
        k.feed(b"\x1b[>3u");
        assert_eq!(k.flags(), (3, 0), "the newest push is on top");
        assert_eq!(k.main.len(), MAX_STACK);
        k.feed(b"\x1b[<u");
        assert_eq!(k.flags(), (1, 0));
        k.feed(b"\x1b[<99u");
        assert_eq!(k.flags(), (0, 0), "still lands on the base");
    }

    #[test]
    fn screens_have_separate_stacks() {
        let mut k = KittyState::default();
        k.feed(b"\x1b[>1u\x1b[?1049h\x1b[>31u");
        assert_eq!(k.flags(), (1, 31));
        k.feed(b"\x1b[?1049l\x1b[<u");
        assert_eq!(k.flags(), (0, 31));
        assert_eq!(k.restore(), b"\x1b[?1047h\x1b[=31;1u\x1b[?1047l".to_vec());
    }

    #[test]
    fn restore_covers_both_screens_and_ends_on_main() {
        let mut k = KittyState::default();
        k.feed(b"\x1b[>7u\x1b[?1049h\x1b[>1u");
        assert_eq!(
            k.restore(),
            b"\x1b[=7;1u\x1b[?1047h\x1b[=1;1u\x1b[?1047l".to_vec()
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
    fn queries_and_other_csi_are_ignored() {
        let mut k = KittyState::default();
        k.feed(b"\x1b[?u\x1b[?1000h\x1b[>4;2m\x1b[1;5u");
        assert_eq!(k.flags(), (0, 0));
    }
}

