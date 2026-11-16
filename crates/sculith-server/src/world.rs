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
        atomic::{AtomicBool, AtomicU32, Ordering},
    },
    time::Instant,
};
use tokio::sync::{broadcast, mpsc, oneshot, watch};

#[derive(Debug, Clone)]
pub enum WorldEvent {
    Joined(PlayerSnapshot),
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
    id: u64,
) {
    leases.remove(&id);
    if let Some(player) = state.retire(id)
        && player.initialized
    {
        publish_game(WorldEvent::Left { id }, events, game_events);
    }
}

pub fn with_region(
    region: Region,
    max_players: u32,
    mut shutdown: watch::Receiver<bool>,
) -> Result<(WorldHandle, impl Future<Output = Result<()>> + Send)> {
    let mut state = GameState::new(region, max_players)?;
    let (commands, mut requests) = mpsc::channel(32);
    let weak_commands = commands.downgrade();
    let (events, _) = broadcast::channel(128);
    let (game_events, _) = broadcast::channel(128);
    let online = Arc::new(AtomicU32::new(0));
    let handle = WorldHandle {
        commands,
        online: online.clone(),
        events: events.clone(),
        game_events: game_events.clone(),
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
                retire(&mut state, &mut leases, &events, &game_events, id);
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
                    for id in dead { retire(&mut state, &mut leases, &events, &game_events, id); }
                    match command {
                        Some(Command::Snapshot { position, reply }) => { let _ = reply.send(state.snapshot(position).map_err(Into::into)); }
                        Some(Command::Capture { reply }) => { let _ = reply.send(state.capture()); }
                        Some(Command::Reserve { profile, permissions, reply }) => {
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
                            if !was_ready && let Ok(player) = &result { publish_game(WorldEvent::Joined(player.clone()), &events, &game_events); }
                            online.store(state.online_count(), Ordering::Release);
                            let _ = reply.send(result);
                        }
                        Some(Command::Retire { id }) => { retire(&mut state, &mut leases, &events, &game_events, id); }
                        Some(Command::Move { id, tick, position, rotation, reply }) => {
                            let outcome = state.move_player(id, tick, position, rotation, Instant::now());
                            if let MoveOutcome::Accepted { player, tick, changed: true } = &outcome {
                                let _ = events.send(Arc::new(WorldEvent::Moved { player: player.clone(), tick: *tick }));
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
                        Some(Command::Select { id, slot, reply }) => { let _ = reply.send(state.select_slot(id, slot)); }
                        Some(Command::SendMessage { id, message, reply }) => {
                            let result = sculith_core::validate_system_message(&message).map_err(anyhow::Error::msg)
                                .and_then(|()| state.permit_message(id, Instant::now()));
                            if result.is_ok() { let _ = events.send(Arc::new(WorldEvent::Message { player_id: id, message })); }
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

    pub fn profile(name: &str) -> Profile {
        Profile {
            subject: format!("test:{name}"),
            name: name.into(),
            uuid: uuid::Uuid::new_v4(),
            edition: Edition::Bedrock,
        }
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
