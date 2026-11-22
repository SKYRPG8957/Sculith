use std::collections::VecDeque;
use std::sync::{Arc, Mutex, PoisonError, Weak};

use pumpkin_util::text::TextComponent;

use crate::command::{commands::is_world_read, dispatcher::CommandDispatcher};
use crate::entity::player::{Player, SpamType};
use crate::plugin::{
    player::{
        player_command_send::PlayerCommandSendEvent,
        player_permission_check::PlayerPermissionCheckEvent,
    },
    server::packet::PacketSentEvent,
};
use crate::world::World;

use super::Server;

const QUEUE_CAPACITY: usize = 256;
const BATCH_SIZE: usize = 32;

struct PlayerCommand {
    player: Weak<Player>,
    world: Weak<World>,
    text: String,
}

#[derive(Default)]
pub struct PlayerCommands {
    pending: Mutex<VecDeque<PlayerCommand>>,
}

#[derive(Clone, Copy)]
pub struct PlayerCommandIngress<'a> {
    pub(crate) queue: &'a PlayerCommands,
    pub(crate) read_owner: Option<&'a Arc<World>>,
}

impl PlayerCommands {
    fn submit(
        &self,
        server: &Arc<Server>,
        player: &Arc<Player>,
        text: &str,
        read_owner: Option<&Arc<World>>,
    ) {
        if server.stop_token.is_cancelled() || !accept_command(server, player) {
            return;
        }

        let mut pending = self.pending.lock().unwrap_or_else(PoisonError::into_inner);
        // Earlier coordinated commands keep their order. A later one must wait for this owner.
        if let Some(world) = read_owner
            && pending.is_empty()
            && let Some(dispatcher) = world_read_dispatcher(server, text)
        {
            drop(pending);
            if is_current(player, world) {
                let _runtime = server.runtime.enter();
                dispatch(server, player, &dispatcher, text, Some(world));
            }
            return;
        }
        if pending.len() == QUEUE_CAPACITY {
            drop(pending);
            player.send_system_message(&TextComponent::text("Server command queue is full"));
            return;
        }
        pending.push_back(PlayerCommand {
            player: Arc::downgrade(player),
            world: Arc::downgrade(&player.world()),
            text: text.to_owned(),
        });
    }

    pub(super) fn has_pending(&self) -> bool {
        !self
            .pending
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .is_empty()
    }

    pub(super) fn run_all(&self, server: &Arc<Server>) {
        while !server.stop_token.is_cancelled() && self.has_pending() {
            self.run_pending(server);
        }
    }

    /// Runs at a coordinated boundary because commands and their hooks can touch any world.
    pub(super) fn run_pending(&self, server: &Arc<Server>) {
        let batch: Vec<_> = {
            let mut pending = self.pending.lock().unwrap_or_else(PoisonError::into_inner);
            let count = pending.len().min(BATCH_SIZE);
            pending.drain(..count).collect()
        };
        for command in batch {
            if server.stop_token.is_cancelled() {
                break;
            }
            let (Some(player), Some(world)) = (command.player.upgrade(), command.world.upgrade())
            else {
                continue;
            };
            if !is_current(&player, &world) {
                continue;
            }

            let mut event = PlayerCommandSendEvent::new(player.clone(), command.text);
            server.plugin_manager.fire_blocking(server, &mut event);
            if !event.cancelled && is_current(&player, &world) {
                let _runtime = server.runtime.enter();
                dispatch(
                    server,
                    &player,
                    &server.command_dispatcher.load(),
                    &event.command,
                    None,
                );
            }
        }
    }
}

fn world_read_dispatcher(server: &Server, text: &str) -> Option<Arc<CommandDispatcher>> {
    let dispatcher = server.command_dispatcher.load_full();
    let plugins = &server.plugin_manager;
    (is_world_read(&dispatcher, text)
        && !plugins.has_handlers::<PlayerCommandSendEvent>()
        && !plugins.has_handlers::<PlayerPermissionCheckEvent>()
        && !plugins.has_handlers::<PacketSentEvent>())
    .then_some(dispatcher)
}

fn is_current(player: &Arc<Player>, world: &World) -> bool {
    !player.client.closed()
        && world.is_open()
        && player.can_tick_in(world)
        && world
            .get_player_by_uuid(player.gameprofile.id)
            .is_some_and(|current| Arc::ptr_eq(&current, player))
}

fn accept_command(server: &Server, player: &Player) -> bool {
    player.update_last_action_time();
    !player.check_chat_spam(server, SpamType::Command)
}

pub fn submit_player_command(
    server: &Arc<Server>,
    player: &Arc<Player>,
    text: &str,
    commands: Option<&PlayerCommands>,
) {
    if let Some(commands) = commands {
        commands.submit(server, player, text, None);
    } else {
        let owner = server.clone();
        let player = player.clone();
        let text = text.to_owned();
        server.spawn_task(async move { execute_player_command(&owner, &player, &text).await });
    }
}

pub fn submit_inbound_player_command(
    server: &Arc<Server>,
    player: &Arc<Player>,
    text: &str,
    ingress: Option<PlayerCommandIngress<'_>>,
) {
    if let Some(ingress) = ingress {
        ingress
            .queue
            .submit(server, player, text, ingress.read_owner);
    } else {
        submit_player_command(server, player, text, None);
    }
}

pub async fn execute_player_command(server: &Arc<Server>, player: &Arc<Player>, text: &str) {
    if accept_command(server, player) {
        let mut event = PlayerCommandSendEvent::new(player.clone(), text.to_owned());
        server.plugin_manager.fire(server, &mut event).await;
        if !event.cancelled {
            dispatch(
                server,
                player,
                &server.command_dispatcher.load(),
                &event.command,
                None,
            );
        }
    }
}

fn dispatch(
    server: &Arc<Server>,
    player: &Arc<Player>,
    dispatcher: &CommandDispatcher,
    text: &str,
    read_owner: Option<&Arc<World>>,
) {
    let source = player.get_command_source(server);
    if read_owner.is_some_and(|world| !Arc::ptr_eq(world, source.world())) {
        return;
    }
    dispatcher.handle_command(&source, text);

    if server.advanced_config.commands.log_console {
        tracing::info!(
            "Player ({}): executed command /{text}",
            player.gameprofile.name
        );
    }
}
