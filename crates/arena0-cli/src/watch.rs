//! `arena0 watch`: subscribe to `events.subscribe` and render frames as a live
//! transcript. A selected execution stops at its terminal; without one the
//! command follows the local Host until Ctrl-C. `--json` emits one compact JSON
//! object per line (a live stream cannot be a single document).

use std::collections::HashMap;

use arena0_client::api::{
    EventData, EventFilter, EventFrame, Request, ResponseOk, SessionTerminal,
};
use arena0_client::protocol::ProgramHash;

use crate::Ctx;
use crate::ui::Palette;

/// `arena0 watch [<exec>]`.
pub(crate) async fn watch(ctx: &Ctx, exec: Option<String>) -> anyhow::Result<()> {
    let names = ctx.client().program_name_map().await;

    let single_exec = match &exec {
        Some(prefix) => {
            let exec_id = ctx.client().resolve_exec(prefix).await?;
            if !ctx.mode.is_json() {
                let program = current_program(ctx, exec_id, &names).await;
                println!("{}  {}", exec_id.fmt_short(), program);
            }
            Some(exec_id)
        }
        None => None,
    };

    let mut sub = ctx.client().subscribe(EventFilter::default()).await?;
    // The event stream starts at the subscription point, so a terminal event
    // that was committed before the subscription would otherwise be missed
    // forever. Re-check after subscribing: this closes the race between the
    // status lookup and the stream handshake while preserving future events.
    if let Some(exec_id) = single_exec {
        let status = match ctx.client().call(&Request::ExecStatus { exec_id }).await? {
            ResponseOk::Status(status) => status,
            other => anyhow::bail!("unexpected response to exec.status: {other:?}"),
        };
        if status.lifecycle().is_terminal() {
            return Ok(());
        }
    }
    loop {
        tokio::select! {
            frame = sub.next() => {
                let Some(frame) = frame? else { break };
                if let Some(target) = single_exec
                    && frame.exec_id != Some(target)
                {
                    continue;
                }
                let step_view = match (single_exec, &frame.data) {
                    (Some(target), EventData::SessionStep { .. }) => {
                        ctx.fetch_exec_view(target).await?
                    }
                    _ => None,
                };
                if ctx.mode.is_json() {
                    let mut doc = serde_json::to_value(&frame)?;
                    if let Some((step, view)) = &step_view {
                        doc["view"] = serde_json::json!({
                            "step": step,
                            "slots": &view.slots,
                        });
                    }
                    println!("{}", serde_json::to_string(&doc).unwrap_or_default());
                } else {
                    let mut line = render_frame(&frame, ctx.palette, &names);
                    if let Some((_step, view)) = &step_view
                        && let Some(inline) = crate::ui::render_view_inline(view, ctx.palette)
                    {
                        line.push_str("  ");
                        line.push_str(&inline);
                    }
                    println!("  {line}");
                }
                if single_exec.is_some_and(|target| frame.exec_id == Some(target))
                    && matches!(frame.data, EventData::SessionEnded { .. } | EventData::Terminated { .. })
                {
                    break;
                }
            }
            _ = tokio::signal::ctrl_c() => break,
        }
    }
    Ok(())
}

/// Render one event frame as a single transcript line (no leading indent; the caller
/// adds one). A UTC HH:MM:SS receive-time prefixes each line.
#[must_use]
pub(crate) fn render_frame(
    event: &EventFrame,
    p: Palette,
    names: &HashMap<ProgramHash, String>,
) -> String {
    let ts = p.dim(&hms_utc());
    let body = match &event.data {
        EventData::HostStarted { version, .. } => format!("host started ({version})"),
        EventData::HostStopped { reason, .. } => {
            format!(
                "host stopped{}",
                reason
                    .as_deref()
                    .map(|r| format!(": {r}"))
                    .unwrap_or_default()
            )
        }
        EventData::OfferSeen {
            program_id,
            negotiation_id,
            creator,
            ..
        } => format!(
            "offer {} {} from {}",
            negotiation_id.fmt_short(),
            program_label(program_id, names),
            creator.fmt_short()
        ),
        EventData::Created { .. } => format!("{} execution created", short_exec(event)),
        EventData::Terminated { reason, .. } => {
            p.red(&format!("{} terminated: {reason}", short_exec(event)))
        }
        EventData::NegotiationStarted { target_size } => {
            format!(
                "{} negotiation started (target {target_size})",
                short_exec(event)
            )
        }
        EventData::NegotiationOfferAccepted { creator, .. } => {
            format!(
                "{} offer accepted from {}",
                short_exec(event),
                creator.fmt_short()
            )
        }
        EventData::NegotiationTicketAccepted {
            participant,
            ticket_count,
            target_size,
            ..
        } => format!(
            "{} ticket accepted from {} ({ticket_count}/{target_size})",
            short_exec(event),
            participant.fmt_short()
        ),
        EventData::NegotiationPeers { lifecycle, peers } => format!(
            "{} negotiation {lifecycle:?} ({} peers)",
            short_exec(event),
            peers.len()
        ),
        EventData::NegotiationPrepared { participants }
        | EventData::NegotiationResumed { participants }
        | EventData::NegotiationCommitted { participants } => format!(
            "{} negotiation {} ({participants} participants)",
            short_exec(event),
            event.kind().rsplit('.').next().unwrap_or("updated")
        ),
        EventData::NegotiationRetried {
            attempt,
            stage,
            ticket_count,
            sig_count,
            target_size,
        } => format!(
            "{} negotiation retry {attempt} ({stage:?}, {ticket_count}/{target_size} tickets, {sig_count} sigs)",
            short_exec(event)
        ),
        EventData::NegotiationRejoined {} => format!("{} negotiation rejoined", short_exec(event)),
        EventData::NegotiationTimedOut { stage, .. } => {
            format!("{} negotiation timed out ({stage:?})", short_exec(event))
        }
        EventData::SessionStarted { ensemble } => format!(
            "{} session started, ensemble {}",
            short_exec(event),
            ensemble.len()
        ),
        EventData::SessionCallout {
            name, pending_id, ..
        } => {
            format!("{} {} (#{pending_id})", p.cyan("callout"), name)
        }
        EventData::SessionCalloutAnswered { pending_id } => {
            format!("{} callout answered (#{pending_id})", short_exec(event))
        }
        EventData::SessionStep {
            step,
            pre_state,
            post_state,
            fuel_used,
            signers,
            participants,
        } => {
            let mark = if signers == participants {
                p.green("agreed")
            } else {
                p.yellow("pending")
            };
            format!(
                "step {step:<4} {}→{}   fuel {}   {}",
                pre_state.fmt_short(),
                post_state.fmt_short(),
                fuel(*fuel_used),
                mark
            )
        }
        EventData::SessionEnded { terminal } => render_terminal(terminal, p),
        EventData::Lagged { skipped } => p.yellow(&format!("lagged: dropped {skipped} frames")),
    };
    let host = if event.host.is_empty() {
        String::new()
    } else {
        format!("{}  ", p.dim(&format!("[{}]", event.host)))
    };
    format!("{ts}  {host}{body}")
}

fn short_exec(event: &EventFrame) -> String {
    event
        .exec_id
        .map(|id| id.fmt_short().to_string())
        .unwrap_or_else(|| "exec".into())
}

fn render_terminal(result: &SessionTerminal, p: Palette) -> String {
    match result {
        SessionTerminal::Completed { outcome } => format!(
            "{}  outcome: {}",
            p.green("completed"),
            outcome
                .as_ref()
                .map(crate::ui::compact_json)
                .unwrap_or_else(|| "(none)".into())
        ),
        SessionTerminal::Aborted { step, reason } => {
            p.red(&format!("aborted at step {step}: {reason}"))
        }
    }
}

fn program_label(id: &ProgramHash, names: &HashMap<ProgramHash, String>) -> String {
    names
        .get(id)
        .cloned()
        .unwrap_or_else(|| id.fmt_short().to_string())
}

/// Render fuel compactly (e.g. `2.1M`, `950k`, `120`).
fn fuel(used: u64) -> String {
    if used >= 1_000_000 {
        format!("{:.1}M", used as f64 / 1_000_000.0)
    } else if used >= 1_000 {
        format!("{:.0}k", used as f64 / 1_000.0)
    } else {
        used.to_string()
    }
}

async fn current_program(
    ctx: &Ctx,
    exec_id: arena0_client::protocol::ExecId,
    names: &HashMap<ProgramHash, String>,
) -> String {
    match ctx.client().call(&Request::ExecStatus { exec_id }).await {
        Ok(ResponseOk::Status(s)) => program_label(&s.program_id, names),
        _ => "?".into(),
    }
}

/// Current UTC time as HH:MM:SS, computed from the unix clock (no timezone crate).
fn hms_utc() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let t = secs % 86_400;
    format!("{:02}:{:02}:{:02}", t / 3600, (t % 3600) / 60, t % 60)
}
