//! The Rex control connection (`rex.control.v1`): JSON requests, responses
//! and events over one WebSocket. See the server's own guide at `/llms.txt`.

use std::collections::VecDeque;
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

use polling::Poller;
use serde_json::{json, Value};

use crate::wire::{self, In, Out, Target, Wire};

/// The API revision this client was written for.
const API_REVISION: u32 = 18;

/// Every connection's messages, funnelled into one queue for the main loop.
pub struct Net {
    rx: mpsc::Receiver<(usize, In)>,
    tx: mpsc::Sender<(usize, In)>,
    poller: Arc<Poller>,
    backlog: VecDeque<(usize, In)>,
    next_wire: usize,
    pub target: Target,
}

impl Net {
    pub fn new(target: Target, poller: Arc<Poller>) -> Self {
        let (tx, rx) = mpsc::channel();
        Self {
            rx,
            tx,
            poller,
            backlog: VecDeque::new(),
            next_wire: 1,
            target,
        }
    }

    pub fn open(&mut self, route: &str, subprotocol: &str) -> Wire {
        self.next_wire += 1;
        wire::spawn(
            self.next_wire,
            self.target.clone(),
            route,
            subprotocol,
            self.tx.clone(),
            Arc::clone(&self.poller),
        )
    }

    /// Wait for the next message on one connection. Messages for others
    /// stay queued, in order.
    pub fn wait_wire(&mut self, wire: usize, timeout: Duration) -> Option<In> {
        if let Some(i) = self.backlog.iter().position(|m| m.0 == wire) {
            return self.backlog.remove(i).map(|m| m.1);
        }
        let deadline = Instant::now() + timeout;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.rx.recv_timeout(left) {
                Ok((w, m)) if w == wire => return Some(m),
                Ok(other) => self.backlog.push_back(other),
                Err(_) => return None,
            }
        }
    }

    /// Take one connection's messages out of the backlog: the ones that
    /// arrived before the response the last `Control::call` waited for.
    pub fn take_wire(&mut self, wire: usize) -> Vec<In> {
        let (mine, rest): (Vec<_>, Vec<_>) = self.backlog.drain(..).partition(|m| m.0 == wire);
        self.backlog = rest.into();
        mine.into_iter().map(|m| m.1).collect()
    }

    /// The next message from any connection, if one is waiting.
    pub fn pop(&mut self) -> Option<(usize, In)> {
        self.backlog.pop_front().or_else(|| self.rx.try_recv().ok())
    }
}

pub struct Control {
    wire: Wire,
    next_id: u64,
    pub client_id: String,
}

impl Control {
    /// Connect, say hello and wait for the welcome.
    ///
    /// `kind` is "other" unless the user opted in to "app". An app receives
    /// the prompts that actions raise, but the CLI picks "the app" only
    /// while exactly one is connected, so a second app changes how `rex`
    /// commands on the Mac choose their client.
    pub fn connect(net: &mut Net, kind: &str) -> Result<Control, String> {
        let wire = net.open("/api/control", "rex.control.v1");
        let hello = json!({
            "type": "hello",
            "api_revision": API_REVISION,
            "client": {
                "kind": kind,
                "platform": "linux",
                "name": "rexpaper",
                "version": env!("CARGO_PKG_VERSION"),
                "label": "remarkable",
            },
        });
        wire.send(Out::Text(hello.to_string()));
        let deadline = Instant::now() + Duration::from_secs(25);
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match net.rx.recv_timeout(left) {
                Ok((w, In::Text(t))) if w == wire.id => {
                    let v: Value = serde_json::from_str(&t).map_err(|e| e.to_string())?;
                    if v["type"] != "welcome" || v["protocol"] != "rex.control.v1" {
                        return Err("the server did not speak rex.control.v1".into());
                    }
                    let client_id = v["client"]["id"].as_str().unwrap_or("").to_string();
                    return Ok(Control {
                        wire,
                        next_id: 0,
                        client_id,
                    });
                }
                Ok((w, In::Closed(reason))) if w == wire.id => return Err(reason),
                Ok(other) => net.backlog.push_back(other),
                Err(_) => return Err("the server did not answer in time".into()),
            }
        }
    }

    pub fn wire_id(&self) -> usize {
        self.wire.id
    }

    /// Send a request without waiting. Returns its id.
    pub fn send(&mut self, method: &str, payload: Value) -> String {
        self.next_id += 1;
        let id = self.next_id.to_string();
        let msg = json!({"type": "request", "id": id, "method": method, "payload": payload});
        self.wire.send(Out::Text(msg.to_string()));
        id
    }

    /// Send a request and wait for its response. Messages that arrive in
    /// the meantime stay queued for the main loop, in order.
    pub fn call(&mut self, net: &mut Net, method: &str, payload: Value) -> Result<Value, String> {
        let id = self.send(method, payload);
        let deadline = Instant::now() + Duration::from_secs(12);
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match net.rx.recv_timeout(left) {
                Ok((w, In::Text(t))) if w == self.wire.id => {
                    if let Ok(v) = serde_json::from_str::<Value>(&t) {
                        if v["type"] == "response" && v["id"] == id.as_str() {
                            return match v.get("error").filter(|e| !e.is_null()) {
                                Some(e) => Err(format!(
                                    "{method}: {}",
                                    e["message"].as_str().unwrap_or("failed")
                                )),
                                None => Ok(v.get("payload").cloned().unwrap_or(Value::Null)),
                            };
                        }
                    }
                    net.backlog.push_back((w, In::Text(t)));
                }
                Ok((w, In::Closed(reason))) if w == self.wire.id => {
                    net.backlog.push_back((w, In::Closed(reason.clone())));
                    return Err(reason);
                }
                Ok(other) => net.backlog.push_back(other),
                Err(_) => return Err(format!("{method}: no answer")),
            }
        }
    }

    /// Call a method of a terminal block.
    pub fn terminal(
        &mut self,
        net: &mut Net,
        method: &str,
        session_id: &str,
        block_id: &str,
        args: Value,
    ) -> Result<Value, String> {
        self.call(
            net,
            &format!("com.superlogical.terminal.{method}"),
            json!({"session_id": session_id, "block_id": block_id, "args": args}),
        )
    }
}
