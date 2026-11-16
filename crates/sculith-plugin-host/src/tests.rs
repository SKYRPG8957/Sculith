use super::*;
use sculith_plugin_api::MAX_MESSAGE_BYTES;

#[test]
fn actions_validate_as_a_batch_before_any_state_mutation() {
    assert!(
        validate_actions(&[
            Action::SetMotd {
                motd: "valid".into()
            },
            Action::SetMotd {
                motd: "injected;field".into()
            },
        ])
        .is_err()
    );
    assert!(
        validate_actions(&[Action::Log {
            message: "injected\nline".into()
        }])
        .is_err()
    );
    assert!(
        validate_actions(&vec![
            Action::Log {
                message: "x".into()
            };
            MAX_ACTIONS + 1
        ])
        .is_err()
    );
}

#[tokio::test]
async fn bounded_plugin_reader_rejects_large_and_incomplete_responses() {
    let mut buffer = Buffer::default();
    let data = vec![b'x'; MAX_MESSAGE_BYTES + 1];
    assert!(
        read_line_bounded(&mut &data[..], &mut buffer)
            .await
            .is_err()
    );
    assert!(
        read_line_bounded(&mut &b"partial"[..], &mut buffer)
            .await
            .is_err()
    );
    let mut input = &b"{}\n{\"next\":1}\n"[..];
    read_line_bounded(&mut input, &mut buffer).await.unwrap();
    assert_eq!(buffer.bytes(), b"{}\n");
    let capacity = buffer.capacity();
    read_line_bounded(&mut input, &mut buffer).await.unwrap();
    assert_eq!(buffer.bytes(), b"{\"next\":1}\n");
    assert!(buffer.capacity() >= capacity);
    let boundary = [vec![b' '; MAX_MESSAGE_BYTES - 1], vec![b'\n']].concat();
    let mut fragmented = tokio::io::BufReader::with_capacity(127, &boundary[..]);
    read_line_bounded(&mut fragmented, &mut buffer)
        .await
        .unwrap();
    assert_eq!(buffer.bytes(), boundary);
    assert_eq!(buffer.capacity(), MAX_MESSAGE_BYTES);
    let capacity = buffer.capacity();
    read_line_bounded(&mut &b"{}\n"[..], &mut buffer)
        .await
        .unwrap();
    assert_eq!(buffer.bytes(), b"{}\n");
    assert_eq!(buffer.capacity(), capacity);
}

#[cfg(unix)]
#[tokio::test]
async fn v1_callbacks_wait_concurrently_but_apply_actions_in_configuration_order() {
    let dir = tempfile::tempdir().unwrap();
    let first = dir.path().join("first");
    let second = dir.path().join("second");
    let script = r#"read -r hello
printf '%s\n' '{"id":1,"message":{"type":"ready","api_version":1,"metadata":{"name":"'"$3"'","version":"1","hooks":["started"]}}}'
read -r event
: > "$1"
while [ ! -e "$2" ]; do sleep 0.01; done
printf '%s\n' '{"id":2,"message":{"type":"actions","actions":[{"type":"set_motd","motd":"'"$3"'"}]}}'
"#;
    let configs = [(&first, &second, "first"), (&second, &first, "second")]
        .into_iter()
        .map(|(own, peer, name)| PluginConfig {
            path: "/bin/sh".into(),
            args: vec![
                "-c".into(),
                script.into(),
                "fixture".into(),
                own.display().to_string(),
                peer.display().to_string(),
                name.into(),
            ],
            timeout_ms: 2000,
        })
        .collect::<Vec<_>>();
    let mut status = ServerStatus::new("original".into(), 20).unwrap();
    let mut plugins = Plugins::load(&configs, &status).await.unwrap();
    let (shutdown, stop) = tokio::sync::watch::channel(false);
    let (world, task) = sculith_runtime::create(stop);
    let task = tokio::spawn(task);
    plugins
        .dispatch(
            Event::Started {
                server: status.clone(),
            },
            &mut status,
            &world,
        )
        .await;
    assert!(
        plugins
            .processes
            .iter()
            .all(|p| !p.disabled && p.api_version == 1)
    );
    assert_eq!(status.motd, "second");
    plugins.stop(&mut status, &world).await;
    shutdown.send_replace(true);
    task.await.unwrap().unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn rejects_incompatible_plugin_and_times_out_unresponsive_plugin() {
    let status = ServerStatus::new("test".into(), 20).unwrap();
    let config = PluginConfig {
            path: "/bin/sh".into(),
            args: vec!["-c".into(), "read -r line; printf '%s\\n' '{\"id\":1,\"message\":{\"type\":\"ready\",\"api_version\":999,\"metadata\":{\"name\":\"wrong\",\"version\":\"1\",\"hooks\":[]}}}'".into()],
            timeout_ms: 1000,
        };
    let error = match Process::start(&config, &status).await {
        Ok(_) => panic!("wrong API accepted"),
        Err(e) => e,
    };
    assert!(error.to_string().contains("incompatible"));
    let config = PluginConfig {
        path: "/bin/sh".into(),
        args: vec!["-c".into(), "exec sleep 10".into()],
        timeout_ms: 50,
    };
    let error = match Process::start(&config, &status).await {
        Ok(_) => panic!("stalled plugin accepted"),
        Err(e) => e,
    };
    assert!(error.to_string().contains("timed out"));
}

#[test]
fn world_commit_hooks_preserve_sources_and_the_legacy_player_contract() {
    use sculith_core::world::WorldActor;
    use sculith_world::{BlockChange, BlockKind, BlockPos, CommittedBatch, RegionId};
    let batch = CommittedBatch {
        region: RegionId(0),
        previous_revision: 3,
        revision: 4,
        changes: vec![BlockChange {
            position: BlockPos { x: 1, y: 4, z: 2 },
            before: BlockKind::Air,
            after: BlockKind::Stone,
        }],
    };
    let plugin = WorldEvent::PluginBlocks {
        plugin: "editor".into(),
        batch: batch.clone(),
        revisions: std::collections::BTreeMap::new(),
    };
    assert!(game_event(&plugin).is_none());
    assert!(
        matches!(world_block_event(&plugin),Some(Event::WorldBlocksChanged {actor:WorldActor::Plugin {name},region_id:0,previous_revision:3,revision:4,changes}) if name=="editor" && changes.len()==1)
    );
    let player = WorldEvent::Blocks {
        player_id: 7,
        batch,
        revisions: std::collections::BTreeMap::new(),
    };
    assert!(matches!(
        game_event(&player),
        Some(Event::BlocksChanged {
            player_id: 7,
            revision: 4,
            ..
        })
    ));
    assert!(matches!(
        world_block_event(&player),
        Some(Event::WorldBlocksChanged {
            actor: WorldActor::Player { player_id: 7 },
            revision: 4,
            ..
        })
    ));
}

#[test]
fn borrowed_requests_preserve_sdk_wire_bytes_and_do_not_require_clone() {
    use sculith_plugin_api::{BlockChange, WorldActor};
    let events = vec![
        Event::Tick { tick: 2 },
        Event::CustomFormSubmitted {
            player_id: 7,
            request_id: 19,
            values: Some(vec![sculith_plugin_api::FormValue::Text(
                "입력 👋 \"quoted\"".into(),
            )]),
        },
        Event::WorldBlocksChanged {
            region_id: 0,
            previous_revision: 8,
            revision: 9,
            actor: WorldActor::Plugin {
                name: "test".into(),
            },
            changes: vec![BlockChange {
                x: -1,
                y: 4,
                z: 2,
                before: "minecraft:air".into(),
                after: "minecraft:stone".into(),
            }],
        },
    ];
    for id in [1, u64::MAX] {
        for event in &events {
            let mut borrowed = Buffer::default();
            framing::request(&mut borrowed, id, &EventMessage::Event { event }).unwrap();
            let mut owned = serde_json::to_vec(&sculith_plugin_api::Request {
                id,
                message: HostMessage::Event {
                    event: event.clone(),
                },
            })
            .unwrap();
            owned.push(b'\n');
            assert_eq!(borrowed.bytes(), owned);
            let mut body = Buffer::default();
            framing::event(&mut body, event).unwrap();
            let mut shared = Buffer::default();
            framing::shared_request(&mut shared, id, body.bytes()).unwrap();
            assert_eq!(shared.bytes(), owned);
            let decoded: sculith_plugin_api::Request =
                serde_json::from_slice(shared.bytes()).unwrap();
            assert_eq!(decoded.id, id);
        }
    }
    #[derive(serde::Serialize)]
    struct NotClone {
        value: u32,
    }
    let mut output = Buffer::default();
    framing::request(&mut output, 1, &NotClone { value: 7 }).unwrap();
    assert_eq!(output.bytes(), b"{\"id\":1,\"message\":{\"value\":7}}\n");
}

#[test]
fn encoding_is_bounded_including_request_id_and_newline() {
    let mut buffer = Buffer::default();
    // A JSON string's quotes plus the envelope occupy 22 bytes at ID 1.
    let exact = "x".repeat(MAX_MESSAGE_BYTES - 22);
    framing::request(&mut buffer, 1, &exact).unwrap();
    assert_eq!(buffer.bytes().len(), MAX_MESSAGE_BYTES);
    assert_eq!(buffer.capacity(), MAX_MESSAGE_BYTES);
    assert!(framing::request(&mut buffer, 1, &(exact.clone() + "x")).is_err());
    assert!(buffer.bytes().len() <= MAX_MESSAGE_BYTES);
    // The same body fits one worker's ID but not a larger worker ID.
    let body = serde_json::to_vec(&exact).unwrap();
    framing::shared_request(&mut buffer, 1, &body).unwrap();
    assert!(framing::shared_request(&mut buffer, 10, &body).is_err());
    assert!(framing::shared_request(&mut buffer, u64::MAX, &body).is_err());
    framing::request(&mut buffer, u64::MAX, &true).unwrap();
    assert_eq!(
        buffer.bytes(),
        b"{\"id\":18446744073709551615,\"message\":true}\n"
    );
    assert_eq!(buffer.capacity(), MAX_MESSAGE_BYTES);
    // An oversized event cannot grow the shared serialization storage either.
    let event = Event::CustomFormSubmitted {
        player_id: 1,
        request_id: 1,
        values: Some(vec![sculith_plugin_api::FormValue::Text(
            "x".repeat(MAX_MESSAGE_BYTES + 1),
        )]),
    };
    assert!(framing::event(&mut buffer, &event).is_err());
    assert!(buffer.bytes().len() <= MAX_MESSAGE_BYTES);
    assert_eq!(buffer.capacity(), MAX_MESSAGE_BYTES);
}

#[cfg(unix)]
#[tokio::test]
async fn shared_events_keep_independent_ids_private_replies_and_owned_responses() {
    let script = r#"read -r hello
printf '%s\n' '{"id":1,"message":{"type":"ready","api_version":11,"metadata":{"name":"'"$1"'","version":"1","hooks":["tick","custom_form_submitted","stopping"]}}}'
while read -r event; do
  id=${event#*\"id\":}; id=${id%%,*}
  printf '%s\n' '{"id":'"$id"',"message":{"type":"actions","actions":[{"type":"set_motd","motd":"'"$1"'"}]}}'
done
"#;
    let configs = ["owner", "observer"].map(|name| PluginConfig {
        path: "/bin/sh".into(),
        args: vec!["-c".into(), script.into(), "fixture".into(), name.into()],
        timeout_ms: 2000,
    });
    let mut status = ServerStatus::new("original".into(), 20).unwrap();
    let mut plugins = Plugins::load(&configs, &status).await.unwrap();
    let (shutdown, stop) = tokio::sync::watch::channel(false);
    let (world, task) = sculith_runtime::create(stop);
    let task = tokio::spawn(task);
    plugins.start_profile(Duration::from_secs(30)).unwrap();
    let private = Event::CustomFormSubmitted {
        player_id: 1,
        request_id: 77,
        values: Some(vec![sculith_plugin_api::FormValue::Text("secret".into())]),
    };
    plugins
        .dispatch_for(private, Some("owner"), &mut status, &world)
        .await;
    assert_eq!(status.motd, "owner");
    assert_eq!(plugins.processes[0].request_id, 2);
    assert_eq!(plugins.processes[1].request_id, 1);
    let retained = plugins.processes[0]
        .request_event(&Event::Tick { tick: 0 })
        .await
        .unwrap();
    for tick in 1..=3 {
        plugins
            .dispatch(Event::Tick { tick }, &mut status, &world)
            .await;
        assert_eq!(status.motd, "observer");
    }
    assert!(
        matches!(retained, PluginMessage::Actions { actions } if matches!(&actions[..], [Action::SetMotd {motd}] if motd == "owner"))
    );
    let profile = plugins.profile_snapshot();
    assert_eq!(profile.dispatcher.message_encodes, 3);
    assert_eq!(profile.plugins[0].ipc.message_encodes, 3); // Hello, private, direct Tick
    assert_eq!(profile.plugins[1].ipc.message_encodes, 1); // Hello only
    assert_eq!(profile.plugins[0].ipc.calls, 6);
    assert_eq!(profile.plugins[1].ipc.calls, 4);
    for (worker, expected) in profile
        .plugins
        .iter()
        .zip([vec![2, 3, 4, 5, 6], vec![2, 3, 4]])
    {
        assert!(!worker.disabled);
        assert_eq!(
            worker
                .ipc
                .samples
                .iter()
                .map(|s| s.request_id.unwrap())
                .collect::<Vec<_>>(),
            expected
        );
    }
    for process in &plugins.processes {
        assert!(process.request_buffer.bytes().is_empty());
        assert!(process.reply_buffer.bytes().is_empty());
        assert!(process.request_buffer.capacity() <= MAX_MESSAGE_BYTES);
        assert!(process.reply_buffer.capacity() <= MAX_MESSAGE_BYTES);
    }
    assert!(plugins.event_buffer.bytes().is_empty());
    let calls = plugins
        .profile_snapshot()
        .plugins
        .iter()
        .map(|p| p.ipc.calls)
        .collect::<Vec<_>>();
    // Neither worker owns this private response, despite both subscribing.
    plugins
        .dispatch_for(
            Event::CustomFormSubmitted {
                player_id: 1,
                request_id: 78,
                values: None,
            },
            Some("missing"),
            &mut status,
            &world,
        )
        .await;
    assert_eq!(
        plugins
            .profile_snapshot()
            .plugins
            .iter()
            .map(|p| p.ipc.calls)
            .collect::<Vec<_>>(),
        calls
    );
    plugins.stop(&mut status, &world).await;
    shutdown.send_replace(true);
    task.await.unwrap().unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn failed_frames_clear_buffers_and_preserve_response_checks() {
    for (reply, expected) in [
        (
            "printf '%s\\n' '{\"id\":99,\"message\":{\"type\":\"actions\",\"actions\":[]}}'",
            "ID mismatch",
        ),
        ("printf '%s\\n' 'not-json'", "invalid plugin JSON"),
        ("printf 'partial'", "complete response"),
        ("exec sleep 10", "timed out"),
    ] {
        let script = format!(
            r#"read -r hello
printf '%s\n' '{{"id":1,"message":{{"type":"ready","api_version":1,"metadata":{{"name":"test","version":"1","hooks":["tick"]}}}}}}'
read -r event
{reply}
"#
        );
        let config = PluginConfig {
            path: "/bin/sh".into(),
            args: vec!["-c".into(), script],
            timeout_ms: 100,
        };
        let status = ServerStatus::new("test".into(), 20).unwrap();
        let mut process = Process::start(&config, &status).await.unwrap();
        let error = process
            .request_event(&Event::Tick { tick: 1 })
            .await
            .unwrap_err();
        assert!(format!("{error:#}").contains(expected), "{error:#}");
        assert_eq!(process.metrics.snapshot(Instant::now()).failures, 1);
        assert!(process.request_buffer.bytes().is_empty());
        assert!(process.reply_buffer.bytes().is_empty());
        let _ = process.child.kill().await;
    }
}

#[cfg(unix)]
#[tokio::test]
async fn oversize_fanout_fails_before_io_and_leaves_other_workers_untouched() {
    let script = r#"read -r hello
printf '%s\n' '{"id":1,"message":{"type":"ready","api_version":11,"metadata":{"name":"'"$1"'","version":"1","hooks":'"$2"'}}}'
read -r event
: > "$3"
"#;
    let dir = tempfile::tempdir().unwrap();
    let configs = ["first", "second", "idle"].map(|name| PluginConfig {
        path: "/bin/sh".into(),
        args: vec![
            "-c".into(),
            script.into(),
            "fixture".into(),
            name.into(),
            if name == "idle" {
                "[]"
            } else {
                "[\"custom_form_submitted\"]"
            }
            .into(),
            dir.path().join(name).display().to_string(),
        ],
        timeout_ms: 1000,
    });
    let mut status = ServerStatus::new("test".into(), 20).unwrap();
    let mut plugins = Plugins::load(&configs, &status).await.unwrap();
    let (shutdown, stop) = tokio::sync::watch::channel(false);
    let (world, task) = sculith_runtime::create(stop);
    let task = tokio::spawn(task);
    // ID exhaustion must not wrap or attempt a write/serialization.
    let idle = &mut plugins.processes[2];
    idle.request_id = u64::MAX;
    assert!(
        idle.request_event(&Event::Tick { tick: 1 })
            .await
            .unwrap_err()
            .to_string()
            .contains("exhausted")
    );
    assert_eq!(idle.request_id, u64::MAX);
    assert_eq!(idle.metrics.snapshot(Instant::now()).message_encodes, 1);
    let event = Event::CustomFormSubmitted {
        player_id: 1,
        request_id: 1,
        values: Some(vec![sculith_plugin_api::FormValue::Text(
            "x".repeat(MAX_MESSAGE_BYTES),
        )]),
    };
    plugins.dispatch(event, &mut status, &world).await;
    assert!(plugins.processes[0].disabled && plugins.processes[1].disabled);
    assert!(!plugins.processes[2].disabled);
    assert_eq!(plugins.profile_snapshot().dispatcher.message_encodes, 1);
    assert_eq!(
        plugins.profile_snapshot().plugins[0].ipc.sent_bytes,
        plugins.profile_snapshot().plugins[1].ipc.sent_bytes
    ); // Hello only
    assert!(dir.path().read_dir().unwrap().next().is_none());
    assert!(plugins.event_buffer.bytes().is_empty());
    assert!(plugins.event_buffer.capacity() <= MAX_MESSAGE_BYTES);
    plugins.stop(&mut status, &world).await;
    shutdown.send_replace(true);
    task.await.unwrap().unwrap();
}
