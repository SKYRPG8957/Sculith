use wasmtime::component::{Access, HasSelf, Resource};

use crate::{
    commands::executor::{WasmCommandExecutor, WasmCommandSuggestionProvider},
    pumpkin::{
        self,
        plugin::{
            command::{
                Arg, ArgumentType, Command, CommandNode, CommandSender, CommandSenderType,
                ConsumedArgs, PermissionLevel, StringType,
            },
            common::{BlockPos as WitBlockPos, Locale, Position},
            player::Player,
            server::Server,
            text::TextComponent,
            world::World,
        },
    },
};
use pumpkin_wasm_host_common::state::{PluginHostState, WasmCommand, WasmCommandNode};
use sculith_core::command::{
    argument_builder::{argument, literal},
    argument_types::{
        block::BlockArgumentType,
        block_predicate::BlockPredicateArgumentType,
        component::ComponentArgumentType,
        coordinates::{
            block_pos::BlockPosArgumentType, rotation::RotationArgumentType,
            vec2::Vec2ArgumentType, vec3::Vec3ArgumentType,
        },
        core::{
            bool::BoolArgumentType, double::DoubleArgumentType, float::FloatArgumentType,
            integer::IntegerArgumentType, long::LongArgumentType, string::StringArgumentType,
        },
        entity::EntityArgumentType,
        entity_anchor::EntityAnchorArgumentType,
        game_profile::GameProfileArgumentType,
        gamemode::GameModeArgumentType,
        identifier::IdentifierArgumentType,
        item::ItemStackArgumentType,
        item_predicate::ItemPredicateArgumentType,
        time::TimeArgumentType,
    },
};

pub mod executor;

impl pumpkin::plugin::command::Host for PluginHostState {}

impl pumpkin::plugin::command::HostConsumedArgs for PluginHostState {
    #[expect(clippy::too_many_lines)]
    fn get_value(
        &mut self,
        consumed_args: Resource<ConsumedArgs>,
        key: String,
    ) -> wasmtime::Result<Arg> {
        use pumpkin_wasm_host_common::args::OwnedArg;

        let resource = self.get(&consumed_args)?;

        let Some(owned_arg) = resource.get(&key).cloned() else {
            return Ok(Arg::Simple(String::new()));
        };

        Ok(match owned_arg {
            OwnedArg::Simple(s) => Arg::Simple(s),
            OwnedArg::Msg(s) => Arg::Msg(s),
            OwnedArg::Bool(b) => Arg::Bool(b),
            OwnedArg::Item(s) => Arg::Item(s),
            OwnedArg::ItemPredicate(s) => Arg::ItemPredicate(s),
            OwnedArg::ResourceLocation(s) => Arg::ResourceLocation(s),
            OwnedArg::Block(s) => Arg::Block(s),
            OwnedArg::BlockPredicate(s) => Arg::BlockPredicate(s),
            OwnedArg::Time(t) => Arg::Time(t),
            OwnedArg::Num(n) => {
                use pumpkin_wasm_host_common::args::{NotInBounds, Number};
                let convert_num = |n: Number| match n {
                    Number::F64(v) => pumpkin::plugin::command::Number::Float64(v),
                    Number::F32(v) => pumpkin::plugin::command::Number::Float32(v),
                    Number::I32(v) => pumpkin::plugin::command::Number::Int32(v),
                    Number::I64(v) => pumpkin::plugin::command::Number::Int64(v),
                };
                Arg::Num(n.map(convert_num).map_err(|e| match e {
                    NotInBounds::LowerBound(a, b) => {
                        pumpkin::plugin::command::NotInBounds::LowerBound((
                            convert_num(a),
                            convert_num(b),
                        ))
                    }
                    NotInBounds::UpperBound(a, b) => {
                        pumpkin::plugin::command::NotInBounds::UpperBound((
                            convert_num(a),
                            convert_num(b),
                        ))
                    }
                }))
            }
            OwnedArg::BlockPos(p) => Arg::BlockPos(WitBlockPos {
                x: p.0.x,
                y: p.0.y,
                z: p.0.z,
            }),
            OwnedArg::Pos3D(v) => Arg::Pos3d((v.x, v.y, v.z)),
            OwnedArg::Pos2D(v) => Arg::Pos2d((v.x, v.y)),
            OwnedArg::Rotation(a, b, c, d) => Arg::Rotation((a, b, c, d)),
            OwnedArg::GameMode(g) => Arg::Gamemode(match g {
                sculith_util::GameMode::Survival => pumpkin::plugin::common::GameMode::Survival,
                sculith_util::GameMode::Creative => pumpkin::plugin::common::GameMode::Creative,
                sculith_util::GameMode::Adventure => pumpkin::plugin::common::GameMode::Adventure,
                sculith_util::GameMode::Spectator => pumpkin::plugin::common::GameMode::Spectator,
            }),
            OwnedArg::Difficulty(d) => Arg::Difficulty(match d {
                sculith_util::Difficulty::Peaceful => pumpkin::plugin::server::Difficulty::Peaceful,
                sculith_util::Difficulty::Easy => pumpkin::plugin::server::Difficulty::Easy,
                sculith_util::Difficulty::Normal => pumpkin::plugin::server::Difficulty::Normal,
                sculith_util::Difficulty::Hard => pumpkin::plugin::server::Difficulty::Hard,
            }),
            OwnedArg::Players(players) => {
                let mut resources = Vec::new();
                for p in players {
                    if let Ok(r) = self.add(p) {
                        resources.push(r);
                    }
                }
                Arg::Players(resources)
            }
            OwnedArg::Particle(p) => Arg::Particle(format!("{p:?}")),
            OwnedArg::TextComponent(t) => Arg::TextComponent(self.add(t)?),
            OwnedArg::BossbarColor(c) => Arg::BossbarColor(match c {
                sculith_core::world::bossbar::BossbarColor::Pink => {
                    pumpkin::plugin::command::BossbarColor::Pink
                }
                sculith_core::world::bossbar::BossbarColor::Blue => {
                    pumpkin::plugin::command::BossbarColor::Blue
                }
                sculith_core::world::bossbar::BossbarColor::Red => {
                    pumpkin::plugin::command::BossbarColor::Red
                }
                sculith_core::world::bossbar::BossbarColor::Green => {
                    pumpkin::plugin::command::BossbarColor::Green
                }
                sculith_core::world::bossbar::BossbarColor::Yellow => {
                    pumpkin::plugin::command::BossbarColor::Yellow
                }
                sculith_core::world::bossbar::BossbarColor::Purple => {
                    pumpkin::plugin::command::BossbarColor::Purple
                }
                sculith_core::world::bossbar::BossbarColor::White => {
                    pumpkin::plugin::command::BossbarColor::White
                }
            }),
            OwnedArg::BossbarStyle(s) => Arg::BossbarStyle(match s {
                sculith_core::world::bossbar::BossbarDivisions::NoDivision => {
                    pumpkin::plugin::command::BossbarStyle::NoDivision
                }
                sculith_core::world::bossbar::BossbarDivisions::Notches6 => {
                    pumpkin::plugin::command::BossbarStyle::Notches6
                }
                sculith_core::world::bossbar::BossbarDivisions::Notches10 => {
                    pumpkin::plugin::command::BossbarStyle::Notches10
                }
                sculith_core::world::bossbar::BossbarDivisions::Notches12 => {
                    pumpkin::plugin::command::BossbarStyle::Notches12
                }
                sculith_core::world::bossbar::BossbarDivisions::Notches20 => {
                    pumpkin::plugin::command::BossbarStyle::Notches20
                }
            }),
            OwnedArg::SoundCategory(s) => Arg::SoundCategory(match s {
                pumpkin_data::sound::SoundCategory::Master
                | pumpkin_data::sound::SoundCategory::Ui => {
                    pumpkin::plugin::command::SoundCategory::Master
                }
                pumpkin_data::sound::SoundCategory::Music => {
                    pumpkin::plugin::command::SoundCategory::Music
                }
                pumpkin_data::sound::SoundCategory::Records => {
                    pumpkin::plugin::command::SoundCategory::Records
                }
                pumpkin_data::sound::SoundCategory::Weather => {
                    pumpkin::plugin::command::SoundCategory::Weather
                }
                pumpkin_data::sound::SoundCategory::Blocks => {
                    pumpkin::plugin::command::SoundCategory::Blocks
                }
                pumpkin_data::sound::SoundCategory::Hostile => {
                    pumpkin::plugin::command::SoundCategory::Hostile
                }
                pumpkin_data::sound::SoundCategory::Neutral => {
                    pumpkin::plugin::command::SoundCategory::Neutral
                }
                pumpkin_data::sound::SoundCategory::Players => {
                    pumpkin::plugin::command::SoundCategory::Players
                }
                pumpkin_data::sound::SoundCategory::Ambient => {
                    pumpkin::plugin::command::SoundCategory::Ambient
                }
                pumpkin_data::sound::SoundCategory::Voice => {
                    pumpkin::plugin::command::SoundCategory::Voice
                }
            }),
            OwnedArg::DamageType(d) => Arg::DamageType(d.message_id.to_string()),
            OwnedArg::Effect(e) => Arg::Effect(e.minecraft_name.to_string()),
            OwnedArg::Enchantment(e) => Arg::Enchantment(e.name.to_string()),
            OwnedArg::Advancement(a) => Arg::Advancement(a.to_string()),
            OwnedArg::EntityAnchor(a) => Arg::EntityAnchor(match a {
                sculith_core::command::argument_types::entity_anchor::EntityAnchor::Eyes => {
                    pumpkin::plugin::command::EntityAnchor::Eyes
                }
                sculith_core::command::argument_types::entity_anchor::EntityAnchor::Feet => {
                    pumpkin::plugin::command::EntityAnchor::Feet
                }
            }),
            // These types don't have direct WIT resource mappings yet
            OwnedArg::Entities(_) | OwnedArg::Entity(_) | OwnedArg::GameProfiles(_) => {
                Arg::Simple(String::new())
            }
        })
    }

    fn drop(&mut self, rep: Resource<ConsumedArgs>) -> wasmtime::Result<()> {
        self.drop(rep)
    }
}

impl pumpkin::plugin::command::HostCommand for PluginHostState {
    fn new(
        &mut self,
        names: Vec<String>,
        description: String,
    ) -> wasmtime::Result<Resource<Command>> {
        self.add(WasmCommand::new(names, description))
            .map_err(|_| wasmtime::Error::msg("Failed to add command resource"))
    }

    fn then(
        &mut self,
        command: Resource<Command>,
        node: Resource<CommandNode>,
    ) -> wasmtime::Result<()> {
        let node_data = self.take(node)?;
        let command_res = self.get_mut(&command)?;
        let cmd = std::mem::replace(command_res, WasmCommand::new(Vec::new(), String::new()));
        *command_res = cmd.then(node_data);
        Ok(())
    }

    fn execute_with_handler_id(
        &mut self,
        command: Resource<Command>,
        handler_id: u32,
    ) -> wasmtime::Result<()> {
        let plugin = self
            .plugin
            .as_ref()
            .and_then(std::sync::Weak::upgrade)
            .ok_or_else(|| wasmtime::Error::msg("Plugin dropped"))?;
        let server = self
            .server
            .clone()
            .ok_or_else(|| wasmtime::Error::msg("Server not initialized"))?;

        let executor = WasmCommandExecutor {
            handler_id,
            plugin,
            server,
        };
        let command_res = self.get_mut(&command)?;
        let cmd = std::mem::replace(command_res, WasmCommand::new(Vec::new(), String::new()));
        *command_res = cmd.executes(executor);
        Ok(())
    }

    fn drop(&mut self, rep: Resource<Command>) -> wasmtime::Result<()> {
        self.drop(rep)
    }
}

impl pumpkin::plugin::command::HostCommandSender for PluginHostState {
    fn get_command_sender_type(
        &mut self,
        res: Resource<CommandSender>,
    ) -> wasmtime::Result<CommandSenderType> {
        let sender = self.get(&res)?.clone();
        match sender {
            sculith_core::command::CommandSender::Console => Ok(CommandSenderType::Console),
            sculith_core::command::CommandSender::Player(player) => {
                Ok(CommandSenderType::Player(self.add(player)?))
            }
            sculith_core::command::CommandSender::CommandBlock(block_entity, world) => Ok(
                CommandSenderType::CommandBlock((self.add(block_entity)?, self.add(world)?)),
            ),
            sculith_core::command::CommandSender::Dummy => Ok(CommandSenderType::Dummy),
        }
    }

    fn get_name(&mut self, sender: Resource<CommandSender>) -> wasmtime::Result<String> {
        Ok(self.get(&sender)?.to_string())
    }

    fn send_message(
        &mut self,
        sender: Resource<CommandSender>,
        text: Resource<TextComponent>,
    ) -> wasmtime::Result<()> {
        let component = self.take(text)?;
        self.get(&sender)?.send_message(component);
        Ok(())
    }

    fn send_system_message(
        &mut self,
        sender: Resource<CommandSender>,
        text: Resource<TextComponent>,
    ) -> wasmtime::Result<()> {
        let component = self.take(text)?;
        self.get(&sender)?.send_message(component);
        Ok(())
    }

    fn send_error(
        &mut self,
        sender: Resource<CommandSender>,
        text: Resource<TextComponent>,
    ) -> wasmtime::Result<()> {
        let component = self.take(text)?;
        self.get(&sender)?
            .send_message(component.color(sculith_util::text::color::Color::Named(
                sculith_util::text::color::NamedColor::Red,
            )));
        Ok(())
    }

    fn set_success_count(
        &mut self,
        sender: Resource<CommandSender>,
        count: i32,
    ) -> wasmtime::Result<()> {
        self.get_mut(&sender)?.set_success_count(count as u32);
        Ok(())
    }

    fn is_player(&mut self, sender: Resource<CommandSender>) -> wasmtime::Result<bool> {
        Ok(matches!(
            self.get(&sender)?,
            sculith_core::command::CommandSender::Player(_)
        ))
    }

    fn is_console(&mut self, sender: Resource<CommandSender>) -> wasmtime::Result<bool> {
        Ok(matches!(
            self.get(&sender)?,
            sculith_core::command::CommandSender::Console
        ))
    }

    fn as_player(
        &mut self,
        sender: Resource<CommandSender>,
    ) -> wasmtime::Result<Option<Resource<Player>>> {
        if let sculith_core::command::CommandSender::Player(player) = &self.get(&sender)? {
            Ok(Some(self.add(player.clone()).map_err(|_| {
                wasmtime::Error::msg("Failed to add player resource")
            })?))
        } else {
            Ok(None)
        }
    }

    fn permission_level(
        &mut self,
        sender: Resource<CommandSender>,
    ) -> wasmtime::Result<PermissionLevel> {
        Ok(match self.get(&sender)?.permission_lvl() {
            sculith_util::PermissionLvl::Zero => PermissionLevel::Zero,
            sculith_util::PermissionLvl::One => PermissionLevel::One,
            sculith_util::PermissionLvl::Two => PermissionLevel::Two,
            sculith_util::PermissionLvl::Three => PermissionLevel::Three,
            sculith_util::PermissionLvl::Four => PermissionLevel::Four,
        })
    }

    fn has_permission_level(
        &mut self,
        sender: Resource<CommandSender>,
        level: PermissionLevel,
    ) -> wasmtime::Result<bool> {
        let required = match level {
            PermissionLevel::Zero => sculith_util::PermissionLvl::Zero,
            PermissionLevel::One => sculith_util::PermissionLvl::One,
            PermissionLevel::Two => sculith_util::PermissionLvl::Two,
            PermissionLevel::Three => sculith_util::PermissionLvl::Three,
            PermissionLevel::Four => sculith_util::PermissionLvl::Four,
        };
        Ok(self.get(&sender)?.permission_lvl() >= required)
    }

    fn position(&mut self, sender: Resource<CommandSender>) -> wasmtime::Result<Option<Position>> {
        Ok(self.get(&sender)?.position().map(|p| (p.x, p.y, p.z)))
    }

    fn world(
        &mut self,
        sender: Resource<CommandSender>,
    ) -> wasmtime::Result<Option<Resource<World>>> {
        if let Some(world) = self.get(&sender)?.world() {
            Ok(Some(self.add(world).map_err(|_| {
                wasmtime::Error::msg("Failed to add world resource")
            })?))
        } else {
            Ok(None)
        }
    }

    fn get_locale(&mut self, sender: Resource<CommandSender>) -> wasmtime::Result<Locale> {
        Ok(map_util_locale_to_wit(self.get(&sender)?.get_locale()))
    }

    fn should_receive_feedback(
        &mut self,
        sender: Resource<CommandSender>,
    ) -> wasmtime::Result<bool> {
        Ok(self.get(&sender)?.should_receive_feedback())
    }

    fn should_broadcast_console_to_ops(
        &mut self,
        sender: Resource<CommandSender>,
    ) -> wasmtime::Result<bool> {
        Ok(self.get(&sender)?.should_broadcast_console_to_ops())
    }

    fn should_track_output(&mut self, sender: Resource<CommandSender>) -> wasmtime::Result<bool> {
        Ok(self.get(&sender)?.should_track_output())
    }

    fn drop(&mut self, rep: Resource<CommandSender>) -> wasmtime::Result<()> {
        self.drop(rep)
    }
}

impl pumpkin::plugin::command::HostCommandSenderWithStore<PluginHostState>
    for HasSelf<PluginHostState>
{
    async fn has_permission(
        mut host: Access<'_, PluginHostState, Self>,
        sender: Resource<CommandSender>,
        server: /* borrow */ Resource<Server>,
        node: String,
    ) -> wasmtime::Result<bool> {
        let (sender, server, plugin) = {
            let state = host.get();
            let sender = state.get(&sender)?.clone();
            let server = state.get(&server)?.clone();
            let plugin = state
                .plugin
                .as_ref()
                .and_then(std::sync::Weak::upgrade)
                .ok_or_else(|| wasmtime::Error::msg("Plugin instance not available"))?;
            (sender, server, plugin)
        };

        plugin
            .store
            .pump_blocking(&mut host, move || sender.has_permission(&server, &node))
            .await
    }
}

impl pumpkin::plugin::command::HostCommandNode for PluginHostState {
    fn literal(&mut self, name: String) -> wasmtime::Result<Resource<CommandNode>> {
        self.add(WasmCommandNode::Literal(literal(name)))
            .map_err(|_| wasmtime::Error::msg("Failed to add literal node"))
    }

    fn argument(
        &mut self,
        name: String,
        arg_type: ArgumentType,
    ) -> wasmtime::Result<Resource<CommandNode>> {
        let node = match arg_type {
            ArgumentType::Bool => WasmCommandNode::Argument(argument(name, BoolArgumentType)),
            ArgumentType::Float((min, max)) => WasmCommandNode::Argument(argument(
                name,
                FloatArgumentType::new(min.unwrap_or(f32::MIN), max.unwrap_or(f32::MAX)),
            )),
            ArgumentType::Double((min, max)) => WasmCommandNode::Argument(argument(
                name,
                DoubleArgumentType::new(min.unwrap_or(f64::MIN), max.unwrap_or(f64::MAX)),
            )),
            ArgumentType::Integer((min, max)) => WasmCommandNode::Argument(argument(
                name,
                IntegerArgumentType::new(min.unwrap_or(i32::MIN), max.unwrap_or(i32::MAX)),
            )),
            ArgumentType::Long((min, max)) => WasmCommandNode::Argument(argument(
                name,
                LongArgumentType::new(min.unwrap_or(i64::MIN), max.unwrap_or(i64::MAX)),
            )),
            ArgumentType::String(st) => match st {
                StringType::SingleWord => {
                    WasmCommandNode::Argument(argument(name, StringArgumentType::SingleWord))
                }
                StringType::Quotable => {
                    WasmCommandNode::Argument(argument(name, StringArgumentType::QuotablePhrase))
                }
                StringType::Greedy => {
                    WasmCommandNode::Argument(argument(name, StringArgumentType::GreedyPhrase))
                }
            },
            ArgumentType::Entities => {
                WasmCommandNode::Argument(argument(name, EntityArgumentType::Entities))
            }
            ArgumentType::Entity => {
                WasmCommandNode::Argument(argument(name, EntityArgumentType::Entity))
            }
            ArgumentType::Players => {
                WasmCommandNode::Argument(argument(name, EntityArgumentType::Players))
            }
            ArgumentType::GameProfile => {
                WasmCommandNode::Argument(argument(name, GameProfileArgumentType))
            }
            ArgumentType::BlockPos => {
                WasmCommandNode::Argument(argument(name, BlockPosArgumentType))
            }
            ArgumentType::Position3d => {
                WasmCommandNode::Argument(argument(name, Vec3ArgumentType::Default))
            }
            ArgumentType::Position2d => {
                WasmCommandNode::Argument(argument(name, Vec2ArgumentType::Default))
            }
            ArgumentType::BlockState => {
                WasmCommandNode::Argument(argument(name, BlockArgumentType))
            }
            ArgumentType::BlockPredicate => {
                WasmCommandNode::Argument(argument(name, BlockPredicateArgumentType))
            }
            ArgumentType::Item => WasmCommandNode::Argument(argument(name, ItemStackArgumentType)),
            ArgumentType::ItemPredicate => {
                WasmCommandNode::Argument(argument(name, ItemPredicateArgumentType))
            }
            ArgumentType::Component => {
                WasmCommandNode::Argument(argument(name, ComponentArgumentType))
            }
            ArgumentType::Rotation => {
                WasmCommandNode::Argument(argument(name, RotationArgumentType))
            }
            ArgumentType::ResourceLocation | ArgumentType::Resource(_) => {
                WasmCommandNode::Argument(argument(name, IdentifierArgumentType))
            }
            ArgumentType::EntityAnchor => {
                WasmCommandNode::Argument(argument(name, EntityAnchorArgumentType))
            }
            ArgumentType::Gamemode => {
                WasmCommandNode::Argument(argument(name, GameModeArgumentType))
            }
            ArgumentType::Difficulty => {
                WasmCommandNode::Argument(argument(name, StringArgumentType::SingleWord))
            }
            ArgumentType::Time(min) => {
                WasmCommandNode::Argument(argument(name, TimeArgumentType::new(min.unwrap_or(0))))
            }
            _ => {
                return Err(wasmtime::Error::msg(format!(
                    "Unimplemented argument type: {arg_type:?}"
                )));
            }
        };
        self.add(node)
            .map_err(|_| wasmtime::Error::msg("Failed to add argument node"))
    }

    fn then(
        &mut self,
        self_node: Resource<CommandNode>,
        node: Resource<CommandNode>,
    ) -> wasmtime::Result<()> {
        let child = self.take(node)?;
        let parent = self.get_mut(&self_node)?;
        let builder = std::mem::replace(parent, WasmCommandNode::Literal(literal("")));
        *parent = builder.then(child);
        Ok(())
    }

    fn execute_with_handler_id(
        &mut self,
        node: Resource<CommandNode>,
        handler_id: u32,
    ) -> wasmtime::Result<()> {
        let plugin = self
            .plugin
            .as_ref()
            .and_then(std::sync::Weak::upgrade)
            .ok_or_else(|| wasmtime::Error::msg("Plugin dropped"))?;
        let server = self
            .server
            .clone()
            .ok_or_else(|| wasmtime::Error::msg("Server not initialized"))?;

        let executor = WasmCommandExecutor {
            handler_id,
            plugin,
            server,
        };
        let resource = self.get_mut(&node)?;
        let builder = std::mem::replace(resource, WasmCommandNode::Literal(literal("")));
        *resource = builder.executes(executor);
        Ok(())
    }

    fn suggest_with_handler_id(
        &mut self,
        node: Resource<CommandNode>,
        handler_id: u32,
    ) -> wasmtime::Result<()> {
        let plugin = self
            .plugin
            .as_ref()
            .and_then(std::sync::Weak::upgrade)
            .ok_or_else(|| wasmtime::Error::msg("Plugin dropped"))?;
        let server = self
            .server
            .clone()
            .ok_or_else(|| wasmtime::Error::msg("Server not initialized"))?;

        let provider = WasmCommandSuggestionProvider {
            handler_id,
            plugin,
            server,
        };
        let resource = self.get_mut(&node)?;
        let builder = std::mem::replace(resource, WasmCommandNode::Literal(literal("")));
        *resource = builder.suggests(provider);
        Ok(())
    }

    fn require_with_handler_id(
        &mut self,
        _node: Resource<CommandNode>,
        _handler_id: u32,
    ) -> wasmtime::Result<()> {
        Err(wasmtime::Error::msg(
            "require_with_handler_id not implemented",
        ))
    }

    fn drop(&mut self, rep: Resource<CommandNode>) -> wasmtime::Result<()> {
        self.drop(rep)
    }
}

#[expect(clippy::too_many_lines)]
const fn map_util_locale_to_wit(locale: sculith_util::translation::Locale) -> Locale {
    match locale {
        sculith_util::translation::Locale::AfZa => Locale::AfZa,
        sculith_util::translation::Locale::ArSa => Locale::ArSa,
        sculith_util::translation::Locale::AstEs => Locale::AstEs,
        sculith_util::translation::Locale::AzAz => Locale::AzAz,
        sculith_util::translation::Locale::BaRu => Locale::BaRu,
        sculith_util::translation::Locale::Bar => Locale::Bar,
        sculith_util::translation::Locale::BeBy => Locale::BeBy,
        sculith_util::translation::Locale::BgBg => Locale::BgBg,
        sculith_util::translation::Locale::BrFr => Locale::BrFr,
        sculith_util::translation::Locale::Brb => Locale::Brb,
        sculith_util::translation::Locale::BsBa => Locale::BsBa,
        sculith_util::translation::Locale::CaEs => Locale::CaEs,
        sculith_util::translation::Locale::CsCz => Locale::CsCz,
        sculith_util::translation::Locale::CyGb => Locale::CyGb,
        sculith_util::translation::Locale::DaDk => Locale::DaDk,
        sculith_util::translation::Locale::DeAt => Locale::DeAt,
        sculith_util::translation::Locale::DeCh => Locale::DeCh,
        sculith_util::translation::Locale::DeDe => Locale::DeDe,
        sculith_util::translation::Locale::ElGr => Locale::ElGr,
        sculith_util::translation::Locale::EnAu => Locale::EnAu,
        sculith_util::translation::Locale::EnCa => Locale::EnCa,
        sculith_util::translation::Locale::EnGb => Locale::EnGb,
        sculith_util::translation::Locale::EnNz => Locale::EnNz,
        sculith_util::translation::Locale::EnPt => Locale::EnPt,
        sculith_util::translation::Locale::EnUd => Locale::EnUd,
        sculith_util::translation::Locale::EnUs => Locale::EnUs,
        sculith_util::translation::Locale::Enp => Locale::Enp,
        sculith_util::translation::Locale::Enws => Locale::Enws,
        sculith_util::translation::Locale::EoUy => Locale::EoUy,
        sculith_util::translation::Locale::EsAr => Locale::EsAr,
        sculith_util::translation::Locale::EsCl => Locale::EsCl,
        sculith_util::translation::Locale::EsEc => Locale::EsEc,
        sculith_util::translation::Locale::EsEs => Locale::EsEs,
        sculith_util::translation::Locale::EsMx => Locale::EsMx,
        sculith_util::translation::Locale::EsUy => Locale::EsUy,
        sculith_util::translation::Locale::EsVe => Locale::EsVe,
        sculith_util::translation::Locale::Esan => Locale::Esan,
        sculith_util::translation::Locale::EtEe => Locale::EtEe,
        sculith_util::translation::Locale::EuEs => Locale::EuEs,
        sculith_util::translation::Locale::FaIr => Locale::FaIr,
        sculith_util::translation::Locale::FiFi => Locale::FiFi,
        sculith_util::translation::Locale::FilPh => Locale::FilPh,
        sculith_util::translation::Locale::FoFo => Locale::FoFo,
        sculith_util::translation::Locale::FrCa => Locale::FrCa,
        sculith_util::translation::Locale::FrFr => Locale::FrFr,
        sculith_util::translation::Locale::FraDe => Locale::FraDe,
        sculith_util::translation::Locale::FurIt => Locale::FurIt,
        sculith_util::translation::Locale::FyNl => Locale::FyNl,
        sculith_util::translation::Locale::GaIe => Locale::GaIe,
        sculith_util::translation::Locale::GdGb => Locale::GdGb,
        sculith_util::translation::Locale::GlEs => Locale::GlEs,
        sculith_util::translation::Locale::HawUs => Locale::HawUs,
        sculith_util::translation::Locale::HeIl => Locale::HeIl,
        sculith_util::translation::Locale::HiIn => Locale::HiIn,
        sculith_util::translation::Locale::HrHr => Locale::HrHr,
        sculith_util::translation::Locale::HuHu => Locale::HuHu,
        sculith_util::translation::Locale::HyAm => Locale::HyAm,
        sculith_util::translation::Locale::IdId => Locale::IdId,
        sculith_util::translation::Locale::IgNg => Locale::IgNg,
        sculith_util::translation::Locale::IoEn => Locale::IoEn,
        sculith_util::translation::Locale::IsIs => Locale::IsIs,
        sculith_util::translation::Locale::Isv => Locale::Isv,
        sculith_util::translation::Locale::ItIt => Locale::ItIt,
        sculith_util::translation::Locale::JaJp => Locale::JaJp,
        sculith_util::translation::Locale::JboEn => Locale::JboEn,
        sculith_util::translation::Locale::KaGe => Locale::KaGe,
        sculith_util::translation::Locale::KkKz => Locale::KkKz,
        sculith_util::translation::Locale::KnIn => Locale::KnIn,
        sculith_util::translation::Locale::KoKr => Locale::KoKr,
        sculith_util::translation::Locale::Ksh => Locale::Ksh,
        sculith_util::translation::Locale::KwGb => Locale::KwGb,
        sculith_util::translation::Locale::LaLa => Locale::LaLa,
        sculith_util::translation::Locale::LbLu => Locale::LbLu,
        sculith_util::translation::Locale::LiLi => Locale::LiLi,
        sculith_util::translation::Locale::Lmo => Locale::Lmo,
        sculith_util::translation::Locale::LoLa => Locale::LoLa,
        sculith_util::translation::Locale::LolUs => Locale::LolUs,
        sculith_util::translation::Locale::LtLt => Locale::LtLt,
        sculith_util::translation::Locale::LvLv => Locale::LvLv,
        sculith_util::translation::Locale::Lzh => Locale::Lzh,
        sculith_util::translation::Locale::MkMk => Locale::MkMk,
        sculith_util::translation::Locale::MnMn => Locale::MnMn,
        sculith_util::translation::Locale::MsMy => Locale::MsMy,
        sculith_util::translation::Locale::MtMt => Locale::MtMt,
        sculith_util::translation::Locale::Nah => Locale::Nah,
        sculith_util::translation::Locale::NdsDe => Locale::NdsDe,
        sculith_util::translation::Locale::NlBe => Locale::NlBe,
        sculith_util::translation::Locale::NlNl => Locale::NlNl,
        sculith_util::translation::Locale::NnNo => Locale::NnNo,
        sculith_util::translation::Locale::NoNo => Locale::NoNo,
        sculith_util::translation::Locale::OcFr => Locale::OcFr,
        sculith_util::translation::Locale::Ovd => Locale::Ovd,
        sculith_util::translation::Locale::PlPl => Locale::PlPl,
        sculith_util::translation::Locale::PtBr => Locale::PtBr,
        sculith_util::translation::Locale::PtPt => Locale::PtPt,
        sculith_util::translation::Locale::QyaAa => Locale::QyaAa,
        sculith_util::translation::Locale::RoRo => Locale::RoRo,
        sculith_util::translation::Locale::Rpr => Locale::Rpr,
        sculith_util::translation::Locale::RuRu => Locale::RuRu,
        sculith_util::translation::Locale::RyUa => Locale::RyUa,
        sculith_util::translation::Locale::SahSah => Locale::SahSah,
        sculith_util::translation::Locale::SeNo => Locale::SeNo,
        sculith_util::translation::Locale::SkSk => Locale::SkSk,
        sculith_util::translation::Locale::SlSi => Locale::SlSi,
        sculith_util::translation::Locale::SoSo => Locale::SoSo,
        sculith_util::translation::Locale::SqAl => Locale::SqAl,
        sculith_util::translation::Locale::SrCs => Locale::SrCs,
        sculith_util::translation::Locale::SrSp => Locale::SrSp,
        sculith_util::translation::Locale::SvSe => Locale::SvSe,
        sculith_util::translation::Locale::Sxu => Locale::Sxu,
        sculith_util::translation::Locale::Szl => Locale::Szl,
        sculith_util::translation::Locale::TaIn => Locale::TaIn,
        sculith_util::translation::Locale::ThTh => Locale::ThTh,
        sculith_util::translation::Locale::TlPh => Locale::TlPh,
        sculith_util::translation::Locale::TlhAa => Locale::TlhAa,
        sculith_util::translation::Locale::Tok => Locale::Tok,
        sculith_util::translation::Locale::TrTr => Locale::TrTr,
        sculith_util::translation::Locale::TtRu => Locale::TtRu,
        sculith_util::translation::Locale::UkUa => Locale::UkUa,
        sculith_util::translation::Locale::ValEs => Locale::ValEs,
        sculith_util::translation::Locale::VecIt => Locale::VecIt,
        sculith_util::translation::Locale::ViVn => Locale::ViVn,
        sculith_util::translation::Locale::YiDe => Locale::YiDe,
        sculith_util::translation::Locale::YoNg => Locale::YoNg,
        sculith_util::translation::Locale::ZhCn => Locale::ZhCn,
        sculith_util::translation::Locale::ZhHk => Locale::ZhHk,
        sculith_util::translation::Locale::ZhTw => Locale::ZhTw,
        sculith_util::translation::Locale::ZlmArab => Locale::ZlmArab,
    }
}
