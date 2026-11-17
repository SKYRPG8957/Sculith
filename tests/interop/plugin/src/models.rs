use pumpkin_plugin_api::{
    Server,
    display::{EntityDisplayExt, ModelDisplayEntity},
    events::{EntitySpawnEvent, EventData, EventHandler},
    wit::pumpkin::plugin::{player::Player, world::Entity},
};
use std::sync::atomic::{AtomicBool, Ordering};

static CANCEL: AtomicBool = AtomicBool::new(false);
pub struct Spawned;

impl EventHandler<EntitySpawnEvent> for Spawned {
    fn handle(
        &self,
        _server: Server,
        mut event: EventData<EntitySpawnEvent>,
    ) -> EventData<EntitySpawnEvent> {
        if CANCEL.load(Ordering::Relaxed) && event.position == (-4.0, -60.0, 4.0) {
            event.cancelled = true;
            tracing::info!(id = event.entity_id, "PROBE_MODEL_CANCELLED");
        }
        event
    }
}

fn existing(player: &Player) -> Option<(Entity, ModelDisplayEntity)> {
    player
        .get_world()
        .get_entities()
        .into_iter()
        .find_map(|entity| entity.as_model_display().map(|model| (entity, model)))
}

fn spawn(player: &Player) {
    assert!(
        existing(player).is_none(),
        "duplicate model in the copied world"
    );
    let world = player.get_world();
    assert!(
        world
            .spawn_model("sculith:missing", (1.0, -60.0, 4.0), 1.0)
            .is_err()
    );
    for scale in [0.0, -1.0, f32::NAN, f32::INFINITY, 17.0] {
        assert!(
            world
                .spawn_model("sculith:demo_statue", (1.0, -60.0, 4.0), scale)
                .is_err()
        );
    }
    assert!(
        world
            .spawn_model("sculith:demo_statue", (f64::NAN, -60.0, 4.0), 1.0)
            .is_err()
    );
    CANCEL.store(true, Ordering::Relaxed);
    assert!(
        world
            .spawn_model("sculith:demo_statue", (-4.0, -60.0, 4.0), 1.0)
            .is_err()
    );
    CANCEL.store(false, Ordering::Relaxed);
    let entity = world
        .spawn_model("sculith:demo_statue", (1.0, -60.0, 4.0), 1.25)
        .unwrap();
    let model = entity.as_model_display().unwrap();
    assert_eq!(model.get_model(), "sculith:demo_statue");
    assert_eq!(model.get_scale(), 1.25);
    assert_eq!(model.get_entity().get_id(), entity.get_id());
    assert!(
        entity.as_item_display().is_none(),
        "Java-only setters must not bypass shared model constraints"
    );
    assert!(model.set_scale(f32::NAN).is_err());
    assert!(model.set_position((f64::INFINITY, -60.0, 4.0)).is_err());
    tracing::info!(id=entity.get_id(),uuid=?entity.get_uuid(),"PROBE_MODEL_SPAWNED");
}

pub fn on_join(player: &Player) {
    if player.get_name().starts_with("ModelOwner") {
        spawn(player);
    }
}

pub fn action(player: &Player, id: &str) -> bool {
    let Some(command) = id.strip_prefix("sculith:probe/model/") else {
        return false;
    };
    if command == "spawn" {
        spawn(player);
        return true;
    }
    let (entity, model) = existing(player).expect("shared model missing from authoritative world");
    match command {
        "scale" => {
            model.set_scale(2.0).unwrap();
            assert_eq!(model.get_scale(), 2.0);
        }
        "move" => {
            model.set_position((3.0, -60.0, 4.0)).unwrap();
            assert_eq!(entity.get_position(), (3.0, -60.0, 4.0));
        }
        "remove" => {
            entity.remove();
            assert!(model.set_scale(1.0).is_err());
            assert!(model.set_position((0.0, -60.0, 0.0)).is_err());
        }
        _ => panic!("unknown model probe action"),
    }
    tracing::info!(%command,id=entity.get_id(),"PROBE_MODEL_ACTION");
    true
}
