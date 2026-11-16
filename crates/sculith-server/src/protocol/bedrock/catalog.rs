//! The registration list is generated from the locked V2225 enum. The exhaustive
//! match makes adding a dependency variant without registering it a compile error.
use super::super::catalog::Descriptor;
use bedrock::protocol::{Packet, PacketDirection, ProtoVersionPackets, V2225};
use serde::Deserialize;

macro_rules! registered_packets {
    ($($variant:ident,)*) => {
        pub fn typed()->Vec<Descriptor> {
            vec![$({
                let meta=<<V2225 as ProtoVersionPackets>::$variant as Packet>::META;
                Descriptor {
                    edition:"bedrock",protocol:2225,phase:"packet".into(),
                    direction:match meta.direction {
                        Some(PacketDirection::ClientToServer)=>"serverbound",
                        Some(PacketDirection::ServerToClient)=>"clientbound",
                        None=>"unspecified",
                    }.into(),
                    id:<<V2225 as ProtoVersionPackets>::$variant as Packet>::ID,
                    name:meta.name.into(),typed_codec:true,
                }
            }),*]
        }
        pub fn variant_name(packet:&V2225)->Option<&'static str> {
            match packet {$(V2225::$variant(_)=>Some(stringify!($variant)),)*V2225::Unknown(_)=>None}
        }
    };
}
include!("registered_packets.rs");

#[derive(Deserialize)]
struct IndependentPool {
    protocol: u32,
    source_commit: String,
    packets: Vec<PoolEntry>,
}
#[derive(Deserialize)]
struct PoolEntry {
    id: u16,
    name: String,
}

pub fn registered() -> Vec<Descriptor> {
    let reference: IndependentPool = serde_json::from_str(include_str!("packet_pool2225.json"))
        .expect("pinned independent packet identifiers");
    assert_eq!(reference.protocol, 2225);
    assert_eq!(
        reference.source_commit,
        "e2251a60f71d5faa9784d6a4bc817c117cb6af78"
    );
    let mut entries = typed();
    let known: std::collections::BTreeSet<_> = entries.iter().map(|p| p.id).collect();
    for entry in reference.packets {
        if !known.contains(&entry.id) {
            // These identifiers pass through the explicit Unknown codec. There
            // is no body validation or gameplay support claimed for this row.
            entries.push(Descriptor {
                edition: "bedrock",
                protocol: 2225,
                phase: "packet".into(),
                direction: "unspecified".into(),
                id: entry.id,
                name: entry.name,
                typed_codec: false,
            });
        }
    }
    entries.sort_by_key(|p| p.id);
    super::super::catalog::validate(&entries).expect("valid native packet identifiers");
    entries
}

pub fn receipt(packet: &V2225, sequence: u64) -> sculith_plugin_api::PacketInfo {
    use bedrock::protocol::Packets;
    use sculith_plugin_api::{Edition, PacketDispatch, PacketInfo};
    use std::sync::OnceLock;
    static ENTRIES: OnceLock<Vec<Descriptor>> = OnceLock::new();
    let entries = ENTRIES.get_or_init(registered);
    let id = packet.id();
    let entry = entries
        .binary_search_by_key(&id, |p| p.id)
        .ok()
        .map(|i| &entries[i]);
    let dispatch = if entry.is_none() {
        PacketDispatch::Unknown
    } else if variant_name(packet).is_none() {
        PacketDispatch::Unsupported
    } else {
        match packet {
            V2225::PlayerAuthInputPacket(_)
            | V2225::TextPacket(_)
            | V2225::CommandRequestPacket(_)
            | V2225::PlayerActionPacket(_)
            | V2225::InventoryTransactionPacket(_)
            | V2225::MobEquipmentPacket(_)
            | V2225::PlayerHotbarPacket(_)
            | V2225::ItemStackRequestPacket(_) => PacketDispatch::Gameplay,
            V2225::RequestChunkRadiusPacket(_)
            | V2225::NetworkStackLatencyPacket(_)
            | V2225::ClientCacheStatusPacket(_)
            | V2225::SetLocalPlayerAsInitializedPacket(_) => PacketDispatch::Control,
            V2225::AnimatePacket(_) | V2225::ModalFormResponsePacket(_) => {
                PacketDispatch::Notification
            }
            _ => PacketDispatch::Unsupported,
        }
    };
    PacketInfo {
        edition: Edition::Bedrock,
        protocol: 2225,
        phase: "play".into(),
        id: u32::from(id),
        name: entry
            .map(|p| p.name.clone())
            .unwrap_or_else(|| "unknown".into()),
        dispatch,
        sequence,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn all_locked_native_types_are_registered_and_unknown_stays_explicit() {
        let entries = typed();
        assert_eq!(entries.len(), 239);
        super::super::super::catalog::validate(&entries).unwrap();
        let ids: std::collections::BTreeSet<_> = entries.iter().map(|p| p.id).collect();
        assert_eq!(ids.len(), entries.len());
        let unknown = V2225::Unknown(Box::new(bedrock::protocol::UnknownPacket {
            id: 1023,
            buf: vec![1].into_boxed_slice(),
        }));
        assert!(variant_name(&unknown).is_none());
        let all = registered();
        assert_eq!(all.len(), 244);
        assert_eq!(all.iter().filter(|p| !p.typed_codec).count(), 5);
        assert!(
            all.iter()
                .any(|p| p.id == 117 && p.name == "ScriptCustomEvent" && !p.typed_codec)
        );
    }
}
