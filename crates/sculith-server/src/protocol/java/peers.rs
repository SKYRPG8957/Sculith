//! Java 26.3 player presentation; registry/packet facts from the pinned official
//! client/report. No texture properties or online account verification are forged.
use super::{codec, write_string, write_varint};
use crate::game::PlayerSnapshot;
use anyhow::{Result, ensure};
use sculith_server::peers::Change;

fn id(value: u64) -> Result<i32> {
    ensure!(
        value <= i32::MAX as u64,
        "peer entity ID exceeds Java range"
    );
    Ok(value as i32)
}
fn angle(value: f64) -> u8 {
    ((value * 256.0 / 360.0).floor() as i32).rem_euclid(256) as u8
}

pub fn packets(change: &Change) -> Result<Vec<Vec<u8>>> {
    Ok(match change {
        Change::InfoAdd(player) => vec![info(player)?],
        Change::Equipment(player) => vec![equipment(player)?],
        Change::InfoRemove { uuid } => {
            let mut out = codec::packet(70);
            write_varint(1, &mut out);
            out.extend_from_slice(uuid.as_bytes());
            vec![out]
        }
        Change::Despawn { id: entity } => {
            let mut out = codec::packet(78);
            write_varint(1, &mut out);
            write_varint(id(*entity)?, &mut out);
            vec![out]
        }
        Change::Spawn(player) => {
            let mut out = codec::packet(1);
            write_varint(id(player.id)?, &mut out);
            out.extend_from_slice(player.profile.uuid.as_bytes());
            write_varint(159, &mut out); // Player entity type in the 26.3 report.
            for coordinate in [player.feet.x, player.feet.y, player.feet.z] {
                out.extend_from_slice(&coordinate.to_be_bytes());
            }
            out.push(0); // LP Vec3 zero velocity.
            out.extend_from_slice(&[
                angle(player.rotation.pitch),
                angle(player.rotation.yaw),
                angle(player.rotation.head_yaw),
            ]);
            write_varint(0, &mut out);
            vec![out, equipment(player)?]
        }
        Change::Move { player, .. } => {
            let mut out = codec::packet(35);
            write_varint(id(player.id)?, &mut out);
            write_varint(0, &mut out); // PositionPath.LINEAR, followed by Vec3 doubles.
            for coordinate in [player.feet.x, player.feet.y, player.feet.z] {
                out.extend_from_slice(&coordinate.to_be_bytes());
            }
            out.extend_from_slice(&(player.rotation.yaw as f32).to_be_bytes());
            out.extend_from_slice(&(player.rotation.pitch as f32).to_be_bytes());
            out.push(0);
            let mut head = codec::packet(85);
            write_varint(id(player.id)?, &mut head);
            head.push(angle(player.rotation.head_yaw));
            vec![out, head]
        }
    })
}

fn equipment(player: &PlayerSnapshot) -> Result<Vec<u8>> {
    ensure!(player.selected_slot <= 8, "invalid owner hotbar slot");
    let mut out = codec::packet(104);
    write_varint(id(player.id)?, &mut out);
    out.push(0); // MAINHAND, ordinal zero, last equipment entry.
    super::inventory::write_stack(player.held(), &mut out)?;
    Ok(out)
}

fn info(player: &PlayerSnapshot) -> Result<Vec<u8>> {
    let mut out = codec::packet(71);
    out.push(0xad); // Add, creative mode, listed, display name, hat. No signed chat session.
    write_varint(1, &mut out);
    out.extend_from_slice(player.profile.uuid.as_bytes());
    let name = &player.profile.name;
    if name.len() <= 16 && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
        write_string(name, &mut out);
    } else {
        write_string(&format!("Scu_{:x}", id(player.id)?), &mut out);
    }
    write_varint(0, &mut out); // No signed skin properties: stock client's default avatar.
    write_varint(1, &mut out); // Creative.
    out.extend_from_slice(&[1, 1]); // Listed; present display-name component.
    codec::text(name, &mut out)?;
    out.push(1); // Show hat.
    Ok(out)
}
