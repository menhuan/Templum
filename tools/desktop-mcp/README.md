# Templum for desktop MCP clients

Discuss a plan in Codex or Claude Desktop and keep the plan in a Templum canvas note. A second client connected to the same workspace can read that note, see the workers and task cards, and continue the discussion. The desktop chat history stays in the client that created it.

## Connect your workspace

1. Start Templum and open the workspace that should hold your plan. Start a worker such as Codex or Claude Code on the canvas if you want to delegate work to it. Check that the CLI prompt has appeared and finish any first-run prompts there. Templum can show a newly opened block as idle while its shell is still starting.
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

## Try one plan item

Ask your desktop assistant to open the workspace, write the agreed decisions and open questions in a plan note, and read it back. Ask the other client to find the same note and continue an open question. Saving an edit uses the revision read with the original text. If someone else edited the note in the meantime, read the newer version before applying your change.

For example, ask in Codex: “Save our three plan items in Templum. Leave item 2 open for discussion. Send item 1 to the idle Claude Code worker on the canvas and tell me what it reports.” In Claude Desktop you can then ask: “Open the same plan and discuss item 2 with me.”

The assistant uses `inspect_workspace` to find the note and worker, `delegate_task` to send the work, and `get_task_result` to read the worker's reply. Keep the dispatch receipt or put the task on a card. If a worker is unavailable, use `edit_card` to change its assignee before delegating to another worker. If two desktops dispatch to the same worker simultaneously, Templum can amend the first task. The connector reports `changed_existing_task` in that case. Check the worker before sending more work or marking either item complete.

A desktop can read a worker reply using the block, task ID and event cursor from the dispatch receipt. For a handoff that should survive a host restart, put the item on a native task card, connect its board to the worker on the canvas, and include the card when dispatching. Both desktops can then see the receipt and the worker's recorded outcome on the card. Without a card, the host retains at most 256 notices for up to an hour. Enable system notifications in Templum to see worker alerts away from the canvas; MCP clients can request the result when you return.

The connector defaults to `http://127.0.0.1:7411` and `~/.config/templum-desktop-mcp/token`. For an SSH-forwarded port, pass e.g. `--url http://127.0.0.1:17411` in the desktop client's server arguments. To use another private credential file, pass `--token-file /absolute/path/to/token`. Keep the credential itself out of chat and client configuration.

To run the connector's tests, use `uv run --locked pytest` in this directory.

See [Templum API tokens, sharing and events](../../docs/api.md) for permissions, remote access and event retention.
