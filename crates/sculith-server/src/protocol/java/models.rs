//! 26.3 facts cross-checked against the checksum-pinned official client/report:
//! item_display type 73, item_model component 10, entity item metadata 23/7.
use super::{codec, write_string, write_varint};
use crate::game::ModelView;
use anyhow::{Result, ensure};

pub fn remove(entity_id: u64) -> Result<Vec<u8>> {
    ensure!(
        entity_id <= i32::MAX as u64,
        "model entity ID exceeds Java range"
    );
    let mut packet = codec::packet(78);
    write_varint(1, &mut packet);
    write_varint(entity_id as i32, &mut packet);
    Ok(packet)
}

pub fn show(view: &ModelView) -> Result<Vec<Vec<u8>>> {
    sculith_core::validate_resource_id(&view.model).map_err(anyhow::Error::msg)?;
    let remove = remove(view.entity_id)?;
    let mut spawn = codec::packet(1);
    write_varint(view.entity_id as i32, &mut spawn);
    spawn.extend_from_slice(view.uuid.as_bytes());
    write_varint(73, &mut spawn);
    // Item rendering centres its 16-pixel cube on the entity origin. The pack
    // adds 8 to native X/Z; this half-block Y offset retains native feet Y.
    for value in [view.position.x, view.position.y + 0.5, view.position.z] {
        spawn.extend_from_slice(&value.to_be_bytes());
    }
    spawn.push(0); // New LP Vec3 zero velocity, not the legacy three shorts.
    spawn.extend_from_slice(&[0, 0, 0]); // Rotation and head rotation.
    write_varint(0, &mut spawn);
    let mut metadata = codec::packet(101);
    write_varint(view.entity_id as i32, &mut metadata);
    metadata.extend_from_slice(&[5, 8, 1]); // Base entity no-gravity Boolean.
    metadata.push(23);
    write_varint(7, &mut metadata); // Optional ItemStack.
    for value in [1, 1, 1, 0, 10] {
        write_varint(value, &mut metadata);
    } // Count, Stone carrier, patch additions/removals, ITEM_MODEL.
    write_string(&view.model, &mut metadata);
    metadata.extend_from_slice(&[24, 0, 0, 255]); // ItemDisplayContext.NONE; metadata terminator.
    Ok(vec![remove, spawn, metadata])
}
