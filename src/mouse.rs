//! Mouse reports on their way to the program, with one repair: a drag for a
//! button that was never pressed goes through as plain pointer motion.
//!
//! Ghostty records a button as down on its press and up on its release. A
//! right-click while no program has the mouse opens Ghostty's context menu,
//! and the menu takes the release, so Ghostty holds the right button down
//! from then on. Every later pointer move it reports as a right-button drag,
//! and in a terminal window in Neovim a drag ends terminal mode: moving the
//! pointer over an agent's pane dropped it to normal mode. Under tmux with
//! the mouse on, a program always had the mouse, the menu never opened, and
//! the release always came.
//!
//! sluice sees every press and release the terminal reports while a window is
//! on screen, so it can tell such a drag from a real one. Only SGR reports
//! (mode 1006, which Neovim, fzf and Claude use) are read; anything else
//! passes as it is.

use std::borrow::Cow;

#[derive(Default)]
pub struct Buttons {
    /// left, middle, right, as the reports have shown them
    held: [bool; 3],
}

const SGR: &[u8] = b"\x1b[<";

impl Buttons {
    /// `buf` with phantom drags turned into motion. `buf` holds whole
    /// sequences: the scanner never splits one across two events. Borrows
    /// `buf` when there is no mouse report in it, which is nearly always.
    pub fn filter<'a>(&mut self, buf: &'a [u8]) -> Cow<'a, [u8]> {
        if !buf.windows(SGR.len()).any(|w| w == SGR) {
            return Cow::Borrowed(buf);
        }
        // Built only once a report needs rewriting; `copied` is how far into
        // `buf` it has got.
        let mut out: Option<Vec<u8>> = None;
        let mut copied = 0;
        let mut i = 0;
        while let Some(off) = buf[i..].windows(SGR.len()).position(|w| w == SGR) {
            let at = i + off;
            let Some((code, len)) = parse(&buf[at..]) else {
                i = at + SGR.len();
                continue;
            };
            let end = at + len;
            if let Some(new) = self.see(code, buf[end - 1] == b'm') {
                let o = out.get_or_insert_with(Vec::new);
                o.extend_from_slice(&buf[copied..at]);
                o.extend_from_slice(SGR);
                o.extend_from_slice(new.to_string().as_bytes());
                // the coordinates and the final byte, as they came
                o.extend_from_slice(&buf[at + SGR.len() + digits(code)..end]);
                copied = end;
            }
            i = end;
        }
        match out {
            Some(mut o) => {
                o.extend_from_slice(&buf[copied..]);
                Cow::Owned(o)
            }
            None => Cow::Borrowed(buf),
        }
    }

    /// Track a report; return its code rewritten when it is a phantom drag.
    fn see(&mut self, code: u32, release: bool) -> Option<u32> {
        // wheel (64..) and the extra buttons (128..) hold nothing
        if code & (64 | 128) != 0 {
            return None;
        }
        let button = (code & 3) as usize;
        let motion = code & 32 != 0;
        if motion {
            // 3 is motion with no button
            if button < 3 && !self.held[button] {
                return Some(code | 3);
            }
            return None;
        }
        if button < 3 {
            self.held[button] = !release;
        }
        None
    }
}

/// `ESC [ < code ; x ; y (M|m)` at the start of `buf`: the code and the
/// sequence's length. None for anything else.
fn parse(buf: &[u8]) -> Option<(u32, usize)> {
    let body = buf.strip_prefix(SGR)?;
    let end = body.iter().position(|&b| b == b'M' || b == b'm')?;
    let fields: Vec<&[u8]> = body[..end].split(|&b| b == b';').collect();
    if fields.len() != 3
        || fields
            .iter()
            .any(|f| f.is_empty() || f.len() > 5 || !f.iter().all(u8::is_ascii_digit))
    {
        return None;
    }
    let code: u32 = std::str::from_utf8(fields[0]).ok()?.parse().ok()?;
    // the rewrite writes the code back in its shortest form
    if fields[0].len() != digits(code) {
        return None;
    }
    Some((code, SGR.len() + end + 1))
}

fn digits(n: u32) -> usize {
    n.to_string().len()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(b: &mut Buttons, input: &[u8]) -> Vec<u8> {
        b.filter(input).into_owned()
    }

    #[test]
    fn a_drag_with_no_press_is_plain_motion() {
        let mut b = Buttons::default();
        // right drag (32 + 2), as Ghostty sends it with the button stuck
        assert_eq!(run(&mut b, b"\x1b[<34;10;5M"), b"\x1b[<35;10;5M");
        // left drag with ctrl (32 + 16 + 0)
        assert_eq!(run(&mut b, b"\x1b[<48;1;1M"), b"\x1b[<51;1;1M");
    }

    #[test]
    fn a_real_drag_goes_through_and_ends_with_its_release() {
        let mut b = Buttons::default();
        let drag = b"\x1b[<2;4;4M\x1b[<34;5;4M\x1b[<34;6;4M\x1b[<2;6;4m";
        assert_eq!(run(&mut b, drag), drag.to_vec());
        // released: the next drag of that button is a phantom again
        assert_eq!(run(&mut b, b"\x1b[<34;7;4M"), b"\x1b[<35;7;4M");
    }

    #[test]
    fn buttons_are_tracked_apart() {
        let mut b = Buttons::default();
        run(&mut b, b"\x1b[<0;1;1M");
        assert_eq!(run(&mut b, b"\x1b[<32;2;1M"), b"\x1b[<32;2;1M", "left held");
        assert_eq!(run(&mut b, b"\x1b[<34;2;1M"), b"\x1b[<35;2;1M", "right not");
    }

    #[test]
    fn everything_else_is_left_alone() {
        let mut b = Buttons::default();
        for input in [
            &b"plain text"[..],
            b"\x1b[<35;3;3M",       // motion with no button
            b"\x1b[<64;3;3M",       // wheel
            b"\x1b[<128;3;3M",      // button 8
            b"\x1b[M !!",           // X10, not read
            b"\x1b[<34;3M",         // two fields
            b"\x1b[<034;3;3M",      // a code not in its shortest form
            b"\x1b[<34;3;3",        // no final byte
            b"\x1b[<x;3;3M",
            b"\x1b[O\x1b[I",
        ] {
            assert_eq!(run(&mut b, input), input.to_vec(), "{input:?}");
            assert!(matches!(b.filter(input), Cow::Borrowed(_)), "{input:?}");
        }
    }

    #[test]
    fn surrounding_bytes_survive_a_rewrite() {
        let mut b = Buttons::default();
        let input = b"a\x1b[<34;1;1Mb\x1b[O\x1b[<34;2;1Mc\x1b[<35;3;1M";
        assert_eq!(
            run(&mut b, input),
            b"a\x1b[<35;1;1Mb\x1b[O\x1b[<35;2;1Mc\x1b[<35;3;1M".to_vec()
        );
    }
}
