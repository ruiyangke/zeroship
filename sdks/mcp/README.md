# @zeroship/mcp

`@zeroship/mcp` is a stdio MCP server that exposes the zeroship control plane as agent-native tools. Agents can list, create, inspect, deploy, tail logs for, and delete zeroship apps without shelling out to `zeroship deploy`.

The server wraps `@zeroship/control` and reads configuration from environment variables:

- `ZEROSHIP_CONTROL_URL` - control-plane URL. Defaults to `http://localhost:9090`.
- `ZEROSHIP_TOKEN` - bearer PAT. Required for tool calls. Create one with `zeroship login` or provide a PAT directly.

## Register With Claude Code

```bash
claude mcp add zeroship -- zeroship-mcp
```

Make sure the process environment includes `ZEROSHIP_TOKEN`, and set `ZEROSHIP_CONTROL_URL` when the control plane is not running at `http://localhost:9090`.

## `.mcp.json`

```json
{
  "mcpServers": {
    "zeroship": {
      "command": "zeroship-mcp",
      "env": {
        "ZEROSHIP_CONTROL_URL": "http://localhost:9090",
        "ZEROSHIP_TOKEN": "<PAT from zeroship login>"
      }
    }
  }
}
```

## Tools

- `list_apps` - list apps visible to the token.
- `get_app` - get an app by UUID or name.
- `create_app` - create an app.
- `deploy_app` - deploy a local `.zship`, creating a missing named app first.
- `app_logs` - read recent worker logs for an app.
- `delete_app` - delete an app by UUID or name.
