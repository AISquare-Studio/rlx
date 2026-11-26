//! Base64, standard alphabet with padding (RFC 4648 §4), as Python's `base64.b64encode`
//! writes it: how a hook's bytes cross JSON, which carries text only. Strict: a decode
//! refuses any other character, missing padding and stray bits.

const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// `bytes` in base64.
pub fn encode(bytes: &[u8]) -> String {
    let mut text = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = (u32::from(chunk[0]) << 16)
            | (u32::from(*chunk.get(1).unwrap_or(&0)) << 8)
            | u32::from(*chunk.get(2).unwrap_or(&0));
        for (i, shift) in [18, 12, 6, 0].into_iter().enumerate() {
            if i <= chunk.len() {
                text.push(char::from(ALPHABET[((n >> shift) & 63) as usize]));
            } else {
                text.push('=');
            }
        }
    }
    text
}

/// The bytes `text` holds, or `None` when it is not strict base64.
pub fn decode(text: &str) -> Option<Vec<u8>> {
    let text = text.as_bytes();
    if !text.len().is_multiple_of(4) {
        return None;
    }
    let value = |c: u8| ALPHABET.iter().position(|&a| a == c).map(|v| v as u32);
    let mut bytes = Vec::with_capacity(text.len() / 4 * 3);
    let quads = text.len() / 4;
    for (q, quad) in text.chunks(4).enumerate() {
        let pad = quad.iter().rev().take_while(|&&c| c == b'=').count();
        if pad > 2 || (pad > 0 && q + 1 != quads) {
            return None;
        }
        let mut n = 0u32;
        for &c in &quad[..4 - pad] {
            n = (n << 6) | value(c)?;
        }
        n <<= 6 * pad as u32;
        let out = [(n >> 16) as u8, (n >> 8) as u8, n as u8];
        let keep = 3 - pad;
        if out[keep..].iter().any(|&b| b != 0) {
            return None;
        }
        bytes.extend_from_slice(&out[..keep]);
    }
    Some(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn it_round_trips_and_matches_rfc_4648() {
        for (plain, coded) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(encode(plain.as_bytes()), coded);
            assert_eq!(decode(coded).unwrap(), plain.as_bytes());
        }
        let all: Vec<u8> = (0..=255).collect();
        assert_eq!(decode(&encode(&all)).unwrap(), all);
    }

    #[test]
    fn it_refuses_what_is_not_strict_base64() {
        for bad in [
            "Zg", "Zg=", "Z===", "Zm9v\n", "Zm=v", "Zh==", "Zm9-", "Zg==Zg==",
        ] {
            assert_eq!(decode(bad), None, "{bad:?}");
        }
    }
}
