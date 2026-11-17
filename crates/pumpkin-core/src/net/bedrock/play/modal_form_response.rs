#[allow(clippy::wildcard_imports)]
use super::*;

impl BedrockClient {
    pub fn handle_modal_form_response(
        &self,
        player: &Arc<Player>,
        server: &Arc<Server>,
        packet: pumpkin_protocol::bedrock::server::modal_form_response::SModalFormResponse<'_>,
    ) {
        match player.bedrock_form_response(packet.form_id.0, packet.json_response.as_deref()) {
            crate::entity::player::forms::FormReply::Valid { id, data } => {
                player.fire_form_response(server, id, data);
                return;
            }
            crate::entity::player::forms::FormReply::Invalid => return,
            crate::entity::player::forms::FormReply::Unhandled => {}
        }
        let mut event =
            crate::plugin::api::events::player::bedrock_form_response::BedrockFormResponseEvent::new(
                player.clone(),
                packet.form_id.0,
                packet
                    .json_response
                    .filter(|data| data != "null")
                    .map(std::borrow::Cow::into_owned),
            );
        server.plugin_manager.fire_blocking(server, &mut event);
    }
}
