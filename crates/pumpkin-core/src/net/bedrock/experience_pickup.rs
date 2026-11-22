use std::time::Instant;

use crate::{entity::player::Player, server::TickDeadline};

use super::BedrockClient;

#[derive(Clone, Copy)]
pub(super) enum PickupClock {
    Collected(Instant),
    Advanced(Instant),
}

impl BedrockClient {
    // The player's pickup delay lock also serializes this deadline stamp.
    pub(crate) fn start_experience_pickup_clock(&self) {
        self.activity
            .experience_pickup
            .store(Some(PickupClock::Collected(Instant::now())));
    }

    pub(crate) fn tick_experience_pickup_delay(
        &self,
        delay: &mut u32,
        deadline: Option<TickDeadline>,
    ) {
        if *delay == 0 {
            return;
        }
        if let Some(deadline) = deadline {
            let due = match self.activity.experience_pickup.load() {
                Some(PickupClock::Collected(at)) => deadline.at > at,
                Some(PickupClock::Advanced(at)) => deadline.follows(at),
                None => true,
            };
            if !due {
                return;
            }
        }
        *delay -= 1;
        self.activity
            .experience_pickup
            .store(deadline.map(|step| PickupClock::Advanced(step.at)));
    }

    /// Returns true while the connection still has work for its current region.
    pub(crate) fn advance_regional_experience_pickup(
        &self,
        player: &Player,
        deadline: TickDeadline,
        owns_player: impl FnOnce() -> bool,
    ) -> bool {
        if !self.is_current_protocol() {
            return false;
        }
        let Ok(mut delay) = player.experience_pick_up_delay.try_lock() else {
            return true;
        };
        if *delay > 0 && owns_player() {
            let previous = *delay;
            self.tick_experience_pickup_delay(&mut delay, Some(deadline));
            if *delay < previous {
                tracing::trace!(
                    player = %player.gameprofile.name,
                    remaining = *delay,
                    "Advanced regional experience pickup delay"
                );
            }
        }
        *delay > 0
    }
}
