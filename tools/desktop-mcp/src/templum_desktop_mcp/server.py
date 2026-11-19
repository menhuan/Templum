"""A stdio MCP server for plans and bounded delegation to existing canvas agents."""

from __future__ import annotations

import argparse
import asyncio
import json
import os
import re
import sys
from pathlib import Path

from jsonschema import Draft202012Validator
from mcp import MCPError
from mcp.server import Server
from mcp.server.stdio import stdio_server
from mcp.types import CallToolResult, ListToolsResult, TextContent, Tool

from .api import Api, BridgeError, read_token


QUEUE_NATIVE = {"queue_card", "cancel_queued_card"}
NATIVE = {"edit_note", "create_card", "edit_card", "move_card", "comment_card", "notify"} | QUEUE_NATIVE
STRING = {"type": "string", "minLength": 1}
NUMBER = {"type": "integer", "minimum": 0}
TASK = {"type": "string", "pattern": r"^t[0-9]+$"}
CURSOR = {"type": "string", "pattern": r"^[0-9a-f]{8}-[0-9]+$"}
RECORD = re.compile(r"^\[templum-desktop-mcp run ([0-9a-f]{8}) task (t[0-9]+) block ([A-Za-z0-9_-]+) (completed|blocked|failed)\](?:\n|$)")
QUEUE_UPGRADE = "Update and restart Templum with queue_card and cancel_queued_card support, then reconnect this desktop."

PLAN_INSTRUCTIONS = """Within a planning conversation or established main-agent plan session (including a dedicated collaboration workspace), maintain one shared canvas plan: agreed decisions, open questions, postponed work and linked native card IDs. Natural agreement on executable scope authorizes capture and queueing; users need not mention Templum or ask to create tasks each turn. Keep tentative, unresolved, postponed and plan-only items in the note. Unrelated chats do not authorize writes.

Locate the plan with list_workspaces; use the sole visible workspace without asking its name. Inspect before changing notes/cards. Reuse card IDs through retries/replanning; for new work, check existing cards and save any new ID in the plan before queueing. After conflicts or uncertain writes, inspect and reconcile instead of creating another card.

Check queue_support before creating executable cards. An unsupported host leaves work pending in the plan; never substitute direct delegation. Queue only agreed scope, preserving required action permissions. Cancel queued work before postponing or changing scope; if running, retain the proposed change in the plan and report that it is still running.

The host executes independently of desktop polling. On return, read card.queue outcomes/verification/blockers and update the linked plan. A queue receipt is not completion; worker completion is not user acceptance. For explicit direct handoffs, preserve task/block/cursor and retrieve the exact result. Answer permission questions in Templum."""

NATIVE_GUIDANCE = {
    "create_card": "In an active shared plan, reuse the linked card for the same work; save a new card ID in the plan before queue_card.",
    "edit_card": "Keep the linked card's agreed outcome, scope and acceptance evidence current; cancel queued work before changing its scope. Preserve completed scope/results; new work after completion needs a linked follow-up card.",
    "queue_card": "Use in an active shared plan for agreed executable work, preserving its linked card ID. The host runs it autonomously; read card.queue for the outcome.",
    "cancel_queued_card": "Use before postponing or changing a queued plan item. A running task cannot be cancelled with this tool; retain the requested change in the plan.",
}


def tool(name, description, properties=None, required=()):
    return Tool(
        name=name,
        description=description,
        input_schema={"type": "object", "properties": properties or {}, "required": list(required), "additionalProperties": False},
    )


CUSTOM = [
    tool("list_workspaces", "List the workspaces this API token can see."),
    tool("inspect_workspace", "Read a workspace's blocks, wires, plan notes (text, rev, cut), cards including native queue status/results, pending questions and host queue_support. In a shared-plan discussion, inspect this before updating linked items or retrying a write; never overwrite a cut note.", {"workspace": STRING}, ["workspace"]),
    tool("create_note", "Create the shared plan note only when the active Templum plan has none; inspect and reuse an existing note first. It holds agreed decisions, open questions, postponed work and linked card IDs. Optionally add a tab to a note block; an empty canvas gets a note block. Returns note and rev.", {"workspace": STRING, "block": STRING, "name": {"type": "string"}, "text": {"type": "string"}}, ["workspace", "text"]),
    tool("delegate_task", "Explicit direct handoff to an existing canvas agent without waiting; use queue_card for agreed shared-plan work that the host should schedule. Returns task, block and cursor: keep these for get_task_result. Optional card preserves a dispatch receipt and asks a wired worker to record its result. Acknowledgement is not completion.", {"block": STRING, "task": STRING, "card": NUMBER}, ["block", "task"]),
    tool("get_task_result", "Use solely for delegate_task receipts; an inspect_workspace snapshot cursor is not a dispatch receipt. For queue_card, use inspect_workspace to read card.queue; after normal enqueue the host notifies without polling. Recover the direct handoff's answer or blockers with its block, task id, cursor and optional card. timeout (0–45 seconds) only bounds this read, never fails the task. Cards keep explicit worker reports; API notices last at most one hour/256 notices and are lost on API restart. Idle alone is not completion.", {"block": STRING, "task": TASK, "cursor": CURSOR, "card": NUMBER, "timeout": {"type": "number", "minimum": 0, "maximum": 45}}, ["block", "task", "cursor"]),
    # Keep these discoverable on older hosts so a queue request returns an actionable
    # upgrade error. Compatible hosts supply the authoritative schemas below.
    tool("queue_card", NATIVE_GUIDANCE["queue_card"], {"card": NUMBER, "workspace": STRING}, ["card", "workspace"]),
    tool("cancel_queued_card", NATIVE_GUIDANCE["cancel_queued_card"], {"card": NUMBER, "workspace": STRING}, ["card", "workspace"]),
]


class Bridge:
    def __init__(self, api: Api):
        self.api = api
        self.schemas = {item.name: item for item in CUSTOM}
        self.native = set()

    async def list_tools(self):
        registry = await self.api.request("GET", "/v1/tools")
        self.schemas = {item.name: item for item in CUSTOM}
        self.native = {item["name"] for item in registry}
        for item in registry:
            if item["name"] in NATIVE:
                guidance = NATIVE_GUIDANCE.get(item["name"])
                self.schemas[item["name"]] = Tool(
                    name=item["name"],
                    description=item["description"] + (f" {guidance}" if guidance else ""),
                    input_schema=item["inputSchema"],
                )
        if not QUEUE_NATIVE <= self.native:
            for name in QUEUE_NATIVE:
                item = self.schemas[name]
                self.schemas[name] = item.model_copy(update={"description": item.description + " Unavailable on this host. " + QUEUE_UPGRADE})
        return list(self.schemas.values())

    async def snapshot(self):
        async with self.api.events() as events:
            return await self.first_snapshot(events)

    async def first_snapshot(self, events):
        try:
            async with asyncio.timeout(15):
                first = await anext(events)
        except (TimeoutError, StopAsyncIteration):
            raise BridgeError("protocol", "Templum did not send its initial snapshot.") from None
        if first["event"] != "snapshot" or not isinstance(first["data"], dict) or not first["id"]:
            raise BridgeError("protocol", "Templum's initial snapshot was not understood.")
        return first

    def workspace(self, state, workspace):
        found = next((w for w in state["workspaces"] if w["id"] == workspace), None)
        if found is None:
            raise BridgeError("not_found", "This workspace is not visible to the token; use list_workspaces for its id.")
        return found

    def block(self, state, block):
        found = next((b for b in state["blocks"] if b["id"] == block), None)
        if found is None:
            raise BridgeError("not_found", "This block is not visible to the token.")
        return found

    async def inspect_workspace(self, workspace):
        await self.list_tools()
        snapshot = await self.snapshot()
        state = snapshot["data"]
        blocks = [b for b in state["blocks"] if b["workspace"] == workspace]
        block_ids = {b["id"] for b in blocks}
        note_ids = {n for b in blocks for n in (b.get("notes") or {}).get("tabs", [])}
        board_ids = {b.get("board") for b in blocks} - {None}
        return {
            "workspace": self.workspace(state, workspace),
            "queue_support": {"supported": QUEUE_NATIVE <= self.native, "missing_tools": sorted(QUEUE_NATIVE - self.native), "upgrade": QUEUE_UPGRADE if not QUEUE_NATIVE <= self.native else None},
            "cursor": snapshot["id"],
            "blocks": blocks,
            "wires": [w for w in state["wires"] if w["a"] in block_ids and w["b"] in block_ids],
            "notes": [n for n in state["notes"] if n["id"] in note_ids or n.get("workspace") == workspace],
            "boards": [b for b in state["boards"] if b["id"] in board_ids],
            "cards": [c for c in state["cards"] if c["board"] in board_ids],
            "requests": [r for r in state["requests"] if r.get("workspace") == workspace or r.get("block") in block_ids],
        }

    async def create_note(self, workspace, text, name=None, block=None):
        state = (await self.snapshot())["data"]
        self.workspace(state, workspace)
        created_block = block is None
        if block:
            target = self.block(state, block)
            if target["workspace"] != workspace or target["kind"] != "note":
                raise BridgeError("invalid", "block must be a note block in this workspace.")
            args = {"block": block, "text": text}
            if name is not None:
                args["name"] = name
            receipt = await self.api.tool("new_note", args)
            made = re.match(r"^Made note ([0-9]+) in ", receipt["text"])
            if made is None:
                raise BridgeError("protocol", "Templum did not identify its new note. Inspect the workspace before retrying.", receipt=receipt)
            note = int(made[1])
        else:
            receipt = await self.api.tool("add_block", {"workspace": workspace, "kind": "note", "text": text})
            block = receipt.get("block")
        if not block:
            raise BridgeError("protocol", "Templum created the note but did not identify its block. Inspect the workspace before retrying.", receipt=receipt)
        after = (await self.snapshot())["data"]
        target = self.block(after, block)
        if created_block:
            tabs = (target.get("notes") or {}).get("tabs", [])
            if len(tabs) != 1:
                raise BridgeError("protocol", "The new block's note cannot be identified unambiguously. Inspect the workspace before retrying.", receipt=receipt)
            note = tabs[0]
            if name is not None:
                await self.api.tool("edit_note", {"block": block, "note": note, "name": name})
                after = (await self.snapshot())["data"]
        return {"receipt": receipt, "block": block, "note": next(n for n in after["notes"] if n["id"] == note)}

    async def edit_note(self, **arguments):
        if "text" in arguments:
            if "rev" not in arguments:
                raise BridgeError("invalid", "Editing text requires the rev of the text you read.")
            state = (await self.snapshot())["data"]
            note = next((n for n in state["notes"] if n["id"] == arguments["note"]), None)
            if note is not None and note.get("cut"):
                raise BridgeError("note_cut", "This note's text was cut; do not replace text you did not read.", current=note)
        try:
            return await self.api.tool("edit_note", arguments)
        except BridgeError as error:
            if error.result["error"].get("http_status") == 422 and "text" in arguments:
                state = (await self.snapshot())["data"]
                current = next((n for n in state["notes"] if n["id"] == arguments["note"]), None)
                error.result["error"]["current"] = current
            raise

    async def delegate_task(self, block, task, card=None):
        snapshot = await self.snapshot()
        state = snapshot["data"]
        worker = self.block(state, block)
        if not worker.get("agent"):
            raise BridgeError("invalid", "Choose an existing agent block; a shell is not a worker.")
        agent = worker["agent"]
        if agent.get("task") or agent["state"] not in {"idle", "done"}:
            raise BridgeError("worker_busy", "This worker already has work or is waiting for its person. Inspect its current task; delegate_task starts only on a ready, idle worker.", worker=worker)
        if card is not None:
            source = await self.api.request("GET", f"/v1/cards/{card}")
            queue = source.get("queue")
            if queue is not None and queue["status"] in {"queued", "running", "blocked", "failed"}:
                if queue["status"] == "queued":
                    action = "Use cancel_queued_card before an explicit direct handoff."
                elif queue["status"] == "running":
                    action = "Wait for its result in card.queue; cancel_queued_card cannot stop a running worker."
                else:
                    action = "Use queue_card for recovery; the host checks whether the prior worker/task still owns this card."
                raise BridgeError("card_in_queue", f"Card {card} is already {queue['status']} in the host queue. {action}", card=card, queue=queue)
            boards = {b["id"] for b in state["blocks"] if b["workspace"] == worker["workspace"] and b.get("board") == source["board"]}
            if not any({w["a"], w["b"]} & boards and block in {w["a"], w["b"]} for w in state["wires"]):
                raise BridgeError("card_unreachable", "This worker must be wired to the card's kanban block to save a result. Connect them on the canvas, or omit card for retained API notices only.")
            marker = f"[templum-desktop-mcp run {snapshot['id'].split('-')[0]} task tN block {block}"
            task += (
                f"\n\nHandoff record: card {card}. Before replying, use Templum comment_card on this card to save the actual outcome, verification and any blockers. "
                f"Start the comment with {marker} completed], {marker} blocked], or {marker} failed], "
                "replacing tN with the task id in Templum's [from ..., task tN] header. A completed report describes your work; it does not claim the person's acceptance."
            )
        task += "\n\nWhen finished or blocked, use Templum notify to tell the person, then reply with the actual result or blocker."
        dispatch = {"block": block, "task": task, "wait": False}
        if card is not None:
            registry = await self.api.request("GET", "/v1/tools")
            native = next((item for item in registry if item["name"] == "delegate"), None)
            if native is not None and "card" in native["inputSchema"]["properties"]:
                dispatch["card"] = card
        receipt = await self.api.tool("delegate", dispatch)
        match = re.search(r"\btask (t[0-9]+)\b", receipt["text"])
        if match is None:
            raise BridgeError("protocol", "Templum accepted the call but did not identify its task. Inspect the worker before retrying.", receipt=receipt)
        if not receipt["text"].startswith(f"Handed to {block} as task {match[1]}."):
            return {"status": "changed_existing_task", "task": match[1], "block": block, "cursor": snapshot["id"], "receipt": receipt, "card": None, "reason": "Another caller handed work to this worker during dispatch; Templum changed that existing task. No new task or card receipt was created. Inspect the worker before interpreting its result."}
        result = {"status": "dispatched", "task": match[1], "block": block, "cursor": snapshot["id"], "receipt": receipt, "card": card}
        if card is not None:
            try:
                await self.api.tool("comment_card", {"card": card, "text": f"[templum-desktop-mcp run {snapshot['id'].split('-')[0]} task {match[1]} block {block} dispatched]\nCursor: {snapshot['id']}\n{receipt['text']}"})
            except BridgeError as error:
                result["receipt_error"] = error.result["error"]
        return result

    async def get_task_result(self, block, task, cursor, card=None, timeout=0):
        run = cursor.split("-")[0]
        if card is not None:
            source = await self.api.request("GET", f"/v1/cards/{card}")
            for comment in reversed(source["comments"]):
                match = RECORD.match(comment["text"])
                if match and (match[1], match[2], match[3], comment["by"]) == (run, task, block, block):
                    return {"status": match[4], "task": task, "block": block, "cursor": cursor, "source": "worker_card_report", "card": card, "report": comment, "accepted": False}
        async with self.api.events(cursor) as events:
            snapshot = await self.first_snapshot(events)
            state = snapshot["data"]
            same_run = snapshot["id"].split("-")[0] == run
            result = {"task": task, "block": block, "cursor": cursor, "card": card}
            try:
                async with asyncio.timeout(max(timeout, 0.3)):
                    async for event in events:
                        if event["event"] == "snapshot":
                            state = event["data"]
                            same_run = event["id"].split("-")[0] == run
                        elif event["event"] == "block":
                            delta = event["data"]
                            state["blocks"] = [b for b in state["blocks"] if b["id"] != delta["id"]]
                            if delta["op"] == "put":
                                state["blocks"].append(delta["value"])
                        elif event["event"] == "request":
                            delta = event["data"]
                            state["requests"] = [r for r in state["requests"] if r["id"] != delta["id"]]
                            if delta["op"] == "put":
                                state["requests"].append(delta["value"])
                        elif event["event"] == "notice":
                            notice = event["data"]
                            if same_run and event["id"].split("-")[0] == run and notice.get("block") == block:
                                match = re.match(rf"^\[task {re.escape(task)} from .*: (done|failed) in [0-9]+ s(?:,|\])", notice["text"])
                                if match:
                                    return {**result, "status": "completed" if match[1] == "done" else "failed", "source": "api_notice", "notice": notice, "accepted": False}
                    raise BridgeError("connection", "Templum's event stream ended before a result. Retry with the same cursor.")
            except TimeoutError:
                pass
        worker = next((b for b in state["blocks"] if b["id"] == block), None)
        agent = (worker or {}).get("agent") or {}
        current = same_run and (agent.get("task") or {}).get("id") == task
        requests = [r for r in state["requests"] if r.get("block") == block]
        result.update({"worker": worker, "requests": requests, "timed_out": timeout > 0})
        if current and (requests or agent.get("state") == "needs_you"):
            return {**result, "status": "blocked", "reason": agent.get("asks") or "The worker is waiting for its person; answer in Templum."}
        if current:
            return {**result, "status": "running", "reason": "No completion result was received. A read timeout does not end the task."}
        return {**result, "status": "result_unavailable", "reason": "No exact result was recovered. Notices last one hour/256 entries and API restarts clear them; idle is not proof of completion. Check the task card or the worker's saved conversation."}

    async def call(self, name, arguments):
        if name in QUEUE_NATIVE:
            await self.list_tools()
            if missing := QUEUE_NATIVE - self.native:
                raise BridgeError("host_upgrade_required", "This Templum host does not support autonomous card queues. " + QUEUE_UPGRADE + " The requested queue change was not sent; keep the linked plan item pending.", missing_tools=sorted(missing))
        if name not in self.schemas:
            if name not in NATIVE:
                raise BridgeError("unknown_tool", "This connector does not expose that tool.")
            await self.list_tools()
        if name not in self.schemas:
            raise BridgeError("unsupported", "This Templum version does not offer that tool.")
        invalid = next(Draft202012Validator(self.schemas[name].input_schema).iter_errors(arguments), None)
        if invalid:
            raise BridgeError("invalid", f"Invalid arguments for {name}: {invalid.message}")
        if name == "list_workspaces":
            return await self.api.request("GET", "/v1/workspaces")
        if name in NATIVE - {"edit_note"}:
            return await self.api.tool(name, arguments)
        return await getattr(self, name)(**arguments)


def make_server(bridge: Bridge):
    async def list_tools(_context, _params):
        try:
            return ListToolsResult(tools=await bridge.list_tools())
        except BridgeError as error:
            details = bridge.api.redact(error.result)["error"]
            raise MCPError(code=-32000, message=details["message"], data=details) from None

    async def call_tool(_context, params):
        failed = False
        try:
            value = await bridge.call(params.name, params.arguments or {})
        except BridgeError as error:
            failed, value = True, error.result
        value = bridge.api.redact(value)
        structured = value if isinstance(value, dict) else {"items": value}
        return CallToolResult(content=[TextContent(type="text", text=json.dumps(value, ensure_ascii=False))], structured_content=structured, is_error=failed)

    return Server(
        "templum-desktop",
        version="0.1.0",
        instructions=PLAN_INSTRUCTIONS,
        on_list_tools=list_tools,
        on_call_tool=call_tool,
    )


async def run(url, token_file):
    api = Api(url, read_token(Path(token_file)))
    try:
        server = make_server(Bridge(api))
        async with stdio_server() as (reader, writer):
            await server.run(reader, writer, server.create_initialization_options())
    finally:
        await api.close()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--url", default=os.environ.get("TEMPLUM_API_URL", "http://127.0.0.1:7411"))
    parser.add_argument("--token-file", default=os.environ.get("TEMPLUM_TOKEN_FILE", "~/.config/templum-desktop-mcp/token"))
    args = parser.parse_args()
    try:
        asyncio.run(run(args.url, args.token_file))
    except BridgeError as error:
        print(error.result["error"]["message"], file=sys.stderr)
        raise SystemExit(1) from None


if __name__ == "__main__":
    main()
