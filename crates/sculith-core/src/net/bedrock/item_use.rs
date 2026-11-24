use std::{sync::atomic::Ordering, time::Instant};

use crate::server::TickDeadline;

use super::BedrockClient;

#[derive(Clone, Copy)]
pub(super) struct FoodUseClock {
    slot: u8,
    started: Instant,
    last_step: Option<Instant>,
}

impl FoodUseClock {
    fn can_step(self, deadline: Option<TickDeadline>, regional: bool) -> bool {
        deadline.is_none_or(|step| {
            (!regional || step.at > self.started)
                && self.last_step.is_none_or(|last| step.follows(last))
        })
    }
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

    pub(crate) fn regional_food_use_due(&self, deadline: TickDeadline) -> bool {
        self.activity
            .food_use
            .load()
            .is_some_and(|clock| clock.can_step(Some(deadline), true))
    }

    pub(crate) fn mark_food_use_metadata_dirty(&self) {
        self.activity
            .food_use_metadata_dirty
            .store(true, Ordering::Release);
    }

    pub(crate) fn take_food_use_metadata_dirty(&self) -> bool {
        self.activity
            .food_use_metadata_dirty
            .swap(false, Ordering::AcqRel)
    }

    pub(crate) fn claim_food_use_tick(
        &self,
        deadline: Option<TickDeadline>,
        regional: bool,
    ) -> bool {
        let Some(mut clock) = self.activity.food_use.load() else {
            return false;
        };
        if !clock.can_step(deadline, regional) {
            return false;
        }
        clock.last_step = deadline.map(|step| step.at);
        self.activity.food_use.store(Some(clock));
        true
    }
}
