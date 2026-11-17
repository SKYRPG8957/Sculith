#![allow(clippy::unwrap_used)]

use std::collections::BTreeMap;

use pumpkin_protocol::{
    Packet,
    bedrock::{
        client::{
            AudioContentSound, CMatchmakingState, CPlayAudioContent, CSetPassengerOfBlock,
            CStonecutterSetRecipe, MatchmakingStateOptions, PassengerOfBlockData,
        },
        server::{
            SActorPickRequest, SMatchmakingCancel, SRegisterAudioContent, SStonecutterSetRecipe,
        },
    },
    codec::{var_int::VarInt, var_long::VarLong, var_uint::VarUInt},
    serial::{PacketRead, PacketWrite},
};
use pumpkin_util::math::{position::BlockPos, vector3::Vector3};

fn fixtures() -> BTreeMap<String, Vec<u8>> {
    serde_json::from_str(include_str!("fixtures/bedrock_controls_2225.json")).unwrap()
}

fn read_fixture<P: Packet + PacketRead>(bytes: &[u8]) -> P {
    let mut reader = bytes;
    assert_eq!(VarUInt::read(&mut reader).unwrap().0, P::PACKET_ID as u32);
    let packet = P::read(&mut reader).unwrap();
    assert!(reader.is_empty(), "unread packet fields: {reader:?}");
    packet
}

fn write_fixture<P: Packet + PacketWrite>(packet: &P, expected: &[u8]) {
    let mut bytes = Vec::new();
    VarUInt(P::PACKET_ID as u32).write(&mut bytes).unwrap();
    packet.write(&mut bytes).unwrap();
    assert_eq!(bytes, expected);
}

#[test]
fn stonecutter_frames_match_independent_2225() {
    let fixtures = fixtures();
    let packet: SStonecutterSetRecipe = read_fixture(&fixtures["stonecutter_request"]);
    assert_eq!(packet.container_id, 12);
    assert_eq!(packet.recipe_index, VarInt(5));
    write_fixture(
        &CStonecutterSetRecipe {
            player_unique_id: VarLong(42),
            container_id: 12,
            recipe_index: VarInt(5),
        },
        &fixtures["stonecutter_response"],
    );
}

#[test]
fn actor_pick_uses_a_fixed_signed_id_in_2225() {
    let bytes = include_bytes!("fixtures/actor_pick_2225.bin");
    let packet: SActorPickRequest = read_fixture(bytes);
    assert_eq!(packet.entity_unique_id, -0x0102_0304_0506_0708);
    assert_eq!(packet.max_slots, 9);
    assert!(packet.with_data);

    let mut payload = bytes.as_slice();
    VarUInt::read(&mut payload).unwrap();
    for end in 0..payload.len() {
        assert!(SActorPickRequest::read(&mut &payload[..end]).is_err());
    }
}

#[test]
fn matchmaking_options_match_independent_2225() {
    let fixtures = fixtures();
    let _: SMatchmakingCancel = read_fixture(&fixtures["matchmaking_cancel"]);
    write_fixture(
        &CMatchmakingState {
            state: 2,
            destination_name: "fixture".into(),
            options: Some(MatchmakingStateOptions {
                triggering_player_name: Some("Player".into()),
                triggered_by_local_player: Some(true),
            }),
        },
        &fixtures["matchmaking_state"],
    );
}

#[test]
fn signed_audio_fields_match_independent_2225() {
    let fixtures = fixtures();
    let packet: SRegisterAudioContent = read_fixture(&fixtures["register_audio"]);
    assert_eq!(packet.registrations.len(), 2);
    let first = &packet.registrations[0];
    assert_eq!(first.audio_content_id, "first");
    assert_eq!(first.shared_metadata, "metadata");
    assert_eq!(first.server_content, "server");
    assert_eq!(first.playback_content, "playback");
    assert_eq!(packet.registrations[1].audio_content_id, "second");
    assert_eq!(packet.registrations[1].playback_content, "p2");
    write_fixture(
        &CPlayAudioContent {
            shared_metadata: "metadata".into(),
            playback_content: "playback".into(),
            playback_type: "Sound".into(),
            sound: AudioContentSound {
                sound_name: "fixture.sound".into(),
                position: BlockPos::new(-10, -484, 31),
                volume: 2.5,
                pitch: 0.75,
                loop_count: VarInt(2),
                bypass_listener_range_check: true,
                handle: Some(42),
                playback_position_seconds: Some(1.25),
            },
        },
        &fixtures["play_audio"],
    );
    // Every truncated registration must fail, including the fourth string.
    let mut bytes = fixtures["register_audio"].as_slice();
    VarUInt::read(&mut bytes).unwrap();
    for end in 0..bytes.len() {
        assert!(SRegisterAudioContent::read(&mut &bytes[..end]).is_err());
    }
    let mut oversized = Vec::new();
    VarUInt(1025).write(&mut oversized).unwrap();
    assert!(SRegisterAudioContent::read(&mut oversized.as_slice()).is_err());
}

#[test]
fn passenger_fields_match_independent_2225() {
    write_fixture(
        &CSetPassengerOfBlock {
            passenger_unique_id: VarLong(42),
            passenger_of_block: Some(PassengerOfBlockData {
                block_position: BlockPos::new(-1, -60, 3),
                offset: Vector3::new(0.25, -0.5, 1.25),
                rotation: 90.0,
                rotation_limit: 180.0,
                emote_type: 1,
            }),
        },
        &fixtures()["passenger_of_block"],
    );
}
