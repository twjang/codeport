use super::*;
use std::path::Path;
use tokio::net::TcpListener;

fn access(command: &str, persistent: bool, cleanup: Option<String>) -> AccessSession {
    AccessSession::new(Some(Access {
        command: command.into(),
        persistent,
        cleanup,
        timeout_secs: 1,
    }))
}

fn quote(path: &Path) -> String {
    format!("'{}'", path.to_string_lossy().replace('\'', "'\\''"))
}

async fn listening_backend() -> (TcpListener, String) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/v1", listener.local_addr().unwrap());
    (listener, url)
}

async fn unavailable_backend() -> String {
    let (listener, url) = listening_backend().await;
    drop(listener);
    url
}

#[tokio::test]
async fn preparation_command_finishes_before_session_starts() {
    let directory = tempfile::tempdir().unwrap();
    let marker = directory.path().join("prepared");
    let (_listener, url) = listening_backend().await;
    let mut session = access(&format!("printf ready > {}", quote(&marker)), false, None);
    session.start(&url).await.unwrap();
    assert_eq!(std::fs::read_to_string(marker).unwrap(), "ready");
    assert!(session
        .process
        .as_mut()
        .unwrap()
        .try_wait()
        .unwrap()
        .is_some());
    session.cleanup().await;
}

#[tokio::test]
async fn failed_preparation_reports_status_and_runs_cleanup_once() {
    let directory = tempfile::tempdir().unwrap();
    let marker = directory.path().join("cleanup");
    let (_listener, url) = listening_backend().await;
    let mut session = access(
        "exit 7",
        false,
        Some(format!("printf cleanup >> {}", quote(&marker))),
    );
    let error = session.start(&url).await.unwrap_err().to_string();
    assert!(error.contains("failed"), "{error}");
    assert!(error.contains('7'), "{error}");
    session.cleanup().await;
    session.cleanup().await;
    assert_eq!(std::fs::read_to_string(marker).unwrap(), "cleanup");
}

#[tokio::test]
async fn persistent_process_death_before_readiness_is_reported() {
    let url = unavailable_backend().await;
    let mut session = access("exit 9", true, None);
    let error = session.start(&url).await.unwrap_err().to_string();
    session.cleanup().await;
    assert!(error.contains("exited before backend readiness"), "{error}");
}

#[tokio::test]
async fn successful_session_runs_configured_cleanup() {
    let directory = tempfile::tempdir().unwrap();
    let marker = directory.path().join("cleanup");
    let (_listener, url) = listening_backend().await;
    let mut session = access(
        "sleep 60",
        true,
        Some(format!("printf done > {}", quote(&marker))),
    );
    session.start(&url).await.unwrap();
    assert!(!marker.exists());
    session.cleanup().await;
    assert_eq!(std::fs::read_to_string(marker).unwrap(), "done");
    assert!(session.process.is_none());
    assert!(session.group.is_none());
}

#[tokio::test]
async fn backend_readiness_timeout_still_allows_cleanup() {
    let directory = tempfile::tempdir().unwrap();
    let marker = directory.path().join("cleanup");
    let url = unavailable_backend().await;
    let mut session = access(
        "true",
        false,
        Some(format!("printf done > {}", quote(&marker))),
    );
    let start = tokio::time::Instant::now();
    let error = session.start(&url).await.unwrap_err().to_string();
    session.cleanup().await;
    assert!(error.contains("did not become reachable"), "{error}");
    assert!(start.elapsed() < Duration::from_secs(4));
    assert_eq!(std::fs::read_to_string(marker).unwrap(), "done");
}

#[tokio::test]
async fn preparation_timeout_is_bounded_and_cleanup_runs() {
    let directory = tempfile::tempdir().unwrap();
    let marker = directory.path().join("cleanup");
    let (_listener, url) = listening_backend().await;
    let mut session = access(
        "sleep 60",
        false,
        Some(format!("printf done > {}", quote(&marker))),
    );
    let error = session.start(&url).await.unwrap_err().to_string();
    session.cleanup().await;
    assert!(error.contains("command timed out"), "{error}");
    assert_eq!(std::fs::read_to_string(marker).unwrap(), "done");
}

#[tokio::test]
async fn cleanup_terminates_persistent_descendants_even_if_they_ignore_term() {
    let directory = tempfile::tempdir().unwrap();
    let heartbeat = directory.path().join("heartbeat");
    let (_listener, url) = listening_backend().await;
    let command = format!(
        "(trap '' TERM; while :; do printf x >> {}; sleep 0.05; done) & wait",
        quote(&heartbeat)
    );
    let mut session = access(&command, true, None);
    session.start(&url).await.unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    loop {
        if std::fs::metadata(&heartbeat)
            .map(|m| m.len() >= 2)
            .unwrap_or(false)
        {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "descendant heartbeat did not start"
        );
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    session.cleanup().await;
    let length = std::fs::metadata(&heartbeat).unwrap().len();
    tokio::time::sleep(Duration::from_millis(250)).await;
    assert_eq!(
        std::fs::metadata(&heartbeat).unwrap().len(),
        length,
        "a descendant kept running after cleanup"
    );
}

#[tokio::test]
async fn unused_session_does_not_execute_cleanup() {
    let directory = tempfile::tempdir().unwrap();
    let marker = directory.path().join("cleanup");
    let mut session = access("true", false, Some(format!("touch {}", quote(&marker))));
    session.cleanup().await;
    assert!(!marker.exists());
}
