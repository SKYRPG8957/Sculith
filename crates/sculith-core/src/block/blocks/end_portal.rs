use std::sync::Arc;

use crate::block::BlockBehaviour;
use crate::block::GetInsideCollisionShapeArgs;
use crate::block::OnEntityCollisionArgs;
use crate::block::PlacedArgs;
use crate::block::entities::end_portal::EndPortalBlockEntity;
use pumpkin_data::dimension::Dimension;
use pumpkin_macros::pumpkin_block;
use sculith_util::math::boundingbox::BoundingBox;
use sculith_util::math::vector3::Vector3;

#[pumpkin_block("minecraft:end_portal")]
pub struct EndPortalBlock;

impl BlockBehaviour for EndPortalBlock {
    fn get_inside_collision_shape(&self, _args: GetInsideCollisionShapeArgs<'_>) -> BoundingBox {
        BoundingBox::new(Vector3::new(0.0, 0.0, 0.0), Vector3::new(1.0, 0.75, 1.0))
    }

    fn on_entity_collision(&self, args: OnEntityCollisionArgs<'_>) {
        let dimension = if args.world.dimension == Dimension::THE_END {
            Dimension::OVERWORLD
        } else {
            Dimension::THE_END
        };
        let Some(target_world) = args.server.find_sibling_dimension(args.world, &dimension) else {
            return;
        };
        args.entity
            .get_entity()
            .try_use_portal(target_world, *args.position);
    }

    fn placed(&self, args: PlacedArgs<'_>) {
        let nbt = EndPortalBlockEntity::create_nbt(*args.position);
        args.world.add_block_entity_nbt(*args.position, &nbt);

        args.world
            .add_block_entity(Arc::new(EndPortalBlockEntity::new(*args.position)));
    }
}
