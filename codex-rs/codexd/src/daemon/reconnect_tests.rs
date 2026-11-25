use super::*;
use serde_json::json;

fn full(generation: &str, revision: u64, busy: bool) -> JsonValue {
    json!({"runtimeId":"r","activeTurns":if busy {vec![json!({"threadId":"review","turnId":"0"})]} else {vec![]},
        "workActivity":{"producerGeneration":generation,"revision":revision,
            "items":if busy {vec!["review:0"]} else {vec![]}}})
}

fn event(method: &str, status: &str) -> JsonValue {
    json!({"runtimeId":"r","notification":{"method":method,"params":{"threadId":"review","turnKey":"review:0","turn":{"id":"0","status":status}}}})
}

fn send(
    d: &mut DaemonState,
    n: u64,
    owned: &mut ConnectionClaims,
    method: &str,
    params: JsonValue,
) -> Option<RequestState> {
    d.producer_message(n, owned, method, Some(params)).unwrap()
}

fn events(d: &DaemonState, after: u64) -> Vec<JsonValue> {
    d.recent_events
        .iter()
        .filter(|(n, _)| *n > after)
        .map(|(_, s)| serde_json::from_str::<JsonValue>(s).unwrap()["params"]["event"].clone())
        .collect()
}

#[tokio::test]
async fn an_independent_legacy_claim_detaches_history_without_revoking_the_runtime() {
    let mut d = DaemonState::default();
    let (mut a, mut b, mut c) = (
        ConnectionClaims::default(),
        ConnectionClaims::default(),
        ConnectionClaims::default(),
    );
    let busy = json!({"runtimeId":"r","activeTurns":[{"threadId":"legacy","turnId":"0"}]});
    let idle = json!({"runtimeId":"r","activeTurns":[]});
    send(&mut d, 1, &mut a, RUNTIME_UPDATE_STATE_METHOD, busy);
    let after = d.seq;
    let invalid = json!({"runtimeId":"r","workActivitySupported":true,"activeTurns":[]});
    assert!(send(&mut d, 2, &mut b, RUNTIME_UPDATE_STATE_METHOD, invalid).is_some());
    assert!(send(&mut d, 3, &mut c, RUNTIME_UPDATE_STATE_METHOD, idle.clone()).is_none());
    let emitted = events(&d, after);
    let order: Vec<_> = emitted
        .iter()
        .map(|e| {
            e["notification"]["method"]
                .as_str()
                .or(e["type"].as_str())
                .unwrap()
        })
        .collect();
    assert_eq!(
        order,
        ["runtime/historyLost", "runtimeUpsert"],
        "a new legacy socket has no generation to identify its predecessor"
    );
    let settled = d.snapshot();
    assert!(settled.runtimes[0].active_turns.is_empty());
    assert!(send(&mut d, 2, &mut b, RUNTIME_UPDATE_STATE_METHOD, idle).is_some());
    assert_eq!(
        d.snapshot(),
        settled,
        "B's own repair did not poison C or disappear during C's claim"
    );
}

#[tokio::test]
async fn negotiated_challenger_cannot_replace_a_busy_legacy_owner_with_legacy_state() {
    for close_incumbent in [false, true] {
        let shared = Arc::new(Mutex::new(DaemonState::default()));
        let mut d = shared.lock().await;
        let (mut incumbent, mut challenger) =
            (ConnectionClaims::default(), ConnectionClaims::default());
        let legacy_busy =
            json!({"runtimeId":"r","activeTurns":[{"threadId":"legacy","turnId":"0"}]});
        let legacy_idle = json!({"runtimeId":"r","activeTurns":[]});
        send(
            &mut d,
            1,
            &mut incumbent,
            RUNTIME_UPDATE_STATE_METHOD,
            legacy_busy,
        );
        let busy = d.snapshot();
        let invalid = json!({"runtimeId":"r","activeTurns":[],"workActivitySupported":true});
        assert!(
            send(
                &mut d,
                2,
                &mut challenger,
                RUNTIME_UPDATE_STATE_METHOD,
                invalid
            )
            .is_some()
        );
        assert_eq!(
            d.snapshot(),
            busy,
            "the incumbent is healthy, not in repair"
        );
        if close_incumbent {
            drop(d);
            cleanup_connection(
                Arc::clone(&shared),
                1,
                std::mem::take(&mut incumbent),
                false,
            )
            .await;
            d = shared.lock().await;
        }
        let before = d.snapshot();
        let _ = d.producer_message(
            2,
            &mut challenger,
            RUNTIME_UPDATE_STATE_METHOD,
            Some(legacy_idle),
        );
        let _ = d.producer_message(
            2,
            &mut challenger,
            RUNTIME_EVENT_METHOD,
            Some(event("turn/started", "inProgress")),
        );
        assert_eq!(
            d.snapshot(),
            before,
            "a negotiated challenger cannot publish legacy state"
        );
        send(
            &mut d,
            2,
            &mut challenger,
            RUNTIME_UPDATE_STATE_METHOD,
            full("H", 1, false),
        );
        let accepted = d.snapshot();
        assert_eq!(
            accepted.runtimes[0]
                .work_activity
                .as_ref()
                .unwrap()
                .producer_generation,
            "H"
        );
        assert!(accepted.runtimes[0].active_turns.is_empty());
        drop(d);
        cleanup_connection(Arc::clone(&shared), 1, incumbent, false).await;
        assert_eq!(
            shared.lock().await.snapshot(),
            accepted,
            "old EOF cannot remove H"
        );
    }
}

#[tokio::test]
async fn negotiated_contract_cannot_downgrade_an_unversioned_repair_to_legacy() {
    for reconnect in [false, true] {
        let shared = Arc::new(Mutex::new(DaemonState::default()));
        let mut d = shared.lock().await;
        let mut owned = ConnectionClaims::default();
        let invalid = json!({"runtimeId":"r","workActivitySupported":true,"activeTurns":[]});
        assert!(
            d.producer_message(1, &mut owned, RUNTIME_UPDATE_STATE_METHOD, Some(invalid))
                .is_err()
        );
        d.require_replacement(1, "r");
        let request = d.take_replacement_request(1, "r").unwrap();
        assert_eq!(
            (request.producer_generation, request.minimum_revision),
            (None, 0)
        );
        if reconnect {
            drop(d);
            cleanup_connection(Arc::clone(&shared), 1, std::mem::take(&mut owned), false).await;
            d = shared.lock().await;
        }
        let connection = if reconnect { 2 } else { 1 };
        let mut legacy = full("g", 1, true);
        legacy.as_object_mut().unwrap().remove("workActivity");
        assert!(
            d.producer_message(
                connection,
                &mut owned,
                RUNTIME_UPDATE_STATE_METHOD,
                Some(legacy)
            )
            .is_err(),
            "advertised capability must survive a missing generation (reconnect={reconnect})"
        );
        send(
            &mut d,
            connection,
            &mut owned,
            RUNTIME_EVENT_METHOD,
            event("turn/started", "inProgress"),
        );
        assert!(d.snapshot().runtimes.is_empty());
        send(
            &mut d,
            connection,
            &mut owned,
            RUNTIME_UPDATE_STATE_METHOD,
            full("g", 1, true),
        );
        assert_eq!(
            d.snapshot().runtimes[0]
                .work_activity
                .as_ref()
                .unwrap()
                .items,
            ["review:0".to_owned()].into()
        );
    }
}

#[tokio::test]
async fn negotiated_contract_real_idle_producer_delivers_a_later_keyed_outcome() {
    use crate::producer::CodexdProducerClient;
    use crate::producer::RuntimeMetadata;
    use tokio::time::Duration;
    use tokio::time::timeout;

    let path = PathBuf::from(format!("/tmp/cdx-{}.sock", uuid::Uuid::new_v4()));
    let listener = UnixListener::bind(&path).unwrap();
    let (_work, owners) = tokio::sync::watch::channel(BTreeMap::new());
    let state = Arc::new(Mutex::new(DaemonState::default()));
    let (tx, mut rx) = mpsc::unbounded_channel();
    state.lock().await.add_subscriber(99, tx, None).unwrap();
    let client = CodexdProducerClient::spawn_with_socket_path(
        path.clone(),
        RuntimeMetadata {
            runtime_id: "r".into(),
            pid: None,
            session_source: None,
            cwd: None,
            display_name: None,
        },
        owners,
    );
    let (stream, _) = listener.accept().await.unwrap();
    tokio::fs::remove_file(path).await.unwrap();
    let handler = tokio::spawn(handle_connection(Arc::clone(&state), stream));
    let next =
        |line: String| serde_json::from_str::<JsonValue>(&line).unwrap()["params"]["event"].clone();
    let initial = next(
        timeout(Duration::from_secs(2), rx.recv())
            .await
            .unwrap()
            .unwrap(),
    );
    assert_eq!(initial["runtime"]["workActivity"]["items"], json!([]));
    for status in ["completed", "failed"] {
        let note = serde_json::from_value(event("turn/completed", status)["notification"].clone())
            .unwrap();
        assert!(client.try_publish_hub_notification(note));
    }
    assert!(client.try_publish_hub_notification(HubNotification {
        method: "fixture/barrier".into(),
        params: None
    }));
    let mut outcomes = Vec::new();
    loop {
        let e = next(
            timeout(Duration::from_secs(2), rx.recv())
                .await
                .unwrap()
                .unwrap(),
        );
        if e["type"] == "runtimeUpsert" {
            assert_eq!(e["runtime"]["workActivity"]["items"], json!([]));
        } else if e["notification"]["method"] == "fixture/barrier" {
            break;
        } else if e["notification"]["method"] == "turn/completed" {
            assert_eq!(e["notification"]["params"]["turnKey"], "review:0");
            outcomes.push(e["notification"]["params"]["turn"]["status"].clone());
        }
    }
    client.shutdown().await;
    timeout(Duration::from_secs(2), handler)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(
        outcomes,
        [json!("completed"), json!("failed")],
        "unchanged idle work must not hide a later terminal"
    );
}

#[tokio::test]
async fn registered_reconnect_requires_full_state_after_a_stale_header_or_first_delta() {
    for first_delta in [false, true] {
        let shared = Arc::new(Mutex::new(DaemonState::default()));
        let mut d = shared.lock().await;
        let mut old = ConnectionClaims::default();
        send(
            &mut d,
            1,
            &mut old,
            RUNTIME_UPDATE_STATE_METHOD,
            full("g", 10, false),
        );
        drop(d);
        cleanup_connection(Arc::clone(&shared), 1, old, false).await;
        let mut d = shared.lock().await;
        let mut new = ConnectionClaims::default();
        send(
            &mut d,
            2,
            &mut new,
            RUNTIME_REGISTER_METHOD,
            json!({"runtimeId":"r"}),
        );
        let request = if first_delta {
            let mut delta = event("turn/started", "inProgress");
            delta["workActivity"] = full("g", 11, true)["workActivity"].clone();
            send(&mut d, 2, &mut new, RUNTIME_EVENT_METHOD, delta)
        } else {
            send(
                &mut d,
                2,
                &mut new,
                RUNTIME_UPDATE_STATE_METHOD,
                full("g", 9, true),
            )
        }
        .or_else(|| d.take_replacement_request(2, "r"));
        assert!(
            request.is_some(),
            "registration cannot turn a rejected first state or delta into legacy work"
        );
        let request = request.unwrap();
        assert_eq!(request.producer_generation.as_deref(), Some("g"));
        assert_eq!(request.minimum_revision, if first_delta { 11 } else { 10 });
        send(
            &mut d,
            2,
            &mut new,
            RUNTIME_REGISTER_METHOD,
            json!({"runtimeId":"r"}),
        );
        let pending = d.snapshot();
        assert!(
            pending.runtimes[0].work_activity_supported
                && pending.runtimes[0].work_activity.is_none()
        );
        send(
            &mut d,
            2,
            &mut new,
            RUNTIME_EVENT_METHOD,
            event("turn/started", "inProgress"),
        );
        assert_eq!(
            d.snapshot(),
            pending,
            "an unversioned start cannot acquire before correction"
        );
        send(
            &mut d,
            2,
            &mut new,
            RUNTIME_UPDATE_STATE_METHOD,
            full("g", 12, true),
        );
        assert_eq!(d.snapshot().runtimes[0].active_turns.len(), 1);
        send(
            &mut d,
            2,
            &mut new,
            RUNTIME_UPDATE_STATE_METHOD,
            full("g", 13, false),
        );
        assert!(d.snapshot().runtimes[0].active_turns.is_empty());
    }
}

#[tokio::test]
async fn an_earlier_never_published_socket_can_replace_a_later_metadata_peer() {
    let shared = Arc::new(Mutex::new(DaemonState::default()));
    let (mut a, mut b) = (ConnectionClaims::default(), ConnectionClaims::default());
    let mut d = shared.lock().await;
    send(
        &mut d,
        2,
        &mut b,
        RUNTIME_UPDATE_METADATA_METHOD,
        json!({"runtimeId":"r","displayName":"metadata first"}),
    );
    let result = d.producer_message(
        1,
        &mut a,
        RUNTIME_UPDATE_STATE_METHOD,
        Some(full("g", 10, true)),
    );
    assert!(
        result.is_ok(),
        "connection age is not evidence of a displaced claim: {:?}",
        result.err()
    );
    let busy = d.snapshot();
    assert_eq!(busy.runtimes[0].active_turns.len(), 1);
    assert!(
        d.producer_message(
            2,
            &mut b,
            RUNTIME_UPDATE_STATE_METHOD,
            Some(full("g", 30, false))
        )
        .is_err()
    );
    assert!(
        d.producer_message(
            2,
            &mut b,
            RUNTIME_UNREGISTER_METHOD,
            Some(json!({"runtimeId":"r"}))
        )
        .is_err()
    );
    drop(d);
    cleanup_connection(Arc::clone(&shared), 2, b, false).await;
    let mut d = shared.lock().await;
    assert_eq!(
        d.snapshot(),
        busy,
        "the displaced metadata socket's close cannot remove the owner"
    );
    send(
        &mut d,
        1,
        &mut a,
        RUNTIME_UPDATE_STATE_METHOD,
        full("g", 11, false),
    );
    assert!(d.snapshot().runtimes[0].active_turns.is_empty());
}

#[tokio::test]
async fn an_unpublished_repair_can_finish_on_its_original_socket() {
    for (close_challenger, higher_header) in [(true, false), (false, false), (true, true)] {
        let shared = Arc::new(Mutex::new(DaemonState::default()));
        let (mut first, mut peer) = (ConnectionClaims::default(), ConnectionClaims::default());
        let mut d = shared.lock().await;
        let mut invalid = full("g", 10, true);
        invalid.as_object_mut().unwrap().remove("activeTurns");
        assert!(
            d.producer_message(
                1,
                &mut first,
                RUNTIME_UPDATE_STATE_METHOD,
                Some(invalid.clone())
            )
            .is_err()
        );
        d.require_replacement(1, "r");
        assert_eq!(
            d.take_replacement_request(1, "r").unwrap().minimum_revision,
            10
        );
        let absent = d.snapshot();
        let request = send(
            &mut d,
            2,
            &mut peer,
            RUNTIME_UPDATE_METADATA_METHOD,
            json!({"runtimeId":"r","displayName":"not an owner"}),
        )
        .or_else(|| d.take_replacement_request(2, "r"));
        assert_eq!(request.unwrap().minimum_revision, 10);
        assert!(absent.runtimes.is_empty());
        assert_eq!(d.snapshot(), absent);
        if higher_header {
            invalid["workActivity"]["revision"] = 30.into();
            assert_eq!(
                send(&mut d, 2, &mut peer, RUNTIME_UPDATE_STATE_METHOD, invalid)
                    .unwrap()
                    .minimum_revision,
                30
            );
        }
        if close_challenger {
            drop(d);
            cleanup_connection(Arc::clone(&shared), 2, peer, false).await;
            d = shared.lock().await;
        }
        let correction = d.producer_message(
            1,
            &mut first,
            RUNTIME_UPDATE_STATE_METHOD,
            Some(full("g", 11, true)),
        );
        assert!(
            correction.is_ok(),
            "metadata must not supersede the first repair socket: {:?}",
            correction.err()
        );
        assert_eq!(d.snapshot().runtimes[0].active_turns.len(), 1);
        send(
            &mut d,
            1,
            &mut first,
            RUNTIME_UPDATE_STATE_METHOD,
            full("g", 12, false),
        );
        assert!(d.snapshot().runtimes[0].active_turns.is_empty());
    }
}

#[test]
fn legacy_turns_use_the_same_explicit_key_on_start_and_completion() {
    let mut d = DaemonState::default();
    let mut legacy = ConnectionClaims::default();
    send(
        &mut d,
        1,
        &mut legacy,
        RUNTIME_REGISTER_METHOD,
        json!({"runtimeId":"r"}),
    );
    for (start_nested, end_nested) in [(false, false), (false, true), (true, false)] {
        let msg = |method, nested, status| {
            let mut v = event(method, status);
            let p = &mut v["notification"]["params"];
            p.as_object_mut().unwrap().remove("turnKey");
            if nested {
                p["turn"]["key"] = "opaque-7".into()
            } else {
                p["turnKey"] = "opaque-7".into()
            }
            v
        };
        send(
            &mut d,
            1,
            &mut legacy,
            RUNTIME_EVENT_METHOD,
            msg("turn/started", start_nested, "inProgress"),
        );
        let busy = d.snapshot();
        send(
            &mut d,
            1,
            &mut legacy,
            RUNTIME_EVENT_METHOD,
            msg("turn/completed", end_nested, "completed"),
        );
        assert!(
            d.snapshot().runtimes[0].active_turns.is_empty(),
            "an explicit legacy key must release without EOF"
        );
        assert_eq!(
            busy.runtimes[0].active_turns[0].turn_key.as_deref(),
            Some("opaque-7")
        );
    }
}

#[tokio::test]
async fn bare_legacy_terminal_checks_its_attached_key_before_removal() {
    let shared = Arc::new(Mutex::new(DaemonState::default()));
    let mut old = ConnectionClaims::default();
    let mut d = shared.lock().await;
    send(
        &mut d,
        1,
        &mut old,
        RUNTIME_UPDATE_STATE_METHOD,
        full("g", 10, true),
    );
    let mut failure = event("turn/completed", "failed");
    failure["workActivity"] = full("g", 13, false)["workActivity"].clone();
    send(&mut d, 1, &mut old, RUNTIME_EVENT_METHOD, failure);
    drop(d);
    cleanup_connection(Arc::clone(&shared), 1, old, false).await;
    let mut d = shared.lock().await;
    let mut legacy = ConnectionClaims::default();
    send(
        &mut d,
        2,
        &mut legacy,
        RUNTIME_REGISTER_METHOD,
        json!({"runtimeId":"r"}),
    );
    let bare = json!({"runtimeId":"r","notification":{"method":"turn/completed","params":{"turn":{"id":"0","status":"completed"}}}});
    for with_status in [true, false] {
        send(
            &mut d,
            2,
            &mut legacy,
            RUNTIME_EVENT_METHOD,
            event("turn/started", "inProgress"),
        );
        let before = d.seq;
        let mut done = bare.clone();
        if !with_status {
            done["notification"]["params"]["turn"]
                .as_object_mut()
                .unwrap()
                .remove("status");
        }
        send(&mut d, 2, &mut legacy, RUNTIME_EVENT_METHOD, done);
        assert!(d.snapshot().runtimes[0].active_turns.is_empty());
        assert_eq!(
            events(&d, before).len(),
            1,
            "a bare completion must release without downgrading the versioned archive"
        );
        assert_eq!(events(&d, before)[0]["type"], "runtimeUpsert");
        let idle = d.seq;
        send(&mut d, 2, &mut legacy, RUNTIME_EVENT_METHOD, bare.clone());
        assert_eq!(
            d.seq, idle,
            "a bare duplicate cannot target a detached archive"
        );
    }
    let start = |thread: &str, key: &str| {
        json!({"runtimeId":"r","notification":{"method":"turn/started",
        "params":{"threadId":thread,"turnKey":key,"turn":{"id":"0"}}}})
    };
    send(
        &mut d,
        2,
        &mut legacy,
        RUNTIME_EVENT_METHOD,
        start("left", "opaque-left"),
    );
    send(
        &mut d,
        2,
        &mut legacy,
        RUNTIME_EVENT_METHOD,
        start("right", "opaque-right"),
    );
    let before = d.snapshot();
    send(&mut d, 2, &mut legacy, RUNTIME_EVENT_METHOD, bare.clone());
    assert_eq!(
        d.snapshot(),
        before,
        "an ambiguous bare ID cannot target either sibling"
    );
    let mut scoped = bare.clone();
    scoped["notification"]["params"]["threadId"] = "left".into();
    send(&mut d, 2, &mut legacy, RUNTIME_EVENT_METHOD, scoped);
    let last = events(&d, before.seq);
    assert_eq!(last[1]["notification"]["params"]["turnKey"], "opaque-left");
    let before = d.seq;
    send(&mut d, 2, &mut legacy, RUNTIME_EVENT_METHOD, bare);
    let last = events(&d, before);
    assert_eq!(last[1]["notification"]["params"]["turnKey"], "opaque-right");
    assert_eq!(last[1]["notification"]["params"]["threadId"], "right");
}

#[tokio::test]
async fn repair_recipient_does_not_own_the_runtime_in_either_close_order() {
    for (owner_first, high_header) in [(false, false), (true, false), (false, true)] {
        let state = Arc::new(Mutex::new(DaemonState::default()));
        let (mut owner, mut challenger) =
            (ConnectionClaims::default(), ConnectionClaims::default());
        let mut d = state.lock().await;
        send(
            &mut d,
            1,
            &mut owner,
            RUNTIME_UPDATE_STATE_METHOD,
            full("g", 10, true),
        );
        let mut bad = full("g", 12, true);
        bad.as_object_mut().unwrap().remove("activeTurns");
        assert!(
            d.producer_message(1, &mut owner, RUNTIME_UPDATE_STATE_METHOD, Some(bad))
                .is_err()
        );
        d.require_replacement(1, "r");
        assert_eq!(
            d.take_replacement_request(1, "r").unwrap().minimum_revision,
            12
        );
        let pending = d.snapshot();
        let request = send(
            &mut d,
            2,
            &mut challenger,
            RUNTIME_UPDATE_METADATA_METHOD,
            json!({"runtimeId":"r","displayName":"challenger"}),
        )
        .or_else(|| d.take_replacement_request(2, "r"));
        assert_eq!(request.unwrap().minimum_revision, 12);
        if high_header {
            let mut incomplete = full("g", 30, true);
            incomplete.as_object_mut().unwrap().remove("activeTurns");
            let request = send(
                &mut d,
                2,
                &mut challenger,
                RUNTIME_UPDATE_STATE_METHOD,
                incomplete,
            );
            assert_eq!(request.unwrap().minimum_revision, 30);
        }
        drop(d);
        if owner_first {
            cleanup_connection(Arc::clone(&state), 1, owner, false).await;
            let mut d = state.lock().await;
            assert!(
                d.snapshot().runtimes.is_empty(),
                "a metadata-only socket cannot keep the old owner alive"
            );
            send(
                &mut d,
                2,
                &mut challenger,
                RUNTIME_UPDATE_STATE_METHOD,
                full("g", 13, true),
            );
            assert_eq!(d.snapshot().runtimes[0].active_turns.len(), 1);
        } else {
            cleanup_connection(Arc::clone(&state), 2, challenger, false).await;
            let mut d = state.lock().await;
            assert_eq!(
                d.snapshot(),
                pending,
                "closing the repair recipient must not revoke accepted work"
            );
            send(
                &mut d,
                1,
                &mut owner,
                RUNTIME_UPDATE_STATE_METHOD,
                full("g", 13, true),
            );
            assert_eq!(
                d.snapshot().runtimes[0]
                    .work_activity
                    .as_ref()
                    .unwrap()
                    .revision,
                13
            );
            send(
                &mut d,
                1,
                &mut owner,
                RUNTIME_UPDATE_STATE_METHOD,
                full("g", 14, false),
            );
            assert!(d.snapshot().runtimes[0].active_turns.is_empty());
        }
    }
}

#[tokio::test]
async fn versioned_archive_does_not_keep_a_reused_legacy_turn_attached() {
    let state = Arc::new(Mutex::new(DaemonState::default()));
    let mut owner = ConnectionClaims::default();
    let mut d = state.lock().await;
    send(
        &mut d,
        1,
        &mut owner,
        RUNTIME_UPDATE_STATE_METHOD,
        full("g", 10, true),
    );
    let mut failure = event("turn/completed", "failed");
    failure["workActivity"] = full("g", 13, false)["workActivity"].clone();
    send(&mut d, 1, &mut owner, RUNTIME_EVENT_METHOD, failure);
    drop(d);
    cleanup_connection(Arc::clone(&state), 1, owner, false).await;
    let mut d = state.lock().await;
    let mut legacy = ConnectionClaims::default();
    send(
        &mut d,
        2,
        &mut legacy,
        RUNTIME_REGISTER_METHOD,
        json!({"runtimeId":"r"}),
    );
    send(
        &mut d,
        2,
        &mut legacy,
        RUNTIME_EVENT_METHOD,
        event("turn/started", "inProgress"),
    );
    assert_eq!(d.snapshot().runtimes[0].active_turns.len(), 1);
    let start = d.seq;
    send(
        &mut d,
        2,
        &mut legacy,
        RUNTIME_EVENT_METHOD,
        event("turn/completed", "completed"),
    );
    assert!(
        d.snapshot().runtimes[0].active_turns.is_empty(),
        "archive ordering cannot keep a completed legacy turn busy"
    );
    let out = events(&d, start);
    assert_eq!(
        out.len(),
        1,
        "publish the idle projection without overwriting the versioned outcome"
    );
    assert_eq!(out[0]["type"], "runtimeUpsert");
    assert_eq!(out[0]["runtime"]["activeTurns"], json!([]));
}

#[test]
fn scoped_repair_terminal_keeps_the_attached_opaque_key() {
    let mut d = DaemonState::default();
    let mut owner = ConnectionClaims::default();
    let mut busy = full("g", 10, true);
    busy["activeTurns"][0]["turnKey"] = "opaque-review".into();
    send(&mut d, 1, &mut owner, RUNTIME_UPDATE_STATE_METHOD, busy);
    d.require_replacement(1, "r");
    let mut failed = event("turn/completed", "failed");
    failed["notification"]["params"]
        .as_object_mut()
        .unwrap()
        .remove("turnKey");
    let before = d.seq;
    send(
        &mut d,
        2,
        &mut ConnectionClaims::default(),
        RUNTIME_EVENT_METHOD,
        failed,
    );
    assert_eq!(
        d.seq, before,
        "thread-scoped repair detail must wait for the attached opaque turn"
    );
    send(
        &mut d,
        1,
        &mut owner,
        RUNTIME_UPDATE_STATE_METHOD,
        full("g", 11, false),
    );
    let out = events(&d, before);
    assert_eq!(out.len(), 2);
    assert_eq!(out[0]["type"], "runtimeUpsert");
    assert_eq!(out[1]["type"], "runtimeArchiveDetail");
    assert_eq!(out[1]["notification"]["params"]["turnKey"], "opaque-review");
}

#[tokio::test]
async fn replayed_recovery_epochs_do_not_override_newer_outcomes_after_eof() {
    let recovery = |g, n, epoch, status| {
        let mut f = full(g, n, false);
        f["historyRecovery"] = json!({"epoch":epoch,"terminals":[{"turnKey":"review:0","threadId":"review","turnId":"0","status":status}]});
        f
    };
    for alternate in [false, true] {
        let state = Arc::new(Mutex::new(DaemonState::default()));
        let mut owner = ConnectionClaims::default();
        let mut d = state.lock().await;
        send(
            &mut d,
            1,
            &mut owner,
            RUNTIME_UPDATE_STATE_METHOD,
            recovery("g", 11, 3, "completed"),
        );
        let mut failed = event("turn/completed", "failed");
        failed["workActivity"] = full("g", 13, false)["workActivity"].clone();
        send(&mut d, 1, &mut owner, RUNTIME_EVENT_METHOD, failed);
        assert_eq!(
            events(&d, 0).last().unwrap()["notification"]["params"]["turn"]["status"],
            "failed"
        );
        drop(d);
        cleanup_connection(Arc::clone(&state), 1, owner, false).await;
        let mut d = state.lock().await;
        if alternate {
            let start = d.seq;
            send(
                &mut d,
                2,
                &mut ConnectionClaims::default(),
                RUNTIME_UPDATE_STATE_METHOD,
                recovery("h", 1, 1, "interrupted"),
            );
            assert_eq!(
                events(&d, start).len(),
                3,
                "a distinct generation has its own epoch"
            );
        }
        let mut replacement = ConnectionClaims::default();
        for (revision, epoch) in [(15, 3), (16, 2)] {
            let start = d.seq;
            send(
                &mut d,
                3,
                &mut replacement,
                RUNTIME_UPDATE_STATE_METHOD,
                recovery("g", revision, epoch, "completed"),
            );
            let out = events(&d, start);
            assert_eq!(
                out.len(),
                1,
                "a replayed recovery epoch can only refresh the projection"
            );
            assert_eq!(out[0]["type"], "runtimeUpsert");
        }
        let start = d.seq;
        send(
            &mut d,
            3,
            &mut replacement,
            RUNTIME_UPDATE_STATE_METHOD,
            recovery("g", 17, 4, "interrupted"),
        );
        let out = events(&d, start);
        assert_eq!(out.len(), 3);
        assert_eq!(out[0]["notification"]["method"], "runtime/historyLost");
        assert_eq!(
            out[2]["notification"]["params"]["turn"]["status"],
            "interrupted"
        );
    }
}
