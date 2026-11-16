//! Loopback-only, explicitly offline protocol-777 shared-world preview.
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
use sculith_world::{BlockKind, BlockPos, ChunkPos};
use std::{
    collections::BTreeMap,
    time::{Duration, Instant},
};
use tokio::{net::TcpStream, sync::broadcast};
use uuid::Uuid;

pub struct Session {
    _lease: PlayerLease,
    player: PlayerSnapshot,
    world: WorldHandle,
    events: broadcast::Receiver<std::sync::Arc<WorldEvent>>,
    interest: BTreeMap<ChunkPos, u64>,
    center: ChunkPos,
    tick: u64,
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
    stream: &mut TcpStream,
    frames: &mut Frames,
    world: WorldHandle,
    max_players: u32,
    phase: &mut super::Phase,
    content: Option<&crate::content::Content>,
) -> Result<Session> {
    let packet = frames.next(stream).await?;
    let mut fields = Fields(&packet);
    ensure!(fields.int()? == 0, "expected Java Login Start");
    let name = fields.string(16)?.to_owned();
    ensure!(
        !name.is_empty() && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_'),
        "invalid Java profile name"
    );
    let _unverified_uuid = fields.take(16)?;
    fields.end()?;
    let uuid = offline_uuid(&name);
    let mut login = codec::packet(2);
    login.extend_from_slice(uuid.as_bytes());
    write_string(&name, &mut login);
    write_varint(0, &mut login); // Signed profile properties absent in offline mode.
    login.extend_from_slice(Uuid::new_v4().as_bytes()); // Protocol 777 session ID.
    codec::send(stream, &login).await?;
    let ack = frames.next(stream).await?;
    ensure!(ack == [3], "expected Java Login Acknowledged");
    *phase = super::Phase::Configuration;
    configure(stream, frames, content).await?;
    *phase = super::Phase::Play;
    let events = world.subscribe();
    let lease = world
        .reserve(
            Profile {
                subject: format!("offline-java:{}", name.to_ascii_lowercase()),
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
        interest: BTreeMap::new(),
        center: ChunkPos { x: 0, z: 0 },
        tick: 0,
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
            tracing::info!(player=%state.player.profile.name, entity_id=state.player.id,
                "Java client acknowledged the shared flat-world spawn");
            return Ok(state);
        }
    }
    bail!("too many Java packets before initialization")
}

async fn configure(
    stream: &mut TcpStream,
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
    async fn spawn(&mut self, stream: &mut TcpStream, max_players: u32) -> Result<()> {
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

    async fn inventory(&self, stream: &mut TcpStream) -> Result<()> {
        for slot in 0..41 {
            let kind = self.player.hotbar.get(slot).copied().flatten();
            let mut packet = codec::packet(0x6e);
            write_varint(slot as i32, &mut packet);
            if let Some(kind) = kind {
                write_varint(64, &mut packet);
                write_varint(registry::get().item(kind), &mut packet);
                packet.extend_from_slice(&[0, 0]); // No component patches.
            } else {
                write_varint(0, &mut packet);
            }
            codec::send(stream, &packet).await?;
        }
        let mut selected = codec::packet(0x6b);
        write_varint(i32::from(self.player.selected_slot), &mut selected);
        codec::send(stream, &selected).await
    }

    async fn teleport(&mut self, stream: &mut TcpStream) -> Result<()> {
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
        stream: &mut TcpStream,
        frames: &mut Frames,
        mut shutdown: tokio::sync::watch::Receiver<bool>,
    ) -> Result<()> {
        let mut pulse = tokio::time::interval(Duration::from_secs(1));
        pulse.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut last_ping = Instant::now();
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

    async fn refresh(&mut self, stream: &mut TcpStream, initial: bool) -> Result<()> {
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
        Ok(())
    }

    async fn input(&mut self, stream: &mut TcpStream, packet: &[u8]) -> Result<()> {
        let mut fields = Fields(packet);
        let id = fields.int()?;
        match id {
            0 => self.confirm_teleport(fields)?,
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
            0x12 | 0x39 => {
                self.inventory(stream).await?;
            } // Never trust client inventory/creative stacks.
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

    async fn sequence(&mut self, stream: &mut TcpStream, sequence: i32) -> Result<bool> {
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

    async fn restore_target(&self, stream: &mut TcpStream, p: BlockPos) -> Result<()> {
        if let Ok(snapshot) = self.world.snapshot(p.chunk()).await {
            let (x, _, z) = p.local();
            if let Ok(kind) = snapshot.get(x, p.y, z) {
                update(stream, p, kind).await?;
            }
        }
        Ok(())
    }

    async fn block_outcome(&self, stream: &mut TcpStream, outcome: BlockOutcome) -> Result<()> {
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

    async fn event(&mut self, stream: &mut TcpStream, event: &WorldEvent) -> Result<()> {
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

async fn update(stream: &mut TcpStream, position: BlockPos, kind: BlockKind) -> Result<()> {
    let mut packet = codec::packet(8);
    codec::position(position, &mut packet);
    write_varint(registry::get().state(kind), &mut packet);
    codec::send(stream, &packet).await
}
