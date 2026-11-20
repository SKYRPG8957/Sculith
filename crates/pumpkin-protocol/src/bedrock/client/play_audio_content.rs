use pumpkin_macros::packet;
use pumpkin_util::math::position::BlockPos;

use crate::{
    codec::var_int::VarInt,
    serial::{PacketRead, PacketWrite},
};

/// Sound playback options for signed audio content.
#[derive(Clone, Debug, PartialEq, PacketRead, PacketWrite)]
pub struct AudioContentSound {
    pub sound_name: String,
    pub position: BlockPos,
    pub volume: f32,
    pub pitch: f32,
    pub loop_count: VarInt,
    pub bypass_listener_range_check: bool,
    pub handle: Option<u64>,
    pub playback_position_seconds: Option<f32>,
}

/// Sent by the server to instruct the client to play signed audio content.
#[derive(Clone, Debug, PartialEq, PacketRead, PacketWrite)]
#[packet(pumpkin_data::bedrock_packet::CLIENTBOUND_PLAY_AUDIO_CONTENT)]
pub struct CPlayAudioContent {
    pub shared_metadata: String,
    pub playback_content: String,
    pub playback_type: String,
    pub sound: AudioContentSound,
}
