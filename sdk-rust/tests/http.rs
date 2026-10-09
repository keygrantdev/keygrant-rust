//! The default HTTP adapter against a real socket: every status is an answer,
//! a body that is not JSON is no JSON, and a server that never answers is no
//! answer at the bound.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use keygrant::{HttpAdapter, UreqHttp};
use serde_json::{json, Value};

/// A request as the server saw it.
#[derive(Debug)]
struct Seen {
    request_line: String,
    content_type: Option<String>,
    body: String,
}

/// What the server does with the one request it takes.
enum Reply {
    /// Answer with this status line, content type and body.
    With(&'static str, &'static str, String),
    /// Take the request and say nothing for this long.
    Silence(Duration),
}

/// A server on 127.0.0.1 for one request: its base URL, and what it saw.
fn serve(reply: Reply) -> (String, mpsc::Receiver<Seen>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("a local port");
    let base = format!("http://{}", listener.local_addr().expect("an address"));
    let (sender, seen) = mpsc::channel();
    thread::spawn(move || {
        let Ok((stream, _)) = listener.accept() else {
            return;
        };
        let request = read_request(&stream);
        let _ = sender.send(request);
        handle(stream, reply);
    });
    (base, seen)
}

fn read_request(stream: &TcpStream) -> Seen {
    let mut reader = BufReader::new(stream);
    let mut request_line = String::new();
    let _ = reader.read_line(&mut request_line);
    let mut length = 0;
    let mut content_type = None;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
            break;
        }
        let (name, value) = line.split_once(':').unwrap_or((&line, ""));
        let value = value.trim().to_owned();
        match name.to_ascii_lowercase().as_str() {
            "content-length" => length = value.parse().unwrap_or(0),
            "content-type" => content_type = Some(value),
            _ => {}
        }
    }
    let mut body = vec![0; length];
    let _ = reader.read_exact(&mut body);
    Seen {
        request_line: request_line.trim().to_owned(),
        content_type,
        body: String::from_utf8_lossy(&body).into_owned(),
    }
}

fn handle(mut stream: TcpStream, reply: Reply) {
    match reply {
        Reply::With(status, content_type, body) => {
            let response = format!(
                "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes());
            let _ = stream.flush();
        }
        Reply::Silence(long) => thread::sleep(long),
    }
}

fn json_reply(status: &'static str, body: Value) -> Reply {
    Reply::With(status, "application/json", body.to_string())
}

/// The adapter with no proxy, so a proxy in this machine's environment
/// cannot stand between it and the local server.
fn direct() -> UreqHttp {
    UreqHttp::with_agent(ureq::Agent::config_builder().proxy(None).build().into())
}

/// A bound no request to a local server that answers reaches, however loaded the machine.
const BOUND: Duration = Duration::from_secs(30);

#[test]
fn posts_json_and_reads_a_refusal_as_an_answer() {
    let (base, seen) = serve(json_reply("403 Forbidden", json!({ "error": "revoked" })));
    let body = json!({ "key": "SLUICE-AAAA-BBBB-CCCC-DDDD", "major": 3 });
    let url = format!("{base}/v1/products/sluice/validate");
    let response = direct().post(&url, &body, BOUND).expect("an answer");
    assert_eq!(response.status, 403);
    assert_eq!(response.json, Some(json!({ "error": "revoked" })));
    let request = seen.recv_timeout(BOUND).expect("the request");
    assert_eq!(
        request.request_line,
        "POST /v1/products/sluice/validate HTTP/1.1"
    );
    assert_eq!(request.content_type.as_deref(), Some("application/json"));
    assert_eq!(
        serde_json::from_str::<Value>(&request.body).ok(),
        Some(body)
    );
}

#[test]
fn reads_an_outage_as_an_answer_too() {
    let (base, _seen) = serve(json_reply(
        "503 Service Unavailable",
        json!({ "error": "busy" }),
    ));
    let response = direct()
        .post(&format!("{base}/x"), &json!({}), BOUND)
        .expect("an answer");
    assert_eq!(response.status, 503);
    assert_eq!(response.json, Some(json!({ "error": "busy" })));
}

#[test]
fn reads_a_body_that_is_not_json_as_no_json() {
    let page = "<html>a captive portal</html>".to_owned();
    let (base, _seen) = serve(Reply::With("200 OK", "text/html", page));
    let response = direct()
        .post(&format!("{base}/x"), &json!({}), BOUND)
        .expect("an answer");
    assert_eq!((response.status, response.json), (200, None));
}

#[test]
fn gives_up_on_a_server_that_never_answers_at_the_bound() {
    // Silent for a minute: given up on at the bound, not when it would answer.
    let (base, _seen) = serve(Reply::Silence(Duration::from_secs(60)));
    let started = Instant::now();
    let answer = direct().post(&format!("{base}/x"), &json!({}), Duration::from_millis(400));
    let took = started.elapsed();
    assert!(answer.is_err(), "{answer:?}");
    assert!(took >= Duration::from_millis(390), "{took:?}");
    assert!(took < Duration::from_secs(20), "{took:?}");
}

#[test]
fn reads_no_server_at_all_as_no_answer() {
    // A port that was just free: nothing listens on it.
    let port = TcpListener::bind("127.0.0.1:0")
        .and_then(|listener| listener.local_addr())
        .expect("a free port")
        .port();
    let answer = direct().post(&format!("http://127.0.0.1:{port}/x"), &json!({}), BOUND);
    assert!(answer.is_err(), "{answer:?}");
}

#[test]
fn reads_a_status_as_an_answer_even_over_an_agent_that_would_not() {
    // ureq's own default treats a 4xx as an error: an app's agent made with
    // it must still give the SDK the refusal, or `invalid-key` reads as
    // `network` and a refusal never lands.
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .http_status_as_error(true)
        .proxy(None)
        .build()
        .into();
    let (base, _seen) = serve(json_reply(
        "404 Not Found",
        json!({ "error": "invalid-key" }),
    ));
    let response = UreqHttp::with_agent(agent)
        .post(
            &format!("{base}/v1/products/sluice/activate"),
            &json!({}),
            BOUND,
        )
        .expect("an answer");
    assert_eq!(response.status, 404);
    assert_eq!(response.json, Some(json!({ "error": "invalid-key" })));
}
