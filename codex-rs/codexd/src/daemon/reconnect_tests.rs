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

#[test]
fn unordered_legacy_full_shares_a_start_rank_and_keeps_reported_times() {
    let (mut daemon, mut claims) = (DaemonState::default(), ConnectionClaims::default());
    let mut turns = vec![
        json!({"threadId":"t","turnId":"9","startedAt":20}),
        json!({"threadId":"t","turnId":"10","startedAt":10}),
    ];
    for _ in 0..2 {
        send(
            &mut daemon,
            1,
            &mut claims,
            RUNTIME_UPDATE_STATE_METHOD,
            json!({"runtimeId":"r","activeTurns":turns}),
        );
        let output = daemon.snapshot();
        let orders: Vec<_> = output.runtimes[0]
            .active_turns
            .iter()
            .map(|t| (t.turn_id.as_str(), t.start_order, t.started_at))
            .collect();
        assert_eq!(
            orders,
            vec![("10", Some(1), Some(10)), ("9", Some(1), Some(20))]
        );
        turns.reverse();
    }
}

#[test]
fn observed_start_order_survives_key_sorted_full_states_and_duplicate_starts() {
    let (mut source, mut c) = (DaemonState::default(), ConnectionClaims::default());
    let start = |id| {
        json!({"runtimeId":"r","notification":{"method":"turn/started","params":{
        "threadId":"T","turn":{"id":id,"startedAt":1700000000}}}})
    };
    for id in ["9", "10", "9"] {
        send(&mut source, 1, &mut c, RUNTIME_EVENT_METHOD, start(id));
    }
    let full = serde_json::to_value(&source.snapshot().runtimes[0]).unwrap();
    let position = |turns: &JsonValue, id: &str| {
        turns
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["turnId"] == id)
            .unwrap()["startOrder"]
            .as_u64()
    };
    assert_eq!(position(&full["activeTurns"], "9"), Some(1));
    assert_eq!(position(&full["activeTurns"], "10"), Some(2));
    let starts: Vec<_> = events(&source, 0)
        .into_iter()
        .filter(|e| e["notification"]["method"] == "turn/started")
        .map(|e| e["notification"]["params"]["turn"]["startOrder"].as_u64())
        .collect();
    assert_eq!(starts, vec![Some(1), Some(2), Some(1)]);
    let (mut relay, mut c) = (DaemonState::default(), ConnectionClaims::default());
    send(&mut relay, 1, &mut c, RUNTIME_UPDATE_STATE_METHOD, full);
    // A legacy replay has neither position nor timestamp; it is not another start.
    let replay = json!({"runtimeId":"r","activeTurns":[{"threadId":"T","turnId":"10"},{"threadId":"T","turnId":"9"}]});
    send(&mut relay, 1, &mut c, RUNTIME_UPDATE_STATE_METHOD, replay);
    send(&mut relay, 1, &mut c, RUNTIME_EVENT_METHOD, start("1"));
    let output = serde_json::to_value(&relay.snapshot().runtimes[0]).unwrap();
    for (id, ordinal) in [("9", 1), ("10", 2), ("1", 3)] {
        assert_eq!(position(&output["activeTurns"], id), Some(ordinal));
    }
    assert!(
        output["activeTurns"]
            .as_array()
            .unwrap()
            .iter()
            .all(|t| t["startedAt"] == 1700000000)
    );
}

#[test]
fn origin_contract_equal_inferred_and_explicit_totals_round_trip_differently() {
    for explicit in [false, true] {
        let (mut d, mut c) = (DaemonState::default(), ConnectionClaims::default());
        let usage = json!({"total":{"totalTokens":20}});
        let state = |turn| json!({"runtimeId":"r","activeTurns":[turn]});
        send(
            &mut d,
            1,
            &mut c,
            RUNTIME_UPDATE_STATE_METHOD,
            state(json!({"threadId":"T","turnId":"A","turnKey":"opaque:A","tokenUsage":usage})),
        );
        send(
            &mut d,
            1,
            &mut c,
            RUNTIME_UPDATE_STATE_METHOD,
            json!({"runtimeId":"r","activeTurns":[]}),
        );
        let mut b = json!({"threadId":"T","turnId":"B","turnKey":"opaque:B"});
        if explicit {
            b["tokenUsageBaseline"] = usage.clone();
        }
        send(&mut d, 1, &mut c, RUNTIME_UPDATE_STATE_METHOD, state(b));
        let wire = serde_json::to_value(&d.snapshot().runtimes[0]).unwrap();
        for _ in 0..2 {
            send(&mut d, 1, &mut c, RUNTIME_UPDATE_STATE_METHOD, wire.clone());
            let turn = serde_json::to_value(&d.snapshot().runtimes[0].active_turns[0]).unwrap();
            assert_eq!(turn["tokenUsageBaseline"], usage);
            assert_eq!(
                turn["tokenUsageBaselinePredecessorKey"].as_str(),
                (!explicit).then_some("opaque:A"),
                "a number alone cannot identify a refinable baseline"
            );
        }
    }
}

#[test]
fn origin_contract_terminal_payload_seeds_the_next_turn_like_a_usage_event() {
    for missed in [false, true] {
        let (mut d, mut c) = (DaemonState::default(), ConnectionClaims::default());
        let usage = json!({"total":{"totalTokens":100},"last":{"totalTokens":40}});
        let turns = if missed {
            vec![]
        } else {
            vec![json!({"threadId":"T","turnId":"A","turnKey":"opaque:A"})]
        };
        send(
            &mut d,
            1,
            &mut c,
            RUNTIME_UPDATE_STATE_METHOD,
            json!({"runtimeId":"r","activeTurns":turns}),
        );
        send(
            &mut d,
            1,
            &mut c,
            RUNTIME_EVENT_METHOD,
            json!({"runtimeId":"r","notification":{"method":"turn/completed","params":{"threadId":"T","turn":{"id":"A","key":"opaque:A","status":"completed","tokenUsage":usage}}}}),
        );
        assert!(d.snapshot().runtimes[0].active_turns.is_empty());
        send(
            &mut d,
            1,
            &mut c,
            RUNTIME_EVENT_METHOD,
            json!({"runtimeId":"r","notification":{"method":"turn/started","params":{"threadId":"T","turn":{"id":"B","key":"opaque:B"}}}}),
        );
        assert_eq!(
            d.snapshot().runtimes[0].active_turns[0].token_usage_baseline,
            Some(usage),
            "terminal-only final sample"
        );
    }
}

#[test]
fn start_deltas_publish_the_resolved_boundary_not_the_unmerged_input() {
    let mut runtime = RuntimeState::default();
    let mut start = |origin: JsonValue| {
        let mut turn = json!({"id":"B"});
        turn.as_object_mut()
            .unwrap()
            .extend(origin.as_object().unwrap().clone());
        let mut n = HubNotification {
            method: "turn/started".into(),
            params: Some(json!({"threadId":"T","turn":turn})),
        };
        assert_eq!(
            apply_notification_to_runtime(&mut runtime, &mut n),
            ProjectionChange::LiveTurn
        );
        n.params.unwrap()["turn"].clone()
    };
    let total = json!({"total":{"totalTokens":20}});
    let fixed = start(json!({"tokenUsageBaseline":total}));
    assert_eq!(
        fixed,
        json!({"id":"B","startOrder":1,"tokenUsageBaseline":total,"tokenUsageBaselineKnown":true})
    );
    assert_eq!(
        start(json!({"tokenUsageBaseline": total, "tokenUsageBaselinePredecessorKey":"A"})),
        fixed
    );
    // An authoritative empty boundary clears a speculative value; a headerless replay keeps it.
    let absent = start(json!({"tokenUsageBaselineKnown":true}));
    assert_eq!(
        absent,
        json!({"id":"B","startOrder":1,"tokenUsageBaselineKnown":true})
    );
    assert_eq!(start(json!({})), absent);
}

#[test]
fn origin_contract_join_time_does_not_change_a_published_start() {
    let usage = |n| json!({"total":{"totalTokens":n}});
    let a = json!({"threadId":"T","turnId":"A","tokenUsage":usage(100)});
    let b = json!({"threadId":"T","turnId":"B","tokenUsage":usage(140)});
    let sibling = json!({"threadId":"T","turnId":"C"});
    for sequential in [false, true] {
        let mut producer = DaemonState::default();
        let mut claims = ConnectionClaims::default();
        let project = |d: &mut DaemonState, c: &mut ConnectionClaims, turns: Vec<JsonValue>| {
            send(
                d,
                1,
                c,
                RUNTIME_UPDATE_STATE_METHOD,
                json!({"runtimeId":"r","activeTurns":turns}),
            );
        };
        project(&mut producer, &mut claims, vec![a.clone()]);
        if sequential {
            project(&mut producer, &mut claims, vec![]);
            project(&mut producer, &mut claims, vec![b.clone()]);
            project(&mut producer, &mut claims, vec![b.clone(), sibling.clone()]);
        } else {
            project(&mut producer, &mut claims, vec![a.clone(), b.clone()]);
            project(&mut producer, &mut claims, vec![b.clone()]);
        }
        let wire = serde_json::to_value(&producer.snapshot().runtimes[0]).unwrap();
        for cached in [false, true] {
            let (mut relay, mut c) = (DaemonState::default(), ConnectionClaims::default());
            if cached {
                project(&mut relay, &mut c, vec![a.clone()]);
                project(&mut relay, &mut c, vec![]);
            }
            send(
                &mut relay,
                1,
                &mut c,
                RUNTIME_UPDATE_STATE_METHOD,
                wire.clone(),
            );
            let output = serde_json::to_value(&relay.snapshot().runtimes[0]).unwrap();
            let turn = output["activeTurns"]
                .as_array()
                .unwrap()
                .iter()
                .find(|t| t["turnId"] == "B")
                .unwrap();
            assert_eq!(
                turn["tokenUsageBaseline"].as_object(),
                sequential
                    .then(|| usage(100))
                    .as_ref()
                    .and_then(JsonValue::as_object)
            );
            assert_eq!(
                turn["tokenUsageBaselinePredecessorKey"].as_str(),
                sequential.then_some("T:A")
            );
            assert_eq!(turn["tokenUsageBaselineKnown"], true);
        }
    }
}

#[test]
fn active_sibling_usage_keeps_local_rounds_and_the_thread_high_water() {
    let (mut d, mut c) = (DaemonState::default(), ConnectionClaims::default());
    let usage = |n| json!({"total":{"totalTokens":n},"last":{"totalTokens":n}});
    let turn = |id| json!({"threadId":"T","turnId":id});
    let full = |turns| json!({"runtimeId":"r","activeTurns":turns});
    send(
        &mut d,
        1,
        &mut c,
        RUNTIME_UPDATE_STATE_METHOD,
        full(vec![turn("Z"), turn("B")]),
    );
    // Z sorts last on replay; sample order must not decide the shared total either.
    for (id, n) in [("B", 140), ("Z", 100)] {
        send(
            &mut d,
            1,
            &mut c,
            RUNTIME_EVENT_METHOD,
            json!({"runtimeId":"r","notification":{
            "method":"thread/tokenUsage/updated","params":{"threadId":"T","turnId":id,"tokenUsage":usage(n)}}}),
        );
    }
    let both = serde_json::to_value(&d.snapshot().runtimes[0]).unwrap();
    send(&mut d, 1, &mut c, RUNTIME_UPDATE_STATE_METHOD, both);
    assert_eq!(d.runtimes["r"].thread_token_usage["T"], usage(140));
    assert_eq!(
        d.runtimes["r"].active_turns["T:Z"].token_usage,
        Some(usage(100))
    );
    send(
        &mut d,
        1,
        &mut c,
        RUNTIME_EVENT_METHOD,
        event_for_completion("B"),
    );
    let only_z = serde_json::to_value(&d.snapshot().runtimes[0]).unwrap();
    assert_eq!(only_z["activeTurns"][0]["threadTokenUsage"], usage(140));
    let (mut fresh, mut claims) = (DaemonState::default(), ConnectionClaims::default());
    send(
        &mut fresh,
        1,
        &mut claims,
        RUNTIME_UPDATE_STATE_METHOD,
        only_z,
    );
    send(
        &mut fresh,
        1,
        &mut claims,
        RUNTIME_EVENT_METHOD,
        event_for_completion("Z"),
    );
    send(
        &mut fresh,
        1,
        &mut claims,
        RUNTIME_UPDATE_STATE_METHOD,
        full(vec![turn("next")]),
    );
    assert_eq!(
        fresh.runtimes["r"].active_turns["T:next"].token_usage_baseline,
        Some(usage(140))
    );
}

fn event_for_completion(id: &str) -> JsonValue {
    json!({"runtimeId":"r","notification":{"method":"turn/completed","params":{"threadId":"T","turn":{"id":id,"status":"completed"}}}})
}

#[test]
fn origin_contract_overlap_excludes_a_running_siblings_latest_total() {
    for full in [false, true] {
        let (mut d, mut c) = (DaemonState::default(), ConnectionClaims::default());
        let a = json!({"threadId":"T","turnId":"A","turnKey":"opaque:A","tokenUsage":{"total":{"totalTokens":100}}});
        send(
            &mut d,
            1,
            &mut c,
            RUNTIME_UPDATE_STATE_METHOD,
            json!({"runtimeId":"r","activeTurns":[a]}),
        );
        if full {
            send(
                &mut d,
                1,
                &mut c,
                RUNTIME_UPDATE_STATE_METHOD,
                json!({"runtimeId":"r","activeTurns":[a,{"threadId":"T","turnId":"B","turnKey":"opaque:B"}]}),
            );
        } else {
            send(
                &mut d,
                1,
                &mut c,
                RUNTIME_EVENT_METHOD,
                json!({"runtimeId":"r","notification":{"method":"turn/started","params":{"threadId":"T","turn":{"id":"B","key":"opaque:B"}}}}),
            );
        }
        let snapshot = d.snapshot();
        let b = snapshot.runtimes[0]
            .active_turns
            .iter()
            .find(|t| t.turn_id == "B")
            .unwrap();
        assert!(
            b.token_usage_baseline.is_none(),
            "overlap must be checked before reading the thread total"
        );
    }
}

#[test]
fn origin_contract_opaque_terminal_is_published_once_after_canonical_release() {
    assert_terminal_waits_for_owner(false);
}

#[test]
fn key_only_terminal_uses_the_previously_observed_thread_until_release() {
    assert_terminal_waits_for_owner(true);
}

fn assert_terminal_waits_for_owner(key_only: bool) {
    let (mut d, mut c) = (DaemonState::default(), ConnectionClaims::default());
    let mut state = full("G", 10, false);
    state["workActivity"]["items"] = json!(["T:A"]);
    state["workActivity"]["runningTurnKeys"] = json!(["T:A"]);
    let mut terminal = json!({"turnKey":"opaque:A","turnId":"A","status":"failed"});
    if key_only {
        let mut visible = state.clone();
        visible["workActivity"]["revision"] = json!(8);
        visible["activeTurns"] = json!([{"turnKey":"opaque:A","threadId":"T","turnId":"A"}]);
        send(&mut d, 1, &mut c, RUNTIME_UPDATE_STATE_METHOD, visible);
        let mut hidden = state.clone();
        hidden["workActivity"]["revision"] = json!(9);
        send(&mut d, 1, &mut c, RUNTIME_UPDATE_STATE_METHOD, hidden);
    } else {
        terminal["threadId"] = json!("T");
    }
    state["historyRecovery"] = json!({"epoch":1,"terminals":[terminal]});
    send(
        &mut d,
        1,
        &mut c,
        RUNTIME_UPDATE_STATE_METHOD,
        state.clone(),
    );
    let count = |d: &DaemonState| {
        events(d, 0)
            .iter()
            .filter(|e| e["notification"]["method"] == "turn/completed")
            .count()
    };
    assert_eq!(
        count(&d),
        0,
        "recovered opaque key must wait for its canonical running owner"
    );
    state["workActivity"]["revision"] = json!(11);
    send(&mut d, 1, &mut c, RUNTIME_UPDATE_STATE_METHOD, state);
    assert_eq!(count(&d), 0);
    let floor = d.seq;
    send(
        &mut d,
        1,
        &mut c,
        RUNTIME_UPDATE_STATE_METHOD,
        full("G", 12, false),
    );
    let end = events(&d, floor);
    assert_eq!(
        end.iter()
            .map(|e| e["type"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["runtimeUpsert", "runtimeArchiveDetail"]
    );
    send(
        &mut d,
        1,
        &mut c,
        RUNTIME_UPDATE_STATE_METHOD,
        full("G", 13, false),
    );
    assert_eq!(count(&d), 1);
}

#[test]
fn disconnected_identity_is_history_only_and_generation_scoped() {
    for generation in ["G", "H"] {
        for omitted_before_eof in [false, true] {
            let (mut d, mut first, mut next) = (
                DaemonState::default(),
                ConnectionClaims::default(),
                ConnectionClaims::default(),
            );
            let project = |g: &str, rev: u64, owners: Vec<&str>, turns: JsonValue| {
                let mut s = full(g, rev, false);
                s["activeTurns"] = turns;
                s["workActivity"]["items"] = json!(owners);
                s["workActivity"]["runningTurnKeys"] = json!(owners);
                s
            };
            let owners = vec!["T:A", "T:B"];
            send(
                &mut d,
                1,
                &mut first,
                RUNTIME_UPDATE_STATE_METHOD,
                project(
                    "G",
                    1,
                    owners.clone(),
                    json!([{"threadId":"T","turnId":"A","turnKey":"opaque:A"}]),
                ),
            );
            if omitted_before_eof {
                send(
                    &mut d,
                    1,
                    &mut first,
                    RUNTIME_UPDATE_STATE_METHOD,
                    project("G", 2, owners.clone(), json!([])),
                );
            }
            d.detach_producer("r");
            assert!(d.snapshot().runtimes.is_empty());
            let cut = d.seq;
            let mut recovery = project(generation, 3, owners, json!([]));
            recovery["historyRecovery"] = json!({"epoch":1,"terminals":[{"turnKey":"opaque:A","turnId":"A","status":"failed"}]});
            send(
                &mut d,
                2,
                &mut next,
                RUNTIME_UPDATE_STATE_METHOD,
                recovery.clone(),
            );
            let terminals = |d: &DaemonState| {
                events(d, cut)
                    .into_iter()
                    .filter(|e| e["notification"]["method"] == "turn/completed")
                    .count()
            };
            assert_eq!(
                terminals(&d),
                usize::from(generation != "G"),
                "same generation waits; an unrelated owner cannot inherit the association"
            );
            recovery["workActivity"]["revision"] = json!(4);
            send(&mut d, 2, &mut next, RUNTIME_UPDATE_STATE_METHOD, recovery);
            let release = d.seq;
            send(
                &mut d,
                2,
                &mut next,
                RUNTIME_UPDATE_STATE_METHOD,
                project(generation, 5, vec!["T:B"], json!([])),
            );
            assert_eq!(terminals(&d), 1);
            let order: Vec<_> = events(&d, release)
                .into_iter()
                .map(|e| e["type"].clone())
                .collect();
            assert_eq!(
                order,
                if generation == "G" {
                    vec![json!("runtimeUpsert"), json!("runtimeArchiveDetail")]
                } else {
                    vec![json!("runtimeUpsert")]
                }
            );
        }
    }
}

#[test]
fn paired_usage_only_binds_to_an_unambiguous_observed_key() {
    let (mut d, mut c) = (DaemonState::default(), ConnectionClaims::default());
    let state = |turns| json!({"runtimeId":"r","activeTurns":turns});
    let a = json!({"threadId":"T","turnId":"A","turnKey":"opaque-first"});
    let b = json!({"threadId":"T","turnId":"B","turnKey":"opaque-next"});
    let sample = |id: &str, n| json!({"runtimeId":"r","notification":{"method":"thread/tokenUsage/updated","params":{"threadId":"T","turnId":id,"tokenUsage":{"total":{"totalTokens":n}}}}});
    send(
        &mut d,
        1,
        &mut c,
        RUNTIME_UPDATE_STATE_METHOD,
        state(json!([a])),
    );
    send(&mut d, 1, &mut c, RUNTIME_EVENT_METHOD, sample("A", 20));
    send(
        &mut d,
        1,
        &mut c,
        RUNTIME_UPDATE_STATE_METHOD,
        state(json!([b])),
    );
    send(&mut d, 1, &mut c, RUNTIME_EVENT_METHOD, sample("B", 140));
    let cut = d.seq;
    send(&mut d, 1, &mut c, RUNTIME_EVENT_METHOD, sample("A", 100));
    let turn = &d.snapshot().runtimes[0].active_turns[0];
    assert_eq!(
        turn.token_usage_baseline.as_ref().unwrap()["total"]["totalTokens"],
        100
    );
    assert_eq!(
        events(&d, cut).last().unwrap()["notification"]["params"]["turnKey"],
        "opaque-first"
    );
    send(
        &mut d,
        1,
        &mut c,
        RUNTIME_UPDATE_STATE_METHOD,
        state(json!([{"threadId":"T","turnId":"A","turnKey":"opaque-second"},b])),
    );
    let cut = d.seq;
    send(&mut d, 1, &mut c, RUNTIME_EVENT_METHOD, sample("A", 90));
    assert!(
        events(&d, cut).is_empty(),
        "two observed keys share the pair; do not guess which one sent the usage"
    );
}

#[test]
fn recovery_journal_preserves_a_terminal_only_cumulative_sample() {
    let usage = json!({"total":{"totalTokens":100},"last":{"totalTokens":40}});
    let terminal = HubNotification {
        method: "turn/completed".into(),
        params: Some(
            json!({"threadId":"T","turn":{"id":"A","key":"opaque:A","status":"failed","tokenUsage":usage}}),
        ),
    };
    let encoded = serde_json::to_value(recovery_terminal(&terminal).unwrap()).unwrap();
    assert_eq!(encoded["tokenUsage"], usage);
    let (mut d, mut c) = (DaemonState::default(), ConnectionClaims::default());
    let mut recovered = full("G", 10, false);
    recovered["historyRecovery"] = json!({"epoch":1,"terminals":[encoded]});
    send(&mut d, 1, &mut c, RUNTIME_UPDATE_STATE_METHOD, recovered);
    assert!(d.snapshot().runtimes[0].active_turns.is_empty());
    let published = events(&d, 0);
    assert_eq!(
        published.last().unwrap()["notification"]["params"]["turn"]["tokenUsage"],
        usage
    );
    send(
        &mut d,
        1,
        &mut c,
        RUNTIME_EVENT_METHOD,
        json!({"runtimeId":"r","workActivity":{"producerGeneration":"G","revision":11,"items":["T:B"],"runningTurnKeys":["T:B"]},"notification":{"method":"turn/started","params":{"threadId":"T","turn":{"id":"B","key":"opaque:B"}}}}),
    );
    assert_eq!(
        d.snapshot().runtimes[0].active_turns[0].token_usage_baseline,
        Some(usage)
    );
}

#[test]
fn bundled_recovery_can_bind_a_successor_but_not_a_still_running_sibling() {
    for attached in [false, true] {
        let mut d = DaemonState::default();
        let mut c = ConnectionClaims::default();
        let mut state = full("G", 10, false);
        state["activeTurns"] = json!([{"turnId":"B","threadId":"T","turnKey":"opaque:B"}]);
        let keys = if attached {
            vec!["T:A", "T:B"]
        } else {
            vec!["T:B"]
        };
        state["workActivity"]["items"] = json!(keys);
        state["workActivity"]["runningTurnKeys"] = json!(keys);
        state["historyRecovery"] = json!({"epoch":1,"terminals":[{"turnKey":"opaque:A","threadId":"T","turnId":"A","status":"completed"}]});
        send(&mut d, 1, &mut c, RUNTIME_UPDATE_STATE_METHOD, state);
        let work = d.snapshot().runtimes[0].work_activity.clone();
        send(
            &mut d,
            1,
            &mut c,
            RUNTIME_EVENT_METHOD,
            json!({"runtimeId":"r","notification":{"method":"thread/tokenUsage/updated","params":{"turnKey":"opaque:A","tokenUsage":{"total":{"totalTokens":100}}}}}),
        );
        let s = d.snapshot();
        assert_eq!(s.runtimes[0].work_activity, work);
        assert_eq!(
            s.runtimes[0].active_turns[0].token_usage_baseline,
            (!attached).then(|| json!({"total":{"totalTokens":100}}))
        );
    }
}

#[test]
fn successor_contract_recovered_terminal_preserves_history_only_accounting() {
    let mut d = DaemonState::default();
    let mut c = ConnectionClaims::default();
    let mut recovered = full("G", 10, false);
    recovered["historyRecovery"] = json!({"epoch":1,"terminals":[{"turnId":"0","threadId":"tokens","turnKey":"opaque:A","status":"failed"}]});
    send(&mut d, 1, &mut c, RUNTIME_UPDATE_STATE_METHOD, recovered);
    let idle = d.snapshot();
    assert!(idle.runtimes[0].active_turns.is_empty());
    let late = json!({"runtimeId":"r","notification":{"method":"thread/tokenUsage/updated","params":{"turnKey":"opaque:A","tokenUsage":{"total":{"totalTokens":100}}}}});
    send(&mut d, 1, &mut c, RUNTIME_EVENT_METHOD, late);
    assert_eq!(d.snapshot().runtimes, idle.runtimes);
    assert_eq!(events(&d, idle.seq).len(), 1);
    let mut started = event("turn/started", "inProgress");
    started["notification"]["params"] =
        json!({"threadId":"tokens","turn":{"id":"1","key":"opaque:B"}});
    started["workActivity"] = json!({"producerGeneration":"G","revision":11,"items":["tokens:1"],"runningTurnKeys":["tokens:1"]});
    send(&mut d, 1, &mut c, RUNTIME_EVENT_METHOD, started);
    assert_eq!(
        d.snapshot().runtimes[0].active_turns[0].token_usage_baseline,
        Some(json!({"total":{"totalTokens":100}})),
        "recovery is the first observation of A's thread"
    );
}

#[test]
fn successor_contract_a_late_final_sample_repairs_only_the_inferred_successor() {
    let usage = |n| json!({"total":{"totalTokens":n}});
    for partial in [None, Some(20)] {
        for explicit in [None, Some(70)] {
            let mut d = DaemonState::default();
            let mut c = ConnectionClaims::default();
            let turn = |id| json!({"threadId":"T","turnId":id,"turnKey":format!("opaque:{id}")});
            let full = |turn| json!({"runtimeId":"r","activeTurns":[turn]});
            let sample = |id, n| json!({"runtimeId":"r","notification":{"method":"thread/tokenUsage/updated","params":{"turnKey":format!("opaque:{id}"),"tokenUsage":usage(n)}}});
            send(
                &mut d,
                1,
                &mut c,
                RUNTIME_UPDATE_STATE_METHOD,
                full(turn("A")),
            );
            if let Some(n) = partial {
                send(&mut d, 1, &mut c, RUNTIME_EVENT_METHOD, sample("A", n));
            }
            send(
                &mut d,
                1,
                &mut c,
                RUNTIME_EVENT_METHOD,
                json!({"runtimeId":"r","notification":{"method":"turn/completed","params":{"turnKey":"opaque:A","turn":{"id":"A","status":"completed"}}}}),
            );
            send(
                &mut d,
                1,
                &mut c,
                RUNTIME_UPDATE_STATE_METHOD,
                full(turn("B")),
            );
            if let Some(n) = explicit {
                let mut b = turn("B");
                b["tokenUsageBaseline"] = usage(n);
                send(&mut d, 1, &mut c, RUNTIME_UPDATE_STATE_METHOD, full(b));
            }
            send(&mut d, 1, &mut c, RUNTIME_EVENT_METHOD, sample("B", 140));
            send(&mut d, 1, &mut c, RUNTIME_EVENT_METHOD, sample("A", 100));
            let snap = d.snapshot();
            assert_eq!(snap.runtimes[0].active_turns.len(), 1);
            let b = &snap.runtimes[0].active_turns[0];
            assert_eq!(b.token_usage, Some(usage(140)));
            assert_eq!(
                b.token_usage_baseline,
                Some(usage(explicit.unwrap_or(100))),
                "the next subscriber must see B's corrected baseline"
            );
        }
    }
}

#[test]
fn opaque_usage_after_completion_seeds_only_its_thread_without_publishing_work() {
    let usage = |n| json!({"total":{"totalTokens":n}});
    let turn = |id: &str| json!({"threadId":"tokens","turnId":id,"turnKey":format!("opaque:{id}")});
    for end in ["terminal", "full", "scheduler", "missed-start"] {
        let mut d = DaemonState::default();
        let mut c = ConnectionClaims::default();
        let mut state = json!({"runtimeId":"r","activeTurns": if end == "missed-start" {vec![]} else {vec![turn("0")]}});
        if end == "scheduler" {
            state["workActivity"] = json!({"producerGeneration":"G","revision":1,"items":["tokens:0"],"runningTurnKeys":["tokens:0"]});
        }
        send(&mut d, 1, &mut c, RUNTIME_UPDATE_STATE_METHOD, state);
        let event = |method, params| json!({"runtimeId":"r","notification":{"method":method,"params":params}});
        match end {
            "full" => {
                send(
                    &mut d,
                    1,
                    &mut c,
                    RUNTIME_UPDATE_STATE_METHOD,
                    json!({"runtimeId":"r","activeTurns":[]}),
                );
            }
            "scheduler" => {
                let mut prune = event("work/changed", json!({}));
                prune["workActivity"] =
                    json!({"producerGeneration":"G","revision":2,"items":[],"runningTurnKeys":[]});
                send(&mut d, 1, &mut c, RUNTIME_EVENT_METHOD, prune);
            }
            _ => {
                send(
                    &mut d,
                    1,
                    &mut c,
                    RUNTIME_EVENT_METHOD,
                    event(
                        "turn/completed",
                        json!({"threadId":"tokens","turnKey":"opaque:0","turn":{"id":"0","status":"completed"}}),
                    ),
                );
            }
        }
        let idle = d.snapshot();
        assert!(idle.runtimes[0].active_turns.is_empty());
        let late = |key, n| {
            event(
                "thread/tokenUsage/updated",
                json!({"turnKey":key,"tokenUsage":usage(n)}),
            )
        };
        for (key, n) in [("opaque:0", 100), ("tokens:unknown", 888)] {
            send(&mut d, 1, &mut c, RUNTIME_EVENT_METHOD, late(key, n));
        }
        assert_eq!(d.snapshot().runtimes, idle.runtimes);
        assert!(
            events(&d, idle.seq)
                .iter()
                .all(|e| e["type"] == "runtimeNotification")
        );
        let start = |id: &str, rev| {
            let mut msg = event(
                "turn/started",
                json!({"threadId":"tokens","turn":{"id":id,"key":format!("opaque:{id}")}}),
            );
            if end == "scheduler" {
                let canonical = format!("tokens:{id}");
                msg["workActivity"] = json!({"producerGeneration":"G","revision":rev,"items":[canonical],"runningTurnKeys":[canonical]});
            }
            msg
        };
        send(&mut d, 1, &mut c, RUNTIME_EVENT_METHOD, start("1", 3));
        assert_eq!(
            d.snapshot().runtimes[0].active_turns[0].token_usage_baseline,
            Some(usage(100)),
            "{end}: late final sample must seed the next turn"
        );
        send(
            &mut d,
            1,
            &mut c,
            RUNTIME_EVENT_METHOD,
            late("opaque:1", 140),
        );
        send(
            &mut d,
            1,
            &mut c,
            RUNTIME_EVENT_METHOD,
            late("opaque:0", 100),
        );
        send(
            &mut d,
            1,
            &mut c,
            RUNTIME_EVENT_METHOD,
            event(
                "turn/completed",
                json!({"threadId":"tokens","turn":{"key":"opaque:1","id":"1","status":"completed"}}),
            ),
        );
        send(&mut d, 1, &mut c, RUNTIME_EVENT_METHOD, start("2", 4));
        let all = d.snapshot();
        let third = all.runtimes[0]
            .active_turns
            .iter()
            .find(|t| t.turn_id == "2")
            .unwrap();
        assert_eq!(
            third.token_usage_baseline,
            Some(usage(140)),
            "a replay of an old turn must not rewind the thread"
        );
    }
}

#[test]
fn retired_usage_keys_expire_and_do_not_transfer_to_a_successor() {
    let mut d = DaemonState::default();
    let (mut a, mut b) = (ConnectionClaims::default(), ConnectionClaims::default());
    send(
        &mut d,
        1,
        &mut a,
        RUNTIME_UPDATE_STATE_METHOD,
        json!({"runtimeId":"r","activeTurns":[]}),
    );
    for i in 0..=crate::protocol::MAX_RECOVERED_TERMINALS {
        send(
            &mut d,
            1,
            &mut a,
            RUNTIME_EVENT_METHOD,
            json!({"runtimeId":"r","notification":{"method":"turn/completed",
            "params":{"threadId":"t","turnKey":format!("opaque:{i}"),"turn":{"id":i.to_string(),"status":"completed"}}}}),
        );
    }
    let sample = |i| {
        json!({"runtimeId":"r","notification":{"method":"thread/tokenUsage/updated","params":{
        "turnKey":format!("opaque:{i}"),"tokenUsage":{"total":{"totalTokens":100}}}}})
    };
    let start = |id| json!({"runtimeId":"r","activeTurns":[{"threadId":"t","turnId":id}]});
    send(&mut d, 1, &mut a, RUNTIME_EVENT_METHOD, sample(0));
    send(&mut d, 1, &mut a, RUNTIME_UPDATE_STATE_METHOD, start("a"));
    assert!(
        d.snapshot().runtimes[0].active_turns[0]
            .token_usage_baseline
            .is_none()
    );
    send(
        &mut d,
        2,
        &mut b,
        RUNTIME_UPDATE_STATE_METHOD,
        json!({"runtimeId":"r","activeTurns":[]}),
    );
    send(&mut d, 2, &mut b, RUNTIME_EVENT_METHOD, sample(1));
    send(&mut d, 2, &mut b, RUNTIME_UPDATE_STATE_METHOD, start("b"));
    assert!(
        d.snapshot().runtimes[0].active_turns[0]
            .token_usage_baseline
            .is_none()
    );
}

#[test]
fn presence_contract_legacy_omission_preserves_the_last_complete_state() {
    for prior_generation in [false, true] {
        let mut d = DaemonState::default();
        if prior_generation {
            let mut prior = ConnectionClaims::default();
            send(
                &mut d,
                1,
                &mut prior,
                RUNTIME_UPDATE_STATE_METHOD,
                full("prior", 8, false),
            );
            send(
                &mut d,
                1,
                &mut prior,
                RUNTIME_UNREGISTER_METHOD,
                json!({"runtimeId":"r"}),
            );
        }
        let mut current = ConnectionClaims::default();
        send(
            &mut d,
            2,
            &mut current,
            RUNTIME_UPDATE_STATE_METHOD,
            json!({"runtimeId":"r","activeTurns":[{"threadId":"legacy","turnId":"0"}]}),
        );
        let before = d.snapshot();
        for bad in [
            json!({"runtimeId":"r"}),
            json!({"runtimeId":"r","activeTurns":null}),
            json!({"runtimeId":"r","activeTurns":[{"threadId":"","turnId":"0"}]}),
        ] {
            assert!(
                d.producer_message(2, &mut current, RUNTIME_UPDATE_STATE_METHOD, Some(bad))
                    .is_err(),
                "only an explicit, complete legacy turn list may replace the busy state"
            );
            d.require_replacement(2, "r");
            assert!(d.take_replacement_request(2, "r").is_none());
            assert_eq!(d.snapshot(), before);
        }
        send(
            &mut d,
            2,
            &mut current,
            RUNTIME_UPDATE_STATE_METHOD,
            json!({"runtimeId":"r","activeTurns":[]}),
        );
        assert!(d.snapshot().runtimes[0].active_turns.is_empty());
        assert!(d.seq > before.seq);
    }
}

#[test]
fn presence_contract_an_incomplete_first_legacy_state_does_not_claim_the_runtime() {
    let mut d = DaemonState::default();
    let mut bad = ConnectionClaims::default();
    assert!(
        d.producer_message(
            1,
            &mut bad,
            RUNTIME_UPDATE_STATE_METHOD,
            Some(json!({"runtimeId":"r"}))
        )
        .is_err()
    );
    assert!(bad.owned.is_empty());
    assert!(d.snapshot().runtimes.is_empty());
    let mut good = ConnectionClaims::default();
    assert!(
        send(
            &mut d,
            2,
            &mut good,
            RUNTIME_UPDATE_STATE_METHOD,
            json!({"runtimeId":"r","activeTurns":[]})
        )
        .is_none()
    );
    assert_eq!(d.snapshot().runtimes.len(), 1);
}

#[test]
fn presence_contract_key_only_usage_seeds_the_next_turn_on_the_matched_thread() {
    for method in [
        "thread/tokenUsage/updated",
        "turn/contextUpdated",
        "turn/stateUpdated",
    ] {
        let mut d = DaemonState::default();
        let mut c = ConnectionClaims::default();
        let usage = |n| json!({"total":{"totalTokens":n}});
        let turn = |thread, key| json!({"threadId":thread,"turnId":"0","turnKey":key});
        let first = turn("tokens", "opaque-first");
        let other = turn("different", "opaque-other");
        send(
            &mut d,
            1,
            &mut c,
            RUNTIME_UPDATE_STATE_METHOD,
            json!({"runtimeId":"r","activeTurns":[first, other]}),
        );
        for params in [
            json!({"turnKey":"opaque-first","tokenUsage":usage(100)}),
            json!({"turnKey":"unknown","tokenUsage":usage(990)}),
            json!({"turnId":"0","tokenUsage":usage(991)}),
            json!({"threadId":"explicit","tokenUsage":usage(12)}),
        ] {
            send(
                &mut d,
                1,
                &mut c,
                RUNTIME_EVENT_METHOD,
                json!({"runtimeId":"r","notification":{"method":method,"params":params}}),
            );
        }
        let seen = d.snapshot();
        let first = seen.runtimes[0]
            .active_turns
            .iter()
            .find(|t| t.thread_id == "tokens")
            .unwrap();
        assert_eq!(first.token_usage, Some(usage(100)));
        send(
            &mut d,
            1,
            &mut c,
            RUNTIME_UPDATE_STATE_METHOD,
            json!({"runtimeId":"r","activeTurns":[]}),
        );
        send(
            &mut d,
            1,
            &mut c,
            RUNTIME_UPDATE_STATE_METHOD,
            json!({"runtimeId":"r","activeTurns":[
            turn("tokens","opaque-next"), turn("different","opaque-independent"), turn("explicit","opaque-explicit")]}),
        );
        for t in &d.snapshot().runtimes[0].active_turns {
            let expected = match t.thread_id.as_str() {
                "tokens" => Some(usage(100)),
                "explicit" => Some(usage(12)),
                "different" => None,
                _ => unreachable!(),
            };
            assert_eq!(
                t.token_usage_baseline, expected,
                "{method}: {}",
                t.thread_id
            );
        }
    }
}

#[test]
fn observer_contract_handoff_orders_history_for_a_subscriber_joining_during_repair() {
    let mut d = DaemonState::default();
    let (mut g, mut h) = (ConnectionClaims::default(), ConnectionClaims::default());
    send(
        &mut d,
        1,
        &mut g,
        RUNTIME_UPDATE_STATE_METHOD,
        full("G", 8, true),
    );
    let mut bad = full("G", 9, true);
    bad.as_object_mut().unwrap().remove("activeTurns");
    assert!(
        d.producer_message(1, &mut g, RUNTIME_UPDATE_STATE_METHOD, Some(bad))
            .is_err()
    );
    d.require_replacement(1, "r");
    let first = d.snapshot();
    assert_eq!(first.runtimes[0].active_turns.len(), 1);
    assert!(first.runtimes[0].work_activity_supported && first.runtimes[0].work_activity.is_none());
    send(
        &mut d,
        2,
        &mut h,
        RUNTIME_UPDATE_STATE_METHOD,
        full("H", 1, false),
    );
    let replay = events(&d, first.seq);
    let order: Vec<_> = replay
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
        "a repair-time observer cannot identify G from its first snapshot"
    );
    assert!(d.snapshot().runtimes[0].active_turns.is_empty());
}

fn check_replayed_baseline(duplicate_start: bool) {
    for prior in [None, Some(60)] {
        let mut d = DaemonState::default();
        let mut claims = ConnectionClaims::default();
        let usage = |n: i64| json!({"total":{"totalTokens":n},"last":{"totalTokens":n}});
        let state = |id: &str, n| json!({"runtimeId":"r", "activeTurns":[{"threadId":"t","turnId":id,"tokenUsage":usage(n)}]});
        if let Some(n) = prior {
            send(
                &mut d,
                1,
                &mut claims,
                RUNTIME_UPDATE_STATE_METHOD,
                state("old", n),
            );
            send(
                &mut d,
                1,
                &mut claims,
                RUNTIME_UPDATE_STATE_METHOD,
                json!({"runtimeId":"r","activeTurns":[]}),
            );
        }
        send(
            &mut d,
            1,
            &mut claims,
            RUNTIME_UPDATE_STATE_METHOD,
            state("1", 100),
        );
        let expected = prior.map(usage);
        assert_eq!(
            d.snapshot().runtimes[0].active_turns[0].token_usage_baseline,
            expected
        );
        if duplicate_start {
            send(
                &mut d,
                1,
                &mut claims,
                RUNTIME_EVENT_METHOD,
                json!({"runtimeId":"r","notification":{
                "method":"turn/started","params":{"threadId":"t","turn":{"id":"1","tokenUsage":usage(100)}}}}),
            );
        } else {
            send(
                &mut d,
                1,
                &mut claims,
                RUNTIME_UPDATE_STATE_METHOD,
                state("1", 100),
            );
        }
        assert_eq!(
            d.snapshot().runtimes[0].active_turns[0].token_usage_baseline,
            expected,
            "a replay must preserve the turn's starting total, including absence"
        );
        // A real supplied baseline can correct the inference. Null on a later replay cannot erase it.
        let mut corrected = state("1", 100);
        corrected["activeTurns"][0]["tokenUsageBaseline"] = usage(25);
        send(
            &mut d,
            1,
            &mut claims,
            RUNTIME_UPDATE_STATE_METHOD,
            corrected,
        );
        if duplicate_start {
            send(
                &mut d,
                1,
                &mut claims,
                RUNTIME_EVENT_METHOD,
                json!({"runtimeId":"r","notification":{
                "method":"turn/started","params":{"threadId":"t","turn":{"id":"1","tokenUsageBaseline":null}}}}),
            );
        } else {
            let mut replay = state("1", 100);
            replay["activeTurns"][0]["tokenUsageBaseline"] = JsonValue::Null;
            send(&mut d, 1, &mut claims, RUNTIME_UPDATE_STATE_METHOD, replay);
        }
        assert_eq!(
            d.snapshot().runtimes[0].active_turns[0].token_usage_baseline,
            Some(usage(25))
        );
    }
}

#[test]
fn observer_contract_legacy_full_replay_preserves_token_baseline() {
    check_replayed_baseline(false);
}

#[test]
fn observer_contract_duplicate_start_preserves_token_baseline() {
    check_replayed_baseline(true);
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
