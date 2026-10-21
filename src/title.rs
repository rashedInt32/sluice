//! The window title, tmux-style: sluice owns it and shows the session name.
//! Programs inside a session set titles of their own (zsh, nvim, Claude), and
//! each one would overwrite it, so with `title on` sluice drops their OSC 0, 1
//! and 2 from the output it forwards. Every other byte passes untouched.
//! With `title off` (the default) none of this runs.

/// An OSC held across reads longer than this is not a title being set; it is
/// let through as it is, so a stray `ESC ]` cannot stall output for ever.
const MAX_HELD: usize = 4096;

/// The OSC that sets the window title to `name`. Control characters are
/// dropped: one would end the sequence early.
pub fn set(name: &str) -> Vec<u8> {
    let clean: String = name.chars().filter(|c| !c.is_control()).collect();
    format!("\x1b]2;{clean}\x07").into_bytes()
}

#[derive(Default)]
pub struct Filter {
    /// The start of a sequence that the last read cut off.
    held: Vec<u8>,
}

impl Filter {
    /// `buf` without the title sequences in it. Borrows `buf` when there is
    /// nothing to drop, which is nearly always.
    pub fn filter<'a>(&mut self, buf: &'a [u8]) -> std::borrow::Cow<'a, [u8]> {
        if self.held.is_empty() && !may_hold_title(buf) {
            return std::borrow::Cow::Borrowed(buf);
        }
        let mut data = std::mem::take(&mut self.held);
        data.extend_from_slice(buf);
        let mut out = Vec::with_capacity(data.len());
        let mut i = 0;
        while i < data.len() {
            let Some(off) = data[i..].iter().position(|&b| b == 0x1b) else {
                out.extend_from_slice(&data[i..]);
                break;
            };
            let esc = i + off;
            out.extend_from_slice(&data[i..esc]);
            match data.get(esc + 1) {
                // a lone ESC at the end may be the start of a title
                None => {
                    self.held = vec![0x1b];
                    break;
                }
                Some(b']') => match osc_len(&data[esc..]) {
                    // An aborted OSC is dropped by the terminal too; what
                    // aborted it is output again.
                    Osc::Done(len) | Osc::Aborted(len) => {
                        if !is_title(&data[esc + 2..esc + len]) {
                            out.extend_from_slice(&data[esc..esc + len]);
                        }
                        i = esc + len;
                    }
                    Osc::Incomplete if data.len() - esc <= MAX_HELD => {
                        self.held = data[esc..].to_vec();
                        break;
                    }
                    Osc::Incomplete => {
                        out.extend_from_slice(&data[esc..]);
                        break;
                    }
                },
                Some(_) => {
                    out.push(0x1b);
                    i = esc + 1;
                }
            }
        }
        std::borrow::Cow::Owned(out)
    }

    /// Forget a held fragment: it belongs to the window we just left.
    pub fn reset(&mut self) {
        self.held.clear();
    }
}

/// Could `buf` hold a title sequence, or the start of one? A cheap pass that
/// lets almost every read through without a copy.
fn may_hold_title(buf: &[u8]) -> bool {
    buf.last() == Some(&0x1b) || buf.windows(2).any(|w| w == b"\x1b]")
}

enum Osc {
    /// Ends with its BEL or ST; this long.
    Done(usize),
    /// Cut off this far in, by CAN, SUB or an ESC that is not ST. A terminal
    /// abandons the OSC there and reads on from that byte.
    Aborted(usize),
    /// The buffer ends inside it.
    Incomplete,
}

/// The OSC at `buf[0..2] == ESC ]`.
fn osc_len(buf: &[u8]) -> Osc {
    for i in 2..buf.len() {
        match buf[i] {
            0x07 => return Osc::Done(i + 1),
            0x18 | 0x1a => return Osc::Aborted(i),
            0x1b => {
                return match buf.get(i + 1) {
                    Some(b'\\') => Osc::Done(i + 2),
                    Some(_) => Osc::Aborted(i),
                    None => Osc::Incomplete,
                }
            }
            _ => {}
        }
    }
    Osc::Incomplete
}

/// OSC 0 (icon and title), 1 (icon) and 2 (title). Not 7, the directory,
/// which is what draws the folder icon next to the title.
fn is_title(osc: &[u8]) -> bool {
    matches!(osc, [b'0' | b'1' | b'2', b';', ..])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(f: &mut Filter, chunks: &[&[u8]]) -> Vec<u8> {
        chunks.iter().flat_map(|c| f.filter(c).into_owned()).collect()
    }

    #[test]
    fn an_unterminated_title_ends_where_a_terminal_ends_it() {
        // A stray `ESC ] 2 ;` used to eat all output until a BEL.
        let mut f = Filter::default();
        let got = run(
            &mut f,
            &[b"\x1b]2;x", b"\x1b[32m$ \x1b[0mls\r\n", b"file1\r\n", b"\x07"],
        );
        assert_eq!(got, b"\x1b[32m$ \x1b[0mls\r\nfile1\r\n\x07".to_vec());
        // CAN and SUB abort it as well
        let mut f = Filter::default();
        assert_eq!(run(&mut f, &[b"\x1b]0;t\x18hello\x07"]), b"\x18hello\x07".to_vec());
        // a non-title OSC cut off the same way goes through as it came
        let mut f = Filter::default();
        assert_eq!(run(&mut f, &[b"\x1b]7;file://h/x\x1b[0m"]), b"\x1b]7;file://h/x\x1b[0m".to_vec());
        // ST split across reads is still ST
        let mut f = Filter::default();
        assert_eq!(run(&mut f, &[b"a\x1b]2;t\x1b", b"\\b"]), b"ab".to_vec());
    }

    #[test]
    fn titles_are_dropped_and_everything_else_kept() {
        let mut f = Filter::default();
        let input: &[u8] =
            b"a\x1b]0;zsh\x07b\x1b]2;nvim\x1b\\c\x1b[31mred\x1b]7;file://h/x\x07\x1b]1;i\x07d";
        assert_eq!(
            run(&mut f, &[input]),
            b"abc\x1b[31mred\x1b]7;file://h/x\x07d"
        );
    }

    #[test]
    fn plain_output_is_borrowed_not_copied() {
        let mut f = Filter::default();
        let buf: &[u8] = b"hello \x1b[2J\x1b[H world";
        assert!(matches!(f.filter(buf), std::borrow::Cow::Borrowed(_)));
    }

    #[test]
    fn a_title_split_across_reads_is_still_dropped() {
        let mut f = Filter::default();
        assert_eq!(run(&mut f, &[b"x\x1b", b"]2;ti", b"tle\x07y"]), b"xy");
        assert_eq!(run(&mut f, &[b"\x1b]2;a\x1b", b"\\z"]), b"z", "split inside ST");
    }

    #[test]
    fn a_held_fragment_that_is_not_a_title_comes_through_whole() {
        let mut f = Filter::default();
        assert_eq!(run(&mut f, &[b"\x1b", b"[1mB"]), b"\x1b[1mB");
        assert_eq!(run(&mut f, &[b"\x1b]7;fi", b"le://h/x\x07"]), b"\x1b]7;file://h/x\x07");
    }

    #[test]
    fn an_unterminated_osc_does_not_stall_output() {
        let mut f = Filter::default();
        let mut big = b"\x1b]2;".to_vec();
        big.extend(std::iter::repeat_n(b'x', MAX_HELD + 10));
        let out = f.filter(&big).into_owned();
        assert_eq!(out, big, "let through once it is too long to be a title");
    }

    #[test]
    fn reset_drops_a_held_fragment() {
        let mut f = Filter::default();
        assert_eq!(run(&mut f, &[b"\x1b]2;old"]), b"");
        f.reset();
        assert_eq!(run(&mut f, &[b"new"]), b"new");
    }

    #[test]
    fn the_title_we_set_cannot_carry_a_sequence() {
        assert_eq!(set("glass"), b"\x1b]2;glass\x07");
        assert_eq!(set("a\x07\x1b]2;b"), b"\x1b]2;a]2;b\x07");
    }
}
