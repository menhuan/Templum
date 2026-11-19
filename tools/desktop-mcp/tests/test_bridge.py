"""Behavior at MCP and HTTP boundaries, with Templum's documented native responses."""

import asyncio
import copy
import json
import sys
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

import httpx
import pytest
from mcp import Client, MCPError
from mcp.client.stdio import StdioServerParameters

from templum_desktop_mcp.api import Api, BridgeError, local_url, read_token
from templum_desktop_mcp.server import Bridge, make_server


pytestmark = pytest.mark.anyio
TOKEN = "tpl_" + "a" * 43
RUN = "8f3a1c20"
CURSOR = RUN + "-100"


@pytest.fixture
def anyio_backend():
    return "asyncio"


def event(identifier, kind, data):
    return f"id: {identifier}\nevent: {kind}\ndata: {json.dumps(data)}\n\n".encode()


class HoldingStream(httpx.AsyncByteStream):
    def __init__(self, frames):
        self.frames = frames

    async def __aiter__(self):
        for frame in self.frames:
            yield frame
        await asyncio.Event().wait()


class Templum:
    """Native contracts used here; snapshots also provide the cursor needed for a handoff."""

    def __init__(self):
        self.state = {
            "workspaces": [{"id": "w1", "name": "Plan"}],
            "blocks": [
                {"id": "worker", "workspace": "w1", "kind": "terminal", "agent": {"state": "idle", "task": None}},
                {"id": "notes", "workspace": "w1", "kind": "note", "agent": None, "notes": {"tabs": [7], "shown": 7}},
                {"id": "kanban", "workspace": "w1", "kind": "kanban", "board": 1, "agent": None},
            ],
            "notes": [{"id": 7, "name": "Plan", "text": "Original", "rev": 10, "cut": False, "workspace": "w1"}],
            "wires": [{"a": "worker", "b": "kanban"}],
            "boards": [{"id": 1, "name": "Work", "columns": []}],
            "cards": [{"id": 40, "board": 1, "title": "Task", "comments": []}],
            "requests": [],
        }
        self.native = {
            "edit_note": {"block": {"type": "string"}, "note": {"type": "integer"}, "name": {"type": "string"}, "text": {"type": "string"}, "rev": {"type": "integer"}},
            "create_card": {"board": {"type": "string"}, "title": {"type": "string"}},
            "move_card": {"card": {"type": "integer"}, "column": {"type": "string"}},
            "comment_card": {"card": {"type": "integer"}, "text": {"type": "string"}},
            "notify": {"text": {"type": "string"}},
            "send_to_terminal": {"block": {"type": "string"}, "text": {"type": "string"}},
        }
        self.calls = []
        self.cursors = []
        self.notices = []
        self.snapshot_id = CURSOR
        self.refuse = None
        self.delegate_reply = "Handed to worker as task t7."
        self.switch_new_tab = False
        self.add_concurrent_tab = False

    def frames(self, request):
        cursor = request.headers.get("last-event-id")
        self.cursors.append(cursor)
        frames = [event(self.snapshot_id, "snapshot", copy.deepcopy(self.state))]
        if cursor and cursor.split("-")[0] == self.snapshot_id.split("-")[0]:
            after = int(cursor.split("-")[1])
            frames.extend(event(identifier, "notice", notice) for identifier, notice in self.notices if int(identifier.split("-")[1]) > after)
        return frames

    def error(self, code, message, status):
        return httpx.Response(status, json={"error": {"code": code, "message": message}})

    def handle(self, request):
        if request.headers.get("authorization") != f"Bearer {TOKEN}":
            return self.error("auth", "Authentication required.", 401)
        assert "templum-mirror" not in request.headers
        path = request.url.path
        if path == "/v1/events":
            return httpx.Response(200, headers={"content-type": "text/event-stream"}, stream=HoldingStream(self.frames(request)))
        if path == "/v1/workspaces":
            return httpx.Response(200, json=self.state["workspaces"])
        if path == "/v1/state":
            return httpx.Response(200, json=self.state)
        if path == "/v1/tools":
            return httpx.Response(200, json=[{"name": name, "description": name, "inputSchema": {"type": "object", "properties": properties, "required": []}} for name, properties in self.native.items()])
        if path == "/v1/cards/40":
            return httpx.Response(200, json=self.state["cards"][0])
        if not path.startswith("/v1/tools/"):
            return self.error("not_found", "Not found.", 404)
        name = path.rsplit("/", 1)[1]
        args = json.loads(request.content)
        self.calls.append((name, args))
        if self.refuse:
            return self.error(*self.refuse)
        if name == "edit_note":
            note = next(n for n in self.state["notes"] if n["id"] == args["note"])
            if "text" in args:
                if args["rev"] != note["rev"]:
                    return self.error("refused", f"Note {note['id']} changed since: its newer text and rev are in the state.", 422)
                note.update(text=args["text"], rev=note["rev"] + 1)
            if "name" in args:
                note["name"] = args["name"]
        elif name in {"new_note", "add_block"}:
            block = args.get("block", "new-note-block")
            self.state["notes"].append({"id": 8, "name": args.get("name", "Untitled"), "text": args["text"], "rev": 11, "cut": False, "workspace": "w1"})
            if name == "new_note":
                target = next(b for b in self.state["blocks"] if b["id"] == block)
                target["notes"]["tabs"].append(8)
                target["notes"]["shown"] = 7 if self.switch_new_tab else 8
            else:
                self.state["blocks"].append({"id": block, "workspace": "w1", "kind": "note", "notes": {"tabs": [8, 7] if self.add_concurrent_tab else [8], "shown": 8}})
            return httpx.Response(200, json={"text": f"Made note 8 in {block}." if name == "new_note" else f"Opened {block}.", "block": block, "workspace": "w1"})
        elif name == "delegate":
            self.state["blocks"][0]["agent"] = {"state": "working", "task": {"id": "t7"}}
            return httpx.Response(200, json={"text": self.delegate_reply})
        elif name == "comment_card":
            self.state["cards"][0]["comments"].append({"number": 1, "by": "you", "text": args["text"]})
        return httpx.Response(200, json={"text": "Done."})


@pytest.fixture
async def connection():
    templum = Templum()
    api = Api("http://127.0.0.1:7411", TOKEN, transport=httpx.MockTransport(templum.handle))
    bridge = Bridge(api)
    async with Client(make_server(bridge), mode="legacy") as client:
        yield templum, client
    await api.close()


def value(result):
    return result.structured_content


async def test_native_schemas_and_no_terminal_or_approval_passthrough(connection):
    templum, client = connection
    tools = {tool.name: tool for tool in (await client.list_tools()).tools}
    assert tools["move_card"].input_schema["properties"] == templum.native["move_card"]
    assert set(tools) == {"list_workspaces", "inspect_workspace", "create_note", "delegate_task", "get_task_result", "edit_note", "create_card", "move_card", "comment_card", "notify"}
    result = await client.call_tool("send_to_terminal", {"block": "worker", "text": "echo unintended"})
    assert result.is_error and value(result)["error"]["code"] == "unknown_tool"
    assert not templum.calls


async def test_scoped_workspace_snapshot_includes_revision_blockers_and_cursor(connection):
    templum, client = connection
    templum.state["requests"] = [{"id": "r1", "block": "worker", "workspace": "w1", "summary": "Choose a database"}]
    result = value(await client.call_tool("inspect_workspace", {"workspace": "w1"}))
    assert result["notes"][0]["rev"] == 10
    assert result["notes"][0]["text"] == "Original"
    assert result["cursor"] == CURSOR and templum.cursors == [None]
    assert result["requests"][0]["summary"] == "Choose a database"
    hidden = await client.call_tool("inspect_workspace", {"workspace": "other"})
    assert hidden.is_error and value(hidden)["error"]["code"] == "not_found"


async def test_note_conflict_returns_current_revision_without_retrying_or_overwriting(connection):
    templum, client = connection
    await client.list_tools()
    templum.state["notes"][0].update(text="Changed on the canvas", rev=11)
    result = await client.call_tool("edit_note", {"block": "notes", "note": 7, "text": "Old desktop text", "rev": 10})
    assert result.is_error
    assert value(result)["error"]["current"]["text"] == "Changed on the canvas"
    assert value(result)["error"]["current"]["rev"] == 11
    assert len(templum.calls) == 1
    assert templum.state["notes"][0]["text"] == "Changed on the canvas"
    ok = await client.call_tool("edit_note", {"block": "notes", "note": 7, "text": "Merged text", "rev": 11})
    assert not ok.is_error and templum.state["notes"][0]["text"] == "Merged text"


async def test_cut_note_and_missing_revision_never_write(connection):
    templum, client = connection
    await client.list_tools()
    templum.state["notes"][0]["cut"] = True
    result = await client.call_tool("edit_note", {"block": "notes", "note": 7, "text": "Truncated", "rev": 10})
    assert result.is_error and value(result)["error"]["code"] == "note_cut"
    result = await client.call_tool("edit_note", {"block": "notes", "note": 7, "text": "No rev"})
    assert result.is_error and value(result)["error"]["code"] == "invalid"
    assert not templum.calls


async def test_empty_workspace_gets_a_plan_note_and_existing_tab_uses_returned_id(connection):
    templum, client = connection
    original_note_block = copy.deepcopy(templum.state["blocks"][1])
    templum.state["blocks"] = []
    result = value(await client.call_tool("create_note", {"workspace": "w1", "text": "Shared plan", "name": "Agreed plan"}))
    assert result["block"] == "new-note-block"
    assert result["note"]["text"] == "Shared plan" and result["note"]["name"] == "Agreed plan"
    assert result["note"]["rev"] == 11
    templum.state["blocks"].append(original_note_block)
    templum.state["notes"] = templum.state["notes"][:1]
    templum.switch_new_tab = True
    result = value(await client.call_tool("create_note", {"workspace": "w1", "block": "notes", "text": "Second", "name": "Second plan"}))
    assert result["note"]["id"] == 8 and result["note"]["name"] == "Second plan"
    assert templum.state["notes"][0]["name"] == "Plan"


async def test_ambiguous_created_note_preserves_receipt_and_does_not_rename(connection):
    templum, client = connection
    templum.add_concurrent_tab = True
    result = await client.call_tool("create_note", {"workspace": "w1", "text": "Created", "name": "Plan"})
    assert result.is_error and value(result)["error"]["receipt"]["block"] == "new-note-block"
    assert [name for name, _ in templum.calls] == ["add_block"]


async def test_new_desktop_recovers_notice_replayed_after_newer_snapshot(connection):
    templum, client = connection
    dispatched = value(await client.call_tool("delegate_task", {"block": "worker", "task": "Implement the agreed plan"}))
    assert dispatched["status"] == "dispatched" and dispatched["cursor"] == CURSOR
    assert templum.calls[0][1]["wait"] is False
    assert "Templum notify" in templum.calls[0][1]["task"]
    templum.state["blocks"][0]["agent"] = {"state": "idle", "task": None}
    templum.snapshot_id = RUN + "-200"
    templum.notices = [(RUN + "-101", {"block": "worker", "workspace": "w1", "text": "[task t7 from Worker: done in 3 s] Its answer\nImplemented; mvn test passed."})]
    second_api = Api("http://127.0.0.1:7411", TOKEN, transport=httpx.MockTransport(templum.handle))
    async with Client(make_server(Bridge(second_api)), mode="legacy") as second:
        result = value(await second.call_tool("get_task_result", {"block": "worker", "task": "t7", "cursor": CURSOR}))
    await second_api.close()
    assert result["status"] == "completed" and result["source"] == "api_notice"
    assert "Implemented; mvn test passed." in result["notice"]["text"]
    assert result["accepted"] is False and templum.cursors[-1] == CURSOR


async def test_busy_worker_never_delegates_and_native_concurrent_change_is_honest(connection):
    templum, client = connection
    templum.state["blocks"][0]["agent"] = {"state": "working", "task": {"id": "t6"}}
    result = await client.call_tool("delegate_task", {"block": "worker", "task": "New task", "card": 40})
    assert result.is_error and value(result)["error"]["code"] == "worker_busy" and not templum.calls
    templum.state["blocks"][0]["agent"] = {"state": "idle", "task": None}
    templum.delegate_reply = "Typed into worker as a change to task t6."
    result = value(await client.call_tool("delegate_task", {"block": "worker", "task": "New task", "card": 40}))
    assert result["status"] == "changed_existing_task" and result["card"] is None
    assert len(templum.calls) == 1 and not templum.state["cards"][0]["comments"]


async def test_blocker_and_bounded_read_do_not_claim_completion_or_failure(connection):
    templum, client = connection
    templum.state["blocks"][0]["agent"] = {"state": "working", "task": {"id": "t7"}}
    args = {"block": "worker", "task": "t7", "cursor": CURSOR, "timeout": 0.01}
    result = value(await client.call_tool("get_task_result", args))
    assert result["status"] == "running" and result["timed_out"] is True
    templum.state["blocks"][0]["agent"].update(state="needs_you", asks="Approve a migration")
    templum.state["requests"] = [{"id": "r12", "block": "worker", "summary": "Approve migration"}]
    result = value(await client.call_tool("get_task_result", args))
    assert result["status"] == "blocked" and result["requests"][0]["id"] == "r12"
    templum.state["blocks"][0]["agent"] = {"state": "idle", "task": None}
    result = value(await client.call_tool("get_task_result", args))
    assert result["status"] == "result_unavailable"


async def test_restart_does_not_confuse_reused_task_and_durable_card_checks_run_block_author(connection):
    templum, client = connection
    templum.snapshot_id = "1234abcd-200"
    templum.state["blocks"][0]["agent"] = {"state": "needs_you", "task": {"id": "t7"}}
    templum.notices = [("1234abcd-201", {"block": "worker", "text": "[task t7 from Worker: done in 1 s] New unrelated task"})]
    comments = templum.state["cards"][0]["comments"]
    comments.extend([
        {"by": "worker", "text": "[templum-desktop-mcp task t7 completed]\nOld ambiguous report"},
        {"by": "worker", "text": "[templum-desktop-mcp run 1234abcd task t7 block worker completed]\nOther run"},
        {"by": "other", "text": f"[templum-desktop-mcp run {RUN} task t7 block worker completed]\nWrong author"},
        {"by": "worker", "text": f"[templum-desktop-mcp run {RUN} task t7 block other completed]\nOther block"},
    ])
    args = {"block": "worker", "task": "t7", "cursor": CURSOR, "card": 40, "timeout": 0.01}
    result = value(await client.call_tool("get_task_result", args))
    assert result["status"] == "result_unavailable"
    comments.append({"by": "worker", "text": f"[templum-desktop-mcp run {RUN} task t7 block worker completed]\nFixed login and ran the regression suite."})
    result = value(await client.call_tool("get_task_result", args))
    assert result["status"] == "completed" and result["source"] == "worker_card_report"
    assert result["accepted"] is False and "Fixed login" in result["report"]["text"]


async def test_card_receipt_and_wiring_requirement(connection):
    templum, client = connection
    templum.state["wires"] = []
    result = await client.call_tool("delegate_task", {"block": "worker", "task": "Work", "card": 40})
    assert result.is_error and value(result)["error"]["code"] == "card_unreachable" and not templum.calls
    templum.state["wires"] = [{"a": "worker", "b": "kanban"}]
    result = value(await client.call_tool("delegate_task", {"block": "worker", "task": "Work", "card": 40}))
    assert result["status"] == "dispatched"
    assert f"run {RUN} task tN block worker completed]" in templum.calls[0][1]["task"]
    assert CURSOR in templum.state["cards"][0]["comments"][0]["text"]


@pytest.mark.parametrize("status,code", [(401, "auth"), (403, "scope"), (422, "refused"), (429, "rate_limited")])
async def test_api_errors_are_explicit_mcp_errors_and_secrets_are_redacted(connection, status, code):
    templum, client = connection
    await client.list_tools()
    templum.refuse = (code, f"Failed; diagnostic accidentally echoed {TOKEN}", status)
    result = await client.call_tool("notify", {"text": "Hello"})
    assert result.is_error and value(result)["error"]["http_status"] == status
    assert value(result)["error"]["code"] == code
    assert TOKEN not in result.content[0].text
    assert "[redacted]" in result.content[0].text


async def test_expired_token_is_actionable_during_tool_discovery():
    templum = Templum()
    api = Api("http://127.0.0.1:7411", "tpl_" + "b" * 43, transport=httpx.MockTransport(templum.handle))
    try:
        async with Client(make_server(Bridge(api)), mode="legacy") as client:
            with pytest.raises(MCPError, match="Authentication required"):
                await client.list_tools()
            result = await client.call_tool("list_workspaces", {})
            assert result.is_error and value(result)["error"]["http_status"] == 401
    finally:
        await api.close()


def test_token_requires_private_user_file_and_never_reports_contents(tmp_path):
    path = tmp_path / "token"
    path.write_text(TOKEN)
    path.chmod(0o600)
    assert read_token(path) == TOKEN
    path.chmod(0o644)
    with pytest.raises(BridgeError, match="chmod 600"):
        read_token(path)
    path.chmod(0o600)
    path.write_text("incorrect-secret")
    with pytest.raises(BridgeError) as error:
        read_token(path)
    assert "incorrect-secret" not in str(error.value)
    link = tmp_path / "link"
    link.symlink_to(path)
    with pytest.raises(BridgeError):
        read_token(link)


@pytest.mark.parametrize("url", ["http://example.com:7411", "http://127.0.0.1@evil.com", "http://127.0.0.1:7411/?token=x", "http://127.0.0.1:7411/v1", "file:///tmp/token"])
def test_no_remote_token_exfiltration_or_url_credentials(url):
    with pytest.raises(BridgeError):
        local_url(url)


async def test_real_stdio_initialize_list_and_tool_roundtrip(tmp_path):
    templum = Templum()

    class Handler(BaseHTTPRequestHandler):
        protocol_version = "HTTP/1.1"

        def log_message(self, *_args):
            pass

        def handle_request(self):
            body = self.rfile.read(int(self.headers.get("content-length", "0")))
            request = httpx.Request(self.command, "http://127.0.0.1" + self.path, headers=dict(self.headers), content=body)
            if self.path == "/v1/events" and self.headers.get("Authorization") == f"Bearer {TOKEN}":
                payload, status, content_type = b"".join(templum.frames(request)), 200, "text/event-stream"
            else:
                response = templum.handle(request)
                payload, status, content_type = response.content, response.status_code, "application/json"
            self.send_response(status)
            self.send_header("Content-Type", content_type)
            self.send_header("Content-Length", str(len(payload)))
            self.end_headers()
            self.wfile.write(payload)

        do_GET = handle_request
        do_POST = handle_request

    httpd = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    thread = threading.Thread(target=httpd.serve_forever, daemon=True)
    thread.start()
    token = tmp_path / "token"
    token.write_text(TOKEN)
    token.chmod(0o600)
    params = StdioServerParameters(command=sys.executable, args=["-m", "templum_desktop_mcp.server", "--url", f"http://127.0.0.1:{httpd.server_port}", "--token-file", str(token)])
    try:
        async with Client(params, mode="legacy") as client:
            assert (await client.list_tools()).tools
            result = value(await client.call_tool("inspect_workspace", {"workspace": "w1"}))
            assert result["notes"][0]["rev"] == 10
            created = value(await client.call_tool("create_note", {"workspace": "w1", "text": "Desktop plan", "name": "Plan"}))
            assert created["note"]["text"] == "Desktop plan"
            task = value(await client.call_tool("delegate_task", {"block": "worker", "task": "Read the shared plan"}))
            templum.notices = [(RUN + "-101", {"block": "worker", "text": "[task t7 from Worker: done in 2 s]\nRead the shared plan and finished."})]
            recovered = value(await client.call_tool("get_task_result", {"block": task["block"], "task": task["task"], "cursor": task["cursor"]}))
            assert recovered["status"] == "completed" and "finished" in recovered["notice"]["text"]
    finally:
        httpd.shutdown()
        httpd.server_close()
        thread.join(timeout=2)
