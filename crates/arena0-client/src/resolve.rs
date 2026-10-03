//! Daemon-side indexed resolution for exec, session, and receipt ids.

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
    /// The daemon could not complete the resolve request.
    #[error(transparent)]
    Request(#[from] anyhow::Error),
    /// The daemon returned a success payload for a different operation.
    #[error("unexpected response to resolve")]
    UnexpectedResponse,
}

impl DaemonClient {
    /// Resolve an execution prefix through the daemon's indexed lookup.
    pub async fn resolve_exec(&self, host: &HostName, prefix: &str) -> anyhow::Result<ExecId> {
        // Preserve the direct parse for full ids, including ids not resident here.
        if let Ok(id) = prefix.parse::<ExecId>() {
            return Ok(id);
        }
        let ResponseOk::Resolved(result) = self
            .call_host(
                host,
                &HostRequest::Resolve {
                    kind: arena0_api::RefKind::Exec,
                    reference: prefix.into(),
                },
            )
            .await?
        else {
            bail!("unexpected response to resolve");
        };
        match result {
            arena0_api::Resolved::Exec { exec_id } => Ok(exec_id),
            arena0_api::Resolved::None => bail!("no execution matches '{prefix}'"),
            arena0_api::Resolved::Ambiguous { candidates, .. } => {
                let candidates = candidates
                    .into_iter()
                    .map(|id| id[..8.min(id.len())].to_owned())
                    .collect::<Vec<_>>();
                bail!(
                    "'{prefix}' is ambiguous; candidates: {}",
                    candidates.join(", ")
                )
            }
            _ => bail!("unexpected response to resolve"),
        }
    }

    /// Resolve a session prefix across committed activations, executions and
    /// receipts through the daemon's indexed lookup.
    pub async fn resolve_session(
        &self,
        host: &HostName,
        prefix: &str,
    ) -> anyhow::Result<SessionHash> {
        if let Ok(id) = prefix.parse::<SessionHash>() {
            return Ok(id);
        }
        let ResponseOk::Resolved(result) = self
            .call_host(
                host,
                &HostRequest::Resolve {
                    kind: arena0_api::RefKind::Session,
                    reference: prefix.into(),
                },
            )
            .await?
        else {
            bail!("unexpected response to resolve");
        };
        match result {
            arena0_api::Resolved::Session { session_id } => Ok(session_id),
            arena0_api::Resolved::None => bail!("no session matches '{prefix}'"),
            arena0_api::Resolved::Ambiguous { candidates, .. } => {
                let candidates = candidates
                    .into_iter()
                    .map(|id| id[..8.min(id.len())].to_owned())
                    .collect::<Vec<_>>();
                bail!(
                    "'{prefix}' is ambiguous; candidates: {}",
                    candidates.join(", ")
                )
            }
            _ => bail!("unexpected response to resolve"),
        }
    }

    /// Resolve a resident receipt id or prefix to its list entry. Its session
    /// and provenance determine whether it can be used as local publication.
    pub async fn resolve_receipt(
        &self,
        host: &HostName,
        prefix: &str,
    ) -> Result<arena0_api::ReceiptListEntry, ResolveError> {
        let ResponseOk::Resolved(result) = self
            .call_host(
                host,
                &HostRequest::Resolve {
                    kind: arena0_api::RefKind::Receipt,
                    reference: prefix.into(),
                },
            )
            .await?
        else {
            return Err(ResolveError::UnexpectedResponse);
        };
        match result {
            arena0_api::Resolved::Receipt { entry } => Ok(entry),
            arena0_api::Resolved::None => Err(ResolveError::NotFound {
                reference: prefix.into(),
            }),
            arena0_api::Resolved::Ambiguous { candidates, .. } => Err(ResolveError::Ambiguous {
                reference: prefix.into(),
                candidates: candidates
                    .into_iter()
                    .map(|id| id[..8.min(id.len())].to_owned())
                    .collect(),
            }),
            _ => Err(ResolveError::UnexpectedResponse),
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
            HostRequest::Resolve {
                kind: arena0_api::RefKind::Receipt,
                reference: entry.receipt_id.clone(),
            },
            Ok(ResponseOk::Resolved(arena0_api::Resolved::Receipt {
                entry: entry.clone(),
            })),
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
            HostRequest::Resolve {
                kind: arena0_api::RefKind::Receipt,
                reference: entry.receipt_id.clone(),
            },
            Ok(ResponseOk::Resolved(arena0_api::Resolved::Receipt {
                entry: entry.clone(),
            })),
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
                HostRequest::Resolve {
                    kind: arena0_api::RefKind::Receipt,
                    reference: "ab".into(),
                },
                Ok(ResponseOk::Resolved(arena0_api::Resolved::Ambiguous {
                    candidates: vec!["ab01".into(), "ab02".into()],
                    matches: 2,
                })),
            ),
            (
                HostRequest::Resolve {
                    kind: arena0_api::RefKind::Receipt,
                    reference: "ab".into(),
                },
                Err(arena0_api::ApiError::new(
                    arena0_api::ApiErrorCode::Storage,
                    "unavailable",
                )),
            ),
            (
                HostRequest::Resolve {
                    kind: arena0_api::RefKind::Receipt,
                    reference: "ab".into(),
                },
                Ok(ResponseOk::Ack),
            ),
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
            HostRequest::Resolve {
                kind: arena0_api::RefKind::Receipt,
                reference: session.to_string(),
            },
            Ok(ResponseOk::Resolved(arena0_api::Resolved::None)),
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
