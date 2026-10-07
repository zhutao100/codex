#!/usr/bin/env python3
"""VM-only release-binary check using Curl Safari and a deterministic Responses server."""

import argparse
import fcntl
import json
import os
from pathlib import Path
import pty
import secrets
import select
import shlex
import struct
import subprocess
import sys
import tempfile
import termios
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from http.cookies import SimpleCookie


def run(argv: list[str | Path], *, env: dict[str, str], cwd: Path) -> str:
    result = subprocess.run(argv, env=env, cwd=cwd, text=True, capture_output=True, timeout=120)
    if result.returncode:
        raise RuntimeError(
            f"{argv[0]} exited {result.returncode}\n{result.stdout[-3000:]}\n{result.stderr[-3000:]}"
        )
    return result.stdout


class ProbeServer(ThreadingHTTPServer):
    def __init__(self) -> None:
        super().__init__(("127.0.0.1", 0), ProbeHandler)
        self.requests: list[dict] = []
        self.command: str | None = None
        self.mcp_tool: str | None = None
        self.cookie_value = secrets.token_hex(16)
        self.cookie_login = threading.Event()
        self.authenticated_fetch = threading.Event()

    @property
    def url(self) -> str:
        return f"http://127.0.0.1:{self.server_port}"


class ProbeHandler(BaseHTTPRequestHandler):
    server: ProbeServer

    def log_message(self, *_args: object) -> None:
        pass

    def do_GET(self) -> None:
        if self.path == "/login":
            self.send_response(200)
            self.send_header(
                "Set-Cookie",
                f"plugin_probe={self.server.cookie_value}; Path=/; Max-Age=3600; HttpOnly",
            )
            self.send_header("Content-Type", "text/html")
            self.end_headers()
            self.wfile.write(b"<html><title>Plugin Probe</title>Guest test login established</html>")
            self.server.cookie_login.set()
        elif self.path == "/private" and self.authenticated():
            self.send_response(200)
            self.end_headers()
            self.wfile.write(b'{"authenticated":true,"marker":"curl-safari-plugin-ok"}')
            self.server.authenticated_fetch.set()
        else:
            self.send_response(401)
            self.end_headers()
            self.wfile.write(b"Guest test login required")

    def authenticated(self) -> bool:
        cookie = SimpleCookie(self.headers.get("Cookie", "")).get("plugin_probe")
        return cookie is not None and cookie.value == self.server.cookie_value

    def do_POST(self) -> None:
        body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        self.server.requests.append(body)
        first = len(self.server.requests) == 1
        item: dict
        if first and self.server.command:
            names = {tool.get("name") for tool in body["tools"]}
            if "exec_command" in names:
                name = "exec_command"
                args = {"cmd": self.server.command, "yield_time_ms": 1000, "max_output_tokens": 1000}
            elif "shell" in names:
                name = "shell"
                args = {"command": ["/bin/sh", "-c", self.server.command], "timeout_ms": 10000}
            else:
                name = "shell_command"
                args = {"command": self.server.command, "timeout_ms": 10000}
            item = {
                "type": "function_call", "id": "fc-plugin", "call_id": "call-plugin",
                "name": name, "arguments": json.dumps(args),
            }
        elif first and self.server.mcp_tool:
            item = {
                "type": "function_call", "id": "fc-plugin", "call_id": "call-plugin",
                "name": self.server.mcp_tool, "arguments": "{}",
            }
        else:
            item = {
                "type": "message", "id": "msg-plugin", "role": "assistant",
                "content": [{"type": "output_text", "text": "Plugin probe complete."}],
            }
        events = [
            {"type": "response.created", "response": {"id": f"resp-{len(self.server.requests)}"}},
            {"type": "response.output_item.done", "item": item},
            {"type": "response.completed", "response": {
                "id": f"resp-{len(self.server.requests)}", "output": [],
                "usage": {"input_tokens": 100, "output_tokens": 20, "total_tokens": 120},
            }},
        ]
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.end_headers()
        for event in events:
            self.wfile.write(f"data: {json.dumps(event)}\n\n".encode())


def verify_tui_override(
    binary: Path, provider_args: list[str], server: ProbeServer,
    env: dict[str, str], work: Path, artifacts: Path, *, enabled: bool,
) -> None:
    """The initial prompt follows the TUI's automatic forced ListSkills operation."""
    server.requests.clear()
    master, slave = pty.openpty()
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 40, 140, 0, 0))
    command = [
        binary, "--no-alt-screen", "--dangerously-bypass-approvals-and-sandbox",
        *provider_args,
        "-c", "notice.hide_full_access_warning=true",
        "-c", f'projects={{ {json.dumps(str(work))} = {{ trust_level = "trusted" }} }}',
        "-c", f"plugins.curl-safari@curl-safari-local.enabled={str(enabled).lower()}",
        "Use $curl-safari:curl-safari if it is available.",
    ]
    process = subprocess.Popen(
        command, stdin=slave, stdout=slave, stderr=slave,
        env=env | {"TERM": "xterm-256color"}, cwd=work, start_new_session=True,
    )
    os.close(slave)
    output = bytearray()

    def wait_for(marker: bytes) -> None:
        deadline = time.monotonic() + 30
        while marker not in output and time.monotonic() < deadline:
            if not select.select([master], [], [], .1)[0]:
                continue
            try:
                data = os.read(master, 65536)
            except OSError:
                break
            if not data:
                break
            output.extend(data)
            if b"\x1b[6n" in data:
                os.write(master, b"\x1b[1;1R")
            if b"\x1b]11;?" in data:
                os.write(master, b"\x1b]11;rgb:0000/0000/0000\x1b\\")
        assert marker in output, f"TUI did not render {marker!r}; exit={process.poll()}"

    try:
        wait_for(b"Plugin probe complete.")
        os.write(master, b"\x1b[200~/plugins\x1b[201~\r")
        state = "enabled" if enabled else "disabled"
        wait_for(f"curl-safari@curl-safari-local ({state})".encode())
        assert len(server.requests) == 1, len(server.requests)
        first = json.dumps(server.requests[0]["input"])
        observed = {
            "catalog": "- curl-safari:curl-safari:" in first,
            "injected": "<name>curl-safari:curl-safari</name>" in first,
        }
        assert observed == {"catalog": enabled, "injected": enabled}, observed
        (artifacts / f"tui-override-{state}.json").write_text(json.dumps({
            "requests": 1, "plugin_skill_present": enabled,
            "session_listing_matches": True, "forced_reload_injection_matches": True,
        }, indent=2) + "\n")
    finally:
        process.terminate()
        try:
            process.wait(timeout=10)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait(timeout=5)
        os.close(master)


MCP_SERVER = """import json, sys
for line in sys.stdin:
    request = json.loads(line)
    if "id" not in request:
        continue
    method = request["method"]
    if method == "initialize":
        result = {"protocolVersion": request["params"]["protocolVersion"],
                  "capabilities": {"tools": {}}, "serverInfo": {"name": "plugin-probe", "version": "1"}}
    elif method == "tools/list":
        result = {"tools": [{"name": "ping", "description": "Plugin test",
                            "inputSchema": {"type": "object", "properties": {}}}]}
    elif method == "tools/call":
        result = {"content": [{"type": "text", "text": "plugin-mcp-ok"}]}
    else:
        result = {}
    print(json.dumps({"jsonrpc": "2.0", "id": request["id"], "result": result}), flush=True)
"""


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--codex", type=Path, required=True)
    parser.add_argument("--plugin-source", type=Path, required=True, help="Already-built Curl Safari package")
    parser.add_argument("--safari-cli", type=Path, required=True, help="macos-safari-browser-use/scripts/safari")
    parser.add_argument("--artifacts", type=Path, required=True)
    args = parser.parse_args()
    model = subprocess.check_output(["/usr/sbin/sysctl", "-n", "hw.model"], text=True).strip()
    if not model.startswith("VirtualMac"):
        raise SystemExit("This test must run in a disposable macOS VM.")
    args.artifacts.mkdir(parents=True, exist_ok=True)
    binary = args.codex.resolve()
    server = ProbeServer()
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    with tempfile.TemporaryDirectory(prefix="codex-plugin-probe-") as temporary:
        work = Path(temporary)
        home = work / "codex-home"
        home.mkdir()
        env = os.environ | {
            "CODEX_HOME": str(home), "CODEX_SANDBOX_NETWORK_DISABLED": "1",
            "CURL_SAFARI_SOCKET_PATH": str(work / "safari.sock"),
        }

        def codex(*argv: str | Path) -> str:
            return run([binary, *argv], env=env, cwd=work)

        provider_args = [
            "-m", "gpt-5.2-codex",
            "-c", 'model_provider="plugin-probe"',
            "-c", 'model_providers.plugin-probe.name="Plugin Probe"',
            "-c", f'model_providers.plugin-probe.base_url="{server.url}/v1"',
            "-c", 'model_providers.plugin-probe.wire_api="responses"',
            "-c", "model_providers.plugin-probe.requires_openai_auth=false",
            "-c", "features.remote_compaction_v2=false",
            "-c", "features.unified_exec=false",
        ]

        def execute(label: str, prompt: str, *, skill: bool, mcp: bool = False) -> None:
            server.requests.clear()
            output = codex(
                "exec", "--skip-git-repo-check", "--sandbox", "danger-full-access",
                *provider_args,
                prompt,
            )
            assert "Plugin probe complete." in output, output
            assert server.requests, "no Responses request"
            first = json.dumps(server.requests[0]["input"])
            assert ("- curl-safari:curl-safari:" in first) == skill, label
            assert ("<name>curl-safari:curl-safari</name>" in first) == skill, label
            tools = {tool.get("name") for tool in server.requests[0]["tools"]}
            assert ("mcp__plugin_probe__ping" in tools) == mcp, tools
            if server.command or server.mcp_tool:
                assert len(server.requests) == 2, len(server.requests)
                output_items = [
                    item for item in server.requests[1]["input"]
                    if item.get("type") == "function_call_output" and item.get("call_id") == "call-plugin"
                ]
                marker = "plugin-mcp-ok" if server.mcp_tool else "curl-safari-plugin-ok"
                assert marker in json.dumps(output_items), output_items
            # Persist only structural evidence; not request bodies or fetched private content.
            (args.artifacts / f"{label}.json").write_text(json.dumps({
                "requests": len(server.requests), "plugin_skill_present": skill,
                "plugin_mcp_present": mcp, "tool_result_verified": bool(server.command or server.mcp_tool),
            }, indent=2) + "\n")

        codex("plugin", "marketplace", "add", args.plugin_source.resolve())
        installed = json.loads(codex("plugin", "add", "curl-safari@curl-safari-local", "--json"))
        root = Path(installed["installed_path"])
        assert (root / "bin/curl-safari").is_file()
        assert (root / "CurlSafariServer.app/Contents/MacOS/curl-safari-server").is_file()
        assert json.loads(codex("plugin", "list", "--json")) == [installed]

        run([args.safari_cli, "open", f"{server.url}/login", "new-window"], env=env, cwd=work)
        assert server.cookie_login.wait(20), "Safari did not visit the localhost login page"
        # Safari persists cookies asynchronously. Poll the real helper/client rather than reading its store.
        helper_log = args.artifacts / "helper.log"
        with helper_log.open("w") as log:
            helper = subprocess.Popen(
                [root / "CurlSafariServer.app/Contents/MacOS/curl-safari-server"],
                env=env, stdout=log, stderr=log,
            )
            try:
                deadline = time.monotonic() + 40
                last_error = ""
                while time.monotonic() < deadline:
                    try:
                        response = run(
                            [root / "bin/curl-safari", "-fsS", f"{server.url}/private"],
                            env=env, cwd=work,
                        )
                        assert json.loads(response)["authenticated"]
                        break
                    except (RuntimeError, AssertionError) as error:
                        last_error = str(error)
                        time.sleep(1)
                else:
                    raise RuntimeError(f"Safari helper authentication failed: {last_error}")
                server.command = f"{shlex.quote(str(root / 'bin/curl-safari'))} -fsS {server.url}/private"
                execute("enabled", "Use $curl-safari:curl-safari to fetch the test account page.", skill=True)
                assert server.authenticated_fetch.is_set()
            finally:
                helper.terminate()
                helper.wait(timeout=10)

        server.command = None
        verify_tui_override(binary, provider_args, server, env, work, args.artifacts, enabled=False)
        codex("plugin", "disable", "curl-safari@curl-safari-local")
        execute("disabled", "Use $curl-safari:curl-safari if it is available.", skill=False)
        verify_tui_override(binary, provider_args, server, env, work, args.artifacts, enabled=True)
        codex("plugin", "enable", "curl-safari@curl-safari-local")
        execute("reenabled", "Use $curl-safari:curl-safari.", skill=True)
        codex("plugin", "remove", "curl-safari@curl-safari-local")
        assert not root.exists()
        execute("removed", "Use $curl-safari:curl-safari if it is available.", skill=False)
        codex("plugin", "marketplace", "remove", "curl-safari-local")
        assert json.loads(codex("plugin", "list", "--json")) == []

        # Exercise bundled MCP through the same install and ordinary agent tool-dispatch paths.
        source = work / "mcp-package"
        (source / ".codex-plugin").mkdir(parents=True)
        (source / ".agents/plugins").mkdir(parents=True)
        (source / ".codex-plugin/plugin.json").write_text(json.dumps({
            "name": "probe", "mcpServers": {"plugin_probe": {
                "command": sys.executable, "args": ["${CODEX_PLUGIN_ROOT}/server.py"],
            }},
        }))
        (source / ".agents/plugins/marketplace.json").write_text(json.dumps({
            "name": "probe-local",
            "plugins": [{"name": "probe", "source": {"source": "local", "path": "."}}],
        }))
        (source / "server.py").write_text(MCP_SERVER)
        codex("plugin", "marketplace", "add", source)
        codex("plugin", "add", "probe@probe-local")
        server.mcp_tool = "mcp__plugin_probe__ping"
        execute("mcp-enabled", "Call the plugin probe.", skill=False, mcp=True)
        server.mcp_tool = None
        codex("plugin", "disable", "probe@probe-local")
        execute("mcp-disabled", "Report available tools.", skill=False)
        codex("plugin", "remove", "probe@probe-local")
        codex("plugin", "marketplace", "remove", "probe-local")
    server.shutdown()
    thread.join(timeout=5)
    print("Plugin VM verification passed: Safari fetch, skill/MCP lifecycle, TUI session overrides.")


if __name__ == "__main__":
    main()
