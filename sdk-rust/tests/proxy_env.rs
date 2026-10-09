//! The default HTTP adapter (`UreqHttp::new()`) and the proxy the environment
//! names, against real sockets. One test, alone in its binary: it sets the
//! process's environment, which no other test may read meanwhile.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use keygrant::{HttpAdapter, HttpResponse, UreqHttp};
use serde_json::json;

const BOUND: Duration = Duration::from_secs(30);

/// Every variable the default adapter reads a proxy from.
const VARIABLES: [&str; 8] = [
    "ALL_PROXY",
    "all_proxy",
    "HTTPS_PROXY",
    "https_proxy",
    "HTTP_PROXY",
    "http_proxy",
    "NO_PROXY",
    "no_proxy",
];

/// A request's head (its first line, and its body's length), read from `reader`.
fn read_head(reader: &mut impl BufRead) -> (String, usize) {
    let mut first = String::new();
    let _ = reader.read_line(&mut first);
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
    (first.trim_end().to_owned(), length)
}

/// Read a request off `reader`, and answer it with a refusal.
fn refuse(reader: &mut BufReader<&TcpStream>, stream: &TcpStream) {
    let (_, length) = read_head(reader);
    let mut body = vec![0; length];
    let _ = reader.read_exact(&mut body);
    let answer = r#"{"error":"limit"}"#;
    let mut stream = stream;
    let _ = write!(
        stream,
        "HTTP/1.1 409 Conflict\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{answer}",
        answer.len()
    );
}

/// A server on 127.0.0.1 for one request, which it refuses: its base URL.
fn server() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("a local port");
    let base = format!("http://{}", listener.local_addr().expect("an address"));
    thread::spawn(move || {
        if let Ok((stream, _)) = listener.accept() {
            refuse(&mut BufReader::new(&stream), &stream);
        }
    });
    base
}

/// An HTTP proxy on 127.0.0.1 for one tunnel: it takes the CONNECT, opens
/// the tunnel, and refuses the request sent through it itself. Its address,
/// and the CONNECT line it was sent.
fn proxy() -> (String, mpsc::Receiver<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("a local port");
    let address = listener.local_addr().expect("an address").to_string();
    let (sender, seen) = mpsc::channel();
    thread::spawn(move || {
        let Ok((stream, _)) = listener.accept() else {
            return;
        };
        let mut reader = BufReader::new(&stream);
        let (connect, _) = read_head(&mut reader);
        let _ = sender.send(connect);
        let _ = (&stream).write_all(b"HTTP/1.1 200 Connection established\r\n\r\n");
        refuse(&mut reader, &stream);
    });
    (address, seen)
}

/// An address on 127.0.0.1 nothing listens on.
fn nothing() -> String {
    TcpListener::bind("127.0.0.1:0")
        .and_then(|listener| listener.local_addr())
        .expect("a free port")
        .to_string()
}

fn post(url: &str) -> HttpResponse {
    UreqHttp::new()
        .post(url, &json!({ "key": "SLUICE-AAAA" }), BOUND)
        .unwrap_or_else(|e| panic!("{url}: {e}"))
}

fn refused(response: &HttpResponse) -> bool {
    response.status == 409 && response.json == Some(json!({ "error": "limit" }))
}

fn set(name: &str, value: &str) {
    std::env::set_var(name, value);
}

/// A host that is nowhere: reached only through a proxy.
const ELSEWHERE: &str = "http://licence.test/v1/products/sluice/activate";

#[test]
fn the_default_adapter_goes_through_the_environments_http_proxy_and_past_a_socks_one() {
    for name in VARIABLES {
        std::env::remove_var(name);
    }
    // No proxy named: direct.
    assert!(refused(&post(&server())));

    // An HTTP proxy named, in lower case as often as not: every request
    // tunnels through it, the host named to the proxy, not looked up here.
    let (address, connects) = proxy();
    set("https_proxy", &format!("http://{address}"));
    assert!(refused(&post(ELSEWHERE)));
    assert_eq!(
        connects.recv_timeout(BOUND).expect("a CONNECT"),
        "CONNECT licence.test:80 HTTP/1.1"
    );
    std::env::remove_var("https_proxy");

    // A SOCKS proxy first in line is passed over for the HTTP one after it:
    // taken, `socks5h` and `socks4a` failed every request at once, and the
    // others went direct past the HTTP proxy.
    for scheme in ["socks5h", "socks4a", "socks5", "socks", "socks4"] {
        set("ALL_PROXY", &format!("{scheme}://{}", nothing()));
        let (address, connects) = proxy();
        set("HTTPS_PROXY", &format!("http://{address}"));
        assert!(refused(&post(ELSEWHERE)), "{scheme}");
        assert_eq!(
            connects.recv_timeout(BOUND).expect("a CONNECT"),
            "CONNECT licence.test:80 HTTP/1.1",
            "{scheme}"
        );
    }

    // Named alone, it is no proxy at all: direct.
    std::env::remove_var("HTTPS_PROXY");
    set("ALL_PROXY", &format!("socks5h://{}", nothing()));
    assert!(refused(&post(&server())));
    std::env::remove_var("ALL_PROXY");

    // A host NO_PROXY lists goes direct past a proxy that is not there.
    set("HTTPS_PROXY", &format!("http://{}", nothing()));
    set("NO_PROXY", "localhost, 127.0.0.1");
    assert!(refused(&post(&server())));
}
