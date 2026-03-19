use std::path::{Path, PathBuf};

use clap::Parser;

use crate::Cli;

use super::{
    command_version, format_clap_error, missing_url_error, parse_node_major_version, parse_url,
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
fn prints_help_only_when_no_arguments_are_provided() {
    assert!(should_print_help(1));
    assert!(!should_print_help(2));
}
