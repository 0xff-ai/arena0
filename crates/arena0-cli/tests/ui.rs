//! End-to-end checks of `arena0 ui` through the real binary: the loopback
//! HTTP surface, the WebSocket handshake, and replication of a launched
//! session from a real daemon.
#![cfg(unix)]

use std::collections::HashMap;
use std::io::{BufRead as _, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use futures::{SinkExt as _, StreamExt as _};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;
use tokio_tungstenite::tungstenite::{Error as WsError, Message};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

const DEADLINE: Duration = Duration::from_secs(90);
const COLLECTIONS: [&str; 9] = [
    "hosts",
    "programs",
    "executions",
    "steps",
    "callouts",
    "receipts",
    "offers",
    "blobs",
    "activity",
];

fn arena0d_binary() -> PathBuf {
    let target = Path::new(env!("CARGO_BIN_EXE_arena0"))
        .parent()
        .expect("arena0 test binary has a directory");
    let target = if target.file_name().is_some_and(|name| name == "deps") {
        target.parent().expect("deps has a parent")
    } else {
        target
    };
    let daemon = target.join("arena0d");
    assert!(
        daemon.is_file(),
        "arena0d is required at {}; run `cargo build -p arena0d` first",
        daemon.display()
    );
    daemon
}

fn arena0(home: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_arena0"));
    command
        .env("ARENA0_HOME", home)
        .env_remove("ARENA0_SOCKET")
        .env_remove("ARENA0_CONTEXT")
        .env_remove("CODEX_THREAD_ID")
        .env_remove("RUST_LOG");
    command
}

/// A running `arena0 ui` process. Dropping it interrupts the process so it
/// stops the daemon it started.
struct Ui {
    child: Child,
    port: u16,
    token: String,
    _home: tempfile::TempDir,
}

impl Ui {
    fn start() -> Self {
        let _ = arena0d_binary();
        let home = tempfile::tempdir().expect("temporary arena0 home");
        let mut child = arena0(home.path())
            .args(["ui", "--no-open", "--json", "--port", "0"])
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("spawn arena0 ui");
        let mut line = String::new();
        BufReader::new(child.stdout.as_mut().expect("piped stdout"))
            .read_line(&mut line)
            .expect("read the URL line");
        let url = serde_json::from_str::<Value>(&line)
            .unwrap_or_else(|error| panic!("URL line {line:?}: {error}"))["url"]
            .as_str()
            .expect("url field")
            .to_owned();
        let rest = url.strip_prefix("http://127.0.0.1:").expect("loopback URL");
        let (port, token) = rest.split_once("/#token=").expect("token fragment");
        Self {
            port: port.parse().expect("port"),
            token: token.to_owned(),
            child,
            _home: home,
        }
    }
}

impl Drop for Ui {
    fn drop(&mut self) {
        let pid = libc::pid_t::try_from(self.child.id()).expect("pid");
        // SAFETY: the unreaped child owns this pid.
        unsafe { libc::kill(pid, libc::SIGINT) };
        let _ = self.child.wait();
    }
}

type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;

async fn upgrade(
    ui: &Ui,
    origin: Option<&str>,
    host: Option<&str>,
    token: &str,
) -> Result<(Ws, Option<String>), WsError> {
    let mut request = format!("ws://127.0.0.1:{}/ws", ui.port)
        .into_client_request()
        .expect("request");
    let headers = request.headers_mut();
    let default_origin = format!("http://127.0.0.1:{}", ui.port);
    headers.insert("Origin", origin.unwrap_or(&default_origin).parse().unwrap());
    if let Some(host) = host {
        headers.insert("Host", host.parse().unwrap());
    }
    headers.insert(
        "Sec-WebSocket-Protocol",
        format!("arena0.v1, arena0.token.{token}").parse().unwrap(),
    );
    let stream = TcpStream::connect(("127.0.0.1", ui.port))
        .await
        .expect("connect");
    let (ws, response) =
        tokio_tungstenite::client_async(request, MaybeTlsStream::Plain(stream)).await?;
    let protocol = response
        .headers()
        .get("sec-websocket-protocol")
        .map(|value| value.to_str().unwrap().to_owned());
    Ok((ws, protocol))
}

fn status_of(result: Result<(Ws, Option<String>), WsError>) -> u16 {
    match result {
        Err(WsError::Http(response)) => response.status().as_u16(),
        Ok(_) => 101,
        Err(other) => panic!("unexpected handshake error: {other}"),
    }
}

async fn next_frame(ws: &mut Ws) -> Value {
    loop {
        let message = tokio::time::timeout(DEADLINE, ws.next())
            .await
            .expect("frame within the deadline")
            .expect("socket open")
            .expect("frame");
        if let Message::Text(text) = message {
            return serde_json::from_str(text.as_str()).expect("JSON frame");
        }
    }
}

/// Send a call and return its reply plus the frames that arrived first.
async fn call(ws: &mut Ws, id: u32, op: Value, seen: &mut Vec<Value>) -> Value {
    let frame = json!({"t": "call", "id": id, "op": op});
    ws.send(Message::text(frame.to_string()))
        .await
        .expect("send");
    loop {
        let frame = next_frame(ws).await;
        if frame["t"] == "reply" && frame["id"] == id {
            return frame["result"].clone();
        }
        seen.push(frame);
    }
}

async fn http_get(port: u16, path: &str) -> (String, HashMap<String, String>, Vec<u8>) {
    let mut stream = TcpStream::connect(("127.0.0.1", port))
        .await
        .expect("connect");
    let request =
        format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n");
    stream.write_all(request.as_bytes()).await.expect("write");
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).await.expect("read");
    let split = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .expect("header end");
    let head = String::from_utf8(raw[..split].to_vec()).expect("utf-8 head");
    let mut lines = head.lines();
    let status = lines.next().expect("status line").to_owned();
    let headers = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.to_ascii_lowercase(), value.trim().to_owned()))
        .collect();
    (status, headers, raw[split + 4..].to_vec())
}

#[tokio::test(flavor = "multi_thread")]
async fn ui_rejects_foreign_origins_hosts_and_tokens() {
    let ui = Ui::start();
    let local = format!("127.0.0.1:{}", ui.port);
    assert_eq!(status_of(upgrade(&ui, None, None, "wrong").await), 403);
    assert_eq!(
        status_of(upgrade(&ui, Some("http://evil.example"), None, &ui.token).await),
        403
    );
    assert_eq!(
        status_of(upgrade(&ui, None, Some("evil.example:80"), &ui.token).await),
        403
    );
    let (_, protocol) = upgrade(&ui, None, None, &ui.token)
        .await
        .expect("right token");
    assert_eq!(protocol.as_deref(), Some("arena0.v1"));
    let localhost = format!("localhost:{}", ui.port);
    let (_, protocol) = upgrade(
        &ui,
        Some(&format!("http://{localhost}")),
        Some(&localhost),
        &ui.token,
    )
    .await
    .expect("localhost is a loopback name");
    assert_eq!(protocol.as_deref(), Some("arena0.v1"));
    drop(local);
}

#[tokio::test(flavor = "multi_thread")]
async fn ui_serves_the_app_with_strict_headers() {
    let ui = Ui::start();
    let (_, _, index) = http_get(ui.port, "/").await;
    for path in ["/", "/s/anything"] {
        let (status, headers, body) = http_get(ui.port, path).await;
        assert!(status.contains("200"), "{path}: {status}");
        assert_eq!(body, index, "{path} serves index.html");
        assert!(headers["content-type"].starts_with("text/html"));
        assert_eq!(headers["x-content-type-options"], "nosniff");
        assert_eq!(headers["referrer-policy"], "no-referrer");
        assert_eq!(headers["x-frame-options"], "DENY");
        assert!(headers["content-security-policy"].contains("default-src 'self'"));
        assert!(headers["content-security-policy"].contains("frame-ancestors 'none'"));
        assert!(
            !headers
                .keys()
                .any(|name| name.starts_with("access-control-"))
        );
    }
    let html = String::from_utf8(index).expect("utf-8 index");
    let src = html
        .split("src=\"")
        .nth(1)
        .and_then(|rest| rest.split('"').next())
        .expect("script tag");
    let (status, headers, body) = http_get(ui.port, src).await;
    assert!(status.contains("200"), "{src}: {status}");
    assert!(headers["content-type"].starts_with("text/javascript"));
    assert!(headers["cache-control"].contains("immutable"));
    assert!(!body.is_empty());
    let (status, _, _) = http_get(ui.port, "/assets/does-not-exist.js").await;
    assert!(status.contains("404"), "{status}");
}

/// Collect the handshake: hello, nine resets in the fixed order, ready.
async fn open(ui: &Ui) -> (Ws, Value, HashMap<String, HashMap<String, Value>>) {
    let (mut ws, _) = upgrade(ui, None, None, &ui.token).await.expect("upgrade");
    let hello = next_frame(&mut ws).await;
    assert_eq!(hello["t"], "hello");
    let mut tables: HashMap<String, HashMap<String, Value>> = HashMap::new();
    for expected in COLLECTIONS {
        let frame = next_frame(&mut ws).await;
        assert_eq!(frame["t"], "rows");
        assert_eq!(frame["reset"], true);
        assert_eq!(frame["batch"]["collection"], expected);
        let table = tables.entry(expected.to_owned()).or_default();
        apply(table, &frame["batch"]["ops"]);
    }
    assert_eq!(next_frame(&mut ws).await["t"], "ready");
    (ws, hello, tables)
}

fn apply(table: &mut HashMap<String, Value>, ops: &Value) {
    for op in ops.as_array().expect("ops") {
        let key = op["key"].as_str().expect("key").to_owned();
        if op["op"] == "upsert" {
            table.insert(key, op["row"].clone());
        } else {
            table.remove(&key);
        }
    }
}

fn first_choice(schema: &Value) -> Option<Value> {
    match schema {
        Value::Object(map) => {
            if let Some(Value::Array(values)) = map.get("enum") {
                return values.first().cloned();
            }
            if let Some(value) = map.get("const") {
                return Some(value.clone());
            }
            map.values().find_map(first_choice)
        }
        Value::Array(items) => items.iter().find_map(first_choice),
        _ => None,
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn ui_replicates_a_launched_session_and_answers_a_callout() {
    let ui = Ui::start();
    let (mut ws, hello, mut tables) = open(&ui).await;
    let strategy = hello["strategies"][0]["name"]
        .as_str()
        .expect("a strategy")
        .to_owned();
    let mut hosts: Vec<_> = tables["hosts"].keys().cloned().collect();
    hosts.sort();
    assert_eq!(hosts, ["host-01", "host-02"]);

    let mut seen = Vec::new();
    let launched = call(
        &mut ws,
        1,
        json!({"op": "launch", "args": {
            "program": "rock-paper-scissors",
            "params": null,
            "seats": [
                {"host": "host-01", "driver": {"kind": "builtin", "strategy": strategy}},
                {"host": "host-02", "driver": {"kind": "you"}},
            ],
        }}),
        &mut seen,
    )
    .await;
    assert!(launched.get("ok").is_some(), "launch failed: {launched}");
    assert_eq!(launched["ok"]["execs"].as_array().expect("execs").len(), 2);
    for frame in seen.drain(..) {
        absorb(&mut tables, &frame);
    }

    let mut first_pending: Option<(String, String)> = None;
    let mut answered = 0u32;
    let mut certified: Vec<(u64, u64)> = Vec::new();
    let mut next_id = 2;
    loop {
        let done = ["host-01", "host-02"].iter().all(|host| {
            tables["executions"]
                .values()
                .any(|row| row["host"] == *host && row["lifecycle"] == "completed")
        }) && ["host-01", "host-02"]
            .iter()
            .all(|host| tables["receipts"].values().any(|row| row["host"] == *host));
        if done {
            break;
        }
        let callout = tables["callouts"]
            .values()
            .find(|row| row["host"] == "host-02")
            .cloned();
        if let Some(callout) = callout {
            let exec_id = callout["exec_id"].as_str().unwrap().to_owned();
            let pending_id = callout["pending_id"].as_str().unwrap().to_owned();
            let answer = first_choice(&callout["schema"]).expect("an allowed answer");
            let reply = call(
                &mut ws,
                next_id,
                json!({"op": "answer", "args": {
                    "host": "host-02", "exec_id": exec_id,
                    "pending_id": pending_id, "answer": answer,
                }}),
                &mut seen,
            )
            .await;
            next_id += 1;
            for frame in seen.drain(..) {
                absorb(&mut tables, &frame);
            }
            if reply.get("ok").is_some() {
                answered += 1;
                first_pending.get_or_insert((exec_id, pending_id.clone()));
            }
            tables
                .get_mut("callouts")
                .unwrap()
                .retain(|_, row| row["pending_id"] != pending_id);
            continue;
        }
        let frame = next_frame(&mut ws).await;
        if frame["t"] == "rows" && frame["batch"]["collection"] == "steps" {
            for op in frame["batch"]["ops"].as_array().unwrap() {
                if op["op"] == "upsert" {
                    certified.push((
                        op["row"]["step"].as_u64().unwrap(),
                        op["row"]["certified_ms"].as_u64().unwrap(),
                    ));
                }
            }
        }
        absorb(&mut tables, &frame);
    }
    assert!(answered >= 1, "the UI seat answered at least one callout");
    assert!(!certified.is_empty(), "steps were replicated as upserts");
    assert!(
        certified.iter().all(|(_, ms)| *ms > 0),
        "steps carry certified times"
    );
    let max_step = certified.iter().map(|(step, _)| *step).max().unwrap();
    assert!(max_step >= 1, "steps increased past step 0");

    let (exec_id, pending_id) = first_pending.expect("an answered callout");
    let stale = call(
        &mut ws,
        next_id,
        json!({"op": "answer", "args": {
            "host": "host-02", "exec_id": exec_id,
            "pending_id": pending_id, "answer": "Rock",
        }}),
        &mut seen,
    )
    .await;
    assert_eq!(stale["err"]["code"], "callout_not_pending", "{stale}");
}

fn absorb(tables: &mut HashMap<String, HashMap<String, Value>>, frame: &Value) {
    if frame["t"] != "rows" {
        return;
    }
    let collection = frame["batch"]["collection"].as_str().unwrap();
    let table = tables.entry(collection.to_owned()).or_default();
    if frame["reset"] == true {
        table.clear();
    }
    apply(table, &frame["batch"]["ops"]);
}

#[test]
fn ui_attach_fails_without_a_daemon() {
    let home = tempfile::tempdir().expect("temporary arena0 home");
    let output = arena0(home.path())
        .args(["ui", "--attach", "--no-open"])
        .output()
        .expect("run arena0 ui --attach");
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("Daemon not reachable"), "stderr: {stderr}");
}
