//! The default HTTP adapter: blocking `ureq` over rustls, every call bounded.

use std::time::Duration;

use serde_json::Value;
use ureq::{Proxy, ProxyProtocol};

use crate::adapters::{AdapterError, HttpAdapter, HttpResponse};

/// How long a request may take before it is abandoned, unless the call or
/// the configuration says otherwise.
pub const DEFAULT_HTTP_TIMEOUT: Duration = Duration::from_millis(15_000);

/// HTTP over `ureq` (rustls, the Mozilla root store), every call bounded by
/// its timeout: without a bound, a network that accepts a connection and
/// never answers (a captive portal, a dead proxy) would hold a check-in for
/// minutes. Abandoned, the call fails, which the SDK treats as no network.
/// A 4xx or 5xx is an answer like any, never an error.
#[derive(Debug, Clone)]
pub struct UreqHttp {
    agent: ureq::Agent,
}

impl Default for UreqHttp {
    fn default() -> Self {
        UreqHttp::new()
    }
}

impl UreqHttp {
    /// The adapter, each call bounded by the timeout the SDK gives it, through
    /// the HTTP proxy the environment names: the first of `ALL_PROXY`,
    /// `HTTPS_PROXY` and `HTTP_PROXY` (each upper case, then lower) that names
    /// an `http` or `https` proxy, less the hosts `NO_PROXY` exempts. A SOCKS
    /// proxy is passed over, as this adapter speaks no SOCKS; with no HTTP
    /// proxy named, requests go direct.
    pub fn new() -> Self {
        UreqHttp::with_agent(agent(env_proxy(|name| std::env::var(name).ok())))
    }

    /// The adapter over an agent of the app's own (a proxy, another TLS
    /// set-up). Whatever the agent's own setting, every request this adapter
    /// makes reads an HTTP status as an answer, never as an error (ureq's
    /// default is the opposite, which would make every refusal no network).
    pub fn with_agent(agent: ureq::Agent) -> Self {
        UreqHttp { agent }
    }
}

/// The variables a proxy is named in, in the order ureq reads them.
const PROXY_VARIABLES: [&str; 6] = [
    "ALL_PROXY",
    "all_proxy",
    "HTTPS_PROXY",
    "https_proxy",
    "HTTP_PROXY",
    "http_proxy",
];

/// The proxy the environment names for the default adapter, `env` reading a
/// variable: ureq's own reading (`Proxy::try_from_env`), less its SOCKS. The
/// first variable of [`PROXY_VARIABLES`] that names an `http` or `https` proxy
/// ureq can read, with the hosts `NO_PROXY` (else `no_proxy`) lists, comma
/// separated, going direct.
///
/// A SOCKS proxy is passed over for the next variable. This build of ureq
/// has no SOCKS support, and taken, one fails every request at once
/// (`socks5h`, `socks4a`), so that no activation or renewal ever lands, or
/// sends it direct past an HTTP proxy also named (`socks5`, `socks`,
/// `socks4`). Direct, as the Electron SDK always goes (Node's `fetch` reads
/// no proxy at all), is the better answer to a SOCKS-only environment.
fn env_proxy(env: impl Fn(&str) -> Option<String>) -> Option<Proxy> {
    let no_proxy = ["NO_PROXY", "no_proxy"].into_iter().find_map(&env);
    PROXY_VARIABLES
        .into_iter()
        .filter_map(&env)
        .find_map(|value| {
            let named = Proxy::new(&value).ok()?;
            let protocol = named.protocol();
            if !matches!(protocol, ProxyProtocol::Http | ProxyProtocol::Https) {
                return None;
            }
            let mut proxy = Proxy::builder(protocol)
                .host(named.host())
                .port(named.port());
            if let Some(username) = named.username() {
                proxy = proxy.username(username);
            }
            if let Some(password) = named.password() {
                proxy = proxy.password(password);
            }
            let exempt = no_proxy.iter().flat_map(|list| list.split(','));
            for host in exempt.map(str::trim).filter(|host| !host.is_empty()) {
                proxy = proxy.no_proxy(host);
            }
            proxy.build().ok()
        })
}

/// The default adapter's agent, through `proxy`.
fn agent(proxy: Option<Proxy>) -> ureq::Agent {
    ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_global(Some(DEFAULT_HTTP_TIMEOUT))
        .proxy(proxy)
        .build()
        .into()
}

impl HttpAdapter for UreqHttp {
    fn post(
        &self,
        url: &str,
        body: &Value,
        timeout: Duration,
    ) -> Result<HttpResponse, AdapterError> {
        let text = serde_json::to_string(body)?;
        let mut response = self
            .agent
            .post(url)
            .config()
            .http_status_as_error(false)
            .timeout_global(Some(timeout))
            .build()
            .content_type("application/json")
            .send(text)?;
        let status = response.status().as_u16();
        let json = response
            .body_mut()
            .read_to_string()
            .ok()
            // As a lease's payload is read: a number out of a double's range fails nothing.
            .and_then(|text| crate::lease::parse_json(&text));
        Ok(HttpResponse { status, json })
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;
    use std::thread;

    use serde_json::json;
    use ureq::http::Uri;

    use super::*;

    /// The agent `new()` makes, less the environment's proxy: a 4xx is an
    /// answer, its body read.
    #[test]
    fn the_default_agent_reads_a_status_as_an_answer() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("a local port");
        let base = format!("http://{}", listener.local_addr().expect("an address"));
        let server = thread::spawn(move || {
            let (stream, _) = listener.accept().expect("a request");
            let mut reader = BufReader::new(&stream);
            let mut length = 0;
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                    break;
                }
                if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                    length = value.trim().parse().unwrap_or(0);
                }
            }
            let mut body = vec![0; length];
            let _ = reader.read_exact(&mut body);
            let answer = r#"{"error":"limit"}"#;
            let _ = write!(
                &stream,
                "HTTP/1.1 409 Conflict\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{answer}",
                answer.len()
            );
            String::from_utf8_lossy(&body).into_owned()
        });
        let response = UreqHttp::with_agent(agent(None))
            .post(
                &format!("{base}/v1/products/sluice/activate"),
                &json!({ "key": "SLUICE-AAAA" }),
                Duration::from_secs(5),
            )
            .expect("an answer");
        assert_eq!(
            (response.status, response.json),
            (409, Some(json!({ "error": "limit" })))
        );
        assert_eq!(
            server.join().expect("the server"),
            r#"{"key":"SLUICE-AAAA"}"#
        );
    }

    /// An environment of these variables only.
    fn env(vars: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let vars: HashMap<String, String> = vars
            .iter()
            .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
            .collect();
        move |name| vars.get(name).cloned()
    }

    /// Where a proxy goes: its protocol, host and port.
    fn to(proxy: &Proxy) -> (ProxyProtocol, &str, u16) {
        (proxy.protocol(), proxy.host(), proxy.port())
    }

    const SOCKS: [&str; 5] = [
        "socks5h://socks.test:1080",
        "socks4a://socks.test:1080",
        "socks5://socks.test:1080",
        "socks://socks.test:1080",
        "socks4://socks.test:1080",
    ];

    #[test]
    fn names_the_first_http_proxy_in_ureqs_order() {
        assert!(env_proxy(env(&[])).is_none());
        let all = [
            ("http_proxy", "http://f.test:6"),
            ("HTTP_PROXY", "http://e.test:5"),
            ("https_proxy", "http://d.test:4"),
            ("HTTPS_PROXY", "http://c.test:3"),
            ("all_proxy", "http://b.test:2"),
            ("ALL_PROXY", "http://a.test:1"),
        ];
        // Each variable in turn, the ones before it unset.
        for first in 0..all.len() {
            let proxy = env_proxy(env(&all[..all.len() - first])).expect("a proxy");
            let (name, value) = all[all.len() - first - 1];
            assert_eq!(
                proxy.uri().to_string().trim_end_matches('/'),
                value,
                "{name}"
            );
        }
        // Without a scheme, an HTTP proxy at its default port; an HTTPS one,
        // and credentials, kept as named.
        let proxy = env_proxy(env(&[("HTTPS_PROXY", "proxy.test")])).expect("a proxy");
        assert_eq!(to(&proxy), (ProxyProtocol::Http, "proxy.test", 80));
        let proxy =
            env_proxy(env(&[("HTTPS_PROXY", "https://u:p%40ss@[::1]:8443")])).expect("a proxy");
        assert_eq!(to(&proxy), (ProxyProtocol::Https, "[::1]", 8443));
        assert_eq!(
            (proxy.username(), proxy.password()),
            (Some("u"), Some("p%40ss"))
        );
    }

    #[test]
    fn passes_over_a_socks_proxy_for_the_next_http_one_or_for_none() {
        for socks in SOCKS {
            let proxy = env_proxy(env(&[
                ("ALL_PROXY", socks),
                ("HTTPS_PROXY", "http://proxy.test:3128"),
            ]))
            .expect(socks);
            assert_eq!(to(&proxy), (ProxyProtocol::Http, "proxy.test", 3128));
            let proxy = env_proxy(env(&[
                ("all_proxy", socks),
                ("HTTPS_PROXY", socks),
                ("http_proxy", "http://proxy.test:3128"),
            ]))
            .expect(socks);
            assert_eq!(to(&proxy), (ProxyProtocol::Http, "proxy.test", 3128));
            assert!(env_proxy(env(&[("ALL_PROXY", socks)])).is_none(), "{socks}");
        }
        // As is an address ureq cannot read.
        let proxy = env_proxy(env(&[
            ("ALL_PROXY", "ftp://proxy.test:21"),
            ("HTTPS_PROXY", "http://proxy.test:3128"),
        ]))
        .expect("a proxy");
        assert_eq!(to(&proxy), (ProxyProtocol::Http, "proxy.test", 3128));
    }

    #[test]
    fn sends_the_hosts_no_proxy_lists_direct() {
        let exempt = |vars: &[(&str, &str)], url: &str| {
            let proxy = env_proxy(env(vars)).expect("a proxy");
            proxy.is_no_proxy(&url.parse::<Uri>().expect("a URI"))
        };
        let named = ("HTTPS_PROXY", "http://proxy.test:3128");
        let vars = [named, ("NO_PROXY", "localhost, 127.0.0.1,.internal.test")];
        assert!(exempt(&vars, "http://127.0.0.1:8787/v1"));
        assert!(exempt(&vars, "http://localhost/v1"));
        assert!(exempt(&vars, "https://api.internal.test/v1"));
        assert!(!exempt(&vars, "https://api.keygrant.dev/v1"));
        // `NO_PROXY` before `no_proxy`, and none without either.
        let vars = [named, ("no_proxy", "localhost")];
        assert!(exempt(&vars, "http://localhost/v1"));
        let vars = [
            named,
            ("NO_PROXY", "api.keygrant.dev"),
            ("no_proxy", "localhost"),
        ];
        assert!(!exempt(&vars, "http://localhost/v1"));
        assert!(exempt(&vars, "https://api.keygrant.dev/v1"));
        assert!(!exempt(&[named], "http://localhost/v1"));
    }
}
