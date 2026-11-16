//! Protocol-777 command tree: executable literals plus a greedy, unsigned
//! brigadier:string tail (registry ID 5 in the official 26.3 registry report).
use super::{codec, write_string, write_varint};
use sculith_core::CommandSpec;

pub fn tree(specs: &[CommandSpec]) -> Vec<u8> {
    let mut packet = codec::packet(16);
    write_varint((1 + specs.len() * 2) as i32, &mut packet);
    packet.push(0); // Root.
    write_varint(specs.len() as i32, &mut packet);
    for index in 0..specs.len() {
        write_varint((1 + index * 2) as i32, &mut packet);
    }
    for (index, spec) in specs.iter().enumerate() {
        packet.push(5); // Executable literal.
        write_varint(1, &mut packet);
        write_varint((2 + index * 2) as i32, &mut packet);
        write_string(&spec.name, &mut packet);
        packet.push(6); // Executable argument.
        write_varint(0, &mut packet);
        write_string("arguments", &mut packet);
        write_varint(5, &mut packet);
        write_varint(2, &mut packet); // brigadier:string, greedy.
    }
    write_varint(0, &mut packet);
    packet
}
