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
                    Some(len) => {
                        if !is_title(&data[esc + 2..esc + len]) {
                            out.extend_from_slice(&data[esc..esc + len]);
                        }
                        i = esc + len;
                    }
                    None if data.len() - esc <= MAX_HELD => {
                        self.held = data[esc..].to_vec();
                        break;
                    }
                    None => {
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

/// Length of the OSC at `buf[0..2] == ESC ]`, through its BEL or ST.
fn osc_len(buf: &[u8]) -> Option<usize> {
    let mut i = 2;
    while i < buf.len() {
        match buf[i] {
            0x07 => return Some(i + 1),
            0x1b if buf.get(i + 1) == Some(&b'\\') => return Some(i + 2),
            _ => i += 1,
        }
    }
    None
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
