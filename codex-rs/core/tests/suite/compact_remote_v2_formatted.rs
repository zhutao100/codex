use super::*;
use pretty_assertions::assert_eq;

fn tokens(item: &Value) -> Result<i64> {
    Ok(serde_json::to_vec(item)?.len().div_ceil(4).try_into()?)
}

fn append_item(log: &mut String, item: &Value) -> Result<()> {
    log.push_str(&serde_json::to_string(&json!({
        "timestamp":"2026-01-01T00:00:00Z","type":"response_item","payload":item
    }))?);
    log.push('\n');
    Ok(())
}

#[test_case::test_case(true; "same formatter")]
#[test_case::test_case(false; "formatter enabled before the decision")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn covered_shell_and_uncovered_patch_fit_the_wire_on_both_boundaries(
    old_freeform: bool,
) -> Result<()> {
    skip_if_no_network_unless_localhost!(Ok(()));
    let cwd = tempfile::tempdir()?;
    let build = |freeform| {
        builder(cwd.path(), 32_000, TruncationPolicyConfig::bytes(32_000)).with_config(move |cfg| {
            use codex_protocol::openai_models::ApplyPatchToolType;
            let p = &mut cfg.model_overlay.as_mut().unwrap().cross_model;
            p.apply_patch_tool_type = Some(Some(if freeform {
                ApplyPatchToolType::Freeform
            } else {
                ApplyPatchToolType::Function
            }));
            p.effective_context_window_percent = Some(100);
        })
    };
    let server = responses::start_mock_server().await;
    let warm = responses::mount_sse_once(&server, text_reply("start", "Ready.", 1_000)).await;
    let original = build(true).build(&server).await?;
    original.codex.submit(user_input("First task.")).await?;
    complete(&original.codex).await;
    shutdown(&original.codex).await?;
    let path = original.codex.rollout_path().unwrap();
    let mut raw = std::fs::read_to_string(&path)?;
    // A completed historic shell result is already part of the next ordinary measurement.
    let envelope = |text: &str| {
        json!({"output":text,"metadata":{"exit_code":0,"duration_seconds":1}}).to_string()
    };
    let evidence = "changed \"filename\"\n".repeat(160);
    let covered =
        json!({"type":"function_call_output","call_id":"covered","output":envelope(&evidence)});
    let formatted = format!("Exit code: 0\nWall time: 1 seconds\nOutput:\n{evidence}");
    let covered_wire =
        json!({"type":"function_call_output","call_id":"covered","output":formatted});
    append_item(
        &mut raw,
        &json!({"type":"function_call","name":"shell","arguments":"{}","call_id":"covered"}),
    )?;
    append_item(&mut raw, &covered)?;
    std::fs::write(&path, raw)?;

    const OUTPUT: i64 = 20;
    let mut receipt = responses::ev_completed_with_tokens("work", 27_100);
    receipt["response"]["usage"]["output_tokens"] = json!(OUTPUT);
    receipt["response"]["usage"]["total_tokens"] = json!(27_100 + OUTPUT);
    let replies = [
        responses::sse(vec![
            responses::ev_apply_patch_custom_tool_call("tail", "*** Begin Patch\n*** End Patch"),
            receipt,
        ]),
        text_reply("notes", NOTES, 24_000),
        compact_response("summary"),
        text_reply("done", "Finished.", 2_000),
    ];
    let live = responses::mount_sse_sequence(&server, replies.to_vec()).await;
    let work = build(old_freeform)
        .resume(&server, original.home.clone(), path.clone())
        .await?;
    work.codex
        .submit(user_input("Continue measuring the work."))
        .await?;
    complete(&work.codex).await;
    shutdown(&work.codex).await?;
    let sent = live.requests();
    assert_eq!(sent.len(), 4);
    let paid = sent[0].function_call_output("covered");
    assert_eq!(
        &paid,
        if old_freeform {
            &covered_wire
        } else {
            &covered
        }
    );
    let covered_delta = tokens(&covered_wire)? - tokens(&covered)?;
    assert!(covered_delta < -OUTPUT);
    let projection_change = tokens(&covered_wire)? - tokens(&paid)?;
    let current_tools = &warm.single_request().body_json()["tools"];
    let tool_change = tokens(current_tools)? - tokens(&sent[0].body_json()["tools"])?;
    let notes_suffix = sent[1].input().pop().context("capture marker")?;
    let trigger = sent[2].input().pop().context("V2 trigger")?;
    let mut prefix = String::new();
    let mut measured = None;
    for line in std::fs::read_to_string(&path)?.split_inclusive('\n') {
        let v: Value = serde_json::from_str(line)?;
        if v["type"] == "token_usage_context"
            && v["payload"]["info"]["last_token_usage"]["input_tokens"] == 27_100
        {
            assert_eq!(
                v["payload"]["measured_format_delta"],
                if old_freeform { covered_delta } else { 0 }
            );
            assert_eq!(
                v["payload"]["measured_surface"]["structured_outputs"],
                old_freeform
            );
            measured = Some(v);
            break;
        }
        prefix.push_str(line);
    }
    let measured = measured.context("ordinary work measurement")?;
    // Stop after the sampled patch call and supply a deterministic raw tool result.
    // Newlines and JSON escaping make its recorded cost materially different from the wire.
    let tail =
        json!({"type":"custom_tool_call_output","call_id":"tail","output":envelope(&evidence)});
    let tail_wire = json!({"type":"custom_tool_call_output","call_id":"tail","output":format!("Exit code: 0\nWall time: 1 seconds\nOutput:\n{evidence}")});
    assert!(tokens(&tail)? > tokens(&tail_wire)? + 10);
    for (notes, budget, suffix) in [(true, 30_400, &notes_suffix), (false, 32_000, &trigger)] {
        for overflow in [0, 1] {
            let usage = budget
                - tokens(suffix)?
                - tokens(&tail_wire)?
                - projection_change
                - tool_change
                - OUTPUT
                + overflow;
            assert!(usage >= 27_000);
            let mut record = measured.clone();
            let info = &mut record["payload"]["info"];
            for field in ["input_tokens", "total_tokens"] {
                info["last_token_usage"][field] =
                    json!(usage + if field == "total_tokens" { OUTPUT } else { 0 });
                info["total_token_usage"][field] =
                    json!(info["total_token_usage"][field].as_i64().unwrap() + usage - 27_100);
            }
            let mut cut = prefix.clone();
            cut.push_str(&serde_json::to_string(&record)?);
            cut.push('\n');
            append_item(&mut cut, &tail)?;
            std::fs::write(&path, cut)?;
            let traffic = responses::mount_sse_sequence(&server, replies[1..].to_vec()).await;
            let restored = build(true)
                .resume(&server, original.home.clone(), path.clone())
                .await?;
            restored.codex.submit(Op::Continue).await?;
            complete(&restored.codex).await;
            shutdown(&restored.codex).await?;
            let reqs = traffic.requests();
            assert_eq!(reqs.len(), 3);
            let helper = &reqs[usize::from(!notes)];
            assert_eq!(&helper.body_json()["tools"], current_tools);
            assert_eq!(helper.function_call_output("covered"), covered_wire);
            let actual = helper.custom_tool_call_output("tail");
            if overflow == 0 {
                assert_eq!(actual, tail_wire);
            } else {
                assert!(
                    tokens(&actual)? < tokens(&tail_wire)?,
                    "notes {notes} failed to fit one token of overflow"
                );
            }
            let saved = std::fs::read_to_string(&path)?;
            assert!(
                saved
                    .lines()
                    .filter_map(|s| serde_json::from_str::<Value>(s).ok())
                    .any(|v| v["payload"] == tail)
            );
            if old_freeform && !notes && overflow == 1 {
                // Restart at a marker written by the modality-aware, pre-formatting fitter.
                // Its correction was calibrated against the larger, unformatted covered result.
                let mut old = String::new();
                let mut found = false;
                for line in saved.lines() {
                    let mut v: Value = serde_json::from_str(line)?;
                    if v["type"] == "token_usage_context" {
                        v["payload"]
                            .as_object_mut()
                            .context("usage record")?
                            .remove("measured_format_delta");
                    }
                    if let Some(d) = v["payload"]
                        .get_mut("pending_compaction")
                        .and_then(Value::as_object_mut)
                    {
                        let fit = d.remove("source_fit").context("fit")?;
                        d.insert("projected_fit".into(), json!(true));
                        let offset = fit["measured"]["transcript_adjustment"]
                            .as_i64()
                            .context("work offset")?
                            + tokens(&sent[0].body_json()["tools"])?;
                        d.insert(
                            "source_token_adjustment".into(),
                            json!(offset + covered_delta),
                        );
                        found = true;
                    }
                    old.push_str(&serde_json::to_string(&v)?);
                    old.push('\n');
                    if v["type"] == "response_item"
                        && v["payload"]["content"] == notes_suffix["content"]
                    {
                        break;
                    }
                }
                assert!(found);
                std::fs::write(&path, old)?;
                let migrated = responses::mount_sse_sequence(&server, replies[1..].to_vec()).await;
                let restored = build(true)
                    .resume(&server, original.home.clone(), path.clone())
                    .await?;
                restored.codex.submit(Op::Continue).await?;
                complete(&restored.codex).await;
                shutdown(&restored.codex).await?;
                let migrated = migrated.requests();
                assert_eq!(migrated.len(), 3);
                for (old, new) in migrated.iter().zip(&reqs).take(2) {
                    assert_eq!(old.body_json(), new.body_json());
                }
            }
        }
    }
    Ok(())
}
