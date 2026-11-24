#[allow(clippy::wildcard_imports)]
use super::*;

impl JavaClient {
    pub fn handle_move_vehicle(&self, player: &Arc<Player>, packet: &SMoveVehicle) {
        // A movement packet was received this tick — tracked for SClientTickEnd zeroing.
        self.received_movement_this_tick
            .store(true, Ordering::Relaxed);
        if let Some(vehicle) = player.controlled_vehicle() {
            player.apply_client_vehicle_move(
                vehicle.as_ref(),
                Vector3::new(packet.x, packet.y, packet.z),
                packet.yaw,
                packet.pitch,
                packet.on_ground,
            );
        }
    }
}
