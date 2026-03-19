# ones-mcp-cli

Minimal CLI for ONES MCP related operations.

## Requirements

The CLI checks these prerequisites on startup:

- `node` must be installed
- `npx` must be installed
- `node --version` must report version 18 or above
- the config file must contain `url`

Run the CLI without any arguments to show help:

```bash
cargo run --
```

When a command is provided, the CLI checks these prerequisites before executing it.

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
