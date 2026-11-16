use sculith_plugin_api::{Action, Event, Hook, Metadata, Plugin, serve};

struct Welcome;

impl Plugin for Welcome {
    fn metadata(&self) -> Metadata {
        Metadata {
            name: "welcome".into(),
            version: env!("CARGO_PKG_VERSION").into(),
            hooks: vec![Hook::Started, Hook::Stopping],
        }
    }

    fn on_event(&mut self, event: Event) -> Vec<Action> {
        match event {
            Event::Started { .. } => vec![
                Action::SetMotd {
                    motd: "Sculith Rust plugin / discovery only".into(),
                },
                Action::Log {
                    message: "공통 MOTD를 변경했습니다. Java와 Bedrock에 함께 반영됩니다.".into(),
                },
            ],
            Event::Stopping => vec![Action::Log {
                message: "welcome plugin stopped".into(),
            }],
            _ => vec![],
        }
    }
}

fn main() -> std::io::Result<()> {
    serve(Welcome)
}
