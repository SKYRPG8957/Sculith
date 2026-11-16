//! One simple menu shared between native Bedrock forms and vanilla Java dialogs.
use sculith_plugin_api::{Action, Event, Hook, Metadata, Plugin, SimpleForm, serve};

struct Forms {
    observer: bool,
}

impl Plugin for Forms {
    fn metadata(&self) -> Metadata {
        Metadata {
            name: if self.observer {
                "forms-observer"
            } else {
                "forms-demo"
            }
            .into(),
            version: env!("CARGO_PKG_VERSION").into(),
            hooks: if self.observer {
                vec![Hook::FormSubmitted]
            } else {
                vec![
                    Hook::Started,
                    Hook::PlayerJoined,
                    Hook::FormSubmitted,
                    Hook::Stopping,
                ]
            },
        }
    }

    fn on_event(&mut self, event: Event) -> Vec<Action> {
        match event {
            Event::Started { .. } => vec![Action::Log {
                message: "forms plugin started".into(),
            }],
            Event::PlayerJoined { player } => vec![Action::ShowForm {
                player_id: player.id,
                request_id: 7,
                form: SimpleForm {
                    title: "Sculith에 오신 것을 환영합니다 👋".into(),
                    body: "Java와 베드락에서 같은 메뉴를 사용합니다.\n원하는 항목을 선택하세요."
                        .into(),
                    buttons: vec!["기능 안내".into(), "닫기".into()],
                },
            }],
            Event::FormSubmitted {
                player_id,
                request_id,
                selection,
            } => {
                if self.observer {
                    return vec![Action::Log {
                        message: "unexpected foreign form reply".into(),
                    }];
                }
                let message = match selection {
                    Some(0) => {
                        "Sculith: 블록을 설치하거나 부수고, Ctrl+C로 서버를 종료하면 월드를 저장합니다."
                    }
                    Some(1) | None => "Sculith: 메뉴를 닫았습니다.",
                    _ => "Sculith: 알 수 없는 메뉴 선택입니다.",
                };
                vec![
                    Action::Log {
                        message: format!(
                            "forms reply request={request_id} player={player_id} selection={selection:?}"
                        ),
                    },
                    Action::SendMessage {
                        player_id,
                        message: message.into(),
                    },
                ]
            }
            Event::Stopping => vec![Action::Log {
                message: "forms plugin stopped".into(),
            }],
            _ => vec![],
        }
    }
}

fn main() -> std::io::Result<()> {
    serve(Forms {
        observer: std::env::args().any(|arg| arg == "--observe"),
    })
}
