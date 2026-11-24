use crate::entity::player::Player;
use base64::{Engine as _, engine::general_purpose};
use pumpkin_data::packet::{CURRENT_MC_VERSION, LOWEST_SUPPORTED_MC_VERSION};
use sculith_config::BasicConfiguration;
use sculith_protocol::{
    Players, Sample, StatusResponse, Version,
    java::client::{config::CPluginMessage, status::CStatusResponse},
};
use sculith_util::text::TextComponent;
use std::{
    fs,
    io::{self, Cursor},
    path::Path,
};
use tracing::warn;
use uuid::Uuid;

const DEFAULT_ICON_PATH: &str = "server-icon.png";
const MAX_SAMPLE_PLAYERS: usize = 12;

fn load_icon_from_file(path: &Path) -> io::Result<String> {
    if !path
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("png"))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "expected a PNG file",
        ));
    }
    let buf = fs::read(path)?;
    let reader = png::Decoder::new(Cursor::new(&buf))
        .read_info()
        .map_err(io::Error::other)?;
    let info = reader.info();
    if info.width != 64 || info.height != 64 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "icon dimensions must be 64x64",
        ));
    }
    let mut result = "data:image/png;base64,".to_owned();
    general_purpose::STANDARD.encode_string(&buf, &mut result);
    Ok(result)
}

pub struct CachedStatus {
    pub status_response: StatusResponse,
    player_samples: Vec<(Uuid, String)>,
}

pub struct CachedBranding {
    /// Cached server brand buffer so we don't have to rebuild them every time a player joins
    cached_server_brand: &'static [u8],
}

impl Default for CachedBranding {
    fn default() -> Self {
        Self::new()
    }
}

impl CachedBranding {
    const BRAND: &'static str = "Sculith";
    const BRAND_BYTES: &'static [u8] = &{
        let brand = Self::BRAND.as_bytes();
        let len = brand.len();
        assert!(len < 128, "Brand length must fit in 1-byte VarInt");
        let mut bytes = [0u8; 1 + Self::BRAND.len()];
        bytes[0] = len as u8;
        let mut i = 0;
        while i < len {
            bytes[i + 1] = brand[i];
            i += 1;
        }
        bytes
    };

    #[must_use]
    pub const fn new() -> Self {
        Self {
            cached_server_brand: Self::BRAND_BYTES,
        }
    }

    #[must_use]
    pub const fn get_branding(&self) -> CPluginMessage<'_> {
        CPluginMessage::new("minecraft:brand", self.cached_server_brand)
    }
}

impl CachedStatus {
    #[must_use]
    pub fn new(config: &BasicConfiguration, motd: &str, max_players: u32) -> Self {
        let status_response = Self::build_response(config, motd, max_players);

        Self {
            status_response,
            player_samples: Vec::new(),
        }
    }

    pub fn get_status_response(&self, client_protocol: i32) -> StatusResponse {
        let mut response = self.status_response.clone();

        let supported_min = LOWEST_SUPPORTED_MC_VERSION.protocol_version();
        let supported_max = CURRENT_MC_VERSION.protocol_version();

        if client_protocol >= supported_min
            && client_protocol <= supported_max
            && let Some(version) = &mut response.version
        {
            version.protocol = client_protocol as u32;
        }

        response
    }

    pub fn get_status_packet(&self, client_protocol: i32) -> CStatusResponse {
        let response = self.get_status_response(client_protocol);
        let json = serde_json::to_string(&response).unwrap_or_default();
        CStatusResponse::new(json)
    }

    fn build_sample_list(&self) -> Vec<Sample> {
        self.player_samples
            .iter()
            .take(MAX_SAMPLE_PLAYERS)
            .map(|(id, name)| Sample {
                name: name.clone(),
                id: id.to_string(),
            })
            .collect()
    }

    pub fn add_player(&mut self, player: &Player) {
        let player_id = player.gameprofile.id;
        let player_name = player.gameprofile.name.clone();

        if !self.player_samples.iter().any(|(id, _)| *id == player_id) {
            self.player_samples.push((player_id, player_name));
            let sample = self.build_sample_list();

            if let Some(players) = &mut self.status_response.players {
                players.online = players.online.saturating_add(1);
                players.sample = sample;
            }
        }
    }

    pub fn remove_player(&mut self, player: &Player) {
        let player_id = player.gameprofile.id;

        if self.player_samples.iter().any(|(id, _)| *id == player_id) {
            self.player_samples.retain(|(id, _)| *id != player_id);
            let sample = self.build_sample_list();

            if let Some(players) = &mut self.status_response.players {
                players.online = players.online.saturating_sub(1);
                players.sample = sample;
            }
        }
    }

    pub fn build_response(
        config: &BasicConfiguration,
        motd: &str,
        max_players: u32,
    ) -> StatusResponse {
        let favicon = if config.use_favicon {
            let path = config.favicon_path.as_deref().unwrap_or(DEFAULT_ICON_PATH);
            match load_icon_from_file(Path::new(path)) {
                Ok(icon) => Some(icon),
                Err(error)
                    if error.kind() == io::ErrorKind::NotFound && config.favicon_path.is_none() =>
                {
                    None
                }
                Err(error) => {
                    warn!("Failed to load server icon from '{path}': {error}");
                    None
                }
            }
        } else {
            None
        };

        StatusResponse {
            version: Some(Version {
                name: if LOWEST_SUPPORTED_MC_VERSION == CURRENT_MC_VERSION {
                    CURRENT_MC_VERSION.to_string()
                } else {
                    format!("{LOWEST_SUPPORTED_MC_VERSION}-{CURRENT_MC_VERSION}")
                },
                protocol: CURRENT_MC_VERSION.protocol_version() as u32,
            }),
            players: Some(Players {
                max: max_players,
                online: 0,
                sample: vec![],
            }),
            description: TextComponent::text(motd.to_string()),
            favicon,
            // This should stay true even when reports are disabled.
            // It prevents the annoying popup when joining the server.
            enforce_secure_chat: true,
        }
    }
}

impl Default for CachedStatus {
    fn default() -> Self {
        Self::new(&BasicConfiguration::default(), "Sculith", 1000)
    }
}
