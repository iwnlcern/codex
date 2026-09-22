//! Public surface contract for the `monitor` tool.

use std::time::Duration;

use anyhow::Context;
use anyhow::Result;
use codex_core::TurnInputRequest;
use codex_features::Feature;
use codex_protocol::protocol::EventMsg;
use codex_protocol::user_input::UserInput;
use core_test_support::responses;
use core_test_support::skip_if_no_network;
use core_test_support::skip_if_sandbox;
use core_test_support::test_codex::TestCodex;
use core_test_support::test_codex::test_codex;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;

struct Fixture {
    server: wiremock::MockServer,
    test: TestCodex,
}

impl Fixture {
    async fn new() -> Result<Self> {
        let server = responses::start_mock_server().await;
        let test = test_codex().build(&server).await?;
        Ok(Self { server, test })
    }

    async fn call(&self, call_id: &str, args: Value) -> Result<String> {
        let response = responses::mount_sse_sequence(
            &self.server,
            vec![
                responses::sse(vec![
                    responses::ev_response_created("r1"),
                    responses::ev_function_call(call_id, "monitor", &args.to_string()),
                    responses::ev_completed("r1"),
                ]),
                responses::sse(vec![
                    responses::ev_response_created("r2"),
                    responses::ev_completed("r2"),
                ]),
            ],
        )
        .await;
        self.test
            .codex
            .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
                text: "exercise monitor surface".into(),
                text_elements: Vec::new(),
            }]))
            .await?;
        tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                if matches!(
                    self.test.codex.next_event().await?.msg,
                    EventMsg::TurnComplete(_)
                ) {
                    return Ok::<(), anyhow::Error>(());
                }
            }
        })
        .await??;
        response
            .function_call_output_text(call_id)
            .context("monitor tool output")
    }

    async fn close(self) -> Result<()> {
        self.test.codex.shutdown_and_wait().await?;
        Ok(())
    }
}

async fn advertised_tools(monitor_enabled: bool) -> Result<Vec<Value>> {
    let server = responses::start_mock_server().await;
    let response = responses::mount_sse_once(
        &server,
        responses::sse(vec![
            responses::ev_response_created("r1"),
            responses::ev_completed("r1"),
        ]),
    )
    .await;
    let mut builder = test_codex().with_config(move |config| {
        if monitor_enabled {
            config
                .features
                .enable(Feature::Monitor)
                .expect("enable monitor feature");
        } else {
            config
                .features
                .disable(Feature::Monitor)
                .expect("disable monitor feature");
        }
    });
    let test = builder.build(&server).await?;
    test.codex
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: "show available tools".into(),
            text_elements: Vec::new(),
        }]))
        .await?;
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            if matches!(
                test.codex.next_event().await?.msg,
                EventMsg::TurnComplete(_)
            ) {
                return Ok::<(), anyhow::Error>(());
            }
        }
    })
    .await??;
    let tools = response.single_request().body_json()["tools"]
        .as_array()
        .context("request tools array")?
        .clone();
    test.codex.shutdown_and_wait().await?;
    Ok(tools)
}

fn tool_name(tool: &Value) -> Option<&str> {
    tool.get("name")
        .and_then(Value::as_str)
        .or_else(|| tool.pointer("/function/name").and_then(Value::as_str))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tool_spec_matches_frozen_surface() -> Result<()> {
    skip_if_no_network!(Ok(()));
    let tools = advertised_tools(true).await?;
    let monitor = tools
        .iter()
        .find(|tool| tool_name(tool) == Some("monitor"))
        .context("advertised monitor tool")?;
    let parameters = monitor
        .get("parameters")
        .or_else(|| monitor.pointer("/function/parameters"))
        .context("monitor parameters")?;
    let properties = parameters["properties"]
        .as_object()
        .context("monitor parameter properties")?;
    assert_eq!(
        properties.keys().map(String::as_str).collect::<Vec<_>>(),
        vec!["action", "command", "description", "id"]
    );
    assert_eq!(
        properties["action"]["enum"],
        json!(["start", "stop", "list"])
    );
    assert_eq!(parameters["required"], json!(["action"]));
    assert_eq!(parameters["additionalProperties"], json!(false));
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn description_over_256_bytes_rejected() -> Result<()> {
    skip_if_no_network!(Ok(()));
    skip_if_sandbox!(Ok(()));
    let fixture = Fixture::new().await?;
    let output = fixture
        .call(
            "overlong-description",
            json!({
                "action": "start",
                "command": "sleep 60",
                "description": "é".repeat(129),
            }),
        )
        .await?;
    assert!(
        output.contains("description")
            && output.contains("256")
            && output.to_ascii_lowercase().contains("byte"),
        "unexpected rejection: {output}"
    );
    fixture.close().await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn list_shows_started_watch_with_command() -> Result<()> {
    skip_if_no_network!(Ok(()));
    skip_if_sandbox!(Ok(()));
    let fixture = Fixture::new().await?;
    let started = fixture
        .call(
            "start-watch",
            json!({
                "action": "start",
                "command": "sleep 60",
                "description": "surface witness",
            }),
        )
        .await?;
    let id = started
        .split_whitespace()
        .nth(2)
        .context("monitor id")?
        .trim_end_matches(':');
    let first = fixture
        .call("first-list", json!({"action": "list"}))
        .await?;
    tokio::time::sleep(Duration::from_millis(10)).await;
    let second = fixture
        .call("second-list", json!({"action": "list"}))
        .await?;
    assert!(first.contains(id), "missing id: {first}");
    assert!(first.contains("sleep 60"), "missing command: {first}");
    assert!(
        first.contains("surface witness"),
        "missing description: {first}"
    );
    assert!(first.contains("started-at="), "missing started-at: {first}");
    assert_eq!(first, second, "started-at must be stored, not recomputed");
    let stopped = fixture
        .call("stop-watch", json!({"action": "stop", "id": id}))
        .await?;
    assert!(stopped.contains("Stopped monitor"), "{stopped}");
    fixture.close().await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stop_unknown_id_reports_not_found() -> Result<()> {
    skip_if_no_network!(Ok(()));
    skip_if_sandbox!(Ok(()));
    let fixture = Fixture::new().await?;
    let output = fixture
        .call(
            "stop-unknown",
            json!({"action": "stop", "id": "mon_does_not_exist"}),
        )
        .await?;
    assert_eq!(output, "No active monitor with id mon_does_not_exist.");
    fixture.close().await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tool_absent_when_feature_disabled() -> Result<()> {
    skip_if_no_network!(Ok(()));
    let tools = advertised_tools(false).await?;
    assert!(
        tools.iter().all(|tool| tool_name(tool) != Some("monitor")),
        "monitor must be absent from the model-visible tool plan: {tools:?}"
    );
    Ok(())
}
