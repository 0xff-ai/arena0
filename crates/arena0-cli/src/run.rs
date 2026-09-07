//! Standalone execution helpers for low-level `arena0 exec` commands.

use std::io::{IsTerminal as _, Write as _};

use anyhow::{Context, bail};
use arena0_client::api::{EnsembleSpec, HostRequest, NextEvent, ResponseOk};
use arena0_client::protocol::{ExecId, NegotiationId, PeerId, SessionHash};
use serde_json::Value;

use crate::Ctx;

/// Parse the exact creator and negotiation identifiers accepted by `exec create --join`.
pub(crate) fn parse_join_ensemble(values: &[String]) -> anyhow::Result<EnsembleSpec> {
    let [creator, negotiation_id] = values else {
        bail!("--join requires exactly <creator> <negotiation-id>");
    };
    let creator = creator
        .parse::<PeerId>()
        .map_err(|_| anyhow::anyhow!("invalid join creator peer id: {creator}"))?;
    let negotiation_id = negotiation_id
        .parse::<NegotiationId>()
        .map_err(|_| anyhow::anyhow!("invalid join negotiation id: {negotiation_id}"))?;
    Ok(EnsembleSpec::Join {
        creator,
        negotiation_id,
    })
}

/// Prompt for local callouts and drive one existing execution to its terminal.
pub(crate) async fn drive_loop(ctx: &Ctx, exec_id: ExecId) -> anyhow::Result<Completed> {
    loop {
        match ctx.call(&HostRequest::ExecNext { exec_id }).await? {
            ResponseOk::Next(NextEvent::Callout {
                pending_id,
                name,
                prompt,
                schema,
                context,
                ..
            }) => {
                if !ctx.mode.is_json()
                    && let Some((_step, view)) = ctx.fetch_exec_view(exec_id).await?
                {
                    let rendered = crate::ui::render_view_summary(&view, ctx.palette);
                    if !rendered.is_empty() {
                        eprintln!();
                        eprint!("{rendered}");
                    }
                }
                let answer =
                    prompt_answer(ctx, exec_id, &name, &prompt, &context, schema.as_value())
                        .await?;
                ctx.call(&HostRequest::ExecSubmit {
                    exec_id,
                    pending_id,
                    answer: Some(answer),
                })
                .await?;
                eprintln!(
                    "{}",
                    ctx.palette
                        .dim("  submitted. waiting for other participants…")
                );
            }
            ResponseOk::Next(NextEvent::Completed {
                session_id,
                outcome,
            }) => {
                return Ok(Completed {
                    session_id,
                    outcome,
                });
            }
            ResponseOk::Next(NextEvent::Failed { reason }) => {
                bail!("execution failed: {reason}")
            }
            other => bail!("unexpected response to exec.next: {other:?}"),
        }
    }
}

/// The terminal returned by a standalone execution drive.
#[derive(Debug, Clone)]
pub(crate) struct Completed {
    pub(crate) session_id: SessionHash,
    pub(crate) outcome: Option<Value>,
}

async fn prompt_answer(
    ctx: &Ctx,
    exec_id: ExecId,
    name: &str,
    prompt: &str,
    context: &Value,
    schema: &Value,
) -> anyhow::Result<Value> {
    if !std::io::stdin().is_terminal()
        || !std::io::stdout().is_terminal()
        || !std::io::stderr().is_terminal()
    {
        bail!(
            "callout `{name}` needs an interactive terminal; use `arena0 exec next {exec_id}` then `arena0 exec submit {exec_id} --pending-id <id> --answer '<json>'`"
        );
    }
    let palette = ctx.palette;
    eprintln!();
    eprintln!(
        "{}  {}  {}  {}",
        palette.bold("callout"),
        palette.cyan(name),
        palette.dim("—"),
        prompt
    );
    if !context.is_null() {
        eprintln!(
            "  {} {}",
            palette.dim("context:"),
            crate::ui::compact_json(context)
        );
    }
    eprintln!("  schema:");
    eprintln!(
        "{}",
        serde_json::to_string_pretty(schema).context("render callout answer schema")?
    );
    eprint!("{} ", palette.bold(">"));
    std::io::stderr().flush().ok();
    crate::coordinated::read_validated_answer(schema, crate::line_input::read_line)
        .await
        .map_err(|error| {
            if error.to_string().contains("stdin closed") {
                eprintln!(
                    "{} session still running — resume with `arena0 exec next {exec_id}` and submit the callout answer",
                    palette.yellow("note:")
                );
            }
            error.context("read answer")
        })
}
