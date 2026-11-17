// Last verified for v2208

use pumpkin_macros::packet;

/// The client's movement prediction state (actor flags, bounding box, movement attributes,
/// flying). Only sent after server-authoritative movement corrections, so its body is not read.
#[packet(pumpkin_data::bedrock_packet::CLIENT_MOVEMENT_PREDICTION_SYNC)]
pub struct SClientMovementPredictionSync;
