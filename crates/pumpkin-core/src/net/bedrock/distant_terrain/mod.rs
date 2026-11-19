//! Packet-only distant terrain for Bedrock clients with the six terrain models.
mod region;
mod surface;
mod view;

use self::{
    region::{Atlas, Grid, Region},
    surface::{MATERIALS, Summary, Surface, summarize},
    view::{Tile, View},
};
use super::BedrockClient;
use crate::{
    entity::{EntityBase, player::Player},
    net::ClientPlatform,
    server::Server,
    world::chunker::get_view_distance,
};
use bytes::Bytes;
use pumpkin_protocol::{
    bedrock::client::{
        CAddActor, CMoveActorAbsolute, CRemoveActor, MetadataValue, SyncedActorDataList,
        entity_data_key,
    },
    codec::{var_long::VarLong, var_ulong::VarULong},
};
use pumpkin_util::math::{vector2::Vector2, vector3::Vector3};
use pumpkin_world::{
    chunk::{
        ChunkData,
        io::{converter::RegionFormat, file_manager::LevelFileIO},
    },
    level::Level,
};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    io,
    sync::{Arc, Weak, atomic::Ordering, mpsc},
    thread,
    time::{Duration, Instant},
};

pub struct DistantTerrain {
    stop: mpsc::Sender<()>,
    thread: thread::JoinHandle<()>,
}

impl DistantTerrain {
    pub fn start(server: &Arc<Server>) -> io::Result<Option<Self>> {
        let config = &server.advanced_config;
        let enabled = config.networking.bedrock.enabled
            && config
                .resource_pack
                .shared
                .as_ref()
                .is_some_and(|p| p.required)
            && server.shared_resources.as_ref().is_some_and(|pack| {
                MATERIALS.iter().all(|name| {
                    let id = format!("sculith:lod_{name}");
                    pack.model(&id).is_some_and(|entry| entry.bedrock == id)
                })
            });
        if !enabled {
            return Ok(None);
        }
        let server = Arc::downgrade(server);
        let (stop, receiver) = mpsc::channel();
        let thread = thread::Builder::new()
            .name("Sculith distant terrain".into())
            .spawn(move || {
                let mut terrain = HashMap::new();
                let mut shown = HashMap::new();
                while receiver.recv_timeout(Duration::from_millis(200))
                    == Err(mpsc::RecvTimeoutError::Timeout)
                {
                    let Some(server) = server.upgrade() else {
                        break;
                    };
                    update(&server, &mut terrain, &mut shown);
                }
            })?;
        tracing::info!("Enabled packet-only Bedrock distant terrain");
        Ok(Some(Self { stop, thread }))
    }

    pub fn stop(self) {
        let _ = self.stop.send(());
        if self.thread.join().is_err() {
            tracing::error!("Distant terrain worker exited unexpectedly");
        }
    }
}

#[derive(Default)]
struct Survey {
    regions: BTreeMap<Grid, Region>,
    live: HashMap<Grid, (Weak<ChunkData>, u64, Summary)>,
    check_files: Option<Instant>,
    scan_cursor: usize,
}

impl Survey {
    fn collect(&mut self, level: &Level, views: &HashSet<View>, generate: bool) -> Atlas {
        let mut wanted = HashMap::<Grid, i32>::new();
        for p in views {
            for (coord, d) in p.regions() {
                wanted
                    .entry(coord)
                    .and_modify(|old| *old = (*old).min(d))
                    .or_insert(d);
            }
        }
        let mut wanted: Vec<_> = wanted.into_iter().collect();
        wanted.sort_unstable_by_key(|&(coord, d)| (d, coord));
        wanted.truncate(32);
        let keep: HashSet<_> = wanted.iter().map(|&(c, _)| c).collect();
        self.regions.retain(|k, _| keep.contains(k));
        self.live
            .retain(|&(x, z), _| keep.contains(&(x.div_euclid(32), z.div_euclid(32))));
        if generate {
            self.read_saved(level, &wanted);
            self.sample_loaded(level, &keep, views);
        }
        let mut rows = Atlas::new();
        for r in self.regions.values() {
            r.add_rows(&mut rows);
        }
        self.live.retain(|coord, (owner, _, _)| {
            owner.strong_count() > 0
                || self
                    .regions
                    .get(&(coord.0.div_euclid(32), coord.1.div_euclid(32)))
                    .is_some_and(Region::pending)
        });
        rows.extend(self.live.iter().map(|(&p, (_, _, s))| (p, *s)));
        rows
    }

    fn read_saved(&mut self, level: &Level, wanted: &[(Grid, i32)]) {
        let kind = match level.chunk_saver.as_ref() {
            LevelFileIO::Anvil(_) => RegionFormat::Anvil,
            LevelFileIO::Linear(_) => RegionFormat::Linear,
            LevelFileIO::Pump(_) => RegionFormat::Pump,
        };
        let check_files = self
            .check_files
            .is_none_or(|t| t.elapsed() >= Duration::from_secs(2));
        for &(k, _) in wanted {
            match self.regions.entry(k) {
                std::collections::btree_map::Entry::Vacant(slot) => slot
                    .insert(Region::new(&level.level_folder, level.seed.0, kind, k))
                    .refresh(),
                std::collections::btree_map::Entry::Occupied(mut slot) if check_files => {
                    slot.get_mut().refresh();
                }
                std::collections::btree_map::Entry::Occupied(_) => {}
            }
        }
        if check_files {
            self.check_files = Some(Instant::now());
        }
        let mut pending: Vec<_> = self.regions.values_mut().filter(|r| r.pending()).collect();
        let start = Instant::now();
        for _ in 0..12 {
            if pending.is_empty() || start.elapsed() >= Duration::from_millis(8) {
                break;
            }
            let index = self.scan_cursor % pending.len();
            self.scan_cursor = self.scan_cursor.wrapping_add(1);
            pending[index].step();
            if !pending[index].pending() {
                pending.swap_remove(index);
            }
        }
    }

    fn sample_loaded(&mut self, level: &Level, keep: &HashSet<Grid>, views: &HashSet<View>) {
        // Only sample chunks the game already loaded. Never acquire a ticket for LOD.
        let mut dirty = Vec::new();
        for entry in level.loaded_chunks.iter() {
            let (x, z) = (entry.key().x, entry.key().y);
            if !keep.contains(&(x.div_euclid(32), z.div_euclid(32))) {
                continue;
            }
            let chunk = entry.value();
            let rev = chunk.section.terrain_revision().0;
            let old = self.live.get(&(x, z));
            if old.is_none_or(|(c, r, _)| *r != rev || c.as_ptr() != Arc::as_ptr(chunk)) {
                let distance = views
                    .iter()
                    .map(|v| (x - v.center.0).abs().max((z - v.center.1).abs()))
                    .min()
                    .unwrap_or_default();
                dirty.push((old.is_none(), distance, (x, z)));
            }
        }
        if dirty.len() > 12 {
            dirty.select_nth_unstable(12);
            dirty.truncate(12);
        }
        dirty.sort_unstable();
        let start = Instant::now();
        for (_, _, coord) in dirty {
            if start.elapsed() >= Duration::from_millis(6) {
                break;
            }
            let Some(chunk) = level
                .loaded_chunks
                .get(&Vector2::new(coord.0, coord.1))
                .map(|c| c.clone())
            else {
                continue;
            };
            let rev = chunk.section.terrain_revision().0;
            let summary = summarize(&chunk);
            if chunk.section.terrain_revision().0 == rev {
                self.live
                    .insert(coord, (Arc::downgrade(&chunk), rev, summary));
            }
        }
    }
}

#[derive(Clone, Copy, Hash, PartialEq, Eq)]
enum Action {
    Add(Surface),
    Move(i32),
    Remove,
}

fn packet(client: &BedrockClient, tile: Tile, action: Action) -> io::Result<Bytes> {
    let id = tile.actor();
    let pos = |y: i32| {
        Vector3::new(
            (tile.x + i32::from(tile.width) / 2) as f32,
            y as f32,
            (tile.z + i32::from(tile.width) / 2) as f32,
        )
    };
    match action {
        Action::Remove => client.serialize_packet(&CRemoveActor::new(VarLong(id as i64))),
        Action::Move(y) => client.serialize_packet(&CMoveActorAbsolute {
            actor_runtime_id: VarULong(id),
            header: CMoveActorAbsolute::FLAG_TELEPORT,
            position: pos(y),
            rotation_x: 0,
            rotation_y: 0,
            rotation_y_head: 0,
        }),
        Action::Add(s) => {
            let mut data = SyncedActorDataList::new();
            data.0.insert(
                entity_data_key::SCALE,
                MetadataValue::Float(f32::from(tile.width)),
            );
            data.0
                .insert(entity_data_key::WIDTH, MetadataValue::Float(0.0));
            data.0
                .insert(entity_data_key::HEIGHT, MetadataValue::Float(0.0));
            client.serialize_packet(&CAddActor {
                target_actor_id: VarLong(id as i64),
                target_runtime_id: VarULong(id),
                actor_type: format!("sculith:lod_{}", MATERIALS[usize::from(s.material)]),
                position: pos(s.y),
                velocity: Vector3::default(),
                rotation: Vector2::default(),
                y_body_rotation: 0.0,
                y_head_rotation: 0.0,
                attributes_list: Vec::new(),
                actor_data: data,
                synced_properties: pumpkin_protocol::bedrock::client::PropertySyncData::default(),
                actor_links: Vec::new(),
                passenger_of_block: None,
            })
        }
    }
}

type Tiles = BTreeMap<Tile, Surface>;
#[derive(Default)]
struct Presented {
    client: Weak<BedrockClient>,
    epoch: u32,
    tiles: Tiles,
}

impl Presented {
    fn sync(
        &mut self,
        player: &Player,
        client: &BedrockClient,
        epoch: u32,
        wanted: &Tiles,
        frames: &mut HashMap<(Tile, Action), Bytes>,
    ) {
        let mut actions = Vec::new();
        for (&k, &old) in &self.tiles {
            if self.epoch != epoch || wanted.get(&k).is_none_or(|s| s.material != old.material) {
                actions.push((k, Action::Remove));
            }
        }
        for (&k, &s) in wanted {
            match self
                .tiles
                .get(&k)
                .filter(|old| self.epoch == epoch && old.material == s.material)
            {
                Some(old) if old.y == s.y => {}
                Some(_) => actions.push((k, Action::Move(s.y))),
                None => actions.push((k, Action::Add(s))),
            }
        }
        if actions.is_empty() {
            self.epoch = epoch;
            return;
        }
        let mut translated = Vec::with_capacity(actions.len());
        for (k, a) in actions {
            let buf = match frames.entry((k, a)) {
                std::collections::hash_map::Entry::Occupied(slot) => slot.into_mut(),
                std::collections::hash_map::Entry::Vacant(slot) => match packet(client, k, a) {
                    Ok(b) => slot.insert(b),
                    Err(error) => {
                        tracing::debug!(%error,"Could not encode distant terrain");
                        return;
                    }
                },
            };
            translated.push(client.translate_outgoing(buf.clone(), true));
        }
        let _guard = player
            .chunk_sender
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if player.chunk_send_epoch.load(Ordering::Relaxed) != epoch {
            return;
        }
        for (packet, extras) in translated {
            let ok = match packet {
                Some(p) => client.queue_translated_packet(p, extras),
                None => client.queue_extra_packets(extras),
            };
            if !ok {
                return;
            }
        }
        self.epoch = epoch;
        self.tiles.clone_from(wanted);
    }
}

fn update(
    server: &Server,
    terrain: &mut HashMap<std::path::PathBuf, Survey>,
    shown: &mut HashMap<u64, Presented>,
) {
    let mut active = HashSet::new();
    let mut frames = HashMap::new();
    let ticks = server.tick_count.load(Ordering::Relaxed);
    let last_tick = ticks.checked_sub(1).map_or(0, |n| {
        server.get_tick_times_nanos_copy()[n.rem_euclid(100) as usize]
    });
    let generate = last_tick < 50_000_000 && server.get_mspt() < 45.0;
    for world in server.worlds.load().iter() {
        let mut clients = Vec::new();
        let mut views = HashSet::new();
        let players = world.players.load();
        for player in players.iter() {
            let ClientPlatform::Bedrock(client) = player.client.as_ref() else {
                continue;
            };
            if client.is_closed()
                || !player.bedrock_spawned.load(Ordering::Acquire)
                || !client.is_current_protocol()
            {
                continue;
            }
            let epoch = player.chunk_send_epoch.load(Ordering::Relaxed);
            if !Arc::ptr_eq(world, &player.world()) {
                continue;
            }
            let p = player.get_entity().pos.load();
            let memory = client.client_data.load();
            let budget = match memory.as_ref().as_ref().map(|d| d.memory_tier) {
                Some(0 | 1) => 12,
                Some(2) => 20,
                _ => 32,
            };
            let view = View {
                center: (
                    (p.x.floor() as i32).div_euclid(16),
                    (p.z.floor() as i32).div_euclid(16),
                ),
                radius: get_view_distance(player),
                budget,
            };
            views.insert(view);
            clients.push((player, client, epoch, view));
        }
        if clients.is_empty() {
            continue;
        }
        let path = &world.level.level_folder.dim_folder;
        active.insert(path.clone());
        let rows = terrain
            .entry(path.clone())
            .or_default()
            .collect(&world.level, &views, generate);
        let selected: HashMap<_, _> = views.into_iter().map(|v| (v, v.select(&rows))).collect();
        for (player, client, epoch, v) in clients {
            shown
                .entry(client.id)
                .or_insert_with(|| Presented {
                    client: Arc::downgrade(client),
                    ..Presented::default()
                })
                .sync(player, client, epoch, &selected[&v], &mut frames);
        }
    }
    terrain.retain(|w, _| active.contains(w));
    shown.retain(|_, state| state.client.upgrade().is_some_and(|c| !c.is_closed()));
}
