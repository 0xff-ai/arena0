//! `GET /sync` (design §3.2): every Host's summary rows as one resumable
//! stream.
//!
//! Per Host, one task owns that Host's part of a connection:
//! 1. send `Host`;
//! 2. if the client's cursor names this Host's current `boot_id` and
//!    `StoreHandle::changes_since(seq)` returns `Keys`, send those keys' rows
//!    as one `Rows` at its `head`; otherwise send `Reset`, then the Host's
//!    snapshot (`HostService::sync_snapshot`) as `Rows` at the head read
//!    *before* the snapshot;
//! 3. send `Synced` at that seq;
//! 4. then loop: wait for the store's watched head to pass the last sent seq,
//!    call `changes_since(last sent)` and send the rows (`Keys`) or reset and
//!    snapshot again (`Snapshot`). Sending blocks on the connection's bounded
//!    channel; changes that arrive meanwhile are coalesced into the next
//!    catch-up, so a slow client is never dropped or told to reload.
//!
//! Reading a head before the rows it covers is what makes a cursor safe: the
//! rows sent are at least as new as `seq`, and every later change has a larger
//! seq and is sent later. Rows are idempotent upserts.
//!
//! Hosts are served concurrently (one task each), so each Host's rows go out
//! as soon as that Host is read. Hosts opened during the connection join it
//! (as `/events` does). Live-only facts flow as `Observed`: the Host's event
//! frames after connect, daemon activity frames, and the Host's open offers
//! (sent on connect and after each `negotiation.offer_seen`/`offer_closed`
//! event).
//!
//! The connection task and all per-Host tasks end when the client goes away
//! (the frame receiver is dropped) or the daemon shuts down.

use std::{collections::BTreeMap, sync::Arc};

use arena0_api::{ApiError, ApiErrorCode, EventData, Observation, SyncCursor, SyncFrame};
use arena0_crypto::AgentPubKey;
use arena0_store::Catchup;
use futures::{StreamExt, stream::BoxStream};
use tokio::{
    sync::{Semaphore, broadcast, mpsc},
    task::{AbortHandle, JoinSet},
};

use crate::ensemble::Daemon;

/// Capacity of one connection's frame channel. A full channel blocks the
/// per-Host tasks, which then coalesce (see module docs).
pub(crate) const SYNC_FRAME_BUFFER: usize = 64;

/// The frames of one `/sync` connection resuming from `cursor`.
pub(crate) fn sync_frames(
    daemon: Arc<Daemon>,
    cursor: SyncCursor,
) -> BoxStream<'static, SyncFrame> {
    // Subscribe before the roster read. A lagged open notification requires
    // another roster read, not a lost Host. Per-Host subscriptions are also
    // installed before returning the HTTP body to the caller.
    let mut opened = daemon.subscribe_host_opened();
    let mut activity = daemon.activity().subscribe();
    let mut shutdown = daemon.shutdown_receiver();
    let endpoint = daemon.daemon_info().expect("HTTP daemon has info").http_url;
    let (tx, rx) = mpsc::channel(SYNC_FRAME_BUFFER);
    let mut tasks = JoinSet::new();
    let mut seen = BTreeMap::new();
    let reads = Arc::new(Semaphore::new(
        std::thread::available_parallelism().map_or(1, usize::from),
    ));
    let online = !*shutdown.borrow();
    let host_tx = tx.clone();
    let add_hosts = move |tasks: &mut JoinSet<Result<(), ApiError>>,
                          seen: &mut BTreeMap<String, (String, AbortHandle)>| {
        for (host, service) in daemon.services() {
            let boot_id = service.store.boot_id().to_owned();
            if seen.get(&host).is_some_and(|(boot, _)| *boot == boot_id) {
                continue;
            }
            // A reopened name has a new change order. Stop its previous
            // publisher before the replacement can enqueue its first Host.
            if let Some((_, task)) = seen.remove(&host) {
                task.abort();
            }
            let mut events = service.events.subscribe();
            let mut watch = service.store.subscribe_changes();
            let resume = cursor
                .0
                .get(&host)
                .filter(|c| c.boot_id == boot_id)
                .cloned();
            let tx = host_tx.clone();
            let reads = Arc::clone(&reads);
            let identity = (host.clone(), boot_id.clone());
            let task = tasks.spawn(async move {
                let send = async |frame| {
                    tx.send(frame).await.map_err(|_| ApiError::new(ApiErrorCode::Storage, "sync receiver closed"))
                };
                send(SyncFrame::Host {
                    host: service.host_info(), online, boot_id: boot_id.clone(),
                    transport_key: AgentPubKey(service.peer_id().0),
                }).await?;
                // Capture the head before projecting. Reads may include later
                // commits; replaying their keys later is an idempotent upsert.
                let catchup = resume.map_or_else(
                    || Catchup::Snapshot { head: *watch.borrow() },
                    |c| service.store.changes_since(c.seq),
                );
                let project = async |catchup: Catchup| -> Result<u64, ApiError> {
                    match catchup {
                        Catchup::Keys { head, keys } => {
                            if !keys.is_empty() {
                                let permit = reads.acquire().await.expect("sync read semaphore open");
                                let ops = service.sync_rows(&keys).await?;
                                drop(permit);
                                send(SyncFrame::Rows { host: host.clone(), seq: head, ops }).await?;
                            }
                            Ok(head)
                        }
                        Catchup::Snapshot { head } => {
                            send(SyncFrame::Reset { host: host.clone() }).await?;
                            let permit = reads.acquire().await.expect("sync read semaphore open");
                            let ops = service.sync_snapshot().await?;
                            drop(permit);
                            send(SyncFrame::Rows { host: host.clone(), seq: head, ops }).await?;
                            Ok(head)
                        }
                    }
                };
                let mut last = project(catchup).await?;
                send(SyncFrame::Synced { host: host.clone(), seq: last }).await?;
                send(SyncFrame::Observed { observation: service.sync_offers().await? }).await?;
                loop {
                    // Check the current head before waiting: commits during
                    // projection or a blocked send still need catch-up even
                    // if no new write ever follows them.
                    if *watch.borrow_and_update() > last {
                        last = project(service.store.changes_since(last)).await?;
                    }
                    tokio::select! {
                        result = watch.changed() => { if result.is_err() { return Ok(()); } }
                        event = events.recv() => {
                            match event {
                                Ok(frame) => {
                                    let online = match &frame.data {
                                        EventData::HostStarted { .. } => Some(true),
                                        EventData::HostStopped { .. } => Some(false),
                                        _ => None,
                                    };
                                    if let Some(online) = online {
                                        send(SyncFrame::Host {
                                            host: frame.host.clone(), online, boot_id: boot_id.clone(),
                                            transport_key: AgentPubKey(service.peer_id().0),
                                        }).await?;
                                    }
                                    let offers_changed = matches!(frame.data, EventData::OfferSeen { .. } | EventData::OfferClosed { .. });
                                    send(SyncFrame::Observed { observation: Observation::Event(frame) }).await?;
                                    if offers_changed {
                                        send(SyncFrame::Observed { observation: service.sync_offers().await? }).await?;
                                    }
                                }
                                // Semantic events have no replay owner. Lost
                                // events do not compromise the row cursor.
                                Err(broadcast::error::RecvError::Lagged(_)) => {}
                                Err(broadcast::error::RecvError::Closed) => return Ok(()),
                            }
                        }
                    }
                }
            });
            seen.insert(identity.0, (identity.1, task));
        }
    };
    add_hosts(&mut tasks, &mut seen);
    tokio::spawn(async move {
        while !*shutdown.borrow() {
            tokio::select! {
                _ = tx.closed() => break,
                _ = shutdown.changed() => break,
                opened = opened.recv() => match opened {
                    Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => add_hosts(&mut tasks, &mut seen),
                    Err(broadcast::error::RecvError::Closed) => break,
                },
                activity = activity.recv() => match activity {
                    Ok(frame) => {
                        tokio::select! {
                            sent = tx.send(SyncFrame::Observed { observation: Observation::Activity(frame) }) => { if sent.is_err() { break; } }
                            _ = shutdown.changed() => break,
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => {}
                    Err(broadcast::error::RecvError::Closed) => break,
                },
                result = tasks.join_next(), if !tasks.is_empty() => {
                    let expected = match result {
                        Some(Ok(Ok(()))) => true,
                        Some(Err(error)) => error.is_cancelled(),
                        _ => false,
                    };
                    if !expected {
                        // A projection error cannot advance a cursor. End the
                        // stream so reconnect resumes from the applied cursor.
                        tracing::error!(event = "sync.connection_failed", "sync Host task failed");
                        break;
                    }
                }
            }
        }
        tasks.abort_all();
        while tasks.join_next().await.is_some() {}
        tracing::debug!(
            event = "sync.connection_closed",
            endpoint = %endpoint,
            "sync connection subscriptions released"
        );
    });
    futures::stream::unfold(rx, |mut rx| async move {
        rx.recv().await.map(|frame| (frame, rx))
    })
    .boxed()
}
