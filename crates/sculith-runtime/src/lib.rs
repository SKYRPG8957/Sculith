//! A bounded mailbox owns the one authoritative game/region state.

pub mod observations;
pub mod peers;

use anyhow::{Context, Result};
use sculith_game::{
    BlockOutcome, GameState, MoveOutcome, Permissions, PlayerSnapshot, Position, Profile, Rotation,
};
use sculith_world::{
    BlockKind, BlockPos, ChunkPos, ChunkSnapshot, CommittedBatch, Region, RegionSnapshot,
};
use std::{
    collections::BTreeMap,
    sync::{
        Arc, Weak,
        atomic::{AtomicBool, AtomicU8, AtomicU32, Ordering},
    },
    time::Instant,
};
use tokio::sync::{broadcast, mpsc, oneshot, watch};

#[derive(Debug, Clone)]
pub enum WorldEvent {
    /// Private adapter update for a server-authored mutation, never a game hook.
    InventoryUpdated {
        player_id: u64,
    },
    PluginInventoryResult {
        plugin: String,
        player_id: u64,
        request_id: u32,
        result: sculith_core::inventory::InventoryResult,
    },
    InventoryChanged {
        player_id: u64,
        changes: Vec<sculith_game::inventory::Change>,
    },
    /// Private command invocation, routed only to its registered plugin.
    CommandSubmitted(sculith_game::commands::Submission),
    Chat(sculith_game::ChatMessage),
    PacketReceived {
        player_id: u64,
        packet: sculith_core::PacketInfo,
    },
    Joined(PlayerSnapshot),
    Equipped(PlayerSnapshot),
    Moved {
        player: PlayerSnapshot,
        tick: u64,
    },
    Left {
        id: u64,
    },
    Message {
        player_id: u64,
        message: String,
    },
    Sound(sculith_game::SoundEffect),
    ModelShown(sculith_game::ModelView),
    ModelHidden {
        player_id: u64,
        entity_id: u64,
    },
    FormRequested {
        player_id: u64,
        form_id: u32,
        form: sculith_core::SimpleForm,
    },
    /// Semantic result only; not broadcast to protocol observers.
    FormSubmitted(sculith_game::FormSubmission),
    Blocks {
        player_id: u64,
        batch: CommittedBatch,
        revisions: BTreeMap<ChunkPos, u64>,
    },
}

#[derive(Clone)]
pub struct WorldHandle {
    commands: mpsc::Sender<Command>,
    online: Arc<AtomicU32>,
    events: broadcast::Sender<Arc<WorldEvent>>,
    game_events: broadcast::Sender<Arc<WorldEvent>>,
    observations: Arc<AtomicU8>,
}

/// Snapshot and its continuation are created atomically inside the owner.
/// Updates contain Joined, Moved, Equipped and Left, with no private presentation.
pub struct PeerObservation {
    pub players: Vec<PlayerSnapshot>,
    pub updates: broadcast::Receiver<Arc<WorldEvent>>,
}

pub struct PlayerLease {
    pub player: PlayerSnapshot,
    alive: Arc<AtomicBool>,
    commands: mpsc::WeakSender<Command>,
}
impl Drop for PlayerLease {
    fn drop(&mut self) {
        self.alive.store(false, Ordering::Release);
        if let Some(commands) = self.commands.upgrade() {
            let _ = commands.try_send(Command::Retire { id: self.player.id });
        }
    }
}

enum Command {
    PlayerSnapshot {
        id: u64,
        reply: oneshot::Sender<Result<PlayerSnapshot>>,
    },
    PluginInventory {
        plugin: String,
        id: u64,
        request_id: u32,
        operation: sculith_core::inventory::InventoryOperation,
        reply: oneshot::Sender<()>,
    },
    InventoryClick {
        id: u64,
        click: sculith_game::inventory::Click,
        reply: oneshot::Sender<Result<sculith_game::InventoryOutcome>>,
    },
    Inventory {
        id: u64,
        request: sculith_game::inventory::Request,
        reply: oneshot::Sender<Result<sculith_game::InventoryOutcome>>,
    },
    CreativeSlot {
        id: u64,
        slot: u8,
        kind: Option<BlockKind>,
        count: u8,
        reply: oneshot::Sender<Result<sculith_game::InventoryOutcome>>,
    },
    RegisterCommands {
        plugin: String,
        specs: Vec<sculith_core::CommandSpec>,
        reply: oneshot::Sender<Result<()>>,
    },
    SealCommands {
        reply: oneshot::Sender<()>,
    },
    DisableCommands {
        plugin: String,
        reply: oneshot::Sender<()>,
    },
    ListCommands {
        reply: oneshot::Sender<Vec<sculith_core::CommandSpec>>,
    },
    Execute {
        id: u64,
        input: String,
        reply: oneshot::Sender<Result<()>>,
    },
    Chat {
        id: u64,
        message: String,
        reply: oneshot::Sender<Result<()>>,
    },
    ObservePacket {
        id: u64,
        packet: sculith_core::PacketInfo,
        reply: oneshot::Sender<Result<()>>,
    },
    ObservePlayers {
        id: u64,
        reply: oneshot::Sender<Result<PeerObservation>>,
    },
    Snapshot {
        position: ChunkPos,
        reply: oneshot::Sender<Result<ChunkSnapshot>>,
    },
    Capture {
        reply: oneshot::Sender<RegionSnapshot>,
    },
    Reserve {
        profile: Profile,
        permissions: Permissions,
        reply: oneshot::Sender<Result<PlayerLease>>,
    },
    Activate {
        id: u64,
        reply: oneshot::Sender<Result<PlayerSnapshot>>,
    },
    Retire {
        id: u64,
    },
    Move {
        id: u64,
        tick: u64,
        position: Position,
        rotation: Rotation,
        reply: oneshot::Sender<MoveOutcome>,
    },
    Break {
        id: u64,
        position: BlockPos,
        reply: oneshot::Sender<BlockOutcome>,
    },
    Place {
        id: u64,
        against: BlockPos,
        face: u8,
        slot: u8,
        claimed: BlockKind,
        reply: oneshot::Sender<BlockOutcome>,
    },
    Select {
        id: u64,
        slot: u8,
        reply: oneshot::Sender<Result<PlayerSnapshot>>,
    },
    SendMessage {
        id: u64,
        message: String,
        reply: oneshot::Sender<Result<()>>,
    },
    ShowModel {
        id: u64,
        model: String,
        reply: oneshot::Sender<Result<()>>,
    },
    HideModel {
        id: u64,
        reply: oneshot::Sender<Result<()>>,
    },
    PlaySound {
        id: u64,
        sound: String,
        volume: f32,
        pitch: f32,
        reply: oneshot::Sender<Result<()>>,
    },
    ShowForm {
        id: u64,
        plugin: String,
        request_id: u32,
        form: sculith_core::SimpleForm,
        reply: oneshot::Sender<Result<()>>,
    },
    SubmitForm {
        id: u64,
        form_id: u32,
        selection: Option<u32>,
        reply: oneshot::Sender<Result<()>>,
    },
}

impl WorldHandle {
    /// Adapters read current owner state when consuming an asynchronous update;
    /// a queued notification must never restore an older client transaction.
    pub async fn player_snapshot(&self, id: u64) -> Result<PlayerSnapshot> {
        let (reply, receiver) = oneshot::channel();
        self.commands
            .send(Command::PlayerSnapshot { id, reply })
            .await
            .context("world owner stopped")?;
        receiver.await.context("player snapshot response lost")?
    }

    pub async fn plugin_inventory(
        &self,
        plugin: String,
        id: u64,
        request_id: u32,
        operation: sculith_core::inventory::InventoryOperation,
    ) -> Result<()> {
        anyhow::ensure!(
            !plugin.is_empty()
                && plugin.len() <= 64
                && plugin
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b)),
            "invalid inventory request owner"
        );
        operation.validate().map_err(anyhow::Error::msg)?;
        let (reply, receiver) = oneshot::channel();
        self.commands
            .send(Command::PluginInventory {
                plugin,
                id,
                request_id,
                operation,
                reply,
            })
            .await
            .context("world owner stopped")?;
        receiver.await.context("inventory dispatch response lost")
    }

    pub async fn inventory_click(
        &self,
        id: u64,
        click: sculith_game::inventory::Click,
    ) -> Result<sculith_game::InventoryOutcome> {
        let (reply, receiver) = oneshot::channel();
        self.commands
            .send(Command::InventoryClick { id, click, reply })
            .await
            .context("world owner stopped")?;
        receiver.await.context("inventory click response lost")?
    }
    pub async fn inventory_request(
        &self,
        id: u64,
        request: sculith_game::inventory::Request,
    ) -> Result<sculith_game::InventoryOutcome> {
        let (reply, receiver) = oneshot::channel();
        self.commands
            .send(Command::Inventory { id, request, reply })
            .await
            .context("world owner stopped")?;
        receiver.await.context("inventory response lost")?
    }
    pub async fn creative_slot(
        &self,
        id: u64,
        slot: u8,
        kind: Option<BlockKind>,
        count: u8,
    ) -> Result<sculith_game::InventoryOutcome> {
        let (reply, receiver) = oneshot::channel();
        self.commands
            .send(Command::CreativeSlot {
                id,
                slot,
                kind,
                count,
                reply,
            })
            .await
            .context("world owner stopped")?;
        receiver.await.context("creative inventory response lost")?
    }
    pub async fn register_commands(
        &self,
        plugin: String,
        specs: Vec<sculith_core::CommandSpec>,
    ) -> Result<()> {
        let (reply, receiver) = oneshot::channel();
        self.commands
            .send(Command::RegisterCommands {
                plugin,
                specs,
                reply,
            })
            .await
            .context("world owner stopped")?;
        receiver.await.context("command registration lost")?
    }
    pub async fn seal_commands(&self) -> Result<()> {
        let (reply, receiver) = oneshot::channel();
        self.commands
            .send(Command::SealCommands { reply })
            .await
            .context("world owner stopped")?;
        receiver.await.context("command seal lost")
    }
    pub async fn disable_commands(&self, plugin: String) -> Result<()> {
        let (reply, receiver) = oneshot::channel();
        self.commands
            .send(Command::DisableCommands { plugin, reply })
            .await
            .context("world owner stopped")?;
        receiver.await.context("command disable lost")
    }
    pub async fn command_specs(&self) -> Result<Vec<sculith_core::CommandSpec>> {
        let (reply, receiver) = oneshot::channel();
        self.commands
            .send(Command::ListCommands { reply })
            .await
            .context("world owner stopped")?;
        receiver.await.context("command listing lost")
    }
    pub async fn submit_command(&self, id: u64, input: String) -> Result<()> {
        let (reply, receiver) = oneshot::channel();
        self.commands
            .send(Command::Execute { id, input, reply })
            .await
            .context("world owner stopped")?;
        receiver.await.context("command submission lost")?
    }
    pub async fn chat(&self, id: u64, message: String) -> Result<()> {
        let (reply, receiver) = oneshot::channel();
        self.commands
            .send(Command::Chat { id, message, reply })
            .await
            .context("world owner stopped")?;
        receiver.await.context("chat submission lost")?
    }
    pub fn configure_observations(&self, flags: u8) {
        self.observations
            .store(flags & crate::observations::ALL, Ordering::Release);
    }
    pub fn observes_packets(&self) -> bool {
        self.observations.load(Ordering::Acquire) & crate::observations::PACKETS != 0
    }
    pub async fn observe_packet(&self, id: u64, packet: sculith_core::PacketInfo) -> Result<()> {
        if !self.observes_packets() {
            return Ok(());
        }
        packet.validate().map_err(anyhow::Error::msg)?;
        let (reply, receiver) = oneshot::channel();
        self.commands
            .send(Command::ObservePacket { id, packet, reply })
            .await
            .context("world owner stopped")?;
        receiver.await.context("packet observation lost")?
    }
    pub async fn observe_players(&self, id: u64) -> Result<PeerObservation> {
        let (reply, receiver) = oneshot::channel();
        self.commands
            .send(Command::ObservePlayers { id, reply })
            .await
            .context("world owner stopped")?;
        receiver.await.context("peer observation lost")?
    }

    pub async fn show_model(&self, id: u64, model: String) -> Result<()> {
        sculith_core::validate_resource_id(&model).map_err(anyhow::Error::msg)?;
        let (reply, receiver) = oneshot::channel();
        self.commands
            .send(Command::ShowModel { id, model, reply })
            .await
            .context("world owner stopped")?;
        receiver.await.context("model response lost")?
    }
    pub async fn hide_model(&self, id: u64) -> Result<()> {
        let (reply, receiver) = oneshot::channel();
        self.commands
            .send(Command::HideModel { id, reply })
            .await
            .context("world owner stopped")?;
        receiver.await.context("model response lost")?
    }

    pub async fn play_sound(&self, id: u64, sound: String, volume: f32, pitch: f32) -> Result<()> {
        sculith_core::validate_sound_id(&sound).map_err(anyhow::Error::msg)?;
        sculith_core::validate_sound_parameters(volume, pitch).map_err(anyhow::Error::msg)?;
        let (reply, receiver) = oneshot::channel();
        self.commands
            .send(Command::PlaySound {
                id,
                sound,
                volume,
                pitch,
                reply,
            })
            .await
            .context("world owner stopped")?;
        receiver.await.context("sound response lost")?
    }

    pub async fn show_form(
        &self,
        id: u64,
        plugin: String,
        request_id: u32,
        form: sculith_core::SimpleForm,
    ) -> Result<()> {
        form.validate().map_err(anyhow::Error::msg)?;
        let (reply, receiver) = oneshot::channel();
        self.commands
            .send(Command::ShowForm {
                id,
                plugin,
                request_id,
                form,
                reply,
            })
            .await
            .context("world owner stopped")?;
        receiver.await.context("form response lost")?
    }

    pub async fn submit_form(&self, id: u64, form_id: u32, selection: Option<u32>) -> Result<()> {
        let (reply, receiver) = oneshot::channel();
        self.commands
            .send(Command::SubmitForm {
                id,
                form_id,
                selection,
                reply,
            })
            .await
            .context("world owner stopped")?;
        receiver.await.context("form submission response lost")?
    }
    pub async fn snapshot(&self, position: ChunkPos) -> Result<ChunkSnapshot> {
        let (reply, receiver) = oneshot::channel();
        self.commands
            .send(Command::Snapshot { position, reply })
            .await
            .context("world owner stopped")?;
        receiver.await.context("snapshot response lost")?
    }
    pub async fn capture(&self) -> Result<RegionSnapshot> {
        let (reply, receiver) = oneshot::channel();
        self.commands
            .send(Command::Capture { reply })
            .await
            .context("world owner stopped")?;
        receiver.await.context("capture response lost")
    }
    pub async fn reserve(&self, profile: Profile, permissions: Permissions) -> Result<PlayerLease> {
        let (reply, receiver) = oneshot::channel();
        self.commands
            .send(Command::Reserve {
                profile,
                permissions,
                reply,
            })
            .await
            .context("world owner stopped")?;
        receiver.await.context("registration response lost")?
    }
    pub async fn activate(&self, id: u64) -> Result<PlayerSnapshot> {
        let (reply, receiver) = oneshot::channel();
        self.commands
            .send(Command::Activate { id, reply })
            .await
            .context("world owner stopped")?;
        receiver.await.context("activation response lost")?
    }
    pub async fn move_player(
        &self,
        id: u64,
        tick: u64,
        position: Position,
        rotation: Rotation,
    ) -> Result<MoveOutcome> {
        let (reply, receiver) = oneshot::channel();
        self.commands
            .send(Command::Move {
                id,
                tick,
                position,
                rotation,
                reply,
            })
            .await
            .context("world owner stopped")?;
        receiver.await.context("movement response lost")
    }
    pub async fn break_block(&self, id: u64, position: BlockPos) -> Result<BlockOutcome> {
        let (reply, receiver) = oneshot::channel();
        self.commands
            .send(Command::Break {
                id,
                position,
                reply,
            })
            .await
            .context("world owner stopped")?;
        receiver.await.context("break response lost")
    }
    pub async fn place_block(
        &self,
        id: u64,
        against: BlockPos,
        face: u8,
        slot: u8,
        claimed: BlockKind,
    ) -> Result<BlockOutcome> {
        let (reply, receiver) = oneshot::channel();
        self.commands
            .send(Command::Place {
                id,
                against,
                face,
                slot,
                claimed,
                reply,
            })
            .await
            .context("world owner stopped")?;
        receiver.await.context("placement response lost")
    }
    pub async fn select_slot(&self, id: u64, slot: u8) -> Result<PlayerSnapshot> {
        let (reply, receiver) = oneshot::channel();
        self.commands
            .send(Command::Select { id, slot, reply })
            .await
            .context("world owner stopped")?;
        receiver.await.context("selection response lost")?
    }
    pub fn online_players(&self) -> u32 {
        self.online.load(Ordering::Acquire)
    }
    pub fn subscribe(&self) -> broadcast::Receiver<Arc<WorldEvent>> {
        self.events.subscribe()
    }

    /// Semantic game hooks exclude high-frequency movement and private messages.
    pub fn subscribe_game(&self) -> broadcast::Receiver<Arc<WorldEvent>> {
        self.game_events.subscribe()
    }

    pub async fn send_message(&self, id: u64, message: String) -> Result<()> {
        sculith_core::validate_system_message(&message).map_err(anyhow::Error::msg)?;
        let (reply, receiver) = oneshot::channel();
        self.commands
            .send(Command::SendMessage { id, message, reply })
            .await
            .context("world owner stopped")?;
        receiver.await.context("message response lost")?
    }
}

fn publish_game(
    event: WorldEvent,
    events: &broadcast::Sender<Arc<WorldEvent>>,
    game_events: &broadcast::Sender<Arc<WorldEvent>>,
) {
    let event = Arc::new(event);
    let _ = events.send(event.clone());
    let _ = game_events.send(event);
}

fn publish_presence(
    event: WorldEvent,
    events: &broadcast::Sender<Arc<WorldEvent>>,
    game_events: &broadcast::Sender<Arc<WorldEvent>>,
    peer_events: &broadcast::Sender<Arc<WorldEvent>>,
) {
    let event = Arc::new(event);
    let _ = events.send(event.clone());
    let _ = game_events.send(event.clone());
    let _ = peer_events.send(event);
}

fn publish_inventory(
    previous: Option<&PlayerSnapshot>,
    outcome: &sculith_game::InventoryOutcome,
    game_events: &broadcast::Sender<Arc<WorldEvent>>,
    peer_events: &broadcast::Sender<Arc<WorldEvent>>,
    flags: u8,
) {
    let Ok(changes) = &outcome.response.result else {
        return;
    };
    if outcome.response.replayed {
        return;
    }
    if flags & crate::observations::INVENTORY != 0 {
        let changes: Vec<_> = changes
            .iter()
            .filter(|change| {
                previous.is_none_or(|previous| {
                    let before = match change.slot {
                        sculith_game::inventory::Slot::Inventory(slot) => {
                            previous.inventory[usize::from(slot)]
                        }
                        sculith_game::inventory::Slot::Cursor => previous.cursor,
                        sculith_game::inventory::Slot::CreatedOutput => previous.created_output,
                    };
                    // Response slots also include non-mutating mining checks
                    // and stack-ID refreshes. Plugins observe semantic changes.
                    before.map(|stack| (stack.kind, stack.count))
                        != change.stack.map(|stack| (stack.kind, stack.count))
                })
            })
            .copied()
            .collect();
        if !changes.is_empty() {
            let _ = game_events.send(Arc::new(WorldEvent::InventoryChanged {
                player_id: outcome.player.id,
                changes,
            }));
        }
    }
    let held = |player: &PlayerSnapshot| {
        (
            player.selected_slot,
            player.held().map(|stack| (stack.kind, stack.count)),
        )
    };
    if previous.map(held) != Some(held(&outcome.player)) {
        let event = Arc::new(WorldEvent::Equipped(outcome.player.clone()));
        if flags & crate::observations::EQUIPMENT != 0 {
            let _ = game_events.send(event.clone());
        }
        let _ = peer_events.send(event);
    }
}

fn publish_blocks(
    state: &mut GameState,
    events: &broadcast::Sender<Arc<WorldEvent>>,
    game_events: &broadcast::Sender<Arc<WorldEvent>>,
    player_id: u64,
    outcome: &BlockOutcome,
) -> Result<()> {
    if let BlockOutcome::Committed { batch } = outcome {
        let mut revisions = BTreeMap::new();
        for change in &batch.changes {
            let position = change.position.chunk();
            if let std::collections::btree_map::Entry::Vacant(entry) = revisions.entry(position) {
                entry.insert(state.snapshot(position)?.revision());
            }
        }
        publish_game(
            WorldEvent::Blocks {
                player_id,
                batch: batch.clone(),
                revisions,
            },
            events,
            game_events,
        );
    }
    Ok(())
}

fn retire(
    state: &mut GameState,
    leases: &mut BTreeMap<u64, Weak<AtomicBool>>,
    events: &broadcast::Sender<Arc<WorldEvent>>,
    game_events: &broadcast::Sender<Arc<WorldEvent>>,
    peer_events: &broadcast::Sender<Arc<WorldEvent>>,
    id: u64,
) {
    leases.remove(&id);
    if let Some(player) = state.retire(id)
        && player.initialized
    {
        publish_presence(WorldEvent::Left { id }, events, game_events, peer_events);
    }
}

pub fn with_region(
    region: Region,
    max_players: u32,
    shutdown: watch::Receiver<bool>,
) -> Result<(WorldHandle, impl Future<Output = Result<()>> + Send)> {
    with_region_and_sounds(region, max_players, shutdown, Default::default())
}

pub fn with_region_and_sounds(
    region: Region,
    max_players: u32,
    shutdown: watch::Receiver<bool>,
    sounds: std::collections::BTreeSet<String>,
) -> Result<(WorldHandle, impl Future<Output = Result<()>> + Send)> {
    with_region_and_content(region, max_players, shutdown, sounds, Default::default())
}

pub fn with_region_and_content(
    region: Region,
    max_players: u32,
    mut shutdown: watch::Receiver<bool>,
    sounds: std::collections::BTreeSet<String>,
    models: std::collections::BTreeSet<String>,
) -> Result<(WorldHandle, impl Future<Output = Result<()>> + Send)> {
    let mut state = GameState::with_content(region, max_players, sounds, models)?;
    let mut command_registry = sculith_game::commands::Registry::default();
    let (commands, mut requests) = mpsc::channel(32);
    let weak_commands = commands.downgrade();
    let (events, _) = broadcast::channel(128);
    let (game_events, _) = broadcast::channel(128);
    let (peer_events, _) = broadcast::channel(128);
    let online = Arc::new(AtomicU32::new(0));
    let observations = Arc::new(AtomicU8::new(0));
    let handle = WorldHandle {
        commands,
        online: online.clone(),
        events: events.clone(),
        game_events: game_events.clone(),
        observations: observations.clone(),
    };
    let task = async move {
        let mut leases: BTreeMap<u64, Weak<AtomicBool>> = BTreeMap::new();
        let result = loop {
            let dead: Vec<_> = leases
                .iter()
                .filter_map(|(&id, flag)| {
                    flag.upgrade()
                        .is_none_or(|flag| !flag.load(Ordering::Acquire))
                        .then_some(id)
                })
                .collect();
            for id in dead {
                retire(
                    &mut state,
                    &mut leases,
                    &events,
                    &game_events,
                    &peer_events,
                    id,
                );
            }
            online.store(state.online_count(), Ordering::Release);
            tokio::select! {
                stopped = shutdown.changed() => {
                    if stopped.is_err() || *shutdown.borrow() { break Ok(()); }
                }
                command = requests.recv() => {
                    // Leases may have died while recv was suspended. Retire them
                    // before accepting a queued intent from a closed connection.
                    let dead: Vec<_> = leases.iter().filter_map(|(&id, flag)|
                        flag.upgrade().is_none_or(|flag| !flag.load(Ordering::Acquire)).then_some(id)
                    ).collect();
                    for id in dead { retire(&mut state, &mut leases, &events, &game_events, &peer_events, id); }
                    match command {
                        Some(Command::PlayerSnapshot {id,reply})=>{
                            let result=state.player(id).filter(|p|p.initialized).ok_or_else(||anyhow::anyhow!("player is not initialized"));
                            let _=reply.send(result);
                        }
                        Some(Command::PluginInventory {plugin,id,request_id,operation,reply})=>{
                            use sculith_core::inventory::InventoryResult;
                            let previous=state.player(id);
                            let result=match state.plugin_inventory(id,&operation,Instant::now()) {
                                Ok(outcome)=>{
                                    if let Ok(changes)=&outcome.response.result && !changes.is_empty() {
                                        let _=events.send(Arc::new(WorldEvent::InventoryUpdated {
                                            player_id:id,
                                        }));
                                    }
                                    publish_inventory(previous.as_ref(),&outcome,&game_events,&peer_events,observations.load(Ordering::Acquire));
                                    InventoryResult::Snapshot {inventory:outcome.player.inventory_view()}
                                }
                                Err(reason)=>InventoryResult::Rejected {reason},
                            };
                            let _=game_events.send(Arc::new(WorldEvent::PluginInventoryResult {plugin,player_id:id,request_id,result}));
                            let _=reply.send(());
                        }
                        Some(Command::InventoryClick {id,click,reply})=>{
                            let previous=state.player(id);
                            let result=state.inventory_click(id,click,Instant::now());
                            if let Ok(outcome)=&result {publish_inventory(previous.as_ref(),outcome,&game_events,&peer_events,observations.load(Ordering::Acquire));}
                            let _=reply.send(result);
                        }
                        Some(Command::Inventory {id,request,reply})=>{
                            let previous=state.player(id);
                            let result=state.inventory_request(id,request,Instant::now());
                            if let Ok(outcome)=&result {publish_inventory(previous.as_ref(),outcome,&game_events,&peer_events,observations.load(Ordering::Acquire));}
                            let _=reply.send(result);
                        }
                        Some(Command::CreativeSlot {id,slot,kind,count,reply})=>{
                            let previous=state.player(id);
                            let result=state.creative_slot(id,slot,kind,count,Instant::now());
                            if let Ok(outcome)=&result {publish_inventory(previous.as_ref(),outcome,&game_events,&peer_events,observations.load(Ordering::Acquire));}
                            let _=reply.send(result);
                        }
                        Some(Command::RegisterCommands {plugin,specs,reply})=>{let _=reply.send(command_registry.register(&plugin,specs));}
                        Some(Command::SealCommands {reply})=>{command_registry.seal();let _=reply.send(());}
                        Some(Command::DisableCommands {plugin,reply})=>{command_registry.disable(&plugin);let _=reply.send(());}
                        Some(Command::ListCommands {reply})=>{let _=reply.send(command_registry.list());}
                        Some(Command::Execute {id,input,reply})=>{
                            let result=state.permit_text_input(id,&input,Instant::now()).and_then(|player| {
                                match command_registry.route(&input) {
                                    sculith_game::commands::Route::Plugin {owner,command,arguments}=>{let _=game_events.send(Arc::new(WorldEvent::CommandSubmitted(sculith_game::commands::Submission {player_id:id,name:player.profile.name,plugin:owner,command,arguments})));}
                                    sculith_game::commands::Route::Feedback(message)=>{
                                        state.permit_message(id,Instant::now())?;
                                        let _=events.send(Arc::new(WorldEvent::Message {player_id:id,message}));
                                    }
                                }
                                Ok(())
                            });let _=reply.send(result);
                        }
                        Some(Command::Chat {id,message,reply})=>{
                            let result=state.chat(id,message,Instant::now()).map(|chat|publish_game(WorldEvent::Chat(chat),&events,&game_events));
                            let _=reply.send(result);
                        }
                        Some(Command::ObservePacket {id,packet,reply})=>{
                            let result=if state.player(id).is_some_and(|p|p.initialized && matches!((&p.profile.edition,&packet.edition),(sculith_game::Edition::Java,sculith_core::Edition::Java)|(sculith_game::Edition::Bedrock,sculith_core::Edition::Bedrock))) {
                                if observations.load(Ordering::Acquire) & crate::observations::PACKETS !=0 {
                                    let _=game_events.send(Arc::new(WorldEvent::PacketReceived {player_id:id,packet}));
                                }
                                Ok(())
                            } else {Err(anyhow::anyhow!("packet observer requires a live initialized player"))};
                            let _=reply.send(result);
                        }
                        Some(Command::ObservePlayers {id,reply})=>{
                            let result=if state.player(id).is_some_and(|p|p.initialized) {
                                // There is no await or external publisher between
                                // capturing membership and subscribing to changes.
                                let players=state.initialized_players().into_iter().filter(|p|p.id!=id).collect();
                                Ok(PeerObservation {players,updates:peer_events.subscribe()})
                            } else {Err(anyhow::anyhow!("peer observer must be a live initialized player"))};
                            let _=reply.send(result);
                        }
                        Some(Command::Snapshot { position, reply }) => { let _ = reply.send(state.snapshot(position).map_err(Into::into)); }
                        Some(Command::Capture { reply }) => { let _ = reply.send(state.capture()); }
                        Some(Command::Reserve { profile, permissions, reply }) => {
                            command_registry.seal();
                            let reservation = state.reserve(profile, permissions, Instant::now()).map(|player| {
                                let alive = Arc::new(AtomicBool::new(true));
                                leases.insert(player.id, Arc::downgrade(&alive));
                                PlayerLease { player, alive, commands: weak_commands.clone() }
                            });
                            let _ = reply.send(reservation);
                        }
                        Some(Command::Activate { id, reply }) => {
                            let was_ready = state.player(id).is_some_and(|p| p.initialized);
                            let result = state.activate(id);
                            if !was_ready && let Ok(player) = &result { publish_presence(WorldEvent::Joined(player.clone()), &events, &game_events, &peer_events); }
                            online.store(state.online_count(), Ordering::Release);
                            let _ = reply.send(result);
                        }
                        Some(Command::Retire { id }) => { retire(&mut state, &mut leases, &events, &game_events, &peer_events, id); }
                        Some(Command::Move { id, tick, position, rotation, reply }) => {
                            let outcome = state.move_player(id, tick, position, rotation, Instant::now());
                            if let MoveOutcome::Accepted { player, tick, changed: true } = &outcome {
                                let event=Arc::new(WorldEvent::Moved { player: player.clone(), tick: *tick });
                                let _ = events.send(event.clone());
                                if observations.load(Ordering::Acquire) & crate::observations::MOVEMENT !=0 {let _=game_events.send(event.clone());}
                                let _ = peer_events.send(event);
                            }
                            let _ = reply.send(outcome);
                        }
                        Some(Command::Break { id, position, reply }) => {
                            let outcome = state.break_block(id, position, Instant::now());
                            publish_blocks(&mut state, &events, &game_events, id, &outcome)?;
                            let _ = reply.send(outcome);
                        }
                        Some(Command::Place { id, against, face, slot, claimed, reply }) => {
                            let outcome = state.place_block(id, against, face, slot, claimed, Instant::now());
                            publish_blocks(&mut state, &events, &game_events, id, &outcome)?;
                            let _ = reply.send(outcome);
                        }
                        Some(Command::Select { id, slot, reply }) => {
                            let previous=state.player(id).map(|p|p.selected_slot);
                            let result=state.select_slot(id,slot);
                            if let Ok(player)=&result && previous!=Some(player.selected_slot) {
                                // Public presentation only: no region revision or
                                // plugin-game feedback for a selected hotbar slot.
                                let event=Arc::new(WorldEvent::Equipped(player.clone()));
                                if observations.load(Ordering::Acquire) & crate::observations::EQUIPMENT !=0 {let _=game_events.send(event.clone());}
                                let _=peer_events.send(event);
                            }
                            let _=reply.send(result);
                        }
                        Some(Command::SendMessage { id, message, reply }) => {
                            let result = sculith_core::validate_system_message(&message).map_err(anyhow::Error::msg)
                                .and_then(|()| state.permit_message(id, Instant::now()));
                            if result.is_ok() { let _ = events.send(Arc::new(WorldEvent::Message { player_id: id, message })); }
                            let _ = reply.send(result);
                        }
                        Some(Command::ShowModel {id,model,reply})=>{
                            let result=state.show_model(id,model,Instant::now()).map(|view| { let _=events.send(Arc::new(WorldEvent::ModelShown(view))); });
                            let _=reply.send(result);
                        }
                        Some(Command::HideModel {id,reply})=>{
                            let result=state.hide_model(id).map(|view| { if let Some(entity_id)=view { let _=events.send(Arc::new(WorldEvent::ModelHidden { player_id:id,entity_id })); } });
                            let _=reply.send(result);
                        }
                        Some(Command::PlaySound { id, sound, volume, pitch, reply }) => {
                            let result = state.play_sound(id, sound, volume, pitch, Instant::now()).map(|effect| {
                                let _ = events.send(Arc::new(WorldEvent::Sound(effect)));
                            });
                            let _ = reply.send(result);
                        }
                        Some(Command::ShowForm { id, plugin, request_id, form, reply }) => {
                            let result = state.show_form(id, plugin, request_id, &form, Instant::now());
                            let result = result.map(|form_id| { let _ = events.send(Arc::new(WorldEvent::FormRequested { player_id: id, form_id, form })); });
                            let _ = reply.send(result);
                        }
                        Some(Command::SubmitForm { id, form_id, selection, reply }) => {
                            let result = state.submit_form(id, form_id, selection, Instant::now()).map(|submission| {
                                let _ = game_events.send(Arc::new(WorldEvent::FormSubmitted(submission)));
                            });
                            let _ = reply.send(result);
                        }
                        None => break Ok(()),
                    }
                }
            }
        };
        online.store(0, Ordering::Release);
        result
    };
    Ok((handle, task))
}

pub fn create(
    shutdown: watch::Receiver<bool>,
) -> (WorldHandle, impl Future<Output = Result<()>> + Send) {
    with_region(Region::flat_origin(), 20, shutdown).expect("fixed test world")
}

#[cfg(test)]
mod tests;
