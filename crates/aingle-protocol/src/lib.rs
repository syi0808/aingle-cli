use std::borrow::Cow;

use thiserror::Error;
use uuid::Uuid;

pub const MAGIC: &[u8; 4] = b"AING";
pub const VERSION: u16 = 1;
pub const MAX_MESSAGE_SIZE: usize = 16 * 1024;

pub mod opcode {
    pub const FIND: u8 = 0x01;
    pub const CANCEL: u8 = 0x02;
    pub const MESSAGE: u8 = 0x03;
    pub const LEAVE: u8 = 0x04;
    pub const PING: u8 = 0x05;
    pub const CLOSE: u8 = 0x06;

    pub const READY: u8 = 0x10;
    pub const SEARCHING: u8 = 0x11;
    pub const MATCHED: u8 = 0x12;
    pub const SERVER_MESSAGE: u8 = 0x13;
    pub const PEER_LEFT: u8 = 0x14;
    pub const RATE_LIMITED: u8 = 0x15;
    pub const SERVER_BUSY: u8 = 0x16;
    pub const ERROR: u8 = 0x17;
    pub const PONG: u8 = 0x18;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Visibility {
    Public = 0,
    Unlisted = 1,
    Private = 2,
}

impl TryFrom<u8> for Visibility {
    type Error = ProtocolError;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(Self::Public),
            1 => Ok(Self::Unlisted),
            2 => Ok(Self::Private),
            _ => Err(ProtocolError::InvalidField("visibility")),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum EndReason {
    Left = 0,
    Next = 1,
    Disconnected = 2,
    Timeout = 3,
    ProtocolError = 4,
}

impl TryFrom<u8> for EndReason {
    type Error = ProtocolError;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(Self::Left),
            1 => Ok(Self::Next),
            2 => Ok(Self::Disconnected),
            3 => Ok(Self::Timeout),
            4 => Ok(Self::ProtocolError),
            _ => Err(ProtocolError::InvalidField("end_reason")),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClientFrame<'a> {
    Find,
    Cancel,
    Message(&'a [u8]),
    Leave,
    Ping(u64),
    Close,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServerFrame<'a> {
    Ready {
        agent_id: Cow<'a, str>,
    },
    Searching,
    Matched {
        conversation_id: Uuid,
        peer_agent_id: Cow<'a, str>,
        visibility: Visibility,
    },
    Message {
        seq: u64,
        sender: u8,
        payload: &'a [u8],
    },
    PeerLeft {
        final_seq: u64,
        reason: EndReason,
    },
    RateLimited {
        retry_after_ms: u32,
    },
    ServerBusy {
        retry_after_ms: u32,
    },
    Error {
        code: u16,
        message: Cow<'a, str>,
    },
    Pong(u64),
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ProtocolError {
    #[error("empty frame")]
    Empty,
    #[error("unknown opcode {0:#04x}")]
    UnknownOpcode(u8),
    #[error("invalid frame length")]
    InvalidLength,
    #[error("invalid UTF-8")]
    InvalidUtf8,
    #[error("invalid {0}")]
    InvalidField(&'static str),
    #[error("message exceeds {MAX_MESSAGE_SIZE} bytes")]
    MessageTooLarge,
    #[error("incompatible protocol version {0}")]
    IncompatibleVersion(u16),
}

pub fn client_hello(flags: u16) -> [u8; 8] {
    let mut bytes = [0_u8; 8];
    bytes[..4].copy_from_slice(MAGIC);
    bytes[4..6].copy_from_slice(&VERSION.to_be_bytes());
    bytes[6..8].copy_from_slice(&flags.to_be_bytes());
    bytes
}

pub fn decode_hello(bytes: &[u8]) -> Result<u16, ProtocolError> {
    if bytes.len() != 8 || &bytes[..4] != MAGIC {
        return Err(ProtocolError::InvalidLength);
    }
    let version = u16::from_be_bytes([bytes[4], bytes[5]]);
    if version != VERSION {
        return Err(ProtocolError::IncompatibleVersion(version));
    }
    Ok(u16::from_be_bytes([bytes[6], bytes[7]]))
}

pub fn encode_client(frame: ClientFrame<'_>) -> Result<Vec<u8>, ProtocolError> {
    let mut output = Vec::new();
    match frame {
        ClientFrame::Find => output.push(opcode::FIND),
        ClientFrame::Cancel => output.push(opcode::CANCEL),
        ClientFrame::Message(payload) => {
            if payload.len() > MAX_MESSAGE_SIZE {
                return Err(ProtocolError::MessageTooLarge);
            }
            std::str::from_utf8(payload).map_err(|_| ProtocolError::InvalidUtf8)?;
            output.push(opcode::MESSAGE);
            output.extend_from_slice(payload);
        }
        ClientFrame::Leave => output.push(opcode::LEAVE),
        ClientFrame::Ping(value) => {
            output.push(opcode::PING);
            output.extend_from_slice(&value.to_be_bytes());
        }
        ClientFrame::Close => output.push(opcode::CLOSE),
    }
    Ok(output)
}

pub fn decode_client(bytes: &[u8]) -> Result<ClientFrame<'_>, ProtocolError> {
    let (&op, payload) = bytes.split_first().ok_or(ProtocolError::Empty)?;
    match op {
        opcode::FIND if payload.is_empty() => Ok(ClientFrame::Find),
        opcode::CANCEL if payload.is_empty() => Ok(ClientFrame::Cancel),
        opcode::MESSAGE if payload.len() <= MAX_MESSAGE_SIZE => {
            std::str::from_utf8(payload).map_err(|_| ProtocolError::InvalidUtf8)?;
            Ok(ClientFrame::Message(payload))
        }
        opcode::MESSAGE => Err(ProtocolError::MessageTooLarge),
        opcode::LEAVE if payload.is_empty() => Ok(ClientFrame::Leave),
        opcode::PING if payload.len() == 8 => Ok(ClientFrame::Ping(read_u64(payload))),
        opcode::CLOSE if payload.is_empty() => Ok(ClientFrame::Close),
        opcode::FIND | opcode::CANCEL | opcode::LEAVE | opcode::PING | opcode::CLOSE => {
            Err(ProtocolError::InvalidLength)
        }
        _ => Err(ProtocolError::UnknownOpcode(op)),
    }
}

pub fn encode_server(frame: ServerFrame<'_>) -> Result<Vec<u8>, ProtocolError> {
    let mut output = Vec::new();
    match frame {
        ServerFrame::Ready { agent_id } => {
            output.push(opcode::READY);
            put_short_string(&mut output, &agent_id)?;
        }
        ServerFrame::Searching => output.push(opcode::SEARCHING),
        ServerFrame::Matched {
            conversation_id,
            peer_agent_id,
            visibility,
        } => {
            output.push(opcode::MATCHED);
            output.extend_from_slice(conversation_id.as_bytes());
            output.push(visibility as u8);
            put_short_string(&mut output, &peer_agent_id)?;
        }
        ServerFrame::Message {
            seq,
            sender,
            payload,
        } => {
            if payload.len() > MAX_MESSAGE_SIZE {
                return Err(ProtocolError::MessageTooLarge);
            }
            if sender > 1 {
                return Err(ProtocolError::InvalidField("sender"));
            }
            std::str::from_utf8(payload).map_err(|_| ProtocolError::InvalidUtf8)?;
            output.push(opcode::SERVER_MESSAGE);
            output.extend_from_slice(&seq.to_be_bytes());
            output.push(sender);
            output.extend_from_slice(payload);
        }
        ServerFrame::PeerLeft { final_seq, reason } => {
            output.push(opcode::PEER_LEFT);
            output.extend_from_slice(&final_seq.to_be_bytes());
            output.push(reason as u8);
        }
        ServerFrame::RateLimited { retry_after_ms } => {
            output.push(opcode::RATE_LIMITED);
            output.extend_from_slice(&retry_after_ms.to_be_bytes());
        }
        ServerFrame::ServerBusy { retry_after_ms } => {
            output.push(opcode::SERVER_BUSY);
            output.extend_from_slice(&retry_after_ms.to_be_bytes());
        }
        ServerFrame::Error { code, message } => {
            output.push(opcode::ERROR);
            output.extend_from_slice(&code.to_be_bytes());
            output.extend_from_slice(message.as_bytes());
        }
        ServerFrame::Pong(value) => {
            output.push(opcode::PONG);
            output.extend_from_slice(&value.to_be_bytes());
        }
    }
    Ok(output)
}

pub fn decode_server(bytes: &[u8]) -> Result<ServerFrame<'_>, ProtocolError> {
    let (&op, payload) = bytes.split_first().ok_or(ProtocolError::Empty)?;
    match op {
        opcode::READY => Ok(ServerFrame::Ready {
            agent_id: Cow::Borrowed(read_short_string(payload)?),
        }),
        opcode::SEARCHING if payload.is_empty() => Ok(ServerFrame::Searching),
        opcode::MATCHED if payload.len() >= 18 => {
            let conversation_id = Uuid::from_slice(&payload[..16])
                .map_err(|_| ProtocolError::InvalidField("conversation_id"))?;
            let visibility = Visibility::try_from(payload[16])?;
            let peer_agent_id = read_short_string(&payload[17..])?;
            Ok(ServerFrame::Matched {
                conversation_id,
                peer_agent_id: Cow::Borrowed(peer_agent_id),
                visibility,
            })
        }
        opcode::SERVER_MESSAGE
            if payload.len() >= 9 && payload.len() - 9 <= MAX_MESSAGE_SIZE && payload[8] <= 1 =>
        {
            std::str::from_utf8(&payload[9..]).map_err(|_| ProtocolError::InvalidUtf8)?;
            Ok(ServerFrame::Message {
                seq: read_u64(payload),
                sender: payload[8],
                payload: &payload[9..],
            })
        }
        opcode::PEER_LEFT if payload.len() == 9 => Ok(ServerFrame::PeerLeft {
            final_seq: read_u64(payload),
            reason: EndReason::try_from(payload[8])?,
        }),
        opcode::RATE_LIMITED if payload.len() == 4 => Ok(ServerFrame::RateLimited {
            retry_after_ms: read_u32(payload),
        }),
        opcode::SERVER_BUSY if payload.len() == 4 => Ok(ServerFrame::ServerBusy {
            retry_after_ms: read_u32(payload),
        }),
        opcode::ERROR if payload.len() >= 2 => Ok(ServerFrame::Error {
            code: u16::from_be_bytes([payload[0], payload[1]]),
            message: Cow::Borrowed(
                std::str::from_utf8(&payload[2..]).map_err(|_| ProtocolError::InvalidUtf8)?,
            ),
        }),
        opcode::PONG if payload.len() == 8 => Ok(ServerFrame::Pong(read_u64(payload))),
        opcode::SEARCHING
        | opcode::MATCHED
        | opcode::SERVER_MESSAGE
        | opcode::PEER_LEFT
        | opcode::RATE_LIMITED
        | opcode::SERVER_BUSY
        | opcode::ERROR
        | opcode::PONG => Err(ProtocolError::InvalidLength),
        _ => Err(ProtocolError::UnknownOpcode(op)),
    }
}

fn put_short_string(output: &mut Vec<u8>, value: &str) -> Result<(), ProtocolError> {
    let length = u8::try_from(value.len()).map_err(|_| ProtocolError::InvalidLength)?;
    output.push(length);
    output.extend_from_slice(value.as_bytes());
    Ok(())
}

fn read_short_string(bytes: &[u8]) -> Result<&str, ProtocolError> {
    let (&length, value) = bytes.split_first().ok_or(ProtocolError::InvalidLength)?;
    if value.len() != usize::from(length) {
        return Err(ProtocolError::InvalidLength);
    }
    std::str::from_utf8(value).map_err(|_| ProtocolError::InvalidUtf8)
}

fn read_u64(bytes: &[u8]) -> u64 {
    u64::from_be_bytes(bytes[..8].try_into().expect("validated length"))
}

fn read_u32(bytes: &[u8]) -> u32 {
    u32::from_be_bytes(bytes[..4].try_into().expect("validated length"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hello_round_trip() {
        assert_eq!(decode_hello(&client_hello(7)), Ok(7));
    }

    #[test]
    fn client_message_round_trip() {
        let bytes = encode_client(ClientFrame::Message(b"hello")).unwrap();
        assert_eq!(decode_client(&bytes), Ok(ClientFrame::Message(b"hello")));
    }

    #[test]
    fn matched_round_trip() {
        let id = Uuid::now_v7();
        let frame = ServerFrame::Matched {
            conversation_id: id,
            peer_agent_id: Cow::Borrowed("agent_peer"),
            visibility: Visibility::Public,
        };
        let bytes = encode_server(frame.clone()).unwrap();
        assert_eq!(decode_server(&bytes), Ok(frame));
    }

    #[test]
    fn rejects_oversized_message() {
        let payload = vec![0; MAX_MESSAGE_SIZE + 1];
        assert_eq!(
            encode_client(ClientFrame::Message(&payload)),
            Err(ProtocolError::MessageTooLarge)
        );
    }

    #[test]
    fn server_message_round_trip() {
        let frame = ServerFrame::Message {
            seq: 42,
            sender: 1,
            payload: b"remote",
        };
        let bytes = encode_server(frame.clone()).unwrap();
        assert_eq!(decode_server(&bytes), Ok(frame));
    }

    #[test]
    fn rejects_non_utf8_message() {
        assert_eq!(
            decode_client(&[opcode::MESSAGE, 0xff]),
            Err(ProtocolError::InvalidUtf8)
        );
    }
}
