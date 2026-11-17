//! Protocol-777 sessions through the shared owner; offline mode is local only.
use super::{
    chunk,
    codec::{self, Fields, Frames},
    registry, write_string, write_varint,
};
use crate::{
    game::{BlockOutcome, Edition, MoveOutcome, Permissions, PlayerSnapshot, Position, Profile},
    world::{PlayerLease, WorldEvent, WorldHandle},
};
use anyhow::{Context, Result, bail, ensure};
use md5::{Digest, Md5};
use sculith_game::inventory::Click;
use sculith_runtime::peers::{Change as PeerChange, Peers};
use sculith_world::{BlockKind, BlockPos, ChunkPos};
use std::{
    collections::BTreeMap,
    time::{Duration, Instant},
};
use tokio::{net::TcpStream, sync::broadcast};
use uuid::Uuid;

type Connection<'a> = super::transport::Stream<&'a mut TcpStream>;

pub struct Session {
    _lease: PlayerLease,
    player: PlayerSnapshot,
    world: WorldHandle,
    events: broadcast::Receiver<std::sync::Arc<WorldEvent>>,
    peers: Option<Peers>,
    peer_updates: Option<broadcast::Receiver<std::sync::Arc<WorldEvent>>>,
    interest: BTreeMap<ChunkPos, u64>,
    center: ChunkPos,
    tick: u64,
    packet_sequence: u64,
    inventory_state: u16,
    inventory_sent: [Option<sculith_game::inventory::ItemStack>; 36],
    teleport_id: i32,
    pending_teleport: Option<(i32, Instant)>,
    keepalive: Option<(i64, Instant)>,
    keepalive_id: i64,
    last_sequence: Option<i32>,
    rate: Rate,
}

struct Rate {
    tokens: f64,
    bytes: f64,
    updated: Instant,
}
impl Rate {
    fn charge(&mut self, bytes: usize) -> Result<()> {
        let now = Instant::now();
        let elapsed = now.saturating_duration_since(self.updated).as_secs_f64();
        self.tokens = (self.tokens + elapsed * 200.0).min(256.0);
        self.bytes = (self.bytes + elapsed * 131072.0).min(262144.0);
        self.updated = now;
        ensure!(
            self.tokens >= 1.0 && self.bytes >= bytes as f64,
            "Java input rate exceeded"
        );
        self.tokens -= 1.0;
        self.bytes -= bytes as f64;
        Ok(())
    }
}

pub fn offline_uuid(name: &str) -> Uuid {
    // Conventional Java offline identifier; this is never account verification.
    let mut bytes: [u8; 16] = Md5::digest(format!("OfflinePlayer:{name}")).into();
    bytes[6] = (bytes[6] & 15) | 0x30;
    bytes[8] = (bytes[8] & 63) | 0x80;
    Uuid::from_bytes(bytes)
}

pub async fn start(
    stream: &mut Connection<'_>,
    frames: &mut Frames,
    world: WorldHandle,
    max_players: u32,
    phase: &mut super::Phase,
    content: Option<&crate::content::Content>,
    authentication: Option<&super::Authentication>,
) -> Result<Session> {
    let packet = frames.next(stream).await?;
    let mut fields = Fields(&packet);
    ensure!(fields.int()? == 0, "expected Java Login Start");
    let name = fields.string(16)?.to_owned();
    ensure!(
        !name.is_empty() && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_'),
        "invalid Java profile name"
    );
    let claimed_uuid = Uuid::from_bytes(fields.take(16)?.try_into()?);
    fields.end()?;
    let (name, uuid, subject) = if let Some(authentication) = authentication {
        let profile = authentication
            .verify(stream, frames, &name, claimed_uuid)
            .await?;
        let subject = format!("java:{}", profile.uuid);
        (profile.name, profile.uuid, subject)
    } else {
        let subject = format!("offline-java:{}", name.to_ascii_lowercase());
        let uuid = offline_uuid(&name);
        (name, uuid, subject)
    };
    let mut login = codec::packet(2);
    login.extend_from_slice(uuid.as_bytes());
    write_string(&name, &mut login);
    write_varint(0, &mut login); // Signed profile properties absent in offline mode.
    login.extend_from_slice(Uuid::new_v4().as_bytes()); // Protocol 777 session ID.
    codec::send(stream, &login).await?;
    let ack = frames.next(stream).await?;
    ensure!(ack == [3], "expected Java Login Acknowledged");
    *phase = super::Phase::Configuration;
    frames.set_phase(sculith_core::connections::PacketPhase::Configuration);
    configure(stream, frames, content).await?;
    *phase = super::Phase::Play;
    frames.set_phase(sculith_core::connections::PacketPhase::Spawn);
    let events = world.subscribe_session();
    let lease = world
        .reserve(
            Profile {
                subject,
                name,
                uuid,
                edition: Edition::Java,
            },
            Permissions { can_build: true },
        )
        .await?;
    let player = lease.player.clone();
    let mut state = Session {
        _lease: lease,
        player,
        world,
        events,
        peers: None,
        peer_updates: None,
        interest: BTreeMap::new(),
        center: ChunkPos { x: 0, z: 0 },
        tick: 0,
        packet_sequence: 0,
        inventory_state: 0,
        inventory_sent: [None; 36],
        teleport_id: 0,
        pending_teleport: None,
        keepalive: None,
        keepalive_id: 0,
        last_sequence: None,
        rate: Rate {
            tokens: 256.0,
            bytes: 262144.0,
            updated: Instant::now(),
        },
    };
    state.spawn(stream, max_players).await?;
    let mut teleported = false;
    let mut loaded = false;
    for _ in 0..4096 {
        let packet = frames.next(stream).await?;
        state.rate.charge(packet.len())?;
        let mut fields = Fields(&packet);
        let id = fields.int()?;
        match id {
            0 => {
                state.confirm_teleport(fields)?;
                teleported = true;
            }
            0x2c => {
                fields.end()?;
                loaded = true;
            }
            0x0e => {
                client_information(fields)?;
            }
            0x0b => {
                let rate = fields.float()?;
                ensure!(rate.is_finite() && rate > 0.0, "invalid chunk rate");
                fields.end()?;
            }
            0x0d | 0x1e..=0x21 | 0x28 | 0x36 => {} // No game intent before both ready acknowledgements.
            _ => {
                ensure!((0..=0x44).contains(&id), "unknown Java startup packet");
            }
        }
        if teleported && loaded {
            state.player = state.world.activate(state.player.id).await?;
            frames.attach(state.player.id);
            frames.set_phase(sculith_core::connections::PacketPhase::Play);
            send_peer_changes(stream, vec![PeerChange::InfoAdd(state.player.clone())]).await?;
            let observation = state.world.observe_players(state.player.id).await?;
            let mut peers = Peers::new(state.player.id, observation.players)?;
            send_peer_changes(stream, peers.initial(&state.interest)).await?;
            state.peers = Some(peers);
            state.peer_updates = Some(observation.updates);
            tracing::info!(player=%state.player.profile.name, entity_id=state.player.id,
                "Java client acknowledged the shared flat-world spawn");
            return Ok(state);
        }
    }
    bail!("too many Java packets before initialization")
}

async fn configure(
    stream: &mut Connection<'_>,
    frames: &mut Frames,
    content: Option<&crate::content::Content>,
) -> Result<()> {
    let mut features = codec::packet(0x0d);
    codec::strings(&["minecraft:vanilla"], &mut features);
    codec::send(stream, &features).await?;
    let mut known = codec::packet(0x0f);
    write_varint(1, &mut known);
    for value in ["minecraft", "core", registry::VERSION] {
        write_string(value, &mut known);
    }
    codec::send(stream, &known).await?;
    let mut selected = false;
    for _ in 0..32 {
        let packet = frames.next(stream).await?;
        let mut fields = Fields(&packet);
        match fields.int()? {
            0 => client_information(fields)?,
            2 => {
                ensure!(fields.0.len() <= 4096, "Java brand too large");
            }
            7 => {
                ensure!(
                    fields.int()? == 1
                        && fields.string(64)? == "minecraft"
                        && fields.string(64)? == "core"
                        && fields.string(64)? == registry::VERSION,
                    "Java preview requires the matching vanilla core pack"
                );
                fields.end()?;
                selected = true;
                break;
            }
            _ => bail!("unexpected Java configuration packet"),
        }
    }
    ensure!(selected, "client did not select its vanilla core");
    for packet in &registry::get().packets {
        codec::send(stream, packet).await?;
    }
    codec::send(stream, &registry::get().tags).await?;
    if let Some(content) = content {
        let bundle = &content.bundle;
        let mut offer = codec::packet(9);
        offer.extend_from_slice(bundle.java_id.as_bytes());
        write_string(&content.java_url, &mut offer);
        write_string(&bundle.java.sha1, &mut offer);
        offer.extend_from_slice(&[1, 0]); // Required; no optional prompt component.
        codec::send(stream, &offer).await?;
        let mut accepted = false;
        let mut downloaded = false;
        let mut loaded = false;
        let mut keepalive = None;
        let mut keepalive_id = 0i64;
        let mut pulse = tokio::time::interval_at(
            tokio::time::Instant::now() + Duration::from_secs(10),
            Duration::from_secs(10),
        );
        pulse.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        for _ in 0..32 {
            let packet = loop {
                tokio::select! {
                    packet = frames.next(stream) => break packet?,
                    _ = pulse.tick() => {
                        ensure!(keepalive.is_none(), "Java configuration keepalive not acknowledged");
                        keepalive_id += 1;
                        let mut packet = codec::packet(4);
                        packet.extend_from_slice(&keepalive_id.to_be_bytes());
                        codec::send(stream, &packet).await?;
                        keepalive = Some(keepalive_id);
                    }
                }
            };
            let mut fields = Fields(&packet);
            match fields.int()? {
                0 => client_information(fields)?,
                2 => ensure!(fields.0.len() <= 4096, "Java custom data too large"),
                4 => {
                    let id = fields.long()?;
                    fields.end()?;
                    ensure!(
                        keepalive == Some(id),
                        "Java configuration keepalive mismatch"
                    );
                    keepalive = None;
                    if loaded {
                        break;
                    }
                }
                6 => {
                    ensure!(
                        fields.take(16)? == bundle.java_id.as_bytes(),
                        "Java pack acknowledgement ID mismatch"
                    );
                    let status = fields.int()?;
                    fields.end()?;
                    match status {
                        3 if !accepted => accepted = true,
                        4 if accepted && !downloaded => downloaded = true,
                        0 if accepted && !loaded => {
                            loaded = true;
                            if keepalive.is_none() {
                                break;
                            }
                        }
                        _ => bail!(
                            "required Java pack was declined, failed or acknowledged out of order (status {status})"
                        ),
                    }
                }
                _ => bail!("unexpected Java packet during resource pack transfer"),
            }
        }
        ensure!(loaded, "Java resource pack loading was not acknowledged");
        tracing::info!(pack_id=%bundle.java_id, "Java client loaded the shared resource pack");
    }
    codec::send(stream, &[3]).await?;
    for _ in 0..32 {
        let packet = frames.next(stream).await?;
        let mut fields = Fields(&packet);
        match fields.int()? {
            3 => {
                fields.end()?;
                return Ok(());
            }
            0 => client_information(fields)?,
            2 => ensure!(fields.0.len() <= 4096, "Java custom data too large"),
            _ => bail!("unexpected Java configuration acknowledgement"),
        }
    }
    bail!("Java configuration acknowledgement missing")
}

fn client_information(mut fields: Fields<'_>) -> Result<()> {
    fields.string(16)?; // Locale.
    let radius = fields.byte()?;
    ensure!((2..=32).contains(&radius), "invalid Java view distance");
    ensure!((0..=2).contains(&fields.int()?), "invalid chat visibility");
    fields.boolean()?;
    fields.byte()?; // Skin parts.
    ensure!((0..=1).contains(&fields.int()?), "invalid main hand");
    fields.boolean()?;
    fields.boolean()?;
    ensure!((0..=2).contains(&fields.int()?), "invalid particle status");
    fields.end()
}

impl Session {
    async fn spawn(&mut self, stream: &mut Connection<'_>, max_players: u32) -> Result<()> {
        ensure!(
            (1..=64).contains(&max_players),
            "invalid Java preview capacity"
        );
        let mut packet = codec::packet(0x32);
        let id = i32::try_from(self.player.id).context("Java entity ID exhausted")?;
        packet.extend_from_slice(&id.to_be_bytes());
        packet.push(0); // Hardcore.
        codec::strings(&["sculith:world"], &mut packet);
        for value in [max_players as i32, 2, 2] {
            write_varint(value, &mut packet);
        }
        packet.extend_from_slice(&[0, 1, 0]); // Debug, respawn screen, limited crafting.
        write_varint(registry::get().dimension, &mut packet);
        write_string("sculith:world", &mut packet);
        packet.extend_from_slice(&0i64.to_be_bytes());
        // Official 26.3 GameType is a VarInt and previous GameType is an
        // OPTIONAL_VAR_INT (zero = absent, otherwise id + 1), not legacy i8/-1.
        write_varint(1, &mut packet); // Creative.
        write_varint(0, &mut packet); // Previous game type absent.
        packet.extend_from_slice(&[0, 1, 0]); // Debug, flat, death position absent.
        write_varint(0, &mut packet);
        write_varint(63, &mut packet);
        packet.extend_from_slice(&[0, 0]); // Offline mode, unsigned chat.
        codec::send(stream, &packet).await?;
        codec::send(
            stream,
            &super::commands::tree(&self.world.command_specs().await?),
        )
        .await?;
        let mut abilities = codec::packet(0x41);
        abilities.push(15);
        abilities.extend_from_slice(&0.05f32.to_be_bytes());
        abilities.extend_from_slice(&0.1f32.to_be_bytes());
        codec::send(stream, &abilities).await?;
        let mut distance = codec::packet(0x61);
        write_varint(2, &mut distance);
        codec::send(stream, &distance).await?;
        let mut simulation = codec::packet(0x71);
        write_varint(2, &mut simulation);
        codec::send(stream, &simulation).await?;
        let mut spawn = codec::packet(0x63);
        write_string("sculith:world", &mut spawn);
        codec::position(BlockPos { x: 0, y: 4, z: 0 }, &mut spawn);
        spawn.extend_from_slice(&[0; 8]); // Yaw and pitch.
        codec::send(stream, &spawn).await?;
        let mut time = codec::packet(0x73);
        time.extend_from_slice(&0i64.to_be_bytes());
        write_varint(1, &mut time);
        write_varint(registry::get().clock, &mut time);
        write_varint(6000, &mut time); // Positive VarLong has identical representation.
        time.extend_from_slice(&[0; 8]); // Partial tick and frozen rate.
        codec::send(stream, &time).await?;
        self.inventory(stream).await?;
        let mut event = codec::packet(0x27);
        event.push(13);
        event.extend_from_slice(&0f32.to_be_bytes());
        codec::send(stream, &event).await?;
        self.refresh(stream, true).await?;
        self.teleport(stream).await?;
        Ok(())
    }

    async fn inventory(&mut self, stream: &mut Connection<'_>) -> Result<()> {
        self.inventory_state = (self.inventory_state + 1) & 0x7fff;
        for slot in 0..41 {
            self.inventory_slot(stream, slot).await?;
        }
        let mut contents = codec::packet(0x12);
        write_varint(0, &mut contents);
        write_varint(i32::from(self.inventory_state), &mut contents);
        write_varint(46, &mut contents);
        for menu_slot in 0..46 {
            let owner_slot = match menu_slot {
                9..=35 => Some(menu_slot),
                36..=44 => Some(menu_slot - 36),
                _ => None,
            };
            super::inventory::write_stack(
                owner_slot.and_then(|slot| self.player.inventory[slot]),
                &mut contents,
            )?;
        }
        super::inventory::write_stack(self.player.cursor, &mut contents)?;
        codec::send(stream, &contents).await?;
        let mut cursor = codec::packet(0x62);
        super::inventory::write_stack(self.player.cursor, &mut cursor)?;
        codec::send(stream, &cursor).await?;
        let mut selected = codec::packet(0x6b);
        write_varint(i32::from(self.player.selected_slot), &mut selected);
        codec::send(stream, &selected).await
    }

    async fn inventory_slot(&mut self, stream: &mut Connection<'_>, slot: u8) -> Result<()> {
        ensure!(slot < 41, "invalid Java inventory synchronization slot");
        let item = self
            .player
            .inventory
            .get(usize::from(slot))
            .copied()
            .flatten();
        let mut packet = codec::packet(0x6e);
        write_varint(i32::from(slot), &mut packet);
        super::inventory::write_stack(item, &mut packet)?;
        codec::send(stream, &packet).await?;
        if let Some(sent) = self.inventory_sent.get_mut(usize::from(slot)) {
            *sent = item;
        }
        Ok(())
    }

    async fn teleport(&mut self, stream: &mut Connection<'_>) -> Result<()> {
        self.teleport_id = self
            .teleport_id
            .checked_add(1)
            .context("Java teleport ID exhausted")?;
        let mut packet = codec::packet(0x49);
        write_varint(self.teleport_id, &mut packet);
        for v in [
            self.player.feet.x,
            self.player.feet.y,
            self.player.feet.z,
            0.0,
            0.0,
            0.0,
        ] {
            packet.extend_from_slice(&v.to_be_bytes());
        }
        packet.extend_from_slice(&(self.player.rotation.yaw as f32).to_be_bytes());
        packet.extend_from_slice(&(self.player.rotation.pitch as f32).to_be_bytes());
        packet.extend_from_slice(&0i32.to_be_bytes()); // Absolute coordinates/velocity.
        codec::send(stream, &packet).await?;
        self.pending_teleport = Some((self.teleport_id, Instant::now()));
        Ok(())
    }

    fn confirm_teleport(&mut self, mut fields: Fields<'_>) -> Result<()> {
        let received = fields.int()?;
        let (expected, _) = self
            .pending_teleport
            .context("unexpected Java teleport confirmation")?;
        ensure!(received == expected, "Java teleport confirmation mismatch");
        // Protocol 777 acknowledges position and rotation as well as the ID.
        // These are confirmation data, never a new movement intent.
        let position = [fields.double()?, fields.double()?, fields.double()?];
        let yaw = f64::from(fields.float()?);
        let pitch = f64::from(fields.float()?);
        fields.end()?;
        ensure!(
            position
                .into_iter()
                .zip([self.player.feet.x, self.player.feet.y, self.player.feet.z])
                .all(|(actual, expected)| actual.is_finite() && (actual - expected).abs() < 1e-5)
                && yaw.is_finite()
                && pitch.is_finite()
                && ((yaw - self.player.rotation.yaw + 180.0).rem_euclid(360.0) - 180.0).abs()
                    < 1e-3
                && (pitch - self.player.rotation.pitch).abs() < 1e-3,
            "Java teleport confirmation disagrees with the server correction"
        );
        self.pending_teleport = None;
        Ok(())
    }

    pub async fn run(
        mut self,
        stream: &mut Connection<'_>,
        frames: &mut Frames,
        mut shutdown: tokio::sync::watch::Receiver<bool>,
    ) -> Result<()> {
        let mut peer_updates = self
            .peer_updates
            .take()
            .context("missing Java peer observation")?;
        let mut pulse = tokio::time::interval(Duration::from_secs(1));
        pulse.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut last_ping = Instant::now();
        let mut peer_resyncs = 0u64;
        loop {
            tokio::select! {
                changed=shutdown.changed()=>{
                    if changed.is_err() || *shutdown.borrow() { return Ok(()); }
                }
                packet=frames.next(stream)=>{
                    let packet=packet?;
                    self.rate.charge(packet.len())?;
                    self.input(stream,&packet).await?;
                }
                event=peer_updates.recv()=>{
                    let peers=self.peers.as_mut().context("missing Java peer projection")?;
                    let changes=match event {
                        Ok(event)=>peers.event(event.as_ref(),&self.interest)?,
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped))=>{
                            let observation=self.world.observe_players(self.player.id).await?;
                            peer_updates=observation.updates;
                            peer_resyncs=peer_resyncs.saturating_add(1);
                            if peer_resyncs<=8 {tracing::debug!(skipped,peer_resyncs,"Java peer snapshot refreshed");}
                            peers.synchronize(observation.players,&self.interest)?
                        }
                        Err(error)=>return Err(error).context("Java peer stream closed"),
                    };
                    send_peer_changes(stream,changes).await?;
                }
                event=self.events.recv()=>{
                    let event=event.context("Java world event backlog; reconnect")?;
                    self.event(stream,event.as_ref()).await?;
                }
                _=pulse.tick()=>{
                    if let Some((_,sent))=self.pending_teleport {
                        ensure!(sent.elapsed()<Duration::from_secs(5),"Java teleport acknowledgement timed out");
                    }
                    if let Some((_,sent))=self.keepalive {
                        ensure!(sent.elapsed()<Duration::from_secs(20),"Java keepalive timed out");
                    } else if last_ping.elapsed()>=Duration::from_secs(10) {
                        self.keepalive_id=self.keepalive_id.checked_add(1).context("Java keepalive ID exhausted")?;
                        let mut packet=codec::packet(0x2d);
                        packet.extend_from_slice(&self.keepalive_id.to_be_bytes());
                        codec::send(stream,&packet).await?;
                        self.keepalive=Some((self.keepalive_id,Instant::now()));
                        last_ping=Instant::now();
                    }
                }
            }
        }
    }

    async fn refresh(&mut self, stream: &mut Connection<'_>, initial: bool) -> Result<()> {
        let center = ChunkPos {
            x: (self.player.feet.x.floor() as i32).div_euclid(16),
            z: (self.player.feet.z.floor() as i32).div_euclid(16),
        };
        if !initial && center == self.center {
            return Ok(());
        }
        let bounds = self.world.capture().await?.bounds();
        let old: Vec<_> = self
            .interest
            .keys()
            .copied()
            .filter(|p| (p.x - center.x).abs() > 2 || (p.z - center.z).abs() > 2)
            .collect();
        for p in old {
            self.interest.remove(&p);
            let mut packet = codec::packet(0x26);
            let word = (u64::from(p.z as u32) << 32) | u64::from(p.x as u32);
            packet.extend_from_slice(&word.to_be_bytes());
            codec::send(stream, &packet).await?;
        }
        let mut packet = codec::packet(0x60);
        write_varint(center.x, &mut packet);
        write_varint(center.z, &mut packet);
        codec::send(stream, &packet).await?;
        let mut additions = Vec::new();
        for z in center.z - 2..=center.z + 2 {
            for x in center.x - 2..=center.x + 2 {
                let p = ChunkPos { x, z };
                if bounds.contains(p) && !self.interest.contains_key(&p) {
                    additions.push(p);
                }
            }
        }
        if additions.is_empty() {
            self.center = center;
            self.refresh_peer_visibility(stream).await?;
            return Ok(());
        }
        codec::send(stream, &[0x0c]).await?;
        let mut count = 0;
        for p in additions {
            let snapshot = self.world.snapshot(p).await?;
            codec::send(stream, &chunk::encode(&snapshot)?).await?;
            self.interest.insert(p, snapshot.revision());
            count += 1;
        }
        let mut done = codec::packet(0x0b);
        write_varint(count, &mut done);
        codec::send(stream, &done).await?;
        self.center = center;
        self.refresh_peer_visibility(stream).await?;
        Ok(())
    }

    async fn refresh_peer_visibility(&mut self, stream: &mut Connection<'_>) -> Result<()> {
        if let Some(peers) = &mut self.peers {
            let changes = peers.refresh(&self.interest);
            send_peer_changes(stream, changes).await?;
        }
        Ok(())
    }

    async fn input(&mut self, stream: &mut Connection<'_>, packet: &[u8]) -> Result<()> {
        if self.world.observes_packets() {
            let id = Fields(packet).int()?;
            self.packet_sequence = self
                .packet_sequence
                .checked_add(1)
                .context("Java packet observation sequence exhausted")?;
            self.world
                .observe_packet(
                    self.player.id,
                    super::super::catalog::java_receipt(id, self.packet_sequence)?,
                )
                .await?;
        }
        let mut fields = Fields(packet);
        let id = fields.int()?;
        match id {
            0 => self.confirm_teleport(fields)?,
            7 => {
                let command = fields.string(256)?.to_owned();
                fields.end()?;
                if let Err(error) = self.world.submit_command(self.player.id, command).await {
                    tracing::debug!(player=self.player.id,%error,"Java command rejected");
                }
            }
            9 => {
                let message = super::chat::read(fields)?;
                if let Err(error) = self.world.chat(self.player.id, message).await {
                    tracing::debug!(player=self.player.id,%error,"Java chat rejected");
                }
            }
            0x44 => match super::forms::custom_response(Fields(fields.0)) {
                Ok(Some((form_id, values))) => {
                    if let Err(error) = self
                        .world
                        .submit_custom_form(self.player.id, form_id, values)
                        .await
                    {
                        tracing::debug!(player=self.player.id,%error,"Java input form reply rejected");
                    }
                }
                Ok(None) => {
                    if let Some((form_id, selection)) = super::forms::response(fields)?
                        && let Err(error) = self
                            .world
                            .submit_form(self.player.id, form_id, selection)
                            .await
                    {
                        tracing::debug!(player=self.player.id,%error,"Java form reply rejected");
                    }
                }
                Err(error) => {
                    tracing::debug!(player=self.player.id,%error,"Java input form reply rejected")
                }
            },
            0x1c => {
                let received = fields.long()?;
                fields.end()?;
                let (expected, _) = self.keepalive.context("unsolicited Java keepalive")?;
                ensure!(received == expected, "Java keepalive mismatch");
                self.keepalive = None;
            }
            0x1e..=0x21 => {
                if self.pending_teleport.is_some() {
                    return Ok(());
                }
                let mut feet = self.player.feet;
                let mut rotation = self.player.rotation;
                if id == 0x1e || id == 0x1f {
                    feet = Position {
                        x: fields.double()?,
                        y: fields.double()?,
                        z: fields.double()?,
                    };
                }
                if id == 0x1f || id == 0x20 {
                    rotation.yaw = f64::from(fields.float()?);
                    rotation.head_yaw = rotation.yaw;
                    rotation.pitch = f64::from(fields.float()?);
                }
                ensure!(fields.byte()? & !3 == 0, "invalid Java movement flags");
                fields.end()?;
                self.tick = self
                    .tick
                    .checked_add(1)
                    .context("Java movement sequence exhausted")?;
                match self
                    .world
                    .move_player(self.player.id, self.tick, feet, rotation)
                    .await?
                {
                    MoveOutcome::Accepted { player, .. } => {
                        self.player = player;
                        self.refresh(stream, false).await?;
                    }
                    MoveOutcome::Rejected {
                        correction: Some(player),
                        reason,
                    } => {
                        tracing::debug!(?reason, "Java movement corrected");
                        self.player = player;
                        self.teleport(stream).await?;
                    }
                    MoveOutcome::Rejected { .. } => bail!("Java player lease expired"),
                }
            }
            0x24 => {
                // PickItemFromBlock, position followed by include-data.
                let position = fields.position()?;
                fields.boolean()?; // Current core blocks have no block-entity data.
                fields.end()?;
                self.player = if self.pending_teleport.is_some() {
                    self.world.player_snapshot(self.player.id).await?
                } else {
                    self.world
                        .pick_block(self.player.id, position)
                        .await?
                        .player
                };
                self.inventory(stream).await?;
            }
            0x36 => {
                let slot = fields.short()?;
                fields.end()?;
                ensure!((0..=8).contains(&slot), "invalid Java held slot");
                self.player = self.world.select_slot(self.player.id, slot as u8).await?;
            }
            0x29 => {
                let action = fields.int()?;
                let position = fields.position()?;
                ensure!(fields.byte()? <= 5, "invalid Java block face");
                let sequence = fields.int()?;
                fields.end()?;
                ensure!((0..=6).contains(&action), "unsupported Java player action");
                if action == 0 || action == 2 {
                    if self.sequence(stream, sequence).await? {
                        if self.pending_teleport.is_some() {
                            self.restore_target(stream, position).await?;
                        } else {
                            let outcome = self.world.break_block(self.player.id, position).await?;
                            self.block_outcome(stream, outcome).await?;
                        }
                    }
                } else if action >= 3 {
                    self.inventory(stream).await?;
                }
            }
            0x42 => {
                let hand = fields.int()?;
                let against = fields.position()?;
                let face = fields.int()?;
                let cursor = [fields.float()?, fields.float()?, fields.float()?];
                fields.boolean()?;
                let border = fields.boolean()?;
                let sequence = fields.int()?;
                fields.end()?;
                ensure!(
                    (0..=1).contains(&hand) && (0..=5).contains(&face),
                    "invalid Java placement hand/face"
                );
                ensure!(
                    cursor
                        .into_iter()
                        .all(|v| v.is_finite() && (0.0..=1.0).contains(&v)),
                    "invalid Java placement cursor"
                );
                if self.sequence(stream, sequence).await? {
                    let mut target = against;
                    match face {
                        0 => target.y -= 1,
                        1 => target.y += 1,
                        2 => target.z -= 1,
                        3 => target.z += 1,
                        4 => target.x -= 1,
                        5 => target.x += 1,
                        _ => unreachable!(),
                    }
                    if hand != 0 || border || self.pending_teleport.is_some() {
                        self.restore_target(stream, target).await?;
                    } else if let Some(kind) =
                        self.player.hotbar[self.player.selected_slot as usize]
                    {
                        let outcome = self
                            .world
                            .place_block(
                                self.player.id,
                                against,
                                face as u8,
                                self.player.selected_slot,
                                kind,
                            )
                            .await?;
                        self.block_outcome(stream, outcome).await?;
                    } else {
                        self.restore_target(stream, target).await?;
                    }
                }
            }
            0x39 => {
                if let Some(change) = super::inventory::creative(fields)? {
                    let outcome = self
                        .world
                        .creative_slot(self.player.id, change.slot, change.kind, change.count)
                        .await?;
                    self.player = outcome.player;
                    if outcome.response.result.is_ok() {
                        // CreativeInventoryScreen keeps its carried stack locally
                        // and submits only the resulting slot replacements. A
                        // full menu/cursor update here destroys that unsent state
                        // between a split and its subsequent placement.
                        self.inventory_slot(stream, change.slot).await?;
                        return Ok(());
                    }
                } else {
                    self.player = self
                        .world
                        .inventory_click(self.player.id, Click::CancelDrag)
                        .await?
                        .player;
                }
                self.inventory(stream).await?;
            }
            0x12 => {
                if let Some(change) = super::inventory::click(fields)? {
                    let stale = change.state_id != self.inventory_state;
                    let drag = matches!(
                        change.click,
                        Click::DragStart { .. } | Click::DragAdd { .. } | Click::DragFinish { .. }
                    );
                    if stale {
                        tracing::debug!(
                            player = self.player.id,
                            "Java inventory prediction state resynchronized"
                        );
                    }
                    let outcome = self
                        .world
                        .inventory_click(
                            self.player.id,
                            if stale && drag {
                                Click::CancelDrag
                            } else {
                                change.click
                            },
                        )
                        .await?;
                    let pending = matches!(
                        change.click,
                        Click::DragStart { .. } | Click::DragAdd { .. }
                    );
                    let accepted = outcome.response.result.is_ok();
                    self.player = outcome.player;
                    // The stock screen keeps its preview between drag phases.
                    // Full contents are required at completion or after rejection.
                    if pending && !stale && accepted {
                        return Ok(());
                    }
                } else {
                    self.player = self
                        .world
                        .inventory_click(self.player.id, Click::CancelDrag)
                        .await?
                        .player;
                }
                self.inventory(stream).await?;
            }
            0x13 => {
                let container = fields.int()?;
                fields.end()?;
                if container == 0 {
                    self.player = self
                        .world
                        .inventory_click(self.player.id, Click::CancelDrag)
                        .await?
                        .player;
                }
            }
            0x0e => client_information(fields)?,
            0x0d | 0x2c => fields.end()?,
            0x0b => {
                let rate = fields.float()?;
                fields.end()?;
                ensure!(rate.is_finite() && rate > 0.0, "invalid Java chunk receipt");
            }
            _ => ensure!((0..=0x44).contains(&id), "unknown Java play packet"),
        }
        Ok(())
    }

    async fn sequence(&mut self, stream: &mut Connection<'_>, sequence: i32) -> Result<bool> {
        ensure!(sequence >= 0, "negative Java block prediction sequence");
        let fresh = self.last_sequence.is_none_or(|last| sequence > last);
        if fresh {
            self.last_sequence = Some(sequence);
        }
        let mut packet = codec::packet(4);
        write_varint(sequence, &mut packet);
        codec::send(stream, &packet).await?;
        Ok(fresh)
    }

    async fn restore_target(&self, stream: &mut Connection<'_>, p: BlockPos) -> Result<()> {
        if let Ok(snapshot) = self.world.snapshot(p.chunk()).await {
            let (x, _, z) = p.local();
            if let Ok(kind) = snapshot.get(x, p.y, z) {
                update(stream, p, kind).await?;
            }
        }
        Ok(())
    }

    async fn block_outcome(
        &self,
        stream: &mut Connection<'_>,
        outcome: BlockOutcome,
    ) -> Result<()> {
        match outcome {
            BlockOutcome::Rejected {
                position,
                current: Some(kind),
                ..
            }
            | BlockOutcome::NoChange {
                position,
                current: kind,
            } => {
                update(stream, position, kind).await?;
            }
            _ => {}
        }
        Ok(())
    }

    async fn event(&mut self, stream: &mut Connection<'_>, event: &WorldEvent) -> Result<()> {
        if let WorldEvent::InventoryUpdated { player_id } = event {
            if *player_id == self.player.id {
                self.player = self.world.player_snapshot(self.player.id).await?;
                for slot in 0..36 {
                    let value = |stack: Option<sculith_game::inventory::ItemStack>| {
                        stack.map(|s| (s.kind, s.count))
                    };
                    if value(self.player.inventory[slot]) != value(self.inventory_sent[slot]) {
                        self.inventory_slot(stream, slot as u8).await?;
                    }
                }
                // Server grants change player slots, never the creative
                // screen's locally carried stack or owner cursor/output.
            }
            return Ok(());
        }
        if let WorldEvent::Chat(chat) = event {
            let mut packet = codec::packet(0x7c);
            codec::text(&format!("<{}> {}", chat.name, chat.message), &mut packet)?;
            packet.push(0);
            codec::send(stream, &packet).await?;
        }
        match event {
            WorldEvent::ModelShown(view) if view.player_id == self.player.id => {
                for packet in super::models::show(view)? {
                    codec::send(stream, &packet).await?;
                }
            }
            WorldEvent::ModelHidden {
                player_id,
                entity_id,
            } if *player_id == self.player.id => {
                codec::send(stream, &super::models::remove(*entity_id)?).await?;
            }
            _ => {}
        }
        if let WorldEvent::Sound(effect) = event
            && effect.player_id == self.player.id
        {
            let mut packet = codec::packet(119); // 26.3 direct SoundEvent holder.
            write_varint(0, &mut packet);
            write_string(&effect.sound, &mut packet);
            packet.push(0); // No fixed range override.
            write_varint(7, &mut packet); // Players category.
            for coordinate in [effect.position.x, effect.position.y, effect.position.z] {
                packet.extend_from_slice(&((coordinate * 8.0) as i32).to_be_bytes());
            }
            packet.extend_from_slice(&effect.volume.to_be_bytes());
            packet.extend_from_slice(&effect.pitch.to_be_bytes());
            packet.extend_from_slice(&0i64.to_be_bytes());
            codec::send(stream, &packet).await?;
        }
        if let WorldEvent::CustomFormRequested {
            player_id,
            form_id,
            form,
        } = event
            && *player_id == self.player.id
        {
            codec::send(stream, &super::forms::show_custom(*form_id, form)?).await?;
        }
        if let WorldEvent::FormRequested {
            player_id,
            form_id,
            form,
        } = event
            && *player_id == self.player.id
        {
            codec::send(stream, &super::forms::show(*form_id, form)?).await?;
        }
        if let WorldEvent::Message { player_id, message } = event
            && *player_id == self.player.id
        {
            let mut packet = codec::packet(0x7c); // Protocol 777 SystemChat.
            codec::text(message, &mut packet)?;
            packet.push(0); // Chat, rather than action-bar overlay.
            codec::send(stream, &packet).await?;
        }
        if let WorldEvent::Blocks {
            batch, revisions, ..
        }
        | WorldEvent::PluginBlocks {
            batch, revisions, ..
        } = event
        {
            for (p, revision) in revisions {
                let Some(known) = self.interest.get(p).copied() else {
                    continue;
                };
                if *revision <= known {
                    continue;
                }
                let snapshot = self.world.snapshot(*p).await?;
                if *revision == known + 1 && snapshot.revision() == *revision {
                    for change in batch.changes.iter().filter(|c| c.position.chunk() == *p) {
                        update(stream, change.position, change.after).await?;
                    }
                    codec::send(stream, &chunk::light_update(&snapshot)?).await?;
                } else {
                    codec::send(stream, &chunk::encode(&snapshot)?).await?;
                }
                self.interest.insert(*p, snapshot.revision());
            }
        }
        Ok(())
    }
}

async fn update(stream: &mut Connection<'_>, position: BlockPos, kind: BlockKind) -> Result<()> {
    let mut packet = codec::packet(8);
    codec::position(position, &mut packet);
    write_varint(registry::get().state(kind), &mut packet);
    codec::send(stream, &packet).await
}

async fn send_peer_changes(stream: &mut Connection<'_>, changes: Vec<PeerChange>) -> Result<()> {
    for change in changes {
        for packet in super::peers::packets(&change)? {
            codec::send(stream, &packet).await?;
        }
    }
    Ok(())
}
