use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

const USER_AGENT: &str = "harness/test";
const START_TIMEOUT: Duration = Duration::from_secs(15);

fn arena0d_binary() -> PathBuf {
    let sibling = std::env::var_os("ARENA0D_BIN")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            let target = Path::new(env!("CARGO_BIN_EXE_arena0"))
                .parent()
                .expect("arena0 test binary should have a target directory");
            let target = if target.file_name().is_some_and(|name| name == "deps") {
                target.parent().expect("deps should have a target parent")
            } else {
                target
            };
            target.join("arena0d")
        });
    assert!(
        sibling.is_file(),
        "arena0d binary is required at {}; build it with `cargo build -p arena0d` before running this integration test",
        sibling.display()
    );
    sibling
}

fn base_command(program: impl AsRef<Path>, home: &Path) -> Command {
    let mut command = Command::new(program.as_ref());
    command
        .env("ARENA0_HOME", home)
        .env_remove("ARENA0_SOCKET")
        .env_remove("ARENA0_CACHE_DIR")
        .env_remove("ARENA0_CONTEXT")
        .env_remove("CODEX_THREAD_ID")
        .env_remove("CODEX_SESSION_ID")
        .env_remove("RUST_LOG");
    command
}

fn invoke(home: &Path, args: &[&str], environment: &[(&str, &str)]) -> Output {
    let mut command = base_command(env!("CARGO_BIN_EXE_arena0"), home);
    command.args(args);
    for (name, value) in environment {
        command.env(name, value);
    }
    command
        .output()
        .unwrap_or_else(|error| panic!("run arena0 {args:?}: {error}"))
}

fn json_output(output: Output, invocation: &str) -> serde_json::Value {
    assert!(
        output.status.success(),
        "{invocation} failed with {}\nstdout: {}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        output.stderr.is_empty(),
        "{invocation} wrote unexpected stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "{invocation} returned invalid JSON ({error}): {}",
            String::from_utf8_lossy(&output.stdout)
        )
    })
}

fn failed(output: Output, invocation: &str) -> String {
    assert!(
        !output.status.success(),
        "{invocation} unexpectedly succeeded\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn hello(home: &Path, context: Option<&str>, user_agent: &str) -> serde_json::Value {
    let environment = context
        .map(|context| vec![("ARENA0_CONTEXT", context)])
        .unwrap_or_default();
    json_output(
        invoke(
            home,
            &["--json", "hello", "--user-agent", user_agent],
            &environment,
        ),
        "arena0 --json hello",
    )
}

fn host_info(
    value: serde_json::Value,
    expected_id: Option<&str>,
    user_agent: &str,
) -> (String, arena0_client::protocol::PeerId) {
    let info: arena0_client::api::HostInfo =
        serde_json::from_value(value).expect("hello should return HostInfo JSON");
    let id = info.id;
    if let Some(expected_id) = expected_id {
        assert_eq!(id, expected_id);
    }
    assert_eq!(info.user_agent.as_deref(), Some(user_agent));
    (id, info.peer_id)
}

fn host_count(home: &Path) -> usize {
    let value = json_output(
        invoke(home, &["--json", "status"], &[]),
        "arena0 --json status",
    );
    assert_eq!(value["reachable"], true);
    let daemon = value["daemon"]
        .as_object()
        .expect("status should contain daemon");
    assert!(
        daemon
            .get("socket")
            .and_then(serde_json::Value::as_str)
            .is_some()
    );
    assert!(!daemon.contains_key("mcp_endpoint"));
    value["hosts"]
        .as_array()
        .expect("status should contain hosts")
        .len()
}

fn assert_context_host_operations(
    home: &Path,
    context: &str,
    peer_id: arena0_client::protocol::PeerId,
) {
    let environment = [("ARENA0_CONTEXT", context)];
    let programs = json_output(
        invoke(home, &["--json", "program", "list"], &environment),
        "arena0 --json program list",
    );
    assert!(
        !programs["programs"]
            .as_array()
            .expect("program list should contain programs")
            .is_empty(),
        "a bootstrapped Host should expose its built-in programs"
    );

    let identities = json_output(
        invoke(home, &["--json", "identity", "list"], &environment),
        "arena0 --json identity list",
    );
    let identities = identities["identities"]
        .as_array()
        .expect("identity list should contain identities");
    assert_eq!(identities.len(), 1);
    assert_eq!(identities[0]["peer_id"], peer_id.to_string());
    assert_eq!(identities[0]["active"], true);

    let executions = json_output(
        invoke(home, &["--json", "exec", "list"], &environment),
        "arena0 --json exec list",
    );
    assert!(
        executions["executions"]
            .as_array()
            .expect("execution list should contain executions")
            .is_empty()
    );
}

struct DaemonGuard {
    child: Child,
    home: PathBuf,
}

impl DaemonGuard {
    fn start(home: &Path) -> Self {
        let mut command = base_command(arena0d_binary(), home);
        let child = command
            .args(["--no-hosts", "--mcp-listen", "127.0.0.1:0"])
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn real arena0d");
        let mut daemon = Self {
            child,
            home: home.to_owned(),
        };
        daemon.wait(true);
        daemon
    }

    fn wait(&mut self, starting: bool) {
        let deadline = Instant::now() + START_TIMEOUT;
        loop {
            if let Some(status) = self.child.try_wait().expect("poll arena0d") {
                if starting {
                    panic!(
                        "arena0d exited during startup with {status}\nstderr: {}",
                        self.stderr()
                    );
                }
                return;
            }
            if starting {
                let output = invoke(&self.home, &["--json", "status"], &[]);
                if output.status.success()
                    && serde_json::from_slice::<serde_json::Value>(&output.stdout)
                        .ok()
                        .is_some_and(|value| value["reachable"] == true)
                {
                    return;
                }
            }
            if Instant::now() >= deadline {
                let _ = self.child.kill();
                let _ = self.child.wait();
                let phase = if starting { "become ready" } else { "stop" };
                panic!(
                    "arena0d did not {phase} within {START_TIMEOUT:?}\nstderr: {}",
                    self.stderr()
                );
            }
            thread::sleep(Duration::from_millis(25));
        }
    }

    fn stop(&mut self) {
        let output = invoke(&self.home, &["--json", "stop"], &[]);
        assert!(
            output.status.success(),
            "arena0 stop failed\nstdout: {}\nstderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        self.wait(false);
    }

    fn stderr(&mut self) -> String {
        self.child
            .stderr
            .as_mut()
            .map_or_else(String::new, |stderr| {
                let mut text = String::new();
                let _ = stderr.read_to_string(&mut text);
                text
            })
    }
}

impl Drop for DaemonGuard {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
        }
        let _ = self.child.wait();
    }
}

#[test]
fn context_hello_is_deterministic_concurrent_distinct_and_persistent() {
    let home = tempfile::tempdir().expect("temporary arena0 home");
    let mut daemon = DaemonGuard::start(home.path());
    let context = "harness:shared";
    let expected_id = "agent-6861726e6573733a736861726564";
    let first_home = home.path().to_owned();
    let second_home = home.path().to_owned();
    let first = thread::spawn(move || hello(&first_home, Some(context), USER_AGENT));
    let second = thread::spawn(move || hello(&second_home, Some(context), USER_AGENT));
    let first = first.join().expect("first concurrent hello");
    let second = second.join().expect("second concurrent hello");
    let (first_id, first_peer) = host_info(first, Some(expected_id), USER_AGENT);
    let (second_id, second_peer) = host_info(second, Some(expected_id), USER_AGENT);
    assert_eq!(first_id, second_id);
    assert_eq!(first_peer, second_peer);
    assert_eq!(host_count(home.path()), 1);

    let repeated = hello(home.path(), Some(context), USER_AGENT);
    let (repeated_id, repeated_peer) = host_info(repeated, None, USER_AGENT);
    assert_eq!((repeated_id, repeated_peer), (first_id.clone(), first_peer));

    for context in ["harness:alpha", "harness:beta"] {
        let value = hello(home.path(), Some(context), USER_AGENT);
        let (id, peer) = host_info(value, None, USER_AGENT);
        assert_ne!(id, first_id);
        assert_ne!(peer, first_peer);
    }
    assert_eq!(host_count(home.path()), 3);
    assert_context_host_operations(home.path(), context, first_peer);

    daemon.stop();
    let mut restarted = DaemonGuard::start(home.path());
    let reopened = hello(home.path(), Some(context), USER_AGENT);
    let (reopened_id, reopened_peer) = host_info(reopened, None, USER_AGENT);
    assert_eq!(reopened_id, first_id);
    assert_eq!(reopened_peer, first_peer);
    assert_eq!(host_count(home.path()), 1);
    assert_context_host_operations(home.path(), context, first_peer);
    restarted.stop();
}

#[test]
fn context_commands_require_an_existing_context_and_never_provision() {
    let home = tempfile::tempdir().expect("temporary arena0 home");
    let mut daemon = DaemonGuard::start(home.path());
    let context = "harness:before";

    let error = failed(
        invoke(
            home.path(),
            &["--json", "program", "list"],
            &[("ARENA0_CONTEXT", context)],
        ),
        "arena0 --json program list before hello",
    );
    assert!(error.contains("Host"), "unexpected context error: {error}");
    assert_eq!(host_count(home.path()), 0);

    let error = failed(
        invoke(
            home.path(),
            &["--json", "hello", "--user-agent", USER_AGENT],
            &[],
        ),
        "arena0 --json hello without context",
    );
    assert!(
        error.contains("ARENA0_CONTEXT"),
        "unexpected missing context error: {error}"
    );
    assert_eq!(host_count(home.path()), 0);

    let error = failed(
        invoke(
            home.path(),
            &["--json", "hello", "--user-agent", USER_AGENT],
            &[
                ("ARENA0_CONTEXT", "invalid-context"),
                ("CODEX_THREAD_ID", "fallback-thread"),
            ],
        ),
        "arena0 --json hello with invalid explicit context",
    );
    assert!(
        error.contains("context"),
        "unexpected invalid context error: {error}"
    );
    assert_eq!(host_count(home.path()), 0);

    let error = failed(
        invoke(
            home.path(),
            &[
                "--json",
                "--host",
                "host-01",
                "hello",
                "--user-agent",
                USER_AGENT,
            ],
            &[("ARENA0_CONTEXT", context)],
        ),
        "arena0 --json --host host-01 hello",
    );
    assert!(
        error.contains("--host"),
        "unexpected hello --host error: {error}"
    );

    let error = failed(
        invoke(
            home.path(),
            &["--json", "--tmp", "hello", "--user-agent", USER_AGENT],
            &[("ARENA0_CONTEXT", context)],
        ),
        "arena0 --json --tmp hello",
    );
    assert!(
        error.contains("--tmp"),
        "unexpected hello --tmp error: {error}"
    );
    assert_eq!(host_count(home.path()), 0);
    daemon.stop();
}

#[test]
fn codex_context_fallback_and_explicit_host_override_are_observable() {
    let home = tempfile::tempdir().expect("temporary arena0 home");
    let mut daemon = DaemonGuard::start(home.path());
    let fallback_context = "codex:thread-42";
    let fallback = json_output(
        invoke(
            home.path(),
            &["--json", "hello", "--user-agent", USER_AGENT],
            &[
                ("CODEX_THREAD_ID", "thread-42"),
                ("CODEX_SESSION_ID", "ignored"),
            ],
        ),
        "arena0 --json hello from CODEX_THREAD_ID",
    );
    let fallback_id = "agent-636f6465783a7468726561642d3432";
    let (_, fallback_peer) = host_info(fallback, Some(fallback_id), USER_AGENT);

    let explicit_context = "harness:other";
    let explicit = hello(home.path(), Some(explicit_context), USER_AGENT);
    let (explicit_id, _explicit_peer) = host_info(explicit, None, USER_AGENT);

    let new_identity = json_output(
        invoke(
            home.path(),
            &[
                "--json",
                "--host",
                explicit_id.as_str(),
                "identity",
                "new",
                "override",
            ],
            &[("ARENA0_CONTEXT", fallback_context)],
        ),
        "arena0 --json --host <explicit> identity new",
    );
    assert_eq!(new_identity["label"], "override");

    let selected = json_output(
        invoke(
            home.path(),
            &["--json", "--host", explicit_id.as_str(), "identity", "list"],
            &[("ARENA0_CONTEXT", fallback_context)],
        ),
        "arena0 --json --host <explicit> identity list",
    );
    assert_eq!(selected["identities"].as_array().unwrap().len(), 2);

    let fallback_identities = json_output(
        invoke(
            home.path(),
            &["--json", "identity", "list"],
            &[("ARENA0_CONTEXT", fallback_context)],
        ),
        "arena0 --json identity list from fallback context",
    );
    let fallback_identities = fallback_identities["identities"].as_array().unwrap();
    assert_eq!(fallback_identities.len(), 1);
    assert_eq!(fallback_identities[0]["peer_id"], fallback_peer.to_string());
    assert_eq!(host_count(home.path()), 2);

    daemon.stop();
}
