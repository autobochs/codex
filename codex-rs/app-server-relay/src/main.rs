use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use anyhow::Context;
use anyhow::Result;
use anyhow::anyhow;
use clap::Parser;
use clap::Subcommand;
use codex_app_server_protocol::JSONRPCErrorError;
use codex_app_server_protocol::JSONRPCMessage;
use codex_app_server_protocol::RemoteControlConnectionStatus;
use codex_app_server_protocol::RemoteControlPairingStartParams;
use codex_app_server_protocol::RemoteControlPairingStatusParams;
use codex_app_server_protocol::RequestId;
use codex_app_server_protocol::ServerNotificationEnvelope;
use codex_app_server_protocol::ServerRequest;
use codex_app_server_transport::ConnectionId;
use codex_app_server_transport::OutgoingError;
use codex_app_server_transport::OutgoingMessage;
use codex_app_server_transport::OutgoingResponse;
use codex_app_server_transport::QueuedOutgoingMessage;
use codex_app_server_transport::RemoteControlPolicy;
use codex_app_server_transport::RemoteControlStartConfig;
use codex_app_server_transport::RemoteControlStartupMode;
use codex_app_server_transport::TransportEvent;
use codex_app_server_transport::start_remote_control;
use codex_config::types::AuthCredentialsStoreMode;
use codex_login::AuthKeyringBackendKind;
use codex_login::AuthManager;
use codex_login::CLIENT_ID;
use codex_login::CodexAuth;
use codex_login::ServerOptions;
use codex_login::run_device_code_login;
use codex_login::run_login_server;
use codex_state::StateRuntime;
use codex_uds::UnixStream;
use futures::SinkExt;
use futures::StreamExt;
use tempfile::TempDir;
use tokio::process::Child;
use tokio::process::Command;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::client_async;
use tokio_tungstenite::tungstenite::Message;
use tokio_util::sync::CancellationToken;
use tracing_subscriber::EnvFilter;

const CHATGPT_BASE_URL: &str = "https://chatgpt.com/backend-api/";
const CHILD_READY_TIMEOUT: Duration = Duration::from_secs(15);
const REMOTE_CONTROL_READY_TIMEOUT: Duration = Duration::from_secs(15);
const UDS_HANDSHAKE_URL: &str = "ws://localhost/rpc";

#[derive(Debug, Parser)]
#[command(name = "codex-relay")]
#[command(about = "Bridge ChatGPT remote control to a stock Codex app-server")]
struct Cli {
    /// Print detailed transport, enrollment, and pairing diagnostics.
    #[arg(long, global = true)]
    verbose: bool,

    #[arg(long, env = "CODEX_RELAY_HOME")]
    relay_home: Option<PathBuf>,

    #[command(subcommand)]
    command: RelayCommand,
}

#[derive(Debug, Subcommand)]
enum RelayCommand {
    /// Log the relay into the ChatGPT account used for remote control.
    Login {
        /// Use the local browser callback flow instead of device-code authentication.
        #[arg(long)]
        browser: bool,
    },
    /// Run the relay, optionally creating a new pairing code.
    RemoteControl {
        #[command(subcommand)]
        command: RemoteControlCommand,
    },
}

#[derive(Debug, Subcommand)]
enum RemoteControlCommand {
    /// Start forwarding already-paired remote clients.
    Start(StartArgs),
    /// Start forwarding and print a new pairing code.
    Pair {
        #[command(flatten)]
        start: StartArgs,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PairingMode {
    None,
    Manual,
}

#[derive(Debug, clap::Args)]
struct StartArgs {
    /// Codex executable used for the child app-server.
    #[arg(long, default_value = "codex")]
    codex: PathBuf,

    /// CODEX_HOME for Codex sessions. Omit to inherit the current environment.
    #[arg(long)]
    session_codex_home: Option<PathBuf>,

    /// Override the ChatGPT backend used by remote control.
    #[arg(long, default_value = CHATGPT_BASE_URL)]
    remote_control_url: String,

    /// Name shown for this machine in remote control.
    #[arg(long)]
    name: Option<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let filter = if cli.verbose {
        EnvFilter::new("codex_app_server_transport=debug,codex_login=info,codex_state=info")
    } else {
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn"))
    };
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .try_init()
        .map_err(|error| anyhow!("failed to initialize relay diagnostics: {error}"))?;
    let verbose = cli.verbose;
    let relay_home = match cli.relay_home {
        Some(path) => path,
        None => dirs::home_dir()
            .context("could not determine the home directory")?
            .join(".codex-relay"),
    };
    std::fs::create_dir_all(&relay_home)
        .with_context(|| format!("failed to create {}", relay_home.display()))?;
    restrict_relay_home_permissions(&relay_home)?;

    match cli.command {
        RelayCommand::Login { browser } => login(relay_home, browser).await,
        RelayCommand::RemoteControl { command } => match command {
            RemoteControlCommand::Start(args) => {
                run_relay(relay_home, args, PairingMode::None, verbose).await
            }
            RemoteControlCommand::Pair { start } => {
                run_relay(relay_home, start, PairingMode::Manual, verbose).await
            }
        },
    }
}

#[cfg(unix)]
fn restrict_relay_home_permissions(relay_home: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    std::fs::set_permissions(relay_home, std::fs::Permissions::from_mode(0o700))
        .with_context(|| format!("failed to secure {}", relay_home.display()))
}

#[cfg(not(unix))]
fn restrict_relay_home_permissions(_relay_home: &Path) -> Result<()> {
    Ok(())
}

async fn login(relay_home: PathBuf, browser: bool) -> Result<()> {
    let options = ServerOptions::new(
        relay_home,
        CLIENT_ID.to_string(),
        /*forced_chatgpt_workspace_id*/ None,
        AuthCredentialsStoreMode::File,
        AuthKeyringBackendKind::default(),
        /*auth_route_config*/ None,
    );
    if !browser {
        run_device_code_login(options).await?;
        return Ok(());
    }

    let server = run_login_server(options)?;
    eprintln!(
        "Open this URL to authenticate the relay:\n\n{}\n",
        server.auth_url
    );
    server.block_until_done().await?;
    Ok(())
}

async fn run_relay(
    relay_home: PathBuf,
    args: StartArgs,
    pairing_mode: PairingMode,
    verbose: bool,
) -> Result<()> {
    let auth_manager = AuthManager::shared(
        relay_home.clone(),
        /*enable_codex_api_key_env*/ false,
        AuthCredentialsStoreMode::File,
        /*forced_chatgpt_workspace_id*/ None,
        Some(args.remote_control_url.clone()),
        AuthKeyringBackendKind::default(),
        /*auth_route_config*/ None,
    )
    .await;
    match auth_manager.auth().await {
        Some(CodexAuth::Chatgpt(_) | CodexAuth::ChatgptAuthTokens(_)) => {}
        Some(_) => {
            return Err(anyhow!(
                "relay authentication is not a ChatGPT login; run `codex-relay login`"
            ));
        }
        None => {
            return Err(anyhow!(
                "relay is not authenticated; run `codex-relay login` first"
            ));
        }
    }
    let state_db = StateRuntime::init(relay_home.clone(), "openai".to_string()).await?;
    let installation_id = load_or_create_installation_id(&relay_home)?;
    let shutdown = CancellationToken::new();
    let (transport_tx, transport_rx) = mpsc::channel(128);
    let (mut remote_task, remote_handle) = start_remote_control(
        RemoteControlStartConfig {
            remote_control_url: args.remote_control_url.clone(),
            installation_id,
            server_name: args.name.clone(),
            policy: RemoteControlPolicy::Allowed,
        },
        Some(state_db),
        auth_manager,
        transport_tx,
        shutdown.clone(),
        /*app_server_client_name_rx*/ None,
        RemoteControlStartupMode::EnabledEphemeral,
    )
    .await?;

    let child_runtime = ChildRuntime::start(&args).await?;
    if pairing_mode == PairingMode::Manual {
        eprintln!("Waiting for the remote-control websocket to connect...");
        let mut status_rx = remote_handle.status_receiver();
        tokio::time::timeout(REMOTE_CONTROL_READY_TIMEOUT, async {
            loop {
                match status_rx.borrow().status {
                    RemoteControlConnectionStatus::Connected => return Ok(()),
                    RemoteControlConnectionStatus::Connecting => {}
                    RemoteControlConnectionStatus::Disabled => {
                        return Err(anyhow!(
                            "remote control became disabled before pairing"
                        ));
                    }
                    RemoteControlConnectionStatus::Errored => {
                        return Err(anyhow!(
                            "remote-control websocket failed before pairing; check the relay account and backend URL"
                        ));
                    }
                }
                status_rx
                    .changed()
                    .await
                    .context("remote-control status channel closed before pairing")?;
            }
        })
        .await
        .context("timed out waiting for the remote-control websocket before pairing")??;
        let connected = status_rx.borrow().clone();
        eprintln!(
            "Remote-control websocket connected as {} (environment {}).",
            connected.server_name,
            connected.environment_id.as_deref().unwrap_or("unknown")
        );
        let pairing = remote_handle
            .start_pairing(
                RemoteControlPairingStartParams { manual_code: true },
                /*app_server_client_name*/ None,
            )
            .await
            .context("failed to create a manual remote-control pairing code")?;
        let manual_code = pairing
            .manual_pairing_code
            .context("remote-control backend did not return the requested manual pairing code")?;
        eprintln!("Pairing code: {manual_code}");
        eprintln!("Pairing expires at Unix time {}", pairing.expires_at);
        eprintln!("Waiting for ChatGPT to claim the pairing code...");
        let pairing_handle = remote_handle.clone();
        tokio::spawn(async move {
            loop {
                let now = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map(|duration| duration.as_secs() as i64)
                    .unwrap_or_default();
                if now >= pairing.expires_at {
                    eprintln!("Pairing expired before it was claimed.");
                    break;
                }
                match pairing_handle
                    .pairing_status(RemoteControlPairingStatusParams {
                        pairing_code: None,
                        manual_pairing_code: Some(manual_code.clone()),
                    })
                    .await
                {
                    Ok(status) if status.claimed => {
                        eprintln!("Pairing completed successfully.");
                        break;
                    }
                    Ok(_) => {
                        if verbose {
                            eprintln!("Pairing status: not yet claimed.");
                        }
                    }
                    Err(error) => {
                        eprintln!("Pairing status check failed: {error}");
                        break;
                    }
                }
                tokio::time::sleep(Duration::from_secs(3)).await;
            }
        });
    }
    eprintln!("Relay connected; press Ctrl-C to stop.");

    let ChildRuntime {
        mut child,
        socket_path,
        _temp_dir,
    } = child_runtime;
    let bridge_result = bridge_connections(transport_rx, &socket_path, &shutdown);
    tokio::pin!(bridge_result);
    let run_result = tokio::select! {
        result = &mut bridge_result => result,
        status = child.wait() => {
            let status = status.context("failed to wait for child app-server")?;
            Err(anyhow!("child app-server exited unexpectedly with {status}"))
        },
        result = &mut remote_task => {
            result.context("remote-control task panicked")?;
            Err(anyhow!("remote-control transport stopped unexpectedly"))
        },
        signal = tokio::signal::ctrl_c() => signal.map_err(anyhow::Error::from),
    };

    shutdown.cancel();
    let _ = child.start_kill();
    let _ = child.wait().await;
    if !remote_task.is_finished()
        && tokio::time::timeout(Duration::from_secs(5), &mut remote_task)
            .await
            .is_err()
    {
        remote_task.abort();
    }
    run_result
}

fn load_or_create_installation_id(relay_home: &Path) -> Result<String> {
    let path = relay_home.join("installation_id");
    if let Ok(value) = std::fs::read_to_string(&path) {
        let value = value.trim();
        if !value.is_empty() {
            return Ok(value.to_string());
        }
    }
    let value = uuid::Uuid::now_v7().to_string();
    std::fs::write(&path, format!("{value}\n"))
        .with_context(|| format!("failed to write {}", path.display()))?;
    Ok(value)
}

struct ChildRuntime {
    child: Child,
    socket_path: PathBuf,
    _temp_dir: TempDir,
}

impl ChildRuntime {
    async fn start(args: &StartArgs) -> Result<Self> {
        let temp_dir = tempfile::tempdir().context("failed to create relay runtime directory")?;
        let socket_path = temp_dir.path().join("app-server.sock");
        let mut command = Command::new(&args.codex);
        command
            .arg("app-server")
            .arg("--listen")
            .arg(format!("unix://{}", socket_path.display()))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .kill_on_drop(true);
        if let Some(session_codex_home) = &args.session_codex_home {
            command.env("CODEX_HOME", session_codex_home);
        }
        let mut child = command.spawn().with_context(|| {
            format!("failed to start Codex executable {}", args.codex.display())
        })?;
        tokio::time::timeout(CHILD_READY_TIMEOUT, async {
            loop {
                if let Some(status) = child
                    .try_wait()
                    .context("failed to inspect child app-server")?
                {
                    return Err(anyhow!(
                        "child app-server exited with {status} before becoming ready"
                    ));
                }
                if socket_path.exists() {
                    return Ok(());
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .context("timed out waiting for child app-server socket")??;
        Ok(Self {
            child,
            socket_path,
            _temp_dir: temp_dir,
        })
    }
}

struct ConnectionBridge {
    incoming_tx: mpsc::Sender<JSONRPCMessage>,
    task: JoinHandle<()>,
}

async fn bridge_connections(
    mut events: mpsc::Receiver<TransportEvent>,
    socket_path: &Path,
    shutdown: &CancellationToken,
) -> Result<()> {
    let mut connections = HashMap::<ConnectionId, ConnectionBridge>::new();
    loop {
        tokio::select! {
            _ = shutdown.cancelled() => break,
            event = events.recv() => {
                let Some(event) = event else {
                    return Err(anyhow!("remote-control transport event channel closed"));
                };
                match event {
                    TransportEvent::ConnectionOpened {
                        connection_id,
                        writer,
                        disconnect_sender,
                        ..
                    } => {
                        match connect_child(socket_path, writer, disconnect_sender.clone()).await {
                            Ok(bridge) => {
                                if let Some(previous) = connections.insert(connection_id, bridge) {
                                    previous.task.abort();
                                }
                            }
                            Err(error) => {
                                eprintln!("Failed to open local bridge for remote connection {connection_id}: {error:#}");
                                if let Some(disconnect_sender) = disconnect_sender {
                                    disconnect_sender.cancel();
                                }
                            }
                        }
                    }
                    TransportEvent::IncomingMessage { connection_id, message } => {
                        if let Some(connection) = connections.get(&connection_id)
                            && connection.incoming_tx.send(message).await.is_err()
                        {
                            connections.remove(&connection_id);
                        }
                    }
                    TransportEvent::ConnectionClosed { connection_id } => {
                        if let Some(connection) = connections.remove(&connection_id) {
                            connection.task.abort();
                        }
                    }
                }
            }
        }
    }
    for (_, connection) in connections {
        connection.task.abort();
    }
    Ok(())
}

async fn connect_child(
    socket_path: &Path,
    remote_writer: mpsc::Sender<QueuedOutgoingMessage>,
    disconnect_sender: Option<CancellationToken>,
) -> Result<ConnectionBridge> {
    let stream = UnixStream::connect(socket_path)
        .await
        .with_context(|| format!("failed to connect to {}", socket_path.display()))?;
    let (websocket, _) = client_async(UDS_HANDSHAKE_URL, stream)
        .await
        .context("child app-server WebSocket handshake failed")?;
    let (incoming_tx, incoming_rx) = mpsc::channel(128);
    let task = tokio::spawn(async move {
        if let Err(error) = run_connection(websocket, incoming_rx, remote_writer).await {
            eprintln!("Local app-server bridge closed: {error:#}");
        }
        if let Some(disconnect_sender) = disconnect_sender {
            disconnect_sender.cancel();
        }
    });
    Ok(ConnectionBridge { incoming_tx, task })
}

async fn run_connection(
    websocket: WebSocketStream<UnixStream>,
    mut incoming_rx: mpsc::Receiver<JSONRPCMessage>,
    remote_writer: mpsc::Sender<QueuedOutgoingMessage>,
) -> Result<()> {
    let (mut sink, mut stream) = websocket.split();
    loop {
        tokio::select! {
            incoming = incoming_rx.recv() => {
                let Some(incoming) = incoming else { break };
                let text = serde_json::to_string(&incoming)
                    .context("failed to serialize remote JSON-RPC message")?;
                sink.send(Message::Text(text.into()))
                    .await
                    .context("failed to send JSON-RPC message to child app-server")?;
            }
            outgoing = stream.next() => {
                match outgoing {
                    Some(Ok(Message::Text(text))) => {
                        let message = parse_child_message(&text)
                            .context("child app-server sent invalid JSON-RPC")?;
                        remote_writer.send(QueuedOutgoingMessage::new(message))
                            .await
                            .context("remote connection closed")?;
                    }
                    Some(Ok(Message::Ping(payload))) => {
                        sink.send(Message::Pong(payload))
                            .await
                            .context("failed to answer child app-server ping")?;
                    }
                    Some(Ok(Message::Pong(_) | Message::Frame(_))) => {}
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Ok(Message::Binary(_))) => {
                        return Err(anyhow!("child app-server sent an unsupported binary frame"));
                    }
                    Some(Err(error)) => return Err(error).context("child app-server WebSocket failed"),
                }
            }
        }
    }
    let _ = sink.close().await;
    Ok(())
}

fn parse_child_message(text: &str) -> serde_json::Result<OutgoingMessage> {
    let object = serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(text)?;
    let value = serde_json::Value::Object(object.clone());
    if object.contains_key("method") {
        if object.contains_key("id") {
            return serde_json::from_value::<ServerRequest>(value).map(OutgoingMessage::Request);
        }
        return serde_json::from_value::<ServerNotificationEnvelope>(value)
            .map(OutgoingMessage::AppServerNotification);
    }
    if object.contains_key("result") {
        let id = serde_json::from_value::<RequestId>(value["id"].clone())?;
        return Ok(OutgoingMessage::Response(OutgoingResponse {
            id,
            result: value["result"].clone(),
        }));
    }
    let id = serde_json::from_value::<RequestId>(value["id"].clone())?;
    let error = serde_json::from_value::<JSONRPCErrorError>(value["error"].clone())?;
    Ok(OutgoingMessage::Error(OutgoingError { error, id }))
}

#[cfg(test)]
#[path = "main_tests.rs"]
mod tests;
