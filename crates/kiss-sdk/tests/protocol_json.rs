//! The wire format is a contract with client authors in other languages, so it
//! is pinned by exact-string assertions rather than round-trip-only checks.

use kiss_sdk::protocol::{
    Command, Incoming, ProtocolError, QueueMode, Request, Response, StreamingBehavior, decode_line,
    decode_request,
};
use serde_json::json;

#[test]
fn every_command_serializes_with_a_snake_case_type() {
    let cases: Vec<(Command, &str)> = vec![
        (
            Command::Prompt {
                message: "hi".into(),
                images: Vec::new(),
                streaming_behavior: None,
            },
            "prompt",
        ),
        (
            Command::Steer {
                message: "hi".into(),
                images: Vec::new(),
            },
            "steer",
        ),
        (
            Command::FollowUp {
                message: "hi".into(),
                images: Vec::new(),
            },
            "follow_up",
        ),
        (Command::Abort {}, "abort"),
        (Command::ClearQueue {}, "clear_queue"),
        (Command::NewSession {}, "new_session"),
        (Command::GetState {}, "get_state"),
        (Command::GetMessages {}, "get_messages"),
        (Command::GetEntries { since: None }, "get_entries"),
        (Command::GetTree {}, "get_tree"),
        (Command::GetLastAssistantText {}, "get_last_assistant_text"),
        (Command::GetSessionStats {}, "get_session_stats"),
        (
            Command::SetSessionName { name: "n".into() },
            "set_session_name",
        ),
        (
            Command::SetModel {
                provider: "p".into(),
                model_id: "m".into(),
            },
            "set_model",
        ),
        (
            Command::GetAvailableModels { search: None },
            "get_available_models",
        ),
        (
            Command::SetThinkingLevel {
                level: "high".into(),
            },
            "set_thinking_level",
        ),
        (
            Command::GetAvailableThinkingLevels {},
            "get_available_thinking_levels",
        ),
        (
            Command::SetSteeringMode {
                mode: QueueMode::All,
            },
            "set_steering_mode",
        ),
        (
            Command::SetFollowUpMode {
                mode: QueueMode::OneAtATime,
            },
            "set_follow_up_mode",
        ),
        (
            Command::Compact {
                custom_instructions: None,
            },
            "compact",
        ),
        (
            Command::SetAutoCompaction { enabled: true },
            "set_auto_compaction",
        ),
        (Command::SetAutoRetry { enabled: false }, "set_auto_retry"),
        (
            Command::Bash {
                command: "ls".into(),
            },
            "bash",
        ),
        (Command::AbortBash {}, "abort_bash"),
        (Command::GetTools {}, "get_tools"),
        (Command::ExportHtml { output_path: None }, "export_html"),
        (
            Command::SwitchSession {
                session_path: "s".into(),
            },
            "switch_session",
        ),
        (
            Command::Fork {
                entry_id: "e".into(),
            },
            "fork",
        ),
        (Command::GetForkMessages {}, "get_fork_messages"),
        (Command::Ping {}, "ping"),
    ];

    assert_eq!(cases.len(), 30, "keep this list exhaustive");
    for (command, expected) in cases {
        assert_eq!(command.name(), expected);
        let value = serde_json::to_value(&command).unwrap();
        assert_eq!(value["type"], expected, "serialized {command:?}");
        // Every command must also decode from its own serialization.
        let line = serde_json::to_string(&Request::new(command.clone())).unwrap();
        assert_eq!(decode_request(&line).unwrap().command, command);
    }
}

#[test]
fn payload_fields_are_camel_case() {
    let request = decode_request(
        r#"{"id":"7","type":"prompt","message":"hi","streamingBehavior":"followUp",
            "images":[{"type":"image","data":"AA==","mimeType":"image/png"}]}"#,
    )
    .unwrap();
    assert_eq!(request.id.as_deref(), Some("7"));
    match request.command {
        Command::Prompt {
            message,
            images,
            streaming_behavior,
        } => {
            assert_eq!(message, "hi");
            assert_eq!(streaming_behavior, Some(StreamingBehavior::FollowUp));
            assert_eq!(images.len(), 1);
            assert_eq!(images[0].mime_type, "image/png");
        }
        other => panic!("expected a prompt, got {other:?}"),
    }

    let set_model = decode_request(r#"{"type":"set_model","provider":"a","modelId":"b"}"#).unwrap();
    assert_eq!(
        set_model.command,
        Command::SetModel {
            provider: "a".into(),
            model_id: "b".into()
        }
    );
}

#[test]
fn queue_modes_use_the_kebab_case_names_from_settings() {
    let json = serde_json::to_value(&Command::SetFollowUpMode {
        mode: QueueMode::OneAtATime,
    })
    .unwrap();
    assert_eq!(json["mode"], "one-at-a-time");
}

#[test]
fn unknown_command_is_reported_by_name() {
    let error = decode_request(r#"{"type":"teleport"}"#).unwrap_err();
    match error {
        ProtocolError::UnknownCommand(name) => assert_eq!(name, "teleport"),
        other => panic!("expected UnknownCommand, got {other}"),
    }
    assert!(
        decode_request(r#"{"type":"teleport"}"#)
            .unwrap_err()
            .to_string()
            .contains("teleport")
    );
}

#[test]
fn missing_type_is_reported() {
    assert!(matches!(
        decode_request(r#"{"message":"hi"}"#).unwrap_err(),
        ProtocolError::MissingType
    ));
}

#[test]
fn response_shapes_are_exact() {
    assert_eq!(
        serde_json::to_string(&Response::err("set_model", "nope")).unwrap(),
        r#"{"type":"response","command":"set_model","success":false,"error":"nope"}"#
    );
    assert_eq!(
        serde_json::to_string(&Response::ok("abort").with_id(Some("3".into()))).unwrap(),
        r#"{"type":"response","id":"3","command":"abort","success":true}"#
    );
    assert_eq!(
        serde_json::to_string(&Response::ok_data("ping", json!({"pong": true}))).unwrap(),
        r#"{"type":"response","command":"ping","success":true,"data":{"pong":true}}"#
    );
}

#[test]
fn agent_output_is_split_into_responses_and_events() {
    match decode_line(r#"{"type":"response","command":"ping","success":true}"#).unwrap() {
        Incoming::Response(response) => {
            assert!(response.success);
            assert_eq!(response.command, "ping");
        }
        other => panic!("expected a response, got {other:?}"),
    }
    match decode_line(r#"{"type":"message_update","assistantMessageEvent":{"type":"text_delta"}}"#)
        .unwrap()
    {
        Incoming::Event(event) => {
            assert_eq!(event.event_type(), "message_update");
            assert_eq!(event.0["assistantMessageEvent"]["type"], "text_delta");
        }
        other => panic!("expected an event, got {other:?}"),
    }
}
