// Ported in part from tmux 3.8's utf8.c and utf8-combined.c, whose notice follows.
//
// Copyright (c) 2008 Nicholas Marriott <nicholas.marriott@gmail.com>
//
// Permission to use, copy, modify, and distribute this software for any
// purpose with or without fee is hereby granted, provided that the above
// copyright notice and this permission notice appear in all copies.
//
// THE SOFTWARE IS PROVIDED "AS IS" AND THE AUTHOR DISCLAIMS ALL WARRANTIES
// WITH REGARD TO THIS SOFTWARE INCLUDING ALL IMPLIED WARRANTIES OF
// MERCHANTABILITY AND FITNESS. IN NO EVENT SHALL THE AUTHOR BE LIABLE FOR
// ANY SPECIAL, DIRECT, INDIRECT, OR CONSEQUENTIAL DAMAGES OR ANY DAMAGES
// WHATSOEVER RESULTING FROM LOSS OF MIND, USE, DATA OR PROFITS, WHETHER
// IN AN ACTION OF CONTRACT, NEGLIGENCE OR OTHER TORTIOUS ACTION, ARISING
// OUT OF OR IN CONNECTION WITH THE USE OR PERFORMANCE OF THIS SOFTWARE.

//! UTF-8 as tmux reads it: one character at a time from the bytes a program writes, each
//! with its width, and the rules for what combines with the character before it.
//!
//! Widths come from `unicode-width`, which agrees with the platform tables tmux uses on
//! the characters the frame tests write (CJK, combining marks). Emoji widths vary by
//! platform and tmux build, and are not matched.

use unicode_width::UnicodeWidthChar;

/// tmux's `variation-selector-always-wide`, which is on by default: VS16 makes the
/// character before it two cells wide.
pub const VARIATION_SELECTOR_ALWAYS_WIDE: bool = true;

/// How one more byte of a character went.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    /// The character needs more bytes.
    More,
    /// The character is complete.
    Done,
    /// The bytes are no character.
    Error,
}

/// A character being read, as tmux's `utf8_data` while `utf8_append` runs.
#[derive(Clone, Debug, Default)]
pub struct Decoder {
    data: Vec<u8>,
    size: usize,
    bad: bool,
    width: u8,
}

impl Decoder {
    /// Starts a character with its lead byte, as `utf8_open` does.
    pub fn open(&mut self, byte: u8) -> Step {
        *self = Self::default();
        self.size = match byte {
            0xc2..=0xdf => 2,
            0xe0..=0xef => 3,
            0xf0..=0xf4 => 4,
            _ => return Step::Error,
        };
        self.append(byte);
        Step::More
    }

    /// Adds a byte, as `utf8_append` does: a byte that isn't a continuation spoils the
    /// character, which still takes its full count of bytes.
    pub fn append(&mut self, byte: u8) -> Step {
        if !self.data.is_empty() && byte & 0xc0 != 0x80 {
            self.bad = true;
        }
        self.data.push(byte);
        if self.data.len() != self.size {
            return Step::More;
        }
        if self.bad {
            return Step::Error;
        }
        match width_of(&self.data) {
            Some(width) => {
                self.width = width;
                Step::Done
            }
            None => Step::Error,
        }
    }

    /// The character's bytes.
    pub fn bytes(&self) -> &[u8] {
        &self.data
    }

    /// Its width, once done.
    pub fn width(&self) -> u8 {
        self.width
    }
}

/// The width of the one character `bytes` holds, as tmux's `utf8_width` works it out:
/// `None` when they are no single valid character.
pub fn width_of(bytes: &[u8]) -> Option<u8> {
    let text = std::str::from_utf8(bytes).ok()?;
    let mut chars = text.chars();
    let c = chars.next()?;
    if chars.next().is_some() {
        return None;
    }
    let width = match c.width() {
        Some(width) => width,
        // A C1 control is zero width; any other unprintable character, one.
        None if ('\u{80}'..='\u{9f}').contains(&c) => 0,
        None => 1,
    };
    u8::try_from(width).ok()
}

/// U+200D ZERO WIDTH JOINER.
pub fn is_zwj(bytes: &[u8]) -> bool {
    bytes == "\u{200d}".as_bytes()
}

/// U+FE0F VARIATION SELECTOR-16.
pub fn is_vs(bytes: &[u8]) -> bool {
    bytes == "\u{fe0f}".as_bytes()
}

/// U+3164 HANGUL FILLER, which tmux drops.
pub fn is_hangul_filler(bytes: &[u8]) -> bool {
    bytes == "\u{3164}".as_bytes()
}

/// Whether `bytes` end with a zero width joiner.
pub fn has_zwj(bytes: &[u8]) -> bool {
    bytes.ends_with("\u{200d}".as_bytes())
}

fn first_char(bytes: &[u8]) -> Option<char> {
    std::str::from_utf8(bytes).ok()?.chars().next()
}

fn regional_count(bytes: &[u8]) -> usize {
    std::str::from_utf8(bytes).map_or(0, |text| {
        text.chars()
            .filter(|c| ('\u{1f1e6}'..='\u{1f1ff}').contains(c))
            .count()
    })
}

/// Emoji that take a skin tone modifier after them, as tmux lists them.
const TAKES_SKIN_TONE: &[u32] = &[
    0x1F44B, 0x1F44C, 0x1F44D, 0x1F44E, 0x1F44F, 0x1F450, 0x1F466, 0x1F467, 0x1F468, 0x1F469,
    0x1F46E, 0x1F470, 0x1F471, 0x1F472, 0x1F473, 0x1F474, 0x1F475, 0x1F476, 0x1F477, 0x1F478,
    0x1F47C, 0x1F481, 0x1F482, 0x1F483, 0x1F485, 0x1F486, 0x1F487, 0x1F4AA, 0x1F575, 0x1F57A,
    0x1F590, 0x1F595, 0x1F596, 0x1F645, 0x1F646, 0x1F647, 0x1F64B, 0x1F64C, 0x1F64D, 0x1F64E,
    0x1F64F, 0x1F6B4, 0x1F6B5, 0x1F6B6, 0x1F926, 0x1F937, 0x1F938, 0x1F939, 0x1F93D, 0x1F93E,
    0x1F9B5, 0x1F9B6, 0x1F9B8, 0x1F9B9, 0x1F9CD, 0x1F9CE, 0x1F9CF, 0x1F9D1, 0x1F9D2, 0x1F9D3,
    0x1F9D4, 0x1F9D5, 0x1F9D6, 0x1F9D7, 0x1F9D8, 0x1F9D9, 0x1F9DA, 0x1F9DB, 0x1F9DC, 0x1F9DD,
    0x1F9DE, 0x1F9DF,
];

/// Whether `add` combines with `with`, as tmux's `utf8_should_combine` says: a second
/// regional indicator, or a skin tone on an emoji that takes one.
pub fn should_combine(with: &[u8], add: &[u8]) -> bool {
    let (Some(w), Some(a)) = (first_char(with), first_char(add)) else {
        return false;
    };
    let regional = '\u{1f1e6}'..='\u{1f1ff}';
    if regional.contains(&a) && regional.contains(&w) {
        return regional_count(with) == 1 && regional_count(add) == 1;
    }
    TAKES_SKIN_TONE.contains(&(a as u32)) && ('\u{1f3fb}'..='\u{1f3ff}').contains(&w)
}

/// Whether `title` may be a title: tmux's `screen_set_title` takes valid UTF-8, which a
/// `&str` always is.
pub fn is_valid_title(title: &str) -> bool {
    !title.contains('\0')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bad_sequence_takes_its_full_count_of_bytes() {
        let mut decoder = Decoder::default();
        assert_eq!(decoder.open(0xe4), Step::More);
        assert_eq!(
            decoder.append(b'x'),
            Step::More,
            "not a continuation, yet counted"
        );
        assert_eq!(decoder.append(0xa9), Step::Error);
        assert_eq!(decoder.open(0xe4), Step::More);
        assert_eq!(decoder.append(0xb8), Step::More);
        assert_eq!(decoder.append(0xad), Step::Done);
        assert_eq!((decoder.bytes(), decoder.width()), ("中".as_bytes(), 2));
        assert_eq!(decoder.open(0x80), Step::Error);
    }

    #[test]
    fn widths_and_combining_follow_tmux() {
        assert_eq!(width_of("é".as_bytes()), Some(1));
        assert_eq!(width_of("\u{301}".as_bytes()), Some(0));
        assert_eq!(width_of("\u{85}".as_bytes()), Some(0));
        assert!(should_combine("🇬".as_bytes(), "🇧".as_bytes()));
        assert!(should_combine("🏽".as_bytes(), "👍".as_bytes()));
        assert!(!should_combine("a".as_bytes(), "b".as_bytes()));
    }
}
