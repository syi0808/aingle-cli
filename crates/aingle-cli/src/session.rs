use std::{
    fs::{self, OpenOptions},
    io::Write,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    path::{Path, PathBuf},
    process::{Command as ProcessCommand, Stdio},
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use aingle_client::{AingleClient, ChatEvent, Command, Config, Identity};
use anyhow::{Context, Result, anyhow, bail};
use clap::Subcommand;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{TcpListener, TcpStream},
    sync::{Mutex, Notify, mpsc, oneshot, watch},
};
use uuid::Uuid;

use crate::{AGENT_SAFETY_NOTICE, write_json};

const LOCAL_STARTUP_TIMEOUT: Duration = Duration::from_secs(5);
const LOCAL_REQUEST_TIMEOUT: Duration = Duration::from_secs(3);
const MAX_CONTROL_LINE_BYTES: usize = 64 * 1024;

#[derive(Subcommand)]
pub enum SessionCommands {
    Start,
    Find {
        session_id: Uuid,
    },
    Send {
        session_id: Uuid,
        #[arg(long)]
        content: String,
    },
    Next {
        session_id: Uuid,
    },
    Leave {
        session_id: Uuid,
    },
    Cancel {
        session_id: Uuid,
    },
    Status {
        session_id: Uuid,
    },
    Events {
        session_id: Uuid,
        #[arg(long, default_value_t = 0)]
        after: u64,
        #[arg(long)]
        wait: Option<humantime::Duration>,
    },
    Attach {
        session_id: Uuid,
        #[arg(long, default_value_t = 0)]
        after: u64,
    },
    Close {
        session_id: Uuid,
    },
    List,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum SessionAction {
    Find,
    Message { content: String },
    Next,
    Leave,
    Cancel,
    Close,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct SessionMetadata {
    session_id: Uuid,
    token: String,
    port: u16,
    pid: u32,
    created_at_ms: u64,
    status: SessionStatus,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct SessionStatus {
    session_id: Uuid,
    state: String,
    last_cursor: u64,
    agent_id: Option<String>,
    conversation_id: Option<Uuid>,
    peer_agent_id: Option<String>,
    visibility: Option<String>,
    last_error: Option<String>,
}

impl SessionStatus {
    fn starting(session_id: Uuid) -> Self {
        Self {
            session_id,
            state: "starting".into(),
            last_cursor: 0,
            agent_id: None,
            conversation_id: None,
            peer_agent_id: None,
            visibility: None,
            last_error: None,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct StoredEvent {
    cursor: u64,
    #[serde(flatten)]
    event: ChatEvent,
}

#[derive(Deserialize, Serialize)]
struct ControlRequest {
    token: String,
    #[serde(flatten)]
    operation: ControlOperation,
}

#[derive(Deserialize, Serialize)]
#[serde(tag = "op", rename_all = "snake_case")]
enum ControlOperation {
    Status,
    Events { after: u64, wait_ms: Option<u64> },
    Action { action: SessionAction },
}

#[derive(Deserialize, Serialize)]
struct ControlResponse {
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    data: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

struct WorkerCommand {
    action: SessionAction,
    response: oneshot::Sender<Result<SessionStatus, String>>,
}

struct RuntimeState {
    metadata: SessionMetadata,
    directory: PathBuf,
    searching_intent: bool,
}

impl RuntimeState {
    fn persist(&self) -> Result<()> {
        write_private_json(&self.directory.join("session.json"), &self.metadata)
    }

    fn public_status(&self) -> SessionStatus {
        self.metadata.status.clone()
    }
}

pub async fn run(command: SessionCommands) -> Result<()> {
    match command {
        SessionCommands::Start => start().await,
        SessionCommands::Find { session_id } => action(session_id, SessionAction::Find).await,
        SessionCommands::Send {
            session_id,
            content,
        } => action(session_id, SessionAction::Message { content }).await,
        SessionCommands::Next { session_id } => action(session_id, SessionAction::Next).await,
        SessionCommands::Leave { session_id } => action(session_id, SessionAction::Leave).await,
        SessionCommands::Cancel { session_id } => action(session_id, SessionAction::Cancel).await,
        SessionCommands::Status { session_id } => status(session_id).await,
        SessionCommands::Events {
            session_id,
            after,
            wait,
        } => events(session_id, after, wait.map(|value| value.into())).await,
        SessionCommands::Attach { session_id, after } => attach(session_id, after).await,
        SessionCommands::Close { session_id } => action(session_id, SessionAction::Close).await,
        SessionCommands::List => list().await,
    }
}

async fn start() -> Result<()> {
    let session_id = Uuid::now_v7();
    let directory = session_directory(session_id)?;
    fs::create_dir_all(&directory).context("create session directory")?;
    set_private_directory(&directory)?;
    let metadata = SessionMetadata {
        session_id,
        token: format!("{}{}", Uuid::now_v7().simple(), Uuid::now_v7().simple()),
        port: 0,
        pid: 0,
        created_at_ms: now_ms(),
        status: SessionStatus::starting(session_id),
    };
    write_private_json(&directory.join("session.json"), &metadata)?;

    let executable = std::env::current_exe().context("locate the Aingle executable")?;
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(directory.join("worker.log"))
        .context("open session worker log")?;
    let mut process = ProcessCommand::new(executable);
    process
        .arg("session-worker")
        .arg(session_id.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::from(log.try_clone()?))
        .stderr(Stdio::from(log));
    configure_detached_process(&mut process);
    let mut child = process.spawn().context("start session worker")?;

    let deadline = tokio::time::Instant::now() + LOCAL_STARTUP_TIMEOUT;
    loop {
        if let Some(exit) = child.try_wait().context("inspect session worker")? {
            bail!("session worker exited during startup with {exit}");
        }
        if let Ok(metadata) = read_metadata(session_id)
            && metadata.port != 0
        {
            let value = control_request(session_id, ControlOperation::Status).await?;
            write_json(&value)?;
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            let _ = child.kill();
            bail!("session worker did not start within the local startup deadline");
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn status(session_id: Uuid) -> Result<()> {
    match control_request(session_id, ControlOperation::Status).await {
        Ok(value) => write_json(&with_worker_reachability(value, true, None)),
        Err(error) => {
            let metadata = read_metadata(session_id).with_context(|| error.to_string())?;
            write_json(&with_worker_reachability(
                json!(metadata.status),
                false,
                Some(error.to_string()),
            ))
        }
    }
}

async fn events(session_id: Uuid, after: u64, wait: Option<Duration>) -> Result<()> {
    let result = control_request(
        session_id,
        ControlOperation::Events {
            after,
            wait_ms: wait.map(duration_millis),
        },
    )
    .await;
    match result {
        Ok(value) => write_json(&value),
        Err(error) => {
            let metadata = read_metadata(session_id).with_context(|| error.to_string())?;
            let found =
                read_events_after(&session_directory(session_id)?.join("events.jsonl"), after)?;
            let next_cursor = found.last().map(|event| event.cursor).unwrap_or(after);
            write_json(&json!({
                "session_id": session_id,
                "status": metadata.status.state,
                "events": found,
                "next_cursor": next_cursor,
                "worker_reachable": false,
                "worker_error": error.to_string(),
            }))
        }
    }
}

async fn action(session_id: Uuid, action: SessionAction) -> Result<()> {
    let closes = matches!(&action, SessionAction::Close);
    match control_request(session_id, ControlOperation::Action { action }).await {
        Ok(value) => write_json(&value),
        Err(error) if closes => {
            let metadata = read_metadata(session_id).with_context(|| error.to_string())?;
            if metadata.status.state == "closed" {
                write_json(&json!(metadata.status))
            } else {
                Err(error)
            }
        }
        Err(error) => Err(error),
    }
}

async fn list() -> Result<()> {
    let root = sessions_root()?;
    let mut sessions = Vec::new();
    if root.exists() {
        for entry in fs::read_dir(root).context("read sessions directory")? {
            let entry = entry?;
            let Ok(session_id) = Uuid::parse_str(&entry.file_name().to_string_lossy()) else {
                continue;
            };
            match control_request(session_id, ControlOperation::Status).await {
                Ok(status) => sessions.push(with_worker_reachability(status, true, None)),
                Err(error) => {
                    let metadata = read_metadata(session_id).ok();
                    if let Some(metadata) = metadata {
                        sessions.push(with_worker_reachability(
                            json!(metadata.status),
                            false,
                            Some(error.to_string()),
                        ));
                    } else {
                        sessions.push(json!({
                            "session_id": session_id,
                            "state": "unreachable",
                            "error": error.to_string(),
                        }));
                    }
                }
            }
        }
    }
    sessions.sort_by_key(|value| value["session_id"].as_str().unwrap_or_default().to_owned());
    write_json(&json!({"sessions": sessions}))
}

async fn attach(session_id: Uuid, mut cursor: u64) -> Result<()> {
    eprintln!("{AGENT_SAFETY_NOTICE}");
    let (input_tx, mut input_rx) = mpsc::channel::<Result<SessionAction, String>>(32);
    tokio::spawn(async move {
        let mut lines = BufReader::new(tokio::io::stdin()).lines();
        loop {
            let line = match lines.next_line().await {
                Ok(Some(line)) => line,
                Ok(None) => return,
                Err(error) => {
                    let _ = input_tx.send(Err(error.to_string())).await;
                    return;
                }
            };
            if line.trim().is_empty() {
                continue;
            }
            let action = serde_json::from_str(&line).map_err(|error| error.to_string());
            if input_tx.send(action).await.is_err() {
                return;
            }
        }
    });

    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => return Ok(()),
            input = input_rx.recv() => {
                let Some(input) = input else { return Ok(()); };
                let action = input.map_err(|error| anyhow!("invalid JSONL input: {error}"))?;
                let closes = matches!(action, SessionAction::Close);
                let value = control_request(session_id, ControlOperation::Action { action }).await?;
                if closes {
                    write_json(&value)?;
                    return Ok(());
                }
            }
            result = control_request(session_id, ControlOperation::Events { after: cursor, wait_ms: Some(1_000) }) => {
                let value = result?;
                if let Some(found) = value["events"].as_array() {
                    for event in found {
                        write_json(event)?;
                    }
                }
                cursor = value["next_cursor"].as_u64().unwrap_or(cursor);
            }
        }
    }
}

pub async fn worker(session_id: Uuid) -> Result<()> {
    let mut metadata = read_metadata(session_id)?;
    let directory = session_directory(session_id)?;
    let listener = TcpListener::bind(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0))
        .await
        .context("bind session control listener")?;
    metadata.port = listener.local_addr()?.port();
    metadata.pid = std::process::id();
    let state = Arc::new(Mutex::new(RuntimeState {
        metadata,
        directory,
        searching_intent: false,
    }));
    state.lock().await.persist()?;

    let notify = Arc::new(Notify::new());
    let (command_tx, command_rx) = mpsc::channel::<WorkerCommand>(64);
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let server = tokio::spawn(control_server(
        listener,
        state.clone(),
        notify.clone(),
        command_tx,
        shutdown_rx,
    ));
    network_loop(state.clone(), notify, command_rx).await;
    let _ = shutdown_tx.send(true);
    let _ = server.await;
    Ok(())
}

async fn control_server(
    listener: TcpListener,
    state: Arc<Mutex<RuntimeState>>,
    notify: Arc<Notify>,
    command_tx: mpsc::Sender<WorkerCommand>,
    mut shutdown: watch::Receiver<bool>,
) {
    loop {
        tokio::select! {
            _ = shutdown.changed() => return,
            accepted = listener.accept() => {
                let Ok((stream, _)) = accepted else { return; };
                tokio::spawn(handle_control(
                    stream,
                    state.clone(),
                    notify.clone(),
                    command_tx.clone(),
                ));
            }
        }
    }
}

async fn handle_control(
    stream: TcpStream,
    state: Arc<Mutex<RuntimeState>>,
    notify: Arc<Notify>,
    command_tx: mpsc::Sender<WorkerCommand>,
) {
    let (reader, mut writer) = stream.into_split();
    let mut line = String::new();
    let response = match BufReader::new(reader).read_line(&mut line).await {
        Ok(0) => response_error("empty control request"),
        Ok(_) if line.len() > MAX_CONTROL_LINE_BYTES => response_error("control request too large"),
        Ok(_) => match serde_json::from_str::<ControlRequest>(&line) {
            Ok(request) => {
                let valid_token = request.token == state.lock().await.metadata.token;
                if !valid_token {
                    response_error("invalid session token")
                } else {
                    execute_control(request.operation, state, notify, command_tx).await
                }
            }
            Err(error) => response_error(&format!("invalid control request: {error}")),
        },
        Err(error) => response_error(&format!("read control request: {error}")),
    };
    if let Ok(mut encoded) = serde_json::to_vec(&response) {
        encoded.push(b'\n');
        let _ = writer.write_all(&encoded).await;
    }
}

async fn execute_control(
    operation: ControlOperation,
    state: Arc<Mutex<RuntimeState>>,
    notify: Arc<Notify>,
    command_tx: mpsc::Sender<WorkerCommand>,
) -> ControlResponse {
    match operation {
        ControlOperation::Status => response_ok(json!(state.lock().await.public_status())),
        ControlOperation::Events { after, wait_ms } => {
            match collect_events(state, notify, after, wait_ms.map(Duration::from_millis)).await {
                Ok(value) => response_ok(value),
                Err(error) => response_error(&format!("{error:#}")),
            }
        }
        ControlOperation::Action { action } => {
            let (response_tx, response_rx) = oneshot::channel();
            if command_tx
                .send(WorkerCommand {
                    action,
                    response: response_tx,
                })
                .await
                .is_err()
            {
                return response_error("session worker is stopping");
            }
            match response_rx.await {
                Ok(Ok(status)) => response_ok(json!(status)),
                Ok(Err(error)) => response_error(&error),
                Err(_) => response_error("session worker stopped before acknowledging the command"),
            }
        }
    }
}

async fn collect_events(
    state: Arc<Mutex<RuntimeState>>,
    notify: Arc<Notify>,
    after: u64,
    wait: Option<Duration>,
) -> Result<Value> {
    let deadline = wait.map(|duration| tokio::time::Instant::now() + duration);
    loop {
        let (session_id, status, path) = {
            let state = state.lock().await;
            (
                state.metadata.session_id,
                state.metadata.status.clone(),
                state.directory.join("events.jsonl"),
            )
        };
        let found = read_events_after(&path, after)?;
        if !found.is_empty() || deadline.is_none() || status.state == "closed" {
            let next_cursor = found.last().map(|event| event.cursor).unwrap_or(after);
            return Ok(json!({
                "session_id": session_id,
                "status": status.state,
                "events": found,
                "next_cursor": next_cursor,
            }));
        }
        let Some(deadline) = deadline else {
            unreachable!()
        };
        if tokio::time::Instant::now() >= deadline {
            return Ok(json!({
                "session_id": session_id,
                "status": status.state,
                "events": [],
                "next_cursor": after,
            }));
        }
        tokio::select! {
            _ = notify.notified() => {}
            _ = tokio::time::sleep_until(deadline) => {}
        }
    }
}

async fn network_loop(
    state: Arc<Mutex<RuntimeState>>,
    notify: Arc<Notify>,
    mut commands: mpsc::Receiver<WorkerCommand>,
) {
    let config = match Config::load() {
        Ok(config) => config,
        Err(error) => {
            set_failed(&state, &notify, error.to_string()).await;
            wait_until_close(&mut commands, &state, &notify).await;
            return;
        }
    };
    let mut backoff_ms = 500_u64;
    loop {
        let identity = match Identity::load_default() {
            Ok(identity) => identity,
            Err(error) => {
                set_failed(&state, &notify, error.to_string()).await;
                wait_until_close(&mut commands, &state, &notify).await;
                return;
            }
        };
        let mut connection = Box::pin(AingleClient::connect(config.clone(), identity));
        let client = loop {
            tokio::select! {
                result = &mut connection => break result,
                command = commands.recv() => {
                    let Some(command) = command else { return; };
                    if handle_unconnected_command(command, &state, &notify).await {
                        return;
                    }
                }
            }
        };
        let mut client = match client {
            Ok(client) => client,
            Err(error) => {
                set_reconnecting(&state, &notify, error.to_string()).await;
                if wait_backoff_or_close(
                    Duration::from_millis(backoff_with_jitter(backoff_ms)),
                    &mut commands,
                    &state,
                    &notify,
                )
                .await
                {
                    return;
                }
                backoff_ms = (backoff_ms * 2).min(30_000);
                continue;
            }
        };
        backoff_ms = 500;

        let disconnected = loop {
            tokio::select! {
                command = commands.recv() => {
                    let Some(command) = command else {
                        let _ = client.send(Command::Close).await;
                        return;
                    };
                    if handle_connected_command(command, &client, &state, &notify).await {
                        return;
                    }
                }
                event = client.next_event() => {
                    match event {
                        Some(Ok(event)) => {
                            let ready = matches!(&event, ChatEvent::Ready { .. });
                            if let Err(error) = record_event(&state, &notify, event).await {
                                set_failed(&state, &notify, format!("record session event: {error:#}")).await;
                                return;
                            }
                            if ready && state.lock().await.searching_intent
                                && let Err(error) = client.send(Command::Find).await
                            {
                                break error.to_string();
                            }
                        }
                        Some(Err(error)) => break error.to_string(),
                        None => break "connection closed".to_owned(),
                    }
                }
            }
        };
        set_reconnecting(&state, &notify, disconnected).await;
        if wait_backoff_or_close(
            Duration::from_millis(backoff_with_jitter(backoff_ms)),
            &mut commands,
            &state,
            &notify,
        )
        .await
        {
            return;
        }
        backoff_ms = (backoff_ms * 2).min(30_000);
    }
}

async fn handle_unconnected_command(
    command: WorkerCommand,
    state: &Arc<Mutex<RuntimeState>>,
    notify: &Arc<Notify>,
) -> bool {
    if matches!(command.action, SessionAction::Close) {
        let status = set_closed(state, notify).await;
        let _ = command.response.send(Ok(status));
        true
    } else {
        let _ = command.response.send(Err(
            "session is not connected; inspect status and events before retrying".into(),
        ));
        false
    }
}

async fn handle_connected_command(
    command: WorkerCommand,
    client: &AingleClient,
    state: &Arc<Mutex<RuntimeState>>,
    notify: &Arc<Notify>,
) -> bool {
    let current_state = state.lock().await.metadata.status.state.clone();
    let operation = match command.action {
        SessionAction::Find if matches!(current_state.as_str(), "ready" | "peer_left") => {
            Ok((Some(Command::Find), None, false))
        }
        SessionAction::Find => Err(format!(
            "find is not valid while session is {current_state}"
        )),
        SessionAction::Message { content } if current_state == "matched" => {
            Ok((Some(Command::Message(content.into_bytes())), None, false))
        }
        SessionAction::Message { .. } => Err(format!(
            "message is not valid while session is {current_state}"
        )),
        SessionAction::Next if current_state == "matched" => {
            Ok((Some(Command::Next), Some("leaving"), false))
        }
        SessionAction::Next => Err(format!(
            "next is not valid while session is {current_state}"
        )),
        SessionAction::Leave if current_state == "searching" => {
            Ok((Some(Command::Cancel), Some("ready"), false))
        }
        SessionAction::Leave if current_state == "matched" => {
            Ok((Some(Command::Leave), Some("leaving"), false))
        }
        SessionAction::Leave => Ok((None, None, false)),
        SessionAction::Cancel if current_state == "searching" => {
            Ok((Some(Command::Cancel), Some("ready"), false))
        }
        SessionAction::Cancel => Ok((None, None, false)),
        SessionAction::Close => Ok((Some(Command::Close), Some("closed"), true)),
    };
    let (network_command, next_state, closes) = match operation {
        Ok(operation) => operation,
        Err(error) => {
            let _ = command.response.send(Err(error));
            return false;
        }
    };
    {
        let mut state = state.lock().await;
        match &network_command {
            Some(Command::Find | Command::Next) => state.searching_intent = true,
            Some(Command::Leave | Command::Cancel | Command::Close) => {
                state.searching_intent = false
            }
            Some(Command::Message(_)) | None => {}
        }
    }
    if let Some(network_command) = network_command
        && let Err(error) = client.send(network_command).await
    {
        let _ = command.response.send(Err(error.to_string()));
        return false;
    }
    let status = if closes {
        set_closed(state, notify).await
    } else if let Some(next_state) = next_state {
        update_state(state, notify, |status| status.state = next_state.into()).await
    } else {
        state.lock().await.public_status()
    };
    let _ = command.response.send(Ok(status));
    closes
}

async fn wait_backoff_or_close(
    duration: Duration,
    commands: &mut mpsc::Receiver<WorkerCommand>,
    state: &Arc<Mutex<RuntimeState>>,
    notify: &Arc<Notify>,
) -> bool {
    let deadline = tokio::time::Instant::now() + duration;
    loop {
        tokio::select! {
            _ = tokio::time::sleep_until(deadline) => return false,
            command = commands.recv() => {
                let Some(command) = command else { return true; };
                if handle_unconnected_command(command, state, notify).await {
                    return true;
                }
            }
        }
    }
}

async fn wait_until_close(
    commands: &mut mpsc::Receiver<WorkerCommand>,
    state: &Arc<Mutex<RuntimeState>>,
    notify: &Arc<Notify>,
) {
    while let Some(command) = commands.recv().await {
        if handle_unconnected_command(command, state, notify).await {
            return;
        }
    }
}

async fn record_event(
    state: &Arc<Mutex<RuntimeState>>,
    notify: &Arc<Notify>,
    event: ChatEvent,
) -> Result<()> {
    let (path, stored) = {
        let mut state = state.lock().await;
        match &event {
            ChatEvent::Searching => state.searching_intent = true,
            ChatEvent::Matched { .. } | ChatEvent::PeerLeft { .. } => {
                state.searching_intent = false
            }
            _ => {}
        }
        let status = &mut state.metadata.status;
        status.last_cursor += 1;
        status.last_error = None;
        match &event {
            ChatEvent::Ready { agent_id } => {
                status.state = "ready".into();
                status.agent_id = Some(agent_id.clone());
            }
            ChatEvent::Searching => status.state = "searching".into(),
            ChatEvent::Matched {
                conversation_id,
                peer_agent_id,
                visibility,
            } => {
                status.state = "matched".into();
                status.conversation_id = Some(*conversation_id);
                status.peer_agent_id = Some(peer_agent_id.clone());
                status.visibility = Some(visibility.clone());
            }
            ChatEvent::PeerLeft { .. } => {
                status.state = "peer_left".into();
                status.conversation_id = None;
                status.peer_agent_id = None;
                status.visibility = None;
            }
            ChatEvent::RateLimited { .. }
            | ChatEvent::ServerBusy { .. }
            | ChatEvent::Error { .. }
            | ChatEvent::Message { .. } => {}
        }
        let stored = StoredEvent {
            cursor: status.last_cursor,
            event,
        };
        state.persist()?;
        (state.directory.join("events.jsonl"), stored)
    };
    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    serde_json::to_writer(&mut file, &stored)?;
    file.write_all(b"\n")?;
    file.flush()?;
    notify.notify_waiters();
    Ok(())
}

async fn set_reconnecting(
    state: &Arc<Mutex<RuntimeState>>,
    notify: &Arc<Notify>,
    error: String,
) -> SessionStatus {
    update_state(state, notify, |status| {
        status.state = "starting".into();
        status.conversation_id = None;
        status.peer_agent_id = None;
        status.visibility = None;
        status.last_error = Some(error);
    })
    .await
}

async fn set_failed(
    state: &Arc<Mutex<RuntimeState>>,
    notify: &Arc<Notify>,
    error: String,
) -> SessionStatus {
    update_state(state, notify, |status| {
        status.state = "failed".into();
        status.last_error = Some(error);
    })
    .await
}

async fn set_closed(state: &Arc<Mutex<RuntimeState>>, notify: &Arc<Notify>) -> SessionStatus {
    update_state(state, notify, |status| {
        status.state = "closed".into();
        status.conversation_id = None;
        status.peer_agent_id = None;
        status.visibility = None;
    })
    .await
}

async fn update_state(
    state: &Arc<Mutex<RuntimeState>>,
    notify: &Arc<Notify>,
    update: impl FnOnce(&mut SessionStatus),
) -> SessionStatus {
    let mut state = state.lock().await;
    update(&mut state.metadata.status);
    if let Err(error) = state.persist() {
        state.metadata.status.last_error = Some(format!("persist session state: {error:#}"));
    }
    let status = state.public_status();
    drop(state);
    notify.notify_waiters();
    status
}

async fn control_request(session_id: Uuid, operation: ControlOperation) -> Result<Value> {
    let metadata = read_metadata(session_id)?;
    if metadata.port == 0 {
        bail!("session worker has not opened its control endpoint");
    }
    let request = ControlRequest {
        token: metadata.token,
        operation,
    };
    let future = async {
        let stream = TcpStream::connect((Ipv4Addr::LOCALHOST, metadata.port))
            .await
            .context("connect to session worker")?;
        let (reader, mut writer) = stream.into_split();
        let mut encoded = serde_json::to_vec(&request)?;
        encoded.push(b'\n');
        writer.write_all(&encoded).await?;
        let mut line = String::new();
        BufReader::new(reader).read_line(&mut line).await?;
        let response: ControlResponse = serde_json::from_str(&line)?;
        if response.ok {
            Ok(response.data.unwrap_or(Value::Null))
        } else {
            Err(anyhow!(
                response
                    .error
                    .unwrap_or_else(|| "session command failed".into())
            ))
        }
    };
    match &request.operation {
        ControlOperation::Events {
            wait_ms: Some(wait_ms),
            ..
        } => tokio::time::timeout(
            Duration::from_millis(*wait_ms) + LOCAL_REQUEST_TIMEOUT,
            future,
        )
        .await
        .map_err(|_| anyhow!("session event wait exceeded its local response deadline"))?,
        _ => tokio::time::timeout(LOCAL_REQUEST_TIMEOUT, future)
            .await
            .map_err(|_| anyhow!("session worker did not respond"))?,
    }
}

fn response_ok(data: Value) -> ControlResponse {
    ControlResponse {
        ok: true,
        data: Some(data),
        error: None,
    }
}

fn response_error(error: &str) -> ControlResponse {
    ControlResponse {
        ok: false,
        data: None,
        error: Some(error.into()),
    }
}

fn with_worker_reachability(mut value: Value, reachable: bool, error: Option<String>) -> Value {
    value["worker_reachable"] = json!(reachable);
    if let Some(error) = error {
        value["worker_error"] = json!(error);
    }
    value
}

fn read_events_after(path: &Path, after: u64) -> Result<Vec<StoredEvent>> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let content = fs::read_to_string(path).context("read session events")?;
    content
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(serde_json::from_str::<StoredEvent>)
        .filter_map(|result| match result {
            Ok(event) if event.cursor > after => Some(Ok(event)),
            Ok(_) => None,
            Err(error) => Some(Err(error.into())),
        })
        .collect()
}

fn read_metadata(session_id: Uuid) -> Result<SessionMetadata> {
    let path = session_directory(session_id)?.join("session.json");
    let content = fs::read_to_string(&path)
        .with_context(|| format!("read session metadata at {}", path.display()))?;
    serde_json::from_str(&content).context("decode session metadata")
}

fn write_private_json(path: &Path, value: &impl Serialize) -> Result<()> {
    let temporary = path.with_extension("tmp");
    fs::write(&temporary, serde_json::to_vec_pretty(value)?)?;
    set_private_file(&temporary)?;
    fs::rename(temporary, path)?;
    Ok(())
}

fn sessions_root() -> Result<PathBuf> {
    Ok(Config::path()?
        .parent()
        .ok_or_else(|| anyhow!("configuration path has no parent"))?
        .join("sessions"))
}

fn session_directory(session_id: Uuid) -> Result<PathBuf> {
    Ok(sessions_root()?.join(session_id.to_string()))
}

fn duration_millis(duration: Duration) -> u64 {
    duration.as_millis().min(u128::from(u64::MAX)) as u64
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u128::from(u64::MAX)) as u64
}

fn backoff_with_jitter(base_ms: u64) -> u64 {
    let jitter = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .subsec_millis() as u64
        % (base_ms / 4 + 1);
    base_ms + jitter
}

#[cfg(unix)]
fn set_private_file(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    Ok(())
}

#[cfg(not(unix))]
fn set_private_file(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(unix)]
fn set_private_directory(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    Ok(())
}

#[cfg(not(unix))]
fn set_private_directory(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(unix)]
fn configure_detached_process(command: &mut ProcessCommand) {
    use std::os::unix::process::CommandExt;
    command.process_group(0);
}

#[cfg(windows)]
fn configure_detached_process(command: &mut ProcessCommand) {
    use std::os::windows::process::CommandExt;
    command.creation_flags(0x0000_0008 | 0x0800_0000);
}

#[cfg(not(any(unix, windows)))]
fn configure_detached_process(_command: &mut ProcessCommand) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_reader_preserves_unbounded_cursor_order() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("events.jsonl");
        let first = StoredEvent {
            cursor: 1,
            event: ChatEvent::Searching,
        };
        let second = StoredEvent {
            cursor: 2,
            event: ChatEvent::PeerLeft {
                final_seq: 4,
                reason: "left".into(),
            },
        };
        fs::write(
            &path,
            format!(
                "{}\n{}\n",
                serde_json::to_string(&first).unwrap(),
                serde_json::to_string(&second).unwrap()
            ),
        )
        .unwrap();

        let found = read_events_after(&path, 1).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].cursor, 2);
    }

    #[test]
    fn wait_duration_is_only_a_control_request_value() {
        assert_eq!(duration_millis(Duration::from_secs(30)), 30_000);
        assert_eq!(SessionStatus::starting(Uuid::nil()).state, "starting");
    }

    #[test]
    fn persisted_status_cannot_look_live() {
        let value = with_worker_reachability(
            json!({"state": "matched"}),
            false,
            Some("worker stopped".into()),
        );

        assert_eq!(value["state"], "matched");
        assert_eq!(value["worker_reachable"], false);
        assert_eq!(value["worker_error"], "worker stopped");
    }
}
