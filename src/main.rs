use std::env;
use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};
use std::process;
use std::process::Command;

use clap::{Args, CommandFactory, Parser, Subcommand, error::ErrorKind};
use serde::{Deserialize, Serialize};

#[derive(Parser, Debug)]
#[command(name = "ones-mcp-cli")]
#[command(about = "ONES MCP command line interface")]
struct Cli {
    /// Path to the config file
    #[arg(long, global = true)]
    config: Option<PathBuf>,

    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Manage CLI configuration
    Config(ConfigCommand),
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

#[derive(Serialize)]
struct Config {
    url: String,
}

#[derive(Deserialize, Serialize)]
struct StoredConfig {
    url: Option<String>,
}

fn main() {
    if should_print_help(env::args_os().len()) {
        let mut command = Cli::command();
        command.print_help().expect("failed to print help");
        println!();
        process::exit(0);
    }

    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(error) => match error.kind() {
            ErrorKind::DisplayHelp | ErrorKind::DisplayVersion => {
                print!("{error}");
                process::exit(0);
            }
            _ => {
                eprintln!("fatal error: {}", format_clap_error(&error));
                process::exit(1);
            }
        },
    };

    if let Err(error) = run(cli) {
        eprintln!("fatal error: {error}");
        process::exit(1);
    }
}

fn should_print_help(arg_count: usize) -> bool {
    arg_count == 1
}

fn run(cli: Cli) -> Result<(), Box<dyn Error>> {
    check_runtime_requirements()?;
    let config_path = resolve_config_path(cli.config.clone())?;

    if !matches!(cli.command, Some(Commands::Config(_))) {
        ensure_url_configured(&config_path, cli.config.as_deref())?;
    }

    match cli.command {
        Some(Commands::Config(command)) => run_config_command(command, &config_path)?,
        None => {}
    }

    Ok(())
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
    let mut command = String::from("mcp-cli");

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
