//! One simple menu shared between native Bedrock forms and vanilla Java dialogs.
use sculith_plugin_api::{
    Action, CustomForm, Event, FormField, FormValue, Hook, Metadata, Plugin, SimpleForm, serve,
};

struct Forms {
    observer: bool,
    inputs: bool,
}

impl Plugin for Forms {
    fn metadata(&self) -> Metadata {
        Metadata {
            name: if self.inputs && self.observer {
                "forms-input-observer"
            } else if self.inputs {
                "forms-input-demo"
            } else if self.observer {
                "forms-observer"
            } else {
                "forms-demo"
            }
            .into(),
            version: env!("CARGO_PKG_VERSION").into(),
            hooks: if self.observer {
                vec![if self.inputs {
                    Hook::CustomFormSubmitted
                } else {
                    Hook::FormSubmitted
                }]
            } else {
                vec![
                    Hook::Started,
                    Hook::PlayerJoined,
                    if self.inputs {
                        Hook::CustomFormSubmitted
                    } else {
                        Hook::FormSubmitted
                    },
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
            Event::PlayerJoined { player } if self.inputs => vec![Action::ShowCustomForm {
                player_id: player.id,
                request_id: 19,
                form: CustomForm {
                    title: "공통 입력 폼 👋".into(),
                    fields: vec![
                        FormField::Text {
                            label: "이름".into(),
                            default: "테스트 👋".into(),
                            max_length: 16,
                        },
                        FormField::Toggle {
                            label: "알림 받기".into(),
                            default: true,
                        },
                        FormField::Dropdown {
                            label: "표시 방식".into(),
                            options: vec!["기본".into(), "확장".into()],
                            default: 0,
                        },
                        FormField::Slider {
                            label: "크기".into(),
                            min: -10,
                            max: 10,
                            step: 5,
                            default: 0,
                        },
                    ],
                },
            }],
            Event::CustomFormSubmitted {
                player_id,
                request_id,
                values,
            } => {
                if self.observer {
                    return vec![Action::Log {
                        message: "unexpected foreign input reply".into(),
                    }];
                }
                let message = match values.as_deref() {
                    None => "입력을 취소했습니다.".into(),
                    Some(
                        [
                            FormValue::Text(name),
                            FormValue::Toggle(enabled),
                            FormValue::Number(option),
                            FormValue::Number(value),
                        ],
                    ) => format!("입력: {name} | 알림: {enabled} | 선택: {option} | 값: {value}"),
                    _ => "입력 항목이 일치하지 않습니다.".into(),
                };
                vec![
                    Action::Log {
                        message: format!(
                            "input reply request={request_id} player={player_id} cancelled={}",
                            values.is_none()
                        ),
                    },
                    Action::SendMessage { player_id, message },
                ]
            }
            Event::PlayerJoined { player } => vec![Action::ShowForm {
                player_id: player.id,
                request_id: 7,
                form: SimpleForm {
                    title: "메뉴".into(),
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
        inputs: std::env::args().any(|arg| arg == "--inputs"),
    })
}
