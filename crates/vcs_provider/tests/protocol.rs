use std::{collections::BTreeMap, path::Path, time::Duration};
use vcs_provider::{Client, read_message, validate_path};

async fn start(mode: &str, timeout: Duration) -> anyhow::Result<Client> {
    Client::start(
        "python3",
        &[
            format!("{}/tests/mock_provider.py", env!("CARGO_MANIFEST_DIR")),
            mode.into(),
        ],
        &BTreeMap::new(),
        Path::new(env!("CARGO_MANIFEST_DIR")),
        timeout,
    )
    .await
}

#[test]
fn mock_discovery_status_notifications_and_lazy_content() {
    smol::block_on(async {
        let client = start("normal", Duration::from_secs(3)).await.unwrap();
        assert!(client.capabilities.staging);
        assert_eq!(client.snapshot().changes.len(), 3);
        let contents = client
            .contents(
                &[
                    "hello.txt".into(),
                    "hello.txt".into(),
                    "new.txt".into(),
                    "clean.txt".into(),
                ],
                &[false, true, false, false],
            )
            .await
            .unwrap();
        assert_eq!(
            contents,
            vec![
                Some(b"base\n\x00\xff".to_vec()),
                Some(b"index\n\x00\xff".to_vec()),
                None,
                Some(b"base\n\x00\xff".to_vec())
            ]
        );
        client.refresh().await.unwrap();
        assert_eq!(client.snapshot().snapshot, "2");
    });
}

#[test]
fn rejects_invalid_providers_and_bounds_request_time() {
    smol::block_on(async {
        for mode in ["version", "eof", "bad-id", "bad-path", "timeout"] {
            assert!(
                start(mode, Duration::from_millis(200)).await.is_err(),
                "{mode}"
            );
        }
    });
}

#[test]
fn errors_preserve_the_last_valid_snapshot() {
    smol::block_on(async {
        let client = start("status-error", Duration::from_secs(3)).await.unwrap();
        let snapshot = client.snapshot();
        assert!(client.refresh().await.is_err());
        assert_eq!(client.snapshot(), snapshot);
        assert!(
            client
                .contents(&["hello.txt".into()], &[false])
                .await
                .is_ok()
        );
    });
}

#[test]
fn framing_and_path_validation() {
    smol::block_on(async {
        for message in [
            b"Content-Length: 999999999\r\n\r\n".as_slice(),
            b"Content-Length: 2\r\nContent-Length: 2\r\n\r\n{}",
            b"Content-Length: 10\r\n\r\n{}",
            b"\r\n",
            &[b'x'; 9000],
        ] {
            assert!(
                read_message(&mut message.to_vec().as_slice())
                    .await
                    .is_err()
            );
        }
        assert_eq!(
            read_message(&mut b"Content-Length: 2\r\n\r\n{}".as_slice())
                .await
                .unwrap(),
            serde_json::json!({})
        );
    });
    for path in ["../a", "/a", "a/../b", "a//b", "a\\b", "C:/a", "a\0b", ""] {
        assert!(validate_path(path).is_err());
    }
    assert!(validate_path("dir/unicode 🦀.txt").is_ok());
}

#[test]
fn expired_snapshot_is_refreshed_and_retried_once() {
    smol::block_on(async {
        let client = start("expired", Duration::from_secs(3)).await.unwrap();
        let contents = client
            .contents(&["hello.txt".into()], &[false])
            .await
            .unwrap();
        assert_eq!(contents, vec![Some(b"base\n\x00\xff".to_vec())]);
        assert_eq!(client.snapshot().snapshot, "2");
    });
}

#[test]
fn optional_history_metadata_and_commit_contents() {
    smol::block_on(async {
        let client = start("history", Duration::from_secs(3)).await.unwrap();
        assert!(client.capabilities.history);
        let history = client.history("revision-2", None, 1).await.unwrap();
        assert_eq!(history.commits.len(), 1);
        assert!(history.has_more);
        assert_eq!(history.commits[0].parents, ["revision-1"]);
        let commit = client.commit_details("revision-2").await.unwrap();
        assert_eq!(commit.message, "Change hello\n\nDetails");
        let changes = client.commit_changes("revision-2").await.unwrap();
        assert_eq!(changes.len(), 3);
        assert!(changes[1].base.is_none());
        assert!(changes[2].target.is_none());
        assert_eq!(
            client
                .read_content(changes[0].base.as_ref().unwrap())
                .await
                .unwrap(),
            b"before\n\x00\xff"
        );
        assert!(
            client
                .history("revision-2", Some("../escape"), 1)
                .await
                .is_err()
        );
        assert!(client.history("revision-2", None, 201).await.is_err());
        let old_provider = start("normal", Duration::from_secs(3)).await.unwrap();
        assert!(!old_provider.capabilities.history);
        assert!(
            old_provider
                .history("opaque-revision", None, 1)
                .await
                .is_err()
        );
        for (mode, limit) in [("history-bad-id", 2), ("history-too-many", 1)] {
            let client = start(mode, Duration::from_secs(3)).await.unwrap();
            assert!(client.history("revision-2", None, limit).await.is_err());
        }
        let client = start("history-bad-path", Duration::from_secs(3))
            .await
            .unwrap();
        assert!(client.commit_changes("revision-2").await.is_err());
    });
}

#[test]
fn capabilities_and_reference_validation() {
    smol::block_on(async {
        let legacy = start("normal", Duration::from_secs(3)).await.unwrap();
        assert!(!legacy.capabilities.branches);
        assert!(!legacy.capabilities.tags);
        assert!(legacy.snapshot().references.is_empty());
        let client = start("history-refs", Duration::from_secs(3)).await.unwrap();
        assert!(
            client.capabilities.branches
                && client.capabilities.tags
                && client.capabilities.tracking
        );
        assert_eq!(client.snapshot().references.len(), 6);
        for mode in [
            "bad-tracking",
            "reserved-feature",
            "bad-refs-duplicate",
            "bad-refs-name",
            "bad-refs-counts",
            "bad-refs-gone",
            "unadvertised-refs",
        ] {
            assert!(start(mode, Duration::from_secs(3)).await.is_err(), "{mode}");
        }
    });
}
