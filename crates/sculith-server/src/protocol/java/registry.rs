//! Wire identifiers generated from pinned official 26.3 reports/core file names.
//! Registry element definitions are supplied by the negotiated vanilla client core.
use super::{codec, write_string, write_varint};
use sculith_world::BlockKind;
use serde::Deserialize;
use std::{collections::BTreeMap, sync::OnceLock};

pub const VERSION: &str = "26.3";
pub const PROTOCOL: i32 = 777;

#[derive(Deserialize)]
struct Block {
    state: i32,
    item: i32,
}
#[derive(Deserialize)]
struct Facts {
    version: String,
    protocol: i32,
    blocks: BTreeMap<String, Block>,
    registries: BTreeMap<String, Vec<String>>,
    tags: BTreeMap<String, BTreeMap<String, Vec<i32>>>,
}

pub struct Registry {
    pub packets: Vec<Vec<u8>>,
    pub tags: Vec<u8>,
    pub biome: i32,
    pub dimension: i32,
    pub clock: i32,
    states: [i32; 5],
    items: [i32; 5],
}

pub fn get() -> &'static Registry {
    static REGISTRY: OnceLock<Registry> = OnceLock::new();
    REGISTRY.get_or_init(|| {
        let facts: Facts = serde_json::from_str(include_str!("protocol777.json"))
            .expect("bundled Java wire facts");
        assert_eq!(facts.version, VERSION);
        assert_eq!(facts.protocol, PROTOCOL);
        assert_eq!(facts.registries.len(), 32);
        let id = |kind: &str, name: &str| {
            facts.registries[kind]
                .iter()
                .position(|v| v == name)
                .expect("bundled registry entry") as i32
        };
        let biome = id("minecraft:worldgen/biome", "minecraft:plains");
        let dimension = id("minecraft:dimension_type", "minecraft:overworld");
        let clock = id("minecraft:world_clock", "minecraft:overworld");
        let mut packets = Vec::new();
        for (kind, values) in &facts.registries {
            let mut packet = codec::packet(7);
            write_string(kind, &mut packet);
            write_varint(values.len() as i32, &mut packet);
            for value in values {
                write_string(value, &mut packet);
                packet.push(0); // Known vanilla core supplies the element.
            }
            packets.push(packet);
        }
        let mut tags = codec::packet(0x0e);
        write_varint(facts.tags.len() as i32, &mut tags);
        for (kind, values) in &facts.tags {
            write_string(kind, &mut tags);
            write_varint(values.len() as i32, &mut tags);
            for (name, entries) in values {
                write_string(name, &mut tags);
                write_varint(entries.len() as i32, &mut tags);
                for &entry in entries {
                    write_varint(entry, &mut tags);
                }
            }
        }
        let mut states = [0; 5];
        let mut items = [0; 5];
        for (kind, name) in [
            (BlockKind::Air, "air"),
            (BlockKind::Bedrock, "bedrock"),
            (BlockKind::Stone, "stone"),
            (BlockKind::Dirt, "dirt"),
            (BlockKind::Grass, "grass_block"),
        ] {
            states[kind as usize] = facts.blocks[name].state;
            items[kind as usize] = facts.blocks[name].item;
        }
        Registry {
            packets,
            tags,
            biome,
            dimension,
            clock,
            states,
            items,
        }
    })
}
impl Registry {
    pub fn state(&self, kind: BlockKind) -> i32 {
        self.states[kind as usize]
    }
    pub fn item(&self, kind: BlockKind) -> i32 {
        self.items[kind as usize]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn facts_use_exact_26_3_states_and_item_ids() {
        let r = get();
        assert_eq!(
            [
                r.state(BlockKind::Air),
                r.state(BlockKind::Bedrock),
                r.state(BlockKind::Stone),
                r.state(BlockKind::Dirt),
                r.state(BlockKind::Grass)
            ],
            [0, 88, 1, 10, 9]
        );
        assert_eq!(
            [
                r.item(BlockKind::Stone),
                r.item(BlockKind::Dirt),
                r.item(BlockKind::Grass)
            ],
            [1, 55, 54]
        );
        assert_eq!((r.biome, r.dimension, r.clock), (41, 0, 0));
        assert_eq!(r.packets.len(), 32);
        assert!(r.tags.len() < 128 * 1024);
    }
}
