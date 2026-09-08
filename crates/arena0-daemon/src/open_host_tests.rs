use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use arena0_api::{HostInfo, Request, Response, ResponseOk, frame};
use arena0_home::{Home, HostName};
use arena0_protocol::PeerId;
use arena0_sandbox::WasmtimeEngine;
use arena0_store::{Store, StoreConfig};
use tempfile::TempDir;
use tokio::io::BufReader;
use tokio::net::UnixStream;
use tokio::task::JoinHandle;

use crate::{Daemon, Keystore, McpConfig, Paths};

const TEST_TIMEOUT: Duration = Duration::from_secs(15);

struct TestDaemon {
    root: TempDir,
    home: Home,
    daemon: Arc<Daemon>,
    serving: JoinHandle<anyhow::Result<()>>,
}

impl TestDaemon {
    async fn new() -> Self {
        Self::new_with_bootstrap(false).await
    }

    async fn bootstrapped() -> Self {
        Self::new_with_bootstrap(true).await
    }

    async fn new_with_bootstrap(bootstrap_new_hosts: bool) -> Self {
        Self::from_root_with_names_and_bootstrap(
            tempfile::tempdir().expect("temporary daemon home"),
            Vec::new(),
            bootstrap_new_hosts,
        )
        .await
    }

    async fn from_root_with_names(root: TempDir, names: Vec<HostName>) -> Self {
        Self::from_root_with_names_and_bootstrap(root, names, false).await
    }

    async fn from_root_with_names_and_bootstrap(
        root: TempDir,
        names: Vec<HostName>,
        bootstrap_new_hosts: bool,
    ) -> Self {
        let home = Home::from_root(root.path().to_path_buf()).expect("home");
        let mcp = McpConfig::new(SocketAddr::from(([127, 0, 0, 1], 0)), None).expect("MCP config");
        let engine = Arc::new(WasmtimeEngine::new().expect("sandbox engine"));
        let daemon = Daemon::start(names, mcp, engine, home.clone(), bootstrap_new_hosts)
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

async fn seed_host(home: &Home, id: &str, user_agent: Option<&str>) -> PeerId {
    let name = id.parse::<HostName>().expect("Host id");
    let paths = Paths::new(home.host(&name).state_dir().to_owned());
    paths.ensure_dirs().expect("Host state directories");
    let keystore = Keystore::open(paths.keys_dir.clone()).expect("Host keystore");
    let identity = keystore
        .new_identity(Some(id.to_owned()))
        .expect("Host identity");
    let store = Store::open(StoreConfig::new(paths.db_path, identity.peer_id)).expect("Host store");
    if let Some(user_agent) = user_agent {
        store
            .handle()
            .set_user_agent(user_agent.to_owned())
            .await
            .expect("Host user agent");
    }
    store.shutdown().await.expect("Host store shutdown");
    identity.peer_id
}

async fn wait_for_socket(home: &Home) {
    tokio::time::timeout(TEST_TIMEOUT, async {
        loop {
            if UnixStream::connect(home.socket()).await.is_ok() {
                return;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("daemon Unix socket should become ready");
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
    wait_for_socket(&test.home).await;

    let _root = test.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn open_racing_stop_leaves_no_owner_or_socket() {
    let test = TestDaemon::bootstrapped().await;
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
    let root = tempfile::tempdir().expect("temporary daemon home");
    let home = Home::from_root(root.path().to_path_buf()).expect("home");
    let old_peer = seed_host(&home, "initial", Some("old-agent")).await;
    let test = TestDaemon::from_root_with_names(root, vec!["initial".parse().unwrap()]).await;
    let reopened = open_until_running(&test.daemon, Some("initial"), "new-agent")
        .await
        .expect("existing Host open");
    assert_eq!(reopened.peer_id, old_peer);
    assert_eq!(reopened.user_agent, Some("new-agent".to_owned()));
    assert_eq!(test.daemon.services().len(), 1);
    assert!(test.home.socket().exists());
    let _root = test.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failed_initial_socket_startup_releases_all_resources_for_retry() {
    let root = tempfile::tempdir().expect("temporary daemon home");
    let home = Home::from_root(root.path().to_path_buf()).expect("home");
    let first_peer = seed_host(&home, "first", None).await;
    let second_peer = seed_host(&home, "second", None).await;
    let socket = home.socket();
    let mcp = McpConfig::new(SocketAddr::from(([127, 0, 0, 1], 0)), None).expect("MCP config");
    let engine = Arc::new(WasmtimeEngine::new().expect("sandbox engine"));
    let daemon = Daemon::start(
        vec!["first".parse().unwrap(), "second".parse().unwrap()],
        mcp,
        engine,
        home.clone(),
        false,
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
    drop(daemon);
    let lease = crate::paths::FileLease::acquire_home(&home)
        .expect("failed startup must release Home ownership");
    drop(lease);
    let retry = TestDaemon::from_root_with_names(
        root,
        vec!["first".parse().unwrap(), "second".parse().unwrap()],
    )
    .await;
    wait_for_socket(&retry.home).await;

    let stream = UnixStream::connect(retry.home.socket())
        .await
        .expect("retry Unix socket");
    let (read, mut write) = stream.into_split();
    let mut read = BufReader::new(read);
    frame::write_frame(&mut write, &Request::HostsList)
        .await
        .expect("HostsList request");
    let response = frame::read_frame::<_, Response>(&mut read)
        .await
        .expect("HostsList response frame")
        .expect("HostsList response");
    let hosts = match response {
        Ok(ResponseOk::Hosts(hosts)) => hosts,
        response => panic!("unexpected HostsList response: {response:?}"),
    };
    assert_eq!(hosts.len(), 2);
    assert!(hosts.iter().any(|host| host.host.peer_id == first_peer));
    assert!(hosts.iter().any(|host| host.host.peer_id == second_peer));
    let _root = retry.stop().await;
}
