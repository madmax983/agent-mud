//! A scripted model server for real-`lg serve` tests.
//!
//! It speaks enough HTTP/1.1 for an OpenAI-compatible client: `GET
//! /v1/models` and `POST /v1/chat/completions`, answered as plain JSON, or
//! as server-sent events ending in `data: [DONE]` when the request asks for
//! `"stream": true` (lg's `openai` provider streams). A script picks each
//! reply from the request, and every request is recorded.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex, PoisonError};

use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// What the model answers.
#[derive(Debug, Clone)]
pub enum Reply {
    /// A plain text answer.
    Text(String),
    /// Tool calls, by name and arguments.
    Calls(Vec<(String, Value)>),
}

/// Picks the reply to one chat request.
pub type Script = Arc<dyn Fn(&Value) -> Reply + Send + Sync>;

/// A running stub.
pub struct ModelStub {
    /// Where it listens.
    pub addr: SocketAddr,
    requests: Arc<Mutex<Vec<Value>>>,
    task: tokio::task::JoinHandle<()>,
}

impl ModelStub {
    /// Starts a stub on a loopback port that answers with `script`.
    ///
    /// # Errors
    /// The port cannot be bound.
    pub async fn start(script: Script) -> std::io::Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        let requests = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&requests);
        let task = tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let (seen, script) = (Arc::clone(&seen), Arc::clone(&script));
                tokio::spawn(async move {
                    let _ = serve(stream, &seen, &script).await;
                });
            }
        });
        Ok(Self {
            addr,
            requests,
            task,
        })
    }

    /// The base URL to give a client, such as `http://127.0.0.1:4000/v1`.
    #[must_use]
    pub fn base_url(&self) -> String {
        format!("http://{}/v1", self.addr)
    }

    /// Every chat request so far, oldest first.
    #[must_use]
    pub fn requests(&self) -> Vec<Value> {
        self.requests
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

impl Drop for ModelStub {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// The text of the last user message of a request.
#[must_use]
pub fn last_user_text(req: &Value) -> String {
    req["messages"]
        .as_array()
        .into_iter()
        .flatten()
        .rev()
        .find(|m| m["role"] == "user")
        .map(content_text)
        .unwrap_or_default()
}

/// The text of one message, whether its content is a string or parts.
#[must_use]
pub fn content_text(m: &Value) -> String {
    match &m["content"] {
        Value::String(s) => s.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|p| p["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// Whether the request is a heartbeat tick: lg marks a tick's prompt.
#[must_use]
pub fn is_tick(req: &Value) -> bool {
    last_user_text(req).contains("[littleguy heartbeat]")
}

/// Whether the last message is a tool result, so the model is to go on.
#[must_use]
pub fn after_tools(req: &Value) -> bool {
    req["messages"]
        .as_array()
        .and_then(|m| m.last())
        .is_some_and(|m| m["role"] == "tool")
}

/// The tool results of a request, by the name of the call they answer.
#[must_use]
pub fn tool_results(req: &Value) -> Vec<(String, String)> {
    let messages = req["messages"].as_array().cloned().unwrap_or_default();
    let name_of = |id: &str| {
        messages
            .iter()
            .flat_map(|m| m["tool_calls"].as_array().cloned().unwrap_or_default())
            .rfind(|c| c["id"] == id)
            .and_then(|c| c["function"]["name"].as_str().map(str::to_owned))
            .unwrap_or_default()
    };
    messages
        .iter()
        .filter(|m| m["role"] == "tool")
        .map(|m| {
            (
                name_of(m["tool_call_id"].as_str().unwrap_or_default()),
                content_text(m),
            )
        })
        .collect()
}

/// The names of the tools a request offers.
#[must_use]
pub fn offered_tools(req: &Value) -> Vec<String> {
    req["tools"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|t| t["function"]["name"].as_str().map(str::to_owned))
        .collect()
}

async fn serve(
    mut stream: TcpStream,
    seen: &Mutex<Vec<Value>>,
    script: &Script,
) -> std::io::Result<()> {
    let mut buf = Vec::new();
    let mut chunk = [0_u8; 8192];
    let head_end = loop {
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            return Ok(());
        }
        buf.extend_from_slice(&chunk[..n]);
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break i + 4;
        }
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
    let length = head
        .lines()
        .find_map(|l| {
            let (k, v) = l.split_once(':')?;
            k.eq_ignore_ascii_case("content-length")
                .then(|| v.trim().parse::<usize>().ok())?
        })
        .unwrap_or(0);
    while buf.len() < head_end + length {
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
    }
    let first = head.lines().next().unwrap_or_default();
    if first.starts_with("GET ") && first.contains("/models") {
        let body = json!({ "object": "list", "data": [{ "id": "stub", "object": "model" }] });
        return respond(&mut stream, "application/json", &body.to_string()).await;
    }
    if !(first.starts_with("POST ") && first.contains("/chat/completions")) {
        return stream
            .write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
            .await;
    }
    let req: Value = serde_json::from_slice(&buf[head_end..]).unwrap_or(Value::Null);
    seen.lock()
        .unwrap_or_else(PoisonError::into_inner)
        .push(req.clone());
    let reply = script(&req);
    if req["stream"] == true {
        respond(&mut stream, "text/event-stream", &sse(&reply)).await
    } else {
        respond(&mut stream, "application/json", &plain(&reply).to_string()).await
    }
}

async fn respond(stream: &mut TcpStream, kind: &str, body: &str) -> std::io::Result<()> {
    let head = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: {kind}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes()).await?;
    stream.write_all(body.as_bytes()).await?;
    stream.shutdown().await
}

/// The usage every answer reports.
fn usage() -> Value {
    json!({ "prompt_tokens": 100, "completion_tokens": 10 })
}

fn calls_json(calls: &[(String, Value)]) -> Vec<Value> {
    // Ids are unique across answers: a transcript holds many calls.
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    calls
        .iter()
        .enumerate()
        .map(|(i, (name, args))| {
            json!({
                "index": i,
                "id": format!("call_{n}_{i}"),
                "type": "function",
                "function": { "name": name, "arguments": args.to_string() },
            })
        })
        .collect()
}

fn plain(reply: &Reply) -> Value {
    let (message, finish) = match reply {
        Reply::Text(t) => (json!({ "role": "assistant", "content": t }), "stop"),
        Reply::Calls(c) => (
            json!({ "role": "assistant", "content": "", "tool_calls": calls_json(c) }),
            "tool_calls",
        ),
    };
    json!({
        "id": "stub",
        "object": "chat.completion",
        "model": "stub",
        "choices": [{ "index": 0, "message": message, "finish_reason": finish }],
        "usage": usage(),
    })
}

fn sse(reply: &Reply) -> String {
    let chunk = |delta: Value, finish: Value| {
        json!({
            "id": "stub",
            "object": "chat.completion.chunk",
            "model": "stub",
            "choices": [{ "index": 0, "delta": delta, "finish_reason": finish }],
        })
    };
    let mut events = Vec::new();
    match reply {
        Reply::Text(t) => {
            events.push(chunk(
                json!({ "role": "assistant", "content": t }),
                Value::Null,
            ));
            events.push(chunk(json!({}), json!("stop")));
        }
        Reply::Calls(c) => {
            events.push(chunk(
                json!({ "role": "assistant", "tool_calls": calls_json(c) }),
                Value::Null,
            ));
            events.push(chunk(json!({}), json!("tool_calls")));
        }
    }
    events.push(
        json!({ "id": "stub", "object": "chat.completion.chunk", "choices": [], "usage": usage() }),
    );
    let mut out = String::new();
    for e in &events {
        out.push_str("data: ");
        out.push_str(&e.to_string());
        out.push_str("\n\n");
    }
    out.push_str("data: [DONE]\n\n");
    out
}
