use std::collections::VecDeque;
use std::env;
use std::error::Error;
use std::fs;
use std::fs::File;
use std::io;
use std::os::unix::fs::FileTypeExt;
use std::os::unix::net::UnixStream as StdUnixStream;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{UnixListener, UnixStream};
use tokio::process::{Child, Command};
use tokio::signal;
use tokio::sync::watch;
use tokio::time::{interval, sleep};

const DAEMON_READY_RETRIES: usize = 100;
const DAEMON_RETRY_DELAY: Duration = Duration::from_millis(50);
const TOOL_CACHE_READY_RETRIES: usize = 600;
const TOOL_CACHE_RETRY_DELAY: Duration = Duration::from_millis(50);
const TOOL_CACHE_REFRESH_INTERVAL: Duration = Duration::from_secs(30 * 60);
const TOOL_CACHE_FILE_NAME: &str = "tools.json";
const MCP_PROTOCOL_VERSION: &str = "2024-11-05";

#[derive(Debug)]
pub(crate) struct DaemonStatus {
    pub(crate) version: String,
    pub(crate) pid: u32,
    pub(crate) control_socket_path: PathBuf,
    pub(crate) url: Option<String>,
}

impl std::fmt::Display for DaemonStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.url {
            Some(url) => write!(
                f,
                "version {}, pid {}, url {}, control {}",
                self.version,
                self.pid,
                url,
                self.control_socket_path.display()
            ),
            None => write!(
                f,
                "version {}, pid {}, control {}",
                self.version,
                self.pid,
                self.control_socket_path.display()
            ),
        }
    }
}

#[derive(Debug, Deserialize, Serialize, PartialEq)]
struct ToolCache {
    url: String,
    tools: Vec<Value>,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct CachedToolSummary {
    pub(crate) name: String,
    pub(crate) description: Option<String>,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub(crate) struct CachedTool {
    pub(crate) name: String,
    pub(crate) description: Option<String>,
    #[serde(rename = "inputSchema", default)]
    pub(crate) input_schema: Value,
}

#[derive(Debug)]
struct DownstreamConnection {
    reader: BufReader<OwnedReadHalf>,
    writer: OwnedWriteHalf,
}

impl DownstreamConnection {
    fn new(stream: UnixStream) -> Self {
        let (reader, writer) = stream.into_split();
        Self {
            reader: BufReader::new(reader),
            writer,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ListToolsResult {
    tools: Vec<Value>,
    next_cursor: Option<String>,
}

#[derive(Debug)]
struct PendingToolRefresh {
    request_id: Value,
    tools: Vec<Value>,
}

#[derive(Debug)]
enum InFlightRequest {
    Client { id: Value },
    Refresh(PendingToolRefresh),
}

pub(crate) async fn run_daemon(
    url: &str,
    socket_override: Option<&Path>,
) -> Result<(), Box<dyn Error>> {
    let socket_path = resolve_socket_path(Some(url), socket_override)?;
    let control_socket_path = control_socket_path(&socket_path)?;
    let tool_cache_path = tool_cache_path(url, socket_override)?;

    prepare_socket_path(&socket_path)?;
    prepare_socket_path(&control_socket_path)?;

    let public_listener = UnixListener::bind(&socket_path)?;
    let control_listener = UnixListener::bind(&control_socket_path)?;
    let _public_guard = SocketFileGuard::new(socket_path.clone());
    let _control_guard = SocketFileGuard::new(control_socket_path.clone());
    let (shutdown_tx, shutdown_rx) = watch::channel(false);

    eprintln!("Listening on {}", socket_path.display());
    eprintln!("Control socket on {}", control_socket_path.display());
    eprintln!("Tool cache on {}", tool_cache_path.display());

    let bridge = run_bridge(
        public_listener,
        url.to_owned(),
        tool_cache_path,
        shutdown_rx.clone(),
    );
    let control = run_control_server(
        control_listener,
        url.to_owned(),
        control_socket_path.clone(),
        shutdown_tx.clone(),
        shutdown_rx.clone(),
    );

    tokio::pin!(bridge);
    tokio::pin!(control);

    tokio::select! {
        result = &mut bridge => {
            result?;
            signal_shutdown(&shutdown_tx)?;
            control.await?;
        }
        result = &mut control => {
            match result? {
                ControlFlow::ExitRequested => {
                    signal_shutdown(&shutdown_tx)?;
                    bridge.await?;
                }
                ControlFlow::ShutdownObserved => {
                    bridge.await?;
                }
            }
        }
        result = shutdown_signal() => {
            result?;
            signal_shutdown(&shutdown_tx)?;
            bridge.await?;
            control.await?;
        }
    }

    Ok(())
}

pub(crate) async fn ensure_daemon_running(
    url: &str,
    config_override: Option<&Path>,
    socket_override: Option<&Path>,
) -> Result<DaemonStatus, Box<dyn Error>> {
    let status = match probe_status(Some(url), socket_override).await? {
        Some(status)
            if status.version == env!("CARGO_PKG_VERSION")
                && status
                    .url
                    .as_deref()
                    .is_some_and(|status_url| urls_share_cache_scope(status_url, url)) =>
        {
            Ok(status)
        }
        Some(_) => {
            request_exit(Some(url), socket_override).await?;
            spawn_detached_daemon(url, config_override, socket_override)?;
            request_status(Some(url), socket_override).await
        }
        None => {
            reset_broken_daemon_state(socket_override)?;
            spawn_detached_daemon(url, config_override, socket_override)?;
            request_status(Some(url), socket_override).await
        }
    }?;

    wait_for_tool_cache(url, socket_override).await?;
    Ok(status)
}

pub(crate) fn spawn_detached_daemon(
    url: &str,
    config_override: Option<&Path>,
    socket_override: Option<&Path>,
) -> Result<(), Box<dyn Error>> {
    let executable = env::current_exe()?;
    let socket_path = resolve_socket_path(Some(url), socket_override)?;
    let control_socket_path = control_socket_path(&socket_path)?;
    let tool_cache_path = tool_cache_path(url, socket_override)?;
    let startup_log_path = daemon_startup_log_path(&control_socket_path)?;
    let mut command = std::process::Command::new(executable);

    if let Some(path) = config_override {
        command.arg("--config").arg(path);
    }

    command.arg("--url").arg(url);
    command.arg("daemon");

    if let Some(path) = socket_override {
        command.arg("--socket").arg(path);
    }

    command.arg("run").arg("--foreground");
    command.stdin(Stdio::null());
    command.stdout(Stdio::null());
    command.stderr(Stdio::from(File::create(&startup_log_path)?));

    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(io::Error::last_os_error());
            }

            libc::signal(libc::SIGHUP, libc::SIG_IGN);
            Ok(())
        });
    }

    let mut child = command.spawn()?;
    wait_until_ready(&control_socket_path, &mut child, &startup_log_path)?;
    wait_until_tool_cache_ready(&tool_cache_path, &mut child, &startup_log_path)?;
    remove_startup_log_if_present(&startup_log_path);
    Ok(())
}

pub(crate) async fn request_status(
    url: Option<&str>,
    socket_override: Option<&Path>,
) -> Result<DaemonStatus, Box<dyn Error>> {
    probe_status(url, socket_override)
        .await?
        .ok_or_else(|| daemon_not_running_error(url, socket_override).into())
}

pub(crate) async fn request_exit(
    url: Option<&str>,
    socket_override: Option<&Path>,
) -> Result<(), Box<dyn Error>> {
    let socket_path = resolve_socket_path(url, socket_override)?;
    let control_socket_path = control_socket_path(&socket_path)?;
    let response = send_control_request(url, socket_override, "exit")
        .await?
        .ok_or_else(|| daemon_not_running_error(url, socket_override))?;
    if response != "exiting" {
        return Err(format!("unexpected daemon exit response: {response}").into());
    }

    wait_until_stopped(&socket_path, &control_socket_path).await
}

pub(crate) async fn call_tool(
    url: &str,
    socket_override: Option<&Path>,
    name: &str,
    arguments: Value,
) -> Result<Value, Box<dyn Error>> {
    let socket_path = resolve_socket_path(Some(url), socket_override)?;
    let stream = UnixStream::connect(&socket_path).await.map_err(|error| {
        format!(
            "failed to connect to daemon socket {}: {error}",
            socket_path.display()
        )
    })?;
    let (reader, mut writer) = stream.into_split();
    let mut reader = BufReader::new(reader);

    let initialize_id = Value::String("ones-mcp-cli-client:initialize".to_owned());
    write_downstream_message(
        &mut writer,
        &json!({
            "jsonrpc": "2.0",
            "id": initialize_id.clone(),
            "method": "initialize",
            "params": {
                "protocolVersion": MCP_PROTOCOL_VERSION,
                "capabilities": {},
                "clientInfo": {
                    "name": "ones-mcp-cli",
                    "version": env!("CARGO_PKG_VERSION"),
                }
            }
        }),
    )
    .await?;
    let initialize_response = read_downstream_response_for_id(&mut reader, &initialize_id).await?;
    response_result(&initialize_response, "initialize")?;
    write_downstream_message(
        &mut writer,
        &json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized",
        }),
    )
    .await?;

    let request_id = Value::String("ones-mcp-cli-client:tools/call".to_owned());
    write_downstream_message(
        &mut writer,
        &json!({
            "jsonrpc": "2.0",
            "id": request_id.clone(),
            "method": "tools/call",
            "params": {
                "name": name,
                "arguments": arguments,
            }
        }),
    )
    .await?;

    let response = read_downstream_response_for_id(&mut reader, &request_id).await?;
    response_result(&response, "tools/call")
}

pub(crate) fn resolve_socket_path(
    url: Option<&str>,
    socket_override: Option<&Path>,
) -> Result<PathBuf, Box<dyn Error>> {
    match socket_override {
        Some(path) => Ok(path.to_path_buf()),
        None => default_socket_path(url),
    }
}

fn default_socket_path(url: Option<&str>) -> Result<PathBuf, Box<dyn Error>> {
    let socket_file_name = default_socket_file_name(url);

    if let Some(runtime_dir) = env::var_os("XDG_RUNTIME_DIR") {
        return Ok(PathBuf::from(runtime_dir)
            .join("ones-mcp-cli")
            .join(socket_file_name));
    }

    let home_dir = env::var_os("HOME").ok_or("HOME environment variable is not set")?;

    Ok(PathBuf::from(home_dir)
        .join(".cache")
        .join("ones-mcp-cli")
        .join(socket_file_name))
}

fn default_socket_file_name(url: Option<&str>) -> String {
    match url {
        Some(url) => format!("daemon-{}.sock", cache_scope_path_component(url)),
        None => "daemon.sock".to_owned(),
    }
}

fn control_socket_path(public_socket_path: &Path) -> Result<PathBuf, Box<dyn Error>> {
    let file_name = public_socket_path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or("failed to determine socket filename")?;
    let parent = public_socket_path
        .parent()
        .ok_or("failed to determine socket directory")?;

    Ok(parent.join(format!("{file_name}.ctl")))
}

fn prepare_socket_path(path: &Path) -> Result<(), Box<dyn Error>> {
    let parent = path
        .parent()
        .ok_or("failed to determine socket directory")?;
    fs::create_dir_all(parent)?;

    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if !metadata.file_type().is_socket() {
                return Err(format!(
                    "refusing to overwrite non-socket file at {}",
                    path.display()
                )
                .into());
            }

            match StdUnixStream::connect(path) {
                Ok(_) => Err(format!("socket already in use: {}", path.display()).into()),
                Err(error) if is_stale_socket_error(error.kind()) => {
                    fs::remove_file(path)?;
                    Ok(())
                }
                Err(error) => Err(format!(
                    "failed to probe existing socket {}: {error}",
                    path.display()
                )
                .into()),
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => {
            Err(format!("failed to inspect socket path {}: {error}", path.display()).into())
        }
    }
}

fn reset_broken_daemon_state(socket_override: Option<&Path>) -> Result<(), Box<dyn Error>> {
    let socket_path = resolve_socket_path(None, socket_override)?;
    let control_socket_path = control_socket_path(&socket_path)?;

    let removed_public_socket = remove_socket_file_if_present(&socket_path)?;
    let removed_control_socket = remove_socket_file_if_present(&control_socket_path)?;

    if removed_public_socket || removed_control_socket {
        eprintln!(
            "removed broken daemon socket state at {} and {}",
            socket_path.display(),
            control_socket_path.display()
        );
    }

    Ok(())
}

fn remove_socket_file_if_present(path: &Path) -> Result<bool, Box<dyn Error>> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if !metadata.file_type().is_socket() {
                return Err(format!(
                    "refusing to overwrite non-socket file at {}",
                    path.display()
                )
                .into());
            }

            fs::remove_file(path)?;
            Ok(true)
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => {
            Err(format!("failed to inspect socket path {}: {error}", path.display()).into())
        }
    }
}

fn daemon_startup_log_path(control_socket_path: &Path) -> Result<PathBuf, Box<dyn Error>> {
    let file_name = control_socket_path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or("failed to determine daemon startup log filename")?;
    let parent = control_socket_path
        .parent()
        .ok_or("failed to determine daemon startup log directory")?;

    Ok(parent.join(format!("{file_name}.startup.log")))
}

fn startup_failure_error(message: String, startup_log_path: &Path) -> Box<dyn Error> {
    let startup_log = read_startup_log(startup_log_path);
    remove_startup_log_if_present(startup_log_path);

    match startup_log {
        Some(startup_log) => format!("{message}\nstartup log:\n{startup_log}").into(),
        None => message.into(),
    }
}

fn read_startup_log(startup_log_path: &Path) -> Option<String> {
    match fs::read_to_string(startup_log_path) {
        Ok(contents) => {
            let contents = contents.trim();
            if contents.is_empty() {
                None
            } else {
                Some(contents.to_owned())
            }
        }
        Err(_) => None,
    }
}

fn remove_startup_log_if_present(startup_log_path: &Path) {
    match fs::remove_file(startup_log_path) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => eprintln!(
            "failed to remove daemon startup log {}: {error}",
            startup_log_path.display()
        ),
    }
}

fn is_stale_socket_error(kind: io::ErrorKind) -> bool {
    matches!(
        kind,
        io::ErrorKind::ConnectionRefused
            | io::ErrorKind::NotFound
            | io::ErrorKind::ConnectionAborted
    )
}

fn wait_until_ready(
    control_socket_path: &Path,
    child: &mut std::process::Child,
    startup_log_path: &Path,
) -> Result<(), Box<dyn Error>> {
    for _ in 0..DAEMON_READY_RETRIES {
        match StdUnixStream::connect(control_socket_path) {
            Ok(_) => return Ok(()),
            Err(error) if is_stale_socket_error(error.kind()) => {}
            Err(error) => {
                return Err(format!(
                    "failed to connect to daemon control socket {}: {error}",
                    control_socket_path.display()
                )
                .into());
            }
        }

        if let Some(status) = child.try_wait()? {
            return Err(startup_failure_error(
                format!("daemon exited before becoming ready: {status}"),
                startup_log_path,
            ));
        }

        std::thread::sleep(DAEMON_RETRY_DELAY);
    }

    Err(format!(
        "timed out waiting for daemon control socket {}",
        control_socket_path.display()
    )
    .into())
}

fn wait_until_tool_cache_ready(
    tool_cache_path: &Path,
    child: &mut std::process::Child,
    startup_log_path: &Path,
) -> Result<(), Box<dyn Error>> {
    for _ in 0..TOOL_CACHE_READY_RETRIES {
        match fs::metadata(tool_cache_path) {
            Ok(metadata) => {
                if metadata.is_file() {
                    return Ok(());
                }

                return Err(format!(
                    "tool cache path exists but is not a file: {}",
                    tool_cache_path.display()
                )
                .into());
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(format!(
                    "failed to inspect tool cache {}: {error}",
                    tool_cache_path.display()
                )
                .into());
            }
        }

        if let Some(status) = child.try_wait()? {
            return Err(startup_failure_error(
                format!(
                    "daemon exited before generating tool cache {}: {status}",
                    tool_cache_path.display()
                ),
                startup_log_path,
            ));
        }

        std::thread::sleep(TOOL_CACHE_RETRY_DELAY);
    }

    Err(format!(
        "timed out waiting for tool cache {}",
        tool_cache_path.display()
    )
    .into())
}

async fn wait_until_stopped(
    socket_path: &Path,
    control_socket_path: &Path,
) -> Result<(), Box<dyn Error>> {
    for _ in 0..DAEMON_READY_RETRIES {
        if !socket_path_exists(socket_path)? && !socket_path_exists(control_socket_path)? {
            return Ok(());
        }

        sleep(DAEMON_RETRY_DELAY).await;
    }

    Err(format!(
        "timed out waiting for daemon to remove sockets {} and {}",
        socket_path.display(),
        control_socket_path.display()
    )
    .into())
}

fn socket_path_exists(path: &Path) -> Result<bool, Box<dyn Error>> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => {
            Err(format!("failed to inspect socket path {}: {error}", path.display()).into())
        }
    }
}

async fn wait_for_tool_cache(
    url: &str,
    socket_override: Option<&Path>,
) -> Result<(), Box<dyn Error>> {
    let cache_path = tool_cache_path(url, socket_override)?;

    for _ in 0..TOOL_CACHE_READY_RETRIES {
        match fs::metadata(&cache_path) {
            Ok(metadata) => {
                if metadata.is_file() {
                    return Ok(());
                }

                return Err(format!(
                    "tool cache path exists but is not a file: {}",
                    cache_path.display()
                )
                .into());
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(format!(
                    "failed to inspect tool cache {}: {error}",
                    cache_path.display()
                )
                .into());
            }
        }

        if probe_status(Some(url), socket_override).await?.is_none() {
            return Err(format!(
                "daemon stopped before generating tool cache {}",
                cache_path.display()
            )
            .into());
        }

        sleep(TOOL_CACHE_RETRY_DELAY).await;
    }

    Err(format!("timed out waiting for tool cache {}", cache_path.display()).into())
}

async fn send_control_request(
    url: Option<&str>,
    socket_override: Option<&Path>,
    command: &str,
) -> Result<Option<String>, Box<dyn Error>> {
    let socket_path = resolve_socket_path(url, socket_override)?;
    let control_socket_path = control_socket_path(&socket_path)?;
    let mut stream = match UnixStream::connect(&control_socket_path).await {
        Ok(stream) => stream,
        Err(error) if is_stale_socket_error(error.kind()) => return Ok(None),
        Err(error) => {
            return Err(format!(
                "failed to connect to daemon control socket {}: {error}",
                control_socket_path.display()
            )
            .into());
        }
    };

    stream.write_all(command.as_bytes()).await?;
    stream.write_all(b"\n").await?;
    stream.shutdown().await?;

    let mut reader = BufReader::new(stream);
    let mut response = String::new();
    let bytes = reader.read_line(&mut response).await?;
    if bytes == 0 {
        return Err("daemon closed the control connection without a response".into());
    }

    Ok(Some(response.trim().to_owned()))
}

async fn shutdown_signal() -> io::Result<()> {
    let mut terminate = signal::unix::signal(signal::unix::SignalKind::terminate())?;

    tokio::select! {
        result = signal::ctrl_c() => result,
        _ = terminate.recv() => Ok(()),
    }
}

async fn run_bridge(
    listener: UnixListener,
    url: String,
    tool_cache_path: PathBuf,
    shutdown_rx: watch::Receiver<bool>,
) -> Result<(), Box<dyn Error>> {
    let npm_cache_dir = tool_cache_path
        .parent()
        .ok_or("failed to determine npm cache directory")?
        .join("npm");
    let mut child = spawn_remote(&url, &npm_cache_dir)?;
    eprintln!("Started remote process for {}", url);

    let child_stdin = child
        .stdin
        .take()
        .ok_or("mcp-remote stdin is no longer available")?;
    let child_stdout = child
        .stdout
        .take()
        .ok_or("mcp-remote stdout is no longer available")?;
    let mut upstream_reader = BufReader::new(child_stdout);
    let mut upstream_writer = child_stdin;
    let mut daemon_request_counter = 0_u64;

    let initialize_result = initialize_upstream(
        &mut upstream_reader,
        &mut upstream_writer,
        &mut daemon_request_counter,
    )
    .await?;
    refresh_tool_cache_once(
        &url,
        &tool_cache_path,
        &mut upstream_reader,
        &mut upstream_writer,
        &mut daemon_request_counter,
    )
    .await?;

    let bridge_result = handle_connection(
        listener,
        &mut upstream_reader,
        &mut upstream_writer,
        initialize_result,
        &url,
        &tool_cache_path,
        daemon_request_counter,
        shutdown_rx,
    )
    .await;

    finish_child(&mut child).await?;
    bridge_result
}

async fn run_control_server(
    listener: UnixListener,
    url: String,
    control_socket_path: PathBuf,
    shutdown_tx: watch::Sender<bool>,
    mut shutdown_rx: watch::Receiver<bool>,
) -> Result<ControlFlow, Box<dyn Error>> {
    loop {
        tokio::select! {
            result = listener.accept() => {
                let (stream, _) = result?;
                if let Some(flow) = handle_control_request(stream, &url, &control_socket_path, &shutdown_tx).await? {
                    return Ok(flow);
                }
            }
            result = shutdown_rx.changed() => {
                result.map_err(|error| format!("failed to observe daemon shutdown: {error}"))?;
                if *shutdown_rx.borrow() {
                    return Ok(ControlFlow::ShutdownObserved);
                }
            }
        }
    }
}

async fn handle_connection(
    listener: UnixListener,
    upstream_reader: &mut BufReader<tokio::process::ChildStdout>,
    upstream_writer: &mut tokio::process::ChildStdin,
    initialize_result: Value,
    url: &str,
    tool_cache_path: &Path,
    mut daemon_request_counter: u64,
    mut shutdown_rx: watch::Receiver<bool>,
) -> Result<(), Box<dyn Error>> {
    let mut downstream = None;
    let mut refresh_interval = interval(TOOL_CACHE_REFRESH_INTERVAL);
    let mut pending_client_messages = VecDeque::new();
    let mut inflight_request = None;
    let mut refresh_requested = false;
    refresh_interval.tick().await;

    loop {
        tokio::select! {
            result = listener.accept(), if downstream.is_none() => {
                let (stream, _) = result?;
                downstream = Some(DownstreamConnection::new(stream));
            }
            result = read_upstream_message(upstream_reader) => {
                match result? {
                    Some(message) => {
                        if handle_inflight_response(
                            &message,
                            url,
                            tool_cache_path,
                            upstream_writer,
                            &mut inflight_request,
                            &mut pending_client_messages,
                            &mut refresh_requested,
                            &mut daemon_request_counter,
                        )
                        .await? {
                        } else if let Some(connection) = downstream.as_mut() {
                            write_downstream_message(&mut connection.writer, &message).await?;
                        } else {
                            eprintln!("dropping upstream message before a client connects");
                        }
                    }
                    None => return Ok(()),
                }
            }
            result = read_downstream_message(&mut downstream), if downstream.is_some() => {
                match result? {
                    Some(message) => {
                        if is_initialize_request(&message) {
                            write_jsonrpc_result(
                                &mut downstream
                                    .as_mut()
                                    .ok_or("downstream connection disappeared")?
                                    .writer,
                                message_id(&message)
                                    .cloned()
                                    .ok_or("initialize request is missing an id")?,
                                initialize_result.clone(),
                            )
                            .await?;
                        } else if is_initialized_notification(&message) {
                        } else {
                            pending_client_messages.push_back(message);
                            dispatch_pending_upstream(
                                upstream_writer,
                                &mut pending_client_messages,
                                &mut inflight_request,
                                &mut refresh_requested,
                                &mut daemon_request_counter,
                            )
                            .await?;
                        }
                    }
                    None => return Ok(()),
                }
            }
            _ = refresh_interval.tick() => {
                refresh_requested = true;
                dispatch_pending_upstream(
                    upstream_writer,
                    &mut pending_client_messages,
                    &mut inflight_request,
                    &mut refresh_requested,
                    &mut daemon_request_counter,
                )
                .await?;
            }
            result = shutdown_rx.changed() => {
                result.map_err(|error| format!("failed to observe daemon shutdown: {error}"))?;
                if *shutdown_rx.borrow() {
                    return Ok(());
                }
            }
        }
    }
}

async fn handle_control_request(
    stream: UnixStream,
    url: &str,
    control_socket_path: &Path,
    shutdown_tx: &watch::Sender<bool>,
) -> Result<Option<ControlFlow>, Box<dyn Error>> {
    let (reader, mut writer) = stream.into_split();
    let mut reader = BufReader::new(reader);
    let mut request = String::new();
    let bytes = reader.read_line(&mut request).await?;

    if bytes == 0 {
        return Ok(None);
    }

    match request.trim() {
        "status" => {
            let response = format!(
                "running version={} pid={} url={} control={}\n",
                env!("CARGO_PKG_VERSION"),
                std::process::id(),
                url,
                control_socket_path.display()
            );
            writer.write_all(response.as_bytes()).await?;
            writer.shutdown().await?;
            Ok(None)
        }
        "exit" => {
            writer.write_all(b"exiting\n").await?;
            writer.shutdown().await?;
            signal_shutdown(shutdown_tx)?;
            Ok(Some(ControlFlow::ExitRequested))
        }
        other => {
            writer
                .write_all(format!("error unknown command: {other}\n").as_bytes())
                .await?;
            writer.shutdown().await?;
            Ok(None)
        }
    }
}

fn signal_shutdown(shutdown_tx: &watch::Sender<bool>) -> Result<(), Box<dyn Error>> {
    shutdown_tx
        .send(true)
        .map_err(|error| format!("failed to signal daemon shutdown: {error}").into())
}

fn spawn_remote(url: &str, npm_cache_dir: &Path) -> Result<Child, Box<dyn Error>> {
    fs::create_dir_all(npm_cache_dir)?;

    let mut command = Command::new("npx");
    command.arg("-y").arg("mcp-remote").arg(url);
    command.env("npm_config_cache", npm_cache_dir);
    command.stdin(Stdio::piped());
    command.stdout(Stdio::piped());
    command.stderr(Stdio::inherit());
    command.kill_on_drop(true);

    Ok(command.spawn()?)
}

async fn initialize_upstream(
    reader: &mut BufReader<tokio::process::ChildStdout>,
    writer: &mut tokio::process::ChildStdin,
    daemon_request_counter: &mut u64,
) -> Result<Value, Box<dyn Error>> {
    let request_id = next_daemon_request_id(daemon_request_counter);
    write_upstream_message(
        writer,
        &json!({
            "jsonrpc": "2.0",
            "id": request_id.clone(),
            "method": "initialize",
            "params": {
                "protocolVersion": MCP_PROTOCOL_VERSION,
                "capabilities": {},
                "clientInfo": {
                    "name": "ones-mcp-cli-daemon",
                    "version": env!("CARGO_PKG_VERSION"),
                }
            }
        }),
    )
    .await?;

    let response = read_response_for_id(reader, &request_id).await?;
    let initialize_result = response_result(&response, "initialize")?;
    write_upstream_message(
        writer,
        &json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized",
        }),
    )
    .await?;

    Ok(initialize_result)
}

async fn refresh_tool_cache_once(
    url: &str,
    cache_path: &Path,
    reader: &mut BufReader<tokio::process::ChildStdout>,
    writer: &mut tokio::process::ChildStdin,
    daemon_request_counter: &mut u64,
) -> Result<(), Box<dyn Error>> {
    let mut tools = Vec::new();
    let mut cursor = None;

    loop {
        let request_id = next_daemon_request_id(daemon_request_counter);
        send_tools_list_request(writer, &request_id, cursor.as_deref()).await?;
        let response = read_response_for_id(reader, &request_id).await?;
        let page = parse_list_tools_result(&response)?;
        tools.extend(page.tools);
        cursor = page.next_cursor;

        if cursor.is_none() {
            break;
        }
    }

    update_tool_cache(url, cache_path, tools)?;
    Ok(())
}

async fn handle_inflight_response(
    message: &Value,
    url: &str,
    cache_path: &Path,
    writer: &mut tokio::process::ChildStdin,
    inflight_request: &mut Option<InFlightRequest>,
    pending_client_messages: &mut VecDeque<Value>,
    refresh_requested: &mut bool,
    daemon_request_counter: &mut u64,
) -> Result<bool, Box<dyn Error>> {
    let Some(current_request) = inflight_request.take() else {
        return Ok(false);
    };

    match current_request {
        InFlightRequest::Client { id } => {
            if message_id(message) != Some(&id) {
                *inflight_request = Some(InFlightRequest::Client { id });
                return Ok(false);
            }

            dispatch_pending_upstream(
                writer,
                pending_client_messages,
                inflight_request,
                refresh_requested,
                daemon_request_counter,
            )
            .await?;
            Ok(false)
        }
        InFlightRequest::Refresh(refresh) => {
            if message_id(message) != Some(&refresh.request_id) {
                *inflight_request = Some(InFlightRequest::Refresh(refresh));
                return Ok(false);
            }

            handle_pending_refresh_response(
                message,
                url,
                cache_path,
                writer,
                refresh,
                inflight_request,
                pending_client_messages,
                refresh_requested,
                daemon_request_counter,
            )
            .await?;
            Ok(true)
        }
    }
}

async fn handle_pending_refresh_response(
    message: &Value,
    url: &str,
    cache_path: &Path,
    writer: &mut tokio::process::ChildStdin,
    refresh: PendingToolRefresh,
    inflight_request: &mut Option<InFlightRequest>,
    pending_client_messages: &mut VecDeque<Value>,
    refresh_requested: &mut bool,
    daemon_request_counter: &mut u64,
) -> Result<(), Box<dyn Error>> {
    let mut refresh = refresh;
    let page = match parse_list_tools_result(message) {
        Ok(page) => page,
        Err(error) => {
            eprintln!("failed to refresh tools from {url}: {error}");
            dispatch_pending_upstream(
                writer,
                pending_client_messages,
                inflight_request,
                refresh_requested,
                daemon_request_counter,
            )
            .await?;
            return Ok(());
        }
    };

    refresh.tools.extend(page.tools);
    if let Some(cursor) = page.next_cursor {
        let request_id = next_daemon_request_id(daemon_request_counter);
        send_tools_list_request(writer, &request_id, Some(&cursor)).await?;
        refresh.request_id = request_id;
        *inflight_request = Some(InFlightRequest::Refresh(refresh));
        return Ok(());
    }

    update_tool_cache(url, cache_path, refresh.tools)?;
    dispatch_pending_upstream(
        writer,
        pending_client_messages,
        inflight_request,
        refresh_requested,
        daemon_request_counter,
    )
    .await
}

async fn dispatch_pending_upstream(
    writer: &mut tokio::process::ChildStdin,
    pending_client_messages: &mut VecDeque<Value>,
    inflight_request: &mut Option<InFlightRequest>,
    refresh_requested: &mut bool,
    daemon_request_counter: &mut u64,
) -> Result<(), Box<dyn Error>> {
    if inflight_request.is_some() {
        return Ok(());
    }

    while let Some(message) = pending_client_messages.pop_front() {
        let request_id = message_id(&message).cloned();
        write_upstream_message(writer, &message).await?;

        if let Some(id) = request_id {
            *inflight_request = Some(InFlightRequest::Client { id });
            return Ok(());
        }
    }

    if *refresh_requested {
        let request_id = next_daemon_request_id(daemon_request_counter);
        send_tools_list_request(writer, &request_id, None).await?;
        *inflight_request = Some(InFlightRequest::Refresh(PendingToolRefresh {
            request_id,
            tools: Vec::new(),
        }));
        *refresh_requested = false;
    }

    Ok(())
}

fn update_tool_cache(
    url: &str,
    cache_path: &Path,
    mut tools: Vec<Value>,
) -> Result<(), Box<dyn Error>> {
    sort_tool_values(&mut tools);
    let tool_names = tools
        .iter()
        .filter_map(tool_name)
        .map(ToOwned::to_owned)
        .collect::<Vec<_>>()
        .join(", ");
    let cache = ToolCache {
        url: url.to_owned(),
        tools,
    };

    match write_tool_cache_if_changed(cache_path, &cache)? {
        true => eprintln!(
            "updated tool cache {} with tools: {}",
            cache_path.display(),
            tool_names
        ),
        false => eprintln!(
            "tool cache unchanged at {} with tools: {}",
            cache_path.display(),
            tool_names
        ),
    }

    Ok(())
}

async fn send_tools_list_request(
    writer: &mut tokio::process::ChildStdin,
    request_id: &Value,
    cursor: Option<&str>,
) -> Result<(), Box<dyn Error>> {
    let params = match cursor {
        Some(cursor) => json!({ "cursor": cursor }),
        None => json!({}),
    };
    write_upstream_message(
        writer,
        &json!({
            "jsonrpc": "2.0",
            "id": request_id.clone(),
            "method": "tools/list",
            "params": params,
        }),
    )
    .await
}

async fn read_response_for_id<R>(
    reader: &mut BufReader<R>,
    request_id: &Value,
) -> Result<Value, Box<dyn Error>>
where
    R: AsyncRead + Unpin,
{
    loop {
        let message = read_upstream_message(reader)
            .await?
            .ok_or("mcp-remote closed the connection while waiting for a response")?;

        if message_id(&message) == Some(request_id) {
            return Ok(message);
        }

        eprintln!("ignoring upstream message while waiting for daemon request {request_id}");
    }
}

async fn read_downstream_response_for_id<R>(
    reader: &mut BufReader<R>,
    request_id: &Value,
) -> Result<Value, Box<dyn Error>>
where
    R: AsyncRead + Unpin,
{
    loop {
        let message = read_downstream_message_frame(reader)
            .await?
            .ok_or("daemon closed the MCP connection while waiting for a response")?;

        if message_id(&message) == Some(request_id) {
            return Ok(message);
        }
    }
}

async fn read_upstream_message<R>(
    reader: &mut BufReader<R>,
) -> Result<Option<Value>, Box<dyn Error>>
where
    R: AsyncRead + Unpin,
{
    let mut line = String::new();
    let bytes = reader.read_line(&mut line).await?;
    if bytes == 0 {
        return Ok(None);
    }

    let line = line.trim_end_matches(['\r', '\n']);
    if line.is_empty() {
        return Err("received an empty upstream JSON-RPC message".into());
    }

    Ok(Some(serde_json::from_str(line)?))
}

async fn read_downstream_message_frame<R>(
    reader: &mut BufReader<R>,
) -> Result<Option<Value>, Box<dyn Error>>
where
    R: AsyncRead + Unpin,
{
    let content_length = match read_content_length(reader).await? {
        Some(content_length) => content_length,
        None => return Ok(None),
    };

    let mut payload = vec![0_u8; content_length];
    reader.read_exact(&mut payload).await?;
    Ok(Some(serde_json::from_slice(&payload)?))
}

async fn read_content_length<R>(reader: &mut BufReader<R>) -> Result<Option<usize>, Box<dyn Error>>
where
    R: AsyncRead + Unpin,
{
    let mut content_length = None;

    loop {
        let mut line = String::new();
        let bytes = reader.read_line(&mut line).await?;
        if bytes == 0 {
            if content_length.is_none() {
                return Ok(None);
            }
            return Err("unexpected EOF while reading MCP headers".into());
        }

        let line = line.trim_end_matches(['\r', '\n']);
        if line.is_empty() {
            let content_length = content_length.ok_or("missing Content-Length header")?;
            return Ok(Some(content_length));
        }

        if let Some((name, value)) = line.split_once(':')
            && name.eq_ignore_ascii_case("content-length")
        {
            content_length = Some(value.trim().parse()?);
        }
    }
}

async fn write_upstream_message<W>(writer: &mut W, message: &Value) -> Result<(), Box<dyn Error>>
where
    W: AsyncWrite + Unpin,
{
    let payload = serde_json::to_vec(message)?;
    writer.write_all(&payload).await?;
    writer.write_all(b"\n").await?;
    writer.flush().await?;
    Ok(())
}

async fn write_downstream_message<W>(writer: &mut W, message: &Value) -> Result<(), Box<dyn Error>>
where
    W: AsyncWrite + Unpin,
{
    let payload = serde_json::to_vec(message)?;
    let header = format!("Content-Length: {}\r\n\r\n", payload.len());
    writer.write_all(header.as_bytes()).await?;
    writer.write_all(&payload).await?;
    writer.flush().await?;
    Ok(())
}

async fn write_jsonrpc_result<W>(
    writer: &mut W,
    id: Value,
    result: Value,
) -> Result<(), Box<dyn Error>>
where
    W: AsyncWrite + Unpin,
{
    write_downstream_message(
        writer,
        &json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": result,
        }),
    )
    .await
}

async fn read_downstream_message(
    downstream: &mut Option<DownstreamConnection>,
) -> Result<Option<Value>, Box<dyn Error>> {
    let connection = downstream
        .as_mut()
        .ok_or("downstream connection is not available")?;
    read_downstream_message_frame(&mut connection.reader).await
}

fn message_id(message: &Value) -> Option<&Value> {
    message.get("id")
}

fn response_result(message: &Value, method: &str) -> Result<Value, Box<dyn Error>> {
    if let Some(error) = message.get("error") {
        return Err(format!(
            "upstream {method} request failed: {}",
            serde_json::to_string(error)?
        )
        .into());
    }

    message
        .get("result")
        .cloned()
        .ok_or_else(|| format!("upstream {method} response is missing a result").into())
}

fn parse_list_tools_result(message: &Value) -> Result<ListToolsResult, Box<dyn Error>> {
    let result = response_result(message, "tools/list")?;
    Ok(serde_json::from_value(result)?)
}

fn next_daemon_request_id(counter: &mut u64) -> Value {
    let request_id = Value::String(format!("ones-mcp-cli-daemon:{counter}"));
    *counter += 1;
    request_id
}

fn is_initialize_request(message: &Value) -> bool {
    message
        .get("method")
        .and_then(Value::as_str)
        .map(|method| method == "initialize" && message.get("id").is_some())
        .unwrap_or(false)
}

fn is_initialized_notification(message: &Value) -> bool {
    message
        .get("method")
        .and_then(Value::as_str)
        .map(|method| method == "notifications/initialized" && message.get("id").is_none())
        .unwrap_or(false)
}

fn sort_tool_values(tools: &mut [Value]) {
    tools.sort_by(|left, right| tool_name(left).cmp(&tool_name(right)));
}

fn tool_name(tool: &Value) -> Option<&str> {
    tool.get("name").and_then(Value::as_str)
}

pub(crate) fn read_cached_tool_summaries(
    url: &str,
    socket_override: Option<&Path>,
) -> Result<Vec<CachedToolSummary>, Box<dyn Error>> {
    Ok(read_cached_tools(url, socket_override)?
        .into_iter()
        .map(|tool| CachedToolSummary {
            name: tool.name,
            description: tool.description,
        })
        .collect())
}

pub(crate) fn read_cached_tools(
    url: &str,
    socket_override: Option<&Path>,
) -> Result<Vec<CachedTool>, Box<dyn Error>> {
    let cache_path = tool_cache_path(url, socket_override)?;

    match fs::read_to_string(&cache_path) {
        Ok(contents) => {
            let mut cache: ToolCache = serde_json::from_str(&contents)?;
            let mut tools = cache
                .tools
                .drain(..)
                .filter_map(|tool| serde_json::from_value::<CachedTool>(tool).ok())
                .collect::<Vec<_>>();
            tools.sort_by(|left, right| left.name.cmp(&right.name));
            Ok(tools)
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(error) => Err(format!(
            "failed to read tool cache {}: {error}",
            cache_path.display()
        )
        .into()),
    }
}

fn tool_cache_path(url: &str, socket_override: Option<&Path>) -> Result<PathBuf, Box<dyn Error>> {
    let socket_path = resolve_socket_path(Some(url), socket_override)?;
    Ok(tool_cache_dir(&socket_path, url)?.join(TOOL_CACHE_FILE_NAME))
}

fn tool_cache_dir(socket_path: &Path, url: &str) -> Result<PathBuf, Box<dyn Error>> {
    let cache_root = socket_path
        .parent()
        .map(Path::to_path_buf)
        .ok_or_else(|| -> Box<dyn Error> { "failed to determine tool cache directory".into() })?;
    Ok(cache_root
        .join("tool-cache")
        .join(cache_scope_path_component(url)))
}

fn cache_scope_path_component(url: &str) -> String {
    let scope = cache_scope_key(url).unwrap_or(url);
    encode_cache_path_component(scope)
}

fn urls_share_cache_scope(left: &str, right: &str) -> bool {
    cache_scope_path_component(left) == cache_scope_path_component(right)
}

fn cache_scope_key(url: &str) -> Option<&str> {
    let (_, remainder) = url.split_once("://")?;
    let authority = remainder
        .split(['/', '?', '#'])
        .next()
        .filter(|authority| !authority.is_empty())?;
    let authority = authority
        .rsplit_once('@')
        .map(|(_, authority)| authority)
        .unwrap_or(authority);

    if authority.starts_with('[') {
        let end = authority.find(']')?;
        return Some(&authority[1..end]);
    }

    authority.split(':').next().filter(|host| !host.is_empty())
}

fn encode_cache_path_component(value: &str) -> String {
    let normalized = value.to_ascii_lowercase();
    let mut encoded = String::with_capacity(normalized.len());

    for byte in normalized.bytes() {
        match byte {
            b'0'..=b'9' | b'a'..=b'z' | b'A'..=b'Z' | b'-' | b'_' | b'.' => {
                encoded.push(char::from(byte));
            }
            _ => encoded.push_str(&format!("_{byte:02X}")),
        }
    }

    encoded
}

fn write_tool_cache_if_changed(
    cache_path: &Path,
    cache: &ToolCache,
) -> Result<bool, Box<dyn Error>> {
    let parent = cache_path
        .parent()
        .ok_or("failed to determine tool cache directory")?;
    fs::create_dir_all(parent)?;

    let contents = serde_json::to_string_pretty(cache)?;
    match fs::read_to_string(cache_path) {
        Ok(existing) if existing == contents => Ok(false),
        Ok(_) => {
            fs::write(cache_path, contents)?;
            Ok(true)
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            fs::write(cache_path, contents)?;
            Ok(true)
        }
        Err(error) => Err(format!(
            "failed to read tool cache {}: {error}",
            cache_path.display()
        )
        .into()),
    }
}

async fn finish_child(child: &mut Child) -> Result<(), Box<dyn Error>> {
    let status = match child.try_wait()? {
        Some(status) => status,
        None => {
            child.kill().await?;
            child.wait().await?
        }
    };

    if status.success() {
        Ok(())
    } else {
        Err(format!("mcp-remote exited with status {status}").into())
    }
}

enum ControlFlow {
    ExitRequested,
    ShutdownObserved,
}

struct SocketFileGuard {
    path: PathBuf,
}

impl SocketFileGuard {
    fn new(path: PathBuf) -> Self {
        Self { path }
    }
}

impl Drop for SocketFileGuard {
    fn drop(&mut self) {
        match fs::remove_file(&self.path) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => eprintln!("failed to remove socket {}: {error}", self.path.display()),
        }
    }
}

async fn probe_status(
    url: Option<&str>,
    socket_override: Option<&Path>,
) -> Result<Option<DaemonStatus>, Box<dyn Error>> {
    let Some(response) = send_control_request(url, socket_override, "status").await? else {
        return Ok(None);
    };

    Ok(Some(parse_status_response(&response)?))
}

fn parse_status_response(response: &str) -> Result<DaemonStatus, Box<dyn Error>> {
    let response = response
        .strip_prefix("running version=")
        .ok_or_else(|| format!("unexpected daemon status response: {response}"))?;
    let (version, response) = response
        .split_once(" pid=")
        .ok_or_else(|| format!("unexpected daemon status response: running version={response}"))?;
    let (pid, response) = response.split_once(' ').ok_or_else(|| {
        format!("unexpected daemon status response: running version={version} pid={response}")
    })?;
    let (url, control_socket_path) =
        if let Some(control_socket_path) = response.strip_prefix("control=") {
            (None, control_socket_path)
        } else if let Some(response) = response.strip_prefix("url=") {
            let (url, control_socket_path) = response.split_once(" control=").ok_or_else(|| {
            format!(
                "unexpected daemon status response: running version={version} pid={pid} {response}"
            )
        })?;
            (Some(url.to_owned()), control_socket_path)
        } else {
            return Err(format!(
                "unexpected daemon status response: running version={version} pid={pid} {response}"
            )
            .into());
        };

    Ok(DaemonStatus {
        version: version.to_owned(),
        pid: pid.parse()?,
        control_socket_path: PathBuf::from(control_socket_path),
        url,
    })
}

fn daemon_not_running_error(url: Option<&str>, socket_override: Option<&Path>) -> String {
    let socket_path = match resolve_socket_path(url, socket_override) {
        Ok(path) => path,
        Err(error) => return format!("failed to resolve daemon socket path: {error}"),
    };

    match control_socket_path(&socket_path) {
        Ok(path) => format!("daemon is not running: {}", path.display()),
        Err(error) => format!("failed to resolve daemon control socket path: {error}"),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};
    use std::fs;
    use std::os::unix::net::UnixListener as StdUnixListener;
    use std::path::{Path, PathBuf};
    use std::time::{SystemTime, UNIX_EPOCH};
    use tokio::io::BufReader;
    use tokio::net::UnixListener;

    use super::{
        ToolCache, cache_scope_key, cache_scope_path_component, call_tool, parse_status_response,
        read_cached_tool_summaries, read_downstream_message_frame, reset_broken_daemon_state,
        resolve_socket_path, sort_tool_values, tool_cache_dir, urls_share_cache_scope,
        write_downstream_message, write_tool_cache_if_changed,
    };

    #[test]
    fn parses_daemon_status_response_with_url() {
        let status = parse_status_response(
            "running version=0.1.0 pid=42 url=https://example.com control=/tmp/ones-mcp-cli.sock.ctl",
        )
        .expect("expected daemon status to parse");

        assert_eq!(status.version, "0.1.0");
        assert_eq!(status.pid, 42);
        assert_eq!(status.url.as_deref(), Some("https://example.com"));
        assert_eq!(
            status.control_socket_path,
            Path::new("/tmp/ones-mcp-cli.sock.ctl")
        );
    }

    #[test]
    fn parses_legacy_daemon_status_response_without_url() {
        let status = parse_status_response(
            "running version=0.1.0 pid=42 control=/tmp/ones-mcp-cli.sock.ctl",
        )
        .expect("expected legacy daemon status to parse");

        assert_eq!(status.version, "0.1.0");
        assert_eq!(status.pid, 42);
        assert_eq!(status.url, None);
        assert_eq!(
            status.control_socket_path,
            Path::new("/tmp/ones-mcp-cli.sock.ctl")
        );
    }

    #[test]
    fn rejects_unexpected_daemon_status_response() {
        let error = parse_status_response("running mcp-cli 0.1.0, pid 42")
            .expect_err("expected daemon status parse failure");

        assert_eq!(
            error.to_string(),
            "unexpected daemon status response: running mcp-cli 0.1.0, pid 42"
        );
    }

    #[test]
    fn extracts_host_for_cache_scope_key() {
        assert_eq!(
            cache_scope_key("https://example.com/api/v1?x=1"),
            Some("example.com")
        );
        assert_eq!(
            cache_scope_key("https://USER:PASS@EXAMPLE.COM:8443/api/v1?x=1"),
            Some("EXAMPLE.COM")
        );
        assert_eq!(cache_scope_key("https://[::1]:8443/api"), Some("::1"));
    }

    #[test]
    fn uses_host_for_cache_path_component() {
        assert_eq!(
            cache_scope_path_component("https://example.com/api/v1?x=1"),
            "example.com"
        );
        assert_eq!(
            cache_scope_path_component("https://EXAMPLE.COM:8443/api/v1?x=1"),
            "example.com"
        );
    }

    #[test]
    fn urls_share_cache_scope_for_same_host() {
        assert!(urls_share_cache_scope(
            "https://example.com/api/v1",
            "http://EXAMPLE.COM:8443/other"
        ));
    }

    #[test]
    fn urls_do_not_share_cache_scope_for_different_hosts() {
        assert!(!urls_share_cache_scope(
            "https://example.com/api/v1",
            "https://example.net/api/v1"
        ));
    }

    #[test]
    fn tool_cache_dir_is_resolved_from_socket_directory_and_url() {
        let dir = tool_cache_dir(
            Path::new("/tmp/ones-mcp-cli/daemon.sock"),
            "https://example.com",
        )
        .expect("expected tool cache dir");
        assert_eq!(dir, Path::new("/tmp/ones-mcp-cli/tool-cache/example.com"));
    }

    #[test]
    fn default_socket_path_uses_host_scope() {
        let path = resolve_socket_path(Some("https://example.com/api/v1"), None)
            .expect("expected socket path");

        assert_eq!(
            path.file_name().and_then(|name| name.to_str()),
            Some("daemon-example.com.sock")
        );
    }

    #[test]
    fn default_socket_paths_are_shared_for_same_host() {
        let left = resolve_socket_path(Some("https://example.com/api/v1"), None)
            .expect("expected left socket path");
        let right = resolve_socket_path(Some("http://EXAMPLE.COM:8443/other"), None)
            .expect("expected right socket path");

        assert_eq!(left, right);
    }

    #[test]
    fn default_socket_paths_are_distinct_for_different_hosts() {
        let left =
            resolve_socket_path(Some("https://example.com"), None).expect("expected left socket");
        let right =
            resolve_socket_path(Some("https://example.net"), None).expect("expected right socket");

        assert_ne!(left, right);
    }

    #[test]
    fn tool_cache_dirs_are_shared_for_same_host() {
        let left = tool_cache_dir(
            Path::new("/tmp/ones-mcp-cli/daemon.sock"),
            "https://example.com/api/v1",
        )
        .expect("expected left tool cache dir");
        let right = tool_cache_dir(
            Path::new("/tmp/ones-mcp-cli/daemon.sock"),
            "http://EXAMPLE.COM/other",
        )
        .expect("expected right tool cache dir");

        assert_eq!(left, right);
    }

    #[test]
    fn tool_cache_dirs_are_distinct_for_different_hosts() {
        let left = tool_cache_dir(
            Path::new("/tmp/ones-mcp-cli/daemon.sock"),
            "https://example.com",
        )
        .expect("expected left tool cache dir");
        let right = tool_cache_dir(
            Path::new("/tmp/ones-mcp-cli/daemon.sock"),
            "https://example.net",
        )
        .expect("expected right tool cache dir");

        assert_ne!(left, right);
    }

    #[test]
    fn skips_cache_write_when_contents_are_unchanged() {
        let temp_dir = unique_temp_dir();
        let cache_path = temp_dir.join("tools.json");
        let cache = ToolCache {
            url: "https://example.com".to_owned(),
            tools: vec![json!({ "name": "alpha", "description": "Alpha tool" })],
        };

        assert!(
            write_tool_cache_if_changed(&cache_path, &cache).expect("expected initial cache write")
        );
        assert!(
            !write_tool_cache_if_changed(&cache_path, &cache).expect("expected unchanged cache")
        );
    }

    #[test]
    fn rewrites_cache_when_contents_change() {
        let temp_dir = unique_temp_dir();
        let cache_path = temp_dir.join("tools.json");
        let initial = ToolCache {
            url: "https://example.com".to_owned(),
            tools: vec![json!({ "name": "alpha", "description": "Alpha tool" })],
        };
        let updated = ToolCache {
            url: "https://example.com".to_owned(),
            tools: vec![
                json!({ "name": "alpha", "description": "Alpha tool" }),
                json!({ "name": "beta", "description": "Beta tool" }),
            ],
        };

        assert!(
            write_tool_cache_if_changed(&cache_path, &initial)
                .expect("expected initial cache write")
        );
        assert!(
            write_tool_cache_if_changed(&cache_path, &updated)
                .expect("expected changed cache write")
        );
    }

    #[test]
    fn reads_cached_tool_summaries_from_tool_cache() {
        let temp_dir = unique_temp_dir();
        let socket_path = temp_dir.join("daemon.sock");
        let cache_path = tool_cache_dir(&socket_path, "https://example.com")
            .expect("expected cache dir")
            .join("tools.json");
        let cache = ToolCache {
            url: "https://example.com".to_owned(),
            tools: vec![
                json!({ "name": "beta", "description": "Beta tool" }),
                json!({ "name": "alpha", "description": "Alpha tool" }),
                json!({ "description": "Ignored tool" }),
            ],
        };

        write_tool_cache_if_changed(&cache_path, &cache).expect("expected cache write");

        let tools = read_cached_tool_summaries("https://example.com", Some(&socket_path))
            .expect("expected cached tools");

        assert_eq!(
            tools,
            vec![
                super::CachedToolSummary {
                    name: "alpha".to_owned(),
                    description: Some("Alpha tool".to_owned()),
                },
                super::CachedToolSummary {
                    name: "beta".to_owned(),
                    description: Some("Beta tool".to_owned()),
                },
            ]
        );
    }

    #[test]
    fn reads_cached_tool_summaries_from_matching_url_only() {
        let temp_dir = unique_temp_dir();
        let socket_path = temp_dir.join("daemon.sock");
        let example_com_cache_path = tool_cache_dir(&socket_path, "https://example.com")
            .expect("expected example.com cache dir")
            .join("tools.json");
        let example_net_cache_path = tool_cache_dir(&socket_path, "https://example.net")
            .expect("expected example.net cache dir")
            .join("tools.json");

        write_tool_cache_if_changed(
            &example_com_cache_path,
            &ToolCache {
                url: "https://example.com".to_owned(),
                tools: vec![json!({ "name": "alpha", "description": "Alpha tool" })],
            },
        )
        .expect("expected example.com cache write");
        write_tool_cache_if_changed(
            &example_net_cache_path,
            &ToolCache {
                url: "https://example.net".to_owned(),
                tools: vec![json!({ "name": "beta", "description": "Beta tool" })],
            },
        )
        .expect("expected example.net cache write");

        let tools = read_cached_tool_summaries("https://example.net", Some(&socket_path))
            .expect("expected cached tools");

        assert_eq!(
            tools,
            vec![super::CachedToolSummary {
                name: "beta".to_owned(),
                description: Some("Beta tool".to_owned()),
            }]
        );
    }

    #[tokio::test]
    async fn call_tool_sends_request_through_daemon_socket() {
        let temp_dir = unique_socket_temp_dir();
        let socket_path = temp_dir.join("daemon.sock");
        let listener = UnixListener::bind(&socket_path).expect("expected socket listener");

        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("expected client connection");
            let (reader, mut writer) = stream.into_split();
            let mut reader = BufReader::new(reader);

            let initialize = read_downstream_message_frame(&mut reader)
                .await
                .expect("expected initialize frame")
                .expect("expected initialize message");
            assert_eq!(
                initialize.get("method").and_then(Value::as_str),
                Some("initialize")
            );
            let initialize_id = initialize
                .get("id")
                .cloned()
                .expect("expected initialize id");
            write_downstream_message(
                &mut writer,
                &json!({
                    "jsonrpc": "2.0",
                    "id": initialize_id,
                    "result": {
                        "protocolVersion": super::MCP_PROTOCOL_VERSION,
                        "capabilities": {}
                    }
                }),
            )
            .await
            .expect("expected initialize response");

            let notification = read_downstream_message_frame(&mut reader)
                .await
                .expect("expected initialized notification")
                .expect("expected initialized message");
            assert_eq!(
                notification.get("method").and_then(Value::as_str),
                Some("notifications/initialized")
            );

            let call = read_downstream_message_frame(&mut reader)
                .await
                .expect("expected tools/call frame")
                .expect("expected tools/call message");
            assert_eq!(
                call.get("method").and_then(Value::as_str),
                Some("tools/call")
            );
            assert_eq!(
                call.pointer("/params/name").and_then(Value::as_str),
                Some("sample_tool")
            );
            assert_eq!(
                call.pointer("/params/arguments"),
                Some(&json!({ "issueID": "ISS-1" }))
            );
            let call_id = call.get("id").cloned().expect("expected tools/call id");
            write_downstream_message(
                &mut writer,
                &json!({
                    "jsonrpc": "2.0",
                    "id": call_id,
                    "result": {
                        "content": [
                            {
                                "type": "text",
                                "text": "ok"
                            }
                        ]
                    }
                }),
            )
            .await
            .expect("expected tools/call response");
        });

        let result = call_tool(
            "https://example.com",
            Some(&socket_path),
            "sample_tool",
            json!({ "issueID": "ISS-1" }),
        )
        .await
        .expect("expected tool result");

        assert_eq!(
            result,
            json!({
                "content": [
                    {
                        "type": "text",
                        "text": "ok"
                    }
                ]
            })
        );

        server.await.expect("expected server task");
    }

    #[test]
    fn sorts_tools_by_name_for_stable_cache_contents() {
        let mut tools = vec![
            json!({ "name": "beta" }),
            json!({ "name": "alpha" }),
            json!({ "name": "gamma" }),
        ];

        sort_tool_values(&mut tools);

        assert_eq!(
            tools,
            vec![
                json!({ "name": "alpha" }),
                json!({ "name": "beta" }),
                json!({ "name": "gamma" }),
            ]
        );
    }

    #[test]
    fn reset_broken_daemon_state_removes_existing_socket_files() {
        let temp_dir = unique_socket_temp_dir();
        let socket_path = temp_dir.join("daemon.sock");
        let control_socket_path = temp_dir.join("daemon.sock.ctl");
        let _public_listener =
            StdUnixListener::bind(&socket_path).expect("expected public socket listener");
        let _control_listener =
            StdUnixListener::bind(&control_socket_path).expect("expected control socket listener");

        reset_broken_daemon_state(Some(&socket_path)).expect("expected socket cleanup");

        assert!(!socket_path.exists());
        assert!(!control_socket_path.exists());
    }

    #[test]
    fn reset_broken_daemon_state_rejects_non_socket_files() {
        let temp_dir = unique_temp_dir();
        let socket_path = temp_dir.join("daemon.sock");
        fs::write(&socket_path, "not a socket").expect("expected regular file");

        let error = reset_broken_daemon_state(Some(&socket_path))
            .expect_err("expected non-socket path to be rejected");

        assert_eq!(
            error.to_string(),
            format!(
                "refusing to overwrite non-socket file at {}",
                socket_path.display()
            )
        );
    }

    fn unique_temp_dir() -> PathBuf {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("expected monotonic clock")
            .as_nanos();
        let path = std::env::temp_dir().join(format!("ones-mcp-cli-daemon-tests-{suffix}"));
        std::fs::create_dir_all(&path).expect("expected temp dir");
        path
    }

    fn unique_socket_temp_dir() -> PathBuf {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("expected monotonic clock")
            .as_nanos()
            % 1_000_000_000;
        let path = std::env::temp_dir().join(format!("omc-{suffix}"));
        std::fs::create_dir_all(&path).expect("expected temp dir");
        path
    }
}
