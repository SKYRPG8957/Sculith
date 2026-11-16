use super::*;

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
    let data = vec![b'x'; MAX_MESSAGE_BYTES + 1];
    assert!(read_line_bounded(&mut &data[..]).await.is_err());
    assert!(read_line_bounded(&mut &b"partial"[..]).await.is_err());
    assert_eq!(read_line_bounded(&mut &b"{}\n"[..]).await.unwrap(), b"{}\n");
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
