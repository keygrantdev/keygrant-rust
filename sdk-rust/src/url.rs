//! The one URL edit the SDK makes: a payment link with its one
//! `client_reference_id`, which `purchase_url` and `upgrade_url` both build
//! their links with.

/// The query parameter a payment link names its checkout's buyer by.
const REFERENCE: &str = "client_reference_id";

/// `link` with `reference` as its one `client_reference_id`, or none when
/// `link` is not an absolute `http`/`https` URL with a host.
///
/// The link is read as an absolute URL, which writes it in its normal form
/// (a lower-case scheme and host, no default port, `/` for an empty path),
/// and its query is then edited as text: every parameter named exactly
/// `client_reference_id` (its text before the first `=`, or all of it) and
/// every empty one is left out, the others are kept as written and in their
/// order, and `client_reference_id=<reference>` (percent-encoded as
/// `encodeURIComponent` encodes it) is added last. The fragment is kept.
pub fn referenced_link(link: &str, reference: &str) -> Option<String> {
    let url = AbsoluteUrl::parse(link)?;
    let query = url
        .query
        .map(|query| encoded(query, in_query_set))
        .unwrap_or_default();
    let mut parts: Vec<&str> = query
        .split('&')
        .filter(|part| !part.is_empty() && part.split('=').next() != Some(REFERENCE))
        .collect();
    let added = format!("{REFERENCE}={}", encode_uri_component(reference));
    parts.push(&added);
    let fragment = url
        .fragment
        .map(|fragment| format!("#{}", encoded(fragment, in_fragment_set)))
        .unwrap_or_default();
    Some(format!(
        "{}://{}{}{}?{}{fragment}",
        url.scheme,
        url.userinfo,
        url.host,
        url.path,
        parts.join("&")
    ))
}

/// Whether `link` is an absolute `http`/`https` URL with a host, as
/// [`referenced_link`] takes it.
pub(crate) fn is_absolute_link(link: &str) -> bool {
    AbsoluteUrl::parse(link).is_some()
}

/// An absolute `http`/`https` URL, split into its parts.
struct AbsoluteUrl<'a> {
    /// Lower case.
    scheme: String,
    /// With its `@`, or empty.
    userinfo: &'a str,
    /// Lower case, the scheme's default port dropped.
    host: String,
    /// `/` when the link has none.
    path: &'a str,
    /// Without its `?`; none when the link has none.
    query: Option<&'a str>,
    /// Without its `#`; none when the link has none.
    fragment: Option<&'a str>,
}

impl<'a> AbsoluteUrl<'a> {
    fn parse(link: &'a str) -> Option<AbsoluteUrl<'a>> {
        // The URL parser drops leading and trailing C0 controls and spaces.
        let link = link.trim_matches(|c: char| c <= ' ');
        let (scheme, rest) = link.split_once("://")?;
        let scheme = scheme.to_ascii_lowercase();
        if scheme != "http" && scheme != "https" {
            return None;
        }
        let (rest, fragment) = match rest.split_once('#') {
            Some((rest, fragment)) => (rest, Some(fragment)),
            None => (rest, None),
        };
        let (rest, query) = match rest.split_once('?') {
            Some((rest, query)) => (rest, Some(query)),
            None => (rest, None),
        };
        let (authority, path) = match rest.find('/') {
            Some(at) => (&rest[..at], &rest[at..]),
            None => (rest, "/"),
        };
        let (userinfo, host) = match authority.rfind('@') {
            Some(at) => (&authority[..=at], &authority[at + 1..]),
            None => ("", authority),
        };
        let host = host.to_ascii_lowercase();
        let default_port = if scheme == "https" { ":443" } else { ":80" };
        let host = host.strip_suffix(default_port).unwrap_or(&host).to_owned();
        if host.is_empty() || host.starts_with(':') {
            return None;
        }
        Some(AbsoluteUrl {
            scheme,
            userinfo,
            host,
            path,
            query,
            fragment,
        })
    }
}

/// The URL parser's C0 control percent-encode set: the C0 controls and every
/// code point above `~`.
fn in_c0_control_set(byte: u8) -> bool {
    !(0x20..=0x7e).contains(&byte)
}

/// The special-query percent-encode set (an `http(s)` URL's query).
fn in_query_set(byte: u8) -> bool {
    in_c0_control_set(byte) || matches!(byte, b' ' | b'"' | b'#' | b'<' | b'>' | b'\'')
}

/// The fragment percent-encode set.
fn in_fragment_set(byte: u8) -> bool {
    in_c0_control_set(byte) || matches!(byte, b' ' | b'"' | b'<' | b'>' | b'`')
}

/// `text` as the URL parser writes it: each byte of `set` percent-encoded,
/// the rest (an escape already there included) as it is.
fn encoded(text: &str, set: fn(u8) -> bool) -> String {
    let mut out = String::with_capacity(text.len());
    for byte in text.bytes() {
        if set(byte) {
            out.push_str(&format!("%{byte:02X}"));
        } else {
            out.push(char::from(byte));
        }
    }
    out
}

/// `text` as JavaScript's `encodeURIComponent` writes it.
fn encode_uri_component(text: &str) -> String {
    encoded(text, |byte| {
        !(byte.is_ascii_alphanumeric()
            || matches!(
                byte,
                b'-' | b'_' | b'.' | b'!' | b'~' | b'*' | b'\'' | b'(' | b')'
            ))
    })
}

#[cfg(test)]
mod tests {
    use super::{is_absolute_link, referenced_link};

    #[test]
    fn sets_the_reference_and_keeps_the_rest_as_written() {
        assert_eq!(
            referenced_link("https://buy.stripe.com/test_abc", "act_device1").as_deref(),
            Some("https://buy.stripe.com/test_abc?client_reference_id=act_device1")
        );
        assert_eq!(
            referenced_link(
                "https://buy.stripe.com/x?prefilled_email=jo%40example.com&client_reference_id=old&client_reference_id=older",
                "act_1"
            )
            .as_deref(),
            Some("https://buy.stripe.com/x?prefilled_email=jo%40example.com&client_reference_id=act_1")
        );
        // The link in its normal form, its query as written, the reference
        // encoded as a URI component.
        assert_eq!(
            referenced_link("HTTPS://Buy.Stripe.COM:443?a=b+c#top", "x y").as_deref(),
            Some("https://buy.stripe.com/?a=b+c&client_reference_id=x%20y#top")
        );
    }

    #[test]
    fn writes_the_query_and_fragment_as_the_url_parser_does() {
        assert_eq!(
            referenced_link("https://b.example/x?q=a b'c&n=\u{e9}#to p", "r").as_deref(),
            Some("https://b.example/x?q=a%20b%27c&n=%C3%A9&client_reference_id=r#to%20p")
        );
    }

    #[test]
    fn refuses_what_is_not_an_absolute_web_url() {
        for link in [
            "",
            "/buy",
            "buy.stripe.com/x",
            "mailto:jo@example.com",
            "https://",
            "https:///path",
            "ftp://x/y",
        ] {
            assert_eq!(referenced_link(link, "a"), None, "{link}");
            assert!(!is_absolute_link(link), "{link}");
        }
        assert!(is_absolute_link("https://buy.stripe.com/x"));
    }
}
