//! Client-side id-prefix resolution for exec, session, and receipt ids.

use anyhow::bail;
use arena0_api::{HostRequest, ReceiptRef, ResponseOk};
use arena0_home::HostName;
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
    pub async fn resolve_exec(&self, host: &HostName, prefix: &str) -> anyhow::Result<ExecId> {
        // A full id parses directly, sparing a list fetch.
        if let Ok(id) = prefix.parse::<ExecId>() {
            return Ok(id);
        }
        let ResponseOk::ExecList(list) = self.call_host(host, &HostRequest::ExecList).await? else {
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
    pub async fn resolve_session(
        &self,
        host: &HostName,
        prefix: &str,
    ) -> anyhow::Result<SessionHash> {
        if let Ok(id) = prefix.parse::<SessionHash>() {
            return Ok(id);
        }
        let mut sessions: Vec<SessionHash> = Vec::new();
        if let ResponseOk::ReceiptList(list) =
            self.call_host(host, &HostRequest::ReceiptList).await?
        {
            sessions.extend(list.iter().map(|e| e.session_id));
        }
        if let ResponseOk::ExecList(list) = self.call_host(host, &HostRequest::ExecList).await? {
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
        host: &HostName,
        prefix: &str,
    ) -> Result<arena0_api::ReceiptListEntry, ResolveError> {
        let ResponseOk::ReceiptList(list) = self.call_host(host, &HostRequest::ReceiptList).await?
        else {
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

    /// Resolve a content ID first, then this Host's publication for a session.
    pub async fn resolve_receipt_ref(
        &self,
        host: &HostName,
        reference: &str,
    ) -> anyhow::Result<ReceiptRef> {
        match self.resolve_receipt(host, reference).await {
            Ok(entry) => Ok(ReceiptRef::Stored(entry.receipt_id.parse()?)),
            Err(ResolveError::NotFound { .. }) => self.resolve_session_ref(host, reference).await,
            Err(error) => Err(error.into()),
        }
    }

    /// Resolve an ID or session to evidence this Host actually produced.
    pub async fn resolve_produced_receipt_ref(
        &self,
        host: &HostName,
        reference: &str,
    ) -> anyhow::Result<ReceiptRef> {
        match self.resolve_receipt(host, reference).await {
            Ok(entry)
                if matches!(
                    entry.provenance,
                    arena0_api::ReceiptProvenance::Produced | arena0_api::ReceiptProvenance::Both
                ) =>
            {
                Ok(ReceiptRef::Produced(entry.session_id))
            }
            Ok(_) => bail!("receipt {reference} was imported but not produced by this Host"),
            Err(ResolveError::NotFound { .. }) => self.resolve_session_ref(host, reference).await,
            Err(error) => Err(error.into()),
        }
    }

    /// Resolve this Host's own publication without selecting imported reports.
    pub async fn resolve_session_ref(
        &self,
        host: &HostName,
        reference: &str,
    ) -> anyhow::Result<ReceiptRef> {
        Ok(ReceiptRef::Produced(
            self.resolve_session(host, reference).await?,
        ))
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
    fn prefixes_resolve_case_insensitively_with_exact_match_precedence() {
        for (prefix, expected) in [
            ("a0", PrefixOutcome::Unique(1)),
            ("A0B3", PrefixOutcome::Unique(1)),
            ("7c1e", PrefixOutcome::Ambiguous(vec![0, 2])),
            ("dead", PrefixOutcome::None),
            ("", PrefixOutcome::None),
        ] {
            assert_eq!(
                resolve_prefix(prefix, &ids()),
                expected,
                "prefix: {prefix:?}"
            );
        }

        let candidates = vec!["7c1e".to_string(), "7c1e0a1b".to_string()];
        assert_eq!(
            resolve_prefix("7c1e", &candidates),
            PrefixOutcome::Unique(0)
        );
    }
    fn serve_responses(
        responses: Vec<(HostRequest, arena0_api::Response)>,
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
                let request: arena0_api::Request = arena0_api::frame::read_frame(&mut stream)
                    .await
                    .unwrap()
                    .unwrap();
                assert_eq!(
                    request,
                    arena0_api::Request::Host {
                        host: "host-01".into(),
                        request: expected
                    }
                );
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
            kind: arena0_protocol::ReceiptKind::Receipt,
            program_id: arena0_program::ProgramHash([3; 32]),
            completed: true,
            provenance: arena0_api::ReceiptProvenance::Produced,
        }
    }

    #[tokio::test]
    async fn produced_receipt_resolution_preserves_local_provenance() {
        let mut entry = receipt(&"ab".repeat(32));
        entry.provenance = arena0_api::ReceiptProvenance::Both;
        let (_dir, client, task) = serve_responses(vec![(
            HostRequest::ReceiptList,
            Ok(ResponseOk::ReceiptList(vec![entry.clone()])),
        )]);
        assert_eq!(
            client
                .resolve_produced_receipt_ref(&HostName::default(), &entry.receipt_id)
                .await
                .unwrap(),
            ReceiptRef::Produced(entry.session_id)
        );
        task.await.unwrap();
        entry.provenance = arena0_api::ReceiptProvenance::Imported;
        let (_dir, client, task) = serve_responses(vec![(
            HostRequest::ReceiptList,
            Ok(ResponseOk::ReceiptList(vec![entry.clone()])),
        )]);
        assert!(
            client
                .resolve_produced_receipt_ref(&HostName::default(), &entry.receipt_id)
                .await
                .unwrap_err()
                .to_string()
                .contains("not produced")
        );
        task.await.unwrap();
    }

    #[tokio::test]
    async fn only_a_missing_receipt_falls_back_to_session_resolution() {
        let responses = vec![
            (
                HostRequest::ReceiptList,
                Ok(ResponseOk::ReceiptList(vec![
                    receipt("ab01"),
                    receipt("ab02"),
                ])),
            ),
            (
                HostRequest::ReceiptList,
                Err(arena0_api::ApiError::new(
                    arena0_api::ApiErrorCode::Storage,
                    "unavailable",
                )),
            ),
            (HostRequest::ReceiptList, Ok(ResponseOk::Ack)),
        ];
        let (_dir, client, task) = serve_responses(responses);

        let ambiguous = client
            .resolve_receipt_ref(&HostName::default(), "ab")
            .await
            .unwrap_err();
        assert!(matches!(
            ambiguous.downcast_ref::<ResolveError>(),
            Some(ResolveError::Ambiguous { .. })
        ));

        let request = client
            .resolve_receipt_ref(&HostName::default(), "ab")
            .await
            .unwrap_err();
        assert!(matches!(
            request.downcast_ref::<ResolveError>(),
            Some(ResolveError::Request(_))
        ));

        let unexpected = client
            .resolve_receipt_ref(&HostName::default(), "ab")
            .await
            .unwrap_err();
        assert!(matches!(
            unexpected.downcast_ref::<ResolveError>(),
            Some(ResolveError::UnexpectedResponse)
        ));
        task.await.unwrap();
    }

    #[tokio::test]
    async fn missing_receipt_falls_back_to_local_session() {
        let session = SessionHash([1; 32]);
        let (_dir, client, task) = serve_responses(vec![(
            HostRequest::ReceiptList,
            Ok(ResponseOk::ReceiptList(vec![])),
        )]);
        let key = client
            .resolve_receipt_ref(&HostName::default(), &session.to_string())
            .await
            .unwrap();
        assert_eq!(key, ReceiptRef::Produced(session));
        task.await.unwrap();
    }
    #[tokio::test]
    async fn transport_failure_remains_a_request_error() {
        let dir = tempfile::tempdir().unwrap();
        let client = DaemonClient::new(dir.path().join("absent.sock"));
        let error = client
            .resolve_receipt_ref(&HostName::default(), "ab")
            .await
            .unwrap_err();
        assert!(matches!(
            error.downcast_ref::<ResolveError>(),
            Some(ResolveError::Request(_))
        ));
        assert!(crate::proto::is_connect_error(&error));
    }
}
