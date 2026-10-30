"""Opt-in semantic checks using production review packets and real model responses."""

import argparse
import datetime
import json
import os
from pathlib import Path
import signal
import subprocess
import tempfile
import time


def run(
    command: list[str], *, env: dict[str, str], cwd: Path, timeout: int, prompt: str | None = None
) -> str:
    with tempfile.TemporaryFile() as log:
        process = subprocess.Popen(
            command,
            env=env,
            cwd=cwd,
            stdout=log,
            stderr=subprocess.STDOUT,
            stdin=subprocess.PIPE if prompt is not None else subprocess.DEVNULL,
            text=True,
            start_new_session=True,
        )
        timed_out = False
        try:
            process.communicate(prompt, timeout=timeout)
        except subprocess.TimeoutExpired:
            timed_out = True
            os.killpg(process.pid, signal.SIGTERM)
            try:
                process.wait(5)
            except subprocess.TimeoutExpired:
                os.killpg(process.pid, signal.SIGKILL)
                process.wait()
        log.seek(0)
        text = log.read().decode(errors="replace")
        if process.returncode or timed_out:
            # Do not dump credentials, requests, or a whole model transcript.
            errors = [
                line[:500]
                for line in text.splitlines()
                if "error" in line.lower() and "authorization" not in line.lower()
            ]
            progress = []
            for line in text.splitlines():
                if line.startswith("{"):
                    event = json.loads(line)
                    progress.append((event.get("type"), event.get("item", {}).get("type")))
            reason = "timed out" if timed_out else f"failed ({process.returncode})"
            raise RuntimeError(f"command {reason}: {errors[-3:]}; last events: {progress[-5:]}")
        return text


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary", type=Path, help="rebuilt codex-exec binary")
    parser.add_argument("--model", required=True, help="main follow-up model")
    parser.add_argument("--review-model", help="review model; defaults to --model")
    parser.add_argument("--auth-source", type=Path, default=Path.home() / ".codex/auth.json")
    parser.add_argument("--timeout", type=int, default=180)
    args = parser.parse_args()
    if args.timeout <= 0:
        parser.error("--timeout must be positive")
    binary = args.binary.expanduser().resolve(strict=True)
    repo = Path(__file__).resolve().parent.parent
    review_model = args.review_model or args.model
    started = time.monotonic()
    with tempfile.TemporaryDirectory(prefix="completion-review-eval-") as directory:
        root = Path(directory)
        state = root / "state"
        workspace = root / "work"
        state.mkdir()
        workspace.mkdir()
        auth = json.loads(args.auth_source.expanduser().read_text())
        # Keep evaluation refreshes isolated from the user's active credentials.
        if isinstance(tokens := auth.get("tokens"), dict):
            tokens["refresh_token"] = ""
            auth["last_refresh"] = datetime.datetime.now(datetime.UTC).isoformat()
        auth_path = state / "auth.json"
        auth_path.write_text(json.dumps(auth))
        auth_path.chmod(0o600)
        env = os.environ.copy()
        env.update(
            CODEX_HOME=str(state),
            CODEX_SANDBOX_NETWORK_DISABLED="1",
            CARGO_BUILD_JOBS="2",
            TOKIO_WORKER_THREADS="2",
            RUST_LOG=os.environ.get("RUST_LOG", "info"),
        )
        fixture_path = root / "cases.json"
        export_env = env | {"CODEX_COMPLETION_REVIEW_EVAL_OUTPUT": str(fixture_path)}
        run(
            [
                "cargo-local", "test", "--quiet", "-p", "codex-core", "--lib",
                "export_completion_review_behavioral_cases", "--", "--ignored", "--test-threads=2",
            ],
            env=export_env,
            cwd=repo,
            timeout=args.timeout,
        )
        fixtures = json.loads(fixture_path.read_text())
        inspections: dict[str, list[dict]] = {}

        def generate(model: str, instructions: str, prompt: str, label: str) -> str:
            print(json.dumps({"stage": label, "model": model}), flush=True)
            instructions_path = state / "instructions.md"
            instructions_path.write_text(instructions)
            state.joinpath("config.toml").write_text(
                f"model = {json.dumps(model)}\nmodel_reasoning_effort = 'low'\n"
                f"model_instructions_file = {json.dumps(str(instructions_path))}\n"
                "approval_policy = 'never'\nsandbox_mode = 'read-only'\nweb_search = 'disabled'\n"
                "[features]\nremote_models = false\nundo = false\ncollab = false\n"
                "apps = false\nauto_rename_thread = false\n"
                "auto_post_turn_completion_review = false\n"
                "responses_websockets = false\nresponses_websockets_v2 = false\n"
            )
            output_path = root / f"{label}.answer"
            events = run(
                [
                    str(binary), "--skip-git-repo-check", "--ephemeral",
                    "--sandbox", "read-only", "--color", "never", "--json",
                    "--output-last-message", str(output_path), "-",
                ],
                prompt=prompt,
                env=env,
                cwd=workspace,
                timeout=args.timeout,
            )
            inspections[label] = []
            for line in events.splitlines():
                if not line.startswith("{"):
                    continue
                event = json.loads(line)
                item = event.get("item", {})
                if event.get("type") == "item.completed" and item.get("type") == "command_execution":
                    inspections[label].append({
                        "command": item["command"].replace(str(workspace), "."),
                        "exit_code": item["exit_code"],
                    })
            return output_path.read_text()

        results = []
        for case in fixtures["cases"]:
            artifact_path = workspace / "result.json"
            artifact_path.write_text(json.dumps(case["artifact"]) + "\n")
            before = artifact_path.read_bytes()
            assert json.loads(before) == case["artifact"]
            candidate = case["candidate"]
            if case["advisory"] is not None:
                candidate = generate(
                    args.model,
                    fixtures["replacement_instructions"] + "\n" + case["advisory"],
                    case["main_context"],
                    case["id"] + "-main",
                )
            packet = case["review_context_template"].replace(
                "EVAL_GENERATED_CANDIDATE", candidate.replace("]]>", "]]]]><![CDATA[>")
            )
            evaluation = generate(
                review_model,
                fixtures["review_instructions"],
                packet,
                case["id"] + "-review",
            )
            stripped = evaluation.strip()
            if stripped.startswith("```"):
                stripped = "\n".join(stripped.splitlines()[1:-1])
            try:
                verdict = json.loads(stripped[stripped.index("{"):stripped.rindex("}") + 1])
            except ValueError as error:
                raise RuntimeError(
                    f"{case['id']} produced no valid review object: {evaluation[:1000]!r}"
                ) from error
            passed = (
                verdict.get("fix_actions_advised") is case["expected_fix_actions_advised"]
                and isinstance(verdict.get("evaluation"), str)
                and bool(verdict["evaluation"].strip())
                and artifact_path.read_bytes() == before
                and any(item["exit_code"] == 0 for item in inspections[case["id"] + "-review"])
            )
            result = {
                "case": case["id"],
                "expected_verdict_matched_and_artifact_unchanged": passed,
                "candidate": candidate.replace(str(workspace), "."),
                "review": verdict,
                "inspection_commands": {
                    label: items for label, items in inspections.items()
                    if label.startswith(case["id"] + "-")
                },
                "rubric_for_manual_assessment": case["rubric"],
            }
            results.append(result)
            print(json.dumps(result), flush=True)
        print(json.dumps({
            "main_model": args.model,
            "review_model": review_model,
            "cases": len(results),
            "elapsed_seconds": round(time.monotonic() - started, 2),
            "all_expected_verdicts_matched": all(
                item["expected_verdict_matched_and_artifact_unchanged"] for item in results
            ),
            "semantic_boundary": "Inspect the answers against the rubrics; boolean checks are not semantic certification.",
        }), flush=True)
        if not all(item["expected_verdict_matched_and_artifact_unchanged"] for item in results):
            raise SystemExit(1)


if __name__ == "__main__":
    try:
        main()
    except (OSError, RuntimeError, ValueError) as error:
        print(json.dumps({"failure": type(error).__name__, "detail": str(error)[:2000]}), flush=True)
        raise SystemExit(1) from None
