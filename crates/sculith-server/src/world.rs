//! A bounded mailbox owns the one authoritative game/region state.
use crate::game::{
    BlockOutcome, GameState, MoveOutcome, Permissions, PlayerSnapshot, Position, Profile, Rotation,
};
use anyhow::{Context, Result};
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
    InventoryChanged {
        player_id: u64,
        changes: Vec<crate::inventory::Change>,
    },
    /// Private command invocation, routed only to its registered plugin.
    CommandSubmitted(crate::commands::Submission),
    Chat(crate::game::ChatMessage),
    PacketReceived {
        player_id: u64,
        packet: sculith_plugin_api::PacketInfo,
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
    Sound(crate::game::SoundEffect),
    ModelShown(crate::game::ModelView),
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
    FormSubmitted(crate::game::FormSubmission),
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
    InventoryClick {
        id: u64,
        click: crate::inventory::Click,
        reply: oneshot::Sender<Result<crate::game::InventoryOutcome>>,
    },
    Inventory {
        id: u64,
        request: crate::inventory::Request,
        reply: oneshot::Sender<Result<crate::game::InventoryOutcome>>,
    },
    CreativeSlot {
        id: u64,
        slot: u8,
        kind: Option<BlockKind>,
        count: u8,
        reply: oneshot::Sender<Result<crate::game::InventoryOutcome>>,
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
        packet: sculith_plugin_api::PacketInfo,
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
    pub async fn inventory_click(
        &self,
        id: u64,
        click: crate::inventory::Click,
    ) -> Result<crate::game::InventoryOutcome> {
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
        request: crate::inventory::Request,
    ) -> Result<crate::game::InventoryOutcome> {
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
    ) -> Result<crate::game::InventoryOutcome> {
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
    pub async fn observe_packet(
        &self,
        id: u64,
        packet: sculith_plugin_api::PacketInfo,
    ) -> Result<()> {
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
    outcome: &crate::game::InventoryOutcome,
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
                        crate::inventory::Slot::Inventory(slot) => {
                            previous.inventory[usize::from(slot)]
                        }
                        crate::inventory::Slot::Cursor => previous.cursor,
                        crate::inventory::Slot::CreatedOutput => previous.created_output,
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
    let mut command_registry = crate::commands::Registry::default();
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
                                    crate::commands::Route::Plugin {owner,command,arguments}=>{let _=game_events.send(Arc::new(WorldEvent::CommandSubmitted(crate::commands::Submission {player_id:id,name:player.profile.name,plugin:owner,command,arguments})));}
                                    crate::commands::Route::Feedback(message)=>{
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
                            let result=if state.player(id).is_some_and(|p|p.initialized && matches!((&p.profile.edition,&packet.edition),(crate::game::Edition::Java,sculith_plugin_api::Edition::Java)|(crate::game::Edition::Bedrock,sculith_plugin_api::Edition::Bedrock))) {
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
mod tests {
    use super::*;
    use crate::game::Edition;

    #[tokio::test]
    async fn inventory_commits_are_private_with_counted_public_hands_and_idempotent_replays() {
        use crate::inventory::{Action, Request, Slot, SlotRef};
        let (stop, receiver) = watch::channel(false);
        let (world, task) = create(receiver);
        let task = tokio::spawn(task);
        let actor = world
            .reserve(profile("inventory"), Permissions { can_build: true })
            .await
            .unwrap();
        let viewer = world
            .reserve(profile("viewer"), Permissions::default())
            .await
            .unwrap();
        let id = actor.player.id;
        assert!(
            world
                .creative_slot(id, 0, Some(BlockKind::Dirt), 5)
                .await
                .is_err()
        );
        world.activate(id).await.unwrap();
        world.activate(viewer.player.id).await.unwrap();
        world.configure_observations(
            crate::observations::INVENTORY | crate::observations::EQUIPMENT,
        );
        let mut hooks = world.subscribe_game();
        let mut normal = world.subscribe();
        let mut peers = world.observe_players(viewer.player.id).await.unwrap();
        let revision = world.capture().await.unwrap().revision();
        let request = Request {
            id: -1,
            actions: vec![
                Action::Destroy {
                    source: SlotRef {
                        slot: Slot::Inventory(0),
                        expected_id: 1,
                    },
                    count: 64,
                },
                Action::CraftCreative { catalogue_id: 2 },
                Action::Transfer {
                    source: SlotRef {
                        slot: Slot::CreatedOutput,
                        expected_id: -1,
                    },
                    destination: SlotRef {
                        slot: Slot::Inventory(0),
                        expected_id: 0,
                    },
                    count: 5,
                },
            ],
        };
        let outcome = world.inventory_request(id, request.clone()).await.unwrap();
        assert!(outcome.response.result.is_ok());
        assert_eq!(
            outcome.player.held().map(|s| (s.kind, s.count)),
            Some((BlockKind::Dirt, 5))
        );
        assert_eq!(outcome.player.hotbar[0], Some(BlockKind::Dirt));
        assert!(
            matches!(hooks.recv().await.unwrap().as_ref(),WorldEvent::InventoryChanged {player_id,..} if *player_id==id)
        );
        assert!(
            matches!(hooks.recv().await.unwrap().as_ref(),WorldEvent::Equipped(p) if p.held().unwrap().count==5)
        );
        assert!(
            matches!(peers.updates.recv().await.unwrap().as_ref(),WorldEvent::Equipped(p) if p.id==id)
        );
        assert!(normal.try_recv().is_err() && peers.updates.try_recv().is_err());
        assert!(
            world
                .inventory_request(id, request)
                .await
                .unwrap()
                .response
                .replayed
        );
        assert!(hooks.try_recv().is_err() && peers.updates.try_recv().is_err());
        let invalid = Request {
            id: -2,
            actions: vec![Action::Destroy {
                source: SlotRef {
                    slot: Slot::Inventory(0),
                    expected_id: 99999,
                },
                count: 5,
            }],
        };
        let rejected = world.inventory_request(id, invalid).await.unwrap();
        assert!(rejected.response.result.is_err());
        assert_eq!(rejected.player.inventory, outcome.player.inventory);
        assert!(hooks.try_recv().is_err() && peers.updates.try_recv().is_err());
        for request in [
            Request {
                id: -3,
                actions: vec![Action::Mine {
                    source: SlotRef {
                        slot: Slot::Inventory(0),
                        expected_id: outcome.player.held().unwrap().net_id,
                    },
                }],
            },
            Request {
                id: -4,
                actions: vec![Action::Swap {
                    source: SlotRef {
                        slot: Slot::Inventory(10),
                        expected_id: 0,
                    },
                    destination: SlotRef {
                        slot: Slot::Inventory(11),
                        expected_id: 0,
                    },
                }],
            },
        ] {
            let response = world.inventory_request(id, request).await.unwrap();
            assert!(!response.response.result.unwrap().is_empty());
            assert!(hooks.try_recv().is_err() && peers.updates.try_recv().is_err());
        }
        assert_eq!(world.capture().await.unwrap().revision(), revision);
        assert!(
            world
                .creative_slot(viewer.player.id, 0, Some(BlockKind::Grass), 2)
                .await
                .unwrap()
                .response
                .result
                .is_err()
        );
        drop(actor);
        assert!(
            world
                .creative_slot(id, 0, Some(BlockKind::Stone), 1)
                .await
                .is_err()
        );
        stop.send(true).unwrap();
        task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn command_arguments_only_enter_the_registered_plugin_stream() {
        let (stop, receiver) = watch::channel(false);
        let (world, task) = create(receiver);
        let task = tokio::spawn(task);
        world
            .register_commands(
                "owner".into(),
                vec![sculith_core::CommandSpec {
                    name: "hello".into(),
                    description: "Say hello".into(),
                }],
            )
            .await
            .unwrap();
        let lease = world
            .reserve(profile("author"), Permissions::default())
            .await
            .unwrap();
        let id = lease.player.id;
        assert!(
            world
                .submit_command(id, "hello secret".into())
                .await
                .is_err()
        );
        world.activate(id).await.unwrap();
        let mut public = world.subscribe();
        let mut hooks = world.subscribe_game();
        let mut peers = world.observe_players(id).await.unwrap();
        world
            .submit_command(id, "/hello secret 😀".into())
            .await
            .unwrap();
        assert!(
            matches!(hooks.recv().await.unwrap().as_ref(),WorldEvent::CommandSubmitted(c) if c.player_id==id && c.plugin=="owner" && c.command=="hello" && c.arguments=="secret 😀" && c.name=="author")
        );
        assert!(public.try_recv().is_err() && peers.updates.try_recv().is_err());
        world.disable_commands("owner".into()).await.unwrap();
        world
            .submit_command(id, "hello secret".into())
            .await
            .unwrap();
        assert!(hooks.try_recv().is_err());
        assert!(
            matches!(public.recv().await.unwrap().as_ref(),WorldEvent::Message {player_id,message} if *player_id==id && message=="This command is temporarily unavailable.")
        );
        drop(lease);
        assert!(world.submit_command(id, "help".into()).await.is_err());
        stop.send(true).unwrap();
        task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn chat_requires_an_initialized_live_author_and_rejects_private_commands() {
        let (stop, receiver) = watch::channel(false);
        let (world, task) = create(receiver);
        let task = tokio::spawn(task);
        let lease = world
            .reserve(profile("author"), Permissions::default())
            .await
            .unwrap();
        let id = lease.player.id;
        assert!(world.chat(id, "before join".into()).await.is_err());
        world.activate(id).await.unwrap();
        let mut events = world.subscribe();
        let mut hooks = world.subscribe_game();
        let mut peers = world.observe_players(id).await.unwrap();
        let revision = world.capture().await.unwrap().revision();
        assert!(world.chat(id, "/password secret".into()).await.is_err());
        assert!(world.chat(id, "line\nbreak".into()).await.is_err());
        assert!(events.try_recv().is_err() && hooks.try_recv().is_err());
        world.chat(id, "안녕 😀".into()).await.unwrap();
        assert!(
            matches!(events.recv().await.unwrap().as_ref(),WorldEvent::Chat(c) if c.player_id==id && c.name=="author" && c.message=="안녕 😀")
        );
        assert!(
            matches!(hooks.recv().await.unwrap().as_ref(),WorldEvent::Chat(c) if c.player_id==id)
        );
        assert!(peers.updates.try_recv().is_err());
        world.chat(id, "last burst".into()).await.unwrap();
        assert!(world.chat(id, "over budget".into()).await.is_err());
        assert_eq!(world.capture().await.unwrap().revision(), revision);
        drop(lease);
        assert!(world.chat(id, "retired".into()).await.is_err());
        stop.send(true).unwrap();
        task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn opted_in_observations_are_owner_validated_and_never_enter_private_or_peer_streams() {
        use sculith_plugin_api::{PacketDispatch, PacketInfo};
        let (stop, receiver) = watch::channel(false);
        let (world, task) = create(receiver);
        let task = tokio::spawn(task);
        let mut identity = profile("observer");
        identity.edition = Edition::Java;
        let lease = world
            .reserve(identity, Permissions::default())
            .await
            .unwrap();
        let id = lease.player.id;
        let packet = PacketInfo {
            edition: sculith_plugin_api::Edition::Java,
            protocol: 777,
            phase: "play".into(),
            id: 22,
            name: "minecraft:custom_payload".into(),
            dispatch: PacketDispatch::Unsupported,
            sequence: 1,
        };
        world.configure_observations(crate::observations::ALL);
        assert!(world.observe_packet(id, packet.clone()).await.is_err());
        world.activate(id).await.unwrap();
        let mut game = world.subscribe_game();
        let mut normal = world.subscribe();
        let mut peers = world.observe_players(id).await.unwrap();
        let revision = world.capture().await.unwrap().revision();
        let mut wrong_edition = packet.clone();
        wrong_edition.edition = sculith_plugin_api::Edition::Bedrock;
        assert!(world.observe_packet(id, wrong_edition).await.is_err());
        assert!(game.try_recv().is_err());
        world.observe_packet(id, packet.clone()).await.unwrap();
        assert!(
            matches!(game.recv().await.unwrap().as_ref(),WorldEvent::PacketReceived {player_id,packet} if *player_id==id && packet.name=="minecraft:custom_payload")
        );
        assert!(normal.try_recv().is_err());
        assert!(peers.updates.try_recv().is_err());
        world.select_slot(id, 1).await.unwrap();
        assert!(
            matches!(game.recv().await.unwrap().as_ref(),WorldEvent::Equipped(p) if p.selected_slot==1)
        );
        world.send_message(id, "private".into()).await.unwrap();
        assert!(game.try_recv().is_err());
        assert_eq!(world.capture().await.unwrap().revision(), revision);
        world.configure_observations(0);
        world.observe_packet(id, packet.clone()).await.unwrap();
        world.select_slot(id, 2).await.unwrap();
        assert!(game.try_recv().is_err());
        world.configure_observations(crate::observations::PACKETS);
        drop(lease);
        assert!(world.observe_packet(id, packet).await.is_err());
        stop.send(true).unwrap();
        task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn held_slot_changes_publish_only_committed_public_equipment() {
        let (stop, receiver) = watch::channel(false);
        let (world, task) = create(receiver);
        let task = tokio::spawn(task);
        let viewer = world
            .reserve(profile("viewer"), Permissions::default())
            .await
            .unwrap();
        let peer = world
            .reserve(profile("peer"), Permissions::default())
            .await
            .unwrap();
        world.activate(viewer.player.id).await.unwrap();
        assert!(world.select_slot(peer.player.id, 1).await.is_err());
        world.activate(peer.player.id).await.unwrap();
        let mut observation = world.observe_players(viewer.player.id).await.unwrap();
        let mut normal = world.subscribe();
        let mut game = world.subscribe_game();
        let revision = world.capture().await.unwrap().revision();
        let selected = world.select_slot(peer.player.id, 1).await.unwrap();
        assert_eq!(selected.hotbar[1], Some(BlockKind::Dirt));
        assert!(
            matches!(observation.updates.recv().await.unwrap().as_ref(),WorldEvent::Equipped(p) if p.id==peer.player.id && p.selected_slot==1 && p.hotbar[1]==Some(BlockKind::Dirt))
        );
        world.select_slot(peer.player.id, 1).await.unwrap();
        assert!(world.select_slot(peer.player.id, 9).await.is_err());
        assert!(observation.updates.try_recv().is_err());
        assert!(normal.try_recv().is_err());
        assert!(game.try_recv().is_err());
        assert_eq!(world.capture().await.unwrap().revision(), revision);
        let fresh = world.observe_players(viewer.player.id).await.unwrap();
        assert_eq!(fresh.players[0].selected_slot, 1);
        stop.send(true).unwrap();
        task.await.unwrap().unwrap();
    }

    pub fn profile(name: &str) -> Profile {
        Profile {
            subject: format!("test:{name}"),
            name: name.into(),
            uuid: uuid::Uuid::new_v4(),
            edition: Edition::Bedrock,
        }
    }

    #[tokio::test]
    async fn peer_observation_has_an_atomic_initial_membership_boundary_and_no_pending_players() {
        let (stop, receiver) = watch::channel(false);
        let (world, task) = create(receiver);
        let task = tokio::spawn(task);
        let viewer = world
            .reserve(profile("viewer"), Permissions::default())
            .await
            .unwrap();
        assert!(world.observe_players(viewer.player.id).await.is_err());
        world.activate(viewer.player.id).await.unwrap();
        let peer = world
            .reserve(profile("peer"), Permissions::default())
            .await
            .unwrap();
        let mut first = world.observe_players(viewer.player.id).await.unwrap();
        assert!(first.players.is_empty());
        world.activate(peer.player.id).await.unwrap();
        assert!(
            matches!(first.updates.recv().await.unwrap().as_ref(),WorldEvent::Joined(p) if p.id==peer.player.id)
        );
        let mut second = world.observe_players(viewer.player.id).await.unwrap();
        assert_eq!(second.players.len(), 1);
        assert_eq!(second.players[0].id, peer.player.id);
        assert!(matches!(
            second.updates.try_recv(),
            Err(broadcast::error::TryRecvError::Empty)
        ));
        world
            .send_message(viewer.player.id, "private".into())
            .await
            .unwrap();
        assert!(matches!(
            second.updates.try_recv(),
            Err(broadcast::error::TryRecvError::Empty)
        ));
        let id = peer.player.id;
        drop(peer);
        // The next mailbox command first retires dead leases and then snapshots.
        let mut third = world.observe_players(viewer.player.id).await.unwrap();
        assert!(third.players.is_empty());
        assert!(
            matches!(second.updates.recv().await.unwrap().as_ref(),WorldEvent::Left {id:current} if *current==id)
        );
        assert!(matches!(
            third.updates.try_recv(),
            Err(broadcast::error::TryRecvError::Empty)
        ));
        let id = viewer.player.id;
        drop(viewer);
        assert!(world.observe_players(id).await.is_err());
        stop.send(true).unwrap();
        task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn model_effects_are_private_presentation_and_hide_is_idempotent() {
        let (stop, receiver) = watch::channel(false);
        let (world, task) = with_region_and_content(
            Region::flat_origin(),
            20,
            receiver,
            Default::default(),
            ["sculith:demo_statue".into()].into(),
        )
        .unwrap();
        let task = tokio::spawn(task);
        let lease = world
            .reserve(profile("model"), Permissions { can_build: true })
            .await
            .unwrap();
        let id = lease.player.id;
        world.activate(id).await.unwrap();
        let mut events = world.subscribe();
        let mut hooks = world.subscribe_game();
        world
            .show_model(id, "sculith:demo_statue".into())
            .await
            .unwrap();
        let event = events.recv().await.unwrap();
        let WorldEvent::ModelShown(view) = event.as_ref() else {
            panic!("missing model")
        };
        assert_eq!(view.player_id, id);
        let entity_id = view.entity_id;
        world.hide_model(id).await.unwrap();
        assert!(
            matches!(events.recv().await.unwrap().as_ref(),WorldEvent::ModelHidden {player_id,entity_id:current} if *player_id==id && *current==entity_id)
        );
        world.hide_model(id).await.unwrap();
        assert!(matches!(
            events.try_recv(),
            Err(broadcast::error::TryRecvError::Empty)
        ));
        assert!(matches!(
            hooks.try_recv(),
            Err(broadcast::error::TryRecvError::Empty)
        ));
        assert_eq!(world.capture().await.unwrap().revision(), 0);
        drop(lease);
        assert!(world.hide_model(id).await.is_err());
        stop.send(true).unwrap();
        task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn private_sound_requires_live_initialized_registered_player_and_never_changes_world_or_game_hooks()
     {
        let (stop, receiver) = watch::channel(false);
        let (world, task) = with_region_and_sounds(
            Region::flat_origin(),
            20,
            receiver,
            ["sculith:demo_tone".into()].into(),
        )
        .unwrap();
        let task = tokio::spawn(task);
        let lease = world
            .reserve(profile("sound"), Permissions { can_build: true })
            .await
            .unwrap();
        let id = lease.player.id;
        assert!(
            world
                .play_sound(id, "sculith:demo_tone".into(), 0.7, 1.0)
                .await
                .is_err()
        );
        world.activate(id).await.unwrap();
        let mut events = world.subscribe();
        let mut hooks = world.subscribe_game();
        let revision = world.capture().await.unwrap().revision();
        assert!(
            world
                .play_sound(id, "sculith:missing".into(), 0.7, 1.0)
                .await
                .is_err()
        );
        assert!(
            world
                .play_sound(id, "sculith:demo_tone".into(), f32::NAN, 1.0)
                .await
                .is_err()
        );
        for _ in 0..4 {
            world
                .play_sound(id, "sculith:demo_tone".into(), 0.7, 1.0)
                .await
                .unwrap();
            let event = events.recv().await.unwrap();
            let WorldEvent::Sound(effect) = event.as_ref() else {
                panic!("wrong effect")
            };
            assert_eq!(effect.player_id, id);
            assert_eq!(effect.position.x, lease.player.feet.x);
            assert_eq!(effect.position.y, lease.player.feet.y + 1.62);
        }
        assert!(
            world
                .play_sound(id, "sculith:demo_tone".into(), 0.7, 1.0)
                .await
                .is_err()
        );
        assert!(matches!(
            hooks.try_recv(),
            Err(broadcast::error::TryRecvError::Empty)
        ));
        assert_eq!(world.capture().await.unwrap().revision(), revision);
        drop(lease);
        assert!(
            world
                .play_sound(id, "sculith:demo_tone".into(), 0.7, 1.0)
                .await
                .is_err()
        );
        stop.send(true).unwrap();
        task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn an_intent_queued_before_lease_drop_cannot_commit_after_disconnect() {
        let (stop, receiver) = watch::channel(false);
        let (world, task) = create(receiver);
        let task = tokio::spawn(task);
        let lease = world
            .reserve(profile("closed"), Permissions { can_build: true })
            .await
            .unwrap();
        let id = lease.player.id;
        world.activate(id).await.unwrap();
        let (reply, response) = oneshot::channel();
        world
            .commands
            .try_send(Command::Break {
                id,
                position: BlockPos { x: 1, y: 3, z: 1 },
                reply,
            })
            .unwrap();
        // No yield between enqueue and drop: the owner must see the dead lease
        // before handling this already queued request, not just before recv().
        drop(lease);
        assert!(matches!(
            response.await.unwrap(),
            BlockOutcome::Rejected {
                reason: crate::game::RejectReason::UnknownPlayer,
                ..
            }
        ));
        assert_eq!(world.capture().await.unwrap().revision(), 0);
        stop.send_replace(true);
        task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn committed_runtime_state_survives_snapshot_reload() {
        let (stop, receiver) = watch::channel(false);
        let (world, task) = create(receiver);
        let task = tokio::spawn(task);
        let lease = world
            .reserve(profile("saved"), Permissions { can_build: true })
            .await
            .unwrap();
        world.activate(lease.player.id).await.unwrap();
        let position = BlockPos { x: 1, y: 3, z: 1 };
        assert!(matches!(
            world.break_block(lease.player.id, position).await.unwrap(),
            BlockOutcome::Committed { .. }
        ));
        let capture = world.capture().await.unwrap();
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("region.sculith");
        sculith_world::storage::save_snapshot(&path, &capture).unwrap();
        let region = sculith_world::storage::load(&path).unwrap();
        assert_eq!(region.revision(), 1);
        assert_eq!(region.get_block(position).unwrap(), BlockKind::Air);
        drop(lease);
        stop.send_replace(true);
        task.await.unwrap().unwrap();
        let (stop, receiver) = watch::channel(false);
        let (restored, task) = with_region(region, 20, receiver).unwrap();
        let task = tokio::spawn(task);
        assert_eq!(
            restored
                .snapshot(ChunkPos { x: 0, z: 0 })
                .await
                .unwrap()
                .get(1, 3, 1)
                .unwrap(),
            BlockKind::Air
        );
        stop.send_replace(true);
        task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn private_messages_require_a_live_initialized_lease_and_never_reenter_game_hooks() {
        let (shutdown, stop) = watch::channel(false);
        let (world, task) = create(stop);
        let task = tokio::spawn(task);
        let mut hooks = world.subscribe_game();
        let lease = world
            .reserve(profile("recipient"), Permissions { can_build: true })
            .await
            .unwrap();
        let id = lease.player.id;
        assert!(world.send_message(id, "pending".into()).await.is_err());
        world.activate(id).await.unwrap();
        assert!(
            matches!(hooks.recv().await.unwrap().as_ref(), WorldEvent::Joined(p) if p.id == id)
        );
        let mut view = world.subscribe();
        assert!(world.send_message(id, "bad\ntext".into()).await.is_err());
        world
            .send_message(id, "환영합니다 👋".into())
            .await
            .unwrap();
        assert!(
            matches!(view.recv().await.unwrap().as_ref(), WorldEvent::Message { player_id, message } if *player_id == id && message == "환영합니다 👋")
        );
        assert!(matches!(
            hooks.try_recv(),
            Err(broadcast::error::TryRecvError::Empty)
        ));
        assert_eq!(world.capture().await.unwrap().revision(), 0);
        drop(lease);
        assert!(world.send_message(id, "closed".into()).await.is_err());
        assert!(
            matches!(hooks.recv().await.unwrap().as_ref(), WorldEvent::Left { id: left } if *left == id)
        );
        shutdown.send_replace(true);
        task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn form_results_are_private_semantic_events_and_dead_leases_cannot_reply() {
        let (shutdown, stop) = watch::channel(false);
        let (world, task) = create(stop);
        let task = tokio::spawn(task);
        let lease = world
            .reserve(profile("forms"), Permissions::default())
            .await
            .unwrap();
        let id = lease.player.id;
        let form = sculith_core::SimpleForm {
            title: "Menu".into(),
            body: "Choose".into(),
            buttons: vec!["A".into()],
        };
        assert!(
            world
                .show_form(id, "owner".into(), 7, form.clone())
                .await
                .is_err()
        );
        world.activate(id).await.unwrap();
        let mut view = world.subscribe();
        let mut hooks = world.subscribe_game();
        world
            .show_form(id, "owner".into(), 7, form.clone())
            .await
            .unwrap();
        let event = view.recv().await.unwrap();
        let WorldEvent::FormRequested { form_id, .. } = event.as_ref() else {
            panic!("expected private form presentation")
        };
        assert!(matches!(
            hooks.try_recv(),
            Err(broadcast::error::TryRecvError::Empty)
        ));
        world.submit_form(id, *form_id, Some(0)).await.unwrap();
        assert!(
            matches!(hooks.recv().await.unwrap().as_ref(),WorldEvent::FormSubmitted(reply) if reply.plugin=="owner" && reply.request_id==7 && reply.selection==Some(0))
        );
        assert!(matches!(
            view.try_recv(),
            Err(broadcast::error::TryRecvError::Empty)
        ));
        assert!(world.submit_form(id, *form_id, Some(0)).await.is_err());
        drop(lease);
        assert!(world.show_form(id, "owner".into(), 8, form).await.is_err());
        assert!(world.submit_form(id, *form_id, None).await.is_err());
        assert_eq!(world.capture().await.unwrap().revision(), 0);
        shutdown.send_replace(true);
        task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn committed_changes_are_shared_and_a_dropped_lease_cleans_up() {
        let (shutdown, stop) = watch::channel(false);
        let (world, task) = create(stop);
        let task = tokio::spawn(task);
        let player = world
            .reserve(profile("builder"), Permissions { can_build: true })
            .await
            .unwrap();
        world.activate(player.player.id).await.unwrap();
        let mut first = world.subscribe();
        let mut second = world.subscribe();
        let old = world.snapshot(ChunkPos { x: 0, z: 0 }).await.unwrap();
        let position = BlockPos { x: 1, y: 3, z: 1 };
        assert!(matches!(
            world.break_block(player.player.id, position).await.unwrap(),
            BlockOutcome::Committed { .. }
        ));
        let a = first.recv().await.unwrap();
        let b = second.recv().await.unwrap();
        assert!(Arc::ptr_eq(&a, &b));
        assert_eq!(old.get(1, 3, 1).unwrap(), BlockKind::Grass);
        assert_eq!(
            world
                .snapshot(ChunkPos { x: 0, z: 0 })
                .await
                .unwrap()
                .get(1, 3, 1)
                .unwrap(),
            BlockKind::Air
        );
        drop(player);
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while world.online_players() != 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        shutdown.send_replace(true);
        task.await.unwrap().unwrap();
    }
}
