use std::env;
use std::error::Error;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::path::{Path, PathBuf};
use std::process;
use std::process::Command;

use clap::{Args, CommandFactory, Parser, Subcommand, error::ErrorKind};
use serde::{Deserialize, Serialize};

mod daemon;
mod tool;

pub(crate) const CLI_COMMAND_NAME: &str = "omc";
const CLI_ABOUT: &str = concat!(
    "ONES MCP command line interface ",
    env!("CARGO_PKG_VERSION")
);
const ROOT_HELP_HIDDEN_TOOLS: &[&str] = &["search", "fetch"];

#[derive(Parser, Debug)]
#[command(name = CLI_COMMAND_NAME)]
#[command(about = CLI_ABOUT)]
struct Cli {
    /// Path to the config file
    #[arg(long, global = true)]
    config: Option<PathBuf>,

    /// ONES service URL, overrides the configured url for this invocation
    #[arg(long, value_parser = parse_url)]
    url: Option<String>,

    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Manage CLI configuration
    Config(ConfigCommand),
    /// Refresh the cached tool list for the current URL
    Reload,
    #[command(hide = true)]
    Daemon(DaemonCommand),
    #[command(external_subcommand)]
    Tool(Vec<OsString>),
}

#[derive(Args, Debug)]
#[command(args_conflicts_with_subcommands = true, arg_required_else_help = true)]
struct ConfigCommand {
    /// ONES service URL, must start with http:// or https://
    #[arg(long, value_parser = parse_url)]
    url: Option<String>,

    #[command(subcommand)]
    command: Option<ConfigSubcommands>,
}

#[derive(Subcommand, Debug)]
enum ConfigSubcommands {
    /// Show the current configuration
    Show,
}

#[derive(Args, Debug)]
#[command(arg_required_else_help = true)]
struct DaemonCommand {
    /// Path to the Unix socket that exposes the daemon bridge
    #[arg(long)]
    socket: Option<PathBuf>,

    #[command(subcommand)]
    command: DaemonSubcommands,
}

#[derive(Subcommand, Debug)]
enum DaemonSubcommands {
    Run,
    Status,
    Exit,
}

#[derive(Serialize)]
struct Config {
    url: String,
}

#[derive(Deserialize, Serialize)]
struct StoredConfig {
    url: Option<String>,
}

#[tokio::main]
async fn main() {
    let original_args = env::args_os().collect::<Vec<_>>();
    let args = rewrite_help_command_for_tool(&original_args);
    let cli = match Cli::try_parse_from(&args) {
        Ok(cli) => cli,
        Err(error) => match error.kind() {
            ErrorKind::DisplayHelp => {
                if should_render_root_help_for_args(original_args.iter().skip(1)) {
                    let help_cache_url = help_cache_url_for_args(&original_args);
                    print!(
                        "{}",
                        render_root_help_with_tools(None, help_cache_url.as_deref())
                    );
                } else {
                    print!("{error}");
                }
                process::exit(0);
            }
            ErrorKind::DisplayVersion => {
                print!("{error}");
                process::exit(0);
            }
            _ => {
                eprintln!("fatal error: {}", format_clap_error(&error));
                process::exit(1);
            }
        },
    };

    if let Err(error) = run(cli).await {
        eprintln!("fatal error: {error}");
        process::exit(1);
    }
}

fn rewrite_help_command_for_tool(args: &[OsString]) -> Vec<OsString> {
    let Some((help_index, target_index)) = help_tool_rewrite_indices(args) else {
        return args.to_vec();
    };

    let mut rewritten = Vec::with_capacity(args.len());
    rewritten.extend_from_slice(&args[..help_index]);
    rewritten.push(args[target_index].clone());
    rewritten.extend_from_slice(&args[target_index + 1..]);
    rewritten.push(OsString::from("--help"));
    rewritten
}

fn help_tool_rewrite_indices(args: &[OsString]) -> Option<(usize, usize)> {
    let mut index = 1;

    while index < args.len() {
        let arg = args[index].to_string_lossy();

        if arg == "--config" || arg == "--url" {
            if index + 1 >= args.len() {
                return None;
            }
            index += 2;
            continue;
        }

        if arg.starts_with("--config=") || arg.starts_with("--url=") {
            index += 1;
            continue;
        }

        if arg != "help" {
            return None;
        }

        let target_index = index + 1;
        let target = args.get(target_index)?.to_string_lossy();
        if target.starts_with('-') || is_builtin_help_target(&target) {
            return None;
        }

        return Some((index, target_index));
    }

    None
}

fn is_builtin_help_target(target: &str) -> bool {
    matches!(target, "config" | "daemon" | "help" | "reload")
}

fn should_print_help(arg_count: usize) -> bool {
    arg_count == 1
}

fn should_render_root_help_for_args<I, T>(args: I) -> bool
where
    I: IntoIterator<Item = T>,
    T: AsRef<OsStr>,
{
    let mut args = args.into_iter();

    while let Some(arg) = args.next() {
        let arg = arg.as_ref().to_string_lossy();
        if arg == "-h" || arg == "--help" {
            continue;
        }

        if arg == "--config" {
            let _ = args.next();
            continue;
        }

        if arg.starts_with("--config=") {
            continue;
        }

        if arg == "--url" {
            let _ = args.next();
            continue;
        }

        if arg.starts_with("--url=") {
            continue;
        }

        return false;
    }

    true
}

fn render_root_help_with_tools(socket_override: Option<&Path>, url: Option<&str>) -> String {
    let mut help = Cli::command().render_help().to_string();

    if let Some(url) = url {
        if let Ok(tools) = daemon::read_cached_tool_summaries(url, socket_override) {
            let visible_tools = filter_root_help_tools(tools);
            if !visible_tools.is_empty() {
                help = replace_commands_section(&help, &visible_tools);
            }
        }
    }

    if !help.ends_with('\n') {
        help.push('\n');
    }

    help
}

fn filter_root_help_tools(tools: Vec<daemon::CachedToolSummary>) -> Vec<daemon::CachedToolSummary> {
    tools
        .into_iter()
        .filter(|tool| !ROOT_HELP_HIDDEN_TOOLS.contains(&tool.name.as_str()))
        .collect()
}

fn replace_commands_section(help: &str, tools: &[daemon::CachedToolSummary]) -> String {
    let Some((section_start, section_end)) = find_commands_section_bounds(help) else {
        let mut output = help.trim_end().to_owned();
        output.push_str("\n\nCommands:\n");
        output.push_str(&format_commands_section(&[], tools));
        output.push('\n');
        return output;
    };

    let existing = &help[section_start..section_end];
    let parsed_commands = parse_command_section(existing);
    let rendered_commands = format_commands_section(&parsed_commands, tools);
    let mut output = String::with_capacity(help.len() + rendered_commands.len());
    output.push_str(&help[..section_start]);
    output.push_str(&rendered_commands);
    output.push_str(&help[section_end..]);
    output
}

fn format_commands_section(
    commands: &[HelpCommandSummary],
    tools: &[daemon::CachedToolSummary],
) -> String {
    let width = commands
        .iter()
        .map(|command| command.name.chars().count())
        .chain(tools.iter().map(|tool| tool.name.chars().count()))
        .max()
        .unwrap_or(0);
    let mut output = String::new();

    for command in commands {
        match command.description.as_deref() {
            Some(description) => {
                output.push_str(&format!(
                    "  {:width$}  {description}\n",
                    command.name,
                    width = width
                ));
            }
            None => output.push_str(&format!("  {}\n", command.name)),
        }
    }

    for tool in tools {
        match tool.description.as_deref() {
            Some(description) => {
                let description = truncate_tool_description(description, 100);
                output.push_str(&format!(
                    "  {:width$}  {description}\n",
                    tool.name,
                    width = width
                ));
            }
            None => output.push_str(&format!("  {}\n", tool.name)),
        }
    }

    output
}

fn find_commands_section_bounds(help: &str) -> Option<(usize, usize)> {
    let commands_header = "\nCommands:\n";
    let commands_start = help.find(commands_header)?;
    let section_start = commands_start + commands_header.len();

    match help[section_start..].find("\n\n") {
        Some(section_end) => Some((section_start, section_start + section_end)),
        None => Some((section_start, help.len())),
    }
}

#[derive(Debug, PartialEq, Eq)]
struct HelpCommandSummary {
    name: String,
    description: Option<String>,
}

fn parse_command_section(section: &str) -> Vec<HelpCommandSummary> {
    section
        .lines()
        .filter_map(parse_command_line)
        .collect::<Vec<_>>()
}

fn parse_command_line(line: &str) -> Option<HelpCommandSummary> {
    let trimmed = line.strip_prefix("  ")?;
    let separator = trimmed.char_indices().find_map(|(index, ch)| {
        (ch == ' ' && trimmed[index..].starts_with("  ")).then_some(index)
    })?;
    let name = trimmed[..separator].trim();
    let description = trimmed[separator..].trim();

    if name.is_empty() {
        return None;
    }

    Some(HelpCommandSummary {
        name: name.to_owned(),
        description: (!description.is_empty()).then(|| description.to_owned()),
    })
}

fn truncate_tool_description(description: &str, max_chars: usize) -> String {
    let normalized = description.split_whitespace().collect::<Vec<_>>().join(" ");
    let char_count = normalized.chars().count();

    if char_count <= max_chars {
        return normalized;
    }

    let truncated = normalized.chars().take(max_chars).collect::<String>();
    format!("{truncated}...")
}

async fn run(cli: Cli) -> Result<(), Box<dyn Error>> {
    let arg_count = env::args_os().len();
    let config_path = resolve_config_path(cli.config.clone())?;
    let requires_config_url = command_requires_config_url(cli.command.as_ref());
    let effective_url = resolve_effective_url(
        cli.url.clone(),
        &config_path,
        cli.config.as_deref(),
        requires_config_url,
    )?;

    if command_requires_runtime_checks(cli.command.as_ref()) {
        check_runtime_requirements()?;
    }

    if command_requires_daemon_ready(cli.command.as_ref()) {
        daemon::ensure_daemon_running(
            effective_url
                .as_deref()
                .ok_or("configured url is required before starting the daemon")?,
            cli.config.as_deref(),
            command_socket_override(cli.command.as_ref()),
        )
        .await?;
    }

    match cli.command {
        Some(Commands::Config(command)) => run_config_command(command, &config_path)?,
        Some(Commands::Reload) => {
            let url = effective_url
                .as_deref()
                .ok_or("configured url is required before reloading cached tools")?;
            let status = daemon::reload_tool_cache(url, None).await?;
            let state = if status.changed {
                "updated"
            } else {
                "unchanged"
            };
            println!("tool cache {state} for {url} ({} tools)", status.tool_count);
        }
        Some(Commands::Daemon(command)) => {
            run_daemon_command(
                command,
                &config_path,
                cli.config.as_deref(),
                effective_url.as_deref(),
            )
            .await?;
        }
        Some(Commands::Tool(args)) => {
            tool::run_tool_command(
                &args,
                None,
                effective_url
                    .as_deref()
                    .ok_or("configured url is required before loading cached tools")?,
            )
            .await?;
        }
        None => {
            if should_print_help(arg_count) {
                print!(
                    "{}",
                    render_root_help_with_tools(None, effective_url.as_deref())
                );
            }
        }
    }

    Ok(())
}

async fn run_daemon_command(
    command: DaemonCommand,
    config_path: &Path,
    config_override: Option<&Path>,
    configured_url: Option<&str>,
) -> Result<(), Box<dyn Error>> {
    match command.command {
        DaemonSubcommands::Run => {
            let url = configured_url
                .map(ToOwned::to_owned)
                .ok_or_else(|| missing_url_error(config_path, config_override))?;
            daemon::run_daemon(&url, command.socket.as_deref()).await?;
        }
        DaemonSubcommands::Status => {
            let status = daemon::request_status(configured_url, command.socket.as_deref()).await?;
            println!("daemon ({status}) is running");
        }
        DaemonSubcommands::Exit => {
            daemon::request_exit(configured_url, command.socket.as_deref()).await?;
            println!("daemon exit");
        }
    }

    Ok(())
}

fn command_requires_runtime_checks(command: Option<&Commands>) -> bool {
    !matches!(
        command,
        Some(Commands::Config(_)) | Some(Commands::Daemon(_))
    )
}

fn command_requires_config_url(command: Option<&Commands>) -> bool {
    match command {
        None => true,
        Some(Commands::Config(_)) => false,
        Some(Commands::Reload) => true,
        Some(Commands::Daemon(DaemonCommand {
            command: DaemonSubcommands::Run,
            ..
        })) => true,
        Some(Commands::Daemon(_)) => false,
        Some(Commands::Tool(_)) => true,
    }
}

fn command_requires_daemon_ready(command: Option<&Commands>) -> bool {
    !matches!(
        command,
        Some(Commands::Config(_)) | Some(Commands::Reload) | Some(Commands::Daemon(_))
    )
}

fn command_socket_override(command: Option<&Commands>) -> Option<&Path> {
    match command {
        Some(Commands::Daemon(DaemonCommand { socket, .. })) => socket.as_deref(),
        _ => None,
    }
}

fn run_config_command(command: ConfigCommand, config_path: &Path) -> Result<(), Box<dyn Error>> {
    match (command.url, command.command) {
        (Some(url), None) => {
            let config = Config { url };
            write_config(config_path, &config)?;
            println!("Saved configuration to {}", config_path.display());
            Ok(())
        }
        (None, Some(ConfigSubcommands::Show)) => {
            let config = read_stored_config(config_path)
                .map_err(|error| -> Box<dyn Error> { error.into() })?;
            println!("{}", toml::to_string_pretty(&config)?);
            Ok(())
        }
        _ => Err("invalid config command".into()),
    }
}

fn ensure_url_configured(
    config_path: &Path,
    config_override: Option<&Path>,
) -> Result<(), Box<dyn Error>> {
    let config = match read_stored_config(config_path) {
        Ok(config) => config,
        Err(error) if error.starts_with("config file not found: ") => {
            return Err(missing_url_error(config_path, config_override).into());
        }
        Err(error) => return Err(error.into()),
    };

    match config.url {
        Some(url) if !url.trim().is_empty() => Ok(()),
        _ => Err(missing_url_error(config_path, config_override).into()),
    }
}

fn help_cache_url_for_args(args: &[OsString]) -> Option<String> {
    let mut args = args.iter().skip(1);
    let mut config_override = None;
    let mut url_override = None;

    while let Some(arg) = args.next() {
        let arg = arg.to_string_lossy();
        if arg == "-h" || arg == "--help" {
            continue;
        }

        if arg == "--config" {
            config_override = args.next().map(PathBuf::from);
            continue;
        }

        if let Some(path) = arg.strip_prefix("--config=") {
            config_override = Some(PathBuf::from(path));
            continue;
        }

        if arg == "--url" {
            url_override = args
                .next()
                .and_then(|value| parse_url(&value.to_string_lossy()).ok());
            continue;
        }

        if let Some(url) = arg.strip_prefix("--url=") {
            url_override = parse_url(url).ok();
            continue;
        }
    }

    if url_override.is_some() {
        return url_override;
    }

    let config_path = resolve_config_path(config_override).ok()?;
    read_configured_url(&config_path).ok()
}

fn resolve_effective_url(
    cli_url: Option<String>,
    config_path: &Path,
    config_override: Option<&Path>,
    require_config_url: bool,
) -> Result<Option<String>, Box<dyn Error>> {
    if let Some(url) = cli_url {
        return Ok(Some(url));
    }

    if require_config_url {
        ensure_url_configured(config_path, config_override)?;
        return Ok(Some(read_configured_url(config_path)?));
    }

    Ok(read_optional_configured_url(config_path))
}

fn read_configured_url(config_path: &Path) -> Result<String, Box<dyn Error>> {
    let config =
        read_stored_config(config_path).map_err(|error| -> Box<dyn Error> { error.into() })?;

    match config.url {
        Some(url) if !url.trim().is_empty() => Ok(url),
        _ => Err(format!("missing `url` in config file {}", config_path.display()).into()),
    }
}

fn read_optional_configured_url(config_path: &Path) -> Option<String> {
    let config = read_stored_config(config_path).ok()?;

    config.url.and_then(|url| {
        let trimmed = url.trim();
        (!trimmed.is_empty()).then(|| trimmed.to_owned())
    })
}

fn read_stored_config(config_path: &Path) -> Result<StoredConfig, String> {
    let contents = fs::read_to_string(config_path).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            format!("config file not found: {}", config_path.display())
        } else {
            format!(
                "failed to read config file {}: {error}",
                config_path.display()
            )
        }
    })?;

    toml::from_str(&contents).map_err(|error| {
        format!(
            "failed to parse config file {}: {error}",
            config_path.display()
        )
    })
}

fn missing_url_error(config_path: &Path, config_override: Option<&Path>) -> String {
    let mut command = String::from(CLI_COMMAND_NAME);

    if let Some(path) = config_override {
        command.push_str(" --config ");
        command.push_str(&path.display().to_string());
    }

    command.push_str(" config --url <URL>");

    format!(
        "missing `url` in config file {}. Configure it with: {}",
        config_path.display(),
        command
    )
}

fn check_runtime_requirements() -> Result<(), Box<dyn Error>> {
    let node_version = command_version("node")?;
    command_version("npx")?;

    let major = parse_node_major_version(&node_version)?;
    if major < 18 {
        return Err(format!("node 18 or above is required, found {node_version}").into());
    }

    Ok(())
}

fn format_clap_error(error: &clap::Error) -> String {
    let message = error.to_string();
    let message = message.trim_end();

    message
        .strip_prefix("error: ")
        .unwrap_or(message)
        .to_owned()
}

fn command_version(command: &str) -> Result<String, Box<dyn Error>> {
    match Command::new(command).arg("--version").output() {
        Ok(output) if output.status.success() => {
            Ok(String::from_utf8(output.stdout)?.trim().into())
        }
        Ok(_) => Err(format!("`{command}` is installed but not working correctly").into()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            Err(format!("`{command}` command not found").into())
        }
        Err(error) => Err(format!("failed to run `{command}`: {error}").into()),
    }
}

fn parse_node_major_version(version: &str) -> Result<u64, Box<dyn Error>> {
    let version = version
        .strip_prefix('v')
        .ok_or("node version output must start with `v`")?;
    let major = version
        .split('.')
        .next()
        .ok_or("node version output is missing a major version")?;

    Ok(major.parse()?)
}

fn parse_url(value: &str) -> Result<String, String> {
    if value.starts_with("http://") || value.starts_with("https://") {
        Ok(value.to_owned())
    } else {
        Err("url must start with http:// or https://".to_owned())
    }
}

fn default_config_path() -> Result<PathBuf, Box<dyn Error>> {
    let home_dir = env::var_os("HOME").ok_or("HOME environment variable is not set")?;
    Ok(PathBuf::from(home_dir)
        .join(".config")
        .join("ones-mcp-cli")
        .join("config.toml"))
}

fn resolve_config_path(path: Option<PathBuf>) -> Result<PathBuf, Box<dyn Error>> {
    match path {
        Some(path) => Ok(path),
        None => default_config_path(),
    }
}

fn write_config(path: &Path, config: &Config) -> Result<(), Box<dyn Error>> {
    let parent = path
        .parent()
        .ok_or("failed to determine config directory")?;

    fs::create_dir_all(parent)?;
    let contents = toml::to_string_pretty(config)?;
    fs::write(path, contents)?;

    Ok(())
}

#[cfg(test)]
mod tests;
