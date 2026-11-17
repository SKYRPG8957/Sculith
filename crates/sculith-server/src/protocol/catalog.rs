//! Versioned identifier registration. Registration is distinct from a working
//! game handler; the current gameplay subset is deliberately reported separately.
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, sync::OnceLock};

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Descriptor {
    pub edition: &'static str,
    pub protocol: u32,
    pub phase: String,
    pub direction: String,
    pub id: u16,
    pub name: String,
    pub typed_codec: bool,
}

#[derive(Deserialize)]
struct JavaFacts {
    version: String,
    protocol: u32,
    packets: BTreeMap<String, BTreeMap<String, BTreeMap<String, u16>>>,
}

pub fn java() -> &'static [Descriptor] {
    static PACKETS: OnceLock<Vec<Descriptor>> = OnceLock::new();
    PACKETS.get_or_init(|| {
        let facts: JavaFacts = serde_json::from_str(include_str!("java/protocol777.json"))
            .expect("pinned Java packet facts");
        assert_eq!((facts.version.as_str(), facts.protocol), ("26.3", 777));
        let mut out = Vec::new();
        for (phase, directions) in facts.packets {
            for (direction, packets) in directions {
                for (name, id) in packets {
                    out.push(Descriptor {
                        edition: "java",
                        protocol: 777,
                        phase: phase.clone(),
                        direction: direction.clone(),
                        id,
                        name,
                        typed_codec: false,
                    });
                }
            }
        }
        out.sort_by(|a, b| (&a.phase, &a.direction, a.id).cmp(&(&b.phase, &b.direction, b.id)));
        validate(&out).expect("valid Java registration");
        out
    })
}

pub fn validate(packets: &[Descriptor]) -> Result<()> {
    let mut keys = std::collections::BTreeSet::new();
    for p in packets {
        ensure!(
            !p.name.is_empty() && p.name.len() <= 128 && p.name.is_ascii(),
            "invalid registered packet name"
        );
        ensure!(
            keys.insert((p.edition, p.protocol, &p.phase, &p.direction, p.id)),
            "duplicate registered packet identifier"
        );
    }
    Ok(())
}

pub fn all() -> Vec<Descriptor> {
    let mut out = java().to_vec();
    out.extend(super::bedrock::catalog::registered());
    out
}

pub fn java_receipt(id: i32, sequence: u64) -> Result<sculith_plugin_api::PacketInfo> {
    use sculith_plugin_api::{Edition, PacketDispatch, PacketInfo};
    ensure!(id >= 0, "negative Java packet identifier");
    let entry = java()
        .iter()
        .find(|p| p.phase == "play" && p.direction == "serverbound" && i32::from(p.id) == id);
    let dispatch = if entry.is_none() {
        PacketDispatch::Unknown
    } else {
        match id {
            0 | 0x0b | 0x0d | 0x0e | 0x1c | 0x2c => PacketDispatch::Control,
            7 | 9 | 0x12 | 0x1e..=0x21 | 0x24 | 0x29 | 0x36 | 0x39 | 0x42 => {
                PacketDispatch::Gameplay
            }
            0x44 => PacketDispatch::Notification,
            _ => PacketDispatch::Unsupported,
        }
    };
    Ok(PacketInfo {
        edition: Edition::Java,
        protocol: 777,
        phase: "play".into(),
        id: id as u32,
        name: entry
            .map(|p| p.name.clone())
            .unwrap_or_else(|| "unknown".into()),
        dispatch,
        sequence,
    })
}

pub fn java_connection_receipt(
    id: i32,
    protocol: Option<u32>,
    phase: sculith_core::connections::PacketPhase,
) -> Result<sculith_core::connections::ConnectionPacket> {
    java_connection_packet(id, protocol, phase, false)
}

pub fn java_connection_sent(
    id: i32,
    protocol: Option<u32>,
    phase: sculith_core::connections::PacketPhase,
) -> Result<sculith_core::connections::ConnectionPacket> {
    java_connection_packet(id, protocol, phase, true)
}

fn java_connection_packet(
    id: i32,
    protocol: Option<u32>,
    phase: sculith_core::connections::PacketPhase,
    sent: bool,
) -> Result<sculith_core::connections::ConnectionPacket> {
    use sculith_core::{
        PacketDispatch,
        connections::{ConnectionPacket, PacketPhase},
    };
    ensure!(id >= 0, "negative Java packet identifier");
    let catalog_phase = match phase {
        PacketPhase::Handshake => "handshake",
        PacketPhase::Status => "status",
        PacketPhase::Login => "login",
        PacketPhase::Configuration => "configuration",
        PacketPhase::Spawn | PacketPhase::Play => "play",
        _ => "unknown",
    };
    // Handshake/status are version-independent. Never give a known-version
    // packet name to a different protocol selected by an unverified client.
    let known_protocol =
        matches!(phase, PacketPhase::Handshake | PacketPhase::Status) || protocol == Some(777);
    let entry = known_protocol
        .then(|| {
            java().iter().find(|p| {
                p.phase == catalog_phase
                    && p.direction == (if sent { "clientbound" } else { "serverbound" })
                    && i32::from(p.id) == id
            })
        })
        .flatten();
    let dispatch = if entry.is_none() {
        PacketDispatch::Unknown
    } else if sent {
        if matches!(phase, PacketPhase::Spawn | PacketPhase::Play) {
            PacketDispatch::Notification
        } else {
            PacketDispatch::Control
        }
    } else if phase == PacketPhase::Play {
        java_receipt(id, 1)?.dispatch
    } else {
        PacketDispatch::Control
    };
    Ok(ConnectionPacket {
        protocol,
        phase,
        id: id as u32,
        name: entry
            .map(|p| p.name.clone())
            .unwrap_or_else(|| "unknown".into()),
        dispatch,
        sequence: 0,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn connection_receipts_use_phase_and_selected_protocol() {
        use sculith_core::connections::PacketPhase;
        assert_eq!(
            java_receipt(0x24, 1).unwrap().dispatch,
            sculith_core::PacketDispatch::Gameplay
        );
        assert_eq!(
            java_connection_receipt(0x24, Some(777), PacketPhase::Play)
                .unwrap()
                .dispatch,
            sculith_core::PacketDispatch::Gameplay
        );
        for (phase, id, name) in [
            (PacketPhase::Handshake, 0, "minecraft:intention"),
            (PacketPhase::Status, 0, "minecraft:status_request"),
            (PacketPhase::Login, 0, "minecraft:hello"),
            (
                PacketPhase::Configuration,
                0,
                "minecraft:client_information",
            ),
            (PacketPhase::Spawn, 0, "minecraft:accept_teleportation"),
        ] {
            assert_eq!(
                java_connection_receipt(id, Some(777), phase).unwrap().name,
                name
            );
        }
        assert_eq!(
            java_connection_receipt(0, Some(776), PacketPhase::Login)
                .unwrap()
                .name,
            "unknown"
        );
        assert_eq!(
            java_connection_receipt(0, None, PacketPhase::Handshake)
                .unwrap()
                .name,
            "minecraft:intention"
        );
        assert_eq!(
            java_connection_receipt(1000, Some(777), PacketPhase::Play)
                .unwrap()
                .dispatch,
            sculith_core::PacketDispatch::Unknown
        );
        assert!(java_connection_receipt(-1, Some(777), PacketPhase::Login).is_err());
    }
    #[test]
    fn full_pinned_java_identifier_table_has_unique_phase_direction_keys() {
        let packets = java();
        assert_eq!(packets.len(), 260);
        assert_eq!(
            packets
                .iter()
                .filter(|p| p.phase == "play" && p.direction == "serverbound")
                .count(),
            69
        );
        assert_eq!(
            packets
                .iter()
                .filter(|p| p.phase == "play" && p.direction == "clientbound")
                .count(),
            144
        );
        assert!(packets.iter().any(|p| p.phase == "play"
            && p.direction == "clientbound"
            && p.id == 104
            && p.name == "minecraft:set_equipment"));
    }
}
