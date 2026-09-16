//! Real-child witnesses for framing, loss notices, and bounded raw transport.
use super::*;
use crate::session::session::Session;
use crate::unified_exec::ExecCommandRequest;
use crate::unified_exec::UnifiedExecContext;
use crate::unified_exec::UnifiedExecOutputMode;
use crate::unified_exec::monitor::MonitorId;
use crate::unified_exec::monitor::MonitorPipeline;
use crate::unified_exec::monitor::PipelineHooks;
use crate::unified_exec::monitor::PipelinePathProbe;
use core_test_support::responses::ResponseMock;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::mount_sse_once;
use core_test_support::responses::sse;
use core_test_support::responses::start_mock_server;
use futures::FutureExt;
use pretty_assertions::assert_eq;
use std::cell::RefCell;
use std::time::Duration;
use tokio::sync::Notify;
use tokio::time::sleep;
use tokio::time::timeout;
use tokio::time::timeout_at;
use tokio_util::sync::CancellationToken;

const LIMIT: Duration = Duration::from_secs(5);
struct FixtureLifetime {
    _server: wiremock::MockServer,
    _events: async_channel::Receiver<codex_protocol::protocol::Event>,
    session: Arc<Session>,
    requests: Arc<Mutex<Vec<serde_json::Value>>>,
    probe: Arc<PipelinePathProbe>,
}
tokio::task_local! {
    static FIXTURE: RefCell<Option<FixtureLifetime>>;
}

async fn fixture(test: impl std::future::Future<Output = ()>) {
    FIXTURE
        .scope(RefCell::new(None), async {
            let outcome = std::panic::AssertUnwindSafe(test).catch_unwind().await;
            let owner = FIXTURE
                .with(|slot| slot.borrow_mut().take())
                .expect("fixture owner");
            timeout(LIMIT, owner.session.services.monitor_manager.abort_all())
                .await
                .expect("monitor shutdown");
            owner
                .session
                .abort_all_tasks(codex_protocol::protocol::TurnAbortReason::Interrupted)
                .await;
            if let Err(panic) = outcome {
                std::panic::resume_unwind(panic);
            }
        })
        .await;
}

async fn hooked_monitor(
    hooks: PipelineHooks,
    command: &str,
) -> (Arc<Session>, MonitorId, ResponseMock) {
    let server = start_mock_server().await;
    let (session, turn, events) =
        crate::session::tests::make_session_and_context_with_auth_and_config_and_rx(
            codex_login::CodexAuth::from_api_key("test-key"),
            Vec::new(),
            |config| {
                config.model_provider.base_url = Some(format!("{}/v1", server.uri()));
                config.model_provider.supports_websockets = false;
                config
                    .permissions
                    .set_permission_profile(codex_protocol::models::PermissionProfile::Disabled)
                    .expect("child fixture files");
                config.permissions.approval_policy = codex_config::Constrained::allow_any(
                    codex_protocol::protocol::AskForApproval::Never,
                );
            },
        )
        .await;
    session
        .services
        .monitor_manager
        .attach(Arc::downgrade(&session));
    let requests = Arc::new(Mutex::new(Vec::new()));
    let captured = Arc::clone(&requests);
    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .and(wiremock::matchers::path_regex(".*/responses$"))
        .respond_with(move |request: &wiremock::Request| {
            captured.lock().unwrap().push(
                request
                    .body_json::<serde_json::Value>()
                    .expect("model request JSON"),
            );
            core_test_support::responses::sse_response(sse(vec![
                ev_response_created("frame-next"),
                ev_completed("frame-next"),
            ]))
        })
        .mount(&server)
        .await;
    let model = mount_sse_once(
        &server,
        sse(vec![
            ev_response_created("frame-response"),
            ev_completed("frame-response"),
        ]),
    )
    .await;
    let (pipeline, sink) = MonitorPipeline::new_with_hooks(hooks);
    let probe = pipeline.path_probe();
    FIXTURE.with(|slot| {
        *slot.borrow_mut() = Some(FixtureLifetime {
            _server: server,
            _events: events,
            session: Arc::clone(&session),
            requests,
            probe,
        })
    });
    let turn_environment = turn
        .environments
        .primary()
        .cloned()
        .expect("primary environment");
    let cwd = turn_environment.cwd().clone();
    let context = UnifiedExecContext::new(
        Arc::clone(&session),
        crate::session::step_context::StepContext::for_test(Arc::clone(&turn)),
        CancellationToken::new(),
        uuid::Uuid::new_v4().to_string(),
    );
    let request = ExecCommandRequest {
        command: vec!["/bin/sh".into(), "-c".into(), command.into()],
        shell_type: crate::shell::ShellType::Sh,
        hook_command: command.into(),
        process_id: 0,
        yield_time_ms: 0,
        max_output_tokens: None,
        cwd: cwd.clone(),
        sandbox_cwd: cwd,
        turn_environment,
        shell_mode: codex_tools::UnifiedExecShellMode::Direct,
        network: turn.network.clone(),
        tty: false,
        output_mode: UnifiedExecOutputMode::Tagged { sink },
        sandbox_permissions: crate::sandboxing::SandboxPermissions::UseDefault,
        additional_permissions: None,
        additional_permissions_preapproved: false,
        justification: None,
        prefix_rule: None,
    };
    let id = timeout(
        LIMIT,
        session.services.monitor_manager.start_with_pipeline(
            &session,
            &context,
            request,
            "frame-path".into(),
            pipeline,
        ),
    )
    .await
    .expect("bounded real child start")
    .expect("monitor start");
    (session, id, model)
}

fn probe() -> Arc<PipelinePathProbe> {
    FIXTURE.with(|slot| Arc::clone(&slot.borrow().as_ref().expect("fixture").probe))
}
fn model_text(model: &ResponseMock) -> String {
    let mut bodies: Vec<_> = model
        .requests()
        .iter()
        .map(core_test_support::responses::ResponsesRequest::body_json)
        .collect();
    FIXTURE.with(|slot| {
        bodies.extend(
            slot.borrow()
                .as_ref()
                .expect("fixture")
                .requests
                .lock()
                .unwrap()
                .iter()
                .cloned(),
        )
    });
    let mut seen = std::collections::HashSet::new();
    let mut texts = Vec::new();
    for body in bodies {
        for item in body["input"].as_array().expect("input array") {
            if item["role"] != "user" {
                continue;
            }
            if let Some(content) = item["content"].as_array() {
                for part in content {
                    if let Some(text) = part["text"].as_str()
                        && text.contains("frame-path delivery")
                        && seen.insert(text.to_owned())
                    {
                        texts.push(text.to_owned());
                    }
                }
            }
        }
    }
    texts.join("\n")
}
async fn wait_for(mut ready: impl FnMut() -> bool) {
    timeout(LIMIT, async {
        while !ready() {
            sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("bounded child/probe handshake");
}
fn loss_counts(probe: &PipelinePathProbe) -> [u64; 4] {
    let mut total = [0; 4];
    for counters in probe.ledger.attempts.lock().unwrap().values() {
        for (sum, counter) in total.iter_mut().zip(&counters.counts) {
            *sum += counter.load(Ordering::Acquire);
        }
    }
    total
}

async fn observed(model: &ResponseMock, needle: &str) -> String {
    timeout(LIMIT, async {
        loop {
            let text = model_text(model);
            if text.contains(needle) {
                return text;
            }
            sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("recorded model request contains witness")
}

#[tokio::test]
async fn final_record_drop_then_exit_emits_notice_in_drain() {
    fixture(async {
        let gate = Arc::new(Notify::new());
        let (_, _, model) = hooked_monitor(
            PipelineHooks {
                delivery_gate: Some(Arc::clone(&gate)),
                ..Default::default()
            },
            "python3 -c 'import os; os.write(1, b\"row\\n\"*256+b\"dropped-final-row\\n\")'",
        )
        .await;
        let probe = probe();
        wait_for(|| loss_counts(&probe)[2] == 1).await;
        wait_for(|| probe.reader_finished()).await;
        probe.hold_receive_for_final_loss();
        gate.notify_one();
        timeout(LIMIT, probe.wait_final_loss())
            .await
            .expect("actual final loss boundary");
        assert_eq!(
            loss_counts(&probe)[2],
            1,
            "channel loss still pending at final collection"
        );
        assert!(!model_text(&model).contains("MONITOR-NOTICE: loss"));
        probe.resume_final_loss();
        let text = observed(&model, "MONITOR-NOTICE: exit").await;
        assert!(text.contains("MONITOR-NOTICE: loss channel-full records=1;"));
        assert!(text.contains(REPLAY_INSTRUCTION));
        assert!(!text.contains("dropped-final-row"));
    })
    .await;
}

#[tokio::test]
async fn drop_then_2s_silence_emits_notice() {
    fixture(async {
        let (session, id, model) = hooked_monitor(
            PipelineHooks::default(),
            "python3 -c 'import os,time; os.write(1, b\"x\"*4097+b\"\\n\"); time.sleep(30)'",
        )
        .await;
        let probe = probe();
        let handshake_budget = Duration::from_secs(5);
        let notice_budget = Duration::from_secs(5);
        let quiet_interval = Duration::from_secs(2);
        timeout(handshake_budget, async {
            while probe.producer_activity_at.lock().unwrap().is_none() {
                sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("actual committed producer activity within handshake budget");
        // A child write can span several transport chunks. Resample P until
        // emission, then require the exact origin selected by the real timer.
        let (producer_at, emitted_at) = loop {
            let producer_at = probe.producer_activity_at.lock().unwrap().unwrap();
            if let Some(emitted_at) = *probe.silence_notice_emitted_at.lock().unwrap() {
                break (producer_at, emitted_at);
            }
            assert!(
                tokio::time::Instant::now()
                    <= tokio::time::Instant::from_std(producer_at) + quiet_interval + notice_budget,
                "silence notice emission exceeded producer-relative deadline"
            );
            sleep(Duration::from_millis(10)).await;
        };
        assert_eq!(probe.silence_timer_origin(), Some(producer_at));
        assert!(
            emitted_at <= producer_at + quiet_interval + notice_budget,
            "silence notice emitted after producer-relative deadline"
        );
        assert!(
            emitted_at.duration_since(producer_at) >= quiet_interval,
            "silence notice emitted before two seconds of producer silence"
        );
        let text = timeout_at(
            tokio::time::Instant::from_std(producer_at) + quiet_interval + notice_budget,
            async {
                loop {
                    let text = model_text(&model);
                    if text.contains("MONITOR-NOTICE: loss oversize records=1;") {
                        break text;
                    }
                    sleep(Duration::from_millis(10)).await;
                }
            },
        )
        .await
        .expect("recorded loss notice within producer-relative deadline");
        assert!(
            tokio::time::Instant::now()
                <= tokio::time::Instant::from_std(producer_at) + quiet_interval + notice_budget,
            "model observation exceeded producer-relative deadline"
        );
        assert_eq!(
            *probe.producer_activity_at.lock().unwrap(),
            Some(producer_at)
        );
        assert!(text.contains(REPLAY_INSTRUCTION));
        assert!(!text.contains("MONITOR-NOTICE: exit"));
        assert!(
            session
                .services
                .monitor_manager
                .list()
                .await
                .iter()
                .any(|monitor| monitor.id == id)
        );
    })
    .await;
}

#[tokio::test]
async fn partial_final_record_rides_exit_notice_not_record() {
    fixture(async {
        let (_, _, model) =
            hooked_monitor(PipelineHooks::default(), "printf 'partial-final-unique'").await;
        let text = observed(
            &model,
            "MONITOR-NOTICE: exit partial-stdout=\"partial-final-unique\"",
        )
        .await;
        assert_eq!(text.matches("partial-final-unique").count(), 1);
        assert!(!text.contains("<stdout>"));
    })
    .await;
}

#[tokio::test]
async fn stderr_forwarded_in_tagged_block_unaltered() {
    fixture(async {
        let (_, _, model) = hooked_monitor(
            PipelineHooks::default(),
            "printf 'stderr  λ \\t{\"cell\":7}\\n' >&2",
        )
        .await;
        let text = observed(&model, "<stderr>\nstderr  λ \t{\"cell\":7}\n</stderr>").await;
        assert!(!text.contains("<stdout>"));
        assert!(!text.contains("MONITOR-NOTICE: loss"));
    })
    .await;
}

#[tokio::test]
async fn rate_drop_then_channel_drop_two_notices_correct_sites() {
    fixture(async {
        let dir = tempfile::tempdir().expect("child barriers");
        let first = dir.path().join("first");
        let second = dir.path().join("second");
        let command = format!(
            r#"python3 -c 'import os,time,pathlib
while not pathlib.Path("{}").exists(): time.sleep(.005)
os.write(1,b"rate-row\n"*201)
while not pathlib.Path("{}").exists(): time.sleep(.005)
os.write(1,b"channel-row\n"*256+b"channel-dropped-final\n")
time.sleep(30)'"#,
            first.display(),
            second.display()
        );
        let (_, _, model) = hooked_monitor(PipelineHooks::default(), &command).await;
        std::fs::write(&first, b"go").expect("first burst");
        observed(&model, "MONITOR-NOTICE: loss rate records=1;").await;
        let probe = probe();
        timeout(LIMIT, probe.pause())
            .await
            .expect("delivery parked before channel burst");
        std::fs::write(&second, b"go").expect("second burst");
        wait_for(|| loss_counts(&probe)[2] == 1).await;
        assert_eq!(loss_counts(&probe), [0, 0, 1, 0]);
        probe.refill().await;
        probe.resume(200);
        timeout(LIMIT, probe.wait_parked())
            .await
            .expect("200 records consumed before second refill");
        probe.refill().await;
        probe.resume(usize::MAX);
        let text = observed(&model, "MONITOR-NOTICE: loss channel-full records=1;").await;
        let notices: Vec<_> = text
            .lines()
            .filter(|line| line.starts_with("MONITOR-NOTICE: loss "))
            .collect();
        assert_eq!(
            notices,
            vec![
                format!("MONITOR-NOTICE: loss rate records=1; {REPLAY_INSTRUCTION}"),
                format!("MONITOR-NOTICE: loss channel-full records=1; {REPLAY_INSTRUCTION}"),
            ]
        );
        assert!(!text.contains("channel-dropped-final"));
        assert_eq!(loss_counts(&probe), [0; 4]);
    })
    .await;
}

#[tokio::test]
async fn stalled_reader_backpressures_producer_no_chunk_loss() {
    fixture(async {
        let dir = tempfile::tempdir().expect("raw progress");
        let progress = dir.path().join("progress");
        let pid_path = dir.path().join("pid");
        let done = dir.path().join("done");
        let reader = Arc::new(Notify::new());
        let delivery = Arc::new(Notify::new());
        let command = format!(
            r#"python3 -c 'import os,pathlib
pathlib.Path("{}").write_text(str(os.getpid()))
with open("{}","ab",buffering=0) as progress:
 for i in range(8192):
  row=(str(i).zfill(7)+":"+"x"*1015+"\n").encode()
  offset=0
  while offset<len(row): offset+=os.write(1,row[offset:])
  if (i+1)%1024==0: progress.write(b"1\n")
pathlib.Path("{}").write_text("done")'"#,
            pid_path.display(),
            progress.display(),
            done.display()
        );
        let (_, _, model) = hooked_monitor(
            PipelineHooks {
                reader_gate: Some(Arc::clone(&reader)),
                delivery_gate: Some(Arc::clone(&delivery)),
                lossless_downstream: true,
            },
            &command,
        )
        .await;
        wait_for(|| progress.exists() && std::fs::metadata(&progress).unwrap().len() > 0).await;
        let pid = std::fs::read_to_string(&pid_path).expect("native writer PID");
        let mut previous = std::fs::read(&progress).expect("progress").len();
        let mut stable = 0;
        timeout(LIMIT, async {
            while stable < 3 {
                sleep(Duration::from_millis(200)).await;
                let current = std::fs::read(&progress).expect("progress").len();
                assert!(
                    std::process::Command::new("/bin/kill")
                        .args(["-0", pid.trim()])
                        .status()
                        .expect("writer liveness")
                        .success()
                );
                assert!(
                    !done.exists(),
                    "8 MiB writer must not finish through bounded paused raw queues"
                );
                stable = if current == previous { stable + 1 } else { 0 };
                previous = current;
            }
        })
        .await
        .expect("three progress-stall polls while child alive");
        assert!(previous < 16, "fewer than eight MiB progress lines");
        let probe = probe();
        assert!(probe.collected.lock().unwrap().is_empty());
        reader.notify_one();
        wait_for(|| done.exists()).await;
        assert_eq!(std::fs::read(&progress).unwrap(), b"1\n".repeat(8));
        wait_for(|| probe.reader_finished()).await;
        assert_eq!(
            loss_counts(&probe),
            [0; 4],
            "reader counters before delivery can reset them"
        );
        delivery.notify_one();
        wait_for(|| probe.collected.lock().unwrap().len() == 8192).await;
        {
            let records = probe.collected.lock().unwrap();
            let attempt = records[0].attempt;
            for (i, record) in records.iter().enumerate() {
                assert_eq!(
                    record,
                    &Record {
                        attempt,
                        stream: Stream::Stdout,
                        bytes: format!("{i:07}:{}", "x".repeat(1015)).into_bytes()
                    }
                );
            }
        }
        assert_eq!(loss_counts(&probe), [0; 4]);
        let text = observed(&model, "0000000:").await;
        assert!(text.contains(&format!("0000000:{}\n", "x".repeat(1015))));
        assert!(!text.contains("MONITOR-NOTICE: loss"));
    })
    .await;
}
