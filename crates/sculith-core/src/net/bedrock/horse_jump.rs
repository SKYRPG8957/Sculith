use std::sync::{Arc, PoisonError};

use sculith_protocol::bedrock::server::player_auth_input::{InputData, SPlayerAuthInput};

use super::BedrockClient;
use crate::entity::{EntityBase, player::Player};
use crate::server::Server;

#[derive(Default)]
pub(super) struct HorseJumpInput {
    vehicle: Option<i32>,
    last_tick: Option<u64>,
    phase: JumpPhase,
    suppress_lift: bool,
}

#[derive(Default)]
enum JumpPhase {
    #[default]
    Ready,
    Charging(u16),
    Released,
}

impl BedrockClient {
    pub(crate) fn reset_horse_jump(&self) {
        let mut input = self
            .activity
            .horse_jump
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        // A new saddle or ride must not make a previously received tick current again.
        *input = HorseJumpInput {
            last_tick: input.last_tick,
            ..Default::default()
        };
    }

    /// Returns whether takeoff is blocked, or discards an outdated input.
    pub(super) fn apply_horse_jump_input(
        &self,
        server: &Arc<Server>,
        player: &Player,
        vehicle: &dyn EntityBase,
        packet: &SPlayerAuthInput,
        grounded: bool,
    ) -> Option<bool> {
        let vehicle_id = Some(vehicle.get_entity().entity_id);
        let tick = packet.tick.0;
        let flags = &packet.input_data;
        let released = flags.get(InputData::JumpReleasedRaw as usize);
        let held = !released
            && (flags.get(InputData::Jumping as usize)
                || flags.get(InputData::JumpCurrentRaw as usize));

        let mut input = self
            .activity
            .horse_jump
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if input.last_tick.is_some_and(|last_tick| tick <= last_tick) {
            return None;
        }
        if input.vehicle != vehicle_id {
            *input = HorseJumpInput {
                vehicle: vehicle_id,
                ..Default::default()
            };
        }
        input.last_tick = Some(tick);

        if held {
            if grounded {
                let ticks = match input.phase {
                    JumpPhase::Charging(ticks) => ticks.saturating_add(1),
                    _ => 0,
                };
                input.phase = JumpPhase::Charging(ticks);
            } else if matches!(input.phase, JumpPhase::Charging(_)) {
                input.phase = JumpPhase::Ready;
            }
            return Some(input.suppress_lift);
        }

        let JumpPhase::Charging(ticks) = input.phase else {
            if released && matches!(input.phase, JumpPhase::Ready) {
                input.phase = JumpPhase::Released;
                input.suppress_lift = true;
            }
            return Some(input.suppress_lift);
        };
        input.phase = JumpPhase::Released;
        drop(input);

        let allowed = grounded && player.try_start_riding_jump(server, vehicle, jump_amount(ticks));
        // A listener can dismount the rider or change the saddle during the event.
        let current = player.controlled_vehicle()?;
        if !std::ptr::addr_eq(current.as_ref(), vehicle) {
            return None;
        }
        let mut input = self
            .activity
            .horse_jump
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if input.vehicle != vehicle_id || input.last_tick != Some(tick) {
            return None;
        }
        input.suppress_lift = !allowed;
        Some(input.suppress_lift)
    }
}

fn jump_amount(ticks: u16) -> i32 {
    // The charge peaks after ten held ticks, then settles toward eighty percent.
    if ticks <= 10 {
        10 * i32::from(ticks.max(1))
    } else {
        80 + 20 / i32::from(ticks - 9)
    }
}
