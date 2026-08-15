mod auth;
mod config;
mod history;
mod identity;

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use aingle_protocol::{
    ClientFrame, EndReason, ServerFrame, Visibility, client_hello, decode_server, encode_client,
};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::mpsc;
use tokio_tungstenite::{
    connect_async,
    tungstenite::{Message, client::IntoClientRequest},
};
use uuid::Uuid;

pub use auth::{AuthClient, Session};
pub use config::Config;
pub use history::{ConversationSummary, HistoryStore, StoredMessage};
pub use identity::Identity;

#[derive(Debug, Error)]
pub enum ClientError {
    #[error("configuration error: {0}")]
    Config(String),
    #[error("authentication error: {0}")]
    Authentication(String),
    #[error("network error: {0}")]
    Network(#[from] tokio_tungstenite::tungstenite::Error),
    #[error("protocol error: {0}")]
    Protocol(#[from] aingle_protocol::ProtocolError),
    #[error("local history error: {0}")]
    History(#[from] history::HistoryError),
    #[error("client command queue is closed")]
    Closed,
}

#[derive(Debug)]
pub enum Command {
    Find,
    Cancel,
    Message(Vec<u8>),
    Leave,
    Next,
    Close,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ChatEvent {
    Ready {
        agent_id: String,
    },
    Searching,
    Matched {
        conversation_id: Uuid,
        peer_agent_id: String,
        visibility: String,
    },
    Message {
        seq: u64,
        sender: String,
        content: String,
    },
    PeerLeft {
        final_seq: u64,
        reason: String,
    },
    RateLimited {
        retry_after_ms: u32,
    },
    ServerBusy {
        retry_after_ms: u32,
    },
    Error {
        code: u16,
        message: String,
    },
}

pub struct AingleClient {
    command_tx: mpsc::Sender<Command>,
    event_rx: mpsc::Receiver<Result<ChatEvent, ClientError>>,
}

enum HistoryCommand {
    Begin(Uuid, String, Visibility),
    Append(Uuid, u64, u8, u64, Vec<u8>),
    Finish(Uuid, u64),
}

#[derive(Clone)]
pub struct ClientHandle {
    command_tx: mpsc::Sender<Command>,
}

impl AingleClient {
    pub async fn connect(config: Config, identity: Identity) -> Result<Self, ClientError> {
        let session = AuthClient::new(config.api_url.clone())
            .authenticate(&identity)
            .await
            .map_err(|error| ClientError::Authentication(error.to_string()))?;
        Self::connect_with_session(config, session).await
    }

    pub async fn connect_with_session(
        config: Config,
        session: Session,
    ) -> Result<Self, ClientError> {
        let mut request = config.websocket_url.as_str().into_client_request()?;
        request.headers_mut().insert(
            "authorization",
            format!("Bearer {}", session.token)
                .parse()
                .map_err(|_| ClientError::Config("invalid session token".into()))?,
        );
        let (socket, _) = connect_async(request).await?;
        let (mut writer, mut reader) = socket.split();
        writer
            .send(Message::Binary(client_hello(0).to_vec().into()))
            .await?;

        let (command_tx, mut command_rx) = mpsc::channel::<Command>(64);
        let (event_tx, event_rx) = mpsc::channel::<Result<ChatEvent, ClientError>>(128);
        let history = config
            .history_dir
            .as_ref()
            .map(HistoryStore::open)
            .transpose()?;
        let history_tx = history.map(|store| {
            let (history_tx, mut history_rx) = mpsc::channel::<HistoryCommand>(128);
            tokio::task::spawn_blocking(move || {
                while let Some(command) = history_rx.blocking_recv() {
                    let result = match command {
                        HistoryCommand::Begin(id, peer, visibility) => {
                            store.begin(id, &peer, visibility)
                        }
                        HistoryCommand::Append(id, seq, sender, timestamp, payload) => {
                            store.append(id, seq, sender, timestamp, &payload)
                        }
                        HistoryCommand::Finish(id, final_seq) => store.finish(id, final_seq),
                    };
                    if let Err(error) = result {
                        tracing::error!(%error, "local history write failed");
                    }
                }
            });
            history_tx
        });

        tokio::spawn(async move {
            let mut heartbeat = tokio::time::interval(Duration::from_secs(30));
            loop {
                let command = tokio::select! {
                    command = command_rx.recv() => match command { Some(command) => command, None => return },
                    _ = heartbeat.tick() => {
                        let timestamp = now_millis();
                        if let Ok(bytes) = encode_client(ClientFrame::Ping(timestamp)) {
                            if writer.send(Message::Binary(bytes.into())).await.is_err() { return; }
                        }
                        continue;
                    }
                };
                let frames = match command {
                    Command::Find => vec![ClientFrame::Find],
                    Command::Cancel => vec![ClientFrame::Cancel],
                    Command::Message(ref content) => vec![ClientFrame::Message(content)],
                    Command::Leave => vec![ClientFrame::Leave],
                    Command::Next => vec![ClientFrame::Leave, ClientFrame::Find],
                    Command::Close => vec![ClientFrame::Close],
                };
                for frame in frames {
                    match encode_client(frame) {
                        Ok(bytes) => {
                            if writer.send(Message::Binary(bytes.into())).await.is_err() {
                                return;
                            }
                        }
                        Err(_) => return,
                    }
                }
            }
        });

        tokio::spawn(async move {
            let mut active: Option<(Uuid, String)> = None;
            loop {
                let result = match reader.next().await {
                    Some(Ok(Message::Binary(bytes))) => {
                        map_event(&bytes, &mut active, history_tx.as_ref())
                    }
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Ok(_)) => continue,
                    Some(Err(error)) => Err(ClientError::Network(error)),
                };
                match result {
                    Ok(Some(event)) => {
                        if event_tx.try_send(Ok(event)).is_err() {
                            break;
                        }
                    }
                    Ok(None) => continue,
                    Err(error) => {
                        if event_tx.try_send(Err(error)).is_err() {
                            break;
                        }
                    }
                }
            }
        });

        Ok(Self {
            command_tx,
            event_rx,
        })
    }

    pub async fn connect_resilient(
        config: Config,
        identity: Identity,
    ) -> Result<Self, ClientError> {
        let identity_path = identity.path().to_path_buf();
        let initial = Self::connect(config.clone(), identity).await?;
        let (command_tx, command_rx) = mpsc::channel::<Command>(64);
        let (event_tx, event_rx) = mpsc::channel::<Result<ChatEvent, ClientError>>(128);
        tokio::spawn(resilient_loop(
            initial,
            config,
            identity_path,
            command_rx,
            event_tx,
        ));
        Ok(Self {
            command_tx,
            event_rx,
        })
    }

    pub async fn send(&self, command: Command) -> Result<(), ClientError> {
        self.command_tx
            .send(command)
            .await
            .map_err(|_| ClientError::Closed)
    }

    pub fn handle(&self) -> ClientHandle {
        ClientHandle {
            command_tx: self.command_tx.clone(),
        }
    }

    pub async fn next_event(&mut self) -> Option<Result<ChatEvent, ClientError>> {
        self.event_rx.recv().await
    }
}

async fn resilient_loop(
    mut client: AingleClient,
    config: Config,
    identity_path: std::path::PathBuf,
    mut command_rx: mpsc::Receiver<Command>,
    event_tx: mpsc::Sender<Result<ChatEvent, ClientError>>,
) {
    let mut should_find = false;
    let mut backoff_ms = 500_u64;
    loop {
        let disconnected = loop {
            tokio::select! {
                command = command_rx.recv() => {
                    let Some(command) = command else { return };
                    match command {
                        Command::Find | Command::Next => should_find = true,
                        Command::Leave => should_find = false,
                        Command::Close => {
                            let _ = client.send(Command::Close).await;
                            return;
                        }
                        _ => {}
                    }
                    if let Err(error) = client.send(command).await {
                        if event_tx.try_send(Err(error)).is_err() { return; }
                        break true;
                    }
                }
                event = client.next_event() => {
                    match event {
                        Some(Ok(event)) => {
                            match &event {
                                ChatEvent::Searching | ChatEvent::Matched { .. } => should_find = true,
                                ChatEvent::PeerLeft { .. } => should_find = false,
                                _ => {}
                            }
                            if event_tx.try_send(Ok(event)).is_err() { return; }
                        }
                        Some(Err(error)) => {
                            if event_tx.try_send(Err(error)).is_err() { return; }
                            break true;
                        }
                        None => break true,
                    }
                }
            }
        };
        if !disconnected {
            return;
        }

        loop {
            tokio::time::sleep(Duration::from_millis(backoff_with_jitter(backoff_ms))).await;
            let identity = match Identity::load(&identity_path) {
                Ok(identity) => identity,
                Err(error) => {
                    let _ = event_tx.try_send(Err(error));
                    return;
                }
            };
            match AingleClient::connect(config.clone(), identity).await {
                Ok(reconnected) => {
                    client = reconnected;
                    if should_find && client.send(Command::Find).await.is_err() {
                        continue;
                    }
                    backoff_ms = 500;
                    break;
                }
                Err(error) => {
                    tracing::warn!(%error, retry_after_ms = backoff_ms, "reconnect failed");
                    backoff_ms = (backoff_ms * 2).min(30_000);
                }
            }
        }
    }
}

fn backoff_with_jitter(base_ms: u64) -> u64 {
    let jitter = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .subsec_millis() as u64
        % (base_ms / 4 + 1);
    base_ms + jitter
}

impl ClientHandle {
    pub async fn send(&self, command: Command) -> Result<(), ClientError> {
        self.command_tx
            .send(command)
            .await
            .map_err(|_| ClientError::Closed)
    }
}

fn map_event(
    bytes: &[u8],
    active: &mut Option<(Uuid, String)>,
    history: Option<&mpsc::Sender<HistoryCommand>>,
) -> Result<Option<ChatEvent>, ClientError> {
    let event = match decode_server(bytes)? {
        ServerFrame::Ready { agent_id } => ChatEvent::Ready {
            agent_id: agent_id.into_owned(),
        },
        ServerFrame::Searching => ChatEvent::Searching,
        ServerFrame::Matched {
            conversation_id,
            peer_agent_id,
            visibility,
        } => {
            let peer_agent_id = peer_agent_id.into_owned();
            history_send(
                history,
                HistoryCommand::Begin(conversation_id, peer_agent_id.clone(), visibility),
            )?;
            *active = Some((conversation_id, peer_agent_id.clone()));
            ChatEvent::Matched {
                conversation_id,
                peer_agent_id,
                visibility: visibility_name(visibility).into(),
            }
        }
        ServerFrame::Message {
            seq,
            sender,
            payload,
        } => {
            if let Some((conversation_id, _)) = active.as_ref() {
                history_send(
                    history,
                    HistoryCommand::Append(
                        *conversation_id,
                        seq,
                        sender,
                        now_millis(),
                        payload.to_vec(),
                    ),
                )?;
            }
            ChatEvent::Message {
                seq,
                sender: if sender == 0 { "self" } else { "peer" }.into(),
                content: String::from_utf8_lossy(payload).into_owned(),
            }
        }
        ServerFrame::PeerLeft { final_seq, reason } => {
            if let Some((conversation_id, _)) = active.as_ref() {
                history_send(history, HistoryCommand::Finish(*conversation_id, final_seq))?;
            }
            *active = None;
            ChatEvent::PeerLeft {
                final_seq,
                reason: reason_name(reason).into(),
            }
        }
        ServerFrame::RateLimited { retry_after_ms } => ChatEvent::RateLimited { retry_after_ms },
        ServerFrame::ServerBusy { retry_after_ms } => ChatEvent::ServerBusy { retry_after_ms },
        ServerFrame::Error { code, message } => ChatEvent::Error {
            code,
            message: message.into_owned(),
        },
        ServerFrame::Pong(_) => return Ok(None),
    };
    Ok(Some(event))
}

fn history_send(
    history: Option<&mpsc::Sender<HistoryCommand>>,
    command: HistoryCommand,
) -> Result<(), ClientError> {
    match history {
        Some(sender) => sender.try_send(command).map_err(|_| ClientError::Closed),
        None => Ok(()),
    }
}

fn visibility_name(value: Visibility) -> &'static str {
    match value {
        Visibility::Public => "public",
        Visibility::Unlisted => "unlisted",
        Visibility::Private => "private",
    }
}

fn reason_name(value: EndReason) -> &'static str {
    match value {
        EndReason::Left => "left",
        EndReason::Next => "next",
        EndReason::Disconnected => "disconnected",
        EndReason::Timeout => "timeout",
        EndReason::ProtocolError => "protocol_error",
    }
}

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[cfg(test)]
mod tests {
    use super::map_event;

    #[test]
    fn heartbeat_pong_is_consumed_without_an_event() {
        let mut active = None;
        let pong = [0x18, 0, 0, 0, 0, 0, 0, 0, 42];

        assert!(map_event(&pong, &mut active, None).unwrap().is_none());
    }
}
