//! `arena0 verify`: spelled-out receipt evidence. Two target kinds:
//!
//! - a **session id** or **receipt id** the daemon holds -> the daemon verifies and
//!   returns the evidence;
//! - a **file path** -> verified daemonlessly: parsing authenticates the artifact.

use std::collections::HashSet;
use std::path::Path;

use anyhow::{Context, bail};
use arena0_client::api::{
    HostRequest, ReceiptArtifact, ReceiptSummary, ReceiptTermination, ResponseOk,
};
use arena0_client::proto::DaemonClient;
use arena0_client::protocol::{ABI_VERSION, PeerId, ProgramHash};
use arena0_home::HostName;
use serde_json::json;
use tokio::task::JoinSet;

use crate::coordinated::{HostEvidence, compare_evidence};
use crate::{Ctx, ui};

/// The evidence a verification recovers, rendered the same whether it came from the
/// daemon or a daemonless run.
struct Evidence {
    summary: ReceiptSummary,
    program_name: Option<String>,
    local_peer: Option<PeerId>,
}

pub(crate) async fn verify(ctx: &Ctx, target: String) -> anyhow::Result<()> {
    let path = Path::new(&target);
    if is_path_target(path, &target) {
        verify_file(ctx, path).await
    } else {
        verify_resident(ctx, &target).await
    }
}

/// Verify every Host of one resident session through independently named
/// local Hosts, then require their authenticated shared evidence to agree.
pub(crate) async fn verify_hosts(
    mode: ui::Mode,
    palette: ui::Palette,
    target: &str,
    hosts: &[HostName],
) -> anyhow::Result<()> {
    let mut names = HashSet::with_capacity(hosts.len());
    for host in hosts {
        if !names.insert(host.clone()) {
            bail!("Host '{host}' was selected more than once");
        }
    }

    let mut jobs = JoinSet::new();
    for (index, host) in hosts.iter().cloned().enumerate() {
        let target = target.to_owned();
        jobs.spawn(async move {
            let client = DaemonClient::from_env()
                .with_context(|| format!("resolve daemon endpoint for Host '{host}'"))?;
            let info = match client.call_host(&host, &HostRequest::Info).await? {
                ResponseOk::HostStatus(info) => info,
                other => bail!("unexpected daemon.info response from Host '{host}': {other:?}"),
            };
            let key = client
                .resolve_produced_receipt_ref(&host, &target)
                .await
                .with_context(|| format!("resolve receipt on Host '{host}'"))?;
            let response = client
                .call_host(&host, &HostRequest::ReceiptVerify { receipt: key })
                .await
                .with_context(|| format!("verify Host receipt on Host '{host}'"))?;
            let ResponseOk::Verified(summary) = response else {
                bail!("unexpected receipt.verify response from Host '{host}'");
            };
            Ok::<_, anyhow::Error>((
                index,
                host,
                HostEvidence {
                    peer_id: info.host.peer_id,
                    summary,
                },
            ))
        });
    }

    let mut ordered = (0..hosts.len())
        .map(|_| None)
        .collect::<Vec<Option<(HostName, HostEvidence)>>>();
    while let Some(joined) = jobs.join_next().await {
        let (index, host, evidence) =
            joined.context("Host receipt verification task failed to join")??;
        ordered[index] = Some((host, evidence));
    }
    let evidence = ordered
        .into_iter()
        .enumerate()
        .map(|(index, entry)| {
            entry.ok_or_else(|| anyhow::anyhow!("verification result for Host {index} missing"))
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    let receipts = evidence
        .iter()
        .map(|(_, receipt)| receipt.clone())
        .collect::<Vec<_>>();
    let agreement = compare_evidence(&receipts)?;
    let producers = receipts
        .iter()
        .map(|receipt| receipt.peer_id)
        .collect::<HashSet<_>>();
    if producers.len() != hosts.len()
        || agreement.ensemble.len() != hosts.len()
        || !agreement
            .ensemble
            .iter()
            .all(|peer| producers.contains(peer))
    {
        bail!(
            "--hosts must name every independent peer_id in the {}-party receipt ensemble",
            agreement.ensemble.len()
        );
    }

    render_host_evidence(mode, palette, &evidence, &agreement);
    Ok(())
}

/// Receipt IDs and session ids are bare opaque values. Any target with path
/// syntax is an explicit file target, even when it does not exist yet; falling
/// through to resident-id lookup would hide the actionable filesystem error.
pub(crate) fn is_path_target(path: &Path, target: &str) -> bool {
    path.is_file()
        || path.is_absolute()
        || target.contains('/')
        || target.contains('\\')
        || path.extension().is_some()
}

/// Verify a session/receipt id the daemon holds.
async fn verify_resident(ctx: &Ctx, target: &str) -> anyhow::Result<()> {
    if !ctx.client().daemon_up().await {
        bail!(
            "daemon not reachable at {}. To verify a receipt file offline, pass its path.",
            ctx.client.socket().display()
        );
    }
    // A receipt-id prefix resolves to its exact peer_id; a session id uses the
    // addressed daemon's peer_id.
    let key = ctx.client().resolve_receipt_ref(&ctx.host, target).await?;
    let resp = ctx
        .call(&HostRequest::ReceiptVerify { receipt: key })
        .await?;
    let ResponseOk::Verified(summary) = resp else {
        bail!("unexpected verify response");
    };
    let names = ctx.client().program_name_map(&ctx.host).await;
    let local_peer = node_peer_id(ctx).await;
    render(
        ctx.mode,
        ctx.palette,
        &Evidence {
            program_name: names.get(&summary.program_id).cloned(),
            summary,
            local_peer,
        },
    );
    Ok(())
}

/// Verify a receipt file's portable proof and optionally enrich its presentation.
async fn verify_file(ctx: &Ctx, path: &Path) -> anyhow::Result<()> {
    verify_file_offline(Some(ctx), ctx.mode, ctx.palette, path).await
}

/// Verify a receipt file without constructing a daemon client. Optional daemon
/// enrichment is deliberately absent when no explicit socket was supplied.
pub(crate) async fn verify_offline(
    mode: crate::ui::Mode,
    palette: crate::ui::Palette,
    path: &Path,
) -> anyhow::Result<()> {
    verify_file_offline(None, mode, palette, path).await
}

/// Read and parse an explicit receipt file before consulting a Host.
pub(crate) fn read_receipt(path: &Path) -> anyhow::Result<ReceiptArtifact> {
    let bytes = std::fs::read(path).with_context(|| format!("read {}", path.display()))?;
    serde_json::from_slice(&bytes).with_context(|| format!("parse receipt {}", path.display()))
}

/// Verify a receipt file's portable proof and render its evidence.
async fn verify_file_offline(
    ctx: Option<&Ctx>,
    mode: crate::ui::Mode,
    palette: crate::ui::Palette,
    path: &Path,
) -> anyhow::Result<()> {
    // Parsing authenticates the artifact entirely offline.
    let summary = read_receipt(path)?.summary();
    let (program_name, local_peer) = match ctx {
        Some(ctx) => enrich_offline(ctx, summary.program_id).await,
        None => (None, None),
    };
    render(
        mode,
        palette,
        &Evidence {
            summary,
            program_name,
            local_peer,
        },
    );
    Ok(())
}

/// Best-effort enrichment for offline evidence: the program's handle name and our
/// own peer id, all only if a daemon happens to be reachable.
async fn enrich_offline(ctx: &Ctx, program_id: ProgramHash) -> (Option<String>, Option<PeerId>) {
    if !ctx.client().daemon_up().await {
        return (None, None);
    }
    let local_peer = node_peer_id(ctx).await;
    let mut name = None;
    if let Ok(ResponseOk::Program(detail)) = ctx
        .call(&HostRequest::ProgramGet {
            program: program_id.to_string(),
        })
        .await
    {
        name = Some(detail.summary.name.clone());
    }
    (name, local_peer)
}

async fn node_peer_id(ctx: &Ctx) -> Option<PeerId> {
    match ctx.call(&HostRequest::Info).await {
        Ok(ResponseOk::HostStatus(i)) => Some(i.host.peer_id),
        _ => None,
    }
}

fn render(mode: crate::ui::Mode, palette: crate::ui::Palette, ev: &Evidence) {
    let summary = &ev.summary;
    if mode.is_json() {
        crate::ui::print_json(&json!({
            "program_id": summary.program_id.to_string(),
            "program": ev.program_name,
            "session_id": summary.session_id.to_string(),
            "receipt_id": summary.receipt_id.to_string(),
            "ensemble": summary.ensemble.iter().map(ToString::to_string).collect::<Vec<_>>(),
            "steps": summary.steps,
            "terminal": summary.terminal,
            "outcome_borsh": summary.outcome_borsh,
        }));
        return;
    }

    let p = palette;
    println!("{}", p.green("verified"));
    let name = ev
        .program_name
        .clone()
        .unwrap_or_else(|| "(unregistered)".into());
    println!(
        "  program     {}  ({}, abi {})",
        name,
        summary.program_id.fmt_short(),
        ABI_VERSION
    );
    println!("  session     {}", summary.session_id.fmt_short());
    println!("  receipt     {}", summary.receipt_id);
    println!(
        "  ensemble    {}",
        render_ensemble(&summary.ensemble, ev.local_peer)
    );
    match &summary.terminal {
        ReceiptTermination::Completed => {
            println!("  steps       {}, chain intact, all agreed", summary.steps);
            println!("  outcome     (JSON projection unavailable without the program)");
        }
        ReceiptTermination::Stopped { cause } => {
            println!("  steps       {}, chain intact", summary.steps);
            println!("  {}     stopped: {cause:?}", p.yellow("terminal"));
        }
    }
}

fn render_host_evidence(
    mode: ui::Mode,
    palette: ui::Palette,
    evidence: &[(HostName, HostEvidence)],
    agreement: &ReceiptSummary,
) {
    if mode.is_json() {
        ui::print_json(&json!({
            "program_id": agreement.program_id.to_string(),
            "session_id": agreement.session_id.to_string(),
            "receipt_id": agreement.receipt_id.to_string(),
            "ensemble": agreement.ensemble.iter().map(ToString::to_string).collect::<Vec<_>>(),
            "steps": agreement.steps,
            "terminal": agreement.terminal,
            "outcome_borsh": agreement.outcome_borsh,
            "producers": evidence.iter().map(|(host, receipt)| json!({
                "host": host.to_string(),
                "peer_id": receipt.peer_id.to_string(),
                "result": "valid",
            })).collect::<Vec<_>>(),
            "all_verified": true,
            "shared_evidence_agrees": true,
        }));
        return;
    }

    println!(
        "{} {}/{} Host receipts",
        palette.green("verified"),
        evidence.len(),
        agreement.ensemble.len()
    );
    println!("  program     {}", agreement.program_id.fmt_short());
    println!("  session     {}", agreement.session_id.fmt_short());
    println!("  receipt     {}", agreement.receipt_id);
    println!("  steps       {}", agreement.steps);
    for (host, receipt) in evidence {
        println!("  peer_id    {}  {}", host, receipt.peer_id.fmt_short());
    }
    match &agreement.terminal {
        ReceiptTermination::Completed => {
            println!("  outcome     (JSON projection unavailable without the program)");
        }
        ReceiptTermination::Stopped { cause } => {
            println!("  {}     stopped: {cause:?}", palette.yellow("terminal"));
        }
    }
}

/// Render the ensemble as `peer (idx)`, marking our own identity as `you`.
fn render_ensemble(ensemble: &[PeerId], local: Option<PeerId>) -> String {
    ensemble
        .iter()
        .enumerate()
        .map(|(i, peer)| {
            if Some(*peer) == local {
                format!("you ({i})")
            } else {
                format!("{} ({i})", peer.fmt_short())
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_and_resident_receipt_references_are_distinguished() {
        for target in [
            "receipt.json",
            "./receipt.json",
            "../receipt.json",
            "/tmp/receipt.json",
            "subdir/receipt",
            r"C:\\receipts\\receipt.json",
            "receipt.bin",
            "receipt.receipt",
            "receipt.txt",
        ] {
            assert!(is_path_target(Path::new(target), target), "path: {target}");
        }

        for target in [
            "a".repeat(64),
            "receipt-id".to_owned(),
            "session-id".to_owned(),
        ] {
            assert!(
                !is_path_target(Path::new(&target), &target),
                "resident reference: {target}"
            );
        }

        let directory = tempfile::tempdir().expect("temporary directory");
        let path = directory.path().join("receipt");
        std::fs::write(&path, b"not a receipt").expect("write fixture");

        assert!(is_path_target(&path, path.to_str().expect("UTF-8 path")));
    }
}
