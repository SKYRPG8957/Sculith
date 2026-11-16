//! Metadata-only observations of decoded inbound connection packets.
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConnectionTransport {
    JavaTcp,
    BedrockRaknet,
    BedrockNethernet,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PacketPhase {
    Handshake,
    Status,
    NetworkSettings,
    Login,
    Encryption,
    Configuration,
    ResourcePacks,
    Spawn,
    Play,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConnectionPacket {
    /// None until the Java handshake selects a protocol. Native packets use the
    /// selected codec version, not an unverified version advertised in a body.
    pub protocol: Option<u32>,
    pub phase: PacketPhase,
    pub id: u32,
    pub name: String,
    /// Before body validation. Registration alone does not establish support.
    pub dispatch: crate::PacketDispatch,
    /// Starts at one and spans all phases of this connection.
    pub sequence: u64,
}

impl ConnectionPacket {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.sequence == 0
            || self.id > i32::MAX as u32
            || self.name.is_empty()
            || self.name.len() > 128
            || !self.name.is_ascii()
            || self.name.chars().any(char::is_control)
        {
            return Err("invalid connection packet metadata");
        }
        Ok(())
    }
}

/// IDs are process-local connection IDs, never world player IDs. No address,
/// profile, authentication material or client-supplied body is exposed.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ConnectionEvent {
    Opened {
        connection_id: u64,
        transport: ConnectionTransport,
    },
    PacketReceived {
        connection_id: u64,
        packet: ConnectionPacket,
    },
    /// Emitted only after the world owner activates the player lease.
    PlayerAttached {
        connection_id: u64,
        player_id: u64,
    },
    Closed {
        connection_id: u64,
    },
}
