//! Word wrapping by measured width. Replaces the bitmap font's "chars
//! per line" arithmetic, which only worked because every glyph was the
//! same width.
//!
//! There is no caret-locating helper here on purpose: the chat buffer
//! only appends and pops, so the caret is always at the end of the last
//! wrapped line. Add one when arrow-key editing arrives, not before.

use super::Face;
use std::ops::Range;

/// Greedy word wrap. Appends byte ranges of `text` into `out` (cleared
/// first). Breaks at spaces where possible; a word wider than `max_w`
/// hard-breaks at the last char that fits; a single char wider than
/// `max_w` still gets its own line, so this never emits an empty range
/// and never loops.
///
/// The space at a soft break belongs to neither line.
pub fn wrap(face: &Face, text: &str, px: f32, max_w: f32, out: &mut Vec<Range<usize>>) {
	out.clear();
	if text.is_empty() {
		out.push(0..0);
		return;
	}
	let mut start = 0usize;
	let mut w = 0.0f32;
	// (end of line, start of next) for the most recent space.
	let mut brk: Option<(usize, usize)> = None;
	for (i, c) in text.char_indices() {
		let a = face.advance(c, px);
		if c == ' ' {
			brk = Some((i, i + c.len_utf8()));
		}
		if w + a > max_w && i > start {
			let (end, next) = match brk.filter(|&(e, _)| e > start) {
				Some(b) => b,
				None => (i, i),
			};
			out.push(start..end);
			start = next;
			brk = None;
			// Whatever came after the break point carries to the new line.
			// `next > i` means the overflowing char was itself the break
			// space: it is consumed, and the new line starts empty.
			w = if next > i {
				0.0
			} else {
				face.measure(&text[next..i], px) + a
			};
		} else {
			w += a;
		}
	}
	if start < text.len() || out.is_empty() {
		out.push(start..text.len());
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::font;

	fn lines(text: &str, px: f32, max_w: f32) -> Vec<&str> {
		let mut out = Vec::new();
		wrap(font::ui(), text, px, max_w, &mut out);
		out.into_iter().map(|r| &text[r]).collect()
	}

	#[test]
	fn breaks_at_spaces_and_fits_every_line() {
		let px = 16.0;
		let max = font::ui().measure("the quick brown", px);
		let text = "the quick brown fox jumps over the lazy dog";
		let got = lines(text, px, max);
		assert!(got.len() > 2);
		for l in &got {
			assert!(
				font::ui().measure(l, px) <= max + 1e-3,
				"line {l:?} overflows"
			);
			assert!(!l.is_empty());
		}
		// Content survives; only the break spaces are dropped.
		assert_eq!(got.join(" "), text);
	}

	#[test]
	fn hard_breaks_a_word_with_no_space_to_break_at() {
		let px = 16.0;
		let max = font::ui().measure("aaaa", px);
		let got = lines("aaaaaaaaaaaa", px, max);
		assert!(got.len() >= 3);
		assert_eq!(got.concat(), "aaaaaaaaaaaa");
	}

	// The degenerate case that would otherwise spin forever or emit empty
	// lines: nothing fits at all.
	#[test]
	fn a_char_wider_than_the_line_still_gets_one() {
		let got = lines("WWW", 16.0, 1.0);
		assert_eq!(got, vec!["W", "W", "W"]);
	}

	#[test]
	fn empty_text_is_one_empty_line() {
		assert_eq!(lines("", 16.0, 100.0), vec![""]);
	}
}
