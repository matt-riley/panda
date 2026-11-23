# Workspace trust, resource packages and MCP

## Trust a checkout

Run `panda --trust status` from anywhere in the repository. `--trust allow` and `--trust deny` remember the decision. `--trust revoke` removes it. Panda keeps these decisions in the private state directory and identifies the checkout by its canonical path, device and directory inode. A replacement directory needs a new grant.

Interactive startup offers a trust choice when a checkout contains `.panda/mcp.json`. Headless startup uses the saved decision. The existing `--trust-project` flag explicitly trusts the selected project for one invocation. Project MCP configuration, installed project packages, prompt templates and project system prompts use that decision. Running a package management command does not start a project server.

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

Use `--project` on mutations for a checkout-scoped install. Personal installs are available across workspaces. The package name comes from the manifest. Changes take effect at the next launch. Installation and update snapshot the selected package's declared Markdown in private SQLite storage. Activation changes which resources are visible and accepts `all`, `prompts`, `skills`, or comma-separated category/name globs. Package JavaScript and npm lifecycle hooks are disabled. Git and npm fetches happen during explicit package management. Local skill identifiers remain stable across launches.

## MCP servers

Put personal server configuration in `$PANDA_HOME/mcp.json` (default `~/.panda/mcp.json`) and local stdio commands for a trusted checkout in `.panda/mcp.json`. Remote HTTP servers belong in personal configuration, including loopback servers.

For example, a personal configuration can contain both transports.

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

Panda negotiates MCP revision `2025-06-18` at startup. Stdio servers exchange newline-delimited JSON-RPC. Streamable HTTP accepts JSON and SSE responses and carries the negotiated session ID until shutdown. Project stdio programs run in the native read-only workspace sandbox. Each server gets its own temporary directory for both `HOME` and `TMPDIR`, separate from the agent shell and other servers. Panda removes it at shutdown. Project configuration cannot override these two variables. Install their runtimes in an allowed toolchain directory if needed. Personal stdio servers receive a minimal process environment plus values explicitly supplied in the server’s `env` object.

A server's `lookup` tool appears as `mcp_docs_lookup`. Declaring it in `readOnlyTools` exposes it in plan mode and in the `mcp.docs` Codemode namespace. Other server tools require code mode. Resource-capable servers expose `mcp_catalog_resources_list`, `mcp_catalog_resources_templates` and `mcp_catalog_resources_read`. List calls accept an optional cursor. Reads take a URI. Tool and resource results preserve their MCP JSON structure, including content blocks and structured data. Existing tool allowlists, hidden/deferred exposure and cancellation apply to these calls.
