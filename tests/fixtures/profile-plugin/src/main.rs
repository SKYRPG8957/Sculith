//! Empty Rust callbacks for repeatable host/IPC measurements, never a default plugin.
use sculith_plugin_api::{Action, Event, Hook, Metadata, Plugin, serve};
struct Fixture {
    name: String,
    kind: String,
}
impl Plugin for Fixture {
    fn metadata(&self) -> Metadata {
        let mut hooks = vec![Hook::Stopping];
        match self.kind.as_str() {
            "world" => hooks.push(Hook::WorldBlocksChanged),
            "tick" => hooks.push(Hook::Tick),
            "none" => {}
            _ => panic!("unknown benchmark hook"),
        }
        Metadata {
            name: self.name.clone(),
            version: env!("CARGO_PKG_VERSION").into(),
            hooks,
        }
    }
    fn on_event(&mut self, _: Event) -> Vec<Action> {
        vec![]
    }
}
fn main() -> std::io::Result<()> {
    let mut args = std::env::args().skip(1);
    serve(Fixture {
        name: args.next().expect("fixture name"),
        kind: args.next().unwrap_or("none".into()),
    })
}
