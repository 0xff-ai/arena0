//! Client-side id-prefix resolution for exec, session, and receipt ids.

use anyhow::bail;
use arena0_api::{ReceiptKey, Request, ResponseOk};
use arena0_protocol::{ExecId, SessionHash};

use crate::proto::DaemonClient;

/// Why a resident receipt reference could not be resolved.
#[derive(Debug, thiserror::Error)]
pub enum ResolveError {
    /// No resident receipt has this id or prefix.
    #[error("no receipt matches '{reference}'")]
    NotFound { reference: String },
    /// Several resident receipts match; this must not trigger session lookup.
    #[error("'{reference}' is ambiguous; candidates: {}", candidates.join(", "))]
    Ambiguous {
        reference: String,
        candidates: Vec<String>,
    },
    /// The daemon could not complete the receipt-list request.
    #[error(transparent)]
    Request(#[from] anyhow::Error),
    /// The daemon returned a success payload for a different operation.
    #[error("unexpected response to receipt.list")]
    UnexpectedResponse,
}

/// The outcome of matching a prefix against a set of full-hex candidates.
#[derive(Debug, PartialEq, Eq)]
pub enum PrefixOutcome {
    /// Exactly one candidate matched, at this index.
    Unique(usize),
    /// No candidate started with the prefix.
    None,
    /// Several candidates started with the prefix, at these indices.
    Ambiguous(Vec<usize>),
}

/// Resolve a (case-insensitive) hex prefix against full-hex `candidates`. A candidate
/// matches when its lowercased form starts with the lowercased prefix. An exact full
/// match short-circuits to that candidate even if it is also a prefix of another.
#[must_use]
pub fn resolve_prefix(prefix: &str, candidates: &[String]) -> PrefixOutcome {
    let needle = prefix.trim().to_ascii_lowercase();
    if needle.is_empty() {
        return PrefixOutcome::None;
    }
    // Exact match wins outright.
    if let Some(i) = candidates
        .iter()
        .position(|c| c.eq_ignore_ascii_case(&needle))
    {
        return PrefixOutcome::Unique(i);
    }
    let hits: Vec<usize> = candidates
        .iter()
        .enumerate()
        .filter(|(_, c)| c.to_ascii_lowercase().starts_with(&needle))
        .map(|(i, _)| i)
        .collect();
    match hits.len() {
        0 => PrefixOutcome::None,
        1 => PrefixOutcome::Unique(hits[0]),
        _ => PrefixOutcome::Ambiguous(hits),
    }
}

impl DaemonClient {
    /// Resolve an exec-id prefix against the daemon's execution list.
    pub async fn resolve_exec(&self, prefix: &str) -> anyhow::Result<ExecId> {
        // A full id parses directly, sparing a list fetch.
        if let Ok(id) = prefix.parse::<ExecId>() {
            return Ok(id);
        }
        let ResponseOk::ExecList(list) = self.call(&Request::ExecList).await? else {
            bail!("unexpected response to exec.list");
        };
        let hexes: Vec<String> = list.iter().map(|s| s.exec_id.to_string()).collect();
        match resolve_prefix(prefix, &hexes) {
            PrefixOutcome::Unique(i) => Ok(list[i].exec_id),
            PrefixOutcome::None => bail!("no execution matches '{prefix}'"),
            PrefixOutcome::Ambiguous(hits) => {
                let cands: Vec<String> = hits
                    .iter()
                    .map(|&i| list[i].exec_id.fmt_short().to_string())
                    .collect();
                bail!("'{prefix}' is ambiguous; candidates: {}", cands.join(", "))
            }
        }
    }

    /// Resolve a session-id prefix against the sessions the daemon holds receipts for
    /// (completed and imported), falling back to active executions.
    pub async fn resolve_session(&self, prefix: &str) -> anyhow::Result<SessionHash> {
        if let Ok(id) = prefix.parse::<SessionHash>() {
            return Ok(id);
        }
        let mut sessions: Vec<SessionHash> = Vec::new();
        if let ResponseOk::ReceiptList(list) = self.call(&Request::ReceiptList).await? {
            sessions.extend(list.iter().map(|e| e.session_id));
        }
        if let ResponseOk::ExecList(list) = self.call(&Request::ExecList).await? {
            sessions.extend(list.iter().filter_map(arena0_api::ExecStatus::session_id));
        }
        sessions.sort_by_key(|s| s.0);
        sessions.dedup();
        let hexes: Vec<String> = sessions.iter().map(ToString::to_string).collect();
        match resolve_prefix(prefix, &hexes) {
            PrefixOutcome::Unique(i) => Ok(sessions[i]),
            PrefixOutcome::None => bail!("no session matches '{prefix}'"),
            PrefixOutcome::Ambiguous(hits) => {
                let cands: Vec<String> = hits
                    .iter()
                    .map(|&i| sessions[i].fmt_short().to_string())
                    .collect();
                bail!("'{prefix}' is ambiguous; candidates: {}", cands.join(", "))
            }
        }
    }

    /// Resolve a receipt-id prefix against the daemon's receipt list, returning the
    /// matched entry (its session and producer are the verify/lookup key).
    pub async fn resolve_receipt(
        &self,
        prefix: &str,
    ) -> Result<arena0_api::ReceiptListEntry, ResolveError> {
        let ResponseOk::ReceiptList(list) = self.call(&Request::ReceiptList).await? else {
            return Err(ResolveError::UnexpectedResponse);
        };
        let hexes: Vec<String> = list.iter().map(|e| e.receipt_id.clone()).collect();
        match resolve_prefix(prefix, &hexes) {
            PrefixOutcome::Unique(i) => Ok(list[i].clone()),
            PrefixOutcome::None => Err(ResolveError::NotFound {
                reference: prefix.into(),
            }),
            PrefixOutcome::Ambiguous(hits) => {
                let cands: Vec<String> = hits
                    .iter()
                    .map(|&i| list[i].receipt_id[..8.min(list[i].receipt_id.len())].to_string())
                    .collect();
                Err(ResolveError::Ambiguous {
                    reference: prefix.into(),
                    candidates: cands,
                })
            }
        }
    }

    /// Resolve a resident receipt reference to its exact session/producer key.
    /// Receipt-id references use the list entry directly; a session reference uses
    /// the addressed daemon's own identity because a session can have receipts from
    /// several producers.
    pub async fn resolve_receipt_key(&self, reference: &str) -> anyhow::Result<ReceiptKey> {
        match self.resolve_receipt(reference).await {
            Ok(entry) => Ok(ReceiptKey {
                session_id: entry.session_id,
                producer: entry.producer,
            }),
            Err(ResolveError::NotFound { .. }) => self.resolve_session_key(reference).await,
            Err(error) => Err(error.into()),
        }
    }

    /// Resolve a session reference to the receipt produced by the addressed daemon.
    /// This is the explicit one-host path for commands whose argument is a session,
    /// not a receipt id; the daemon identity is obtained rather than inferred from
    /// whichever producer happens to sort first in storage.
    pub async fn resolve_session_key(&self, reference: &str) -> anyhow::Result<ReceiptKey> {
        let session_id = self.resolve_session(reference).await?;
        let ResponseOk::DaemonInfo(info) = self.call(&Request::DaemonInfo).await? else {
            bail!("unexpected response to daemon.info");
        };
        Ok(ReceiptKey {
            session_id,
            producer: info.peer_id,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids() -> Vec<String> {
        vec![
            "7c1e0a1b2c3d4e5f".to_string(),
            "a0b3ffee11223344".to_string(),
            "7c1e99998888aaaa".to_string(),
        ]
    }

    #[test]
    fn unique_prefix_resolves() {
        assert_eq!(resolve_prefix("a0", &ids()), PrefixOutcome::Unique(1));
        assert_eq!(resolve_prefix("A0B3", &ids()), PrefixOutcome::Unique(1));
    }

    #[test]
    fn ambiguous_prefix_lists_all() {
        assert_eq!(
            resolve_prefix("7c1e", &ids()),
            PrefixOutcome::Ambiguous(vec![0, 2])
        );
    }

    #[test]
    fn no_match_reports_none() {
        assert_eq!(resolve_prefix("dead", &ids()), PrefixOutcome::None);
        assert_eq!(resolve_prefix("", &ids()), PrefixOutcome::None);
    }

    #[test]
    fn exact_full_match_wins_over_prefix_ambiguity() {
        let cands = vec!["7c1e".to_string(), "7c1e0a1b".to_string()];
        // Exact match wins even when the same string is also a prefix.
        assert_eq!(resolve_prefix("7c1e", &cands), PrefixOutcome::Unique(0));
    }
    fn serve_responses(
        responses: Vec<(Request, arena0_api::Response)>,
    ) -> (tempfile::TempDir, DaemonClient, tokio::task::JoinHandle<()>) {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("daemon.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let task = tokio::spawn(async move {
            for (expected, response) in responses {
                let (mut stream, _) =
                    tokio::time::timeout(std::time::Duration::from_secs(5), listener.accept())
                        .await
                        .unwrap()
                        .unwrap();
                let request: Request = arena0_api::frame::read_frame(&mut stream)
                    .await
                    .unwrap()
                    .unwrap();
                assert_eq!(request, expected);
                arena0_api::frame::write_frame(&mut stream, &response)
                    .await
                    .unwrap();
            }
        });
        (dir, DaemonClient::new(socket), task)
    }

    fn receipt(id: &str) -> arena0_api::ReceiptListEntry {
        arena0_api::ReceiptListEntry {
            receipt_id: id.into(),
            session_id: SessionHash([1; 32]),
            producer: arena0_protocol::PeerId([2; 32]),
            program_id: arena0_program::ProgramHash([3; 32]),
            completed: true,
            provenance: arena0_api::ReceiptProvenance::Produced,
        }
    }

    #[tokio::test]
    async fn ambiguous_receipts_do_not_fall_back_to_sessions() {
        let (_dir, client, task) = serve_responses(vec![(
            Request::ReceiptList,
            Ok(ResponseOk::ReceiptList(vec![
                receipt("ab01"),
                receipt("ab02"),
            ])),
        )]);
        let error = client.resolve_receipt_key("ab").await.unwrap_err();
        assert!(matches!(
            error.downcast_ref::<ResolveError>(),
            Some(ResolveError::Ambiguous { .. })
        ));
        task.await.unwrap();
    }

    #[tokio::test]
    async fn receipt_request_errors_do_not_fall_back_to_sessions() {
        let (_dir, client, task) = serve_responses(vec![(
            Request::ReceiptList,
            Err(arena0_api::ApiError::new(
                arena0_api::ApiErrorCode::Storage,
                "unavailable",
            )),
        )]);
        let error = client.resolve_receipt_key("ab").await.unwrap_err();
        assert!(matches!(
            error.downcast_ref::<ResolveError>(),
            Some(ResolveError::Request(_))
        ));
        task.await.unwrap();
    }

    #[tokio::test]
    async fn missing_receipt_falls_back_to_exact_local_producer() {
        let peer = arena0_protocol::PeerId([2; 32]);
        let session = SessionHash([1; 32]);
        let (_dir, client, task) = serve_responses(vec![
            (Request::ReceiptList, Ok(ResponseOk::ReceiptList(vec![]))),
            (
                Request::DaemonInfo,
                Ok(ResponseOk::DaemonInfo(arena0_api::DaemonInfo {
                    name: "host-01".into(),
                    peer_id: peer,
                    transport_key: arena0_crypto::AgentPubKey([0; 32]),
                    version: "test".into(),
                    abi_version: 1,
                    uptime_secs: 0,
                    socket: String::new(),
                    programs: 0,
                    execs_active: 0,
                })),
            ),
        ]);
        let key = client
            .resolve_receipt_key(&session.to_string())
            .await
            .unwrap();
        assert_eq!(
            key,
            ReceiptKey {
                session_id: session,
                producer: peer
            }
        );
        task.await.unwrap();
    }
    #[tokio::test]
    async fn transport_failure_remains_a_request_error() {
        let dir = tempfile::tempdir().unwrap();
        let client = DaemonClient::new(dir.path().join("absent.sock"));
        let error = client.resolve_receipt_key("ab").await.unwrap_err();
        assert!(matches!(
            error.downcast_ref::<ResolveError>(),
            Some(ResolveError::Request(_))
        ));
        assert!(crate::proto::is_connect_error(&error));
    }

    #[tokio::test]
    async fn unexpected_receipt_response_does_not_fall_back() {
        let (_dir, client, task) =
            serve_responses(vec![(Request::ReceiptList, Ok(ResponseOk::Ack))]);
        let error = client.resolve_receipt_key("ab").await.unwrap_err();
        assert!(matches!(
            error.downcast_ref::<ResolveError>(),
            Some(ResolveError::UnexpectedResponse)
        ));
        task.await.unwrap();
    }
}
