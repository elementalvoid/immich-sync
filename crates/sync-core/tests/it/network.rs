//! The known-network gate (SPEC R8.8): the pure check, and the engine pausing
//! and resuming over a fake backend.
//!
//! Every case the ticket names runs here without a platform: the check's own
//! table, startup, an OS change event, the fallback poll, an OS that errors,
//! and an upload that could not reach the server.

use super::common::wait_until;

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use mock_immich::{Fault, MockImmich, Operation};
use sync_core::connection::{AuthMethod, Connection, Secret, SharedSession, SignIn};
use sync_core::credential::MemoryCredentialStore;
use sync_core::engine::{Engine, EngineConfig};
use sync_core::network::{
    CurrentNetwork, KnownNetworks, NetworkDecision, NetworkProblem, NetworkSource, decide,
};
use sync_core::status::{Activity, PauseReason};
use sync_core::store::{SourceFolder, StateStore};
use sync_core::tracking::Tracking;
use tokio::sync::mpsc;

/// A backend a test sets by hand, with the change channel a platform would
/// own.
struct FakeNetwork {
    current: Mutex<Result<CurrentNetwork, NetworkProblem>>,
    /// The sender the test announces with. `None` when the backend's event
    /// channel is closed, which is what a backend that stops listening looks
    /// like.
    announce: Option<mpsc::Sender<()>>,
    events: Mutex<Option<mpsc::Receiver<()>>>,
    /// How many times the engine asked for the network.
    calls: std::sync::atomic::AtomicUsize,
    /// Whether the first answer takes a long time, for the timeout case.
    slow_first: std::sync::atomic::AtomicBool,
}

impl FakeNetwork {
    fn new(current: Result<CurrentNetwork, NetworkProblem>) -> Arc<Self> {
        Self::build(current, true, false)
    }

    /// A backend whose event channel is already closed, as when its listener
    /// thread bailed out.
    fn with_closed_events(current: Result<CurrentNetwork, NetworkProblem>) -> Arc<Self> {
        Self::build(current, false, false)
    }

    /// A backend whose first answer arrives after the check's timeout.
    fn answering_after_the_first_check(
        current: Result<CurrentNetwork, NetworkProblem>,
    ) -> Arc<Self> {
        Self::build(current, true, true)
    }

    fn build(
        current: Result<CurrentNetwork, NetworkProblem>,
        events_open: bool,
        slow_first: bool,
    ) -> Arc<Self> {
        use std::sync::atomic::AtomicBool;
        let (sender, events) = mpsc::channel(8);
        let announce = if events_open {
            Some(sender)
        } else {
            drop(sender);
            None
        };
        Arc::new(Self {
            current: Mutex::new(current),
            announce,
            events: Mutex::new(Some(events)),
            calls: std::sync::atomic::AtomicUsize::new(0),
            slow_first: AtomicBool::new(slow_first),
        })
    }

    fn set(&self, current: Result<CurrentNetwork, NetworkProblem>) {
        *self.current.lock().expect("the fake network") = current;
    }

    fn announce(&self) {
        if let Some(announce) = &self.announce {
            let _ = announce.try_send(());
        }
    }

    fn calls(&self) -> usize {
        self.calls.load(std::sync::atomic::Ordering::SeqCst)
    }
}

impl NetworkSource for FakeNetwork {
    fn current(&self) -> Result<CurrentNetwork, NetworkProblem> {
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if self
            .slow_first
            .swap(false, std::sync::atomic::Ordering::SeqCst)
        {
            std::thread::sleep(Duration::from_millis(300));
        }
        self.current.lock().expect("the fake network").clone()
    }

    fn changes(&self) -> Option<mpsc::Receiver<()>> {
        self.events.lock().expect("the fake events").take()
    }
}

fn wireless(name: &str) -> Result<CurrentNetwork, NetworkProblem> {
    Ok(CurrentNetwork::Wireless(name.to_string()))
}

fn listed(name: &str) -> KnownNetworks {
    KnownNetworks {
        enabled: true,
        names: vec![name.to_string()],
    }
}

#[test]
fn the_check_fails_closed() {
    let off = KnownNetworks::default();
    let home = listed("Home");
    let wired = Ok(CurrentNetwork::Wired);
    let unknown = Err(NetworkProblem::new("NetworkManager not found"));
    let cases = [
        // R8.8: the list off keeps the old behaviour on any network.
        (&off, &wired, NetworkDecision::Allowed),
        (&off, &wireless("Cafe"), NetworkDecision::Allowed),
        (&off, &unknown, NetworkDecision::Allowed),
        // A listed wireless name, and a wired connection, always sync.
        (&home, &wireless("Home"), NetworkDecision::Allowed),
        (&home, &wired, NetworkDecision::Allowed),
        // A name the user did not list waits.
        (&home, &wireless("Cafe"), NetworkDecision::NotListed),
        // Nothing proves the network: an OS that errors pauses.
        (&home, &unknown, NetworkDecision::Unknown),
        // An empty name proves nothing either, so it reads as unknown.
        (&home, &wireless(""), NetworkDecision::Unknown),
        // A listed name with stray space still matches.
        (
            &KnownNetworks {
                enabled: true,
                names: vec![" Home ".to_string()],
            },
            &wireless("Home"),
            NetworkDecision::Allowed,
        ),
        // An empty list allows nothing but a wired connection.
        (
            &KnownNetworks {
                enabled: true,
                names: Vec::new(),
            },
            &wireless("Home"),
            NetworkDecision::NotListed,
        ),
    ];
    for (known, current, expected) in cases {
        assert_eq!(
            decide(known, current.as_ref()),
            expected,
            "known {known:?} on {current:?}"
        );
    }
}

async fn signed_in(server: &MockImmich) -> SharedSession {
    let mut connection = Connection::new(server.base_url(), Arc::new(MemoryCredentialStore::new()))
        .expect("a valid URL");
    connection
        .sign_in(SignIn {
            email: "sync@example.com".to_string(),
            secret: Secret::new("correct horse"),
            method: AuthMethod::Password,
        })
        .await;
    connection.shared()
}

fn store_in(temp: &tempfile::TempDir) -> Arc<StateStore> {
    Arc::new(StateStore::open(temp.path().join("data").join("state.db")).expect("open the store"))
}

fn add_folder(store: &StateStore, path: &Path) {
    store
        .add_folder(&SourceFolder {
            path: path.display().to_string(),
            recursive: true,
            ..Default::default()
        })
        .expect("add the source folder");
}

fn write_file(root: &Path, name: &str, bytes: &[u8]) -> PathBuf {
    let path = root.join(name);
    std::fs::create_dir_all(root).expect("create the fixture directory");
    std::fs::write(&path, bytes).expect("write the fixture");
    path
}

/// Starts an engine that watches the fake backend, with the watcher on so a
/// new file reaches the queue without a manual scan.
async fn start_engine(
    server: &MockImmich,
    store: &Arc<StateStore>,
    fake: &Arc<FakeNetwork>,
    known: KnownNetworks,
    poll: Duration,
) -> Arc<Engine> {
    start_engine_with(server, store, fake, known, poll, Duration::from_secs(5)).await
}

/// [`start_engine`], with the check's own timeout.
async fn start_engine_with(
    server: &MockImmich,
    store: &Arc<StateStore>,
    fake: &Arc<FakeNetwork>,
    known: KnownNetworks,
    poll: Duration,
    check_timeout: Duration,
) -> Arc<Engine> {
    let session = signed_in(server).await;
    let engine = Arc::new(
        Engine::new(
            Arc::new(Tracking::new(store.clone())),
            session,
            EngineConfig {
                network_poll: poll,
                network_check_timeout: check_timeout,
                watch_debounce: Duration::from_millis(50),
                ..EngineConfig::default()
            },
        )
        .expect("build the engine"),
    );
    engine.watch_network(Arc::clone(fake) as Arc<dyn NetworkSource>, known);
    engine.start();
    engine
}

fn uploaded(store: &StateStore, path: &Path) -> bool {
    matches!(
        store.item(&path.display().to_string()),
        Ok(Some(row)) if row.state.is_terminal_success()
    )
}

/// R8.8: the engine never uploads on a network the user did not list.
#[tokio::test]
async fn a_start_on_an_unlisted_network_waits() {
    let server = MockImmich::start().await;
    let temp = tempfile::tempdir().expect("create a temp dir");
    let store = store_in(&temp);
    let photos = temp.path().join("photos");
    let file = write_file(&photos, "first.jpg", b"first");
    add_folder(&store, &photos);
    let fake = FakeNetwork::new(wireless("Cafe"));
    let engine = start_engine(
        &server,
        &store,
        &fake,
        listed("Home"),
        Duration::from_secs(60),
    )
    .await;

    assert!(
        wait_until(
            || engine.status().pause == Some(PauseReason::NotListed),
            Duration::from_secs(5)
        )
        .await,
        "the engine did not report the unlisted network"
    );
    assert_eq!(engine.status().activity, Activity::Paused);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        server.upload_requests(),
        0,
        "R8.8: no upload may start on an unlisted network"
    );
    assert!(!uploaded(&store, &file));
    engine.stop();
}

/// R8.8: leaving a listed network pauses, and returning resumes with the
/// queue kept, so nothing failed and nothing was lost.
#[tokio::test]
async fn leaving_a_listed_network_pauses_and_returning_resumes_with_the_queue() {
    let server = MockImmich::start().await;
    let temp = tempfile::tempdir().expect("create a temp dir");
    let store = store_in(&temp);
    let photos = temp.path().join("photos");
    std::fs::create_dir_all(&photos).expect("create the folder");
    add_folder(&store, &photos);
    let fake = FakeNetwork::new(wireless("Home"));
    let engine = start_engine(
        &server,
        &store,
        &fake,
        listed("Home"),
        Duration::from_secs(60),
    )
    .await;
    assert!(
        wait_until(|| engine.status().pause.is_none(), Duration::from_secs(5)).await,
        "the listed network did not let uploads run"
    );

    // The OS reports another network, and the change event wakes the check
    // long before the 60-second poll.
    fake.set(wireless("Cafe"));
    fake.announce();
    assert!(
        wait_until(
            || engine.status().pause == Some(PauseReason::NotListed),
            Duration::from_secs(5)
        )
        .await,
        "leaving the listed network did not pause"
    );

    // A file written while paused is queued, not failed.
    let queued = write_file(&photos, "queued.jpg", b"queued while paused");
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(
        server.upload_requests(),
        0,
        "R8.8: the queue is held, not attempted"
    );
    assert!(
        !uploaded(&store, &queued),
        "the queued file must still wait"
    );

    // The listed network comes back: the engine resumes on its own.
    fake.set(wireless("Home"));
    fake.announce();
    assert!(
        wait_until(|| uploaded(&store, &queued), Duration::from_secs(10)).await,
        "returning to the listed network did not resume the queue"
    );
    assert_eq!(engine.status().pause, None, "the engine reports the resume");
    engine.stop();
}

/// R8.8: the 30-second poll is the net under a missed change event, so a
/// shorter poll must notice a change no event announced.
#[tokio::test]
async fn the_fallback_poll_notices_a_missed_change() {
    let server = MockImmich::start().await;
    let temp = tempfile::tempdir().expect("create a temp dir");
    let store = store_in(&temp);
    let photos = temp.path().join("photos");
    write_file(&photos, "first.jpg", b"first");
    add_folder(&store, &photos);
    let fake = FakeNetwork::new(wireless("Home"));
    let engine = start_engine(
        &server,
        &store,
        &fake,
        listed("Home"),
        Duration::from_millis(50),
    )
    .await;
    assert!(
        wait_until(
            || uploaded(&store, &photos.join("first.jpg")),
            Duration::from_secs(10)
        )
        .await,
        "the listed network did not upload"
    );

    // No event is sent: only the poll can see this.
    fake.set(Err(NetworkProblem::new("no active network connection")));
    assert!(
        wait_until(
            || engine.status().pause == Some(PauseReason::Unknown),
            Duration::from_secs(5)
        )
        .await,
        "the fallback poll did not notice the vanished network"
    );
    assert_eq!(engine.status().activity, Activity::Paused);
    engine.stop();
}

/// R8.8: an OS call that errors fails closed, and a later successful call
/// releases the hold.
#[tokio::test]
async fn an_os_call_that_errors_fails_closed_and_recovers() {
    let server = MockImmich::start().await;
    let temp = tempfile::tempdir().expect("create a temp dir");
    let store = store_in(&temp);
    let photos = temp.path().join("photos");
    let file = write_file(&photos, "first.jpg", b"first");
    add_folder(&store, &photos);
    let fake = FakeNetwork::new(Err(NetworkProblem::new("NetworkManager not found")));
    let engine = start_engine(
        &server,
        &store,
        &fake,
        listed("Home"),
        Duration::from_millis(50),
    )
    .await;
    assert!(
        wait_until(
            || engine.status().network.problem.as_deref() == Some("NetworkManager not found"),
            Duration::from_secs(5)
        )
        .await,
        "an OS that errors must hold uploads and report the reason"
    );
    assert_eq!(engine.status().pause, Some(PauseReason::Unknown));
    assert_eq!(server.upload_requests(), 0);

    fake.set(wireless("Home"));
    assert!(
        wait_until(|| uploaded(&store, &file), Duration::from_secs(10)).await,
        "a recovered check did not release the hold"
    );
    engine.stop();
}

/// R8.8: a request that cannot reach the server re-checks the network at once,
/// so an "allowed" that went stale cannot keep failing. The poll is a minute
/// here, so only the error path can produce the pause.
#[tokio::test]
async fn an_upload_network_error_rechecks_the_network() {
    let server = MockImmich::start().await;
    let temp = tempfile::tempdir().expect("create a temp dir");
    let store = store_in(&temp);
    let photos = temp.path().join("photos");
    std::fs::create_dir_all(&photos).expect("create the folder");
    add_folder(&store, &photos);
    let fake = FakeNetwork::new(wireless("Home"));
    let engine = start_engine(
        &server,
        &store,
        &fake,
        listed("Home"),
        Duration::from_secs(60),
    )
    .await;

    // The first check lets uploads run, and the answer then goes stale: the OS
    // has already lost the network, but the engine still believes "Home".
    assert!(
        wait_until(
            || engine.status().network.name.as_deref() == Some("Home"),
            Duration::from_secs(5)
        )
        .await,
        "the first network check did not land"
    );
    assert_eq!(engine.status().pause, None);
    server.fail(Operation::Upload, Fault::Disconnect);
    fake.set(Err(NetworkProblem::new("no active network connection")));

    // The request now fails on the wire while the stale answer still says
    // Home; that failure is what wakes the check.
    let file = write_file(&photos, "first.jpg", b"first");
    assert!(
        wait_until(
            || {
                store.item(&file.display().to_string()).is_ok_and(|row| {
                    row.is_some_and(|row| {
                        matches!(
                            row.state,
                            sync_core::store::ItemState::RetryWait
                                | sync_core::store::ItemState::FailedPermanent
                                | sync_core::store::ItemState::DeadLetter
                        )
                    })
                })
            },
            Duration::from_secs(10)
        )
        .await,
        "the failing upload never recorded a failure"
    );
    assert!(
        wait_until(
            || engine.status().pause == Some(PauseReason::Unknown),
            Duration::from_secs(5)
        )
        .await,
        "R8.8: an upload network error did not re-check the network"
    );
    engine.stop();
}

/// R8.8: a backend whose event channel has closed is polled, not spun. A
/// closed channel returns at once, so the loop must stop selecting on it.
#[tokio::test]
async fn a_closed_event_channel_does_not_spin_the_check() {
    let server = MockImmich::start().await;
    let temp = tempfile::tempdir().expect("create a temp dir");
    let store = store_in(&temp);
    let photos = temp.path().join("photos");
    std::fs::create_dir_all(&photos).expect("create the folder");
    add_folder(&store, &photos);
    let fake =
        FakeNetwork::with_closed_events(Err(NetworkProblem::new("the event subscription failed")));
    let engine = start_engine(
        &server,
        &store,
        &fake,
        KnownNetworks::default(),
        Duration::from_secs(60),
    )
    .await;
    // The one-shot wakeups and the closed channel's first return each cost a
    // check; after that the 60-second poll is the only driver, so the count
    // must stop growing.
    tokio::time::sleep(Duration::from_millis(200)).await;
    let settled = fake.calls();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        fake.calls(),
        settled,
        "a closed event channel kept checking with a 60-second poll"
    );
    engine.stop();
}

/// R8.8: an OS call that outlives its timeout fails closed, and the next check
/// does not spawn a second call on top of the one still running.
#[tokio::test]
async fn a_hung_os_call_times_out_and_holds_then_recovers() {
    let server = MockImmich::start().await;
    let temp = tempfile::tempdir().expect("create a temp dir");
    let store = store_in(&temp);
    let photos = temp.path().join("photos");
    std::fs::create_dir_all(&photos).expect("create the folder");
    add_folder(&store, &photos);
    let fake = FakeNetwork::answering_after_the_first_check(wireless("Home"));
    let engine = start_engine_with(
        &server,
        &store,
        &fake,
        listed("Home"),
        Duration::from_millis(50),
        Duration::from_millis(50),
    )
    .await;
    // Both reasons prove the hung call holds: the first check reports the
    // timeout, and the checks behind it report the still-running call rather
    // than spawning another.
    assert!(
        wait_until(
            || {
                let status = engine.status();
                status.pause == Some(PauseReason::Unknown)
                    && status
                        .network
                        .problem
                        .as_deref()
                        .is_some_and(|problem| problem.contains("network check"))
            },
            Duration::from_secs(2)
        )
        .await,
        "a check past its timeout must hold uploads and say why: {:?}",
        engine.status().network.problem
    );
    assert_eq!(
        fake.calls(),
        1,
        "the running call must not be joined by another"
    );
    // The hung call finally answers and clears the in-flight flag; the next
    // poll reads the network and lets uploads run again.
    assert!(
        wait_until(|| engine.status().pause.is_none(), Duration::from_secs(5)).await,
        "a check that returned after its timeout must not hold forever"
    );
    engine.stop();
}

/// R8.8: a list that is off keeps the old behaviour, whatever the OS says.
#[tokio::test]
async fn a_list_that_is_off_syncs_on_any_network() {
    let server = MockImmich::start().await;
    let temp = tempfile::tempdir().expect("create a temp dir");
    let store = store_in(&temp);
    let photos = temp.path().join("photos");
    let file = write_file(&photos, "first.jpg", b"first");
    add_folder(&store, &photos);
    let fake = FakeNetwork::new(Err(NetworkProblem::new("NetworkManager not found")));
    let engine = start_engine(
        &server,
        &store,
        &fake,
        KnownNetworks::default(),
        Duration::from_millis(50),
    )
    .await;
    assert!(
        wait_until(|| uploaded(&store, &file), Duration::from_secs(10)).await,
        "the list off must not hold uploads"
    );
    assert_eq!(engine.status().pause, None);
    engine.stop();
}
