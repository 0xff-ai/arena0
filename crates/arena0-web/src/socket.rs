//! One task per WebSocket.
//!
//! The task sends `hello`, the snapshot, and `ready`, then forwards row
//! changes and answers calls. Writing happens in a separate task fed through
//! an [`Outbox`], so a slow browser never blocks reading calls or the
//! replica's broadcast.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::ws::{Message, WebSocket};
use futures::{SinkExt as _, StreamExt as _};
use serde::Deserialize;
use tokio::sync::{Notify, broadcast};
use tokio::task::JoinSet;

use crate::http::Gateway;
use crate::protocol::{CallResult, ClientFrame, ErrorCode, ErrorRow, Hello, ServerFrame};

/// Queued row frames beyond this drop the queue and resend a snapshot.
const OUTGOING_FRAMES: usize = 256;
const CLOSE_GRACE: Duration = Duration::from_secs(2);

/// Frames waiting for the writer. Replies are never dropped; row frames are,
/// wholesale, when the browser falls behind.
#[derive(Default)]
struct Outbox {
    state: Mutex<OutboxState>,
    wake: Notify,
}

#[derive(Default)]
struct OutboxState {
    rows: VecDeque<String>,
    replies: VecDeque<String>,
    closed: bool,
}

impl Outbox {
    fn state(&self) -> std::sync::MutexGuard<'_, OutboxState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn reply(&self, frame: String) {
        self.state().replies.push_back(frame);
        self.wake.notify_one();
    }

    /// Queue row frames. When the queue would exceed its bound it is emptied
    /// and false is returned: the caller must send a fresh snapshot.
    fn rows(&self, frames: impl IntoIterator<Item = String>) -> bool {
        let mut state = self.state();
        state.rows.extend(frames);
        let fits = state.rows.len() <= OUTGOING_FRAMES;
        if !fits {
            state.rows.clear();
        }
        drop(state);
        self.wake.notify_one();
        fits
    }

    /// Drop everything queued and queue `frames` instead.
    fn replace_rows(&self, frames: Vec<String>) {
        let mut state = self.state();
        state.rows.clear();
        state.rows.extend(frames);
        drop(state);
        self.wake.notify_one();
    }

    fn close(&self) {
        self.state().closed = true;
        self.wake.notify_one();
    }

    async fn next(&self) -> Option<String> {
        loop {
            {
                let mut state = self.state();
                if let Some(frame) = state.replies.pop_front().or_else(|| state.rows.pop_front()) {
                    return Some(frame);
                }
                if state.closed {
                    return None;
                }
            }
            self.wake.notified().await;
        }
    }
}

fn to_json(frame: &ServerFrame) -> String {
    serde_json::to_string(frame).expect("server frames serialize")
}

/// Just enough of a call to answer it when the operation is malformed.
#[derive(Deserialize)]
struct Envelope {
    id: u32,
}

pub(crate) async fn serve(socket: WebSocket, gateway: Arc<Gateway>) {
    // Held until this task ends so `UiServer::shutdown` can wait for it.
    let alive = gateway
        .sockets
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone();
    let Some(_alive) = alive else { return };
    let mut shutdown = gateway.shutdown.clone();
    let (mut sink, mut stream) = socket.split();

    let Ok(daemon) = gateway.daemon_row().await else {
        let _ = sink.close().await;
        return;
    };
    let hello = to_json(&ServerFrame::Hello(Hello {
        version: env!("CARGO_PKG_VERSION").to_owned(),
        daemon,
        strategies: gateway.launcher.strategies(),
        attached: gateway.attached,
    }));
    let outbox = Arc::new(Outbox::default());
    let crate::replica::Subscription {
        resets,
        mut changes,
    } = gateway.replica.subscribe().await;
    // `hello` travels with the replies, which are never dropped, and every
    // snapshot ends in `ready`, so a resync before the first flush still
    // completes the handshake. Nothing else is queued yet, so `hello` leaves
    // first.
    outbox.reply(hello);
    outbox.replace_rows(snapshot(resets));

    let mut writer = {
        let outbox = outbox.clone();
        tokio::spawn(async move {
            while let Some(frame) = outbox.next().await {
                if sink.send(Message::text(frame)).await.is_err() {
                    return;
                }
            }
            let _ = sink.close().await;
        })
    };

    let mut calls: JoinSet<(u32, CallResult)> = JoinSet::new();
    loop {
        tokio::select! {
            _ = shutdown.changed() => break,
            _ = &mut writer => break,
            message = stream.next() => match message {
                Some(Ok(Message::Text(text))) => {
                    match serde_json::from_str::<ClientFrame>(text.as_str()) {
                        Ok(ClientFrame::Call { id, op }) => {
                            let gateway = gateway.clone();
                            calls.spawn(async move { (id, gateway.ops.call(op).await) });
                        }
                        Err(_) => match serde_json::from_str::<Envelope>(text.as_str()) {
                            Ok(Envelope { id }) => outbox.reply(to_json(&ServerFrame::Reply {
                                id,
                                result: CallResult::Err(ErrorRow {
                                    code: ErrorCode::BadRequest,
                                    message: "unknown or malformed operation".to_owned(),
                                }),
                            })),
                            Err(_) => break,
                        },
                    }
                }
                Some(Ok(Message::Ping(_) | Message::Pong(_))) => {}
                Some(Ok(Message::Binary(_) | Message::Close(_)) | Err(_)) | None => break,
            },
            change = changes.recv() => match change {
                Ok(frames) => {
                    if !outbox.rows(frames.iter().cloned()) {
                        resync(&gateway, &outbox, &mut changes).await;
                    }
                }
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    resync(&gateway, &outbox, &mut changes).await;
                }
                Err(broadcast::error::RecvError::Closed) => break,
            },
            Some(done) = calls.join_next(), if !calls.is_empty() => {
                if let Ok((id, result)) = done {
                    outbox.reply(to_json(&ServerFrame::Reply { id, result }));
                }
            }
        }
    }

    // In-flight calls finish on the daemon; their replies have nowhere to go.
    calls.detach_all();
    outbox.close();
    if tokio::time::timeout(CLOSE_GRACE, &mut writer)
        .await
        .is_err()
    {
        writer.abort();
    }
}

/// The browser fell behind: replace what it has not received with a full
/// snapshot and continue from that instant.
async fn resync(
    gateway: &Gateway,
    outbox: &Outbox,
    changes: &mut broadcast::Receiver<Arc<Vec<String>>>,
) {
    let crate::replica::Subscription {
        resets,
        changes: fresh,
    } = gateway.replica.subscribe().await;
    *changes = fresh;
    outbox.replace_rows(snapshot(resets));
}

/// A full snapshot: every collection's reset, then `ready`. The browser
/// treats a repeated `ready` as a no-op.
fn snapshot(resets: Vec<String>) -> Vec<String> {
    let mut frames = resets;
    frames.push(to_json(&ServerFrame::Ready));
    frames
}
