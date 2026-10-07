//! Deterministic ACP bridge checks. Both sides use the production JSON codecs,
//! with a socket-pair daemon and captured newline-delimited ACP output.
use super::*;
use tokio::io::DuplexStream;

#[tokio::test]
async fn cancelled_daemon_read_preserves_partial_json_for_the_next_control() {
    let (runtime, _output, _daemon, mut writer) = harness().await;
    let session = runtime.sessions.lock().await["s1"].clone();
    let event = format!(
        "{}\n",
        serde_json::to_string(&ServerEvent::TextDelta {
            text: "preserved".into()
        })
        .unwrap()
    );
    writer.write_all(&event.as_bytes()[..10]).await.unwrap();
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(25), session.read_event())
            .await
            .is_err()
    );
    writer.write_all(&event.as_bytes()[10..]).await.unwrap();
    assert!(
        matches!(session.read_event().await.unwrap(), ServerEvent::TextDelta { text } if text == "preserved")
    );
}

#[tokio::test]
async fn stalled_daemon_control_returns_error_and_keeps_acp_usable() {
    let (runtime, mut output, mut daemon, _writer) = harness().await;
    rpc(
        &runtime,
        1,
        "session/set_config_option",
        json!({"sessionId": "s1", "configId": "reasoning_effort", "value": "low"}),
    )
    .await;
    let request: Request = serde_json::from_value(receive(&mut daemon).await).unwrap();
    assert!(matches!(request, Request::SetReasoningEffort { .. }));
    let error = receive(&mut output).await;
    assert_eq!(error["error"]["code"], JSONRPC_SERVER_ERROR);
    assert!(
        error["error"]["message"]
            .as_str()
            .unwrap()
            .contains("timed out after 30 seconds")
    );
    rpc(&runtime, 2, "initialize", json!({"protocolVersion": 1})).await;
    assert_eq!(receive(&mut output).await["id"], 2);
}

#[test]
fn interleaved_tool_inputs_follow_explicit_call_ids() {
    let mut mapper = EventMapper::new("root".into(), AcpProfile::Standard);
    for id in ["first", "second"] {
        mapper.map_event(ServerEvent::ToolStart {
            id: id.into(),
            name: "read".into(),
        });
    }
    let first = mapper.map_event(ServerEvent::ToolInput {
        id: Some("first".into()),
        delta: "{\"file_path\":\"first.rs\"}".into(),
    });
    let second = mapper.map_event(ServerEvent::ToolInput {
        id: Some("second".into()),
        delta: "{\"file_path\":\"second.rs\"}".into(),
    });
    assert_eq!(first[0]["toolCallId"], "first");
    assert_eq!(first[0]["rawInput"]["file_path"], "first.rs");
    assert_eq!(second[0]["toolCallId"], "second");
    assert_eq!(second[0]["rawInput"]["file_path"], "second.rs");
}

#[tokio::test]
async fn unmatched_history_image_anchors_are_preserved() {
    let (runtime, mut output, _, _) = harness().await;
    let messages =
        serde_json::from_value(json!([{ "role": "system", "content": "hidden" }])).unwrap();
    let images = serde_json::from_value(json!([
        {"media_type": "image/png", "data": "first", "label": null, "source": {"kind": "user_input"}, "anchor": {"kind": "user_prompt", "ordinal": 9}, "history_message_index": 0},
        {"media_type": "image/png", "data": "second", "label": null, "source": {"kind": "tool_result", "tool_name": "read"}, "history_message_index": 99}
    ])).unwrap();
    runtime
        .replay_history("s1", messages, images)
        .await
        .unwrap();
    assert_eq!(
        receive(&mut output).await["params"]["update"]["content"]["data"],
        "first"
    );
    assert_eq!(
        receive(&mut output).await["params"]["update"]["content"]["data"],
        "second"
    );
}

#[tokio::test]
async fn busy_or_disconnected_configuration_requests_leave_acp_connection_usable() {
    let (runtime, mut output, daemon, writer) = harness().await;
    let session = runtime.sessions.lock().await["s1"].clone();
    session.prompt_running.store(true, Ordering::SeqCst);
    rpc(
        &runtime,
        1,
        "session/set_config_option",
        json!({"sessionId": "s1", "configId": "reasoning_effort", "value": "low"}),
    )
    .await;
    assert_eq!(
        receive(&mut output).await["error"]["code"],
        JSONRPC_SERVER_ERROR
    );
    session.prompt_running.store(false, Ordering::SeqCst);
    drop(daemon);
    drop(writer);
    rpc(
        &runtime,
        2,
        "session/set_config_option",
        json!({"sessionId": "s1", "configId": "reasoning_effort", "value": "low"}),
    )
    .await;
    assert_eq!(
        receive(&mut output).await["error"]["code"],
        JSONRPC_SERVER_ERROR
    );
    assert_eq!(
        session.ui_state.lock().await.reasoning_effort.as_deref(),
        Some("high")
    );
    rpc(&runtime, 3, "initialize", json!({"protocolVersion": 1})).await;
    assert_eq!(receive(&mut output).await["result"]["protocolVersion"], 1);
}

#[tokio::test]
async fn cancel_routes_while_prompt_streaming_and_finishes_original_request() {
    let (runtime, mut output, mut daemon, mut writer) = harness().await;
    rpc(
        &runtime,
        1,
        "session/prompt",
        json!({"sessionId": "s1", "prompt": [{"type": "text", "text": "work"}]}),
    )
    .await;
    let prompt: Request = serde_json::from_value(receive(&mut daemon).await).unwrap();
    assert!(matches!(prompt, Request::Message { .. }));
    rpc(&runtime, 2, "session/cancel", json!({"sessionId": "s1"})).await;
    let cancel: Request = serde_json::from_value(receive(&mut daemon).await).unwrap();
    assert!(matches!(cancel, Request::Cancel { .. }));
    assert_eq!(receive(&mut output).await["id"], 2);
    send_event(&mut writer, ServerEvent::Interrupted).await;
    send_event(&mut writer, ServerEvent::Done { id: prompt.id() }).await;
    let response = receive(&mut output).await;
    assert_eq!(response["id"], 1);
    assert_eq!(response["result"]["stopReason"], "cancelled");
}

#[tokio::test]
async fn new_load_resume_hydrate_catalog_and_preserve_buffered_events() {
    let (runtime, mut output, _, _) = harness().await;
    runtime.sessions.lock().await.clear();
    rpc(&runtime, 0, "initialize", json!({"protocolVersion": 1})).await;
    assert_eq!(
        receive(&mut output).await["result"]["agentCapabilities"]["sessionCapabilities"]["list"],
        json!({})
    );
    for (rpc_id, method) in [
        (1, "session/new"),
        (2, "session/load"),
        (3, "session/resume"),
    ] {
        let (client, daemon) = crate::transport::stream_pair().unwrap();
        runtime
            .daemon_connections
            .lock()
            .await
            .push(client.into_split());
        let is_new = method == "session/new";
        let server = tokio::spawn(async move {
            let (reader, mut writer) = daemon.into_split();
            let mut reader = BufReader::new(reader);
            let subscribe: Request = serde_json::from_value(receive(&mut reader).await).unwrap();
            let Request::Subscribe {
                id,
                target_session_id,
                working_dir,
                allow_session_takeover,
                ..
            } = subscribe
            else {
                panic!("expected subscribe")
            };
            assert_eq!(target_session_id.as_deref(), (!is_new).then_some("s1"));
            assert_eq!(working_dir.as_deref(), Some("/project"));
            assert!(!allow_session_takeover);
            let history = |id, catalog: bool| -> ServerEvent {
                serde_json::from_value(json!({
                    "type": "history", "id": id, "session_id": "s1",
                    "messages": [{"role": "user", "content": "question"}, {"role": "assistant", "content": "answer"}],
                    "provider_name": "openai", "provider_model": "gpt-5.2", "reasoning_effort": "high",
                    "available_models": if catalog { vec!["gpt-5.2", "gpt-5.2-codex"] } else { vec![] },
                    "subagent_model": "gpt-5.2-codex", "compaction_mode": "semantic"
                })).unwrap()
            };
            if !is_new {
                send_event(&mut writer, history(id, false)).await
            }
            send_event(&mut writer, ServerEvent::Done { id }).await;
            if is_new {
                let request: Request = serde_json::from_value(receive(&mut reader).await).unwrap();
                assert!(matches!(request, Request::GetHistory { .. }));
                send_event(&mut writer, history(request.id(), false)).await;
            }
            let request: Request = serde_json::from_value(receive(&mut reader).await).unwrap();
            assert!(matches!(request, Request::GetModelCatalog { .. }));
            let events = format!(
                "{}\n{}\n",
                serde_json::to_string(&history(request.id(), true)).unwrap(),
                serde_json::to_string(&ServerEvent::TextDelta {
                    text: "buffered".into()
                })
                .unwrap()
            );
            writer.write_all(events.as_bytes()).await.unwrap();
        });
        rpc(
            &runtime,
            rpc_id,
            method,
            json!({"sessionId": "s1", "cwd": "/project", "mcpServers": []}),
        )
        .await;
        if method == "session/load" {
            assert_eq!(
                receive(&mut output).await["params"]["update"]["content"]["text"],
                "question"
            );
            assert_eq!(
                receive(&mut output).await["params"]["update"]["content"]["text"],
                "answer"
            );
        }
        let response = receive(&mut output).await;
        assert_eq!(response["id"], rpc_id);
        assert_eq!(
            response["result"]["models"]["availableModels"]
                .as_array()
                .unwrap()
                .len(),
            2
        );
        assert_eq!(
            option(&response, CONFIG_ID_COMPACTION)["currentValue"],
            "semantic"
        );
        assert_eq!(
            option(&response, CONFIG_ID_SUBAGENT_MODEL)["currentValue"],
            "gpt-5.2-codex"
        );
        assert_eq!(
            receive(&mut output).await["params"]["update"]["sessionUpdate"],
            "available_commands_update"
        );
        if is_new {
            rpc(&runtime, 10, "session/list", json!({"cwd": "/project"})).await;
            let listed = receive(&mut output).await;
            assert!(
                listed["result"]["sessions"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|session| session["sessionId"] == "s1" && session["cwd"] == "/project")
            );
            rpc(
                &runtime,
                11,
                "session/list",
                json!({"cwd": "/other-project"}),
            )
            .await;
            let filtered = receive(&mut output).await;
            assert!(
                !filtered["result"]["sessions"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|session| session["sessionId"] == "s1")
            );
        }
        let session = runtime.sessions.lock().await["s1"].clone();
        assert!(
            matches!(session.read_event().await.unwrap(), ServerEvent::TextDelta { text } if text == "buffered")
        );
        server.await.unwrap();
    }
}

#[tokio::test]
async fn skills_reload_command_round_trips_without_a_provider_turn() {
    let (runtime, mut output, mut daemon, mut writer) = harness().await;
    let server = tokio::spawn(async move {
        let request: Request = serde_json::from_value(receive(&mut daemon).await).unwrap();
        let Request::ReloadSkills { id } = request else {
            panic!("expected reload")
        };
        send_event(
            &mut writer,
            ServerEvent::SkillsReloaded {
                id,
                skills: vec!["new-skill".into()],
                error: None,
            },
        )
        .await;
    });
    rpc(
        &runtime,
        9,
        "session/prompt",
        json!({"sessionId": "s1", "prompt": [{"type": "text", "text": "/skills reload"}]}),
    )
    .await;
    assert!(
        receive(&mut output).await["params"]["update"]["content"]["text"]
            .as_str()
            .unwrap()
            .contains("new-skill")
    );
    assert_eq!(
        receive(&mut output).await["result"]["stopReason"],
        "end_turn"
    );
    server.await.unwrap();
}

#[test]
fn worker_status_is_scoped_deduplicated_and_not_a_draft_protocol_extension() {
    let mut mapper = EventMapper::new("root".into(), AcpProfile::Standard);
    let event = |status| {
        serde_json::from_value(json!({"type": "swarm_status", "members": [
        {"session_id": "child", "report_back_to_session_id": "root", "status": status, "task_label": "review"},
        {"session_id": "other", "report_back_to_session_id": "unrelated", "status": status}
    ]})).unwrap()
    };
    let start = mapper.map_event(event("running"));
    assert_eq!(start.len(), 1);
    assert_eq!(start[0]["sessionUpdate"], "tool_call");
    assert_eq!(start[0]["toolCallId"], "swarm/child");
    assert!(mapper.map_event(event("running")).is_empty());
    let done = mapper.map_event(event("completed"));
    assert_eq!(done[0]["sessionUpdate"], "tool_call_update");
    assert_eq!(done[0]["status"], "completed");
    assert_eq!(mapper.map_event(event("failed"))[0]["status"], "failed");
}

#[test]
fn absent_effort_and_unknown_providers_never_advertise_fictitious_state() {
    let mut state = state();
    state.reasoning_effort = None;
    assert!(
        !session_config_options(&state)
            .iter()
            .any(|option| option["id"] == CONFIG_ID_EFFORT)
    );
    state.provider_name = Some("ollama".into());
    state.model = Some("llama3".into());
    assert_eq!(mode_options(&state).len(), 1);
}

fn state() -> SessionUiState {
    SessionUiState {
        provider_name: Some("openai".into()),
        model: Some("gpt-5.2".into()),
        available_models: vec!["gpt-5.2".into(), "gpt-5.2-codex".into()],
        reasoning_effort: Some("high".into()),
        ..SessionUiState::default()
    }
}

async fn harness() -> (
    AcpRuntime,
    BufReader<DuplexStream>,
    BufReader<ReadHalf>,
    WriteHalf,
) {
    let (output, capture) = tokio::io::duplex(128 * 1024);
    let mut runtime = AcpRuntime::new(AcpProfile::Standard, ProviderChoice::Openai, None, None);
    runtime.stdout = Arc::new(Mutex::new(Box::new(output)));
    let (client, daemon) = crate::transport::stream_pair().unwrap();
    let (reader, writer) = client.into_split();
    let session = DaemonSession::new("s1".into(), reader, writer, 1).with_ui_state(state());
    runtime
        .sessions
        .lock()
        .await
        .insert("s1".into(), Arc::new(session));
    let (reader, writer) = daemon.into_split();
    (
        runtime,
        BufReader::new(capture),
        BufReader::new(reader),
        writer,
    )
}

async fn receive(reader: &mut BufReader<impl tokio::io::AsyncRead + Unpin>) -> Value {
    let mut line = String::new();
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        reader.read_line(&mut line),
    )
    .await
    .unwrap()
    .unwrap();
    serde_json::from_str(&line).unwrap()
}

async fn send_event(writer: &mut WriteHalf, event: ServerEvent) {
    writer
        .write_all(format!("{}\n", serde_json::to_string(&event).unwrap()).as_bytes())
        .await
        .unwrap();
}

async fn rpc(runtime: &AcpRuntime, id: u64, method: &str, params: Value) {
    let line = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}).to_string();
    runtime
        .handle_message(JsonRpcMessage::parse(&line).unwrap())
        .await
        .unwrap();
}

fn option<'a>(response: &'a Value, id: &str) -> &'a Value {
    response["result"]["configOptions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|option| option["id"] == id)
        .unwrap()
}

#[tokio::test]
async fn config_controls_round_trip_native_requests_and_complete_acp_state() {
    let (runtime, mut output, mut daemon, mut writer) = harness().await;
    let server = tokio::spawn(async move {
        for expected in [
            "set_reasoning_effort",
            "set_reasoning_effort",
            "set_subagent_model",
            "set_compaction_mode",
            "set_model",
        ] {
            let request = receive(&mut daemon).await;
            let decoded: Request = serde_json::from_value(request.clone()).unwrap();
            assert_eq!(request["type"], expected);
            let id = decoded.id();
            let event = match decoded {
                Request::SetReasoningEffort { effort, .. } => ServerEvent::ReasoningEffortChanged { id, effort: Some(effort), error: None },
                Request::SetSubagentModel { model, .. } => { assert_eq!(model.as_deref(), Some("gpt-5.2-codex")); ServerEvent::Done { id } },
                Request::SetCompactionMode { mode, .. } => ServerEvent::CompactionModeChanged { id, mode, error: None },
                Request::SetModel { model, .. } => serde_json::from_value(json!({"type": "model_changed", "id": id, "model": model, "provider_name": "openai", "reasoning_effort": "low"})).unwrap(),
                _ => panic!("unexpected native request"),
            };
            send_event(&mut writer, event).await;
        }
    });
    for (id, method, params, changed_id, expected) in [
        (
            1,
            "session/set_mode",
            json!({"sessionId": "s1", "modeId": "swarm-deep"}),
            CONFIG_ID_EFFORT,
            "swarm-deep",
        ),
        (
            2,
            "session/set_config_option",
            json!({"sessionId": "s1", "configId": "mode", "value": "solo"}),
            CONFIG_ID_EFFORT,
            "medium",
        ),
        (
            3,
            "session/set_config_option",
            json!({"sessionId": "s1", "configId": "subagent_model", "value": "gpt-5.2-codex"}),
            CONFIG_ID_SUBAGENT_MODEL,
            "gpt-5.2-codex",
        ),
        (
            4,
            "session/set_config_option",
            json!({"sessionId": "s1", "configId": "compaction_mode", "value": "semantic"}),
            CONFIG_ID_COMPACTION,
            "semantic",
        ),
        (
            5,
            "session/set_model",
            json!({"sessionId": "s1", "modelId": "gpt-5.2-codex"}),
            CONFIG_ID_EFFORT,
            "low",
        ),
    ] {
        rpc(&runtime, id, method, params).await;
        let response = receive(&mut output).await;
        assert_eq!(response["id"], id);
        assert_eq!(
            response["result"]["configOptions"]
                .as_array()
                .unwrap()
                .len(),
            5
        );
        assert_eq!(option(&response, changed_id)["currentValue"], expected);
        let mode = receive(&mut output).await;
        assert_eq!(
            mode["params"]["update"]["sessionUpdate"],
            "current_mode_update"
        );
        let config = receive(&mut output).await;
        assert_eq!(
            config["params"]["update"]["configOptions"],
            response["result"]["configOptions"]
        );
    }
    server.await.unwrap();
    rpc(
        &runtime,
        6,
        "session/set_config_option",
        json!({"sessionId": "s1", "configId": "reasoning_effort", "value": "invented"}),
    )
    .await;
    assert_eq!(
        receive(&mut output).await["error"]["code"],
        JSONRPC_INVALID_PARAMS
    );
}

#[tokio::test]
async fn subagent_prompt_streams_tools_thoughts_and_completion_over_acp() {
    let (runtime, mut output, mut daemon, mut writer) = harness().await;
    let server = tokio::spawn(async move {
        let request: Request = serde_json::from_value(receive(&mut daemon).await).unwrap();
        let Request::RunSubagent {
            id,
            prompt,
            subagent_type,
            model,
            session_id,
        } = request
        else {
            panic!("expected native subagent launch")
        };
        assert_eq!(prompt, "inspect the code");
        assert_eq!(subagent_type, "general");
        assert!(model.is_none() && session_id.is_none());
        for event in [
            ServerEvent::ReasoningDelta {
                text: "Inspecting".into(),
            },
            ServerEvent::ToolStart {
                id: "child-call".into(),
                name: "subagent".into(),
            },
            ServerEvent::ToolDone {
                id: "child-call".into(),
                name: "subagent".into(),
                output: "checked".into(),
                error: None,
            },
            ServerEvent::Done { id },
        ] {
            send_event(&mut writer, event).await
        }
    });
    rpc(&runtime, 8, "session/prompt", json!({"sessionId": "s1", "prompt": [{"type": "text", "text": "/subagent inspect the code"}]})).await;
    assert_eq!(
        receive(&mut output).await["params"]["update"]["sessionUpdate"],
        "agent_thought_chunk"
    );
    assert_eq!(
        receive(&mut output).await["params"]["update"]["sessionUpdate"],
        "tool_call"
    );
    assert_eq!(
        receive(&mut output).await["params"]["update"]["status"],
        "completed"
    );
    assert_eq!(
        receive(&mut output).await,
        json!({"jsonrpc": "2.0", "id": 8, "result": {"stopReason": "end_turn"}})
    );
    server.await.unwrap();
    assert!(
        !runtime.sessions.lock().await["s1"]
            .prompt_running
            .load(Ordering::SeqCst)
    );
}

#[test]
fn session_list_keyset_pagination_and_validation() {
    let sessions: Vec<Value> = (0..120).map(|id| json!({"sessionId": format!("s{id:03}"), "updatedAt": "2026-10-07T00:00:00+00:00", "cwd": "/project"})).collect();
    let first = session_list_page(sessions.clone(), None);
    assert_eq!(first["sessions"].as_array().unwrap().len(), 50);
    let second = session_list_page(sessions.clone(), first["nextCursor"].as_str());
    assert_eq!(second["sessions"][0]["sessionId"], "s050");
    let third = session_list_page(sessions, second["nextCursor"].as_str());
    assert_eq!(third["sessions"].as_array().unwrap().len(), 20);
    assert!(third.get("nextCursor").is_none());
    assert!(list_params(&json!({"cursor": first["nextCursor"]})).is_ok());
    for params in [
        json!({"cwd": "relative"}),
        json!({"cursor": "bad"}),
        json!({"cursor": 5}),
    ] {
        assert!(list_params(&params).is_err())
    }
}

#[tokio::test]
async fn history_replay_preserves_tool_identity_and_images_without_system_leaks() {
    let (runtime, mut output, _, _) = harness().await;
    let history = serde_json::from_value(json!([
        {"role": "system", "content": "hidden"},
        {"role": "user", "content": "inspect"},
        {"role": "tool", "content": "file content", "tool_data": {"id": "t1", "name": "read", "input": {"file_path": "file.rs"}}},
        {"role": "assistant", "content": "done"}
    ])).unwrap();
    let images = serde_json::from_value(json!([{"media_type": "image/png", "data": "aGVsbG8=", "label": null, "source": {"kind": "user_input"}, "anchor": {"kind": "user_prompt", "ordinal": 0}}])).unwrap();
    runtime.replay_history("s1", history, images).await.unwrap();
    assert_eq!(
        receive(&mut output).await["params"]["update"]["content"]["text"],
        "inspect"
    );
    assert_eq!(
        receive(&mut output).await["params"]["update"]["content"]["type"],
        "image"
    );
    let tool = receive(&mut output).await;
    assert_eq!(tool["params"]["update"]["toolCallId"], "t1");
    assert_eq!(tool["params"]["update"]["rawInput"]["file_path"], "file.rs");
    assert_eq!(
        receive(&mut output).await["params"]["update"]["content"]["text"],
        "done"
    );
}
