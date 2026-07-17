use super::*;
use codex_app_server_protocol::JSONRPCNotification;
use codex_uds::UnixListener;
use pretty_assertions::assert_eq;
use tokio::time::timeout;
use tokio_tungstenite::accept_async;

#[test]
fn login_defaults_to_device_auth_with_an_explicit_browser_override() {
    let default = Cli::try_parse_from(["codex-relay", "login"]).expect("command should parse");
    assert!(matches!(
        default.command,
        RelayCommand::Login { browser: false }
    ));

    let browser =
        Cli::try_parse_from(["codex-relay", "login", "--browser"]).expect("command should parse");
    assert!(matches!(
        browser.command,
        RelayCommand::Login { browser: true }
    ));
}

#[test]
fn defaults_to_stock_codex_and_chatgpt_backend() {
    let cli = Cli::try_parse_from(["codex-relay", "remote-control", "start"])
        .expect("command should parse");
    let RelayCommand::RemoteControl {
        command: RemoteControlCommand::Start(args),
    } = cli.command
    else {
        panic!("expected remote-control start command");
    };

    assert_eq!(
        (
            args.codex,
            args.codex_home,
            args.remote_control_url,
            args.name,
        ),
        (
            PathBuf::from("codex"),
            None,
            CHATGPT_BASE_URL.to_string(),
            None,
        )
    );
}

#[test]
fn verbose_is_a_global_option() {
    let cli = Cli::try_parse_from(["codex-relay", "remote-control", "pair", "--verbose"])
        .expect("command should parse");

    assert!(cli.verbose);
}

#[test]
fn relay_home_is_global_and_codex_home_selects_the_child_home() {
    let cli = Cli::try_parse_from([
        "codex-relay",
        "remote-control",
        "start",
        "--relay-home",
        "/tmp/relay-home",
        "--codex-home",
        "/tmp/codex-home",
    ])
    .expect("command should parse");
    let RelayCommand::RemoteControl {
        command: RemoteControlCommand::Start(start),
    } = cli.command
    else {
        panic!("expected remote-control start command");
    };

    assert_eq!(cli.relay_home, Some(PathBuf::from("/tmp/relay-home")));
    assert_eq!(start.codex_home, Some(PathBuf::from("/tmp/codex-home")));
}

#[test]
fn pair_accepts_a_machine_name_without_a_manual_mode_flag() {
    let cli = Cli::try_parse_from([
        "codex-relay",
        "remote-control",
        "pair",
        "--name",
        "build-box",
    ])
    .expect("command should parse");
    let RelayCommand::RemoteControl {
        command: RemoteControlCommand::Pair { start },
    } = cli.command
    else {
        panic!("expected remote-control pair command");
    };

    assert_eq!(start.name.as_deref(), Some("build-box"));
}

#[test]
fn installation_id_is_stable_within_codex_home() {
    let codex_home = tempfile::tempdir().expect("temporary Codex home should be created");

    let first = load_or_create_installation_id(codex_home.path())
        .expect("installation id should be created");
    let second = load_or_create_installation_id(codex_home.path())
        .expect("installation id should be loaded");

    assert_eq!(first, second);
}

#[test]
fn explicit_codex_home_wins_for_enrollment_state() {
    assert_eq!(
        resolve_codex_home(Some(Path::new("/tmp/codex-home")))
            .expect("explicit Codex home should resolve"),
        PathBuf::from("/tmp/codex-home")
    );
}

#[test]
fn parses_every_child_jsonrpc_message_kind() {
    let cases = [
        r#"{"id":1,"method":"currentTime/read","params":{"threadId":"thread-1"}}"#,
        r#"{"method":"thread/closed","params":{"threadId":"thread-1"}}"#,
        r#"{"id":1,"result":null}"#,
        r#"{"id":1,"error":{"code":-32600,"message":"bad request"}}"#,
    ];

    for message in cases {
        let parsed = parse_child_message(message).expect("valid JSON-RPC should parse");
        assert_eq!(
            serde_json::to_value(parsed).expect("outgoing message should serialize"),
            serde_json::from_str::<serde_json::Value>(message)
                .expect("test JSON-RPC should deserialize")
        );
    }
}

#[test]
fn rejects_non_object_child_message() {
    let error = parse_child_message("[]").expect_err("array is not a JSON-RPC message");

    assert_eq!(
        error.to_string(),
        "invalid type: sequence, expected a map at line 1 column 0"
    );
}

#[tokio::test]
async fn bridge_forwards_messages_control_frames_and_disconnects() {
    let temp_dir = tempfile::tempdir().expect("temporary socket directory should be created");
    let socket_path = temp_dir.path().join("app-server.sock");
    let mut listener = UnixListener::bind(&socket_path)
        .await
        .expect("test listener should bind");
    let expected_incoming = JSONRPCMessage::Notification(JSONRPCNotification {
        method: "initialized".to_string(),
        params: None,
    });
    let server = tokio::spawn({
        let expected_incoming = expected_incoming.clone();
        async move {
            let stream = listener.accept().await.expect("relay should connect");
            let mut websocket = accept_async(stream)
                .await
                .expect("WebSocket handshake should complete");
            let message = websocket
                .next()
                .await
                .expect("relay should send a message")
                .expect("relay message should be valid");
            let Message::Text(text) = message else {
                panic!("expected a text message");
            };
            assert_eq!(
                serde_json::from_str::<JSONRPCMessage>(&text)
                    .expect("relay JSON-RPC should deserialize"),
                expected_incoming
            );

            websocket
                .send(Message::Ping(vec![1, 2, 3].into()))
                .await
                .expect("ping should send");
            assert_eq!(
                websocket
                    .next()
                    .await
                    .expect("relay should answer ping")
                    .expect("pong should be valid"),
                Message::Pong(vec![1, 2, 3].into())
            );
            websocket
                .send(Message::Text(r#"{"id":7,"result":{"ok":true}}"#.into()))
                .await
                .expect("response should send");
            websocket.close(None).await.expect("close should send");
        }
    });
    let (remote_writer, mut remote_reader) = mpsc::channel(8);
    let disconnect = CancellationToken::new();
    let bridge = connect_child(&socket_path, remote_writer, Some(disconnect.clone()))
        .await
        .expect("relay should connect to child");

    bridge
        .incoming_tx
        .send(expected_incoming)
        .await
        .expect("remote message should enter bridge");
    let forwarded = timeout(Duration::from_secs(1), remote_reader.recv())
        .await
        .expect("child response should arrive")
        .expect("remote writer should remain open");
    assert_eq!(
        serde_json::to_value(forwarded.message).expect("forwarded response should serialize"),
        serde_json::json!({"id": 7, "result": {"ok": true}})
    );
    timeout(Duration::from_secs(1), disconnect.cancelled())
        .await
        .expect("child close should disconnect remote client");
    server.await.expect("test server should not panic");
    bridge.task.await.expect("bridge task should not panic");
}
