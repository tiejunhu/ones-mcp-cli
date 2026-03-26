use std::ffi::OsString;
use std::path::{Path, PathBuf};

use clap::{CommandFactory, Parser};

use crate::Cli;

use super::{
    CLI_ABOUT, command_requires_config_url, command_requires_daemon_ready,
    command_requires_runtime_checks, command_socket_override, command_version,
    find_commands_section_bounds, format_clap_error, format_commands_section, missing_url_error,
    parse_command_line, parse_node_major_version, parse_url, render_root_help_with_tools,
    resolve_config_path, rewrite_help_command_for_tool, should_print_help,
    should_render_root_help_for_args, truncate_tool_description,
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
fn parses_top_level_url_override() {
    let cli = Cli::try_parse_from(["omc", "--url", "https://example.com", "daemon", "run"])
        .expect("expected top-level url override");

    assert_eq!(cli.url.as_deref(), Some("https://example.com"));
    assert!(matches!(cli.command, Some(crate::Commands::Daemon(_))));
}

#[test]
fn preserves_tool_url_arguments_after_tool_name() {
    let cli = Cli::try_parse_from([
        "omc",
        "--url",
        "https://example.com",
        "sample_tool",
        "--url",
        "https://payload.example.com",
    ])
    .expect("expected tool url argument");

    match cli.command {
        Some(crate::Commands::Tool(args)) => {
            assert_eq!(args.len(), 3);
            assert_eq!(args[0].to_string_lossy(), "sample_tool");
            assert_eq!(args[1].to_string_lossy(), "--url");
            assert_eq!(args[2].to_string_lossy(), "https://payload.example.com");
        }
        other => panic!("expected external tool command, got {other:?}"),
    }
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
        "missing `url` in config file /tmp/config.toml. Configure it with: omc config --url <URL>"
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
        "missing `url` in config file /tmp/custom-config.toml. Configure it with: omc --config /tmp/custom-config.toml config --url <URL>"
    );
}

#[test]
fn strips_clap_error_prefix() {
    let error = Cli::try_parse_from(["omc", "config", "--url", "ftp://example.com"])
        .expect_err("expected clap parse error");
    assert_eq!(
        format_clap_error(&error),
        "invalid value 'ftp://example.com' for '--url <URL>': url must start with http:// or https://\n\nFor more information, try '--help'."
    );
}

#[test]
fn parses_config_show_subcommand() {
    let cli =
        Cli::try_parse_from(["omc", "config", "show"]).expect("expected config show to parse");
    assert!(matches!(cli.command, Some(crate::Commands::Config(_))));
}

#[test]
fn parses_hidden_daemon_run_subcommand() {
    let cli = Cli::try_parse_from(["omc", "daemon", "run"]).expect("expected daemon to parse");
    assert!(matches!(cli.command, Some(crate::Commands::Daemon(_))));
}

#[test]
fn root_help_appends_current_version_to_about() {
    let help = Cli::command().render_help().to_string();

    assert!(help.contains(CLI_ABOUT));
}

#[test]
fn daemon_socket_override_is_available_for_run() {
    let cli = Cli::try_parse_from(["omc", "daemon", "--socket", "/tmp/ones-mcp-cli.sock", "run"])
        .expect("expected daemon run with socket");

    assert_eq!(
        command_socket_override(cli.command.as_ref()),
        Some(Path::new("/tmp/ones-mcp-cli.sock"))
    );
}

#[test]
fn daemon_run_accepts_top_level_url_override() {
    let cli = Cli::try_parse_from(["omc", "--url", "https://example.com", "daemon", "run"])
        .expect("expected daemon run with top-level url override");

    assert_eq!(cli.url.as_deref(), Some("https://example.com"));
}

#[test]
fn daemon_run_rejects_foreground_flag() {
    let error = Cli::try_parse_from(["omc", "daemon", "run", "--foreground"])
        .expect_err("expected daemon run foreground flag to be rejected");

    assert!(format_clap_error(&error).contains("unexpected argument '--foreground'"));
}

#[test]
fn daemon_commands_do_not_require_program_startup_daemon_check() {
    let cli = Cli::try_parse_from(["omc", "daemon", "run"]).expect("expected daemon run");
    assert!(!command_requires_daemon_ready(cli.command.as_ref()));
    assert!(!command_requires_runtime_checks(cli.command.as_ref()));
    assert!(command_requires_config_url(cli.command.as_ref()));
}

#[test]
fn config_commands_do_not_require_program_startup_daemon_check() {
    let cli = Cli::try_parse_from(["omc", "config", "show"]).expect("expected config show");
    assert!(!command_requires_daemon_ready(cli.command.as_ref()));
    assert!(!command_requires_runtime_checks(cli.command.as_ref()));
    assert!(!command_requires_config_url(cli.command.as_ref()));
}

#[test]
fn no_command_requires_program_startup_daemon_check() {
    let cli = Cli::try_parse_from(["omc"]).expect("expected empty invocation to parse");
    assert!(command_requires_daemon_ready(cli.command.as_ref()));
}

#[test]
fn no_command_requires_runtime_checks() {
    let cli = Cli::try_parse_from(["omc"]).expect("expected empty invocation to parse");
    assert!(command_requires_runtime_checks(cli.command.as_ref()));
}

#[test]
fn no_command_requires_config_url() {
    let cli = Cli::try_parse_from(["omc"]).expect("expected empty invocation to parse");
    assert!(command_requires_config_url(cli.command.as_ref()));
}

#[test]
fn daemon_status_does_not_require_config_url() {
    let cli = Cli::try_parse_from(["omc", "daemon", "status"]).expect("expected daemon status");
    assert!(!command_requires_config_url(cli.command.as_ref()));
}

#[test]
fn reload_requires_runtime_checks_and_config_url_but_not_daemon_ready() {
    let cli = Cli::try_parse_from(["omc", "reload"]).expect("expected reload command");

    assert!(command_requires_runtime_checks(cli.command.as_ref()));
    assert!(command_requires_config_url(cli.command.as_ref()));
    assert!(!command_requires_daemon_ready(cli.command.as_ref()));
}

#[test]
fn tool_commands_require_runtime_checks_and_daemon() {
    let cli = Cli::try_parse_from(["omc", "who_am_i"]).expect("expected tool command");
    assert!(command_requires_runtime_checks(cli.command.as_ref()));
    assert!(command_requires_config_url(cli.command.as_ref()));
    assert!(command_requires_daemon_ready(cli.command.as_ref()));
}

#[test]
fn parses_hidden_daemon_status_subcommand() {
    let cli = Cli::try_parse_from(["omc", "daemon", "status"]).expect("expected daemon status");
    assert!(matches!(cli.command, Some(crate::Commands::Daemon(_))));
}

#[test]
fn parses_hidden_daemon_exit_subcommand() {
    let cli = Cli::try_parse_from(["omc", "daemon", "exit"]).expect("expected daemon exit");
    assert!(matches!(cli.command, Some(crate::Commands::Daemon(_))));
}

#[test]
fn parses_reload_subcommand() {
    let cli = Cli::try_parse_from(["omc", "reload"]).expect("expected reload");
    assert!(matches!(cli.command, Some(crate::Commands::Reload)));
}

#[test]
fn parses_external_tool_subcommand() {
    let cli = Cli::try_parse_from(["omc", "who_am_i", "--includeDetails", "true"])
        .expect("expected external tool command");

    match cli.command {
        Some(crate::Commands::Tool(args)) => {
            assert_eq!(args.len(), 3);
            assert_eq!(args[0].to_string_lossy(), "who_am_i");
            assert_eq!(args[1].to_string_lossy(), "--includeDetails");
            assert_eq!(args[2].to_string_lossy(), "true");
        }
        other => panic!("expected external tool command, got {other:?}"),
    }
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

#[test]
fn rewrites_help_subcommand_for_tool_help() {
    let rewritten = rewrite_help_command_for_tool(&[
        OsString::from("omc"),
        OsString::from("help"),
        OsString::from("sample_tool"),
    ]);

    assert_eq!(
        rewritten,
        vec![
            OsString::from("omc"),
            OsString::from("sample_tool"),
            OsString::from("--help"),
        ]
    );

    let cli = Cli::try_parse_from(rewritten).expect("expected rewritten tool help to parse");
    match cli.command {
        Some(crate::Commands::Tool(args)) => {
            assert_eq!(args.len(), 2);
            assert_eq!(args[0].to_string_lossy(), "sample_tool");
            assert_eq!(args[1].to_string_lossy(), "--help");
        }
        other => panic!("expected external tool command, got {other:?}"),
    }
}

#[test]
fn rewrites_help_subcommand_for_tool_help_after_global_options() {
    let rewritten = rewrite_help_command_for_tool(&[
        OsString::from("omc"),
        OsString::from("--config"),
        OsString::from("/tmp/config.toml"),
        OsString::from("--url=https://example.com"),
        OsString::from("help"),
        OsString::from("sample_tool"),
    ]);

    assert_eq!(
        rewritten,
        vec![
            OsString::from("omc"),
            OsString::from("--config"),
            OsString::from("/tmp/config.toml"),
            OsString::from("--url=https://example.com"),
            OsString::from("sample_tool"),
            OsString::from("--help"),
        ]
    );
}

#[test]
fn does_not_rewrite_help_subcommand_for_builtin_commands() {
    assert_eq!(
        rewrite_help_command_for_tool(&[
            OsString::from("omc"),
            OsString::from("help"),
            OsString::from("config"),
        ]),
        vec![
            OsString::from("omc"),
            OsString::from("help"),
            OsString::from("config"),
        ]
    );
    assert_eq!(
        rewrite_help_command_for_tool(&[
            OsString::from("omc"),
            OsString::from("help"),
            OsString::from("daemon"),
        ]),
        vec![
            OsString::from("omc"),
            OsString::from("help"),
            OsString::from("daemon"),
        ]
    );
    assert_eq!(
        rewrite_help_command_for_tool(&[
            OsString::from("omc"),
            OsString::from("help"),
            OsString::from("reload"),
        ]),
        vec![
            OsString::from("omc"),
            OsString::from("help"),
            OsString::from("reload"),
        ]
    );
}

#[test]
fn root_help_detection_accepts_help_flags_and_global_config() {
    assert!(should_render_root_help_for_args(["-h"]));
    assert!(should_render_root_help_for_args(["--help"]));
    assert!(should_render_root_help_for_args([
        "--config",
        "/tmp/config.toml",
        "-h",
    ]));
    assert!(should_render_root_help_for_args([
        "--config=/tmp/config.toml",
        "--help",
    ]));
    assert!(should_render_root_help_for_args([
        "--url",
        "https://example.com",
        "-h",
    ]));
    assert!(should_render_root_help_for_args([
        "--url=https://example.com",
        "--help",
    ]));
}

#[test]
fn root_help_detection_rejects_subcommand_help() {
    assert!(!should_render_root_help_for_args(["config", "-h"]));
    assert!(!should_render_root_help_for_args(["daemon", "-h"]));
}

#[test]
fn formats_commands_section_with_aligned_descriptions() {
    let section = format_commands_section(
        &[super::HelpCommandSummary {
            name: "config".to_owned(),
            description: Some("Manage CLI configuration".to_owned()),
        }],
        &[
            crate::daemon::CachedToolSummary {
                name: "alpha".to_owned(),
                description: Some("Alpha tool".to_owned()),
            },
            crate::daemon::CachedToolSummary {
                name: "beta".to_owned(),
                description: Some("Beta tool".to_owned()),
            },
        ],
    );

    assert!(section.contains("  config  Manage CLI configuration"));
    assert!(section.contains("  alpha   Alpha tool"));
    assert!(section.contains("  beta    Beta tool"));
}

#[test]
fn truncates_tool_descriptions_to_single_line_and_100_chars() {
    let description = "line one\nline two\tline three ".repeat(10);
    let truncated = truncate_tool_description(&description, 100);

    assert!(!truncated.contains('\n'));
    assert!(!truncated.contains('\t'));
    assert_eq!(truncated.chars().count(), 103);
    assert!(truncated.ends_with("..."));
}

#[test]
fn finds_commands_section_bounds() {
    let help = "Usage: test\n\nCommands:\n  config  Config command\n\nOptions:\n  -h, --help\n";
    let (section_start, section_end) =
        find_commands_section_bounds(help).expect("expected commands section");

    assert_eq!(
        &help[section_start..section_end],
        "  config  Config command"
    );
}

#[test]
fn parses_command_lines_from_clap_help() {
    let command =
        parse_command_line("  help    Print this message or the help of the given subcommand(s)")
            .expect("expected command line");

    assert_eq!(command.name, "help");
    assert_eq!(
        command.description.as_deref(),
        Some("Print this message or the help of the given subcommand(s)")
    );
}

#[test]
fn root_help_includes_cached_tools_when_available() {
    let temp_dir = std::env::temp_dir().join(format!("ones-mcp-cli-help-{}", std::process::id()));
    std::fs::create_dir_all(&temp_dir).expect("expected temp dir");
    let socket_path = temp_dir.join("daemon.sock");
    let cache_dir = temp_dir.join("tool-cache").join("example.com");
    std::fs::create_dir_all(&cache_dir).expect("expected tool cache dir");
    std::fs::write(
        cache_dir.join("tools.json"),
        serde_json::json!({
            "url": "https://example.com",
            "tools": [
                { "name": "alpha", "description": "Alpha tool" },
                { "name": "beta", "description": "Beta tool" },
                { "name": "fetch", "description": "Fetch tool" },
                { "name": "search", "description": "Search tool" }
            ]
        })
        .to_string(),
    )
    .expect("expected tool cache");

    let help = render_root_help_with_tools(Some(&socket_path), Some("https://example.com"));

    assert_eq!(help.matches("\nCommands:\n").count(), 1);
    assert!(help.contains("alpha"));
    assert!(help.contains("Alpha tool"));
    assert!(!help.contains("fetch"));
    assert!(!help.contains("Fetch tool"));
    assert!(!help.contains("search"));
    assert!(!help.contains("Search tool"));
    assert!(help.contains("config"));
    assert!(help.contains("  config  "));
    assert!(help.contains("  help  "));
    assert!(help.find("config").unwrap() < help.find("alpha").unwrap());
    assert!(help.find("help").unwrap() < help.find("alpha").unwrap());
    assert!(help.contains("given subcommand(s)\n  alpha"));

    std::fs::remove_dir_all(&temp_dir).expect("expected temp dir cleanup");
}
