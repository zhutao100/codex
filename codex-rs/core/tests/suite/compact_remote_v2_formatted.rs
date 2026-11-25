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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn covered_shell_and_uncovered_patch_fit_the_wire_on_both_boundaries() -> Result<()> {
    skip_if_no_network_unless_localhost!(Ok(()));
    let cwd = tempfile::tempdir()?;
    let build = || {
        builder(cwd.path(), 32_000, TruncationPolicyConfig::bytes(32_000)).with_config(|cfg| {
            let p = &mut cfg.model_overlay.as_mut().unwrap().cross_model;
            p.apply_patch_tool_type = Some(Some(
                codex_protocol::openai_models::ApplyPatchToolType::Freeform,
            ));
            p.effective_context_window_percent = Some(100);
        })
    };
    let server = responses::start_mock_server().await;
    let _warm = responses::mount_sse_once(&server, text_reply("start", "Ready.", 1_000)).await;
    let original = build().build(&server).await?;
    original.codex.submit(user_input("First task.")).await?;
    complete(&original.codex).await;
    shutdown(&original.codex).await?;
    let path = original.codex.rollout_path().unwrap();
    let mut raw = std::fs::read_to_string(&path)?;
    // A completed historic shell result is already part of the next ordinary measurement.
    let envelope = |text: &str| {
        json!({"output":text,"metadata":{"exit_code":0,"duration_seconds":1}}).to_string()
    };
    let covered = json!({"type":"function_call_output","call_id":"covered","output":envelope("archived work\n")});
    append_item(
        &mut raw,
        &json!({"type":"function_call","name":"shell","arguments":"{}","call_id":"covered"}),
    )?;
    append_item(&mut raw, &covered)?;
    std::fs::write(&path, raw)?;

    let replies = [
        responses::sse(vec![
            responses::ev_apply_patch_custom_tool_call("tail", "*** Begin Patch\n*** End Patch"),
            responses::ev_completed_with_tokens("work", 27_100),
        ]),
        text_reply("notes", NOTES, 24_000),
        compact_response("summary"),
        text_reply("done", "Finished.", 2_000),
    ];
    let server = responses::start_mock_server().await;
    let live = responses::mount_sse_sequence(&server, replies.to_vec()).await;
    let work = build()
        .resume(&server, original.home.clone(), path.clone())
        .await?;
    work.codex
        .submit(user_input("Continue measuring the work."))
        .await?;
    complete(&work.codex).await;
    shutdown(&work.codex).await?;
    let sent = live.requests();
    assert_eq!(sent.len(), 4);
    let covered_wire = sent[0].function_call_output("covered");
    assert_eq!(
        covered_wire["output"],
        "Exit code: 0\nWall time: 1 seconds\nOutput:\narchived work\n"
    );
    let covered_delta = tokens(&covered_wire)? - tokens(&covered)?;
    assert!(covered_delta < 0);
    let notes_suffix = sent[1].input().pop().context("capture marker")?;
    let trigger = sent[2].input().pop().context("V2 trigger")?;
    let mut prefix = String::new();
    let mut measured = None;
    for line in std::fs::read_to_string(&path)?.split_inclusive('\n') {
        let v: Value = serde_json::from_str(line)?;
        if v["type"] == "token_usage_context"
            && v["payload"]["info"]["last_token_usage"]["input_tokens"] == 27_100
        {
            assert_eq!(v["payload"]["measured_format_delta"], covered_delta);
            measured = Some(v);
            break;
        }
        prefix.push_str(line);
    }
    let measured = measured.context("ordinary work measurement")?;
    // Stop after the sampled patch call and supply a deterministic raw tool result.
    // Newlines and JSON escaping make its recorded cost materially different from the wire.
    let evidence = "changed \"filename\"\n".repeat(160);
    let tail =
        json!({"type":"custom_tool_call_output","call_id":"tail","output":envelope(&evidence)});
    let tail_wire = json!({"type":"custom_tool_call_output","call_id":"tail","output":format!("Exit code: 0\nWall time: 1 seconds\nOutput:\n{evidence}")});
    assert!(tokens(&tail)? > tokens(&tail_wire)? + 10);
    for (notes, budget, suffix) in [(true, 30_400, &notes_suffix), (false, 32_000, &trigger)] {
        for overflow in [0, 1] {
            let usage = budget - tokens(suffix)? - tokens(&tail_wire)? + overflow;
            assert!(usage >= 27_000);
            let mut record = measured.clone();
            let info = &mut record["payload"]["info"];
            for field in ["input_tokens", "total_tokens"] {
                info["last_token_usage"][field] = json!(usage);
                info["total_token_usage"][field] =
                    json!(info["total_token_usage"][field].as_i64().unwrap() + usage - 27_100);
            }
            let mut cut = prefix.clone();
            cut.push_str(&serde_json::to_string(&record)?);
            cut.push('\n');
            append_item(&mut cut, &tail)?;
            std::fs::write(&path, cut)?;
            let server = responses::start_mock_server().await;
            let traffic = responses::mount_sse_sequence(&server, replies[1..].to_vec()).await;
            let restored = build()
                .resume(&server, original.home.clone(), path.clone())
                .await?;
            restored.codex.submit(Op::Continue).await?;
            complete(&restored.codex).await;
            shutdown(&restored.codex).await?;
            let reqs = traffic.requests();
            assert_eq!(reqs.len(), 3);
            for req in &reqs[..2] {
                assert_eq!(req.function_call_output("covered"), covered_wire);
            }
            let actual = reqs[usize::from(!notes)].custom_tool_call_output("tail");
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
            if !notes && overflow == 1 {
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
                let server = responses::start_mock_server().await;
                let migrated = responses::mount_sse_sequence(&server, replies[1..].to_vec()).await;
                let restored = build()
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
