# ones-mcp-cli

Minimal CLI for ONES MCP related operations.

## Requirements

The CLI checks these prerequisites on startup for every invocation except `config`, `daemon status`, and `daemon exit`:

- `node` must be installed
- `npx` must be installed
- `node --version` must report version 18 or above
- the config file must contain `url` when starting without a command or before `daemon run`

Run the CLI without any arguments to show help:

```bash
cargo run --
```

Show the same help explicitly:

```bash
cargo run -- -h
```

When no command is provided, the CLI still completes the startup checks first and then prints help. When the current URL's cached `tools.json` is available, the root help appends every cached MCP tool to the existing `Commands:` section, using the tool name and a single-line `description` preview capped at 100 characters. Longer descriptions end with `...`.

If a check fails, the CLI prints a clear error and exits with a non-zero status.

If `url` is missing, the CLI prints a setup hint such as:

```text
fatal error: missing `url` in config file ~/.config/ones-mcp-cli/config.toml. Configure it with: mcp-cli config --url <URL>
```

## Usage

Save the service URL:

```bash
cargo run -- config --url https://example.com
```

Show the current configuration:

```bash
cargo run -- config show
```

Call a cached MCP tool by command name and pass tool arguments with `--<parameter> <value>`:

```bash
cargo run -- who_am_i
cargo run -- get_issue_details --issueID ISSUE-123
cargo run -- add_project_members --projectID 123 --members u1 --members u2
```

Show parameter help for a cached MCP tool:

```bash
cargo run -- get_issue_details --help
```

Start the hidden daemon in the background:

```bash
cargo run -- daemon run
```

Start the hidden daemon in the foreground:

```bash
cargo run -- daemon run --foreground
```

Query daemon status:

```bash
cargo run -- daemon status
```

Exit the daemon:

```bash
cargo run -- daemon exit
```

Start the daemon on a custom Unix socket path:

```bash
cargo run -- daemon --socket /tmp/ones-mcp-cli.sock run
```

Save the service URL to a custom config file:

```bash
cargo run -- --config /tmp/ones-mcp-cli/config.toml config --url https://example.com
```

The URL must start with `http://` or `https://`.

By default the command writes the configuration to:

```text
~/.config/ones-mcp-cli/config.toml
```

Use the global `--config` option to override that path.

## Daemon

The `daemon` subcommand is intentionally hidden from the generated CLI help, but it is supported and documented here.

`daemon run` detaches from the current shell, checks whether the daemon is already running, and returns after a compatible daemon is running and the MCP tool cache is ready.

If no daemon is running, `daemon run` starts one in the background.

If a daemon is already running and both its version and host match the current CLI configuration, `daemon run` reuses it.

If a daemon is already running but its version or host does not match the current CLI configuration, `daemon run` stops it and starts a new daemon from the current CLI binary.

`daemon run --foreground` keeps the daemon attached to the current process.

The daemon listens on a Unix socket and starts exactly one `npx -y mcp-remote <url>` child process for its lifetime. It keeps ownership of that stdio session so it can initialize MCP itself, refresh the cached tool list, and proxy the local client over the same upstream connection.

The daemon also opens a control socket next to the public socket with the `.ctl` suffix. `daemon status` and `daemon exit` use that control socket.

The default socket path is:

- `$XDG_RUNTIME_DIR/ones-mcp-cli/daemon.sock` when `XDG_RUNTIME_DIR` is set
- `~/.cache/ones-mcp-cli/daemon.sock` otherwise

Use `--socket` to override the socket path.

After startup, the daemon uses the same upstream stdio MCP session to complete the MCP initialize handshake, list all tools, and write the result to `tool-cache/<host>/tools.json` under the same directory as the daemon socket.

If that MCP tool cache file for the current host is missing when `daemon run` checks startup state, the command waits until the daemon generates it before returning.

If multiple downstream commands arrive at the same time, the daemon queues them and sends them to the upstream stdio session one at a time. The next command is only sent after the previous request has received its response.

The daemon refreshes that tool cache every 30 minutes. If the refreshed tool list is unchanged, the cache file is left untouched. If the tool list changes, the cache file is rewritten with the new content.
