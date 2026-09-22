//! Model-visible witnesses of the monitor-owned process route.
use anyhow::Context;
use anyhow::Result;
use codex_core::TurnInputRequest;
use codex_protocol::config_types::CollaborationMode;
use codex_protocol::config_types::ModeKind;
use codex_protocol::config_types::Settings;
use codex_protocol::models::PermissionProfile;
use codex_protocol::protocol::AskForApproval;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::ThreadSettingsOverrides;
use codex_protocol::user_input::UserInput;
use core_test_support::responses::ResponseMock;
use core_test_support::responses::{self};
use core_test_support::skip_if_no_network;
use core_test_support::skip_if_sandbox;
use core_test_support::test_codex::TestCodex;
use core_test_support::test_codex::test_codex;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;

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
    async fn writable() -> Result<Self> {
        let server = responses::start_mock_server().await;
        let test = test_codex()
            .with_config(|config| {
                config
                    .permissions
                    .set_permission_profile(PermissionProfile::Disabled)
                    .expect("fixture PID writes");
            })
            .build(&server)
            .await?;
        Ok(Self { server, test })
    }
    async fn call(&self, name: &str, args: Value) -> Result<(String, ResponseMock)> {
        let (mut outputs, mock) = self.call_many(vec![(name, args)]).await?;
        Ok((outputs.remove(0), mock))
    }
    async fn call_many(&self, calls: Vec<(&str, Value)>) -> Result<(Vec<String>, ResponseMock)> {
        let ids: Vec<_> = calls
            .iter()
            .map(|_| uuid::Uuid::new_v4().to_string())
            .collect();
        let mut events = vec![responses::ev_response_created("r1")];
        events.extend(
            calls
                .iter()
                .zip(&ids)
                .map(|((name, args), id)| responses::ev_function_call(id, name, &args.to_string())),
        );
        events.push(responses::ev_completed("r1"));
        let mock = responses::mount_sse_sequence(
            &self.server,
            vec![
                responses::sse(events),
                responses::sse(vec![
                    responses::ev_response_created("r2"),
                    responses::ev_completed("r2"),
                ]),
            ],
        )
        .await;
        let request = TurnInputRequest::user_input(vec![UserInput::Text {
            text: "exercise monitor route".into(),
            text_elements: vec![],
        }])
        .with_thread_settings(ThreadSettingsOverrides {
            collaboration_mode: Some(CollaborationMode {
                mode: ModeKind::Plan,
                settings: Settings {
                    model: self.test.session_configured.model.clone(),
                    reasoning_effort: None,
                    developer_instructions: None,
                },
            }),
            ..Default::default()
        });
        // TurnComplete is sent before active_turn is cleared. Wait for actual
        // admission instead of steering a task that is still finishing.
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if matches!(
                    self.test.codex.start_turn_if_idle(request.clone()).await?,
                    codex_protocol::turn_input::StartIfIdleSubmission::Started { .. }
                ) {
                    break;
                }
                tokio::task::yield_now().await;
            }
            Ok::<(), anyhow::Error>(())
        })
        .await??;
        tokio::time::timeout(Duration::from_secs(30), async {
            let mut recent = std::collections::VecDeque::new();
            loop {
                let event = match tokio::time::timeout(Duration::from_secs(10), self.test.codex.next_event()).await {
                    Ok(event) => event.expect("event stream").msg,
                    Err(error) => {
                        let tcp = tokio::time::timeout(Duration::from_millis(500), tokio::net::TcpStream::connect(self.server.address())).await.map(|result| result.map(|_| ()));
                        let http = tokio::time::timeout(Duration::from_millis(500), async {
                            use tokio::io::{AsyncReadExt, AsyncWriteExt};
                            let mut stream = tokio::net::TcpStream::connect(self.server.address()).await?;
                            stream.write_all(b"GET /models HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n").await?;
                            let mut bytes = vec![0; 128];
                            let count = stream.read(&mut bytes).await?;
                            Ok::<_, std::io::Error>(String::from_utf8_lossy(&bytes[..count]).into_owned())
                        }).await;
                        #[cfg(unix)] {
                            let mut limit = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
                            let result = unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) };
                            eprintln!("test process nofile result={result} soft={} hard={}", limit.rlim_cur, limit.rlim_max);
                        }
                        let characteristics: Vec<_> = mock.requests().iter().map(|request| (request.body_json().to_string().len(), request.header("content-encoding"), request.header("content-length"))).collect();
                        panic!("event timeout {error}: calls={calls:?} server={} tcp={tcp:?} http={http:?} requests={} characteristics={characteristics:?} recent={recent:?}", self.server.uri(), mock.requests().len());
                    }
                };
                recent.push_back(format!("{event:?}"));
                if recent.len() > 6 { recent.pop_front(); }
                match event {
                    EventMsg::ExecApprovalRequest(approval) => {
                        self.test.codex.submit(codex_protocol::protocol::Op::ExecApproval {
                            id: approval.effective_approval_id(), turn_id: Some(approval.turn_id),
                            decision: codex_protocol::protocol::ReviewDecision::Approved,
                        }).await?;
                    }
                    EventMsg::TurnComplete(_) => break,
                    _ => {}
                }
            }
            Ok::<(), anyhow::Error>(())
        }).await??;
        let outputs = ids
            .iter()
            .map(|id| mock.function_call_output_text(id).context("tool output"))
            .collect::<Result<Vec<_>>>()?;
        Ok((outputs, mock))
    }
    async fn start(&self, command: &str) -> Result<String> {
        let (output, _) = self
            .call(
                "monitor",
                json!({"action":"start","command":command,"description":"pool witness"}),
            )
            .await?;
        assert!(output.contains("Started monitor mon_"), "{output}");
        Ok(output
            .split_whitespace()
            .nth(2)
            .context("monitor id")?
            .trim_end_matches(':')
            .into())
    }
    async fn drained(&self) -> Result<String> {
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let (output, mock) = self.call("monitor", json!({"action":"list"})).await?;
                let history = mock
                    .last_request()
                    .context("history request")?
                    .message_input_texts("user")
                    .join("\n");
                if output == "No active monitors." && history.contains("MONITOR-NOTICE: exit") {
                    return Ok(history);
                }
                tokio::task::yield_now().await;
            }
        })
        .await?
    }
    async fn close(self) -> Result<()> {
        self.test.codex.shutdown_and_wait().await?;
        Ok(())
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn immediate_output_and_exit_delivers_and_releases() -> Result<()> {
    skip_if_no_network!(Ok(()));
    skip_if_sandbox!(Ok(()));
    let f = Fixture::new().await?;
    f.start("printf 'IMMEDIATE_ROW\n'").await?;
    let history = f.drained().await?;
    assert!(
        history.contains("<stdout>\nIMMEDIATE_ROW\n</stdout>"),
        "{history}"
    );
    f.close().await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tagged_mode_keeps_streams_separate() -> Result<()> {
    skip_if_no_network!(Ok(()));
    skip_if_sandbox!(Ok(()));
    let f = Fixture::new().await?;
    f.start("printf 'OUT_ROW\n'; printf 'ERR_ROW\n' >&2")
        .await?;
    let history = f.drained().await?;
    assert!(
        history.contains("<stdout>\nOUT_ROW\n</stdout>"),
        "{history}"
    );
    assert!(
        history.contains("<stderr>\nERR_ROW\n</stderr>"),
        "{history}"
    );
    f.close().await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tagged_output_task_fills_diagnostic_buffer_and_closes() -> Result<()> {
    skip_if_no_network!(Ok(()));
    skip_if_sandbox!(Ok(()));
    let f = Fixture::new().await?;
    f.start("printf 'COMPLETE\npartial-out'; printf 'partial-err' >&2")
        .await?;
    let history = f.drained().await?;
    assert!(
        history.contains("<stdout>\nCOMPLETE\n</stdout>"),
        "{history}"
    );
    assert!(
        history.contains("partial-stdout=\"partial-out\""),
        "{history}"
    );
    assert!(
        history.contains("partial-stderr=\"partial-err\""),
        "{history}"
    );
    assert!(!history.contains("<stdout>\npartial-out\n"));
    // The raw unit saturation controls assert the diagnostic snapshot and
    // output_closed directly; this witness observes their final-drain consumer.
    f.close().await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn combined_mode_unchanged_for_ordinary_exec() -> Result<()> {
    skip_if_no_network!(Ok(()));
    skip_if_sandbox!(Ok(()));
    let f = Fixture::new().await?;
    let (output, _) = f.call("exec_command", json!({"cmd":"printf 'ordinary-out\n'; printf 'ordinary-err\n' >&2", "yield_time_ms":1000})).await?;
    assert!(
        output.contains("ordinary-out") && output.contains("ordinary-err"),
        "{output}"
    );
    assert!(!output.contains("<stdout>") && !output.contains("MONITOR-NOTICE"));
    assert_eq!(
        f.call("monitor", json!({"action":"list"})).await?.0,
        "No active monitors."
    );
    f.close().await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn explicit_stop_cleans_record() -> Result<()> {
    skip_if_no_network!(Ok(()));
    skip_if_sandbox!(Ok(()));
    let f = Fixture::writable().await?;
    let pidfile = f.test.workspace_path("monitor.pid");
    let id = f
        .start(&format!("echo $$ > '{}'; exec sleep 60", pidfile.display()))
        .await?;
    let pid = core_test_support::process::wait_for_pid_file(&pidfile).await?;
    assert!(core_test_support::process::process_is_alive(&pid)?);
    let output = f.call("monitor", json!({"action":"stop","id":id})).await?.0;
    assert!(output.contains("Stopped monitor"));
    core_test_support::process::wait_for_process_exit(&pid).await?;
    assert_eq!(
        f.call("monitor", json!({"action":"list"})).await?.0,
        "No active monitors."
    );
    f.close().await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shutdown_with_three_monitors_leaves_nothing() -> Result<()> {
    skip_if_no_network!(Ok(()));
    skip_if_sandbox!(Ok(()));
    let f = Fixture::writable().await?;
    let mut pids = Vec::new();
    for n in 0..3 {
        let path = f.test.workspace_path(format!("monitor-{n}.pid"));
        f.start(&format!("echo $$ > '{}'; exec sleep 60", path.display()))
            .await?;
        pids.push(core_test_support::process::wait_for_pid_file(&path).await?);
    }
    let listed = f.call("monitor", json!({"action":"list"})).await?.0;
    assert_eq!(listed.lines().count(), 3);
    f.close().await?;
    for pid in pids {
        core_test_support::process::wait_for_process_exit(&pid).await?;
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn monitor_child_env_has_codex_thread_id_and_policy() -> Result<()> {
    skip_if_no_network!(Ok(()));
    skip_if_sandbox!(Ok(()));
    let f = Fixture::new().await?;
    f.start("printf 'THREAD=%s\nPOLICY=%s\n' \"$CODEX_THREAD_ID\" \"$CODEX_PERMISSION_PROFILE\"")
        .await?;
    let history = f.drained().await?;
    assert!(
        history.contains(&format!(
            "THREAD={}\n",
            f.test.session_configured.session_id
        )),
        "{history}"
    );
    assert!(
        history.contains("POLICY=") && !history.contains("POLICY=\n"),
        "{history}"
    );
    f.close().await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sixty_four_ordinary_processes_do_not_evict_or_count_a_monitor() -> Result<()> {
    let _ = tracing_subscriber::fmt()
        .with_env_filter("codex_core=debug,codex_client=trace,reqwest=debug,hyper_util=debug")
        .with_test_writer()
        .try_init();
    skip_if_no_network!(Ok(()));
    skip_if_sandbox!(Ok(()));
    let f = Fixture::writable().await?;
    let files: Vec<_> = (0..64)
        .map(|n| f.test.workspace_path(format!("ordinary-{n}.pid")))
        .collect();
    let calls = files.iter().map(|file| ("exec_command", json!({"cmd":format!("echo $$ > '{}'; exec sleep 120", file.display()),"yield_time_ms":50}))).collect();
    let (outputs, _) = f.call_many(calls).await?;
    assert_eq!(outputs.len(), 64);
    let mut pids = Vec::new();
    for (output, file) in outputs.iter().zip(&files) {
        assert!(
            output.contains("Process running with session ID"),
            "{output}"
        );
        let pid = core_test_support::process::wait_for_pid_file(file).await?;
        assert!(
            core_test_support::process::process_is_alive(&pid)?,
            "ordinary process {pid} must be live before monitor admission"
        );
        pids.push(pid);
    }
    assert_eq!(
        pids.iter().collect::<std::collections::HashSet<_>>().len(),
        64,
        "64 distinct ordinary children must be live before monitor admission"
    );
    let id = f.start("exec sleep 120").await?;
    for pid in &pids {
        assert!(
            core_test_support::process::process_is_alive(pid)?,
            "monitor evicted ordinary process {pid}"
        );
    }
    let listed = f.call("monitor", json!({"action":"list"})).await?.0;
    assert!(listed.contains(&id));
    // Saturating the ordinary store again may evict an ordinary entry, never the monitor.
    f.call(
        "exec_command",
        json!({"cmd":"exec sleep 120","yield_time_ms":50}),
    )
    .await?;
    assert!(
        f.call("monitor", json!({"action":"list"}))
            .await?
            .0
            .contains(&id)
    );
    f.close().await?;
    for pid in pids {
        core_test_support::process::wait_for_process_exit(&pid).await?;
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sandbox_denial_retry_records_only_final_attempt() -> Result<()> {
    skip_if_no_network!(Ok(()));
    skip_if_sandbox!(Ok(()));
    let server = responses::start_mock_server().await;
    let test = test_codex()
        .with_config(|config| {
            config.permissions.approval_policy = codex_config::Constrained::allow_any(
                AskForApproval::Granular(codex_protocol::protocol::GranularApprovalConfig {
                    sandbox_approval: true,
                    rules: true,
                    skill_approval: true,
                    request_permissions: true,
                    mcp_elicitations: true,
                }),
            );
            config
                .permissions
                .set_permission_profile(PermissionProfile::workspace_write())
                .expect("sandbox policy");
        })
        .build(&server)
        .await?;
    let f = Fixture { server, test };
    let marker = f.test.workspace_path("attempts");
    f.start(&format!("if test ! -f '{}'; then echo first > '{}'; printf 'FIRST_ATTEMPT permission denied\n'; exit 1; else echo second >> '{}'; printf 'FINAL_ATTEMPT\n'; fi", marker.display(), marker.display(), marker.display())).await?;
    let history = f.drained().await?;
    assert_eq!(std::fs::read_to_string(marker)?, "first\nsecond\n");
    assert!(
        history.contains("<stdout>\nFINAL_ATTEMPT\n</stdout>"),
        "{history}"
    );
    assert!(
        !history.contains("FIRST_ATTEMPT"),
        "denied bytes published: {history}"
    );
    f.close().await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn network_denial_terminates_and_unregisters() -> Result<()> {
    skip_if_no_network!(Ok(()));
    skip_if_sandbox!(Ok(()));
    let server = responses::start_mock_server().await;
    let home = Arc::new(tempfile::tempdir()?);
    std::fs::write(
        home.path().join("config.toml"),
        "default_permissions = \"workspace\"\n[permissions.workspace.filesystem]\n\":minimal\" = \"read\"\n[permissions.workspace.network]\nenabled = true\nmode = \"limited\"\nallow_local_binding = true\n",
    )?;
    let test = test_codex()
        .with_home(home)
        .with_cloud_config_bundle(core_test_support::managed_network_requirements_loader())
        .with_config(|config| {
            config.permissions.approval_policy =
                codex_config::Constrained::allow_any(AskForApproval::Never);
            config
                .permissions
                .set_permission_profile(PermissionProfile::workspace_write_with(
                    &[],
                    codex_protocol::protocol::NetworkSandboxPolicy::Enabled,
                    false,
                    false,
                ))
                .expect("managed policy");
        })
        .build(&server)
        .await?;
    assert!(test.config.permissions.network.is_some());
    let f = Fixture { server, test };
    let pidfile = f.test.workspace_path("denied.pid");
    let command = format!(
        "python3 -c \"import os,socket,urllib.parse,time; open('{}','w').write(str(os.getpid())); p=urllib.parse.urlparse(os.environ['HTTP_PROXY']); s=socket.create_connection((p.hostname,p.port),timeout=2); s.sendall(b'GET http://task5-denied.invalid/ HTTP/1.1\\r\\nHost: task5-denied.invalid\\r\\n\\r\\n'); s.recv(1024); time.sleep(60)\"",
        pidfile.display()
    );
    // Direct approval-ID coverage is the companion real-route subcase in
    // monitor_pool_tests::failure_before_registry_insert_kills_process_releases_slot.
    // This external binary observes model-visible exit/no-entry and child death.
    let observed: Result<()> = async {
        f.start(&command).await?;
        let pid = core_test_support::process::wait_for_pid_file(&pidfile).await?;
        let history = f.drained().await?; // Requires both exit notice and empty list.
        anyhow::ensure!(
            history.contains("MONITOR-NOTICE: exit"),
            "visible denial exit"
        );
        core_test_support::process::wait_for_process_exit(&pid).await?;
        Ok(())
    }
    .await;
    let cleanup = f.close().await;
    observed?;
    cleanup
}
