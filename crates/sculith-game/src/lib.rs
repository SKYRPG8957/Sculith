//! Pure authoritative creative-builder state for the single region owner.
//! Protocol tasks submit untrusted intent values; only successful decisions commit
//! region changes. Edition packet types and item/block runtime registries live
//! in adapters. The caller owns I/O, leases, scheduling and event broadcasts.

pub mod commands;
pub mod inventory;

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, Instant},
};

use anyhow::{Result, ensure};
use sculith_world::{
    BlockEdit, BlockKind, BlockPos, ChunkPos, ChunkSnapshot, CommittedBatch, MAX_Y, MIN_Y, Region,
    RegionSnapshot, WORLD_BORDER, WorldError,
};
use uuid::Uuid;

const MAX_PLAYERS: u32 = 64;
const BODY_RADIUS: f64 = 0.3;
const BODY_HEIGHT: f64 = 1.8;
const EYE_HEIGHT: f64 = 1.62;
const MOVE_RATE: f64 = 12.0;
const MOVE_BURST: f64 = 2.0;
const BUILD_RATE: f64 = 20.0;
const BUILD_BURST: f64 = 4.0;
const REACH: f64 = 6.0;
const EPSILON: f64 = 1e-9;
const MAX_RAY_STEPS: usize = 64;
static NEXT_PLAYER_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum Edition {
    Bedrock,
    Java,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Profile {
    /// Already verified or explicitly namespaced offline identity, not a client name.
    pub subject: String,
    pub name: String,
    pub uuid: Uuid,
    pub edition: Edition,
}

#[derive(Debug, Copy, Clone, Default, PartialEq, Eq)]
pub struct Permissions {
    pub can_build: bool,
}

#[derive(Debug, Copy, Clone, PartialEq)]
pub struct Position {
    pub x: f64,
    pub y: f64,
    pub z: f64,
}

impl Position {
    fn finite(self) -> bool {
        self.x.is_finite() && self.y.is_finite() && self.z.is_finite()
    }
    fn eye(self) -> Self {
        Self {
            y: self.y + EYE_HEIGHT,
            ..self
        }
    }
    fn distance(self, other: Self) -> f64 {
        ((self.x - other.x).powi(2) + (self.y - other.y).powi(2) + (self.z - other.z).powi(2))
            .sqrt()
    }
    fn axes(self) -> [f64; 3] {
        [self.x, self.y, self.z]
    }
}

#[derive(Debug, Copy, Clone, Default, PartialEq)]
pub struct Rotation {
    pub yaw: f64,
    pub pitch: f64,
    pub head_yaw: f64,
}

impl Rotation {
    fn validated(self) -> Option<Self> {
        if !self.yaw.is_finite()
            || !self.head_yaw.is_finite()
            || !self.pitch.is_finite()
            || !(-90.0..=90.0).contains(&self.pitch)
        {
            return None;
        }
        Some(Self {
            yaw: (self.yaw + 180.0).rem_euclid(360.0) - 180.0,
            head_yaw: (self.head_yaw + 180.0).rem_euclid(360.0) - 180.0,
            pitch: self.pitch,
        })
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct PlayerSnapshot {
    pub id: u64,
    pub profile: Profile,
    pub permissions: Permissions,
    /// Feet in world coordinates; eye/base offsets are not accepted as a position.
    pub feet: Position,
    pub rotation: Rotation,
    pub selected_slot: u8,
    pub initialized: bool,
    pub hotbar: [Option<BlockKind>; 9],
    /// Immutable presentation copies; the private Inventory owns item mutations.
    pub inventory: [Option<crate::inventory::ItemStack>; 36],
    pub cursor: Option<crate::inventory::ItemStack>,
    pub created_output: Option<crate::inventory::ItemStack>,
}

impl PlayerSnapshot {
    pub fn inventory_view(&self) -> sculith_core::inventory::InventoryView {
        let item = |stack: crate::inventory::ItemStack| sculith_core::inventory::InventoryItem {
            item: crate::inventory::item_name(stack.kind).into(),
            count: u16::from(stack.count),
        };
        sculith_core::inventory::InventoryView {
            slots: self.inventory.iter().map(|stack| stack.map(item)).collect(),
            selected_slot: self.selected_slot,
            cursor: self.cursor.map(item),
            created_output: self.created_output.map(item),
        }
    }

    pub fn held(&self) -> Option<crate::inventory::ItemStack> {
        self.inventory
            .get(usize::from(self.selected_slot))
            .copied()
            .flatten()
    }
}

#[derive(Debug, Clone)]
pub struct InventoryOutcome {
    pub player: PlayerSnapshot,
    pub response: crate::inventory::Response,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatMessage {
    pub player_id: u64,
    pub name: String,
    pub message: String,
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum RejectReason {
    UnknownPlayer,
    NotInitialized,
    PermissionDenied,
    InvalidPosition,
    InvalidRotation,
    OutsideRegion,
    ClockRegression,
    StaleTick,
    SpeedLimit,
    Collision,
    TooFar,
    Occluded,
    Unbreakable,
    Occupied,
    InvalidTarget,
    InvalidFace,
    InvalidSlot,
    WrongHeldItem,
    RateLimited,
    WorldState,
}

#[derive(Debug, Clone, PartialEq)]
pub enum MoveOutcome {
    Accepted {
        player: PlayerSnapshot,
        tick: u64,
        changed: bool,
    },
    Rejected {
        correction: Option<PlayerSnapshot>,
        reason: RejectReason,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BlockOutcome {
    Committed {
        batch: CommittedBatch,
    },
    NoChange {
        position: BlockPos,
        current: BlockKind,
    },
    Rejected {
        position: BlockPos,
        current: Option<BlockKind>,
        reason: RejectReason,
    },
}

#[derive(Debug, Copy, Clone, PartialEq)]
struct Budget {
    remaining: f64,
    updated: Instant,
}

impl Budget {
    fn available(self, now: Instant, rate: f64, burst: f64) -> Option<f64> {
        let elapsed = now.checked_duration_since(self.updated)?.as_secs_f64();
        Some((self.remaining + elapsed * rate).min(burst))
    }
}

#[derive(Debug)]
struct Player {
    snapshot: PlayerSnapshot,
    inventory: crate::inventory::Inventory,
    inventory_budget: Budget,
    plugin_inventory_budget: Budget,
    last_tick: Option<u64>,
    movement: Budget,
    building: Budget,
    messages: Budget,
    text_input: Budget,
    forms: Budget,
    sounds: Budget,
    models: Budget,
    model: Option<ModelView>,
    pending_form: Option<PendingForm>,
}

impl Player {
    fn permit_plugin_inventory(&mut self, now: Instant) -> bool {
        let Some(available) = self.plugin_inventory_budget.available(now, 8.0, 16.0) else {
            return false;
        };
        if available < 1.0 {
            return false;
        }
        self.plugin_inventory_budget = Budget {
            remaining: available - 1.0,
            updated: now,
        };
        true
    }

    fn finish_inventory(&mut self, response: crate::inventory::Response) -> InventoryOutcome {
        self.snapshot.inventory = *self.inventory.slots();
        self.snapshot.cursor = self.inventory.cursor();
        self.snapshot.created_output = self.inventory.created_output();
        self.snapshot.hotbar =
            std::array::from_fn(|slot| self.snapshot.inventory[slot].map(|stack| stack.kind));
        InventoryOutcome {
            player: self.snapshot.clone(),
            response,
        }
    }
    fn permit_inventory(&mut self, now: Instant) -> bool {
        let Some(available) = self.inventory_budget.available(now, 40.0, 64.0) else {
            return false;
        };
        if available < 1.0 {
            return false;
        }
        self.inventory_budget = Budget {
            remaining: available - 1.0,
            updated: now,
        };
        true
    }
}

#[derive(Debug)]
struct PendingForm {
    id: u32,
    plugin: String,
    request_id: u32,
    buttons: u32,
    shown: Instant,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FormSubmission {
    pub player_id: u64,
    pub plugin: String,
    pub request_id: u32,
    pub selection: Option<u32>,
}

#[derive(Debug, Clone)]
pub struct SoundEffect {
    pub player_id: u64,
    pub sound: String,
    pub position: Position,
    pub volume: f32,
    pub pitch: f32,
}

#[derive(Debug, Clone)]
pub struct ModelView {
    pub player_id: u64,
    pub entity_id: u64,
    pub uuid: Uuid,
    pub model: String,
    pub position: Position,
}

/// One exclusively owned game state containing the existing authoritative Region.
#[derive(Debug)]
pub struct GameState {
    region: Region,
    max_players: u32,
    spawn: Position,
    players: BTreeMap<u64, Player>,
    next_form_id: u32,
    registered_sounds: BTreeSet<String>,
    registered_models: BTreeSet<String>,
}

impl GameState {
    pub fn new(region: Region, max_players: u32) -> Result<Self> {
        Self::with_sounds(region, max_players, BTreeSet::new())
    }

    pub fn with_sounds(
        region: Region,
        max_players: u32,
        registered_sounds: BTreeSet<String>,
    ) -> Result<Self> {
        Self::with_content(region, max_players, registered_sounds, BTreeSet::new())
    }

    pub fn with_content(
        region: Region,
        max_players: u32,
        registered_sounds: BTreeSet<String>,
        registered_models: BTreeSet<String>,
    ) -> Result<Self> {
        ensure!(
            registered_models.len() <= 32,
            "at most 32 registered models"
        );
        for model in &registered_models {
            sculith_core::validate_resource_id(model).map_err(anyhow::Error::msg)?;
        }
        ensure!(
            registered_sounds.len() <= 64,
            "at most 64 registered sounds"
        );
        for sound in &registered_sounds {
            sculith_core::validate_sound_id(sound).map_err(anyhow::Error::msg)?;
        }
        ensure!(
            (1..=MAX_PLAYERS).contains(&max_players),
            "max_players must be between 1 and {MAX_PLAYERS}"
        );
        let (x, y, z) = region.spawn();
        let spawn = clear_spawn(
            &region,
            Position {
                x: f64::from(x),
                y: f64::from(y),
                z: f64::from(z),
            },
        )?;
        Ok(Self {
            registered_sounds,
            registered_models,
            next_form_id: 1,
            region,
            max_players,
            spawn,
            players: BTreeMap::new(),
        })
    }

    pub fn reserve(
        &mut self,
        profile: Profile,
        permissions: Permissions,
        now: Instant,
    ) -> Result<PlayerSnapshot> {
        ensure!(valid_text(&profile.subject, 256), "invalid player subject");
        ensure!(valid_text(&profile.name, 64), "invalid player display name");
        ensure!(!profile.uuid.is_nil(), "nil player UUID is not allowed");
        ensure!(
            self.players.len() < self.max_players as usize,
            "server is full, including pending players"
        );
        ensure!(
            !self.players.values().any(|player| {
                let existing = &player.snapshot.profile;
                (existing.edition == profile.edition && existing.subject == profile.subject)
                    || existing.uuid == profile.uuid
            }),
            "player identity is already reserved"
        );
        let feet = clear_spawn(&self.region, self.spawn)?;
        let id = allocate_id(&NEXT_PLAYER_ID)?;
        let inventory = crate::inventory::Inventory::default();
        let snapshot = PlayerSnapshot {
            id,
            profile,
            permissions,
            feet,
            rotation: Rotation::default(),
            selected_slot: 0,
            initialized: false,
            inventory: *inventory.slots(),
            cursor: None,
            created_output: None,
            hotbar: [
                Some(BlockKind::Stone),
                Some(BlockKind::Dirt),
                Some(BlockKind::Grass),
                None,
                None,
                None,
                None,
                None,
                None,
            ],
        };
        self.players.insert(
            id,
            Player {
                snapshot: snapshot.clone(),
                inventory,
                inventory_budget: Budget {
                    remaining: 64.0,
                    updated: now,
                },
                plugin_inventory_budget: Budget {
                    remaining: 16.0,
                    updated: now,
                },
                last_tick: None,
                movement: Budget {
                    remaining: MOVE_BURST,
                    updated: now,
                },
                building: Budget {
                    remaining: BUILD_BURST,
                    updated: now,
                },
                messages: Budget {
                    remaining: 8.0,
                    updated: now,
                },
                text_input: Budget {
                    remaining: 4.0,
                    updated: now,
                },
                forms: Budget {
                    remaining: 1.0,
                    updated: now,
                },
                sounds: Budget {
                    remaining: 4.0,
                    updated: now,
                },
                models: Budget {
                    remaining: 1.0,
                    updated: now,
                },
                model: None,
                pending_form: None,
            },
        );
        Ok(snapshot)
    }

    pub fn activate(&mut self, id: u64) -> Result<PlayerSnapshot> {
        let player = self
            .players
            .get_mut(&id)
            .ok_or_else(|| anyhow::anyhow!("unknown player"))?;
        player.snapshot.initialized = true;
        Ok(player.snapshot.clone())
    }

    pub fn retire(&mut self, id: u64) -> Option<PlayerSnapshot> {
        self.players.remove(&id).map(|player| player.snapshot)
    }
    pub fn player(&self, id: u64) -> Option<PlayerSnapshot> {
        self.players.get(&id).map(|player| player.snapshot.clone())
    }
    /// Owned observations expose initialized sessions only, never pending leases.
    pub fn initialized_players(&self) -> Vec<PlayerSnapshot> {
        self.players
            .values()
            .filter(|p| p.snapshot.initialized)
            .map(|p| p.snapshot.clone())
            .collect()
    }

    pub fn online_count(&self) -> u32 {
        self.players
            .values()
            .filter(|player| player.snapshot.initialized)
            .count() as u32
    }

    /// Trusted server operations remain owner-validated and do not spend the
    /// client's transaction budget. Building permission is not a grant right.
    pub fn plugin_inventory(
        &mut self,
        id: u64,
        operation: &sculith_core::inventory::InventoryOperation,
        now: Instant,
    ) -> Result<InventoryOutcome, sculith_core::inventory::InventoryError> {
        use crate::inventory::RejectReason;
        use sculith_core::inventory::{InventoryError, InventoryOperation};
        operation
            .validate()
            .map_err(|_| InventoryError::InvalidRequest)?;
        let player = self
            .players
            .get_mut(&id)
            .filter(|p| p.snapshot.initialized)
            .ok_or(InventoryError::PlayerUnavailable)?;
        if !player.permit_plugin_inventory(now) {
            return Err(InventoryError::RateLimited);
        }
        let changes = match operation {
            InventoryOperation::Get => vec![],
            InventoryOperation::Give { item, count } => {
                let kind =
                    crate::inventory::item_kind(item).ok_or(InventoryError::UnsupportedItem)?;
                player
                    .inventory
                    .give(kind, *count)
                    .map_err(|reason| match reason {
                        RejectReason::Full => InventoryError::Full,
                        RejectReason::InvalidCount => InventoryError::InvalidRequest,
                        RejectReason::UnsupportedItem => InventoryError::UnsupportedItem,
                        _ => InventoryError::Unavailable,
                    })?
            }
        };
        Ok(player.finish_inventory(crate::inventory::Response {
            request_id: 0,
            result: Ok(changes),
            replayed: false,
        }))
    }

    pub fn inventory_request(
        &mut self,
        id: u64,
        request: crate::inventory::Request,
        now: Instant,
    ) -> Result<InventoryOutcome> {
        let player = self
            .players
            .get_mut(&id)
            .ok_or_else(|| anyhow::anyhow!("unknown player"))?;
        ensure!(player.snapshot.initialized, "player is not initialized");
        let response = if player.permit_inventory(now) {
            player
                .inventory
                .apply(&request, player.snapshot.permissions.can_build)
        } else {
            crate::inventory::Response {
                request_id: request.id,
                result: Err(crate::inventory::RejectReason::RateLimited),
                replayed: false,
            }
        };
        Ok(player.finish_inventory(response))
    }

    pub fn creative_slot(
        &mut self,
        id: u64,
        slot: u8,
        kind: Option<BlockKind>,
        count: u8,
        now: Instant,
    ) -> Result<InventoryOutcome> {
        let player = self
            .players
            .get_mut(&id)
            .ok_or_else(|| anyhow::anyhow!("unknown player"))?;
        ensure!(player.snapshot.initialized, "player is not initialized");
        let result = if player.permit_inventory(now) {
            player
                .inventory
                .replace_creative(slot, kind, count, player.snapshot.permissions.can_build)
                .map_err(|error| {
                    error
                        .downcast_ref::<crate::inventory::RejectReason>()
                        .copied()
                        .unwrap_or(crate::inventory::RejectReason::UnsupportedAction)
                })
        } else {
            Err(crate::inventory::RejectReason::RateLimited)
        };
        Ok(player.finish_inventory(crate::inventory::Response {
            request_id: 0,
            result,
            replayed: false,
        }))
    }

    pub fn inventory_click(
        &mut self,
        id: u64,
        click: crate::inventory::Click,
        now: Instant,
    ) -> Result<InventoryOutcome> {
        let player = self
            .players
            .get_mut(&id)
            .ok_or_else(|| anyhow::anyhow!("unknown player"))?;
        ensure!(player.snapshot.initialized, "player is not initialized");
        let result = if player.permit_inventory(now) {
            player
                .inventory
                .click(click, player.snapshot.permissions.can_build)
                .map_err(|error| {
                    error
                        .downcast_ref::<crate::inventory::RejectReason>()
                        .copied()
                        .unwrap_or(crate::inventory::RejectReason::UnsupportedAction)
                })
        } else {
            Err(crate::inventory::RejectReason::RateLimited)
        };
        Ok(player.finish_inventory(crate::inventory::Response {
            request_id: 0,
            result,
            replayed: false,
        }))
    }

    pub fn permit_message(&mut self, id: u64, now: Instant) -> Result<()> {
        let player = self
            .players
            .get_mut(&id)
            .ok_or_else(|| anyhow::anyhow!("unknown player"))?;
        ensure!(player.snapshot.initialized, "player is not initialized");
        let available = player
            .messages
            .available(now, 4.0, 8.0)
            .ok_or_else(|| anyhow::anyhow!("message clock regression"))?;
        ensure!(available >= 1.0, "player message rate exceeded");
        player.messages = Budget {
            remaining: available - 1.0,
            updated: now,
        };
        Ok(())
    }

    pub fn permit_text_input(
        &mut self,
        id: u64,
        text: &str,
        now: Instant,
    ) -> Result<PlayerSnapshot> {
        let player = self
            .players
            .get_mut(&id)
            .ok_or_else(|| anyhow::anyhow!("unknown player"))?;
        ensure!(player.snapshot.initialized, "player is not initialized");
        let available = player
            .text_input
            .available(now, 2.0, 4.0)
            .ok_or_else(|| anyhow::anyhow!("text clock regression"))?;
        ensure!(available >= 1.0, "player text rate exceeded");
        player.text_input = Budget {
            remaining: available - 1.0,
            updated: now,
        };
        sculith_core::validate_player_text(text).map_err(anyhow::Error::msg)?;
        Ok(player.snapshot.clone())
    }

    pub fn chat(&mut self, id: u64, message: String, now: Instant) -> Result<ChatMessage> {
        let player = self.permit_text_input(id, &message, now)?;
        ensure!(!message.starts_with('/'), "use the command channel");
        Ok(ChatMessage {
            player_id: id,
            name: player.profile.name,
            message,
        })
    }

    pub fn show_model(&mut self, id: u64, model: String, now: Instant) -> Result<ModelView> {
        sculith_core::validate_resource_id(&model).map_err(anyhow::Error::msg)?;
        ensure!(
            self.registered_models.contains(&model),
            "model is not registered by the active content bundle"
        );
        let player = self
            .players
            .get_mut(&id)
            .ok_or_else(|| anyhow::anyhow!("unknown player"))?;
        ensure!(player.snapshot.initialized, "player is not initialized");
        let remaining = player
            .models
            .available(now, 1.0, 1.0)
            .ok_or_else(|| anyhow::anyhow!("model clock regression"))?;
        ensure!(remaining >= 1.0, "player model rate exceeded");
        let yaw = player.snapshot.rotation.yaw.to_radians();
        let position = Position {
            x: player.snapshot.feet.x - yaw.sin() * 3.0,
            y: player.snapshot.feet.y,
            z: player.snapshot.feet.z + yaw.cos() * 3.0,
        };
        ensure!(
            position.y - 1.0 >= f64::from(MIN_Y) && position.y + 2.0 <= f64::from(MAX_Y + 1),
            "model preview exceeds vertical bounds"
        );
        for x in [-2.0, 2.0] {
            for z in [-2.0, 2.0] {
                ensure!(
                    self.region.bounds().contains(
                        BlockPos {
                            x: (position.x + x).floor() as i32,
                            y: 0,
                            z: (position.z + z).floor() as i32
                        }
                        .chunk()
                    ),
                    "model preview exceeds region bounds"
                );
            }
        }
        let (entity_id, uuid) = if let Some(old) = &player.model {
            (old.entity_id, old.uuid)
        } else {
            let id = allocate_id(&NEXT_PLAYER_ID)?;
            ensure!(id <= i32::MAX as u64, "model entity identifiers exhausted");
            (id, Uuid::new_v4())
        };
        let view = ModelView {
            player_id: id,
            entity_id,
            uuid,
            model,
            position,
        };
        player.model = Some(view.clone());
        player.models = Budget {
            remaining: remaining - 1.0,
            updated: now,
        };
        Ok(view)
    }

    pub fn hide_model(&mut self, id: u64) -> Result<Option<u64>> {
        let player = self
            .players
            .get_mut(&id)
            .ok_or_else(|| anyhow::anyhow!("unknown player"))?;
        ensure!(player.snapshot.initialized, "player is not initialized");
        Ok(player.model.take().map(|model| model.entity_id))
    }

    pub fn play_sound(
        &mut self,
        id: u64,
        sound: String,
        volume: f32,
        pitch: f32,
        now: Instant,
    ) -> Result<SoundEffect> {
        sculith_core::validate_sound_id(&sound).map_err(anyhow::Error::msg)?;
        sculith_core::validate_sound_parameters(volume, pitch).map_err(anyhow::Error::msg)?;
        ensure!(
            self.registered_sounds.contains(&sound),
            "sound is not registered by the active content bundle"
        );
        let player = self
            .players
            .get_mut(&id)
            .ok_or_else(|| anyhow::anyhow!("unknown player"))?;
        ensure!(player.snapshot.initialized, "player is not initialized");
        let remaining = player
            .sounds
            .available(now, 4.0, 4.0)
            .ok_or_else(|| anyhow::anyhow!("sound clock regression"))?;
        ensure!(remaining >= 1.0, "player sound rate exceeded");
        player.sounds = Budget {
            remaining: remaining - 1.0,
            updated: now,
        };
        Ok(SoundEffect {
            player_id: id,
            sound,
            position: player.snapshot.feet.eye(),
            volume,
            pitch,
        })
    }

    pub fn show_form(
        &mut self,
        id: u64,
        plugin: String,
        request_id: u32,
        form: &sculith_core::SimpleForm,
        now: Instant,
    ) -> Result<u32> {
        form.validate().map_err(anyhow::Error::msg)?;
        ensure!(
            !plugin.is_empty()
                && plugin.len() <= 64
                && plugin
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b)),
            "invalid form owner"
        );
        let player = self
            .players
            .get_mut(&id)
            .ok_or_else(|| anyhow::anyhow!("unknown player"))?;
        ensure!(player.snapshot.initialized, "player is not initialized");
        ensure!(
            player.pending_form.is_none(),
            "player already has a pending form"
        );
        let remaining = player
            .forms
            .available(now, 1.0, 1.0)
            .ok_or_else(|| anyhow::anyhow!("form clock regression"))?;
        ensure!(remaining >= 1.0, "player form rate exceeded");
        let form_id = self.next_form_id;
        self.next_form_id = form_id
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("form identifiers exhausted"))?;
        player.forms = Budget {
            remaining: remaining - 1.0,
            updated: now,
        };
        player.pending_form = Some(PendingForm {
            id: form_id,
            plugin,
            request_id,
            buttons: form.buttons.len() as u32,
            shown: now,
        });
        Ok(form_id)
    }

    pub fn submit_form(
        &mut self,
        id: u64,
        form_id: u32,
        selection: Option<u32>,
        now: Instant,
    ) -> Result<FormSubmission> {
        let player = self
            .players
            .get_mut(&id)
            .ok_or_else(|| anyhow::anyhow!("unknown player"))?;
        ensure!(player.snapshot.initialized, "player is not initialized");
        let pending = player
            .pending_form
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("no pending form"))?;
        ensure!(
            pending.id == form_id,
            "form identifier does not belong to this session"
        );
        let elapsed = now
            .checked_duration_since(pending.shown)
            .ok_or_else(|| anyhow::anyhow!("form clock regression"))?;
        if elapsed > Duration::from_secs(60) {
            player.pending_form.take();
            anyhow::bail!("form reply expired");
        }
        ensure!(
            selection.is_none_or(|choice| choice < pending.buttons),
            "form choice out of range"
        );
        let pending = player.pending_form.take().expect("validated above");
        Ok(FormSubmission {
            player_id: id,
            plugin: pending.plugin,
            request_id: pending.request_id,
            selection,
        })
    }
    pub fn snapshot(&mut self, pos: ChunkPos) -> sculith_world::Result<ChunkSnapshot> {
        self.region.snapshot(pos)
    }
    pub fn capture(&self) -> RegionSnapshot {
        self.region.capture()
    }
    pub fn revision(&self) -> u64 {
        self.region.revision()
    }

    pub fn select_slot(&mut self, id: u64, slot: u8) -> Result<PlayerSnapshot> {
        ensure!(slot <= 8, "hotbar slot must be 0..=8");
        let player = self
            .players
            .get_mut(&id)
            .ok_or_else(|| anyhow::anyhow!("unknown player"))?;
        ensure!(player.snapshot.initialized, "player is not initialized");
        player.snapshot.selected_slot = slot;
        Ok(player.snapshot.clone())
    }

    pub fn move_player(
        &mut self,
        id: u64,
        tick: u64,
        feet: Position,
        rotation: Rotation,
        now: Instant,
    ) -> MoveOutcome {
        let decision = self.check_movement(id, tick, feet, rotation, now);
        let (rotation, remaining) = match decision {
            Ok(value) => value,
            Err(reason) => {
                return MoveOutcome::Rejected {
                    correction: self.player(id),
                    reason,
                };
            }
        };
        // The check and update occur within this synchronous exclusive owner call.
        let Some(player) = self.players.get_mut(&id) else {
            return MoveOutcome::Rejected {
                correction: None,
                reason: RejectReason::UnknownPlayer,
            };
        };
        let changed = player.snapshot.feet != feet || player.snapshot.rotation != rotation;
        player.snapshot.feet = feet;
        player.snapshot.rotation = rotation;
        player.last_tick = Some(tick);
        player.movement = Budget {
            remaining,
            updated: now,
        };
        MoveOutcome::Accepted {
            player: player.snapshot.clone(),
            tick,
            changed,
        }
    }

    fn check_movement(
        &self,
        id: u64,
        tick: u64,
        feet: Position,
        rotation: Rotation,
        now: Instant,
    ) -> std::result::Result<(Rotation, f64), RejectReason> {
        let player = self.active(id)?;
        let rotation = rotation.validated().ok_or(RejectReason::InvalidRotation)?;
        validate_body(&self.region, feet)?;
        if player.last_tick.is_some_and(|last| tick <= last) {
            return Err(RejectReason::StaleTick);
        }
        let available = player
            .movement
            .available(now, MOVE_RATE, MOVE_BURST)
            .ok_or(RejectReason::ClockRegression)?;
        let distance = player.snapshot.feet.distance(feet);
        if distance > available + EPSILON {
            return Err(RejectReason::SpeedLimit);
        }
        if motion_collides(&self.region, player.snapshot.feet, feet).map_err(world_reason)? {
            return Err(RejectReason::Collision);
        }
        Ok((rotation, (available - distance).max(0.0)))
    }

    pub fn break_block(&mut self, id: u64, position: BlockPos, now: Instant) -> BlockOutcome {
        let decision = (|| {
            let player = self.builder(id)?;
            let current = self.region.get_block(position).map_err(world_reason)?;
            self.reachable(player.snapshot.feet.eye(), position)?;
            if current == BlockKind::Bedrock {
                return Err(RejectReason::Unbreakable);
            }
            if current == BlockKind::Air {
                return Ok(None);
            }
            let remaining = player
                .building
                .available(now, BUILD_RATE, BUILD_BURST)
                .ok_or(RejectReason::ClockRegression)?;
            if remaining + EPSILON < 1.0 {
                return Err(RejectReason::RateLimited);
            }
            Ok(Some(remaining - 1.0))
        })();
        match decision {
            Ok(None) => BlockOutcome::NoChange {
                position,
                current: BlockKind::Air,
            },
            Ok(Some(remaining)) => self.commit_block(id, position, BlockKind::Air, remaining, now),
            Err(reason) => self.reject_block(position, reason),
        }
    }

    pub fn place_block(
        &mut self,
        id: u64,
        against: BlockPos,
        face: u8,
        slot: u8,
        claimed: BlockKind,
        now: Instant,
    ) -> BlockOutcome {
        let Some(position) = adjacent(against, face) else {
            return self.reject_block(
                against,
                if face <= 5 {
                    RejectReason::InvalidPosition
                } else {
                    RejectReason::InvalidFace
                },
            );
        };
        let decision = (|| {
            let player = self.builder(id)?;
            if slot > 8 || player.snapshot.selected_slot != slot {
                return Err(RejectReason::InvalidSlot);
            }
            let held = player.snapshot.hotbar[slot as usize].ok_or(RejectReason::WrongHeldItem)?;
            if held != claimed {
                return Err(RejectReason::WrongHeldItem);
            }
            if self.region.get_block(against).map_err(world_reason)? == BlockKind::Air {
                return Err(RejectReason::InvalidTarget);
            }
            if self.region.get_block(position).map_err(world_reason)? != BlockKind::Air {
                return Err(RejectReason::Occupied);
            }
            self.reachable(player.snapshot.feet.eye(), against)?;
            if player.snapshot.feet.eye().distance(block_center(position)) > REACH + EPSILON {
                return Err(RejectReason::TooFar);
            }
            if self
                .players
                .values()
                .any(|player| block_intersects_body(position, player.snapshot.feet))
            {
                return Err(RejectReason::Collision);
            }
            let remaining = player
                .building
                .available(now, BUILD_RATE, BUILD_BURST)
                .ok_or(RejectReason::ClockRegression)?;
            if remaining + EPSILON < 1.0 {
                return Err(RejectReason::RateLimited);
            }
            Ok((held, remaining - 1.0))
        })();
        match decision {
            Ok((held, remaining)) => self.commit_block(id, position, held, remaining, now),
            Err(reason) => self.reject_block(position, reason),
        }
    }

    fn commit_block(
        &mut self,
        id: u64,
        position: BlockPos,
        block: BlockKind,
        remaining: f64,
        now: Instant,
    ) -> BlockOutcome {
        match self
            .region
            .commit(self.region.revision(), &[BlockEdit { position, block }])
        {
            Ok(batch) if batch.changes.is_empty() => BlockOutcome::NoChange {
                position,
                current: block,
            },
            Ok(batch) => {
                if let Some(player) = self.players.get_mut(&id) {
                    player.building = Budget {
                        remaining,
                        updated: now,
                    };
                }
                BlockOutcome::Committed { batch }
            }
            Err(error) => self.reject_block(position, world_reason(error)),
        }
    }

    fn reject_block(&self, position: BlockPos, reason: RejectReason) -> BlockOutcome {
        BlockOutcome::Rejected {
            position,
            current: self.region.get_block(position).ok(),
            reason,
        }
    }

    fn active(&self, id: u64) -> std::result::Result<&Player, RejectReason> {
        let player = self.players.get(&id).ok_or(RejectReason::UnknownPlayer)?;
        if !player.snapshot.initialized {
            return Err(RejectReason::NotInitialized);
        }
        Ok(player)
    }

    fn builder(&self, id: u64) -> std::result::Result<&Player, RejectReason> {
        let player = self.active(id)?;
        if !player.snapshot.permissions.can_build {
            return Err(RejectReason::PermissionDenied);
        }
        Ok(player)
    }

    fn reachable(
        &self,
        eye: Position,
        position: BlockPos,
    ) -> std::result::Result<(), RejectReason> {
        if eye.distance(block_center(position)) > REACH + EPSILON {
            return Err(RejectReason::TooFar);
        }
        if !line_of_sight(&self.region, eye, position).map_err(world_reason)? {
            return Err(RejectReason::Occluded);
        }
        Ok(())
    }
}

fn valid_text(value: &str, max: usize) -> bool {
    !value.trim().is_empty() && value.len() <= max && !value.chars().any(char::is_control)
}

fn clear_spawn(region: &Region, mut spawn: Position) -> Result<Position> {
    // A persisted or recent build may cover the default spawn. The search is
    // bounded by the region's fixed vertical world extent and never changes blocks.
    loop {
        validate_body(region, spawn)
            .map_err(|reason| anyhow::anyhow!("initial spawn is invalid: {reason:?}"))?;
        if !body_collides(region, spawn)? {
            return Ok(spawn);
        }
        spawn.y += 1.0;
        ensure!(
            spawn.y + BODY_HEIGHT <= f64::from(MAX_Y + 1),
            "no clear initial spawn exists"
        );
    }
}

fn allocate_id(counter: &AtomicU64) -> Result<u64> {
    counter
        .try_update(Ordering::Relaxed, Ordering::Relaxed, |next| {
            (next != 0 && next <= i64::MAX as u64).then(|| next + 1)
        })
        .map_err(|_| anyhow::anyhow!("global player entity IDs exhausted"))
}

fn world_reason(error: WorldError) -> RejectReason {
    match error {
        WorldError::InvalidBlockPosition(_) | WorldError::InvalidChunkPosition(_) => {
            RejectReason::InvalidPosition
        }
        WorldError::OutsideRegion { .. } => RejectReason::OutsideRegion,
        _ => RejectReason::WorldState,
    }
}

#[derive(Copy, Clone)]
struct Bounds {
    min: [f64; 3],
    max: [f64; 3],
}

fn body(feet: Position) -> Bounds {
    Bounds {
        min: [feet.x - BODY_RADIUS, feet.y, feet.z - BODY_RADIUS],
        max: [
            feet.x + BODY_RADIUS,
            feet.y + BODY_HEIGHT,
            feet.z + BODY_RADIUS,
        ],
    }
}

fn validate_body(region: &Region, feet: Position) -> std::result::Result<(), RejectReason> {
    if !feet.finite() {
        return Err(RejectReason::InvalidPosition);
    }
    let bounds = body(feet);
    if bounds.min[0] < -f64::from(WORLD_BORDER)
        || bounds.max[0] > f64::from(WORLD_BORDER)
        || bounds.min[2] < -f64::from(WORLD_BORDER)
        || bounds.max[2] > f64::from(WORLD_BORDER)
        || bounds.min[1] < f64::from(MIN_Y)
        || bounds.max[1] > f64::from(MAX_Y + 1)
    {
        return Err(RejectReason::InvalidPosition);
    }
    let (min, max) = occupied_cells(bounds);
    if !region.bounds().contains(min.chunk()) || !region.bounds().contains(max.chunk()) {
        return Err(RejectReason::OutsideRegion);
    }
    Ok(())
}

fn occupied_cells(bounds: Bounds) -> (BlockPos, BlockPos) {
    (
        BlockPos {
            x: bounds.min[0].floor() as i32,
            y: bounds.min[1].floor() as i32,
            z: bounds.min[2].floor() as i32,
        },
        BlockPos {
            x: bounds.max[0].ceil() as i32 - 1,
            y: bounds.max[1].ceil() as i32 - 1,
            z: bounds.max[2].ceil() as i32 - 1,
        },
    )
}

fn body_collides(region: &Region, feet: Position) -> sculith_world::Result<bool> {
    let (min, max) = occupied_cells(body(feet));
    for x in min.x..=max.x {
        for y in min.y..=max.y {
            for z in min.z..=max.z {
                if region.get_block(BlockPos { x, y, z })? != BlockKind::Air {
                    return Ok(true);
                }
            }
        }
    }
    Ok(false)
}

fn block_intersects_body(block: BlockPos, feet: Position) -> bool {
    let bounds = body(feet);
    let low = [f64::from(block.x), f64::from(block.y), f64::from(block.z)];
    (0..3).all(|axis| {
        bounds.max[axis] > low[axis] + EPSILON && bounds.min[axis] < low[axis] + 1.0 - EPSILON
    })
}

fn motion_collides(region: &Region, from: Position, to: Position) -> sculith_world::Result<bool> {
    let start = body(from);
    let end = body(to);
    let envelope = Bounds {
        min: std::array::from_fn(|axis| start.min[axis].min(end.min[axis])),
        max: std::array::from_fn(|axis| start.max[axis].max(end.max[axis])),
    };
    let (min, max) = occupied_cells(envelope);
    // Called only after a <=2m budget check, so at most 4*5*4 cells are examined.
    for x in min.x..=max.x {
        for y in min.y..=max.y {
            for z in min.z..=max.z {
                let block = BlockPos { x, y, z };
                if region.get_block(block)? != BlockKind::Air && swept_intersection(from, to, block)
                {
                    return Ok(true);
                }
            }
        }
    }
    Ok(false)
}

fn swept_intersection(from: Position, to: Position, block: BlockPos) -> bool {
    let min = [
        f64::from(block.x) - BODY_RADIUS,
        f64::from(block.y) - BODY_HEIGHT,
        f64::from(block.z) - BODY_RADIUS,
    ];
    let max = [
        f64::from(block.x) + 1.0 + BODY_RADIUS,
        f64::from(block.y) + 1.0,
        f64::from(block.z) + 1.0 + BODY_RADIUS,
    ];
    let from = from.axes();
    let to = to.axes();
    let (mut enter, mut leave) = (0.0f64, 1.0f64);
    for axis in 0..3 {
        let delta = to[axis] - from[axis];
        if delta.abs() < EPSILON {
            if from[axis] <= min[axis] + EPSILON || from[axis] >= max[axis] - EPSILON {
                return false;
            }
        } else {
            let first = (min[axis] - from[axis]) / delta;
            let second = (max[axis] - from[axis]) / delta;
            enter = enter.max(first.min(second));
            leave = leave.min(first.max(second));
            if enter >= leave - EPSILON {
                return false;
            }
        }
    }
    enter < leave - EPSILON && leave > EPSILON && enter < 1.0 - EPSILON
}

fn block_center(position: BlockPos) -> Position {
    Position {
        x: f64::from(position.x) + 0.5,
        y: f64::from(position.y) + 0.5,
        z: f64::from(position.z) + 0.5,
    }
}

fn line_of_sight(
    region: &Region,
    origin: Position,
    target: BlockPos,
) -> sculith_world::Result<bool> {
    let end = block_center(target).axes();
    let start = origin.axes();
    let mut cell = [
        origin.x.floor() as i32,
        origin.y.floor() as i32,
        origin.z.floor() as i32,
    ];
    let target_cell = [target.x, target.y, target.z];
    let direction: [f64; 3] = std::array::from_fn(|axis| end[axis] - start[axis]);
    let step: [i32; 3] = std::array::from_fn(|axis| {
        if direction[axis] > 0.0 {
            1
        } else if direction[axis] < 0.0 {
            -1
        } else {
            0
        }
    });
    let increments: [f64; 3] = std::array::from_fn(|axis| {
        if step[axis] == 0 {
            f64::INFINITY
        } else {
            1.0 / direction[axis].abs()
        }
    });
    let mut crossing: [f64; 3] = std::array::from_fn(|axis| {
        if step[axis] == 0 {
            f64::INFINITY
        } else {
            (f64::from(cell[axis] + i32::from(step[axis] > 0)) - start[axis]) / direction[axis]
        }
    });
    for _ in 0..MAX_RAY_STEPS {
        if cell == target_cell {
            return Ok(true);
        }
        if region.get_block(BlockPos {
            x: cell[0],
            y: cell[1],
            z: cell[2],
        })? != BlockKind::Air
        {
            return Ok(false);
        }
        let next = crossing[0].min(crossing[1]).min(crossing[2]);
        if !next.is_finite() || next > 1.0 + EPSILON {
            return Ok(false);
        }
        for axis in 0..3 {
            if crossing[axis] <= next + EPSILON {
                cell[axis] += step[axis];
                crossing[axis] += increments[axis];
            }
        }
    }
    Ok(false)
}

fn adjacent(against: BlockPos, face: u8) -> Option<BlockPos> {
    let (dx, dy, dz) = match face {
        0 => (0, -1, 0),
        1 => (0, 1, 0),
        2 => (0, 0, -1),
        3 => (0, 0, 1),
        4 => (-1, 0, 0),
        5 => (1, 0, 0),
        _ => return None,
    };
    Some(BlockPos {
        x: against.x.checked_add(dx)?,
        y: against.y.checked_add(dy)?,
        z: against.z.checked_add(dz)?,
    })
}

#[cfg(test)]
mod tests;
