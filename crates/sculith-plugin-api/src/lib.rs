//! Versioned JSON-lines API for trusted Rust plugin executables.
//! stdout is reserved for this protocol; diagnostics belong on stderr.

use std::io::{self, BufRead, Write};

use sculith_core::ServerStatus;
use serde::{Deserialize, Serialize};

pub const API_VERSION: u32 = 12;
pub const LEGACY_API_VERSION: u32 = 1;
pub const GAME_API_VERSION: u32 = 2;
pub const UI_API_VERSION: u32 = 3;
pub const SOUND_API_VERSION: u32 = 4;
pub const MODEL_API_VERSION: u32 = 5;
pub const OBSERVATION_API_VERSION: u32 = 6;
pub const INVENTORY_API_VERSION: u32 = 7;
pub const CONNECTION_API_VERSION: u32 = 8;
pub const PACKET_SENT_API_VERSION: u32 = 9;
pub const WORLD_API_VERSION: u32 = 10;
pub const CUSTOM_FORM_API_VERSION: u32 = 11;
pub const CONSOLE_API_VERSION: u32 = 12;
pub const SUPPORTED_API_VERSIONS: &[u32] = &[
    LEGACY_API_VERSION,
    GAME_API_VERSION,
    UI_API_VERSION,
    SOUND_API_VERSION,
    MODEL_API_VERSION,
    OBSERVATION_API_VERSION,
    INVENTORY_API_VERSION,
    CONNECTION_API_VERSION,
    PACKET_SENT_API_VERSION,
    WORLD_API_VERSION,
    CUSTOM_FORM_API_VERSION,
    CONSOLE_API_VERSION,
];
pub use sculith_core::connections::{
    ConnectionEvent, ConnectionPacket, ConnectionTransport, PacketPhase,
};
pub use sculith_core::inventory::{
    InventoryError, InventoryItem, InventoryOperation, InventoryResult, InventoryView,
};
pub use sculith_core::world::{
    BlockChange, RegionInfo, WorldActor, WorldBlock, WorldError, WorldOperation, WorldPosition,
    WorldResult,
};
pub use sculith_core::{
    CommandSpec, CustomForm, Edition, FormField, FormValue, PacketDispatch, PacketInfo, SimpleForm,
};
pub const MAX_MESSAGE_BYTES: usize = 65_536;
pub const MAX_ACTIONS: usize = 32;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Hook {
    Connection,
    PacketSent,
    WorldResponse,
    WorldBlocksChanged,
    Started,
    Tick,
    Stopping,
    PlayerJoined,
    PlayerLeft,
    BlocksChanged,
    FormSubmitted,
    CustomFormSubmitted,
    PlayerMoved,
    PlayerEquipped,
    PacketReceived,
    PlayerChat,
    PlayerInventoryChanged,
    InventoryResponse,
    CommandSubmitted,
    ConsoleCommand,
}

impl Hook {
    pub const ALL: &[Self] = &[
        Self::Connection,
        Self::PacketSent,
        Self::WorldResponse,
        Self::WorldBlocksChanged,
        Self::Started,
        Self::Tick,
        Self::Stopping,
        Self::PlayerJoined,
        Self::PlayerLeft,
        Self::BlocksChanged,
        Self::FormSubmitted,
        Self::CustomFormSubmitted,
        Self::PlayerMoved,
        Self::PlayerEquipped,
        Self::PacketReceived,
        Self::PlayerChat,
        Self::CommandSubmitted,
        Self::ConsoleCommand,
        Self::PlayerInventoryChanged,
        Self::InventoryResponse,
    ];
    pub fn minimum_api(&self) -> u32 {
        match self {
            Self::ConsoleCommand => CONSOLE_API_VERSION,
            Self::WorldResponse | Self::WorldBlocksChanged => WORLD_API_VERSION,
            Self::Connection => CONNECTION_API_VERSION,
            Self::PacketSent => PACKET_SENT_API_VERSION,
            Self::Started | Self::Tick | Self::Stopping => LEGACY_API_VERSION,
            Self::FormSubmitted => UI_API_VERSION,
            Self::CustomFormSubmitted => CUSTOM_FORM_API_VERSION,
            Self::InventoryResponse => INVENTORY_API_VERSION,
            Self::PlayerMoved
            | Self::PlayerEquipped
            | Self::PacketReceived
            | Self::PlayerChat
            | Self::PlayerInventoryChanged
            | Self::CommandSubmitted => OBSERVATION_API_VERSION,
            Self::PlayerJoined | Self::PlayerLeft | Self::BlocksChanged => GAME_API_VERSION,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Metadata {
    pub name: String,
    pub version: String,
    pub hooks: Vec<Hook>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Request {
    pub id: u64,
    pub message: HostMessage,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HostMessage {
    Hello {
        api_version: u32,
        /// Old v1 SDKs ignore this additive field and still select api_version.
        #[serde(default)]
        supported_versions: Vec<u32>,
        server: ServerStatus,
    },
    Event {
        event: Event,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    /// Private to the plugin that submitted this world operation.
    WorldResponse {
        request_id: u32,
        result: WorldResult,
    },
    /// Committed player or plugin block changes; no event for rejected/no-op edits.
    WorldBlocksChanged {
        region_id: u64,
        previous_revision: u64,
        revision: u64,
        actor: WorldActor,
        changes: Vec<BlockChange>,
    },
    /// Opt-in lifecycle and packet metadata, including pre-login.
    /// PacketSent is delivered only to the separate API-v9 hook.
    Connection {
        event: ConnectionEvent,
    },
    /// Returned only to the plugin that issued this request.
    InventoryResponse {
        player_id: u64,
        request_id: u32,
        result: InventoryResult,
    },
    /// Owner-committed item changes; stack-network IDs remain adapter-private.
    PlayerInventoryChanged {
        player_id: u64,
        changes: Vec<InventoryChange>,
    },
    /// Private to the plugin that registered this command. No shell evaluation.
    CommandSubmitted {
        player_id: u64,
        name: String,
        command: String,
        arguments: String,
    },
    /// Local operator command; private to its registered owner, with no player ID.
    ConsoleCommand {
        command: String,
        arguments: String,
    },
    /// Validated public chat; the author comes from the initialized session.
    PlayerChat {
        player_id: u64,
        name: String,
        message: String,
    },
    Started {
        server: ServerStatus,
    },
    Tick {
        tick: u64,
    },
    Stopping,
    PlayerJoined {
        player: PlayerInfo,
    },
    /// Runtime disconnect notification; use Stopping for whole-server shutdown.
    PlayerLeft {
        player_id: u64,
    },
    /// Owner-committed movement, only when the position/rotation changed.
    PlayerMoved {
        player_id: u64,
        tick: u64,
        x: f64,
        y: f64,
        z: f64,
        yaw: f64,
        pitch: f64,
        head_yaw: f64,
    },
    /// Owner-selected slot and committed held item/count, never client claims.
    PlayerEquipped {
        player_id: u64,
        slot: u8,
        item: Option<String>,
        count: u16,
    },
    /// Receipt metadata for an initialized session, before the gameplay handler.
    /// No raw body, account token, identity chain, address or skin is included.
    PacketReceived {
        player_id: u64,
        packet: PacketInfo,
    },
    /// Post-commit notification. This event cannot cancel or rewrite a commit.
    BlocksChanged {
        player_id: u64,
        revision: u64,
        changes: Vec<BlockChange>,
    },
    /// Ordered validated input values. None means cancelled; private to the owner.
    CustomFormSubmitted {
        player_id: u64,
        request_id: u32,
        values: Option<Vec<FormValue>>,
    },
    /// Delivered only to the plugin that showed this form. None means cancelled.
    FormSubmitted {
        player_id: u64,
        request_id: u32,
        selection: Option<u32>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlayerInfo {
    pub id: u64,
    pub name: String,
    pub edition: Edition,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InventoryContainer {
    Inventory,
    Cursor,
    CreatedOutput,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InventoryChange {
    pub container: InventoryContainer,
    pub slot: u8,
    pub item: Option<String>,
    pub count: u16,
}

impl Event {
    pub fn hook(&self) -> Hook {
        match self {
            Self::WorldResponse { .. } => Hook::WorldResponse,
            Self::WorldBlocksChanged { .. } => Hook::WorldBlocksChanged,
            Self::Connection {
                event: ConnectionEvent::PacketSent { .. },
            } => Hook::PacketSent,
            Self::Connection { .. } => Hook::Connection,
            Self::InventoryResponse { .. } => Hook::InventoryResponse,
            Self::PlayerInventoryChanged { .. } => Hook::PlayerInventoryChanged,
            Self::CommandSubmitted { .. } => Hook::CommandSubmitted,
            Self::ConsoleCommand { .. } => Hook::ConsoleCommand,
            Self::Started { .. } => Hook::Started,
            Self::Tick { .. } => Hook::Tick,
            Self::Stopping => Hook::Stopping,
            Self::PlayerJoined { .. } => Hook::PlayerJoined,
            Self::PlayerLeft { .. } => Hook::PlayerLeft,
            Self::BlocksChanged { .. } => Hook::BlocksChanged,
            Self::FormSubmitted { .. } => Hook::FormSubmitted,
            Self::CustomFormSubmitted { .. } => Hook::CustomFormSubmitted,
            Self::PlayerMoved { .. } => Hook::PlayerMoved,
            Self::PlayerEquipped { .. } => Hook::PlayerEquipped,
            Self::PacketReceived { .. } => Hook::PacketReceived,
            Self::PlayerChat { .. } => Hook::PlayerChat,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Response {
    pub id: u64,
    pub message: PluginMessage,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PluginMessage {
    Ready {
        api_version: u32,
        metadata: Metadata,
    },
    Actions {
        actions: Vec<Action>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Action {
    World {
        request_id: u32,
        operation: WorldOperation,
    },
    Inventory {
        player_id: u64,
        request_id: u32,
        operation: InventoryOperation,
    },
    /// Register during Started. Command hooks determine the eligible sources.
    RegisterCommand {
        command: CommandSpec,
    },
    SetMotd {
        motd: String,
    },
    Log {
        message: String,
    },
    /// Plain system text to one initialized player; the owner applies a rate limit.
    SendMessage {
        player_id: u64,
        message: String,
    },
    ShowCustomForm {
        player_id: u64,
        request_id: u32,
        form: CustomForm,
    },
    ShowForm {
        player_id: u64,
        request_id: u32,
        form: SimpleForm,
    },
    /// One private, noncolliding static model preview, positioned by the owner.
    ShowModel {
        player_id: u64,
        model: String,
    },
    HideModel {
        player_id: u64,
    },
    /// Play a registered pack sound privately at the initialized player's eye.
    PlaySound {
        player_id: u64,
        sound: String,
        volume: f32,
        pitch: f32,
    },
}

impl Action {
    pub fn minimum_api(&self) -> u32 {
        match self {
            Self::World { .. } => WORLD_API_VERSION,
            Self::Inventory { .. } => INVENTORY_API_VERSION,
            Self::RegisterCommand { .. } => OBSERVATION_API_VERSION,
            Self::SetMotd { .. } | Self::Log { .. } => LEGACY_API_VERSION,
            Self::SendMessage { .. } => GAME_API_VERSION,
            Self::ShowForm { .. } => UI_API_VERSION,
            Self::ShowCustomForm { .. } => CUSTOM_FORM_API_VERSION,
            Self::PlaySound { .. } => SOUND_API_VERSION,
            Self::ShowModel { .. } | Self::HideModel { .. } => MODEL_API_VERSION,
        }
    }
}

pub trait Plugin {
    fn metadata(&self) -> Metadata;
    fn on_event(&mut self, event: Event) -> Vec<Action>;
}

/// Run a plugin on stdin/stdout until the server closes the pipe.
pub fn serve(plugin: impl Plugin) -> io::Result<()> {
    serve_io(plugin, io::stdin().lock(), io::stdout().lock())
}

fn serve_io(
    mut plugin: impl Plugin,
    mut input: impl BufRead,
    mut output: impl Write,
) -> io::Result<()> {
    let mut negotiated = None;
    while let Some(line) = read_line_bounded(&mut input)? {
        let request: Request = serde_json::from_slice(&line).map_err(io::Error::other)?;
        let message = match request.message {
            HostMessage::Hello {
                api_version,
                supported_versions,
                ..
            } if negotiated.is_none() => {
                let version = SUPPORTED_API_VERSIONS
                    .iter()
                    .copied()
                    .rev()
                    .find(|version| *version == api_version || supported_versions.contains(version))
                    .ok_or_else(|| io::Error::other("incompatible plugin API version"))?;
                let metadata = plugin.metadata();
                if metadata
                    .hooks
                    .iter()
                    .any(|hook| hook.minimum_api() > version)
                {
                    return Err(io::Error::other(
                        "host does not support the plugin's game hooks",
                    ));
                }
                negotiated = Some(version);
                PluginMessage::Ready {
                    api_version: version,
                    metadata,
                }
            }
            HostMessage::Event { event } if negotiated.is_some() => {
                let version = negotiated.expect("guarded above");
                if event.hook().minimum_api() > version {
                    return Err(io::Error::other("event exceeds negotiated API version"));
                }
                let actions = plugin.on_event(event);
                if actions.len() > MAX_ACTIONS {
                    return Err(io::Error::other("too many plugin actions"));
                }
                if actions.iter().any(|action| action.minimum_api() > version) {
                    return Err(io::Error::other("action exceeds negotiated API version"));
                }
                PluginMessage::Actions { actions }
            }
            _ => return Err(io::Error::other("invalid plugin handshake or API version")),
        };
        let response = serde_json::to_vec(&Response {
            id: request.id,
            message,
        })
        .map_err(io::Error::other)?;
        if response.len() + 1 > MAX_MESSAGE_BYTES {
            return Err(io::Error::other("plugin response too large"));
        }
        output.write_all(&response)?;
        output.write_all(b"\n")?;
        output.flush()?;
    }
    Ok(())
}

fn read_line_bounded(input: &mut impl BufRead) -> io::Result<Option<Vec<u8>>> {
    let mut line = Vec::new();
    loop {
        let available = input.fill_buf()?;
        if available.is_empty() {
            return if line.is_empty() {
                Ok(None)
            } else {
                Err(io::Error::other("unterminated plugin message"))
            };
        }
        let end = available.iter().position(|&b| b == b'\n').map(|i| i + 1);
        let count = end.unwrap_or(available.len());
        if line.len() + count > MAX_MESSAGE_BYTES {
            return Err(io::Error::other("plugin request too large"));
        }
        line.extend_from_slice(&available[..count]);
        input.consume(count);
        if end.is_some() {
            return Ok(Some(line));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestPlugin;

    #[test]
    fn inventory_capabilities_require_v7_instead_of_silently_loading_on_v6() {
        struct InventoryPlugin;
        impl Plugin for InventoryPlugin {
            fn metadata(&self) -> Metadata {
                Metadata {
                    name: "inventory".into(),
                    version: "1".into(),
                    hooks: vec![Hook::InventoryResponse],
                }
            }
            fn on_event(&mut self, _: Event) -> Vec<Action> {
                vec![]
            }
        }
        assert_eq!(Hook::InventoryResponse.minimum_api(), 7);
        assert_eq!(
            Action::Inventory {
                player_id: 1,
                request_id: 1,
                operation: InventoryOperation::Get
            }
            .minimum_api(),
            7
        );
        for highest in [6, 7] {
            let request = Request {
                id: 1,
                message: HostMessage::Hello {
                    api_version: 1,
                    supported_versions: (1..=highest).collect(),
                    server: ServerStatus::new("test".into(), 20).unwrap(),
                },
            };
            let mut input = serde_json::to_vec(&request).unwrap();
            input.push(b'\n');
            let mut output = vec![];
            let result = serve_io(InventoryPlugin, &input[..], &mut output);
            if highest == 6 {
                assert!(result.is_err() && output.is_empty());
            } else {
                result.unwrap();
                let response: Response = serde_json::from_slice(&output).unwrap();
                assert!(matches!(
                    response.message,
                    PluginMessage::Ready { api_version: 7, .. }
                ));
            }
        }
    }
    impl Plugin for TestPlugin {
        fn metadata(&self) -> Metadata {
            Metadata {
                name: "test".into(),
                version: "1".into(),
                hooks: vec![Hook::Started],
            }
        }
        fn on_event(&mut self, _: Event) -> Vec<Action> {
            vec![Action::SetMotd {
                motd: "Hello both editions".into(),
            }]
        }
    }

    #[test]
    fn plugin_handshake_and_event_keep_request_ids() {
        let requests = [
            Request {
                id: 7,
                message: HostMessage::Hello {
                    api_version: API_VERSION,
                    supported_versions: vec![LEGACY_API_VERSION, API_VERSION],
                    server: ServerStatus::new("test".into(), 20).unwrap(),
                },
            },
            Request {
                id: 8,
                message: HostMessage::Event {
                    event: Event::Tick { tick: 1 },
                },
            },
        ];
        let mut input = Vec::new();
        for request in requests {
            serde_json::to_writer(&mut input, &request).unwrap();
            input.push(b'\n');
        }
        let mut output = Vec::new();
        serve_io(TestPlugin, &input[..], &mut output).unwrap();
        let replies: Vec<Response> = output
            .split(|b| *b == b'\n')
            .filter(|line| !line.is_empty())
            .map(|line| serde_json::from_slice(line).unwrap())
            .collect();
        assert_eq!(replies[0].id, 7);
        assert!(matches!(
            replies[0].message,
            PluginMessage::Ready {
                api_version: API_VERSION,
                ..
            }
        ));
        assert_eq!(replies[1].id, 8);
        assert!(
            matches!(&replies[1].message, PluginMessage::Actions { actions } if actions.len() == 1)
        );
    }

    #[test]
    fn sdk_rejects_wrong_api_event_before_hello_and_oversize() {
        let mut output = Vec::new();
        for message in [
            HostMessage::Hello {
                api_version: API_VERSION + 1,
                supported_versions: vec![],
                server: ServerStatus::new("test".into(), 20).unwrap(),
            },
            HostMessage::Event {
                event: Event::Stopping,
            },
        ] {
            let mut input = serde_json::to_vec(&Request { id: 0, message }).unwrap();
            input.push(b'\n');
            assert!(serve_io(TestPlugin, &input[..], &mut output).is_err());
        }
        assert!(read_line_bounded(&mut &vec![b'x'; MAX_MESSAGE_BYTES + 1][..]).is_err());
    }

    #[test]
    fn negotiates_highest_common_version_and_retains_all_older_apis() {
        for (supported, expected) in [
            (vec![1, 2, 3, 4, 5, 6, 7, 8], 8),
            (vec![1, 2, 3, 4, 5, 6, 7], 7),
            (vec![1, 2, 3, 4, 5, 6], 6),
            (vec![1, 2, 3, 4, 5], 5),
            (vec![1, 2, 3, 4], 4),
            (vec![1, 2, 3], 3),
            (vec![1, 2], 2),
            (vec![], 1),
        ] {
            let mut input = serde_json::to_vec(&Request {
                id: 1,
                message: HostMessage::Hello {
                    api_version: 1,
                    supported_versions: supported,
                    server: ServerStatus::new("test".into(), 20).unwrap(),
                },
            })
            .unwrap();
            input.push(b'\n');
            let mut output = Vec::new();
            serve_io(TestPlugin, &input[..], &mut output).unwrap();
            let response: Response = serde_json::from_slice(&output).unwrap();
            assert!(
                matches!(response.message, PluginMessage::Ready { api_version, .. } if api_version == expected)
            );
        }
    }

    #[test]
    fn outbound_metadata_routes_only_to_the_new_hook() {
        let event = Event::Connection {
            event: ConnectionEvent::PacketSent {
                connection_id: 1,
                packet: ConnectionPacket {
                    protocol: Some(777),
                    phase: PacketPhase::Login,
                    id: 2,
                    name: "minecraft:login_finished".into(),
                    dispatch: PacketDispatch::Control,
                    sequence: 1,
                },
            },
        };
        assert_eq!(event.hook(), Hook::PacketSent);
        assert_eq!(event.hook().minimum_api(), 9);
        assert_eq!(Hook::Connection.minimum_api(), 8);
    }

    #[test]
    fn custom_input_hooks_negotiate_v11_without_changing_v3_forms() {
        struct Inputs;
        impl Plugin for Inputs {
            fn metadata(&self) -> Metadata {
                Metadata {
                    name: "inputs".into(),
                    version: "1".into(),
                    hooks: vec![Hook::CustomFormSubmitted],
                }
            }
            fn on_event(&mut self, _: Event) -> Vec<Action> {
                vec![]
            }
        }
        for highest in [10, 11] {
            let mut input = serde_json::to_vec(&Request {
                id: 1,
                message: HostMessage::Hello {
                    api_version: 1,
                    supported_versions: (1..=highest).collect(),
                    server: ServerStatus::new("test".into(), 20).unwrap(),
                },
            })
            .unwrap();
            input.push(b'\n');
            assert_eq!(
                serve_io(Inputs, &input[..], &mut Vec::new()).is_ok(),
                highest == 11
            );
        }
        assert_eq!(Hook::CustomFormSubmitted.minimum_api(), 11);
        assert_eq!(Hook::FormSubmitted.minimum_api(), 3);
        assert_eq!(
            Action::ShowCustomForm {
                player_id: 1,
                request_id: 0,
                form: CustomForm {
                    title: "Input".into(),
                    fields: vec![FormField::Toggle {
                        label: "Enabled".into(),
                        default: true
                    }]
                }
            }
            .minimum_api(),
            11
        );
        let invalid = serde_json::json!({"type":"show_custom_form","player_id":1,"request_id":0,"form":{"title":"Input","fields":[{"type":"toggle","label":"Enabled","default":true,"command":"injected"}]}});
        assert!(serde_json::from_value::<Action>(invalid).is_err());
    }

    #[test]
    fn world_hooks_require_v10_and_keep_legacy_block_changes_unchanged() {
        struct WorldPlugin;
        impl Plugin for WorldPlugin {
            fn metadata(&self) -> Metadata {
                Metadata {
                    name: "world".into(),
                    version: "1".into(),
                    hooks: vec![Hook::WorldResponse, Hook::WorldBlocksChanged],
                }
            }
            fn on_event(&mut self, _: Event) -> Vec<Action> {
                vec![]
            }
        }
        for version in [9, 10] {
            let mut input = serde_json::to_vec(&Request {
                id: 1,
                message: HostMessage::Hello {
                    api_version: 1,
                    supported_versions: (1..=version).collect(),
                    server: ServerStatus::new("test".into(), 20).unwrap(),
                },
            })
            .unwrap();
            input.push(b'\n');
            assert_eq!(
                serve_io(WorldPlugin, &input[..], &mut Vec::new()).is_ok(),
                version == 10
            );
        }
        assert_eq!(
            Action::World {
                request_id: 0,
                operation: WorldOperation::Read { positions: vec![] }
            }
            .minimum_api(),
            10
        );
        assert_eq!(Hook::WorldResponse.minimum_api(), 10);
        assert_eq!(Hook::WorldBlocksChanged.minimum_api(), 10);
        assert_eq!(Hook::BlocksChanged.minimum_api(), 2);
        let change = serde_json::to_value(BlockChange {
            x: 1,
            y: 4,
            z: 2,
            before: "minecraft:air".into(),
            after: "minecraft:stone".into(),
        })
        .unwrap();
        assert_eq!(change.as_object().unwrap().len(), 5);
        assert_eq!(change["x"], 1);
    }

    #[test]
    fn connection_hooks_negotiate_v8_and_v9_without_changing_player_hooks() {
        struct Observer(bool);
        impl Plugin for Observer {
            fn metadata(&self) -> Metadata {
                Metadata {
                    name: "connections".into(),
                    version: "1".into(),
                    hooks: if self.0 {
                        vec![Hook::Connection, Hook::PacketSent]
                    } else {
                        vec![Hook::Connection]
                    },
                }
            }
            fn on_event(&mut self, _: Event) -> Vec<Action> {
                vec![]
            }
        }
        for (version, outgoing) in [(7, false), (8, false), (8, true), (9, true)] {
            let mut input = serde_json::to_vec(&Request {
                id: 1,
                message: HostMessage::Hello {
                    api_version: 1,
                    supported_versions: (1..=version).collect(),
                    server: ServerStatus::new("test".into(), 20).unwrap(),
                },
            })
            .unwrap();
            input.push(b'\n');
            assert_eq!(
                serve_io(Observer(outgoing), &input[..], &mut Vec::new()).is_ok(),
                version >= if outgoing { 9 } else { 8 }
            );
        }
        assert_eq!(Hook::PacketReceived.minimum_api(), 6);
        assert_eq!(
            Event::Connection {
                event: ConnectionEvent::Closed { connection_id: 1 }
            }
            .hook(),
            Hook::Connection
        );
    }

    #[test]
    fn observation_hooks_require_v6_and_receipts_contain_only_bounded_metadata() {
        for hook in [
            Hook::PacketReceived,
            Hook::PlayerMoved,
            Hook::PlayerEquipped,
        ] {
            assert_eq!(hook.minimum_api(), 6);
        }
        let packet = PacketInfo {
            edition: Edition::Java,
            protocol: 777,
            phase: "play".into(),
            id: 22,
            name: "minecraft:custom_payload".into(),
            dispatch: PacketDispatch::Unsupported,
            sequence: 1,
        };
        packet.validate().unwrap();
        let value = serde_json::to_value(Event::PacketReceived {
            player_id: 1,
            packet: packet.clone(),
        })
        .unwrap();
        let fields = value["packet"].as_object().unwrap();
        assert_eq!(fields.len(), 7);
        assert!(fields.keys().all(|key| {
            [
                "edition", "protocol", "phase", "id", "name", "dispatch", "sequence",
            ]
            .contains(&key.as_str())
        }));
        let mut invalid = packet;
        invalid.name = "token\nvalue".into();
        assert!(invalid.validate().is_err());
        invalid.name = "unknown".into();
        invalid.sequence = 0;
        assert!(invalid.validate().is_err());
    }

    struct GamePlugin;
    impl Plugin for GamePlugin {
        fn metadata(&self) -> Metadata {
            Metadata {
                name: "game".into(),
                version: "1".into(),
                hooks: vec![Hook::PlayerJoined],
            }
        }
        fn on_event(&mut self, _: Event) -> Vec<Action> {
            vec![]
        }
    }

    #[test]
    fn legacy_host_cannot_silently_accept_a_game_hook_plugin() {
        let mut input = serde_json::to_vec(&Request {
            id: 1,
            message: HostMessage::Hello {
                api_version: 1,
                supported_versions: vec![],
                server: ServerStatus::new("test".into(), 20).unwrap(),
            },
        })
        .unwrap();
        input.push(b'\n');
        assert!(serve_io(GamePlugin, &input[..], &mut Vec::new()).is_err());
    }

    #[test]
    fn v2_host_accepts_legacy_game_hooks_but_rejects_ui_hooks() {
        struct FormPlugin;
        impl Plugin for FormPlugin {
            fn metadata(&self) -> Metadata {
                Metadata {
                    name: "forms".into(),
                    version: "1".into(),
                    hooks: vec![Hook::FormSubmitted],
                }
            }
            fn on_event(&mut self, _: Event) -> Vec<Action> {
                vec![]
            }
        }
        let mut input = serde_json::to_vec(&Request {
            id: 1,
            message: HostMessage::Hello {
                api_version: 1,
                supported_versions: vec![1, 2],
                server: ServerStatus::new("test".into(), 20).unwrap(),
            },
        })
        .unwrap();
        input.push(b'\n');
        let mut output = Vec::new();
        serve_io(GamePlugin, &input[..], &mut output).unwrap();
        assert!(matches!(
            serde_json::from_slice::<Response>(&output).unwrap().message,
            PluginMessage::Ready { api_version: 2, .. }
        ));
        assert!(serve_io(FormPlugin, &input[..], &mut Vec::new()).is_err());
        assert_eq!(Hook::PlayerJoined.minimum_api(), 2);
        assert_eq!(Hook::FormSubmitted.minimum_api(), 3);
    }
    #[test]
    fn console_hook_is_versioned_separately_from_player_commands() {
        struct Console;
        impl Plugin for Console {
            fn metadata(&self) -> Metadata {
                Metadata {
                    name: "console".into(),
                    version: "1".into(),
                    hooks: vec![Hook::ConsoleCommand],
                }
            }
            fn on_event(&mut self, _: Event) -> Vec<Action> {
                vec![]
            }
        }
        for highest in [11, 12] {
            let request = Request {
                id: 1,
                message: HostMessage::Hello {
                    api_version: 1,
                    supported_versions: (1..=highest).collect(),
                    server: ServerStatus::new("test".into(), 20).unwrap(),
                },
            };
            let mut input = serde_json::to_vec(&request).unwrap();
            input.push(b'\n');
            let mut output = Vec::new();
            let result = serve_io(Console, &input[..], &mut output);
            if highest == 11 {
                assert!(result.is_err() && output.is_empty());
            } else {
                result.unwrap();
                let response: Response = serde_json::from_slice(&output).unwrap();
                assert!(matches!(
                    response.message,
                    PluginMessage::Ready {
                        api_version: 12,
                        ..
                    }
                ));
            }
        }
        let event = Event::ConsoleCommand {
            command: "admin".into(),
            arguments: "private".into(),
        };
        assert_eq!(event.hook().minimum_api(), 12);
        assert_eq!(
            serde_json::to_value(event).unwrap(),
            serde_json::json!({"type":"console_command","command":"admin","arguments":"private"})
        );
        let old = serde_json::json!({"type":"command_submitted","player_id":7,"name":"player","command":"old","arguments":"private"});
        let decoded: Event = serde_json::from_value(old.clone()).unwrap();
        assert_eq!(serde_json::to_value(decoded).unwrap(), old);
    }
}
