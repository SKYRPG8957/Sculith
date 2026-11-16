//! Small socket fixture for cross-adapter owner tests. Independent full wire
//! and voxel/lighting validation lives in tests/interop/java_smoke.py.
use super::{
    codec::{self, Fields},
    write_string, write_varint,
};
use anyhow::{Context, Result, bail, ensure};
use sculith_world::BlockPos;
use std::{net::SocketAddr, time::Duration};
use tokio::{io::AsyncReadExt, net::TcpStream};

pub struct Probe {
    stream: super::transport::Stream<TcpStream>,
}
impl Probe {
    pub async fn enter(address: SocketAddr) -> Result<Self> {
        Self::enter_mode(address, None).await
    }
    pub async fn enter_online(
        address: SocketAddr,
        uuid: uuid::Uuid,
        corrupt_challenge: bool,
    ) -> Result<Self> {
        Self::enter_mode(address, Some((uuid, corrupt_challenge))).await
    }
    async fn enter_mode(address: SocketAddr, online: Option<(uuid::Uuid, bool)>) -> Result<Self> {
        let mut stream = super::transport::Stream::new(TcpStream::connect(address).await?);
        let mut packet = codec::packet(0);
        write_varint(777, &mut packet);
        write_string("localhost", &mut packet);
        packet.extend_from_slice(&address.port().to_be_bytes());
        packet.push(2);
        codec::send(&mut stream, &packet).await?;
        let mut packet = codec::packet(0);
        write_string("JavaShared", &mut packet);
        packet.extend_from_slice(online.map_or(uuid::Uuid::nil(), |v| v.0).as_bytes());
        codec::send(&mut stream, &packet).await?;
        let mut client = Self { stream };
        if let Some((_, corrupt)) = online {
            use aws_lc_rs::rsa::{Pkcs1PublicEncryptingKey, PublicEncryptingKey};
            let packet = client.next().await?;
            let mut fields = Fields(&packet);
            ensure!(
                fields.int()? == 1 && fields.string(20)?.is_empty(),
                "expected Java Encryption Request"
            );
            let key = Pkcs1PublicEncryptingKey::new(PublicEncryptingKey::from_der(
                fields.byte_array(512)?,
            )?)?;
            let mut challenge = fields.byte_array(64)?.to_vec();
            ensure!(fields.boolean()?, "authentication must be required");
            fields.end()?;
            ensure!(challenge.len() == 16, "expected bounded fresh challenge");
            if corrupt {
                challenge[0] ^= 1;
            }
            let mut packet = codec::packet(1);
            for plain in [&[0x42u8; 16][..], &challenge[..]] {
                let mut encrypted = vec![0; key.ciphertext_size()];
                let encrypted = key.encrypt(plain, &mut encrypted)?;
                write_varint(encrypted.len() as i32, &mut packet);
                packet.extend_from_slice(encrypted);
            }
            codec::send(&mut client.stream, &packet).await?;
            client.stream.enable_encryption(&[0x42; 16])?;
        }
        let packet = client.next().await?;
        ensure!(Fields(&packet).int()? == 2, "expected Java LoginSuccess");
        codec::send(&mut client.stream, &[3]).await?;
        let mut configured = false;
        for _ in 0..40 {
            let packet = client.next().await?;
            let mut fields = Fields(&packet);
            match fields.int()? {
                0x0f => {
                    let mut reply = codec::packet(7);
                    write_varint(1, &mut reply);
                    for value in ["minecraft", "core", "26.3"] {
                        write_string(value, &mut reply);
                    }
                    codec::send(&mut client.stream, &reply).await?;
                }
                3 => {
                    codec::send(&mut client.stream, &[3]).await?;
                    configured = true;
                    break;
                }
                7 | 0x0d | 0x0e => {}
                _ => bail!("Java fixture configuration failed"),
            }
        }
        ensure!(configured, "Java fixture configuration did not finish");
        let mut chunks = 0;
        let mut initialized = false;
        for _ in 0..128 {
            let packet = client.next().await?;
            let mut fields = Fields(&packet);
            match fields.int()? {
                0x2e => chunks += 1,
                0x49 => {
                    ensure!(chunks == 25, "Java fixture did not receive all chunks");
                    let mut confirm = vec![0];
                    write_varint(fields.int()?, &mut confirm);
                    confirm.extend_from_slice(fields.take(24)?); // Confirmed coordinates.
                    fields.take(24)?; // Server velocity is not in the acknowledgement.
                    confirm.extend_from_slice(fields.take(8)?); // Yaw/pitch.
                    ensure!(fields.take(4)? == [0; 4], "expected absolute teleport");
                    fields.end()?;
                    codec::send(&mut client.stream, &confirm).await?;
                    codec::send(&mut client.stream, &[0x2c]).await?;
                    initialized = true;
                    break;
                }
                0x20 => bail!("Java fixture disconnected before spawn"),
                _ => {}
            }
        }
        ensure!(initialized, "Java fixture spawn did not finish");
        Ok(client)
    }
    async fn next(&mut self) -> Result<Vec<u8>> {
        tokio::time::timeout(Duration::from_secs(5), async {
            let mut prefix = Vec::new();
            loop {
                ensure!(prefix.len() < 5, "Java fixture oversized prefix");
                let byte = self.stream.read_u8().await?;
                prefix.push(byte);
                if byte & 128 == 0 {
                    break;
                }
            }
            let length = super::read_varint(&mut &prefix[..])?;
            ensure!(
                (1..=1 << 20).contains(&length),
                "Java fixture frame too large"
            );
            let mut bytes = vec![0; length as usize];
            self.stream.read_exact(&mut bytes).await?;
            Ok(bytes)
        })
        .await
        .context("Java fixture timed out")?
    }
    pub async fn next_block(&mut self) -> Result<(BlockPos, i32)> {
        for _ in 0..16 {
            let packet = self.next().await?;
            let mut fields = Fields(&packet);
            if fields.int()? == 8 {
                let point = fields.position()?;
                let state = fields.int()?;
                fields.end()?;
                return Ok((point, state));
            }
        }
        bail!("Java fixture did not receive a block update")
    }
    pub async fn place_stone(&mut self, against: BlockPos, sequence: i32) -> Result<()> {
        let mut packet = codec::packet(0x42);
        write_varint(0, &mut packet);
        codec::position(against, &mut packet);
        write_varint(1, &mut packet);
        for value in [0.5f32, 1.0, 0.5] {
            packet.extend_from_slice(&value.to_be_bytes());
        }
        packet.extend_from_slice(&[0, 0]);
        write_varint(sequence, &mut packet);
        codec::send(&mut self.stream, &packet).await
    }
}
