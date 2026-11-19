# Templum for desktop MCP clients

Discuss a plan in Codex or Claude Desktop and keep the plan in a Templum canvas note. A second client connected to the same workspace can read that note, see the workers and task cards, and continue the discussion. The desktop chat history stays in the client that created it.

## Connect your workspace

1. Start Templum from this branch and open the workspace that should hold your plan. For automatic pickup, connect the workspace's kanban block to the canvas agents that may take work. Start a worker such as Codex or Claude Code on the canvas if you want to delegate work to it. Check that the CLI prompt has appeared and finish any first-run prompts there. Templum can show a newly opened block as idle while its shell is still starting.
2. Under **Settings → API**, enable the API at its default loopback address. Create a **Control** token limited to that workspace. Copy the token when Templum displays it.
3. Store the token outside the checkout. On macOS, run the following immediately after copying it. It writes the clipboard to a private file without printing its contents.

   ```sh
   install -d -m 700 ~/.config/templum-desktop-mcp
   (umask 077 && pbpaste > ~/.config/templum-desktop-mcp/token)
   chmod 600 ~/.config/templum-desktop-mcp/token
   ```

4. In this directory, install the locked connector and print its absolute executable path.

   ```sh
   uv sync --locked --no-dev
   realpath .venv/bin/templum-desktop-mcp
   ```

### Codex

Register the executable from the previous step. For example, run this in the connector directory:

```sh
codex mcp add templum-desktop -- "$(realpath .venv/bin/templum-desktop-mcp)"
```

### Claude Desktop

Add the following entry to `mcpServers` in Claude Desktop's developer configuration, using the absolute executable path printed above. Keep your other server entries, then restart Claude Desktop.

```json
{
  "mcpServers": {
    "templum-desktop": {
      "command": "/absolute/path/to/Templum/tools/desktop-mcp/.venv/bin/templum-desktop-mcp"
    }
  }
}
```

## Plan in conversation

Open the same Templum workspace in either desktop chat. For example, “Let’s plan the release. The smoke test is agreed: check that the app opens the sample project and record the result. I’m still deciding whether we need a migration guide.”

The controller keeps the migration guide as an open question in the shared note. It writes the agreed smoke test into a task card with enough context for another agent to run it and queues it. You can switch desktops and ask “Where did we leave the migration guide?” Once you settle it, the controller updates the same plan. You can also say “just discuss this” or “hold off on execution.”

Templum waits for an eligible worker wired to the board. When one is free, it receives the scope that was confirmed when the card was queued. To change it before pickup, withdraw the enrollment, edit the same card and queue it again. The worker reports its result or blocker on that card through Templum. A permission prompt keeps the current task with its worker. A blocked outcome stays available for discussion until you resolve it. Templum sends a notification when a result or blocker needs your attention. Keep Templum and the workers running. Either desktop chat can close while they work. To see the result later, ask the other desktop to open the workspace and read the task card.

On a host that doesn't offer `queue_card`, install the matching host build to enable automatic pickup. The MCP executable alone cannot run the queue.

## Send work to a particular worker

For a direct handoff, ask for a specific canvas agent by name. The assistant uses `inspect_workspace` to find it, `delegate_task` to send the work, and `get_task_result` to read the reply. Keep the dispatch receipt or put the task on a card. To switch a queued card to a direct handoff, withdraw its pending pickup first. If a worker is unavailable, use `edit_card` to change its assignee before delegating to another worker. If two desktops dispatch to the same worker simultaneously, Templum can amend the first task. The connector reports `changed_existing_task` in that case. Check the worker before sending more work or marking either item complete.

A desktop can read a worker reply using the block, task ID and event cursor from the dispatch receipt. For a handoff that should survive a host restart, put the item on a native task card, connect its board to the worker on the canvas, and include the card when dispatching. Both desktops can then see the receipt and the worker's recorded outcome on the card. Without a card, the host retains at most 256 notices for up to an hour. Enable system notifications in Templum to see worker alerts away from the canvas; MCP clients can request the result when you return.

The connector defaults to `http://127.0.0.1:7411` and `~/.config/templum-desktop-mcp/token`. For an SSH-forwarded port, pass e.g. `--url http://127.0.0.1:17411` in the desktop client's server arguments. To use another private credential file, pass `--token-file /absolute/path/to/token`. Keep the credential itself out of chat and client configuration.

To run the connector's tests, use `uv run --locked pytest` in this directory.

See [Templum API tokens, sharing and events](../../docs/api.md) for permissions, remote access and event retention.
