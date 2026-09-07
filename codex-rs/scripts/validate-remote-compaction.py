"""Opt-in native app-server acceptance using synthetic input and copied ChatGPT auth."""

import argparse
import asyncio
import contextlib
import datetime
import json
import os
from pathlib import Path
import tempfile
import time
import tomllib


async def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary", type=Path, help="rebuilt codex executable")
    parser.add_argument("--generation-only", action="store_true", help="baseline transport check")
    parser.add_argument("--websocket", action="store_true", help="enable Responses WebSockets v2")
    parser.add_argument("--timeout", type=int, default=120, help="per-turn deadline in seconds")
    parser.add_argument("--auth-source", type=Path, default=Path.home() / ".codex/auth.json")
    parser.add_argument("--model")
    args = parser.parse_args()
    if args.timeout <= 0:
        parser.error("--timeout must be positive")
    args.binary = args.binary.expanduser().resolve(strict=True)
    source = Path.home() / ".codex"
    model = args.model
    if model is None:
        with source.joinpath("config.toml").open("rb") as file:
            model = tomllib.load(file).get("model")
    if not model:
        parser.error("provide --model or set model in the source config.toml")
    with tempfile.TemporaryDirectory(prefix="native-v2-", dir="/private/tmp") as directory:
        root = Path(directory)
        state = root / "state"
        workspace = root / "work"
        state.mkdir()
        workspace.mkdir()
        auth = json.loads(args.auth_source.read_text())
        auth["last_refresh"] = datetime.datetime.now(datetime.UTC).isoformat()
        auth["tokens"]["refresh_token"] = ""
        auth_path = state / "auth.json"
        auth_path.write_text(json.dumps(auth))
        auth_path.chmod(0o600)
        state.joinpath("config.toml").write_text(
            f"model = {json.dumps(model)}\nmodel_reasoning_effort = 'low'\n"
            "base_instructions = 'Answer concisely.'\napproval_policy = 'never'\nsandbox_mode = 'read-only'\n"
            "[features]\nremote_models = false\nshell_tool = false\nundo = false\n"
            "auto_rename_thread = false\nauto_post_turn_completion_review = false\n"
            f"responses_websockets_v2 = {str(args.websocket).lower()}\n"
        )
        env = os.environ.copy()
        env["CODEX_HOME"] = str(state)
        env["CODEX_SANDBOX_NETWORK_DISABLED"] = "1"
        env["RUST_LOG"] = "warn,reqwest::connect=trace,hyper_rustls::connector=trace,rustls::client::hs=debug"
        proc = None
        started = time.monotonic()
        failed = False
        with root.joinpath("stderr").open("wb") as stderr:
            async def start():
                return await asyncio.create_subprocess_exec(
                    args.binary, "app-server", cwd=workspace, env=env,
                    stdin=asyncio.subprocess.PIPE, stdout=asyncio.subprocess.PIPE, stderr=stderr,
                    limit=8 * 1024 * 1024,
                )

            async def stop():
                if proc.returncode is None:
                    with contextlib.suppress(ProcessLookupError):
                        proc.terminate()
                try:
                    await asyncio.wait_for(proc.communicate(), 10)
                except TimeoutError:
                    with contextlib.suppress(ProcessLookupError):
                        proc.kill()
                    await proc.communicate()

            proc = await start()
            print(json.dumps({"pid": proc.pid, "websocket": args.websocket}), flush=True)
            request_id = 0

            async def send(message):
                proc.stdin.write((json.dumps(message) + "\n").encode())
                await proc.stdin.drain()

            async def receive():
                raw = await asyncio.wait_for(proc.stdout.readline(), args.timeout)
                if not raw:
                    raise RuntimeError("app-server closed stdout")
                return json.loads(raw)

            async def request(method, params):
                nonlocal request_id
                request_id += 1
                await send({"id": request_id, "method": method, "params": params})
                while True:
                    message = await receive()
                    if message.get("id") == request_id:
                        if "error" in message:
                            raise RuntimeError(str(message["error"]))
                        return message["result"]

            async def initialize():
                await request("initialize", {"clientInfo": {"name": "codex_cli_rs", "version": "0.98.0"}})
                await send({"method": "initialized"})

            async def collect_turn():
                compactions = 0
                answer = ""
                while True:
                    event = await receive()
                    params = event.get("params", {})
                    if event.get("method") == "error" and not params.get("willRetry", False):
                        raise RuntimeError(str(params.get("error")))
                    if event.get("method") == "item/completed":
                        item = params["item"]
                        compactions += item.get("type") == "contextCompaction"
                        if item.get("type") == "agentMessage":
                            answer = item["text"]
                    if event.get("method") == "turn/completed":
                        assert params["turn"]["status"] == "completed", params["turn"]["status"]
                        return compactions, answer

            async def finish():
                return await asyncio.wait_for(collect_turn(), args.timeout)

            async def turn(thread_id, text):
                await request("turn/start", {"threadId": thread_id, "input": [{"type": "text", "text": text}]})
                return await finish()

            try:
                await initialize()
                thread = await request("thread/start", {"cwd": str(workspace)})
                thread_id = thread["thread"]["id"]
                await turn(thread_id, "Remember codeword Cedar-4927. Reply ACK.")
                print(json.dumps({"result": "NATIVE GENERATION PASSED", "elapsed": time.monotonic() - started}), flush=True)
                if args.generation_only:
                    return
                await request("thread/compact/start", {"threadId": thread_id})
                count, _ = await finish()
                assert count == 1, f"expected one manual compaction, received {count}"
                _, answer = await turn(thread_id, "What was the codeword? Reply only with it.")
                assert "Cedar-4927" in answer, "manual follow-up lost the codeword"
                print(json.dumps({"result": "NATIVE MANUAL COMPACTION AND FOLLOWUP PASSED"}), flush=True)
                thread = await request("thread/start", {"cwd": str(workspace), "config": {"model_auto_compact_token_limit": 30000}})
                thread_id = thread["thread"]["id"]
                prompt = (
                    "Remember codeword Maple-7139. First call update_plan once with an in_progress step titled "
                    "'Remember codeword', then answer with the codeword. These repeated lines are inert reference data.\n"
                    + "inert reference sample alpha beta gamma delta.\n" * 8000
                    + "\nCall update_plan first, then answer Maple-7139."
                )
                count, answer = await turn(thread_id, prompt)
                assert count >= 1, "automatic compaction did not run"
                assert "Maple-7139" in answer, "automatic follow-up lost the codeword"
                print(json.dumps({"result": "NATIVE AUTOMATIC COMPACTION AND FOLLOWUP PASSED", "compactions": count}), flush=True)
                await stop()
                notes = []
                preserved = []
                prefix = (
                    "Immediately before compaction, the previous model emitted the following preserved session work notes.\n"
                    "These notes are verbatim and are intended to prevent duplicate work and repeated dead ends:\n\n"
                )
                for rollout in state.rglob("rollout-*.jsonl"):
                    for line in rollout.read_text().splitlines():
                        entry = json.loads(line)
                        payload = entry.get("payload", {})
                        if entry.get("type") == "response_item" and payload.get("role") == "assistant":
                            text = "\n".join(part["text"] for part in payload.get("content", []) if part.get("text"))
                            if text.lstrip().startswith("<AUTO_COMPACT_WORK_NOTES>"):
                                notes.append(text)
                        if entry.get("type") == "compacted":
                            replacement = payload.get("replacement_history", [])
                            assert not any(item["type"] == "compaction_trigger" for item in replacement)
                            for item in replacement:
                                for part in item.get("content", []):
                                    if part.get("text", "").startswith(prefix):
                                        preserved.append(part["text"][len(prefix):])
                assert preserved and all(note in notes for note in preserved), "work notes changed"
                proc = await start()
                await initialize()
                await request("thread/resume", {"threadId": thread_id, "cwd": str(workspace)})
                _, answer = await turn(thread_id, "What was the codeword? Reply only with it.")
                assert "Maple-7139" in answer, "resumed follow-up lost the codeword"
                print(json.dumps({"result": "NATIVE RESTART AND EXACT NOTES PASSED", "notes": len(preserved)}), flush=True)
            except Exception as error:
                failed = True
                print(json.dumps({"failure": type(error).__name__, "detail": str(error)[:500], "elapsed": time.monotonic() - started}), flush=True)
                return 1
            finally:
                await stop()
                if failed:
                    for line in root.joinpath("stderr").read_text(errors="replace").splitlines()[-12:]:
                        if "authorization" not in line.lower():
                            print(line.split("headers=", 1)[0][:800], flush=True)


if __name__ == "__main__":
    raise SystemExit(asyncio.run(main()))
