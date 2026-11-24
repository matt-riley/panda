# Workspace trust, resource packages and MCP

## Trust a checkout

Run `panda --trust status` from anywhere in the repository. `--trust allow` and `--trust deny` remember the decision. `--trust revoke` removes it. Panda keeps these decisions in the private state directory and identifies the checkout by its canonical path, device and directory inode. A replacement directory needs a new grant.

Interactive startup offers a trust choice when a checkout contains `.panda/mcp.json`. Headless startup uses the saved decision. Use `--trust-project` to explicitly trust the selected project for one invocation. Project MCP configuration, installed project packages, prompt templates and project system prompts use that decision. Running a package management command does not start a project server.

## Resource packages

A package can declare Markdown in the `pi.prompts` and `pi.skills` arrays in its `package.json`, using directories, paths, or globs with `!` exclusions. Without a `pi` manifest Panda looks under `prompts/` and `skills/`. A skill uses a `SKILL.md` file and its normal frontmatter.

```sh
panda --package install ./team-tools --project --only 'prompts/review,skills/test-*'
panda --package install npm:@example/tools@1.0.0
panda --package install git:github.com/example/panda-tools#v1
panda --package list
panda --package activate team-tools --project --only skills
panda --package update team-tools --project
panda --package remove team-tools --project
```

Use `--project` when changing packages for a single checkout. Personal installs are available across workspaces. The package name comes from the manifest. Changes take effect at the next launch. Installation and update snapshot the selected package's declared Markdown in private SQLite storage. Activation changes which resources are visible and accepts `all`, `prompts`, `skills`, or comma-separated category/name globs. Package JavaScript and npm lifecycle hooks are disabled. Git and npm fetches happen during explicit package management. Local skill identifiers remain stable across launches.

## MCP servers

Put personal server configuration in `$PANDA_HOME/mcp.json` (default `~/.panda/mcp.json`) and local stdio commands for a trusted checkout in `.panda/mcp.json`. Remote HTTP servers belong in personal configuration, including loopback servers.

A personal configuration with both transports:

```json
{
  "mcpServers": {
    "catalog": {"url": "http://127.0.0.1:8080/mcp"},
    "docs": {
      "command": "/usr/bin/env",
      "args": ["docs-server", "--stdio"],
      "readOnlyTools": ["lookup"]
    }
  }
}
```

Panda negotiates MCP revision `2025-06-18` at startup. Stdio servers exchange newline-delimited JSON-RPC. Streamable HTTP accepts JSON and SSE responses and carries the negotiated session ID until shutdown. Project stdio programs run in the native read-only workspace sandbox. Each server gets its own temporary directory for both `HOME` and `TMPDIR`, separate from the agent shell and other servers. Panda removes it at shutdown. Project configuration cannot override these two variables. Install server runtimes in an allowed toolchain directory if needed. Personal stdio servers receive a minimal process environment plus values explicitly supplied in the server’s `env` object.

A server's `lookup` tool appears as `mcp_docs_lookup`. Declaring it in `readOnlyTools` exposes it in plan mode and in the `mcp.docs` Codemode namespace. Other server tools require code mode. Resource-capable servers expose `mcp_catalog_resources_list`, `mcp_catalog_resources_templates` and `mcp_catalog_resources_read`. List calls accept an optional cursor. Reads take a URI. Tool and resource results preserve their MCP JSON structure, including content blocks and structured data. Existing tool allowlists, hidden/deferred exposure and cancellation apply to these calls.

### Manage servers

Use `panda --mcp list` to list personal servers and servers from the trusted project. `panda --mcp status NAME` reports the configuration scope, transport and saved login state. `panda --mcp diagnostics NAME` opens a separate connection, performs the handshake and reports how many tools it found. In the terminal, the same commands start with `:mcp`. Use `:mcp reconnect NAME` to reload one server and replace its live tool declarations.

Panda automatically reinitializes an HTTP session once when a read loses its connection or the server reports that the session expired. An interrupted mutation returns an error because its outcome can be unknown.

### Authenticate an HTTP server

Register Panda as a public OAuth client with the server's authorization service. Configure a loopback redirect at `http://127.0.0.1:{port}/callback` with a dynamic port. Add the registered client ID to your personal configuration. For example:

```json
{
  "mcpServers": {
    "catalog": {
      "url": "https://catalog.example/mcp",
      "oauth": {"clientId": "panda-desktop", "scope": "catalog.read"},
      "exposure": "deferred",
      "toolExposure": {"resources_list": "direct"}
    }
  }
}
```

Run `panda --mcp login catalog` and open the printed URL. Panda discovers the issuer from the server's Bearer challenge, uses an authorization code with PKCE, and saves the resource-bound access token under the private Panda state directory. The scope is optional. HTTP OAuth endpoints require HTTPS except during local loopback use. Sign in again when the token expires or is rejected. A signed-out server contributes no tools at startup.

Use `:mcp login catalog` to sign in and load the server in the active terminal. `panda --mcp logout catalog` removes the locally saved grant. Other running Panda processes check that grant before their next request.

### Change tool exposure

`exposure` sets the server default. `toolExposure` overrides it for individual remote tool names or the resource helpers `resources_list`, `resources_templates` and `resources_read`. Choose `direct`, `deferred` or `hidden`. Deferred tools become callable after discovery on the active branch.

```sh
panda --mcp exposure catalog deferred
panda --mcp exposure catalog resources_list direct
panda --mcp exposure catalog resources_list reset
```

These commands save personal preferences for the configured server. Project-server preferences belong to that trusted checkout. A per-tool setting takes precedence over the server default. Use `reset` to restore the corresponding configuration setting. Terminal `:mcp exposure` changes take effect immediately in that session. Other sessions pick them up on reconnect or restart. Tool selections, plan mode and explicit CLI hide/defer flags still apply.
