use std::path::{Path, PathBuf};

use clap::{CommandFactory, Parser};

use crate::Cli;

use super::{
    command_requires_config_url, command_requires_daemon_ready,
    command_requires_runtime_checks, command_socket_override, command_version,
    format_clap_error, missing_url_error, parse_node_major_version, parse_url,
    resolve_config_path, should_print_help,
};

#[test]
fn accepts_http_url() {
    assert_eq!(
        parse_url("http://example.com").expect("expected valid http url"),
        "http://example.com"
    );
}

#[test]
fn accepts_https_url() {
    assert_eq!(
        parse_url("https://example.com").expect("expected valid https url"),
        "https://example.com"
    );
}

#[test]
fn rejects_other_schemes() {
    let error = parse_url("ftp://example.com").expect_err("expected invalid url");
    assert_eq!(error, "url must start with http:// or https://");
}

#[test]
fn parses_node_major_version() {
    assert_eq!(
        parse_node_major_version("v18.20.8").expect("expected valid node version"),
        18
    );
}

#[test]
fn rejects_node_version_without_v_prefix() {
    let error = parse_node_major_version("18.20.8").expect_err("expected invalid version");
    assert_eq!(error.to_string(), "node version output must start with `v`");
}

#[test]
fn uses_custom_config_path_when_provided() {
    let path = PathBuf::from("/tmp/custom-config.toml");
    assert_eq!(
        resolve_config_path(Some(path.clone())).expect("expected custom config path"),
        path
    );
}

#[test]
fn reads_command_version() {
    let version = command_version("node").expect("expected node version");
    assert!(version.starts_with('v'));
}

#[test]
fn missing_url_error_uses_default_command() {
    let error = missing_url_error(Path::new("/tmp/config.toml"), None);
    assert_eq!(
        error,
        "missing `url` in config file /tmp/config.toml. Configure it with: mcp-cli config --url <URL>"
    );
}

#[test]
fn missing_url_error_includes_config_override() {
    let error = missing_url_error(
        Path::new("/tmp/custom-config.toml"),
        Some(Path::new("/tmp/custom-config.toml")),
    );
    assert_eq!(
        error,
        "missing `url` in config file /tmp/custom-config.toml. Configure it with: mcp-cli --config /tmp/custom-config.toml config --url <URL>"
    );
}

#[test]
fn strips_clap_error_prefix() {
    let error = Cli::try_parse_from(["mcp-cli", "config", "--url", "ftp://example.com"])
        .expect_err("expected clap parse error");
    assert_eq!(
        format_clap_error(&error),
        "invalid value 'ftp://example.com' for '--url <URL>': url must start with http:// or https://\n\nFor more information, try '--help'."
    );
}

#[test]
fn parses_config_show_subcommand() {
    let cli =
        Cli::try_parse_from(["mcp-cli", "config", "show"]).expect("expected config show to parse");
    assert!(matches!(cli.command, Some(crate::Commands::Config(_))));
}

#[test]
fn parses_hidden_daemon_run_subcommand() {
    let cli = Cli::try_parse_from(["mcp-cli", "daemon", "run"]).expect("expected daemon to parse");
    assert!(matches!(cli.command, Some(crate::Commands::Daemon(_))));
}

#[test]
fn parses_hidden_daemon_foreground_subcommand() {
    let cli = Cli::try_parse_from(["mcp-cli", "daemon", "run", "--foreground"])
        .expect("expected daemon foreground to parse");
    assert!(matches!(cli.command, Some(crate::Commands::Daemon(_))));
}

#[test]
fn daemon_socket_override_is_available_for_background_run() {
    let cli = Cli::try_parse_from([
        "mcp-cli",
        "daemon",
        "--socket",
        "/tmp/ones-mcp-cli.sock",
        "run",
    ])
    .expect("expected daemon run with socket");

    assert_eq!(
        command_socket_override(cli.command.as_ref()),
        Some(Path::new("/tmp/ones-mcp-cli.sock"))
    );
}

#[test]
fn daemon_commands_do_not_require_program_startup_daemon_check() {
    let cli = Cli::try_parse_from(["mcp-cli", "daemon", "run"]).expect("expected daemon run");
    assert!(!command_requires_daemon_ready(cli.command.as_ref()));
    assert!(!command_requires_runtime_checks(cli.command.as_ref()));
    assert!(command_requires_config_url(cli.command.as_ref()));
}

#[test]
fn config_commands_do_not_require_program_startup_daemon_check() {
    let cli =
        Cli::try_parse_from(["mcp-cli", "config", "show"]).expect("expected config show");
    assert!(!command_requires_daemon_ready(cli.command.as_ref()));
    assert!(!command_requires_runtime_checks(cli.command.as_ref()));
    assert!(!command_requires_config_url(cli.command.as_ref()));
}

#[test]
fn no_command_requires_program_startup_daemon_check() {
    let cli = Cli::try_parse_from(["mcp-cli"]).expect("expected empty invocation to parse");
    assert!(command_requires_daemon_ready(cli.command.as_ref()));
}

#[test]
fn no_command_requires_runtime_checks() {
    let cli = Cli::try_parse_from(["mcp-cli"]).expect("expected empty invocation to parse");
    assert!(command_requires_runtime_checks(cli.command.as_ref()));
}

#[test]
fn no_command_requires_config_url() {
    let cli = Cli::try_parse_from(["mcp-cli"]).expect("expected empty invocation to parse");
    assert!(command_requires_config_url(cli.command.as_ref()));
}

#[test]
fn daemon_status_does_not_require_config_url() {
    let cli = Cli::try_parse_from(["mcp-cli", "daemon", "status"]).expect("expected daemon status");
    assert!(!command_requires_config_url(cli.command.as_ref()));
}

#[test]
fn parses_hidden_daemon_status_subcommand() {
    let cli = Cli::try_parse_from(["mcp-cli", "daemon", "status"]).expect("expected daemon status");
    assert!(matches!(cli.command, Some(crate::Commands::Daemon(_))));
}

#[test]
fn parses_hidden_daemon_exit_subcommand() {
    let cli = Cli::try_parse_from(["mcp-cli", "daemon", "exit"]).expect("expected daemon exit");
    assert!(matches!(cli.command, Some(crate::Commands::Daemon(_))));
}

#[test]
fn hides_daemon_subcommand_from_help() {
    let help = Cli::command().render_help().to_string();
    assert!(!help.contains("daemon"));
}

#[test]
fn prints_help_only_when_no_arguments_are_provided() {
    assert!(should_print_help(1));
    assert!(!should_print_help(2));
}
