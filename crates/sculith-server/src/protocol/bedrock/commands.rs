//! V2225 command projection, independently decoded by pinned gophertunnel.
use anyhow::{Result, ensure};
use bedrock::protocol::{
    ProtoVersionPackets, V2225,
    v898::packets::{
        AvailableCommandsPacket, CommandPermissionLevelString, CommandsEntry, OverloadsEntry,
        ParameterDataEntry,
    },
};
use sculith_core::CommandSpec;

pub fn available(specs: &[CommandSpec]) -> V2225 {
    V2225::AvailableCommandsPacket(Box::new(AvailableCommandsPacket {
        enum_values: vec![],
        sub_command_values: vec![],
        post_fixes: vec![],
        enum_data: vec![],
        chained_sub_command_data: vec![],
        soft_enums: vec![],
        constraints: vec![],
        commands: specs
            .iter()
            .map(|spec| CommandsEntry {
                name: spec.name.clone(),
                description: spec.description.clone(),
                flags: 0,
                permission_level: CommandPermissionLevelString::Any,
                alias_enum: -1,
                chained_sub_command_indices: vec![],
                overloads: vec![OverloadsEntry {
                    is_chaining: false,
                    parameter_data: vec![ParameterDataEntry {
                        name: "arguments".into(),
                        parse_symbol: 0x100000 | 70,
                        is_optional: true,
                        options: 0, // Valid | RawText, gophertunnel e2251a6.
                    }],
                }],
            })
            .collect(),
    }))
}

pub fn input(
    request: &<V2225 as ProtoVersionPackets>::CommandRequestPacket,
    id: u64,
) -> Result<String> {
    ensure!(
        matches!(
            request.command_origin.command_type,
            bedrock::protocol::v898::enums::CommandOriginType::Player
        ) && !request.internal,
        "only player commands are accepted"
    );
    ensure!(
        request.command_origin.player_id == 0
            || u64::try_from(request.command_origin.player_id).ok() == Some(id),
        "command actor mismatch"
    );
    ensure!(
        request.command_origin.request_id.len() <= 64 && request.version.len() <= 32,
        "command metadata too long"
    );
    // The UUID, request ID and client actor never replace the live owner lease.
    Ok(request.command.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use bedrock::protocol::v898::{
        enums::CommandOriginType, packets::CommandRequestPacket, types::CommandOriginData,
    };
    #[test]
    fn player_lease_cannot_be_replaced_with_a_console_origin_or_actor_claim() {
        let mut request = CommandRequestPacket::<V2225> {
            command: "/hello secret".into(),
            internal: false,
            version: String::new(),
            command_origin: CommandOriginData {
                command_type: CommandOriginType::Player,
                command_uuid: uuid::Uuid::new_v4(),
                request_id: String::new(),
                player_id: 0,
            },
        };
        assert_eq!(input(&request, 1).unwrap(), "/hello secret");
        request.command_origin.player_id = 2;
        assert!(input(&request, 1).is_err());
        request.command_origin.player_id = 1;
        assert!(input(&request, 1).is_ok());
        request.command_origin.command_type = CommandOriginType::DedicatedServer;
        assert!(input(&request, 1).is_err());
        request.command_origin.command_type = CommandOriginType::Player;
        request.internal = true;
        assert!(input(&request, 1).is_err());
        request.internal = false;
        request.command_origin.request_id = "x".repeat(65);
        assert!(input(&request, 1).is_err());
    }
}
