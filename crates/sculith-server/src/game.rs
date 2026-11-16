//! Pure authoritative creative-builder state for the single region owner.
//! Protocol tasks submit untrusted intent values; only successful decisions commit
//! region changes. No packets, runtime IDs, I/O, async work or separate simulation
//! live here. The caller owns leases, scheduling and committed-event broadcasts.

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
    last_tick: Option<u64>,
    movement: Budget,
    building: Budget,
    messages: Budget,
    forms: Budget,
    sounds: Budget,
    models: Budget,
    model: Option<ModelView>,
    pending_form: Option<PendingForm>,
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
        let snapshot = PlayerSnapshot {
            id,
            profile,
            permissions,
            feet,
            rotation: Rotation::default(),
            selected_slot: 0,
            initialized: false,
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
    pub fn online_count(&self) -> u32 {
        self.players
            .values()
            .filter(|player| player.snapshot.initialized)
            .count() as u32
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
mod tests {
    use super::*;
    use std::time::Duration;

    fn profile(index: u128) -> Profile {
        Profile {
            subject: format!("verified:{index}"),
            name: format!("player{index}"),
            uuid: Uuid::from_u128(index),
            edition: Edition::Bedrock,
        }
    }

    fn active(region: Region, now: Instant) -> (GameState, u64) {
        let mut game = GameState::new(region, 20).unwrap();
        let player = game
            .reserve(profile(1), Permissions { can_build: true }, now)
            .unwrap();
        game.activate(player.id).unwrap();
        (game, player.id)
    }

    #[test]
    fn models_require_registered_live_players_and_owner_clock_without_world_mutation() {
        let now = Instant::now();
        let mut state = GameState::with_content(
            Region::flat_origin(),
            20,
            Default::default(),
            ["sculith:demo_statue".into()].into(),
        )
        .unwrap();
        let player = state
            .reserve(profile(123), Permissions { can_build: true }, now)
            .unwrap();
        let id = player.id;
        assert!(
            state
                .show_model(id, "sculith:demo_statue".into(), now)
                .is_err()
        );
        assert!(state.hide_model(id).is_err());
        state.activate(id).unwrap();
        let revision = state.capture().revision();
        assert!(state.show_model(id, "sculith:missing".into(), now).is_err());
        let first = state
            .show_model(id, "sculith:demo_statue".into(), now)
            .unwrap();
        assert_ne!(first.entity_id, id);
        assert_eq!(
            first.position,
            Position {
                x: 0.5,
                y: 4.0,
                z: 3.5
            }
        );
        assert!(
            state
                .show_model(id, "sculith:demo_statue".into(), now)
                .is_err()
        );
        let second = state
            .show_model(
                id,
                "sculith:demo_statue".into(),
                now + Duration::from_secs(1),
            )
            .unwrap();
        assert_eq!(
            (first.entity_id, first.uuid),
            (second.entity_id, second.uuid)
        );
        assert!(
            state
                .show_model(id, "sculith:demo_statue".into(), now)
                .is_err()
        );
        assert_eq!(state.hide_model(id).unwrap(), Some(first.entity_id));
        assert_eq!(state.hide_model(id).unwrap(), None);
        let third = state
            .show_model(
                id,
                "sculith:demo_statue".into(),
                now + Duration::from_secs(2),
            )
            .unwrap();
        assert_ne!(third.entity_id, first.entity_id);
        state.players.get_mut(&id).unwrap().snapshot.feet.x = 63.5;
        assert!(
            state
                .show_model(
                    id,
                    "sculith:demo_statue".into(),
                    now + Duration::from_secs(3)
                )
                .is_err()
        );
        assert_eq!(
            state.players[&id].model.as_ref().unwrap().entity_id,
            third.entity_id
        );
        assert_eq!(state.capture().revision(), revision);
        state.retire(id);
        assert!(
            state
                .show_model(
                    id,
                    "sculith:demo_statue".into(),
                    now + Duration::from_secs(4)
                )
                .is_err()
        );
        assert!(state.hide_model(id).is_err());
    }

    #[test]
    fn message_budget_is_per_initialized_player_and_uses_the_owner_clock() {
        let now = Instant::now();
        let (mut state, id) = active(Region::flat_origin(), now);
        for _ in 0..8 {
            state.permit_message(id, now).unwrap();
        }
        assert!(state.permit_message(id, now).is_err());
        assert!(
            state
                .permit_message(id, now - Duration::from_millis(1))
                .is_err()
        );
        assert!(
            state
                .permit_message(id, now + Duration::from_millis(249))
                .is_err()
        );
        state
            .permit_message(id, now + Duration::from_millis(250))
            .unwrap();
        let other = state
            .reserve(profile(2), Permissions { can_build: true }, now)
            .unwrap();
        assert!(state.permit_message(other.id, now).is_err());
        state.activate(other.id).unwrap();
        state.permit_message(other.id, now).unwrap();
        state.retire(id);
        assert!(
            state
                .permit_message(id, now + Duration::from_secs(10))
                .is_err()
        );
        assert_eq!(state.revision(), 0);
    }

    #[test]
    fn forms_bind_replies_to_live_sessions_and_owner_without_replay_or_world_changes() {
        let now = Instant::now();
        let (mut state, id) = active(Region::flat_origin(), now);
        let other = state
            .reserve(profile(2), Permissions::default(), now)
            .unwrap();
        let form = sculith_core::SimpleForm {
            title: "Menu".into(),
            body: "Choose".into(),
            buttons: vec!["A".into(), "B".into()],
        };
        assert!(
            state
                .show_form(other.id, "owner".into(), 7, &form, now)
                .is_err()
        );
        state.activate(other.id).unwrap();
        let first = state.show_form(id, "owner".into(), 7, &form, now).unwrap();
        assert!(
            state
                .show_form(id, "another".into(), 8, &form, now + Duration::from_secs(1))
                .is_err()
        );
        assert!(state.submit_form(other.id, first, Some(0), now).is_err());
        assert!(state.submit_form(id, first + 1, Some(0), now).is_err());
        assert!(state.submit_form(id, first, Some(2), now).is_err());
        assert!(
            state
                .submit_form(id, first, Some(0), now - Duration::from_millis(1))
                .is_err()
        );
        let result = state.submit_form(id, first, Some(1), now).unwrap();
        assert_eq!(
            result,
            FormSubmission {
                player_id: id,
                plugin: "owner".into(),
                request_id: 7,
                selection: Some(1)
            }
        );
        assert!(state.submit_form(id, first, Some(0), now).is_err());
        assert!(
            state
                .show_form(
                    id,
                    "owner".into(),
                    8,
                    &form,
                    now + Duration::from_millis(999)
                )
                .is_err()
        );
        let second = state
            .show_form(id, "owner".into(), 8, &form, now + Duration::from_secs(1))
            .unwrap();
        assert_ne!(first, second);
        assert!(
            state
                .submit_form(id, first, Some(0), now + Duration::from_secs(1))
                .is_err()
        );
        assert!(
            state
                .submit_form(id, second, None, now + Duration::from_secs(1))
                .unwrap()
                .selection
                .is_none()
        );
        let third = state
            .show_form(id, "owner".into(), 9, &form, now + Duration::from_secs(2))
            .unwrap();
        assert!(
            state
                .submit_form(id, third, Some(0), now + Duration::from_secs(63))
                .is_err()
        );
        let fourth = state
            .show_form(id, "owner".into(), 10, &form, now + Duration::from_secs(64))
            .unwrap();
        state.retire(id);
        assert!(
            state
                .submit_form(id, fourth, None, now + Duration::from_secs(64))
                .is_err()
        );
        state.next_form_id = u32::MAX;
        assert!(
            state
                .show_form(other.id, "owner".into(), 0, &form, now)
                .is_err()
        );
        assert_eq!(state.revision(), 0);
    }

    fn accepted(outcome: MoveOutcome) -> PlayerSnapshot {
        match outcome {
            MoveOutcome::Accepted { player, .. } => player,
            other => panic!("movement rejected: {other:?}"),
        }
    }

    fn rejected_move(
        game: &mut GameState,
        id: u64,
        tick: u64,
        feet: Position,
        rotation: Rotation,
        now: Instant,
        expected: RejectReason,
    ) {
        let before = game.player(id);
        let budget = game
            .players
            .get(&id)
            .map(|player| (player.last_tick, player.movement, player.building));
        let world = sculith_world::storage::encode_snapshot(&game.capture()).unwrap();
        assert!(
            matches!(game.move_player(id, tick, feet, rotation, now), MoveOutcome::Rejected { reason, .. } if reason == expected)
        );
        assert_eq!(game.player(id), before, "rejection changed player snapshot");
        assert_eq!(
            game.players.get(&id).map(|player| (
                player.last_tick,
                player.movement,
                player.building
            )),
            budget,
            "rejection changed anti-cheat state"
        );
        assert_eq!(
            sculith_world::storage::encode_snapshot(&game.capture()).unwrap(),
            world,
            "rejection changed world state/revision/cache"
        );
    }

    fn rejected_block(
        game: &mut GameState,
        outcome: BlockOutcome,
        expected: RejectReason,
        before: &RegionSnapshot,
    ) {
        assert!(
            matches!(outcome, BlockOutcome::Rejected { reason, .. } if reason == expected),
            "{outcome:?}"
        );
        assert_eq!(
            sculith_world::storage::encode_snapshot(&game.capture()).unwrap(),
            sculith_world::storage::encode_snapshot(before).unwrap()
        );
    }

    #[test]
    fn lifecycle_reservations_capacity_namespaced_identity_and_cleanup_are_authoritative() {
        let now = Instant::now();
        let mut game = GameState::new(Region::flat_origin(), 2).unwrap();
        let first = game
            .reserve(profile(1), Permissions::default(), now)
            .unwrap();
        assert_eq!(game.online_count(), 0);
        assert!(!first.initialized);
        let mut duplicate = profile(2);
        duplicate.subject = first.profile.subject.clone();
        assert!(
            game.reserve(duplicate.clone(), Permissions::default(), now)
                .is_err()
        );
        duplicate.edition = Edition::Java;
        let second = game
            .reserve(duplicate, Permissions::default(), now)
            .unwrap();
        assert_ne!(first.id, second.id);
        assert!(first.id != 0 && second.id <= i64::MAX as u64);
        assert!(
            game.reserve(profile(3), Permissions::default(), now)
                .is_err()
        );
        game.activate(first.id).unwrap();
        game.activate(first.id).unwrap();
        assert_eq!(game.online_count(), 1);
        assert!(!game.retire(second.id).unwrap().initialized);
        assert!(game.retire(second.id).is_none());
        assert!(game.retire(first.id).unwrap().initialized);
        assert_eq!(game.online_count(), 0);
        assert!(
            game.reserve(profile(1), Permissions::default(), now)
                .is_ok()
        );
    }

    #[test]
    fn invalid_profiles_capacity_and_id_exhaustion_fail_without_world_changes() {
        let now = Instant::now();
        assert!(GameState::new(Region::flat_origin(), 0).is_err());
        assert!(GameState::new(Region::flat_origin(), 65).is_err());
        let mut game = GameState::new(Region::flat_origin(), 1).unwrap();
        for mut candidate in [profile(1), profile(2), profile(3), profile(4)] {
            match candidate.uuid.as_u128() {
                1 => candidate.subject.clear(),
                2 => candidate.name = "forged\nname".into(),
                3 => candidate.uuid = Uuid::nil(),
                _ => candidate.subject = "x".repeat(257),
            }
            assert!(
                game.reserve(candidate, Permissions::default(), now)
                    .is_err()
            );
        }
        assert!(game.players.is_empty());
        assert_eq!(game.region.loaded_chunks(), 0);
        let counter = AtomicU64::new(i64::MAX as u64);
        assert_eq!(allocate_id(&counter).unwrap(), i64::MAX as u64);
        assert!(allocate_id(&counter).is_err());
        assert_eq!(counter.load(Ordering::Relaxed), i64::MAX as u64 + 1);
    }

    #[test]
    fn pending_players_cannot_move_build_or_select_inventory() {
        let now = Instant::now();
        let mut game = GameState::new(Region::flat_origin(), 2).unwrap();
        let player = game
            .reserve(profile(1), Permissions { can_build: true }, now)
            .unwrap();
        rejected_move(
            &mut game,
            player.id,
            1,
            player.feet,
            Rotation::default(),
            now,
            RejectReason::NotInitialized,
        );
        let before = game.capture();
        let result = game.break_block(player.id, BlockPos { x: 1, y: 3, z: 0 }, now);
        rejected_block(&mut game, result, RejectReason::NotInitialized, &before);
        assert!(game.select_slot(player.id, 1).is_err());
        rejected_move(
            &mut game,
            u64::MAX,
            1,
            player.feet,
            Rotation::default(),
            now,
            RejectReason::UnknownPlayer,
        );
    }

    #[test]
    fn wall_clock_budget_cannot_be_bypassed_by_client_tick_jumps() {
        let now = Instant::now();
        let (mut game, id) = active(Region::flat_origin(), now);
        let rotation = Rotation {
            yaw: 450.0,
            pitch: 10.0,
            head_yaw: 720.0,
        };
        let first = accepted(game.move_player(
            id,
            1,
            Position {
                x: 1.5,
                y: 4.0,
                z: 0.5,
            },
            rotation,
            now,
        ));
        assert_eq!(
            first.rotation,
            Rotation {
                yaw: 90.0,
                pitch: 10.0,
                head_yaw: 0.0
            }
        );
        rejected_move(
            &mut game,
            id,
            u64::MAX,
            Position {
                x: 3.5,
                y: 4.0,
                z: 0.5,
            },
            rotation,
            now,
            RejectReason::SpeedLimit,
        );
        accepted(game.move_player(
            id,
            1_000_000_000,
            Position {
                x: 2.5,
                y: 4.0,
                z: 0.5,
            },
            rotation,
            now,
        ));
        rejected_move(
            &mut game,
            id,
            1_000_000_001,
            Position {
                x: 2.6,
                y: 4.0,
                z: 0.5,
            },
            rotation,
            now,
            RejectReason::SpeedLimit,
        );
        accepted(game.move_player(
            id,
            1_000_000_001,
            Position {
                x: 3.1,
                y: 4.0,
                z: 0.5,
            },
            rotation,
            now + Duration::from_millis(50),
        ));
        assert_eq!(game.revision(), 0, "movement is not a block revision");
    }

    #[test]
    fn invalid_movement_and_replayed_ticks_leave_all_state_unchanged() {
        let now = Instant::now();
        let (mut game, id) = active(Region::flat_origin(), now);
        let feet = game.player(id).unwrap().feet;
        accepted(game.move_player(id, 10, feet, Rotation::default(), now));
        for position in [
            Position {
                x: f64::NAN,
                ..feet
            },
            Position {
                y: f64::INFINITY,
                ..feet
            },
            Position {
                x: f64::from(WORLD_BORDER),
                ..feet
            },
            Position { y: 319.0, ..feet },
        ] {
            rejected_move(
                &mut game,
                id,
                11,
                position,
                Rotation::default(),
                now,
                RejectReason::InvalidPosition,
            );
        }
        rejected_move(
            &mut game,
            id,
            11,
            Position { x: 64.0, ..feet },
            Rotation::default(),
            now,
            RejectReason::OutsideRegion,
        );
        for rotation in [
            Rotation {
                yaw: f64::NAN,
                ..Rotation::default()
            },
            Rotation {
                pitch: 90.1,
                ..Rotation::default()
            },
            Rotation {
                head_yaw: f64::INFINITY,
                ..Rotation::default()
            },
        ] {
            rejected_move(
                &mut game,
                id,
                11,
                feet,
                rotation,
                now,
                RejectReason::InvalidRotation,
            );
        }
        rejected_move(
            &mut game,
            id,
            10,
            feet,
            Rotation::default(),
            now,
            RejectReason::StaleTick,
        );
        rejected_move(
            &mut game,
            id,
            9,
            feet,
            Rotation::default(),
            now,
            RejectReason::StaleTick,
        );
        rejected_move(
            &mut game,
            id,
            11,
            feet,
            Rotation::default(),
            now - Duration::from_millis(1),
            RejectReason::ClockRegression,
        );
    }

    #[test]
    fn swept_body_rejects_wall_tunneling_and_floor_penetration_but_allows_touching() {
        let now = Instant::now();
        let mut region = Region::flat_origin();
        region
            .commit(
                0,
                &[BlockEdit {
                    position: BlockPos { x: 1, y: 5, z: 0 },
                    block: BlockKind::Stone,
                }],
            )
            .unwrap();
        let (mut game, id) = active(region, now);
        rejected_move(
            &mut game,
            id,
            1,
            Position {
                x: 2.5,
                y: 4.0,
                z: 0.5,
            },
            Rotation::default(),
            now,
            RejectReason::Collision,
        );
        rejected_move(
            &mut game,
            id,
            1,
            Position {
                x: 0.5,
                y: 3.99,
                z: 0.5,
            },
            Rotation::default(),
            now,
            RejectReason::Collision,
        );
        accepted(game.move_player(
            id,
            1,
            Position {
                x: 0.7,
                y: 4.0,
                z: 0.5,
            },
            Rotation::default(),
            now,
        ));
        rejected_move(
            &mut game,
            id,
            2,
            Position {
                x: 0.70001,
                y: 4.0,
                z: 0.5,
            },
            Rotation::default(),
            now,
            RejectReason::Collision,
        );
        accepted(game.move_player(
            id,
            2,
            Position {
                x: 0.7,
                y: 4.0,
                z: 1.5,
            },
            Rotation::default(),
            now,
        ));
    }

    #[test]
    fn player_body_must_fit_region_not_only_its_center() {
        let now = Instant::now();
        let bounds =
            sculith_world::RegionBounds::new(ChunkPos { x: 0, z: 0 }, ChunkPos { x: 1, z: 1 })
                .unwrap();
        let (mut game, id) = active(Region::new(sculith_world::RegionId(1), bounds), now);
        rejected_move(
            &mut game,
            id,
            1,
            Position {
                x: 0.2,
                y: 4.0,
                z: 0.5,
            },
            Rotation::default(),
            now,
            RejectReason::OutsideRegion,
        );
        accepted(game.move_player(
            id,
            1,
            Position {
                x: 0.3,
                y: 4.0,
                z: 0.5,
            },
            Rotation::default(),
            now,
        ));
    }

    #[test]
    fn new_reservations_find_clearance_after_persisted_or_recent_spawn_builds() {
        let now = Instant::now();
        let mut region = Region::flat_origin();
        region
            .commit(
                0,
                &[BlockEdit {
                    position: BlockPos { x: 0, y: 4, z: 0 },
                    block: BlockKind::Stone,
                }],
            )
            .unwrap();
        let mut game = GameState::new(region, 2).unwrap();
        let first = game
            .reserve(profile(1), Permissions::default(), now)
            .unwrap();
        assert_eq!(first.feet.y, 5.0);
        game.region
            .commit(
                1,
                &[BlockEdit {
                    position: BlockPos { x: 0, y: 5, z: 0 },
                    block: BlockKind::Stone,
                }],
            )
            .unwrap();
        let second = game
            .reserve(profile(2), Permissions::default(), now)
            .unwrap();
        assert_eq!(second.feet.y, 6.0);
    }

    #[test]
    fn creative_place_break_and_noop_use_one_world_and_server_inventory() {
        let now = Instant::now();
        let (mut game, id) = active(Region::flat_origin(), now);
        let old = game.snapshot(ChunkPos { x: 0, z: 0 }).unwrap();
        let target = BlockPos { x: 1, y: 4, z: 0 };
        let BlockOutcome::Committed { batch } =
            game.place_block(id, BlockPos { y: 3, ..target }, 1, 0, BlockKind::Stone, now)
        else {
            panic!("place failed")
        };
        assert_eq!((batch.previous_revision, batch.revision), (0, 1));
        assert_eq!(batch.changes[0].after, BlockKind::Stone);
        assert_eq!(old.get(1, 4, 0).unwrap(), BlockKind::Air);
        assert_eq!(game.region.get_block(target).unwrap(), BlockKind::Stone);
        let before = game.capture();
        let result = game.place_block(id, BlockPos { y: 3, ..target }, 1, 0, BlockKind::Stone, now);
        rejected_block(&mut game, result, RejectReason::Occupied, &before);
        assert!(matches!(
            game.break_block(id, target, now),
            BlockOutcome::Committed { .. }
        ));
        let remaining = game.players[&id].building;
        assert_eq!(
            game.break_block(id, target, now),
            BlockOutcome::NoChange {
                position: target,
                current: BlockKind::Air
            }
        );
        assert_eq!(game.players[&id].building, remaining);
        assert_eq!(game.revision(), 2);
        assert_eq!(game.player(id).unwrap().hotbar[0], Some(BlockKind::Stone));
    }

    #[test]
    fn permissions_and_immutable_bedrock_are_enforced_before_committing() {
        let now = Instant::now();
        let mut region = Region::flat_origin();
        region
            .commit(
                0,
                &(1..=3)
                    .map(|y| BlockEdit {
                        position: BlockPos { x: 0, y, z: 0 },
                        block: BlockKind::Air,
                    })
                    .collect::<Vec<_>>(),
            )
            .unwrap();
        let (mut game, id) = active(region, now);
        let before = game.capture();
        let result = game.break_block(id, BlockPos { x: 0, y: 0, z: 0 }, now);
        rejected_block(&mut game, result, RejectReason::Unbreakable, &before);
        let other = game
            .reserve(profile(2), Permissions::default(), now)
            .unwrap();
        game.activate(other.id).unwrap();
        let result = game.break_block(other.id, BlockPos { x: 1, y: 3, z: 0 }, now);
        rejected_block(&mut game, result, RejectReason::PermissionDenied, &before);
    }

    #[test]
    fn reach_occlusion_bounds_and_checked_face_offsets_refuse_bad_builds() {
        let now = Instant::now();
        let mut region = Region::flat_origin();
        let mut edits: Vec<_> = (4..=6)
            .map(|y| BlockEdit {
                position: BlockPos { x: 1, y, z: 0 },
                block: BlockKind::Stone,
            })
            .collect();
        edits.push(BlockEdit {
            position: BlockPos { x: 2, y: 4, z: 0 },
            block: BlockKind::Stone,
        });
        region.commit(0, &edits).unwrap();
        let (mut game, id) = active(region, now);
        let before = game.capture();
        for (position, reason) in [
            (BlockPos { x: 2, y: 4, z: 0 }, RejectReason::Occluded),
            (BlockPos { x: 10, y: 3, z: 0 }, RejectReason::TooFar),
            (BlockPos { x: 64, y: 3, z: 0 }, RejectReason::OutsideRegion),
            (
                BlockPos {
                    x: i32::MAX,
                    y: 3,
                    z: 0,
                },
                RejectReason::InvalidPosition,
            ),
        ] {
            let result = game.break_block(id, position, now);
            rejected_block(&mut game, result, reason, &before);
        }
        for (against, face, reason) in [
            (
                BlockPos { x: 1, y: 3, z: 1 },
                255,
                RejectReason::InvalidFace,
            ),
            (
                BlockPos {
                    x: i32::MAX,
                    y: 3,
                    z: 0,
                },
                5,
                RejectReason::InvalidPosition,
            ),
            (
                BlockPos { x: -1, y: 4, z: 0 },
                1,
                RejectReason::InvalidTarget,
            ),
        ] {
            let result = game.place_block(id, against, face, 0, BlockKind::Stone, now);
            rejected_block(&mut game, result, reason, &before);
        }
    }

    #[test]
    fn selected_slot_and_server_held_kind_must_match_client_claims() {
        let now = Instant::now();
        let (mut game, id) = active(Region::flat_origin(), now);
        let against = BlockPos { x: 1, y: 3, z: 0 };
        let before = game.capture();
        let mut forged_copy = game.player(id).unwrap();
        forged_copy.hotbar[0] = Some(BlockKind::Bedrock);
        let result = game.place_block(id, against, 1, 0, BlockKind::Bedrock, now);
        rejected_block(&mut game, result, RejectReason::WrongHeldItem, &before);
        assert!(game.select_slot(id, 9).is_err());
        game.select_slot(id, 1).unwrap();
        let result = game.place_block(id, against, 1, 0, BlockKind::Stone, now);
        rejected_block(&mut game, result, RejectReason::InvalidSlot, &before);
        let result = game.place_block(id, against, 1, 1, BlockKind::Stone, now);
        rejected_block(&mut game, result, RejectReason::WrongHeldItem, &before);
        game.select_slot(id, 8).unwrap();
        let result = game.place_block(id, against, 1, 8, BlockKind::Stone, now);
        rejected_block(&mut game, result, RejectReason::WrongHeldItem, &before);
        game.select_slot(id, 1).unwrap();
        assert!(matches!(
            game.place_block(id, against, 1, 1, BlockKind::Dirt, now),
            BlockOutcome::Committed { .. }
        ));
        assert_eq!(game.player(id).unwrap().hotbar[1], Some(BlockKind::Dirt));
    }

    #[test]
    fn placing_cannot_intersect_any_active_or_pending_player_body() {
        let now = Instant::now();
        let (mut game, id) = active(Region::flat_origin(), now);
        let second = game
            .reserve(profile(2), Permissions::default(), now)
            .unwrap();
        game.activate(second.id).unwrap();
        accepted(game.move_player(
            second.id,
            1,
            Position {
                x: 1.5,
                y: 4.0,
                z: 0.5,
            },
            Rotation::default(),
            now,
        ));
        let before = game.capture();
        let result = game.place_block(
            id,
            BlockPos { x: 1, y: 3, z: 0 },
            1,
            0,
            BlockKind::Stone,
            now,
        );
        rejected_block(&mut game, result, RejectReason::Collision, &before);
        accepted(game.move_player(
            id,
            1,
            Position {
                x: 2.5,
                y: 4.0,
                z: 0.5,
            },
            Rotation::default(),
            now,
        ));
        game.reserve(profile(3), Permissions::default(), now)
            .unwrap();
        let result = game.place_block(
            id,
            BlockPos { x: 0, y: 3, z: 0 },
            1,
            0,
            BlockKind::Stone,
            now,
        );
        rejected_block(&mut game, result, RejectReason::Collision, &before);
    }

    #[test]
    fn creative_build_rate_uses_server_time_and_rejections_do_not_consume_budget() {
        let now = Instant::now();
        let (mut game, id) = active(Region::flat_origin(), now);
        for (x, z) in [(1, 0), (-1, 0), (0, 1), (0, -1)] {
            assert!(matches!(
                game.place_block(id, BlockPos { x, y: 3, z }, 1, 0, BlockKind::Stone, now),
                BlockOutcome::Committed { .. }
            ));
        }
        let before = game.capture();
        let budget = game.players[&id].building;
        let against = BlockPos { x: 1, y: 3, z: 1 };
        let result = game.place_block(id, against, 1, 0, BlockKind::Stone, now);
        rejected_block(&mut game, result, RejectReason::RateLimited, &before);
        assert_eq!(game.players[&id].building, budget);
        assert!(matches!(
            game.place_block(
                id,
                against,
                1,
                0,
                BlockKind::Stone,
                now + Duration::from_millis(50)
            ),
            BlockOutcome::Committed { .. }
        ));
        let before = game.capture();
        let result = game.break_block(id, BlockPos { y: 4, ..against }, now);
        rejected_block(&mut game, result, RejectReason::ClockRegression, &before);
    }
}
