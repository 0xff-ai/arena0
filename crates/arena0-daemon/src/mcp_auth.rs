//! Daemon-owned JWT authentication for the MCP endpoint.
//!
//! The signing key is one random, owner-only file in the daemon home. It is
//! deliberately separate from every Host identity: MCP access authenticates a
//! caller to one already-owned Host, while protocol signing remains in the
//! Host keystore.

use std::fmt;
use std::fs;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::Context as _;
use arena0_home::HostName;
use arena0_protocol::PeerId;
use jsonwebtoken::{Algorithm, DecodingKey, EncodingKey, Header, Validation, decode, encode};
use rand::RngCore as _;
use rand::rngs::OsRng;
use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

pub(crate) const DEFAULT_ACCESS_TOKEN_LIFETIME: Duration = Duration::from_secs(86_400);
const ISSUER: &str = "arena0d";
const AUDIENCE: &str = "arena0-mcp";
const TOKEN_TYPE: &str = "arena0-mcp";
const TOKEN_SCHEMA: u16 = 1;
const SIGNING_KEY_BYTES: usize = 32;

/// A supplied token whose Debug representation never contains the bearer
/// material. Keep this type at the boundary so accidental tracing cannot leak
/// the raw JWT.
#[derive(Clone, Eq, PartialEq)]
pub(crate) struct RawToken(String);

impl RawToken {
    pub(crate) fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for RawToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("<redacted JWT>")
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Claims {
    iss: String,
    aud: String,
    typ: String,
    schema: u16,
    sub: String,
    peer_id: String,
    iat: u64,
    exp: u64,
}

/// Claims after signature, fixed-claim, time, and identity parsing.
///
/// Fields remain private so callers cannot construct an authenticated value
/// from unverified JSON. The daemon is the only owner of this transition.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct VerifiedClaims {
    host_name: HostName,
    peer_id: PeerId,
    issued_at: u64,
    expires_at: u64,
}

impl VerifiedClaims {
    pub(crate) fn host_name(&self) -> &HostName {
        &self.host_name
    }

    pub(crate) const fn peer_id(&self) -> PeerId {
        self.peer_id
    }
}

#[derive(Clone)]
pub(crate) struct IssuedToken {
    pub(crate) token: RawToken,
    pub(crate) peer_id: PeerId,
    pub(crate) expires_at: u64,
    pub(crate) renew_after: u64,
}

impl fmt::Debug for IssuedToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("IssuedToken")
            .field("token", &self.token)
            .field("peer_id", &self.peer_id)
            .field("expires_at", &self.expires_at)
            .field("renew_after", &self.renew_after)
            .finish()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AuthError {
    InvalidToken,
    TokenExpired,
    HostUnavailable,
}

impl fmt::Display for AuthError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::InvalidToken => "invalid MCP token",
            Self::TokenExpired => "MCP token expired",
            Self::HostUnavailable => "authenticated Host unavailable",
        })
    }
}

impl std::error::Error for AuthError {}

/// The daemon-wide JWT signer/verifier.
pub(crate) struct McpAuth {
    encoding: EncodingKey,
    decoding: DecodingKey,
    lifetime: Duration,
}

impl fmt::Debug for McpAuth {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("McpAuth")
            .field("lifetime", &self.lifetime)
            .field("key", &"<redacted>")
            .finish()
    }
}

impl McpAuth {
    pub(crate) fn load_or_create(path: &Path, lifetime: Duration) -> anyhow::Result<Self> {
        anyhow::ensure!(
            lifetime.as_secs() > 0,
            "MCP access token lifetime must be greater than zero"
        );
        anyhow::ensure!(
            now_seconds().checked_add(lifetime.as_secs()).is_some(),
            "MCP access token lifetime overflows Unix time"
        );
        let key = match fs::symlink_metadata(path) {
            Ok(metadata) => {
                crate::store::keystore::ensure_private_regular(path, &metadata, "MCP signing key")?;
                read_key(path)?
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let key = generate_key()?;
                crate::store::keystore::publish_new_private(path, &key[..], "MCP signing key")?;
                key
            }
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("inspect MCP signing key {}", path.display()));
            }
        };
        let encoding = EncodingKey::from_secret(&key[..]);
        let decoding = DecodingKey::from_secret(&key[..]);
        Ok(Self {
            encoding,
            decoding,
            lifetime,
        })
    }

    pub(crate) fn issue(
        &self,
        host_name: &HostName,
        peer_id: PeerId,
    ) -> anyhow::Result<IssuedToken> {
        self.issue_at(host_name, peer_id, now_seconds())
    }

    fn issue_at(
        &self,
        host_name: &HostName,
        peer_id: PeerId,
        issued_at: u64,
    ) -> anyhow::Result<IssuedToken> {
        let lifetime = self.lifetime.as_secs();
        anyhow::ensure!(
            lifetime > 0,
            "MCP access token lifetime must be greater than zero"
        );
        let expires_at = issued_at
            .checked_add(lifetime)
            .ok_or_else(|| anyhow::anyhow!("MCP access token lifetime overflows Unix time"))?;
        let claims = Claims {
            iss: ISSUER.to_owned(),
            aud: AUDIENCE.to_owned(),
            typ: TOKEN_TYPE.to_owned(),
            schema: TOKEN_SCHEMA,
            sub: host_name.to_string(),
            peer_id: peer_id.to_string(),
            iat: issued_at,
            exp: expires_at,
        };
        let header = Header::new(Algorithm::HS256);
        let token = encode(&header, &claims, &self.encoding)
            .map_err(|_| anyhow::anyhow!("sign MCP access token"))?;
        Ok(IssuedToken {
            token: RawToken::new(token),
            peer_id,
            expires_at,
            renew_after: issued_at + lifetime / 2,
        })
    }

    pub(crate) fn verify(&self, token: RawToken) -> Result<VerifiedClaims, AuthError> {
        self.verify_at(token, now_seconds())
    }

    fn verify_at(&self, token: RawToken, now: u64) -> Result<VerifiedClaims, AuthError> {
        let mut validation = Validation::new(Algorithm::HS256);
        validation.leeway = 0;
        // jsonwebtoken validates against its own wall clock. We still use its
        // signature, algorithm, issuer, audience, and required-claim checks,
        // but perform exp/iat checks against the dispatch clock below so the
        // expiry boundary is explicit and testable.
        validation.validate_exp = false;
        validation.set_issuer(&[ISSUER]);
        validation.set_audience(&[AUDIENCE]);
        validation.set_required_spec_claims(&["exp", "iss", "aud", "sub"]);

        let decoded =
            decode::<Claims>(token.as_str(), &self.decoding, &validation).map_err(|error| {
                if matches!(
                    error.kind(),
                    jsonwebtoken::errors::ErrorKind::ExpiredSignature
                ) {
                    AuthError::TokenExpired
                } else {
                    AuthError::InvalidToken
                }
            })?;
        if decoded.header.alg != Algorithm::HS256 || decoded.header.typ.as_deref() != Some("JWT") {
            return Err(AuthError::InvalidToken);
        }
        let claims = decoded.claims;
        if claims.iss != ISSUER
            || claims.aud != AUDIENCE
            || claims.typ != TOKEN_TYPE
            || claims.schema != TOKEN_SCHEMA
            || claims.exp <= now
            || claims.iat > now
            || claims.exp <= claims.iat
        {
            return if claims.exp <= now {
                Err(AuthError::TokenExpired)
            } else {
                Err(AuthError::InvalidToken)
            };
        }
        let host_name = claims.sub.parse().map_err(|_| AuthError::InvalidToken)?;
        let peer_id = claims
            .peer_id
            .parse()
            .map_err(|_| AuthError::InvalidToken)?;
        Ok(VerifiedClaims {
            host_name,
            peer_id,
            issued_at: claims.iat,
            expires_at: claims.exp,
        })
    }
}

fn now_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock is before Unix epoch")
        .as_secs()
}

fn generate_key() -> anyhow::Result<Zeroizing<[u8; SIGNING_KEY_BYTES]>> {
    let mut key = Zeroizing::new([0u8; SIGNING_KEY_BYTES]);
    OsRng
        .try_fill_bytes(&mut *key)
        .context("generate MCP signing key from the operating-system CSPRNG")?;
    anyhow::ensure!(
        key.iter().any(|byte| *byte != 0),
        "operating-system CSPRNG returned an all-zero MCP signing key"
    );
    Ok(key)
}

fn read_key(path: &Path) -> anyhow::Result<Zeroizing<[u8; SIGNING_KEY_BYTES]>> {
    let bytes = Zeroizing::new(
        fs::read(path).with_context(|| format!("read MCP signing key {}", path.display()))?,
    );
    anyhow::ensure!(
        bytes.len() == SIGNING_KEY_BYTES,
        "MCP signing key must be exactly {SIGNING_KEY_BYTES} bytes"
    );
    let mut key = Zeroizing::new([0u8; SIGNING_KEY_BYTES]);
    key.copy_from_slice(&bytes);
    anyhow::ensure!(
        key.iter().any(|byte| *byte != 0),
        "MCP signing key must not be all zero"
    );
    Ok(key)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn signer() -> (tempfile::TempDir, McpAuth) {
        let dir = tempfile::tempdir().expect("key directory");
        let auth =
            McpAuth::load_or_create(&dir.path().join("mcp-signing.key"), Duration::from_secs(20))
                .expect("signer");
        (dir, auth)
    }

    #[test]
    fn token_claims_bind_host_and_expire_strictly() {
        let (_dir, auth) = signer();
        let host: HostName = "host-01".parse().unwrap();
        let peer = PeerId([7; 32]);
        let issued = auth.issue_at(&host, peer, 100).unwrap();
        let verified = auth.verify_at(issued.token.clone(), 119).unwrap();
        assert_eq!(verified.host_name(), &host);
        assert_eq!(verified.peer_id(), peer);
        assert_eq!(verified.expires_at, 120);
        assert_eq!(verified.issued_at, 100);
        assert_eq!(
            auth.verify_at(issued.token, 120),
            Err(AuthError::TokenExpired)
        );
    }

    #[test]
    fn signing_key_is_private_and_corruption_fails_closed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mcp-signing.key");
        let _auth = McpAuth::load_or_create(&path, Duration::from_secs(1)).unwrap();
        assert_eq!(
            fs::symlink_metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        fs::write(&path, [1u8; 3]).unwrap();
        assert!(McpAuth::load_or_create(&path, Duration::from_secs(1)).is_err());
    }

    #[test]
    fn verification_rejects_wrong_keys_algorithms_and_claims() {
        let (_dir, auth) = signer();
        let (_other_dir, other) = signer();
        let claims = serde_json::json!({
            "iss": ISSUER, "aud": AUDIENCE, "typ": TOKEN_TYPE,
            "schema": TOKEN_SCHEMA, "sub": "host-01",
            "peer_id": PeerId([7; 32]).to_string(), "iat": 100, "exp": 120,
        });
        let signed = |value: &serde_json::Value, algorithm, key: &EncodingKey| {
            RawToken::new(encode(&Header::new(algorithm), value, key).unwrap())
        };
        assert!(
            auth.verify_at(signed(&claims, Algorithm::HS256, &auth.encoding), 110)
                .is_ok()
        );
        assert_eq!(
            auth.verify_at(signed(&claims, Algorithm::HS256, &other.encoding), 110),
            Err(AuthError::InvalidToken)
        );
        assert_eq!(
            auth.verify_at(signed(&claims, Algorithm::HS384, &auth.encoding), 110),
            Err(AuthError::InvalidToken)
        );
        for (field, value) in [
            ("iss", serde_json::json!("other")),
            ("aud", serde_json::json!("other")),
            ("typ", serde_json::json!("other")),
            ("schema", serde_json::json!(2)),
            ("sub", serde_json::json!("../other")),
            ("peer_id", serde_json::json!("invalid")),
            ("iat", serde_json::json!(111)),
        ] {
            let mut invalid = claims.clone();
            invalid[field] = value;
            assert_eq!(
                auth.verify_at(signed(&invalid, Algorithm::HS256, &auth.encoding), 110),
                Err(AuthError::InvalidToken),
                "accepted invalid {field}"
            );
        }
        for field in [
            "iss", "aud", "typ", "schema", "sub", "peer_id", "iat", "exp",
        ] {
            let mut invalid = claims.clone();
            invalid.as_object_mut().unwrap().remove(field);
            assert_eq!(
                auth.verify_at(signed(&invalid, Algorithm::HS256, &auth.encoding), 110),
                Err(AuthError::InvalidToken),
                "accepted missing {field}"
            );
        }
    }

    #[test]
    fn reloading_signing_key_preserves_issued_tokens() {
        let (dir, auth) = signer();
        let token = auth
            .issue(&"host-01".parse().unwrap(), PeerId([7; 32]))
            .unwrap()
            .token;
        let before = auth.verify(token.clone()).unwrap();
        drop(auth);
        let reopened =
            McpAuth::load_or_create(&dir.path().join("mcp-signing.key"), Duration::from_secs(20))
                .unwrap();
        assert_eq!(reopened.verify(token).unwrap(), before);
    }

    #[test]
    fn raw_tokens_are_redacted_in_debug() {
        let raw = RawToken::new("header.payload.signature");
        assert!(!format!("{raw:?}").contains("header"));
    }
}
