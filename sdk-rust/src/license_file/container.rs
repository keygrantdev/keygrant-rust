//! A licence file as it is saved and sent: UTF-8 text holding its token on a
//! line of its own, with `#` lines for a person who opens it. Only the token
//! is read; a token pasted alone is a licence file too.

/// The most a licence file may be, in bytes. A real one is well under 2 KB.
pub const MAX_LICENSE_FILE_BYTES: usize = 16_384;

/// The token a licence file holds, or none when `content` is no licence
/// file's container: more than [`MAX_LICENSE_FILE_BYTES`]; not UTF-8 (read
/// strictly); or not exactly one line once each line (split on LF) is trimmed
/// of spaces, tabs and carriage returns at both ends, and empty lines and
/// lines starting with `#` are left out. One leading byte-order mark is
/// dropped, and a lone CR breaks no line. Whether the token is a licence
/// file's at all is [`verify_license_file`](super::verify_license_file)'s to
/// say.
pub fn license_file_token(content: &[u8]) -> Option<String> {
    if content.len() > MAX_LICENSE_FILE_BYTES {
        return None;
    }
    let text = std::str::from_utf8(content).ok()?;
    let text = text.strip_prefix('\u{FEFF}').unwrap_or(text);
    let mut lines = text
        .split('\n')
        .map(|line| line.trim_matches([' ', '\t', '\r']))
        .filter(|line| !line.is_empty() && !line.starts_with('#'));
    let token = lines.next()?;
    lines.next().is_none().then(|| token.to_owned())
}
