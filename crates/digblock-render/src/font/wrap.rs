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
/// first). Breaks at spaces where possible, or after a `-` or `/`; a
/// word wider than `max_w` hard-breaks at the last char that fits; a
/// single char wider than `max_w` still gets its own line, so this never
/// emits an empty range and never loops.
///
/// The space at a soft break belongs to neither line. A `-` or `/`
/// belongs to the line above it, which is why it is only recorded as a
/// break opportunity once it is known to fit.
pub(crate) fn wrap(face: &Face, text: &str, px: f32, max_w: f32, out: &mut Vec<Range<usize>>) {
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
			// Break-after opportunity. Recorded here, in the branch that
			// knows the character fits, because unlike a space it stays on
			// the line above — offering it before the fit test could push
			// it past `max_w`.
			if c == '-' || c == '/' {
				let after = i + c.len_utf8();
				brk = Some((after, after));
			}
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

	/// A long hyphenated or slashed run is still made of words; breaking
	/// it at the punctuation beats hard-breaking it mid-word. The
	/// separator stays on the line above, so nothing is lost and no line
	/// starts with one.
	#[test]
	fn breaks_after_hyphens_and_slashes() {
		let px = 16.0;
		let text = "north-east-of-spawn";
		let max = font::ui().measure("north-east", px);
		let got = lines(text, px, max);
		assert!(got.len() >= 2, "{got:?}");
		assert_eq!(got.concat(), text, "no characters lost");
		for line in &got {
			assert!(font::ui().measure(line, px) <= max + 1e-3, "{line:?}");
			assert!(
				!line.starts_with(['-', '/']),
				"separator led a line: {got:?}"
			);
		}
		// The hard-break fallback still covers a run with no separator.
		let plain = lines("northeastofspawnnn", px, max);
		assert_eq!(plain.concat(), "northeastofspawnnn");
	}

	#[test]
	fn empty_text_is_one_empty_line() {
		assert_eq!(lines("", 16.0, 100.0), vec![""]);
	}

	/// The property the chat column actually depends on, stated directly:
	/// a break may only land where a space was, unless the word itself is
	/// too wide for a whole line. Checked at the real chat geometry (the
	/// column is half the window wide, in the UI face) over a message at
	/// the wire's length limit, because a rule that holds for "the quick
	/// brown fox" can still fail on a realistic sentence.
	#[test]
	fn a_full_length_chat_line_only_breaks_between_words() {
		let px = 16.0;
		// overlay.rs: half the window, less MARGIN + 2*PAD at scale 1.
		let max_w = 1280.0 / 2.0 - (8.0 + 6.0 * 2.0);
		let text = "hey has anyone seen the waterfall east of spawn? \
		            i built a staircase down to the lake and there is a \
		            cave behind it with iron";
		let got = lines(text, px, max_w);
		assert!(got.len() >= 2, "expected a wrap, got {got:?}");
		let mut at = 0usize;
		for line in &got {
			assert!(font::ui().measure(line, px) <= max_w + 1e-3, "{line:?}");
			// Every line boundary must sit on a space in the original.
			at += line.len();
			if at < text.len() {
				assert_eq!(
					&text[at..at + 1],
					" ",
					"break inside a word at byte {at}: {got:?}"
				);
				at += 1;
			}
		}
		assert_eq!(got.join(" "), text);
	}
}
