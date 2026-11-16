//! One bounded resource pack, with explicit download/cache/stack transitions.
use super::{Observed, Packets, Transport, V2225};
use anyhow::{Context, Result, bail, ensure};
use bedrock::protocol::{
    ProtoVersion,
    v662::{
        enums::PackType,
        packets::{ResourcePackChunkDataPacket, ResourcePackDataInfoPacket},
        types::{BaseGameVersion, Experiments},
    },
    v898::packets::{PackEntry, ResourcePackStackPacket},
    v2168::packets::{
        ResourcePackClientResponsePacket, ResourcePackEntry, ResourcePacksInfoPacket,
    },
};

const CHUNK_SIZE: usize = 32 * 1024;

pub(super) async fn negotiate<T: Transport>(
    conn: &mut Observed<'_, T>,
    packets: &mut Packets,
    content: Option<&crate::content::Content>,
) -> Result<()> {
    let pack = content.map(|content| &content.bundle);
    let required = pack.is_some();
    let entries = pack
        .map(|pack| {
            vec![ResourcePackEntry {
                id: pack.bedrock_id,
                version: pack.bedrock_version.clone(),
                size: pack.bedrock.bytes.len() as u64,
                content_key: String::new(),
                sub_pack_name: String::new(),
                content_identity: String::new(),
                has_scripts: false,
                is_addon: false,
                is_ray_tracing_capable: false,
                cdn_url: String::new(),
            }]
        })
        .unwrap_or_default();
    conn.send(&[V2225::ResourcePacksInfoPacket(Box::new(
        ResourcePacksInfoPacket {
            resource_pack_required: required,
            has_addon_packs: false,
            has_scripts: false,
            force_disable_vibrant_visuals: false,
            world_template_uuid: uuid::Uuid::nil(),
            world_template_version: String::new(),
            resource_packs: entries,
        },
    ))])
    .await?;
    let mut stack_sent = false;
    let mut cache_seen = false;
    let mut downloading: Option<String> = None;
    let mut chunks: Vec<u8> = Vec::new();
    // Shared startup deadline also bounds cache/transfer negotiation. Each chunk
    // may be requested twice; reliable-transport replay is handled below us.
    for _ in 0..4096 {
        match packets.next(conn).await? {
            V2225::ClientCacheStatusPacket(_) if !cache_seen => cache_seen = true,
            V2225::ResourcePackClientResponsePacket(response) => match *response {
                ResourcePackClientResponsePacket::Cancel => {
                    bail!("client cancelled required resource packs")
                }
                ResourcePackClientResponsePacket::Downloading(names)
                    if !stack_sent && downloading.is_none() =>
                {
                    let pack = pack.context("requested an unadvertised resource pack")?;
                    ensure!(names.len() == 1, "expected one advertised pack request");
                    let name = &names[0];
                    let id = pack.bedrock_id.to_string();
                    ensure!(
                        name == &id || name == &format!("{id}_{}", pack.bedrock_version),
                        "requested pack ID/version mismatch"
                    );
                    chunks.resize(pack.bedrock.bytes.len().div_ceil(CHUNK_SIZE), 0);
                    conn.send(&[V2225::ResourcePackDataInfoPacket(Box::new(
                        ResourcePackDataInfoPacket {
                            resource_name: name.clone(),
                            chunk_size: CHUNK_SIZE as u32,
                            chunk_amount: chunks.len() as u32,
                            file_size: pack.bedrock.bytes.len() as u64,
                            file_hash: pack.bedrock.sha256.to_vec(),
                            is_premium: false,
                            pack_type: PackType::Resources,
                        },
                    ))])
                    .await?;
                    downloading = Some(name.clone());
                }
                ResourcePackClientResponsePacket::DownloadingFinished if !stack_sent => {
                    ensure!(
                        chunks.iter().all(|count| *count > 0),
                        "pack completed before all requested chunks were served"
                    );
                    let addon_list = pack
                        .map(|pack| {
                            vec![PackEntry {
                                id: pack.bedrock_id.to_string(),
                                version: pack.bedrock_version.clone(),
                                sub_pack_name: String::new(),
                            }]
                        })
                        .unwrap_or_default();
                    conn.send(&[V2225::ResourcePackStackPacket(Box::new(
                        ResourcePackStackPacket {
                            texture_pack_required: required,
                            addon_list,
                            base_game_version: BaseGameVersion(V2225::GAME_VERSION.into()),
                            experiments: Experiments {
                                experiments: vec![],
                                ever_toggled: false,
                            },
                            include_editor_packs: false,
                        },
                    ))])
                    .await?;
                    stack_sent = true;
                }
                ResourcePackClientResponsePacket::ResourcePackStackFinished if stack_sent => {
                    if let Some(pack) = pack {
                        tracing::info!(pack_id=%pack.bedrock_id, cached=downloading.is_none(), "Bedrock client accepted the shared resource pack stack");
                    }
                    return Ok(());
                }
                _ => bail!("resource-pack response out of sequence"),
            },
            V2225::ResourcePackChunkRequestPacket(request) if !stack_sent => {
                let pack = pack.context("unadvertised resource pack chunk")?;
                let name = downloading
                    .as_ref()
                    .context("pack chunk requested before data info")?;
                ensure!(&request.resource_name == name, "pack chunk ID mismatch");
                let index = request.chunk as usize;
                let count = chunks
                    .get_mut(index)
                    .context("pack chunk index out of bounds")?;
                ensure!(*count < 2, "pack chunk retry budget exceeded");
                *count += 1;
                let start = index * CHUNK_SIZE;
                let end = (start + CHUNK_SIZE).min(pack.bedrock.bytes.len());
                conn.send(&[V2225::ResourcePackChunkDataPacket(Box::new(
                    ResourcePackChunkDataPacket {
                        resource_name: name.clone(),
                        chunk_id: request.chunk,
                        byte_offset: start as u64,
                        chunk_data: pack.bedrock.bytes[start..end].to_vec(),
                    },
                ))])
                .await?;
            }
            _ => bail!("unexpected packet during resource-pack negotiation"),
        }
    }
    bail!("too many resource pack negotiation packets")
}
