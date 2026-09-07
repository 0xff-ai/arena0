//! CLI-local binding between a harness context and one daemon Host namespace.

use std::env::VarError;

use anyhow::bail;
use arena0_home::HostName;

const MAX_CONTEXT_BYTES: usize = 120;

/// The validated Host namespace assigned to one agent context.
#[derive(Debug)]
pub(crate) struct AgentContext {
    host: HostName,
}

impl AgentContext {
    /// Parse and map `harness:session[:agent]` to a stable Host name.
    pub(crate) fn parse(raw: &str) -> anyhow::Result<Self> {
        if raw.len() > MAX_CONTEXT_BYTES {
            bail!("arena0 context must be at most {MAX_CONTEXT_BYTES} UTF-8 bytes");
        }
        if raw.chars().any(char::is_control) {
            bail!("arena0 context must not contain control characters");
        }

        let parts: Vec<&str> = raw.split(':').collect();
        if !(parts.len() == 2 || parts.len() == 3) || parts.iter().any(|part| part.is_empty()) {
            bail!("arena0 context must have the form harness:session[:agent]");
        }

        // ponytail: derive the namespace directly; no separate binding store.
        let encoded: String = raw.bytes().map(|byte| format!("{byte:02x}")).collect();
        let host = format!("agent-{encoded}");
        Ok(Self {
            host: host.parse()?,
        })
    }

    /// Read the harness context once, with Codex's thread id as a fallback.
    ///
    /// A present but invalid variable is an error and never falls through to
    /// the fallback.
    pub(crate) fn from_env() -> anyhow::Result<Option<Self>> {
        Self::from_env_with(|name| std::env::var(name))
    }

    /// Borrow the Host namespace selected by this context.
    pub(crate) fn host(&self) -> &HostName {
        &self.host
    }

    fn from_env_with<F>(mut read: F) -> anyhow::Result<Option<Self>>
    where
        F: FnMut(&str) -> Result<String, VarError>,
    {
        match read("ARENA0_CONTEXT") {
            Ok(value) => Self::parse(&value).map(Some),
            Err(VarError::NotUnicode(_)) => {
                bail!("ARENA0_CONTEXT must contain valid UTF-8")
            }
            Err(VarError::NotPresent) => match read("CODEX_THREAD_ID") {
                Ok(value) => Self::parse(&format!("codex:{value}")).map(Some),
                Err(VarError::NotUnicode(_)) => {
                    bail!("CODEX_THREAD_ID must contain valid UTF-8")
                }
                Err(VarError::NotPresent) => Ok(None),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;

    fn injected(
        context: Option<Result<String, VarError>>,
        thread: Option<Result<String, VarError>>,
    ) -> anyhow::Result<Option<AgentContext>> {
        let mut context = Some(context.unwrap_or(Err(VarError::NotPresent)));
        let mut thread = Some(thread.unwrap_or(Err(VarError::NotPresent)));
        AgentContext::from_env_with(|name| match name {
            "ARENA0_CONTEXT" => context.take().expect("context read once"),
            "CODEX_THREAD_ID" => thread.take().expect("thread id read once"),
            _ => Err(VarError::NotPresent),
        })
    }

    #[test]
    fn parse_validates_parts_and_derives_lowercase_hex_host() {
        let context = AgentContext::parse("harness:session:agent").unwrap();
        assert_eq!(
            context.host().as_str(),
            "agent-6861726e6573733a73657373696f6e3a6167656e74"
        );

        for value in ["", "harness", ":session", "harness:", "harness::agent"] {
            assert!(AgentContext::parse(value).is_err(), "accepted {value:?}");
        }
        assert!(AgentContext::parse("harness:session:agent:extra").is_err());
        assert!(AgentContext::parse("harness:\nagent").is_err());
        assert!(AgentContext::parse(&format!("h:{}", "x".repeat(118))).is_ok());
        assert!(AgentContext::parse(&format!("h:{}", "x".repeat(119))).is_err());
        assert_ne!(
            AgentContext::parse("h:é").unwrap().host(),
            AgentContext::parse("h:e\u{301}").unwrap().host()
        );
        assert_ne!(
            AgentContext::parse("h:A").unwrap().host(),
            AgentContext::parse("h:a").unwrap().host()
        );
    }

    #[test]
    fn environment_precedence_rejects_invalid_present_values() {
        let context = injected(
            Some(Ok("harness:session".into())),
            Some(Ok("ignored".into())),
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            context.host().as_str(),
            "agent-6861726e6573733a73657373696f6e"
        );

        let fallback = injected(None, Some(Ok("thread".into()))).unwrap().unwrap();
        assert_eq!(fallback.host().as_str(), "agent-636f6465783a746872656164");
        assert!(injected(None, None).unwrap().is_none());
        assert!(injected(Some(Ok(String::new())), Some(Ok("thread".into()))).is_err());
        assert!(
            injected(
                Some(Err(VarError::NotUnicode(OsString::from("invalid")))),
                Some(Ok("thread".into())),
            )
            .is_err()
        );
        assert!(
            injected(
                None,
                Some(Err(VarError::NotUnicode(OsString::from("invalid")))),
            )
            .is_err()
        );
    }
}
