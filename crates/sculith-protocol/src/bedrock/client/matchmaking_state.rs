use pumpkin_macros::packet;

use crate::serial::{PacketRead, PacketWrite};

/// Matchmaking state reported to the client.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PacketWrite)]
#[repr(u8)]
pub enum MatchmakingState {
    Idle = 0,
    Matchmaking = 1,
    MatchFound = 2,
    Canceled = 3,
    PlayerLeftParty = 4,
    PlayerLeftServer = 5,
    ServerShutdown = 6,
    TimedOut = 7,
    RequeueAsParty = 8,
}

/// Optional details regarding the matchmaking transition.
#[derive(Clone, Debug, PartialEq, Eq, PacketRead, PacketWrite)]
pub struct MatchmakingStateOptions {
    pub triggering_player_name: Option<String>,
    pub triggered_by_local_player: Option<bool>,
}

/// Sent by the server to inform the client of matchmaking status.
#[derive(Clone, Debug, PartialEq, Eq, PacketRead, PacketWrite)]
#[packet(pumpkin_data::bedrock_packet::CLIENTBOUND_MATCHMAKING_STATE)]
pub struct CMatchmakingState {
    pub state: u8,
    pub destination_name: String,
    pub options: Option<MatchmakingStateOptions>,
}
