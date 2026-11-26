use std::sync::LazyLock;

use sculith_util::{
    PermissionLvl,
    math::vector2::Vector2,
    permission::{Permission, PermissionDefault, PermissionRegistry},
    text::TextComponent,
};

use crate::{
    command::{
        argument_builder::{ArgumentBuilder, RequiredArgumentBuilder, argument, command, literal},
        argument_types::{
            coordinates::block_pos::BlockPosArgumentType,
            core::{
                bool::BoolArgumentType, integer::IntegerArgumentType, string::StringArgumentType,
            },
        },
        context::command_context::CommandContext,
        errors::{command_syntax_error::CommandSyntaxError, error_types::CommandErrorType},
        node::{
            CommandExecutor, CommandExecutorResult, detached::CommandDetachedNode,
            dispatcher::CommandDispatcher,
        },
    },
    world::ticking_areas::{AreaSelection, AreaShape, MAX_AREAS, TickingArea},
};

const DESCRIPTION: &str = "Manage the named areas that remain loaded and ticking.";
const PERMISSION: &str = "minecraft:command.tickingarea";
static NO_WORLD: CommandErrorType<0> = CommandErrorType::new(
    "commands.tickingarea.noneExist.currentDimension",
    "No world in command source",
);

fn column(context: &CommandContext, name: &str) -> Result<Vector2<i32>, CommandSyntaxError> {
    BlockPosArgumentType::get_block_pos(context, name).map(|pos| pos.chunk_position())
}

fn describe(area: &TickingArea) -> String {
    let shape = match area.shape {
        AreaShape::Rectangle { min, max } => format!(
            "{} 0 {} to {} 0 {}",
            min[0] * 16,
            min[1] * 16,
            max[0] * 16 + 15,
            max[1] * 16 + 15,
        ),
        AreaShape::Circle { center, radius } => format!(
            "circle centered at {} 0 {} with radius {radius} chunks",
            center[0] * 16 + 8,
            center[1] * 16 + 8,
        ),
    };
    let status = if area.preload { " (preload)" } else { "" };
    format!("{}: {shape}{status}", area.name)
}

fn report_used(context: &CommandContext, used: usize) {
    context.source.send_feedback(
        TextComponent::text(format!("{used}/{MAX_AREAS} ticking areas in use")),
        false,
    );
}

fn report_areas<'a>(context: &CommandContext, areas: impl IntoIterator<Item = &'a TickingArea>) {
    for area in areas {
        context
            .source
            .send_feedback(TextComponent::text(describe(area)), false);
    }
}

#[derive(Clone, Copy)]
struct AddArea {
    circle: bool,
}

impl CommandExecutor for AddArea {
    fn execute(&self, context: &CommandContext) -> CommandExecutorResult {
        let world = context
            .source
            .world
            .as_ref()
            .ok_or_else(|| NO_WORLD.create_without_context())?;
        let shape = if self.circle {
            AreaShape::circle(
                column(context, "center")?,
                IntegerArgumentType::get(context, "radius")?,
            )
        } else {
            AreaShape::rectangle(column(context, "from")?, column(context, "to")?)
        };
        let name = StringArgumentType::get(context, "name").ok();
        let preload = BoolArgumentType::get(context, "preload").unwrap_or(false);
        match world.add_ticking_area(name, shape, preload) {
            Ok((area, used)) => {
                context.source.send_feedback(
                    TextComponent::text(format!("Added ticking area {}", describe(&area))),
                    true,
                );
                report_used(context, used);
                Ok(1)
            }
            Err(error) => {
                context.source.send_error(TextComponent::text(error));
                Ok(0)
            }
        }
    }
}

#[derive(Clone, Copy)]
enum Target {
    Name,
    Position,
    All,
}

impl Target {
    fn resolve<'a>(
        self,
        context: &'a CommandContext<'_>,
    ) -> Result<AreaSelection<'a>, CommandSyntaxError> {
        Ok(match self {
            Self::Name => AreaSelection::Name(StringArgumentType::get(context, "name")?),
            Self::Position => AreaSelection::Column(column(context, "position")?),
            Self::All => AreaSelection::All,
        })
    }
}

struct RemoveArea(Target);

impl CommandExecutor for RemoveArea {
    fn execute(&self, context: &CommandContext) -> CommandExecutorResult {
        let world = context
            .source
            .world
            .as_ref()
            .ok_or_else(|| NO_WORLD.create_without_context())?;
        let (areas, used) = world.remove_ticking_areas(self.0.resolve(context)?);
        if areas.is_empty() {
            context.source.send_error(TextComponent::text(
                "No matching ticking areas in this dimension",
            ));
        } else {
            context
                .source
                .send_feedback(TextComponent::text("Removed ticking areas"), true);
            report_areas(context, &areas);
            report_used(context, used);
        }
        Ok(areas.len() as i32)
    }
}

struct AreaPreload(Target);

impl CommandExecutor for AreaPreload {
    fn execute(&self, context: &CommandContext) -> CommandExecutorResult {
        let world = context
            .source
            .world
            .as_ref()
            .ok_or_else(|| NO_WORLD.create_without_context())?;
        let preload = BoolArgumentType::get(context, "preload").ok();
        let areas = world.set_ticking_area_preload(self.0.resolve(context)?, preload);
        if areas.is_empty() {
            context.source.send_error(TextComponent::text(
                "No matching ticking areas in this dimension",
            ));
        } else {
            report_areas(context, &areas);
            let count = areas.iter().filter(|area| area.preload).count();
            context.source.send_feedback(
                TextComponent::text(format!(
                    "{count} matching ticking areas are marked for preload"
                )),
                false,
            );
        }
        Ok(areas.len() as i32)
    }
}

struct ListAreas {
    all_dimensions: bool,
}

impl CommandExecutor for ListAreas {
    fn execute(&self, context: &CommandContext) -> CommandExecutorResult {
        let world = context
            .source
            .world
            .as_ref()
            .ok_or_else(|| NO_WORLD.create_without_context())?;
        let (dimensions, used) = world.list_ticking_areas(self.all_dimensions);
        if dimensions.is_empty() {
            context
                .source
                .send_feedback(TextComponent::text("No ticking areas in this scope"), false);
        }
        for (dimension, areas) in &dimensions {
            if self.all_dimensions {
                context
                    .source
                    .send_feedback(TextComponent::text(dimension.clone()), false);
            }
            report_areas(context, areas);
        }
        report_used(context, used);
        Ok(dimensions.values().map(|areas| areas.len() as i32).sum())
    }
}

fn with_add_options(
    argument_node: RequiredArgumentBuilder,
    circle: bool,
) -> RequiredArgumentBuilder {
    let add = AddArea { circle };
    argument_node.executes(add).then(
        argument("name", StringArgumentType::QuotablePhrase)
            .executes(add)
            .then(argument("preload", BoolArgumentType).executes(add)),
    )
}

fn with_preload(argument_node: RequiredArgumentBuilder, target: Target) -> RequiredArgumentBuilder {
    argument_node
        .executes(AreaPreload(target))
        .then(argument("preload", BoolArgumentType).executes(AreaPreload(target)))
}

pub(super) static TICKING_AREA_COMMAND: LazyLock<CommandDetachedNode> = LazyLock::new(|| {
    command("tickingarea", DESCRIPTION)
        .requires(PERMISSION)
        .then(
            literal("add")
                .then(
                    argument("from", BlockPosArgumentType).then(with_add_options(
                        argument("to", BlockPosArgumentType),
                        false,
                    )),
                )
                .then(
                    literal("circle").then(argument("center", BlockPosArgumentType).then(
                        with_add_options(
                            argument("radius", IntegerArgumentType { min: 0, max: 4 }),
                            true,
                        ),
                    )),
                ),
        )
        .then(
            literal("remove")
                .then(
                    argument("name", StringArgumentType::QuotablePhrase)
                        .executes(RemoveArea(Target::Name)),
                )
                .then(
                    argument("position", BlockPosArgumentType)
                        .executes(RemoveArea(Target::Position)),
                ),
        )
        .then(literal("remove_all").executes(RemoveArea(Target::All)))
        .then(
            literal("preload")
                .then(with_preload(
                    argument("name", StringArgumentType::QuotablePhrase),
                    Target::Name,
                ))
                .then(with_preload(
                    argument("position", BlockPosArgumentType),
                    Target::Position,
                )),
        )
        .then(
            literal("list")
                .executes(ListAreas {
                    all_dimensions: false,
                })
                .then(literal("all-dimensions").executes(ListAreas {
                    all_dimensions: true,
                })),
        )
        .build()
});

pub fn register(dispatcher: &mut CommandDispatcher, registry: &PermissionRegistry) {
    registry.register_permission_or_panic(Permission::new(
        PERMISSION,
        DESCRIPTION,
        PermissionDefault::Op(PermissionLvl::Two),
    ));
    dispatcher.register(TICKING_AREA_COMMAND.clone());
}
