//! A tokio WebSocket fake substrate node that replays a recorded JSON-RPC cassette. Every
//! request is answered by exact `(method, params)` lookup; anything unrecorded gets a
//! JSON-RPC error naming the method so the test fails loudly.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message;

type Responses = Arc<Mutex<HashMap<(String, String), Value>>>;

#[derive(Clone)]
pub struct Cassette {
    pub block_number: u64,
    pub block_hash: String,
    responses: Responses,
}

fn params_key(params: &Value) -> String {
    match params {
        Value::Null => "null".into(),
        Value::Array(items) if items.is_empty() => "null".into(),
        other => other.to_string(),
    }
}

impl Cassette {
    pub fn load(name: &str) -> Self {
        let path = format!("{}/tests/golden/{name}", env!("CARGO_MANIFEST_DIR"));
        let file: Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        let responses = file["entries"]
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| {
                let method = entry["method"].as_str().unwrap().to_owned();
                (
                    (method, params_key(&entry["params"])),
                    entry["result"].clone(),
                )
            })
            .collect();
        Self {
            block_number: file["block_number"].as_u64().unwrap(),
            block_hash: file["block_hash"].as_str().unwrap().to_owned(),
            responses: Arc::new(Mutex::new(responses)),
        }
    }

    /// The recorded answer to one request.
    pub fn get(&self, method: &str, params: Value) -> Option<Value> {
        self.responses
            .lock()
            .unwrap()
            .get(&(method.to_owned(), params_key(&params)))
            .cloned()
    }

    /// Replace (or add) the answer to one request; works while the node is serving.
    pub fn set(&self, method: &str, params: Value, result: Value) {
        self.responses
            .lock()
            .unwrap()
            .insert((method.to_owned(), params_key(&params)), result);
    }

    /// The recorded answer for one storage key at the cassette's block: a `state_getStorage`
    /// reply, or the pair inside a `state_queryStorageAt` batch.
    pub fn storage(&self, key_hex: &str) -> Option<Value> {
        let responses = self.responses.lock().unwrap();
        let params = json!([key_hex, self.block_hash]);
        if let Some(value) = responses.get(&("state_getStorage".to_owned(), params_key(&params))) {
            return Some(value.clone());
        }
        responses
            .iter()
            .filter(|((method, _), _)| method == "state_queryStorageAt")
            .flat_map(|(_, batch)| batch.as_array().into_iter().flatten())
            .flat_map(|change_set| change_set["changes"].as_array().into_iter().flatten())
            .find(|pair| pair[0] == key_hex)
            .map(|pair| pair[1].clone())
    }

    /// Replace the answer for one storage key wherever the cassette holds it. `Null` inside a
    /// batch is what a node answers for a missing key, and the client drops that entry.
    pub fn set_storage(&self, key_hex: &str, result: Value) {
        let mut responses = self.responses.lock().unwrap();
        let params = json!([key_hex, self.block_hash]);
        responses.insert(
            ("state_getStorage".to_owned(), params_key(&params)),
            result.clone(),
        );
        for ((method, _), batch) in responses.iter_mut() {
            if method != "state_queryStorageAt" {
                continue;
            }
            for change_set in batch.as_array_mut().into_iter().flatten() {
                for pair in change_set["changes"].as_array_mut().into_iter().flatten() {
                    if pair[0] == key_hex {
                        pair[1] = result.clone();
                    }
                }
            }
        }
    }
}

impl Cassette {
    /// Reverse the pairs inside every `state_queryStorageAt` batch: the roster must not
    /// depend on the order a node answers in.
    pub fn reverse_batches(&self) {
        let mut responses = self.responses.lock().unwrap();
        for ((method, _), batch) in responses.iter_mut() {
            if method != "state_queryStorageAt" {
                continue;
            }
            for change_set in batch.as_array_mut().into_iter().flatten() {
                if let Some(changes) = change_set["changes"].as_array_mut() {
                    changes.reverse();
                }
            }
        }
    }
}

pub struct FakeNode {
    pub url: String,
    /// Hex of every extrinsic submitted through `author_submitAndWatchExtrinsic`.
    pub submissions: Arc<Mutex<Vec<String>>>,
    pub stall_submissions: Arc<AtomicBool>,
}

#[derive(Clone)]
struct Serving {
    cassette: Cassette,
    submissions: Arc<Mutex<Vec<String>>>,
    stall_submissions: Arc<AtomicBool>,
}

impl FakeNode {
    /// Serve from a background thread with its own runtime, for tests that drive the
    /// validator's blocking client (which owns a runtime of its own).
    pub fn spawn(cassette: Cassette) -> Self {
        let (sender, receiver) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(async move {
                let node = Self::start(cassette).await;
                sender
                    .send((node.url, node.submissions, node.stall_submissions))
                    .unwrap();
                std::future::pending::<()>().await;
            });
        });
        let (url, submissions, stall_submissions) = receiver.recv().unwrap();
        Self {
            url,
            submissions,
            stall_submissions,
        }
    }

    pub async fn start(cassette: Cassette) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}", listener.local_addr().unwrap());
        let serving = Serving {
            cassette,
            submissions: Arc::default(),
            stall_submissions: Arc::default(),
        };
        let submissions = serving.submissions.clone();
        let stall_submissions = serving.stall_submissions.clone();
        tokio::spawn(async move {
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                let serving = serving.clone();
                tokio::spawn(async move {
                    let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
                    while let Some(Ok(message)) = socket.next().await {
                        let replies = match message {
                            Message::Text(text) => serving.respond(&text),
                            Message::Close(_) => break,
                            _ => continue,
                        };
                        for reply in replies {
                            if socket.send(Message::text(reply.to_string())).await.is_err() {
                                return;
                            }
                        }
                    }
                });
            }
        });
        Self {
            url,
            submissions,
            stall_submissions,
        }
    }
}

impl Serving {
    /// The reply, followed by any subscription notifications it triggers.
    fn respond(&self, text: &str) -> Vec<Value> {
        let request: Value = serde_json::from_str(text).unwrap();
        let id = request["id"].clone();
        let method = request["method"].as_str().unwrap();
        let params = request.get("params").cloned().unwrap_or(Value::Null);
        let reply = |result: Value| json!({"jsonrpc": "2.0", "id": id, "result": result});
        match method {
            // Record the extrinsic and mark it ready, in block, finalized at
            // the cassette's block. Success or failure comes from System.Events at that
            // hash, which the test sets.
            "author_submitAndWatchExtrinsic" => {
                let extrinsic = params[0].as_str().unwrap().to_owned();
                self.submissions.lock().unwrap().push(extrinsic);
                let subscription = json!("burn-1");
                let update = |status: Value| {
                    json!({
                        "jsonrpc": "2.0",
                        "method": "author_extrinsicUpdate",
                        "params": {"subscription": subscription, "result": status},
                    })
                };
                let hash = &self.cassette.block_hash;
                if self.stall_submissions.load(Ordering::Relaxed) {
                    return vec![reply(subscription.clone()), update(json!("ready"))];
                }
                vec![
                    reply(subscription.clone()),
                    update(json!("ready")),
                    update(json!({"inBlock": hash})),
                    update(json!({"finalized": hash})),
                ]
            }
            "author_unwatchExtrinsic" => vec![reply(json!(true))],
            "chain_getBlock" if params[0] == self.cassette.block_hash => {
                let header = self
                    .cassette
                    .responses
                    .lock()
                    .unwrap()
                    .get(&("chain_getHeader".to_owned(), params_key(&params)))
                    .cloned()
                    .expect("recorded header");
                // The latest submission is extrinsic 0 of the block, matching the
                // ApplyExtrinsic(0) phase of the events the test serves.
                let extrinsics: Vec<String> = self
                    .submissions
                    .lock()
                    .unwrap()
                    .last()
                    .cloned()
                    .into_iter()
                    .collect();
                vec![reply(json!({
                    "block": {"header": header, "extrinsics": extrinsics},
                    "justifications": null,
                }))]
            }
            _ => {
                let found = self
                    .cassette
                    .responses
                    .lock()
                    .unwrap()
                    .get(&(method.to_owned(), params_key(&params)))
                    .cloned();
                vec![match found {
                    Some(result) => reply(result),
                    None => json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "error": {"code": -32601, "message": format!("fake node has no entry for {method} {params}")},
                    }),
                }]
            }
        }
    }
}
