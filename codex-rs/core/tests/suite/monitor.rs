//! Real-process witnesses of idle wake, tagged records, exit diagnostics, and cleanup.

use std::time::Duration;

use anyhow::Context;
use anyhow::Result;
use codex_core::TurnInputRequest;
use codex_protocol::models::PermissionProfile;
use codex_protocol::protocol::EventMsg;
use codex_protocol::user_input::UserInput;
use core_test_support::responses;
use core_test_support::responses::ResponseMock;
use core_test_support::skip_if_no_network;
use core_test_support::skip_if_sandbox;
use core_test_support::test_codex::TestCodex;
use core_test_support::test_codex::test_codex;
use pretty_assertions::assert_eq;
use serde_json::json;

struct Watch {
    _server: wiremock::MockServer,
    test: TestCodex,
    mock: ResponseMock,
}

impl Watch {
    async fn start(body: &str, wake_responses: Vec<String>) -> Result<Self> {
        let server = responses::start_mock_server().await;
        let test = test_codex()
            .with_config(|config| {
                config
                    .permissions
                    .set_permission_profile(PermissionProfile::Disabled)
                    .expect("fixture file barriers");
            })
            .build(&server)
            .await?;
        let gate = test.workspace_path("emit");
        let command = format!(
            "while test ! -f '{}'; do sleep 0.02; done; {body}",
            gate.display()
        );
        let mut replies = vec![
            responses::sse(vec![
                responses::ev_response_created("start"),
                responses::ev_function_call(
                    "start",
                    "monitor",
                    &json!({
                        "action":"start", "command":command, "description":"signal watch"
                    })
                    .to_string(),
                ),
                responses::ev_completed("start"),
            ]),
            responses::sse(vec![
                responses::ev_assistant_message("watching", "watching"),
                responses::ev_completed("watching"),
            ]),
        ];
        replies.extend(wake_responses);
        let mock = responses::mount_sse_sequence(&server, replies).await;
        test.codex
            .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
                text: "watch for the signal".into(),
                text_elements: vec![],
            }]))
            .await?;
        let watch = Self {
            _server: server,
            test,
            mock,
        };
        watch.idle().await?;
        assert!(
            watch
                .mock
                .function_call_output_text("start")
                .context("start result")?
                .contains("Started monitor mon_")
        );
        assert_eq!(watch.mock.requests().len(), 2);
        Ok(watch)
    }

    async fn idle(&self) -> Result<()> {
        tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                if matches!(
                    self.test.codex.next_event().await?.msg,
                    EventMsg::TurnComplete(_)
                ) {
                    break;
                }
            }
            // TurnComplete precedes clearing active_turn. An empty injection
            // observes that lock without submitting any user input or new turn.
            while self.test.codex.inject_if_running(vec![]).await.is_ok() {
                tokio::task::yield_now().await;
            }
            Ok::<(), anyhow::Error>(())
        })
        .await?
    }

    fn release(&self, name: &str) -> Result<()> {
        std::fs::write(self.test.workspace_path(name), b"go")?;
        Ok(())
    }

    async fn output(&self, call_id: &str) -> Result<String> {
        tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                if let Some(output) = self.mock.function_call_output_text(call_id) {
                    return output;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .context("tool output timeout")
    }

    fn notifications(&self) -> Vec<String> {
        self.mock
            .requests()
            .iter()
            .flat_map(|request| request.message_input_texts("user"))
            .filter(|text| text.contains("[signal watch]"))
            .collect()
    }

    async fn close(self) -> Result<()> {
        self.test.codex.shutdown_and_wait().await?;
        Ok(())
    }
}

fn acknowledgement() -> Vec<String> {
    vec![responses::sse(vec![
        responses::ev_assistant_message("saw", "saw it"),
        responses::ev_completed("saw"),
    ])]
}

async fn tagged_wake(body: &str, expected: &str) -> Result<()> {
    let watch = Watch::start(body, acknowledgement()).await?;
    // Only the filesystem gate changes after observed idle; the monitor itself
    // must start the next turn. Holding exit excludes an exit-only false positive.
    watch.release("emit")?;
    watch.idle().await?;
    assert_eq!(watch.mock.requests().len(), 3);
    let notifications = watch.notifications();
    assert_eq!(notifications.len(), 1);
    assert!(notifications[0].contains(expected), "{notifications:?}");
    assert!(!notifications[0].contains("MONITOR-NOTICE: exit"));
    watch.close().await
}

// Removing automatic admission fails even though the process emits its record.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn monitor_stdout_output_wakes_agent() -> Result<()> {
    skip_if_no_network!(Ok(()));
    skip_if_sandbox!(Ok(()));
    tagged_wake(
        "printf 'MONITOR_STDOUT\\n'; sleep 60",
        "<stdout>\nMONITOR_STDOUT\n</stdout>",
    )
    .await
}

// Merging stderr into stdout fails the tagged-block assertion.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn monitor_stderr_output_wakes_agent() -> Result<()> {
    skip_if_no_network!(Ok(()));
    skip_if_sandbox!(Ok(()));
    tagged_wake(
        "printf 'MONITOR_STDERR\\n' >&2; sleep 60",
        "<stderr>\nMONITOR_STDERR\n</stderr>",
    )
    .await
}

// Publishing partial bytes as a record fails both the quiet and exit-only checks.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn monitor_delivers_unterminated_final_line() -> Result<()> {
    skip_if_no_network!(Ok(()));
    skip_if_sandbox!(Ok(()));
    let watch = Watch::start("printf MONITOR_PARTIAL; touch partial-written; while test ! -f finish; do sleep 0.02; done", acknowledgement()).await?;
    watch.release("emit")?;
    tokio::time::timeout(Duration::from_secs(10), async {
        while !watch.test.workspace_path("partial-written").exists() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await?;
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(
        watch.mock.requests().len(),
        2,
        "partial must not wake before exit"
    );
    assert!(watch.test.codex.inject_if_running(vec![]).await.is_err());
    watch.release("finish")?;
    watch.idle().await?;
    let notifications = watch.notifications();
    assert_eq!(notifications.len(), 1);
    assert!(
        notifications[0].contains("MONITOR-NOTICE: exit partial-stdout=\"MONITOR_PARTIAL\""),
        "{notifications:?}"
    );
    assert!(
        !notifications[0].contains("<stdout>"),
        "partial became a row"
    );
    watch.close().await
}

// Losing the empty-process exit notice leaves the observed-idle session asleep.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn monitor_delivers_exit_notice_when_command_ends() -> Result<()> {
    skip_if_no_network!(Ok(()));
    skip_if_sandbox!(Ok(()));
    let watch = Watch::start("true", acknowledgement()).await?;
    watch.release("emit")?;
    watch.idle().await?;
    let notifications = watch.notifications();
    assert_eq!(notifications.len(), 1);
    assert!(
        notifications[0].contains("MONITOR-NOTICE: exit"),
        "{notifications:?}"
    );
    assert!(!notifications[0].contains("<stdout>") && !notifications[0].contains("<stderr>"));
    watch.close().await
}

// Omitting self-prune fails the empty list; leaking the exited watch's permit
// prevents admission of all eight replacements even when the registry is empty.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn monitor_self_prunes_from_registry_when_command_exits() -> Result<()> {
    skip_if_no_network!(Ok(()));
    skip_if_sandbox!(Ok(()));
    let mut replacements = vec![responses::ev_response_created("replace")];
    for n in 0..8 {
        replacements.push(responses::ev_function_call(
            &format!("replacement-{n}"),
            "monitor",
            &json!({
                "action":"start", "command":"sleep 60", "description":format!("replacement {n}")
            })
            .to_string(),
        ));
    }
    replacements.push(responses::ev_completed("replace"));
    let watch = Watch::start(
        "true",
        vec![
            responses::sse(vec![
                responses::ev_response_created("list"),
                responses::ev_function_call(
                    "empty-list",
                    "monitor",
                    &json!({"action":"list"}).to_string(),
                ),
                responses::ev_completed("list"),
            ]),
            responses::sse(replacements),
            responses::sse(vec![
                responses::ev_response_created("full"),
                responses::ev_function_call(
                    "full-list",
                    "monitor",
                    &json!({"action":"list"}).to_string(),
                ),
                responses::ev_completed("full"),
            ]),
            acknowledgement().remove(0),
        ],
    )
    .await?;
    watch.release("emit")?;
    watch.idle().await?;
    assert_eq!(watch.output("empty-list").await?, "No active monitors.");
    assert!(
        watch
            .notifications()
            .iter()
            .any(|text| text.contains("MONITOR-NOTICE: exit"))
    );
    let mut ids = std::collections::HashSet::new();
    for n in 0..8 {
        let output = watch.output(&format!("replacement-{n}")).await?;
        assert!(output.contains("Started monitor mon_"), "{output}");
        ids.insert(
            output
                .split_whitespace()
                .nth(2)
                .context("replacement id")?
                .trim_end_matches(':')
                .to_owned(),
        );
    }
    assert_eq!(ids.len(), 8);
    let listed = watch.output("full-list").await?;
    assert_eq!(listed.lines().count(), 8);
    for id in ids {
        assert!(listed.contains(&id), "{listed}");
    }
    watch.close().await
}

// The historical name is retained, but truncation is forbidden: the entire
// oversize row is lost once, then framing resumes only after LF.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn monitor_truncates_a_newline_free_flood() -> Result<()> {
    skip_if_no_network!(Ok(()));
    skip_if_sandbox!(Ok(()));
    let watch = Watch::start("printf OVERSIZE_PREFIX; head -c 200000 /dev/zero | tr '\\0' x; printf 'OVERSIZE_SUFFIX\\nAFTER_LOSS\\n'", acknowledgement()).await?;
    watch.release("emit")?;
    watch.idle().await?;
    let notifications = watch.notifications();
    let text = notifications.join("\n");
    assert!(
        text.contains("MONITOR-NOTICE: loss oversize records=1;"),
        "{text}"
    );
    assert!(text.contains("<stdout>\nAFTER_LOSS\n</stdout>"), "{text}");
    assert!(
        !text.contains("OVERSIZE_PREFIX")
            && !text.contains("OVERSIZE_SUFFIX")
            && !text.contains("xxxxxxxx"),
        "oversize row leaked: {text}"
    );
    assert!(!text.contains("line truncated"));
    watch.close().await
}
