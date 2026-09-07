use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use arena0_api::HostInfo;
use arena0_home::{Home, HostName};
use arena0_sandbox::WasmtimeEngine;
use arena0_store::Store;
use tempfile::TempDir;
use tokio::task::JoinHandle;

use crate::{Daemon, McpConfig};

const TEST_TIMEOUT: Duration = Duration::from_secs(15);

struct TestDaemon {
    root: TempDir,
    home: Home,
    daemon: Arc<Daemon>,
    serving: JoinHandle<anyhow::Result<()>>,
}

impl TestDaemon {
    async fn new() -> Self {
        let root = tempfile::tempdir().expect("temporary daemon home");
        Self::from_root(root).await
    }

    async fn from_root(root: TempDir) -> Self {
        Self::from_root_with_names(root, Vec::new()).await
    }

    async fn from_root_with_names(root: TempDir, names: Vec<HostName>) -> Self {
        let home = Home::from_root(root.path().to_path_buf()).expect("home");
        let mcp = McpConfig::new(SocketAddr::from(([127, 0, 0, 1], 0)), None).expect("MCP config");
        let engine = Arc::new(WasmtimeEngine::new().expect("sandbox engine"));
        let daemon = Daemon::start(names, mcp, engine, home.clone(), true)
            .await
            .expect("start empty daemon");
        let before_serve = daemon
            .open_host(None, "harness/test".into())
            .await
            .expect_err("startup must reject opens before preparation");
        assert!(
            before_serve
                .to_string()
                .contains("not accepting Host opens")
        );
        let serving = tokio::spawn(Arc::clone(&daemon).serve());
        Self {
            root,
            home,
            daemon,
            serving,
        }
    }

    async fn stop(self) -> TempDir {
        self.daemon.stop().await;
        tokio::time::timeout(TEST_TIMEOUT, self.serving)
            .await
            .expect("daemon serve should finish")
            .expect("daemon serve task should join")
            .expect("daemon serve should stop cleanly");
        self.root
    }
}

async fn open_until_running(
    daemon: &Arc<Daemon>,
    id: Option<&str>,
    user_agent: &str,
) -> anyhow::Result<HostInfo> {
    tokio::time::timeout(TEST_TIMEOUT, async {
        loop {
            match daemon
                .open_host(id.map(str::to_owned), user_agent.to_owned())
                .await
            {
                Err(error) if error.to_string().contains("not accepting Host opens") => {
                    tokio::task::yield_now().await;
                }
                result => return result,
            }
        }
    })
    .await
    .expect("Host open should complete")
}

fn location(home: &Home, id: &str) -> arena0_home::HostLocation {
    let id = id.parse::<HostName>().expect("Host id");
    home.host(&id)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_supplied_id_opens_share_one_ready_peer() {
    let test = TestDaemon::new().await;
    let (first, second) = tokio::join!(
        open_until_running(&test.daemon, Some("shared"), "harness/1"),
        open_until_running(&test.daemon, Some("shared"), "harness/1"),
    );
    let first = first.expect("first open should succeed");
    let second = second.expect("second open should succeed");

    assert_eq!(first.id, "shared");
    assert_eq!(first.peer_id, second.peer_id);
    assert_eq!(first.user_agent, Some("harness/1".to_owned()));
    assert_eq!(second.user_agent, Some("harness/1".to_owned()));
    assert!(test.daemon.service("shared").is_some());
    assert_eq!(test.daemon.services().len(), 1);
    assert!(test.home.socket().exists());

    let _root = test.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lazy_restart_preserves_identity_and_missing_ids_are_distinct() {
    let first = TestDaemon::new().await;
    let expected = open_until_running(&first.daemon, Some("persisted"), "harness/2")
        .await
        .expect("first Host open");
    let root = first.stop().await;

    let second = TestDaemon::from_root(root).await;
    let reopened = open_until_running(&second.daemon, Some("persisted"), "harness/2")
        .await
        .expect("reopen Host");
    assert_eq!(reopened.id, "persisted");
    assert_eq!(reopened.peer_id, expected.peer_id);
    assert_eq!(reopened.user_agent, Some("harness/2".to_owned()));

    let generated_a = open_until_running(&second.daemon, None, "harness/a")
        .await
        .expect("first generated Host");
    let generated_b = open_until_running(&second.daemon, None, "harness/b")
        .await
        .expect("second generated Host");
    assert_ne!(generated_a.id, generated_b.id);
    assert_ne!(generated_a.peer_id, generated_b.peer_id);
    assert_eq!(second.daemon.services().len(), 3);

    let _root = second.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn invalid_id_and_user_agent_do_not_create_state() {
    let test = TestDaemon::new().await;
    let invalid_id = test
        .daemon
        .open_host(Some("../escape".to_owned()), "harness/3".to_owned())
        .await
        .expect_err("invalid Host id should be rejected");
    assert!(invalid_id.to_string().contains("Host name"));

    let invalid_agent = test
        .daemon
        .open_host(Some("rejected".to_owned()), " \t".to_owned())
        .await
        .expect_err("whitespace-only user agent should be rejected");
    assert!(invalid_agent.to_string().contains("user agent"));
    assert!(!test.home.root().join("hosts").exists());

    let healthy = open_until_running(&test.daemon, Some("healthy"), "harness/3")
        .await
        .expect("daemon should remain usable");
    assert_eq!(healthy.id, "healthy");
    assert!(test.daemon.service("healthy").is_some());

    let _root = test.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failed_provisioning_does_not_mutate_keystore_and_retry_recovers() {
    let test = TestDaemon::new().await;

    let locked = location(&test.home, "locked");
    std::fs::create_dir_all(locked.state_dir()).expect("create locked Host state");
    let locked_db = locked.state_dir().join("arena0.sqlite");
    let reservation = Store::reserve(&locked_db).expect("reserve locked Host");
    let locked_result = open_until_running(&test.daemon, Some("locked"), "harness/4").await;
    let locked_error = locked_result.expect_err("another owner should block provisioning");
    assert!(
        format!("{locked_error:#}").contains("already owned"),
        "unexpected lock error: {locked_error:#}"
    );
    let keys_dir = locked.state_dir().join("keys");
    assert!(
        keys_dir.is_dir(),
        "path setup may create the empty keys dir"
    );
    assert!(
        std::fs::read_dir(&keys_dir)
            .expect("read keys dir")
            .next()
            .is_none(),
        "ownership must be acquired before keystore mutation"
    );
    drop(reservation);
    let recovered = open_until_running(&test.daemon, Some("locked"), "harness/4")
        .await
        .expect("retry after releasing owner");
    assert_eq!(recovered.id, "locked");

    let _root = test.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn open_racing_stop_leaves_no_owner_or_socket() {
    let test = TestDaemon::new().await;
    open_until_running(&test.daemon, Some("ready"), "harness/5")
        .await
        .expect("ready Host");
    let racing = location(&test.home, "racing");
    std::fs::create_dir_all(racing.state_dir()).expect("create racing Host state");

    let daemon = Arc::clone(&test.daemon);
    let opening = tokio::spawn(async move {
        daemon
            .open_host(Some("racing".to_owned()), "harness/race".to_owned())
            .await
    });
    tokio::task::yield_now().await;
    test.daemon.stop().await;
    let _ = tokio::time::timeout(TEST_TIMEOUT, opening)
        .await
        .expect("racing open should finish")
        .expect("racing open task should join");
    tokio::time::timeout(TEST_TIMEOUT, test.serving)
        .await
        .expect("daemon serve should finish")
        .expect("daemon serve task should join")
        .expect("daemon serve should stop cleanly");

    assert!(!test.home.socket().exists());
    let racing_db = racing.state_dir().join("arena0.sqlite");
    let reservation = Store::reserve(&racing_db).expect("store owner must be released");
    drop(reservation);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn startup_restores_initial_metadata_before_accepting_opens() {
    let first = TestDaemon::new().await;
    let old = open_until_running(&first.daemon, Some("initial"), "old-agent")
        .await
        .expect("create initial Host");
    let root = first.stop().await;

    let test = TestDaemon::from_root_with_names(root, vec!["initial".parse().unwrap()]).await;
    let reopened = open_until_running(&test.daemon, Some("initial"), "new-agent")
        .await
        .expect("existing Host open");
    assert_eq!(reopened.peer_id, old.peer_id);
    assert_eq!(reopened.user_agent, Some("new-agent".to_owned()));
    assert_eq!(test.daemon.services().len(), 1);
    assert!(test.home.socket().exists());
    let _root = test.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failed_initial_socket_startup_releases_all_resources_for_retry() {
    let root = tempfile::tempdir().expect("temporary daemon home");
    let home = Home::from_root(root.path().to_path_buf()).expect("home");
    let socket = home.socket();
    let mcp = McpConfig::new(SocketAddr::from(([127, 0, 0, 1], 0)), None).expect("MCP config");
    let engine = Arc::new(WasmtimeEngine::new().expect("sandbox engine"));
    let daemon = Daemon::start(
        vec!["first".parse().unwrap(), "second".parse().unwrap()],
        mcp,
        engine,
        home.clone(),
        true,
    )
    .await
    .expect("start daemon with initial Hosts");
    std::fs::create_dir_all(&socket).expect("inject startup socket conflict");
    let serving = tokio::spawn(Arc::clone(&daemon).serve());
    let result = tokio::time::timeout(TEST_TIMEOUT, serving)
        .await
        .expect("failed startup should finish")
        .expect("serve task should join");
    assert!(result.is_err(), "socket conflict must fail startup");
    assert!(socket.is_dir(), "injected conflict remains for repair");
    for id in ["first", "second"] {
        let reservation = Store::reserve(
            home.host(&id.parse().unwrap())
                .state_dir()
                .join("arena0.sqlite"),
        )
        .expect("failed startup must release store owner");
        drop(reservation);
    }
    std::fs::remove_dir(&socket).expect("remove startup conflict");
    daemon.stop().await;
    drop(daemon);
    let retry = TestDaemon::from_root(root).await;
    let first_retry = open_until_running(&retry.daemon, Some("first"), "retry-agent")
        .await
        .expect("retry first Host");
    assert_eq!(first_retry.id, "first");
    let second_retry = open_until_running(&retry.daemon, Some("second"), "retry-agent")
        .await
        .expect("retry second Host");
    assert_ne!(first_retry.peer_id, second_retry.peer_id);
    let _root = retry.stop().await;
}
