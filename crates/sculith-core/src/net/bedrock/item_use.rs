use std::time::Instant;

use crate::server::TickDeadline;

use super::BedrockClient;

#[derive(Clone, Copy)]
pub(super) struct FoodUseClock {
    slot: u8,
    started: Instant,
    last_step: Option<Instant>,
}

impl BedrockClient {
    // The living entity's edit guard excludes regional steps while a world
    // caller starts, stops or ticks the active item.
    pub(crate) fn reset_food_use(&self, slot: Option<u8>) {
        self.activity.food_use.store(slot.map(|slot| FoodUseClock {
            slot,
            started: Instant::now(),
            last_step: None,
        }));
    }

    pub(crate) fn food_use_slot(&self) -> Option<u8> {
        self.activity.food_use.load().map(|clock| clock.slot)
    }

    pub(crate) fn claim_food_use_tick(
        &self,
        deadline: Option<TickDeadline>,
        regional: bool,
    ) -> bool {
        let Some(mut clock) = self.activity.food_use.load() else {
            return false;
        };
        if deadline.is_some_and(|step| {
            clock
                .last_step
                .map_or(regional && step.at <= clock.started, |last| {
                    !step.follows(last)
                })
        }) {
            return false;
        }
        clock.last_step = deadline.map(|step| step.at);
        self.activity.food_use.store(Some(clock));
        true
    }
}
