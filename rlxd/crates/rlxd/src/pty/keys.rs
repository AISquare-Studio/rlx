// Ported from tmux 3.8's key-string.c, input-keys.c, cmd-send-keys.c and, for `paste`,
// cmd-paste-buffer.c. Their notices follow: the permission notice is the same for each.
//
// Copyright (c) 2007 Nicholas Marriott <nicholas.marriott@gmail.com>
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

//! Keys as tmux's `send-keys` sends them: a key name read as tmux reads it, then the
//! bytes tmux writes for it, which depend on the modes the program set (application
//! cursor keys and keypad, extended keys).
//!
//! Key codes keep tmux's layout, so its table lookups and fallbacks work the same: the
//! key in the low 40 bits (a character's code point, or a function key, whose type is
//! in bits 32 to 39), then the modifiers and the flags. A name tmux doesn't know is typed as
//! its characters, as `send-keys` types it. Mouse and user keys send nothing.

use crate::vt::InputModes;

// tmux.h's layout: the key in bits 0 to 39, which holds its type in bits 32 to 39; the
// modifiers in 40 to 47; the flags in 48 to 55.
const META: u64 = 1 << 44;
const CTRL: u64 = 1 << 45;
const SHIFT: u64 = 1 << 46;
const KEYPAD: u64 = 1 << 49;
const CURSOR: u64 = 1 << 50;
const IMPLIED_META: u64 = 1 << 51;
const MASK_TYPE: u64 = 0xff << 32;
const MASK_MODIFIERS: u64 = 0xff << 40;
const MASK_FLAGS: u64 = 0xff << 48;
const MASK_KEY: u64 = (1 << 40) - 1;

const TYPE_USER: u64 = 1 << 32;
const TYPE_FUNCTION: u64 = 2 << 32;
const TYPE_MOUSE: u64 = 3 << 32;

// The function keys, in tmux's order from KEYC_NONE.
const NONE: u64 = TYPE_FUNCTION;
const ANY: u64 = TYPE_FUNCTION + 4;
const BSPACE: u64 = TYPE_FUNCTION + 7;
const F1: u64 = TYPE_FUNCTION + 8;
const IC: u64 = F1 + 12;
const DC: u64 = IC + 1;
const HOME: u64 = IC + 2;
const END: u64 = IC + 3;
const NPAGE: u64 = IC + 4;
const PPAGE: u64 = IC + 5;
const BTAB: u64 = IC + 6;
const UP: u64 = IC + 7;
const DOWN: u64 = IC + 8;
const LEFT: u64 = IC + 9;
const RIGHT: u64 = IC + 10;
const KP: u64 = RIGHT + 1;

/// The keypad keys, in tmux's order: their names and what each sends outside
/// application keypad mode.
const KEYPAD_KEYS: [(&str, &str, &str); 16] = [
    ("KP/", "/", "\x1bOo"),
    ("KP*", "*", "\x1bOj"),
    ("KP-", "-", "\x1bOm"),
    ("KP7", "7", "\x1bOw"),
    ("KP8", "8", "\x1bOx"),
    ("KP9", "9", "\x1bOy"),
    ("KP+", "+", "\x1bOk"),
    ("KP4", "4", "\x1bOt"),
    ("KP5", "5", "\x1bOu"),
    ("KP6", "6", "\x1bOv"),
    ("KP1", "1", "\x1bOq"),
    ("KP2", "2", "\x1bOr"),
    ("KP3", "3", "\x1bOs"),
    ("KPEnter", "\n", "\x1bOM"),
    ("KP0", "0", "\x1bOp"),
    ("KP.", ".", "\x1bOn"),
];

/// The C0 controls' names, as tmux's table spells them, from NUL.
const C0_NAMES: [&str; 32] = [
    "[NUL]", "[SOH]", "[STX]", "[ETX]", "[EOT]", "[ENQ]", "[ASC]", "[BEL]", "[BS]", "Tab", "[LF]",
    "[VT]", "[FF]", "Enter", "[SO]", "[SI]", "[DLE]", "[DC1]", "[DC2]", "[DC3]", "[DC4]", "[NAK]",
    "[SYN]", "[ETB]", "[CAN]", "[EM]", "[SUB]", "Escape", "[FS]", "[GS]", "[RS]", "[US]",
];

/// tmux's `key_string_table`, but the keypad, C0 and mouse names.
fn named(name: &str) -> Option<u64> {
    let fixed: [(&str, u64); 25] = [
        ("IC", IC | IMPLIED_META),
        ("Insert", IC | IMPLIED_META),
        ("DC", DC | IMPLIED_META),
        ("Delete", DC | IMPLIED_META),
        ("Home", HOME | IMPLIED_META),
        ("End", END | IMPLIED_META),
        ("NPage", NPAGE | IMPLIED_META),
        ("PageDown", NPAGE | IMPLIED_META),
        ("PgDn", NPAGE | IMPLIED_META),
        ("PPage", PPAGE | IMPLIED_META),
        ("PageUp", PPAGE | IMPLIED_META),
        ("PgUp", PPAGE | IMPLIED_META),
        ("BTab", BTAB),
        ("Space", b' ' as u64),
        ("BSpace", BSPACE),
        ("Up", UP | CURSOR | IMPLIED_META),
        ("Down", DOWN | CURSOR | IMPLIED_META),
        ("Left", LEFT | CURSOR | IMPLIED_META),
        ("Right", RIGHT | CURSOR | IMPLIED_META),
        ("F1", F1 | IMPLIED_META),
        ("F2", (F1 + 1) | IMPLIED_META),
        ("F3", (F1 + 2) | IMPLIED_META),
        ("F4", (F1 + 3) | IMPLIED_META),
        ("F5", (F1 + 4) | IMPLIED_META),
        ("F6", (F1 + 5) | IMPLIED_META),
    ];
    let more: [(&str, u64); 6] = [
        ("F7", (F1 + 6) | IMPLIED_META),
        ("F8", (F1 + 7) | IMPLIED_META),
        ("F9", (F1 + 8) | IMPLIED_META),
        ("F10", (F1 + 9) | IMPLIED_META),
        ("F11", (F1 + 10) | IMPLIED_META),
        ("F12", (F1 + 11) | IMPLIED_META),
    ];
    if let Some((_, key)) = fixed
        .iter()
        .chain(more.iter())
        .find(|(n, _)| n.eq_ignore_ascii_case(name))
    {
        return Some(*key);
    }
    if let Some(i) = C0_NAMES.iter().position(|n| n.eq_ignore_ascii_case(name)) {
        return Some(i as u64);
    }
    if let Some(i) = KEYPAD_KEYS
        .iter()
        .position(|(n, _, _)| n.eq_ignore_ascii_case(name))
    {
        return Some((KP + i as u64) | KEYPAD);
    }
    if let Some(n) = name.strip_prefix("User")
        && let Ok(n) = n.parse::<u64>()
        && n <= 1000
    {
        return Some(TYPE_USER + n);
    }
    if is_mouse_name(name) {
        return Some(TYPE_MOUSE);
    }
    None
}

/// Whether `name` is one of tmux's mouse keys, such as `MouseDown1Pane` or `WheelUp`:
/// they send nothing without a mouse event.
fn is_mouse_name(name: &str) -> bool {
    [
        "MouseDown",
        "MouseUp",
        "MouseDragEnd",
        "MouseDrag",
        "MouseMove",
        "WheelUp",
        "WheelDown",
        "SecondClick",
        "DoubleClick",
        "TripleClick",
    ]
    .iter()
    .any(|prefix| name.len() > prefix.len() && name.starts_with(prefix))
}

/// The key `name` names, as tmux's `key_string_lookup_string` reads it: `None` for one
/// it doesn't know, which `send-keys` then types as its characters.
fn lookup(name: &str) -> Option<u64> {
    if name.eq_ignore_ascii_case("None") {
        return None;
    }
    if name.eq_ignore_ascii_case("Any") {
        return Some(ANY);
    }
    if let Some(hex) = name.strip_prefix("0x") {
        // As sscanf's %x reads it: the hex digits at the start, and no more.
        let digits: String = hex.chars().take_while(char::is_ascii_hexdigit).collect();
        let u = u32::from_str_radix(&digits, 16).ok()?;
        if u < 32 {
            return Some(u64::from(u));
        }
        return char::from_u32(u).map(|c| c as u64);
    }
    let mut rest = name;
    let mut modifiers = 0;
    if let Some(after) = rest.strip_prefix('^')
        && !after.is_empty()
    {
        let mut chars = after.chars();
        if let (Some(c), None) = (chars.next(), chars.clone().next())
            && c.is_ascii()
        {
            return Some(c.to_ascii_lowercase() as u64 | CTRL);
        }
        modifiers |= CTRL;
        rest = after;
    }
    loop {
        let bytes = rest.as_bytes();
        if bytes.len() < 2 || bytes[1] != b'-' {
            break;
        }
        modifiers |= match bytes[0] {
            b'C' | b'c' => CTRL,
            b'M' | b'm' => META,
            b'S' | b's' => SHIFT,
            _ => return None,
        };
        rest = &rest[2..];
    }
    if rest.is_empty() {
        return None;
    }
    let mut chars = rest.chars();
    let first = chars.next()?;
    if chars.next().is_none() {
        if first.is_ascii() {
            if (first as u32) < 32 {
                return None;
            }
            return Some(first as u64 | modifiers);
        }
        return Some(first as u64 | modifiers);
    }
    let mut key = named(rest)?;
    if modifiers & META == 0 {
        key &= !IMPLIED_META;
    }
    Some(key | modifiers)
}

/// tmux's `input_key_defaults`, with the modifier sequences built as
/// `input_key_build` builds them.
fn table(key: u64) -> Option<String> {
    let base = key & !(MASK_MODIFIERS | IMPLIED_META);
    let plain: Option<&str> = match key {
        k if k == F1 => Some("\x1bOP"),
        k if k == F1 + 1 => Some("\x1bOQ"),
        k if k == F1 + 2 => Some("\x1bOR"),
        k if k == F1 + 3 => Some("\x1bOS"),
        k if k == F1 + 4 => Some("\x1b[15~"),
        k if k == F1 + 5 => Some("\x1b[17~"),
        k if k == F1 + 6 => Some("\x1b[18~"),
        k if k == F1 + 7 => Some("\x1b[19~"),
        k if k == F1 + 8 => Some("\x1b[20~"),
        k if k == F1 + 9 => Some("\x1b[21~"),
        k if k == F1 + 10 => Some("\x1b[23~"),
        k if k == F1 + 11 => Some("\x1b[24~"),
        k if k == IC => Some("\x1b[2~"),
        k if k == DC => Some("\x1b[3~"),
        k if k == HOME => Some("\x1b[1~"),
        k if k == END => Some("\x1b[4~"),
        k if k == NPAGE => Some("\x1b[6~"),
        k if k == PPAGE => Some("\x1b[5~"),
        k if k == BTAB => Some("\x1b[Z"),
        k if k == UP | CURSOR => Some("\x1bOA"),
        k if k == DOWN | CURSOR => Some("\x1bOB"),
        k if k == RIGHT | CURSOR => Some("\x1bOC"),
        k if k == LEFT | CURSOR => Some("\x1bOD"),
        k if k == UP => Some("\x1b[A"),
        k if k == DOWN => Some("\x1b[B"),
        k if k == RIGHT => Some("\x1b[C"),
        k if k == LEFT => Some("\x1b[D"),
        _ => None,
    };
    if let Some(data) = plain {
        return Some(data.to_owned());
    }
    if (KP..KP + 16).contains(&(key & !KEYPAD)) && key & (MASK_MODIFIERS | IMPLIED_META) == 0 {
        let (_, normal, application) = KEYPAD_KEYS[(key & !KEYPAD) as usize - KP as usize];
        return Some(
            if key & KEYPAD != 0 {
                application
            } else {
                normal
            }
            .to_owned(),
        );
    }
    // The modified forms: modifier 2 to 8, as input_key_modifiers lists them.
    let modifiers = key & (MASK_MODIFIERS | IMPLIED_META);
    let digit = [
        (SHIFT, 2),
        (META | IMPLIED_META, 3),
        (SHIFT | META | IMPLIED_META, 4),
        (CTRL, 5),
        (SHIFT | CTRL, 6),
        (META | IMPLIED_META | CTRL, 7),
        (SHIFT | META | IMPLIED_META | CTRL, 8),
    ]
    .iter()
    .find(|(m, _)| *m == modifiers)
    .map(|(_, d)| *d)?;
    let template = match base {
        k if k == F1 => "\x1b[1;_P",
        k if k == F1 + 1 => "\x1b[1;_Q",
        k if k == F1 + 2 => "\x1b[1;_R",
        k if k == F1 + 3 => "\x1b[1;_S",
        k if k == F1 + 4 => "\x1b[15;_~",
        k if k == F1 + 5 => "\x1b[17;_~",
        k if k == F1 + 6 => "\x1b[18;_~",
        k if k == F1 + 7 => "\x1b[19;_~",
        k if k == F1 + 8 => "\x1b[20;_~",
        k if k == F1 + 9 => "\x1b[21;_~",
        k if k == F1 + 10 => "\x1b[23;_~",
        k if k == F1 + 11 => "\x1b[24;_~",
        k if k == UP => "\x1b[1;_A",
        k if k == DOWN => "\x1b[1;_B",
        k if k == RIGHT => "\x1b[1;_C",
        k if k == LEFT => "\x1b[1;_D",
        k if k == HOME => "\x1b[1;_H",
        k if k == END => "\x1b[1;_F",
        k if k == PPAGE => "\x1b[5;_~",
        k if k == NPAGE => "\x1b[6;_~",
        k if k == IC => "\x1b[2;_~",
        k if k == DC => "\x1b[3;_~",
        _ => return None,
    };
    Some(template.replacen('_', &digit.to_string(), 1))
}

fn is_unicode(key: u64) -> bool {
    key & MASK_TYPE == 0 && key & MASK_KEY > 0x7f
}

fn utf8_of(key: u64) -> Vec<u8> {
    char::from_u32((key & MASK_KEY) as u32)
        .map(|c| c.to_string().into_bytes())
        .unwrap_or_default()
}

/// The extended form of a modified key, xterm's `CSI 27 ; m ; k ~`, as tmux's default
/// `extended-keys-format` writes it.
fn extended(key: u64, out: &mut Vec<u8>) -> bool {
    let modifier = match key & MASK_MODIFIERS {
        m if m == SHIFT => '2',
        m if m == META => '3',
        m if m == SHIFT | META => '4',
        m if m == CTRL => '5',
        m if m == SHIFT | CTRL => '6',
        m if m == META | CTRL => '7',
        m if m == SHIFT | META | CTRL => '8',
        _ => return false,
    };
    let code = key & MASK_KEY;
    out.extend_from_slice(format!("\x1b[27;{modifier};{code}~").as_bytes());
    true
}

/// The "standard" mode, tmux's `input_key_vt10x`.
fn vt10x(key: u64, out: &mut Vec<u8>) -> bool {
    const FROM: &[u8] = b"1!9(0)=+;:'\",<.>/-8? 2";
    const TO: &[u8] = b"119900=+;;'',,..\x1f\x1f\x7f\x7f\0\0";
    if key & META != 0 {
        out.push(0x1b);
    }
    if is_unicode(key) {
        out.extend(utf8_of(key));
        return true;
    }
    let mut key = key;
    let only = key & MASK_KEY;
    if only == u64::from(b'\r') || only == u64::from(b'\n') || only == u64::from(b'\t') {
        key &= !CTRL;
    }
    if key & CTRL != 0 {
        let byte = only as u8;
        key = if let Some(at) = FROM.iter().position(|&c| u64::from(c) == only) {
            u64::from(TO[at])
        } else if (b'3'..=b'7').contains(&byte) && only < 0x80 {
            u64::from(byte - 0x18)
        } else if (b'@'..=b'~').contains(&byte) && only < 0x80 {
            u64::from(byte & 0x1f)
        } else {
            return false;
        };
    }
    out.push((key & 0x7f) as u8);
    true
}

/// tmux's `input_key_mode1`: the keys modifyOtherKeys 1 still sends in the standard form.
fn mode1(key: u64, out: &mut Vec<u8>) -> bool {
    if key & (CTRL | META) == META {
        return vt10x(key, out);
    }
    let only = key & MASK_KEY;
    if key & CTRL != 0 && only < 0x80 {
        let c = only as u8;
        if c == b' '
            || c == b'/'
            || c == b'@'
            || c == b'^'
            || (b'2'..=b'8').contains(&c)
            || (b'@'..=b'~').contains(&c)
        {
            return vt10x(key, out);
        }
    }
    false
}

/// Writes the bytes tmux's `input_key` writes for `key`, given the program's `modes`.
/// `false` where tmux's returns -1, a key it can't send in this mode: what was written
/// stays, and `send-keys` types the name instead.
fn input_key(key: u64, modes: InputModes, out: &mut Vec<u8>) -> bool {
    let mut key = key;
    if key & MASK_TYPE == TYPE_MOUSE {
        return true;
    }
    if key & MASK_KEY == BSPACE {
        // tmux's `backspace` option: C-?, so DEL.
        if key & MASK_MODIFIERS == 0 {
            out.push(0x7f);
            return true;
        }
        key = 0x7f | (key & (MASK_FLAGS | MASK_MODIFIERS));
    }
    if key & MASK_KEY == BTAB {
        if modes.extended_keys == 2 {
            key = u64::from(b'\t') | (key & !MASK_KEY) | SHIFT;
        } else {
            key &= !MASK_MODIFIERS;
        }
    }
    if key & !MASK_KEY == 0 {
        if key == 0x09 || key == 0x0d || key == 0x1b || (0x20..=0x7f).contains(&key) {
            out.push(key as u8);
            return true;
        }
        if is_unicode(key) {
            out.extend(utf8_of(key));
            return true;
        }
    }
    if !modes.keypad {
        key &= !KEYPAD;
    }
    if !modes.cursor_keys {
        key &= !CURSOR;
    }
    let mut found = table(key);
    if found.is_none() && key & META != 0 && key & IMPLIED_META == 0 {
        found = table(key & !META);
    }
    if found.is_none() && key & CURSOR != 0 {
        found = table(key & !CURSOR);
    }
    if found.is_none() && key & KEYPAD != 0 {
        found = table(key & !KEYPAD);
    }
    if let Some(data) = found {
        if key & META != 0 && key & IMPLIED_META == 0 {
            out.push(0x1b);
        }
        out.extend_from_slice(data.as_bytes());
        return true;
    }
    if key & MASK_TYPE == TYPE_USER || key & MASK_TYPE == TYPE_FUNCTION {
        return true;
    }
    match modes.extended_keys {
        2 => extended(key, out),
        1 => mode1(key, out) || extended(key, out),
        _ => vt10x(key, out),
    }
}

/// The bytes `send-keys` sends for `keys`, each a key name or, when tmux knows no key of
/// that name, characters to type, given the program's `modes`.
pub fn send_keys(keys: &[String], modes: InputModes) -> Vec<u8> {
    let mut out = Vec::new();
    for name in keys {
        let sent = match lookup(name) {
            Some(key) if key != NONE => input_key(key, modes, &mut out),
            _ => false,
        };
        if !sent {
            for c in name.chars() {
                input_key(c as u64, modes, &mut out);
            }
        }
    }
    out
}

/// `text` as tmux's `paste-buffer -p` sends it: each line made safe for a terminal, as
/// `vis(VIS_SAFE | VIS_NOSLASH)` makes it (a control character but tab, backspace, bell
/// and carriage return written as `^X`, delete as `^?`), each newline a carriage
/// return, and the whole bracketed when the program asked for bracketed paste.
pub fn paste(text: &str, bracketed: bool) -> Vec<u8> {
    let mut out = Vec::with_capacity(text.len() + 12);
    if bracketed {
        out.extend_from_slice(b"\x1b[200~");
    }
    for c in text.chars() {
        match c {
            '\n' => out.push(b'\r'),
            '\t' | '\u{8}' | '\u{7}' | '\r' | ' '..='~' => out.push(c as u8),
            '\u{7f}' => out.extend_from_slice(b"^?"),
            c if (c as u32) < 0x20 => {
                out.push(b'^');
                out.push(c as u8 + b'@');
            }
            c => {
                let mut utf8 = [0u8; 4];
                out.extend_from_slice(c.encode_utf8(&mut utf8).as_bytes());
            }
        }
    }
    if bracketed {
        out.extend_from_slice(b"\x1b[201~");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys(names: &[&str], modes: InputModes) -> Vec<u8> {
        send_keys(
            &names.iter().map(|n| (*n).to_owned()).collect::<Vec<_>>(),
            modes,
        )
    }

    fn plain(names: &[&str]) -> Vec<u8> {
        keys(names, InputModes::default())
    }

    #[test]
    fn the_fleets_keys_send_tmuxs_bytes() {
        assert_eq!(plain(&["Enter", "Escape", "Tab", "Space"]), b"\r\x1b\t ");
        assert_eq!(
            plain(&["C-c", "C-d", "C-j", "C-l", "C-u", "C-@"]),
            b"\x03\x04\n\x0c\x15\0"
        );
        assert_eq!(plain(&["BSpace", "BTab", "S-Enter"]), b"\x7f\x1b[Z\r");
        assert_eq!(
            plain(&["Up", "Down", "Left", "Right"]),
            b"\x1b[A\x1b[B\x1b[D\x1b[C"
        );
        assert_eq!(
            plain(&["Home", "End", "PageUp", "PPage", "NPage", "DC"]),
            b"\x1b[1~\x1b[4~\x1b[5~\x1b[5~\x1b[6~\x1b[3~"
        );
        assert_eq!(plain(&["F1", "F5", "F12"]), b"\x1bOP\x1b[15~\x1b[24~");
        assert_eq!(
            plain(&["M-x", "M-Up", "C-Up", "C-M-S-Up", "S-F5"]),
            b"\x1bx\x1b[1;3A\x1b[1;5A\x1b[1;8A\x1b[15;2~"
        );
        assert_eq!(plain(&["1", "y", "n", "é"]), "1yné".as_bytes());
    }

    #[test]
    fn the_programs_modes_change_what_a_key_sends() {
        let cursor = InputModes {
            cursor_keys: true,
            ..InputModes::default()
        };
        assert_eq!(keys(&["Up", "M-Up"], cursor), b"\x1bOA\x1b[1;3A");
        let keypad = InputModes {
            keypad: true,
            ..InputModes::default()
        };
        assert_eq!(keys(&["KP5", "KPEnter"], keypad), b"\x1bOu\x1bOM");
        assert_eq!(plain(&["KP5", "KPEnter"]), b"5\n");
        let extended = InputModes {
            extended_keys: 1,
            ..InputModes::default()
        };
        assert_eq!(
            keys(&["S-Enter", "C-c", "C-Enter"], extended),
            b"\x1b[27;2;13~\x03\x1b[27;5;13~"
        );
    }

    #[test]
    fn a_name_tmux_does_not_know_is_typed() {
        assert_eq!(plain(&["Entr", "C-", "None"]), b"EntrC-None");
        assert_eq!(plain(&["MouseDown1Pane", "User3", "Any"]), b"");
        // A key tmux can't send in this mode is typed as its name, after what it wrote.
        assert_eq!(
            plain(&["C-BSpace", "C-M-BSpace"]),
            b"C-BSpace\x1bC-M-BSpace"
        );
        assert_eq!(plain(&["^c", "^Up"]), b"\x03\x1b[1;5A");
    }

    #[test]
    fn a_paste_is_made_safe_and_bracketed_when_asked() {
        assert_eq!(paste("a\nb", false), b"a\rb");
        assert_eq!(paste("a\nb", true), b"\x1b[200~a\rb\x1b[201~");
        assert_eq!(paste("\x1b[31m\x03\x7f\tok\\", false), b"^[[31m^C^?\tok\\");
        assert_eq!(paste("中\r\n", false), "中\r\r".as_bytes());
    }
}
