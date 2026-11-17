use std::sync::LazyLock;

use pumpkin_plugin_api::{
    events::{EventData, PacketSentEvent},
    wit::pumpkin::plugin::{player::Player, sounds::SoundCategory},
};

static JAVA_PACKETS: LazyLock<serde_json::Value> = LazyLock::new(|| {
    serde_json::from_str(include_str!("../../../../assets/packets.json")).unwrap()
});
const BEDROCK_PLAY_SOUND: i32 = 86;
const BEDROCK_STOP_SOUND: i32 = 87;

const REWRITTEN: &str = "sculith:rewritten";
const PAYLOAD: &str = "sculith:payload";
const CANCELLED: &str = "sculith:cancelled";
const SENTINEL: &str = "sculith:sentinel";

pub fn on_join(player: &Player) -> bool {
    if !player.get_name().starts_with("Packet") {
        return false;
    }
    player.play_custom_sound_at(
        (0.0, -60.0, 0.0),
        "sculith:demo_tone",
        SoundCategory::Master,
        1.0,
        1.0,
    );
    for name in ["sculith:demo_tone", CANCELLED, SENTINEL] {
        player.stop_custom_sound(Some(name), None);
    }
    true
}

pub fn sent(event: &mut EventData<PacketSentEvent>) {
    if !event.player.get_name().starts_with("Packet") {
        return;
    }
    let native = event.player.as_bedrock().is_some();
    let (sound, stop) = if native {
        (BEDROCK_PLAY_SOUND, BEDROCK_STOP_SOUND)
    } else {
        let ids = &JAVA_PACKETS["clientbound"]["play"];
        (
            ids["sound"].as_i64().unwrap() as i32,
            ids["stop_sound"].as_i64().unwrap() as i32,
        )
    };
    if event.packet_id == sound {
        event.packet_id = stop;
        event.raw_payload = stop_payload(native, REWRITTEN);
        tracing::info!(player=%event.player.get_name(), "PROBE_PACKET_ID_REWRITTEN");
    } else if event.packet_id == stop {
        if event
            .raw_payload
            .windows(CANCELLED.len())
            .any(|w| w == CANCELLED.as_bytes())
        {
            event.cancelled = true;
            tracing::info!(player=%event.player.get_name(), "PROBE_PACKET_CANCELLED");
        } else if !event
            .raw_payload
            .windows(SENTINEL.len())
            .any(|w| w == SENTINEL.as_bytes())
        {
            event.raw_payload = stop_payload(native, PAYLOAD);
            tracing::info!(player=%event.player.get_name(), "PROBE_PACKET_PAYLOAD_REWRITTEN");
        }
    }
}

fn stop_payload(native: bool, name: &str) -> Vec<u8> {
    assert!(name.len() < 128);
    let mut bytes = if native { vec![] } else { vec![2] };
    bytes.push(name.len() as u8);
    bytes.extend_from_slice(name.as_bytes());
    if native {
        bytes.extend_from_slice(&[0, 0]);
    }
    bytes
}
