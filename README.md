# ones-mcp-cli

Minimal CLI for ONES MCP related operations.

## Install

Install the latest release for the current platform:

```bash
curl -fsSL https://raw.githubusercontent.com/tiejunhu/ones-mcp-cli/master/install.sh | bash
```

The installer resolves the latest version through the GitHub Releases redirect path instead of the GitHub REST API, which avoids unauthenticated `api.github.com` rate limits.

By default the installer writes `omc` to `/opt/homebrew/bin` when that directory exists and is writable, then falls back to `/usr/local/bin`, and finally to `~/.local/bin`.

If the final install directory is not already in `PATH`, the installer prints a reminder to add it before running `omc` directly.

Install to a custom location instead:

```bash
curl -fsSL https://raw.githubusercontent.com/tiejunhu/ones-mcp-cli/master/install.sh | INSTALL_DIR=/tmp/omc/bin bash
```

Install a specific released version instead of the latest one:

```bash
curl -fsSL https://raw.githubusercontent.com/tiejunhu/ones-mcp-cli/master/install.sh | VERSION=v0.0.2 bash
```

## Requirements

The CLI checks these prerequisites on startup for every invocation except `config`, `daemon status`, and `daemon exit`:

- `node` must be installed
- `npx` must be installed
- `node --version` must report version 18 or above
- either `--url <URL>` or a configured `url` must be available when starting without a command, before `daemon run`, or before calling a tool

Run the CLI without any arguments to show help:

```bash
omc
```

Show the same help explicitly:

```bash
omc -h
```

When no command is provided, the CLI still completes the startup checks first and then prints help. When the current URL's cached `tools.json` is available, the root help appends cached MCP tools to the existing `Commands:` section, excluding `search` and `fetch`, and using each tool name with a single-line `description` preview capped at 100 characters. Longer descriptions end with `...`.

If a check fails, the CLI prints a clear error and exits with a non-zero status.

If `url` is missing, the CLI prints a setup hint such as:

```text
fatal error: missing `url` in config file ~/.config/ones-mcp-cli/config.toml. Configure it with: omc config --url <URL>
```

## Usage

Save the service URL:

```bash
omc config --url https://example.com
```

Temporarily override the configured URL for a single invocation:

```bash
omc --url https://example.com daemon run
omc --url https://example.com who_am_i
```

Show the current configuration:

```bash
omc config show
```

Refresh the cached tool list for the current URL and wait for the check to finish:

```bash
omc reload
omc --url https://example.com reload
```

Call a cached MCP tool by command name. Pass tool arguments with `--<parameter> <value>` only when the tool accepts parameters:

```bash
omc who_am_i
omc get_issue_details --issueID ISSUE-123
omc add_project_members --projectID 123 --members u1 --members u2
```

Show parameter help for a cached MCP tool:

```bash
omc get_issue_details --help
omc help get_issue_details
```

Required parameters are listed first in tool help output. Remaining parameters follow the order defined in the tool input schema.

For object-style parameters, `omc <tool> --help` includes a synthesized object shape summary, required keys, and nested key notes so the expected JSON is still readable even when a consumer does not fully interpret JSON Schema.

Run the daemon in the foreground:

```bash
omc daemon run
```

Query daemon status:

```bash
omc daemon status
```

Exit the daemon:

```bash
omc daemon exit
```

Start the daemon on a custom Unix socket path:

```bash
omc daemon --socket /tmp/ones-mcp-cli.sock run
```

Save the service URL to a custom config file:

```bash
omc --config /tmp/ones-mcp-cli/config.toml config --url https://example.com
```

The URL must start with `http://` or `https://`.

By default the command writes the configuration to:

```text
~/.config/ones-mcp-cli/config.toml
```

Use the global `--config` option to override that path.

Use the top-level `--url` option to override the configured URL without writing it back to the config file.

## Release

Create the next patch release, commit the version bump, create a `v*` tag, and push that tag:

```bash
./publish.sh
```

Create a specific release version instead:

```bash
./publish.sh 0.1.1
```

The script updates the project version, verifies the project, commits the release version, creates a tag like `v0.1.1`, and pushes that tag to `origin`.

Pushing a `v*` tag triggers the GitHub Actions release workflow, which builds release archives for Linux and macOS on both x86_64 and arm64, then uploads those archives to a GitHub Release.

This repository does not publish a Homebrew formula.

The repository includes `install.sh`, which resolves the latest compatible release through GitHub Releases redirects, downloads the matching archive, and installs the `omc` binary.

## Daemon

The `daemon` subcommand is intentionally hidden from the generated CLI help, but it is supported and documented here.

`daemon run` starts the daemon in the foreground and keeps it attached to the current process until it is stopped.

Different hosts use different default Unix sockets, so multiple daemons for different hosts can run at the same time. The client picks the matching daemon from the current `--url` value, or from the configured `url` when `--url` is not provided.

Normal `omc <tool>` commands still auto-start a compatible background daemon when needed. That background startup path checks whether the daemon for the current host is already running, reuses it when compatible, restarts it when the version is incompatible, and waits until the MCP tool cache is ready before returning.

The daemon listens on a Unix socket and starts exactly one `npx -y mcp-remote <url>` child process for its lifetime. Before spawning `mcp-remote`, the CLI normalizes the configured URL: `https://ones.cn/...` is rewritten to `https://sz.ones.cn/...`, `https://ones.com/...` is rewritten to `https://us.ones.com/...`, and the final upstream URL always ends with `/mcp`. It keeps ownership of that stdio session so it can initialize MCP itself, refresh the cached tool list, and proxy the local client over the same upstream connection.

`omc reload` performs an immediate synchronous `tools/list` fetch for the current URL. It updates the local cache file if the tool list changed, and exits only after that refresh check completes.

When a regular `omc <tool>` command auto-starts the background daemon, it passes the current `--url` override through to the detached process.

The daemon also opens a control socket next to the public socket with the `.ctl` suffix. `daemon status` and `daemon exit` use that control socket.

The default socket path is:

- `$XDG_RUNTIME_DIR/ones-mcp-cli/daemon-<host>.sock` when `XDG_RUNTIME_DIR` is set
- `~/.cache/ones-mcp-cli/daemon-<host>.sock` otherwise

Use `--socket` to override the socket path.

After startup, the daemon uses the same upstream stdio MCP session to complete the MCP initialize handshake, list all tools, and write the result to `tool-cache/<host>/tools.json` under the same directory as the daemon socket.

If that MCP tool cache file for the current host is missing when a regular `omc <tool>` command checks startup state, the command waits until the daemon generates it before returning.

If multiple downstream commands arrive at the same time, the daemon keeps all of those local MCP sessions open, forwards their requests over the shared upstream stdio session concurrently, rewrites request IDs to avoid collisions, and routes each response back to the originating client by request ID.

The daemon refreshes that tool cache every 30 minutes. If the refreshed tool list is unchanged, the cache file is left untouched. If the tool list changes, the cache file is rewritten with the new content.
