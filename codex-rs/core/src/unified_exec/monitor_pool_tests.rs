//! Controlled raw queues distinguish reader scheduling from downstream delivery.
use super::super::monitor_frame::AttemptNonce;
use super::super::monitor_frame::LossLedger;
use super::super::monitor_frame::Record;
use super::super::monitor_frame::Stream;
use super::super::process::NoopSpawnLifecycle;
use super::*;
use codex_sandboxing::SandboxType;
use codex_utils_pty::ProcessDriver;
use codex_utils_pty::SpawnedPty;
use codex_utils_pty::spawn_from_driver;
use pretty_assertions::assert_eq;
use std::sync::atomic::Ordering;
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tokio::time::timeout;

const LIMIT: Duration = Duration::from_secs(2);

async fn bounded<F: std::future::Future>(future: F) -> F::Output {
    timeout(LIMIT, future)
        .await
        .expect("bounded seam handshake")
}

// Each send is a separate raw message. No OS read or broadcast can coalesce it.
async fn controlled_source() -> (
    SpawnedPty,
    mpsc::Sender<Vec<u8>>,
    mpsc::Sender<Vec<u8>>,
    oneshot::Sender<i32>,
    oneshot::Sender<i32>,
) {
    let (writer_tx, _writer_rx) = mpsc::channel(1);
    let (_driver_tx, driver_rx) = tokio::sync::broadcast::channel(1);
    let (driver_exit, driver_exit_rx) = oneshot::channel();
    let driver = spawn_from_driver(ProcessDriver {
        writer_tx,
        stdout_rx: driver_rx,
        stderr_rx: None,
        exit_rx: driver_exit_rx,
        terminator: None,
        writer_handle: None,
        resizer: None,
        #[cfg(windows)]
        tty: false,
    });
    let (stdout_tx, stdout_rx) = mpsc::channel(128);
    let (stderr_tx, stderr_rx) = mpsc::channel(128);
    let (exit_tx, exit_rx) = oneshot::channel();
    (
        SpawnedPty {
            session: driver.session,
            stdout_rx,
            stderr_rx,
            exit_rx,
        },
        stdout_tx,
        stderr_tx,
        exit_tx,
        driver_exit,
    )
}

struct Saturated {
    process: UnifiedExecProcess,
    tagged_rx: mpsc::Receiver<TaggedChunk>,
    stdout: mpsc::Sender<Vec<u8>>,
    stderr: mpsc::Sender<Vec<u8>>,
    helper: JoinHandle<()>,
    _driver_exit: oneshot::Sender<i32>,
}

async fn saturate(payload: &'static [u8], exit_code: i32) -> Saturated {
    let (spawned, stdout, stderr, exit, driver_exit) = controlled_source().await;
    let (sink, tagged_rx) = mpsc::channel(128);
    let (process, _) = bounded(UnifiedExecProcess::from_spawned_tagged(
        spawned,
        SandboxType::MacosSeatbelt,
        Box::new(NoopSpawnLifecycle),
        sink.clone(),
    ))
    .await
    .expect("constructor returns after grace without exit");
    assert!(!process.has_exited());
    let (filled_tx, filled_rx) = oneshot::channel();
    let (release_tx, release_rx) = oneshot::channel();
    let (pending_tx, pending_rx) = oneshot::channel();
    let sender = stdout.clone();
    let helper = tokio::spawn(async move {
        for _ in 0..257 {
            bounded(sender.send(vec![b'x'])).await.expect("raw item");
        }
        filled_tx.send(()).expect("filled handshake");
        bounded(release_rx).await.expect("payload handshake");
        let send = sender.send(payload.to_vec());
        tokio::pin!(send);
        bounded(std::future::poll_fn(|cx| {
            use std::future::Future;
            assert!(
                send.as_mut().poll(cx).is_pending(),
                "payload must block at saturated boundary"
            );
            std::task::Poll::Ready(())
        }))
        .await;
        pending_tx.send(()).expect("pending-send observation");
        bounded(send).await.expect("boundary payload");
    });
    bounded(filled_rx).await.expect("257 sends completed");
    bounded(async {
        while sink.capacity() != 0 || stdout.capacity() != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await;
    release_tx.send(()).expect("allow blocked payload send");
    bounded(pending_rx)
        .await
        .expect("payload send was polled Pending");
    // Both channels are full, so this helper cannot complete before release.
    assert!(!helper.is_finished());
    exit.send(exit_code).expect("independent exit signal");
    bounded(async {
        while !process.has_exited() {
            tokio::task::yield_now().await;
        }
    })
    .await;
    Saturated {
        process,
        tagged_rx,
        stdout,
        stderr,
        helper,
        _driver_exit: driver_exit,
    }
}

#[tokio::test]
async fn late_denial_after_saturated_tagged_channel_is_classified() {
    let Saturated {
        process,
        tagged_rx,
        stdout,
        stderr,
        helper,
        _driver_exit,
    } = saturate(b"permission denied\n", 1).await;
    let (records_tx, _records_rx) = mpsc::channel(256);
    let reader = spawn_reader(tagged_rx, Arc::new(LossLedger::default()), records_tx);
    bounded(helper).await.expect("payload ingested");
    bounded(async {
        loop {
            if process
                .output_handles()
                .output_buffer
                .lock()
                .await
                .to_bytes()
                .ends_with(b"permission denied\n")
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await;
    assert!(matches!(
        bounded(process.check_for_sandbox_denial()).await,
        Err(super::super::UnifiedExecError::SandboxDenied { .. })
    ));
    drop((stdout, stderr));
    bounded(reader).await.expect("reader closes");
}

#[tokio::test]
async fn no_denial_saturation_control_drains_and_closes() {
    let Saturated {
        process,
        tagged_rx,
        stdout,
        stderr,
        helper,
        _driver_exit,
    } = saturate(b"ok\n", 0).await;
    let (records_tx, _records_rx) = mpsc::channel(256);
    let reader = spawn_reader(tagged_rx, Arc::new(LossLedger::default()), records_tx);
    bounded(helper).await.expect("payload send completes");
    drop((stdout, stderr));
    bounded(async {
        while !process
            .output_handles()
            .output_closed
            .load(Ordering::Acquire)
        {
            tokio::task::yield_now().await;
        }
    })
    .await;
    let mut expected = vec![b'x'; 257];
    expected.extend_from_slice(b"ok\n");
    assert_eq!(
        process
            .output_handles()
            .output_buffer
            .lock()
            .await
            .to_bytes(),
        expected
    );
    bounded(process.check_for_sandbox_denial())
        .await
        .expect("normal exit");
    bounded(reader).await.expect("reader closes");
}

#[tokio::test]
async fn unattached_reader_control_stalls_ingestion_before_denial_line() {
    let Saturated {
        process,
        tagged_rx,
        stdout,
        stderr,
        helper,
        _driver_exit,
    } = saturate(b"permission denied\n", 1).await;
    assert_eq!(
        process
            .output_handles()
            .output_buffer
            .lock()
            .await
            .to_bytes(),
        vec![b'x'; 129]
    );
    bounded(process.check_for_sandbox_denial())
        .await
        .expect("unattached reader hides late denial");
    helper.abort();
    let _ = bounded(helper).await;
    drop((tagged_rx, stdout, stderr));
    bounded(async {
        while !process
            .output_handles()
            .output_closed
            .load(Ordering::Acquire)
        {
            tokio::task::yield_now().await;
        }
    })
    .await;
    assert_eq!(
        process
            .output_handles()
            .output_buffer
            .lock()
            .await
            .to_bytes(),
        vec![b'x'; 257]
    );
}

#[tokio::test]
async fn pipeline_reader_runs_before_any_attempt_is_spawned() {
    let (mut pipeline, sink) = MonitorPipeline::new();
    let attempt = AttemptNonce::new(41);
    bounded(sink.send(TaggedChunk {
        attempt,
        stream: Stream::Stdout,
        bytes: b"ready\n".to_vec(),
    }))
    .await
    .expect("tagged item");
    assert_eq!(
        bounded(pipeline.records.recv()).await,
        Some(Record {
            attempt,
            stream: Stream::Stdout,
            bytes: b"ready".to_vec()
        })
    );
}

#[tokio::test]
async fn reader_resets_framer_and_ledger_on_new_attempt_nonce() {
    let (mut pipeline, sink) = MonitorPipeline::new();
    for (attempt, bytes) in [
        (AttemptNonce::new(1), b"abc".as_slice()),
        (AttemptNonce::new(2), b"def\n".as_slice()),
    ] {
        bounded(sink.send(TaggedChunk {
            attempt,
            stream: Stream::Stdout,
            bytes: bytes.to_vec(),
        }))
        .await
        .expect("attempt item");
    }
    assert_eq!(
        bounded(pipeline.records.recv()).await,
        Some(Record {
            attempt: AttemptNonce::new(2),
            stream: Stream::Stdout,
            bytes: b"def".to_vec()
        })
    );
    assert!(pipeline.commit(AttemptNonce::new(2)).take().is_empty());
}

use super::super::ExecCommandRequest;
use super::super::UnifiedExecContext;
use super::super::UnifiedExecOutputMode;
use crate::session::turn_context::TurnContext;
use codex_protocol::protocol::AskForApproval;
use codex_protocol::protocol::Event;
use codex_protocol::protocol::EventMsg;

struct PoolFixture {
    session: Arc<Session>,
    turn: Arc<TurnContext>,
    events: async_channel::Receiver<Event>,
}
impl PoolFixture {
    async fn new(approval: AskForApproval) -> Self {
        let (session, turn, events) =
            crate::session::tests::make_session_and_context_with_auth_and_config_and_rx(
                codex_login::CodexAuth::from_api_key("test-key"),
                Vec::new(),
                |config| {
                    if matches!(approval, AskForApproval::Never) {
                        config
                            .permissions
                            .set_permission_profile(
                                codex_protocol::models::PermissionProfile::Disabled,
                            )
                            .expect("fixture marker writes");
                    }
                    config.permissions.approval_policy =
                        codex_config::Constrained::allow_any(approval);
                },
            )
            .await;
        session
            .services
            .monitor_manager
            .attach(Arc::downgrade(&session));
        Self {
            session,
            turn,
            events,
        }
    }
    fn request(&self, command: &str) -> (ExecCommandRequest, UnifiedExecContext, MonitorPipeline) {
        let (pipeline, sink) = MonitorPipeline::new();
        let turn_environment = self.turn.environments.primary().cloned().expect("primary");
        let cwd = turn_environment.cwd().clone();
        let context = UnifiedExecContext::new(
            Arc::clone(&self.session),
            crate::session::step_context::StepContext::for_test(Arc::clone(&self.turn)),
            CancellationToken::new(),
            uuid::Uuid::new_v4().to_string(),
        );
        (
            ExecCommandRequest {
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
                network: self.turn.network.clone(),
                tty: false,
                output_mode: UnifiedExecOutputMode::Tagged { sink },
                sandbox_permissions: crate::sandboxing::SandboxPermissions::UseDefault,
                additional_permissions: None,
                additional_permissions_preapproved: false,
                justification: None,
                prefix_rule: None,
            },
            context,
            pipeline,
        )
    }
}

#[tokio::test]
async fn ninth_start_refused_before_spawn() {
    let f = PoolFixture::new(AskForApproval::Never).await;
    let slots: Vec<_> = (0..8)
        .map(|_| f.session.services.monitor_manager.reserve().expect("slot"))
        .collect();
    let dir = tempfile::tempdir().expect("marker dir");
    let marker = dir.path().join("spawned");
    let (request, context, pipeline) = f.request(&format!("touch '{}'", marker.display()));
    let result = bounded(f.session.services.monitor_manager.start_with_pipeline(
        &f.session,
        &context,
        request,
        "ninth".into(),
        pipeline,
    ))
    .await;
    assert!(result.is_err());
    assert!(!marker.exists(), "refusal must precede preparation/spawn");
    assert!(f.events.try_recv().is_err(), "no preparation events");
    drop(slots);
    assert!(f.session.services.monitor_manager.reserve().is_some());
    f.session.services.monitor_manager.abort_all().await;
}

#[tokio::test]
async fn two_concurrent_starts_race_last_slot_exactly_one_spawns() {
    let f = PoolFixture::new(AskForApproval::Never).await;
    let slots: Vec<_> = (0..7)
        .map(|_| f.session.services.monitor_manager.reserve().expect("slot"))
        .collect();
    let dir = tempfile::tempdir().expect("marker dir");
    let marker = dir.path().join("spawned");
    let command = format!("echo child >> '{}'; exec sleep 60", marker.display());
    let (ra, ca, pa) = f.request(&command);
    let (rb, cb, pb) = f.request(&command);
    let manager = &f.session.services.monitor_manager;
    let (a, b) = bounded(async {
        tokio::join!(
            manager.start_with_pipeline(&f.session, &ca, ra, "a".into(), pa),
            manager.start_with_pipeline(&f.session, &cb, rb, "b".into(), pb),
        )
    })
    .await;
    assert_eq!(usize::from(a.is_ok()) + usize::from(b.is_ok()), 1);
    assert_eq!(
        std::fs::read_to_string(marker).expect("spawn marker"),
        "child\n"
    );
    assert_eq!(manager.list().await.len(), 1);
    drop(slots);
    manager.abort_all().await;
}

#[tokio::test]
async fn spawn_failure_releases_slot_no_registry_entry() {
    let f = PoolFixture::new(AskForApproval::Never).await;
    let (mut request, context, pipeline) = f.request("unused");
    request.command = vec!["/nonexistent/task5-monitor-executable".into()];
    assert!(
        bounded(f.session.services.monitor_manager.start_with_pipeline(
            &f.session,
            &context,
            request,
            "fail".into(),
            pipeline
        ))
        .await
        .is_err()
    );
    assert!(f.session.services.monitor_manager.list().await.is_empty());
    let slots: Vec<_> = (0..8)
        .map(|_| {
            f.session
                .services
                .monitor_manager
                .reserve()
                .expect("all slots free")
        })
        .collect();
    drop(slots);
    f.session.services.monitor_manager.abort_all().await;
}

#[tokio::test]
async fn remote_attempt_reports_unsupported() {
    let f = PoolFixture::new(AskForApproval::Never).await;
    let (mut request, context, pipeline) = f.request("echo must-not-run");
    request.turn_environment.environment = Arc::new(
        codex_exec_server::Environment::create_for_tests(Some("ws://127.0.0.1:9".into()))
            .expect("remote handle"),
    );
    let error = bounded(f.session.services.monitor_manager.start_with_pipeline(
        &f.session,
        &context,
        request,
        "remote".into(),
        pipeline,
    ))
    .await
    .expect_err("remote refused without connecting");
    assert!(error.to_string().contains("unavailable: remote-execution"));
    assert!(f.session.services.monitor_manager.list().await.is_empty());
    let slots: Vec<_> = (0..8)
        .map(|_| {
            f.session
                .services
                .monitor_manager
                .reserve()
                .expect("slot free")
        })
        .collect();
    drop(slots);
    f.session.services.monitor_manager.abort_all().await;
}

struct ApprovalHeldTask {
    started: Arc<tokio::sync::Barrier>,
}
impl crate::tasks::SessionTask for ApprovalHeldTask {
    fn kind(&self) -> crate::state::TaskKind {
        crate::state::TaskKind::Regular
    }
    fn span_name(&self) -> &'static str {
        "session_task.monitor_pool_approval"
    }
    async fn run(
        self: Arc<Self>,
        _session: Arc<Session>,
        _ctx: Arc<TurnContext>,
        _input: Vec<crate::session::TurnInput>,
        cancellation: CancellationToken,
    ) -> crate::tasks::SessionTaskResult {
        self.started.wait().await;
        cancellation.cancelled().await;
        Ok(None)
    }
}

#[tokio::test]
async fn approval_cancelled_during_preparation_leaves_no_process_and_free_slot() {
    for stop in ["before_registration", "token", "caller", "shutdown"] {
        let f = PoolFixture::new(AskForApproval::OnRequest).await;
        let started = Arc::new(tokio::sync::Barrier::new(2));
        f.session
            .spawn_task(
                Arc::clone(&f.turn),
                Vec::new(),
                ApprovalHeldTask {
                    started: Arc::clone(&started),
                },
            )
            .await;
        bounded(started.wait()).await;
        assert!(f.session.active_turn.lock().await.is_some());
        let dir = tempfile::tempdir().expect("marker dir");
        let marker = dir.path().join("spawned");
        let (mut request, context, pipeline) = f.request(&format!("touch '{}'", marker.display()));
        request.sandbox_permissions = crate::sandboxing::SandboxPermissions::RequireEscalated;
        let original_call_id = context.call_id.clone();
        let (unrelated_tx, mut unrelated_rx) = oneshot::channel();
        let turn_state = Arc::clone(
            &f.session
                .active_turn
                .lock()
                .await
                .as_ref()
                .expect("held task")
                .turn_state,
        );
        turn_state
            .lock()
            .await
            .insert_pending_approval(original_call_id.clone(), unrelated_tx);
        let session = Arc::clone(&f.session);
        let cancel = context.cancellation_token.clone();
        if stop == "before_registration" {
            cancel.cancel();
        }
        let task = tokio::spawn(async move {
            session
                .services
                .monitor_manager
                .start_with_pipeline(&session, &context, request, "approval".into(), pipeline)
                .await
        });
        let approval_id = if stop == "before_registration" {
            None
        } else {
            let id = bounded(async {
                loop {
                    if let EventMsg::ExecApprovalRequest(approval) =
                        f.events.recv().await.expect("event").msg
                    {
                        break approval.effective_approval_id();
                    }
                }
            })
            .await;
            assert!(id.starts_with("monitor-preparation-"));
            assert_ne!(id, original_call_id);
            Some(id)
        };
        match stop {
            "before_registration" => {
                assert!(bounded(task).await.expect("task joins").is_err());
            }
            "token" => {
                cancel.cancel();
                assert!(bounded(task).await.expect("task joins").is_err());
            }
            "caller" => {
                task.abort();
                assert!(
                    bounded(task)
                        .await
                        .expect_err("cancelled caller")
                        .is_cancelled()
                );
            }
            "shutdown" => {
                bounded(f.session.services.monitor_manager.abort_all()).await;
                assert!(bounded(task).await.expect("task joins").is_err());
            }
            _ => unreachable!(),
        }
        f.session.services.monitor_manager.starts.close();
        bounded(f.session.services.monitor_manager.starts.wait()).await;
        assert!(!marker.exists());
        assert!(f.session.services.monitor_manager.list().await.is_empty());
        assert_eq!(
            f.session.services.monitor_manager.slots.available_permits(),
            8
        );
        assert!(f.session.services.monitor_manager.starts.is_empty());
        assert!(f.session.active_turn.lock().await.is_some());
        if let Some(approval_id) = &approval_id {
            assert!(
                turn_state
                    .lock()
                    .await
                    .remove_pending_approval(approval_id)
                    .is_none(),
                "cancelled preparation left an approval callback"
            );
        } else {
            while let Ok(event) = f.events.try_recv() {
                assert!(
                    !matches!(event.msg, EventMsg::ExecApprovalRequest(_)),
                    "pre-cancelled preparation requested approval"
                );
            }
        }
        assert!(
            matches!(
                unrelated_rx.try_recv(),
                Err(oneshot::error::TryRecvError::Empty)
            ),
            "ordinary approval was disturbed"
        );
        f.session
            .notify_approval(
                &original_call_id,
                codex_protocol::protocol::ReviewDecision::Approved,
            )
            .await;
        assert_eq!(
            bounded(unrelated_rx)
                .await
                .expect("ordinary approval still registered"),
            codex_protocol::protocol::ReviewDecision::Approved
        );
        f.session
            .abort_all_tasks(codex_protocol::protocol::TurnAbortReason::Interrupted)
            .await;
        bounded(f.session.services.monitor_manager.abort_all()).await;
        assert!(f.session.services.monitor_manager.admissions.is_empty());
        assert!(
            f.session
                .services
                .monitor_manager
                .retry_task
                .lock()
                .expect("retry lock")
                .is_none()
        );
    }
}

#[tokio::test]
#[expect(
    clippy::await_holding_invalid_type,
    reason = "holds the real registry boundary to observe a spawned child and approval before cancellation"
)]
#[expect(clippy::print_stderr, reason = "retained stage-specific gate evidence")]
async fn failure_before_registry_insert_kills_process_releases_slot() {
    let mut f = PoolFixture::new(AskForApproval::Never).await;
    let profile = codex_protocol::models::PermissionProfile::Disabled;
    let spec = crate::config::NetworkProxySpec::from_config_and_constraints(
        codex_network_proxy::NetworkProxyConfig {
            enabled: true,
            proxy_url: "http://127.0.0.1:0".into(),
            enable_socks5: false,
            ..Default::default()
        },
        None,
        &profile,
    )
    .expect("proxy specification");
    // Attribution registration needs controller state, not a running listener.
    let proxy = codex_network_proxy::NetworkProxy::builder()
        .state(Arc::new(
            spec.build_state_with_audit_metadata(
                codex_network_proxy::NetworkProxyAuditMetadata::default(),
            )
            .expect("proxy state"),
        ))
        .build()
        .await
        .expect("controller state");
    Arc::get_mut(&mut f.turn)
        .expect("unique fixture context")
        .network = Some(proxy.clone());
    let dir = tempfile::tempdir().expect("pid dir");
    let marker = dir.path().join("pid");
    let (request, context, pipeline) =
        f.request(&format!("echo $$ > '{}'; exec sleep 60", marker.display()));
    let registry = f.session.services.monitor_manager.monitors.lock().await;
    let session = Arc::clone(&f.session);
    let cancel = context.cancellation_token.clone();
    let task = tokio::spawn(async move {
        session
            .services
            .monitor_manager
            .start_with_pipeline(
                &session,
                &context,
                request,
                "cancel before insert".into(),
                pipeline,
            )
            .await
    });
    let observation = timeout(LIMIT, async {
        loop {
            let registered = f.session.services.monitor_manager.registered_approval_ids();
            if let Ok(text) = std::fs::read_to_string(&marker)
                && let Ok(pid) = text.trim().parse::<i32>()
                && registered.len() == 1
            {
                assert!(
                    unsafe { libc::kill(pid, 0) } == 0,
                    "child live before cancellation"
                );
                assert!(
                    f.session
                        .services
                        .monitor_manager
                        .unregistered_approval_ids()
                        .is_empty(),
                    "registration must precede cleanup"
                );
                break (pid, registered[0].clone());
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
    eprintln!("pre-insertion registration/child observation: {observation:?}");
    cancel.cancel();
    drop(registry);
    let start_result = timeout(LIMIT, task).await;
    eprintln!("cancelled start settlement: {start_result:?}");
    let child_dead = if let Ok((pid, _)) = &observation {
        timeout(LIMIT, async {
            while unsafe { libc::kill(*pid, 0) } == 0 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
    } else {
        Ok(())
    };
    eprintln!("child-death stage: {child_dead:?}");
    let cleanup_ids = timeout(LIMIT, async {
        loop {
            let ids = f
                .session
                .services
                .monitor_manager
                .unregistered_approval_ids();
            if !ids.is_empty() {
                break ids;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
    eprintln!("approval-unregistration stage: {cleanup_ids:?}");
    let registry_empty = f.session.services.monitor_manager.list().await.is_empty();
    let free_slots = f.session.services.monitor_manager.slots.available_permits();
    eprintln!("registry_empty={registry_empty} free_slots={free_slots}");
    let shutdown = timeout(LIMIT, f.session.services.monitor_manager.abort_all()).await;
    eprintln!("manager shutdown stage: {shutdown:?}");
    Arc::get_mut(&mut f.turn)
        .expect("fixture context released")
        .network = None;
    drop(proxy);
    let (_, id) = observation.expect("registered approval and live child before insertion");
    assert!(
        start_result
            .expect("start settles")
            .expect("start joins")
            .is_err()
    );
    child_dead.expect("child-death deadline");
    assert_eq!(
        f.session.services.monitor_manager.registered_approval_ids(),
        vec![id.clone()]
    );
    assert_eq!(
        cleanup_ids.expect("approval-unregistration deadline"),
        vec![id]
    );
    assert!(registry_empty, "no registry entry after cancelled start");
    assert_eq!(free_slots, 8);
    shutdown.expect("manager shutdown deadline");
    // Companion scenarios keep the exact bound identity while proving cleanup
    // ownership after a record leaves the registry, on both removal paths.
    let self_owned = cleanup_after_removal_is_owned(false).await;
    let stop_owned = cleanup_after_removal_is_owned(true).await;
    assert!(self_owned, "shutdown must await self-deregister cleanup");
    assert!(stop_owned, "shutdown must await external-stop cleanup");
    real_network_denial_unregisters().await;
}

#[expect(clippy::print_stderr, reason = "retained stage-specific gate evidence")]
async fn cleanup_after_removal_is_owned(external_stop: bool) -> bool {
    let mut f = PoolFixture::new(AskForApproval::Never).await;
    let spec = crate::config::NetworkProxySpec::from_config_and_constraints(
        codex_network_proxy::NetworkProxyConfig {
            enabled: true,
            proxy_url: "http://127.0.0.1:0".into(),
            enable_socks5: false,
            ..Default::default()
        },
        None,
        &codex_protocol::models::PermissionProfile::Disabled,
    )
    .expect("proxy specification");
    let proxy = codex_network_proxy::NetworkProxy::builder()
        .state(Arc::new(
            spec.build_state_with_audit_metadata(
                codex_network_proxy::NetworkProxyAuditMetadata::default(),
            )
            .expect("proxy state"),
        ))
        .build()
        .await
        .expect("controller state");
    Arc::get_mut(&mut f.turn).expect("unique context").network = Some(proxy);
    let (request, context, pipeline) = f.request("exec sleep 60");
    let id = bounded(f.session.services.monitor_manager.start_with_pipeline(
        &f.session,
        &context,
        request,
        "cleanup ownership".into(),
        pipeline,
    ))
    .await
    .expect("running monitor");
    let manager = &f.session.services.monitor_manager;
    let registered = manager.registered_approval_ids();
    assert_eq!(registered.len(), 1);
    assert!(manager.unregistered_approval_ids().is_empty());
    let (process, reader, ledger) = {
        let records = manager.monitors.lock().await;
        let record = records.get(&id).expect("registered record");
        (
            Arc::clone(&record.process.as_ref().expect("process").process),
            record
                .pipeline
                .as_ref()
                .expect("pipeline")
                .reader
                .abort_handle(),
            Arc::downgrade(&record.pipeline.as_ref().expect("pipeline").ledger),
        )
    };
    let reached = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    *manager.cleanup_gate.lock().expect("cleanup gate") = Some((reached.clone(), release.clone()));
    let remover = if external_stop {
        let session = Arc::clone(&f.session);
        Some(tokio::spawn(async move {
            session.services.monitor_manager.remove(&id).await
        }))
    } else {
        process.terminate();
        None
    };
    bounded(reached.notified()).await;
    assert!(
        manager.list().await.is_empty(),
        "paused after actual registry removal"
    );
    assert_eq!(manager.slots.available_permits(), 7);
    assert!(manager.unregistered_approval_ids().is_empty());
    assert!(
        ledger.upgrade().is_some(),
        "pipeline retained during cleanup"
    );
    let session = Arc::clone(&f.session);
    let mut shutdown =
        tokio::spawn(async move { session.services.monitor_manager.abort_all().await });
    bounded(async {
        while !manager.shutdown.is_cancelled() {
            tokio::task::yield_now().await;
        }
    })
    .await;
    let pending = timeout(Duration::from_millis(50), &mut shutdown)
        .await
        .is_err();
    eprintln!("cleanup external_stop={external_stop}: shutdown_pending={pending}");
    release.notify_one();
    if pending {
        bounded(shutdown).await.expect("shutdown joins");
    }
    if let Some(remover) = remover {
        bounded(remover).await.expect("stop joins");
    }
    bounded(async {
        while manager.unregistered_approval_ids() != registered
            || !reader.is_finished()
            || ledger.upgrade().is_some()
            || manager.slots.available_permits() != 8
            || !process.has_exited()
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
    assert!(process.has_exited());
    assert!(manager.list().await.is_empty());
    assert!(manager.admissions.is_empty());
    assert!(manager.starts.is_empty());
    assert!(manager.cleanups.is_empty());
    pending
}

// Companion to suite::monitor_pool::network_denial_terminates_and_unregisters:
// that external binary observes model-visible denial/exit and no entry; this
// in-crate scenario observes the exact approval operations on the same native
// start_with_pipeline -> denial watcher -> deregister/cleanup route.
#[expect(clippy::print_stderr, reason = "retained stage-specific gate evidence")]
async fn real_network_denial_unregisters() {
    let mut f = PoolFixture::new(AskForApproval::Never).await;
    let mut config = codex_network_proxy::NetworkProxyConfig {
        enabled: true,
        proxy_url: "http://127.0.0.1:0".into(),
        enable_socks5: false,
        ..Default::default()
    };
    config.set_denied_domains(vec!["task5-denied.invalid".into()]);
    let spec = crate::config::NetworkProxySpec::from_config_and_constraints(
        config,
        None,
        &codex_protocol::models::PermissionProfile::Disabled,
    )
    .expect("denial proxy specification");
    let proxy = codex_network_proxy::NetworkProxy::builder()
        .state(Arc::new(
            spec.build_state_with_audit_metadata(
                codex_network_proxy::NetworkProxyAuditMetadata::default(),
            )
            .expect("denial proxy state"),
        ))
        .blocked_request_observer_arc(
            crate::tools::network_approval::build_blocked_request_observer(Arc::clone(
                &f.session.services.network_approval,
            )),
        )
        .build()
        .await
        .expect("denial controller");
    let controller = proxy.run().await.expect("running denial controller");
    Arc::get_mut(&mut f.turn)
        .expect("unique denial context")
        .network = Some(proxy);
    let dir = tempfile::tempdir().expect("denial markers");
    let pidfile = dir.path().join("pid");
    let trigger = dir.path().join("deny");
    let command = format!(
        "echo $$ > '{}'; while [ ! -f '{}' ]; do sleep 0.01; done; exec python3 -c \"import os,socket,urllib.parse,time; p=urllib.parse.urlparse(os.environ['HTTP_PROXY']); s=socket.create_connection((p.hostname,p.port),timeout=2); s.sendall(b'GET http://task5-denied.invalid/ HTTP/1.1\\r\\nHost: task5-denied.invalid\\r\\n\\r\\n'); s.recv(1024); time.sleep(60)\"",
        pidfile.display(),
        trigger.display(),
    );
    let observed: anyhow::Result<()> = async {
        let (request, context, pipeline) = f.request(&command);
        let id = timeout(
            LIMIT,
            f.session.services.monitor_manager.start_with_pipeline(
                &f.session,
                &context,
                request,
                "real network denial".into(),
                pipeline,
            ),
        )
        .await??;
        let manager = &f.session.services.monitor_manager;
        let registered = manager.registered_approval_ids();
        anyhow::ensure!(
            registered.len() == 1,
            "exactly one actual registration: {registered:?}"
        );
        anyhow::ensure!(
            manager.unregistered_approval_ids().is_empty(),
            "registration before denial cleanup"
        );
        let process = {
            let records = manager.monitors.lock().await;
            Arc::clone(
                &records
                    .get(&id)
                    .expect("denial record")
                    .process
                    .as_ref()
                    .expect("denial process")
                    .process,
            )
        };
        let pid = timeout(LIMIT, async {
            loop {
                if let Ok(text) = std::fs::read_to_string(&pidfile)
                    && let Ok(pid) = text.trim().parse::<i32>()
                {
                    break pid;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await?;
        anyhow::ensure!(
            unsafe { libc::kill(pid, 0) } == 0,
            "denial child live before trigger"
        );
        eprintln!("network-denial before trigger: pid={pid}, registered={registered:?}");
        std::fs::write(&trigger, "deny")?;
        let death = timeout(LIMIT, async {
            while unsafe { libc::kill(pid, 0) } == 0 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await;
        eprintln!("network-denial child-death stage: {death:?}");
        let cleanup = timeout(LIMIT, async {
            loop {
                let ids = manager.unregistered_approval_ids();
                if !ids.is_empty() {
                    break ids;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await;
        let no_entry = manager.list().await.is_empty();
        let failure = process.failure_message();
        eprintln!("network-denial cleanup={cleanup:?}, no_entry={no_entry}, failure={failure:?}");
        death?;
        anyhow::ensure!(
            cleanup? == registered,
            "exact denial approval unregister once"
        );
        anyhow::ensure!(
            manager.registered_approval_ids() == registered,
            "no additional registration"
        );
        anyhow::ensure!(no_entry, "denied monitor removed");
        anyhow::ensure!(
            failure.is_some_and(
                |text| text.contains("task5-denied.invalid") && text.contains("blocked")
            ),
            "actual network-denial watcher failure"
        );
        Ok(())
    }
    .await;
    let shutdown = timeout(LIMIT, f.session.services.monitor_manager.abort_all()).await;
    Arc::get_mut(&mut f.turn)
        .expect("denial context released")
        .network = None;
    let proxy_shutdown = timeout(LIMIT, controller.shutdown()).await;
    eprintln!("network-denial final cleanup: manager={shutdown:?}, controller={proxy_shutdown:?}");
    observed.expect("real network-denial observations");
    shutdown.expect("network-denial manager cleanup");
    proxy_shutdown
        .expect("network-denial controller deadline")
        .expect("network-denial controller cleanup");
    assert_eq!(
        f.session.services.monitor_manager.slots.available_permits(),
        8
    );
    assert!(f.session.services.monitor_manager.cleanups.is_empty());
}
