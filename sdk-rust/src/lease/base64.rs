//! Base64url, as the reference reads and writes a token's parts and a key's `x`.

use crate::wire::utf16_len;

/// Base64url as the reference decodes it: `-` and `_` read as `+` and `/`,
/// padded to a multiple of four, then decoded as `atob` does (the WHATWG
/// "forgiving-base64 decode": ASCII whitespace ignored, up to two trailing
/// `=`, leftover bits discarded). None where `atob` throws.
pub(crate) fn b64url_decode(input: &str) -> Option<Vec<u8>> {
    let mut text: String = input
        .chars()
        .map(|c| match c {
            '-' => '+',
            '_' => '/',
            other => other,
        })
        .collect();
    let length = utf16_len(&text);
    let pad = if length % 4 == 0 { 0 } else { 4 - length % 4 };
    text.extend(std::iter::repeat_n('=', pad));
    forgiving_base64_decode(&text)
}

/// CANONICAL base64url, as the server's verifier takes it for a token's parts:
/// the URL-safe alphabet only, no padding, and nothing the encoder would not
/// have written (encoding the result gives the input back, so no stray bits in
/// the last character). None for anything else: one signature must have one
/// spelling, and every lease the server signs is canonical.
pub(crate) fn b64url_decode_canonical(input: &str) -> Option<Vec<u8>> {
    let alphabet = input
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
    if !alphabet || input.len() % 4 == 1 {
        return None;
    }
    let bytes = b64url_decode(input)?;
    (b64url_encode(&bytes) == input).then_some(bytes)
}

fn forgiving_base64_decode(input: &str) -> Option<Vec<u8>> {
    let mut data: Vec<char> = input
        .chars()
        .filter(|c| !matches!(c, '\t' | '\n' | '\x0c' | '\r' | ' '))
        .collect();
    if data.len() % 4 == 0 {
        if data.ends_with(&['=', '=']) {
            data.truncate(data.len() - 2);
        } else if data.ends_with(&['=']) {
            data.truncate(data.len() - 1);
        }
    }
    if data.len() % 4 == 1 {
        return None;
    }
    let mut out = Vec::with_capacity(data.len() * 3 / 4);
    let mut buffer: u32 = 0;
    let mut bits = 0;
    for c in data {
        let sextet = match c {
            'A'..='Z' => c as u32 - 'A' as u32,
            'a'..='z' => c as u32 - 'a' as u32 + 26,
            '0'..='9' => c as u32 - '0' as u32 + 52,
            '+' => 62,
            '/' => 63,
            _ => return None,
        };
        buffer = (buffer << 6) | sextet;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(((buffer >> bits) & 0xff) as u8);
        }
    }
    Some(out)
}

/// Base64url without padding.
pub(crate) fn b64url_encode(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        let chars = chunk.len() + 1;
        for i in 0..chars {
            out.push(char::from(ALPHABET[((n >> (18 - 6 * i)) & 0x3f) as usize]));
        }
    }
    out
}
