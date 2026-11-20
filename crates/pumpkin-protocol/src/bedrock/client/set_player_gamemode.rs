// Last verified for v2169

use crate::{
    bedrock::client::GameType,
    serial::{PacketRead, PacketWrite},
};
use pumpkin_macros::packet;

/// Updates the player's active game mode (e.g. Survival, Creative, Spectator).
#[derive(PacketRead, PacketWrite)]
#[packet(pumpkin_data::bedrock_packet::SET_PLAYER_GAME_TYPE)]
pub struct CSetPlayerGameType {
    /// Target game mode to apply.
    pub player_game_type: GameType,
}
