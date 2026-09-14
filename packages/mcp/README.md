# @zeroship/mcp

`@zeroship/mcp` is a stdio MCP server that exposes the zeroship control plane as agent-native tools. Agents can list, create, inspect, deploy, tail logs for, archive, and restore zeroship apps without shelling out to `zeroship deploy`.

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

Tools that select an existing app require one explicit target shape:

```json
{ "target": { "kind": "id", "appId": "app_0000000002e4nenowz3qmamtd" } }
```

```json
{ "target": { "kind": "name", "appName": "my-app" } }
```

An ID target accepts only a canonical `app_...` AppId. A name target performs
the name lookup; `deploy_app` creates that name when it does not exist.

- `list_apps` - list apps visible to the token.
- `get_app` - get an app by its explicit target.
- `create_app` - create an app.
- `deploy_app` - deploy a local `.zship`, creating a missing named app first.
- `app_logs` - read recent worker logs for an app.
- `archive_app` - stop serving an app while preserving its history and data.
- `restore_app` - restore an archived app by its explicit target.
