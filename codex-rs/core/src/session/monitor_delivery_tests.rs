//! Real admission, abort, mailbox, and prompt witnesses for monitor delivery.
use super::*;
use crate::session::step_settings::StepSettingsUpdate;
use crate::state::TaskKind;
use crate::tasks::SessionTask;
use crate::tasks::SessionTaskResult;
use crate::tools::context::ToolCallSource;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolPayload;
use crate::tools::handlers::MonitorHandler;
use crate::tools::registry::ToolExecutor;
use crate::turn_diff_tracker::TurnDiffTracker;
use codex_login::CodexAuth;
use codex_protocol::AgentPath;
use codex_protocol::config_types::ModeKind;
use codex_protocol::models::ContentItem;
use codex_protocol::protocol::InterAgentCommunication;
use codex_protocol::protocol::TurnAbortReason;
use codex_protocol::turn_input::TurnInput as SubmittedTurnInput;
use codex_protocol::turn_input::TurnInputMode;
use codex_protocol::turn_input::TurnInputRequest;
use codex_tools::ToolName;
use core_test_support::responses::ResponseMock;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::mount_sse_once;
use core_test_support::responses::sse;
use core_test_support::responses::start_mock_server;
use core_test_support::responses::{self};
use pretty_assertions::assert_eq;
use std::time::Duration;
use tokio::sync::Barrier;
use tokio::sync::Notify;
use tokio::time::sleep;
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;

const LIMIT: Duration = Duration::from_secs(5);

fn record(text: &str) -> ResponseItem {
    ResponseItem::Message {
        id: None,
        role: "user".into(),
        content: vec![ContentItem::InputText { text: text.into() }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    }
}

struct Fixture {
    session: Arc<Session>,
    turn: Arc<TurnContext>,
    events: async_channel::Receiver<Event>,
    server: wiremock::MockServer,
}

impl Fixture {
    async fn new() -> Self {
        let server = start_mock_server().await;
        let (session, turn, events) = tests::make_session_and_context_with_auth_and_config_and_rx(
            CodexAuth::from_api_key("test-key"),
            Vec::new(),
            |config| {
                config.model_provider.base_url = Some(format!("{}/v1", server.uri()));
                config.model_provider.supports_websockets = false;
            },
        )
        .await;
        session
            .services
            .monitor_manager
            .attach(Arc::downgrade(&session));
        assert!(
            session.active_turn.lock().await.is_none(),
            "fixture starts at observed idle"
        );
        Self {
            session,
            turn,
            events,
            server,
        }
    }

    async fn model(&self) -> ResponseMock {
        mount_sse_once(
            &self.server,
            sse(vec![
                ev_response_created("monitor-response"),
                ev_completed("monitor-response"),
            ]),
        )
        .await
    }

    async fn slow_model(&self) -> ResponseMock {
        responses::mount_response_once(
            &self.server,
            responses::sse_response(sse(vec![
                ev_response_created("held-response"),
                ev_completed("held-response"),
            ]))
            .set_delay(Duration::from_secs(2)),
        )
        .await
    }

    async fn mode(&self, mode: ModeKind) {
        let mut collaboration_mode = self.session.collaboration_mode().await;
        collaboration_mode.mode = mode;
        self.session
            .update_settings(SessionSettingsUpdate {
                step_settings: StepSettingsUpdate {
                    collaboration_mode: Some(collaboration_mode),
                    ..Default::default()
                },
                ..Default::default()
            })
            .await
            .expect("settings-only update");
        assert_eq!(
            self.session
                .thread_settings_snapshot()
                .await
                .collaboration_mode
                .mode,
            mode,
            "applied snapshot barrier"
        );
    }

    async fn deliver(&self, marker: &str) {
        self.session
            .services
            .monitor_manager
            .deliver(&self.session, record(marker))
            .await;
    }

    async fn complete(&self) {
        timeout(LIMIT, async {
            loop {
                let event = self.events.recv().await.expect("session event stream");
                if let EventMsg::Error(error) = &event.msg {
                    panic!("unexpected model error: {error:?}");
                }
                if matches!(event.msg, EventMsg::TurnComplete(_)) {
                    break;
                }
            }
        })
        .await
        .expect("turn completes");
        assert!(
            self.session.active_turn.lock().await.is_none(),
            "completion is an idle barrier"
        );
    }

    async fn quiet(&self, duration: Duration) {
        sleep(duration).await;
        assert!(self.session.active_turn.lock().await.is_none());
        while let Ok(event) = self.events.try_recv() {
            assert!(
                !matches!(event.msg, EventMsg::TurnStarted(_)),
                "unexpected automatic turn"
            );
        }
    }

    async fn user(&self) {
        let result = turn_input::handle(
            &self.session,
            TurnInputRequest::user_input(vec![UserInput::Text {
                text: "resume".into(),
                text_elements: Vec::new(),
            }]),
            TurnInputMode::StartIfIdle,
            new_submission_id(),
        )
        .await
        .expect("user submission");
        assert!(matches!(
            result,
            codex_protocol::turn_input::TurnInputSubmission::Started { .. }
        ));
    }

    async fn held(&self) -> Arc<Notify> {
        let finish = Arc::new(Notify::new());
        let started = Arc::new(Barrier::new(2));
        self.session
            .spawn_task(
                Arc::clone(&self.turn),
                Vec::new(),
                HeldTask {
                    finish: Arc::clone(&finish),
                    started: Arc::clone(&started),
                },
            )
            .await;
        timeout(LIMIT, started.wait())
            .await
            .expect("running task barrier");
        assert!(self.session.active_turn.lock().await.is_some());
        finish
    }

    async fn interrupt(&self, reason: TurnAbortReason) {
        self.held().await;
        self.session.abort_all_tasks(reason).await;
        assert!(self.session.is_interrupted());
        assert!(self.session.active_turn.lock().await.is_none());
        while self.events.try_recv().is_ok() {}
    }

    async fn monitor(&self, arguments: serde_json::Value) {
        self.monitor_result(arguments)
            .await
            .expect("real monitor operation");
    }

    async fn monitor_result(
        &self,
        arguments: serde_json::Value,
    ) -> Result<(), codex_tools::FunctionCallError> {
        let cancellation_token = CancellationToken::new();
        let step_context = self
            .session
            .capture_step_context(Arc::clone(&self.turn), &cancellation_token)
            .await
            .expect("monitor step");
        MonitorHandler
            .handle(ToolInvocation {
                session: Arc::clone(&self.session),
                turn: Arc::clone(&self.turn),
                step_context,
                cancellation_token,
                tracker: Arc::new(tokio::sync::Mutex::new(TurnDiffTracker::new())),
                call_id: new_submission_id(),
                tool_name: ToolName::plain("monitor"),
                source: ToolCallSource::Direct,
                payload: ToolPayload::Function {
                    arguments: arguments.to_string(),
                },
            })
            .await
            .map(|_| ())
    }

    async fn close(&self) {
        handlers::shutdown_session_runtime(&self.session).await;
    }
}

struct HeldTask {
    finish: Arc<Notify>,
    started: Arc<Barrier>,
}
impl SessionTask for HeldTask {
    fn kind(&self) -> TaskKind {
        TaskKind::Regular
    }
    fn span_name(&self) -> &'static str {
        "session_task.monitor_delivery_test"
    }
    async fn run(
        self: Arc<Self>,
        _session: Arc<Session>,
        _ctx: Arc<TurnContext>,
        _input: Vec<TurnInput>,
        cancellation: CancellationToken,
    ) -> SessionTaskResult {
        self.started.wait().await;
        tokio::select! { () = self.finish.notified() => {}, () = cancellation.cancelled() => {} }
        Ok(None)
    }
}

fn copies(mock: &ResponseMock, marker: &str) -> usize {
    mock.single_request()
        .message_input_texts("user")
        .iter()
        .map(|text| text.matches(marker).count())
        .sum()
}

async fn rendezvous(barrier: &Barrier) {
    timeout(LIMIT, barrier.wait())
        .await
        .expect("gate barrier reached");
}

fn start_delivery(session: &Arc<Session>, marker: &'static str) -> tokio::task::JoinHandle<()> {
    let session = Arc::clone(session);
    tokio::spawn(async move {
        session
            .services
            .monitor_manager
            .deliver(&session, record(marker))
            .await
    })
}

// Removing automatic idle admission must fail even when recording succeeds.
#[tokio::test]
async fn wakes_idle_default_mode_without_user_input() {
    let f = Fixture::new().await;
    let prompt = f.model().await;
    f.deliver("IDLE_RECORD").await;
    f.complete().await;
    assert_eq!(copies(&prompt, "IDLE_RECORD"), 1);
    f.close().await;
}

// A process-independent retry must preserve the previously recorded item.
#[tokio::test]
async fn plan_mode_arrival_records_then_wakes_after_settings_only_default() {
    let f = Fixture::new().await;
    f.mode(ModeKind::Plan).await;
    let prompt = f.model().await;
    f.deliver("PLAN_RECORD").await;
    assert!(f.session.services.monitor_manager.wake_pending().is_some());
    f.quiet(Duration::from_millis(1100)).await;
    assert!(prompt.requests().is_empty());
    f.mode(ModeKind::Default).await;
    f.complete().await;
    assert_eq!(copies(&prompt, "PLAN_RECORD"), 1);
    f.close().await;
}

// Terminating the real watcher must not terminate its session's obligation.
#[tokio::test]
async fn exit_while_refused_then_mode_change_still_wakes() {
    let f = Fixture::new().await;
    f.mode(ModeKind::Plan).await;
    let prompt = f.model().await;
    let dir = tempfile::tempdir().unwrap();
    let release = dir.path().join("release");
    let command = format!(
        "while [ ! -f '{}' ]; do sleep 0.02; done; echo EXIT_RECORD",
        release.display()
    );
    f.monitor(
        serde_json::json!({"action":"start", "description":"exit witness", "command":command}),
    )
    .await;
    assert_eq!(f.session.services.monitor_manager.list().await.len(), 1);
    std::fs::write(release, b"go").unwrap();
    timeout(LIMIT, async {
        loop {
            if f.session.services.monitor_manager.list().await.is_empty()
                && f.session.services.monitor_manager.wake_pending().is_some()
            {
                break;
            }
            sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("watcher exits while refused");
    f.quiet(Duration::from_millis(1100)).await;
    f.mode(ModeKind::Default).await;
    f.complete().await;
    assert_eq!(copies(&prompt, "EXIT_RECORD"), 1);
    assert_eq!(copies(&prompt, "watcher exited"), 1);
    f.close().await;
}

// Admission can lose the idle race after D0; D2 must retain a wake obligation.
#[tokio::test]
async fn completion_between_not_idle_and_next_poll_wakes() {
    let f = Fixture::new().await;
    let prompt = f.model().await;
    let before = Arc::new(Barrier::new(2));
    f.session
        .services
        .monitor_manager
        .install_gate_hooks(GateHooks {
            before_start: Some(Arc::clone(&before)),
            after_start: None,
        });
    let delivery = start_delivery(&f.session, "NOT_IDLE_RECORD");
    rendezvous(&before).await;
    let finish = f.held().await;
    rendezvous(&before).await;
    delivery.await.unwrap();
    assert!(f.session.services.monitor_manager.wake_pending().is_some());
    finish.notify_one();
    f.complete().await;
    f.complete().await;
    assert_eq!(copies(&prompt, "NOT_IDLE_RECORD"), 1);
    f.close().await;
}

// Trigger-turn mail wins admission; the monitor wakes after that real turn.
#[tokio::test]
async fn mailbox_competition_defers_then_wakes() {
    let f = Fixture::new().await;
    let mailbox = f.slow_model().await;
    let wake = f.model().await;
    f.session
        .input_queue
        .enqueue_mailbox_communication(
            InterAgentCommunication::new(
                AgentPath::root(),
                AgentPath::root(),
                Vec::new(),
                "mailbox witness".into(),
                true,
            ),
            Default::default(),
        )
        .await;
    f.deliver("MAIL_RECORD").await;
    assert!(f.session.services.monitor_manager.wake_pending().is_some());
    f.session.maybe_start_turn_for_pending_work().await;
    f.complete().await;
    f.complete().await;
    assert_eq!(
        (
            copies(&mailbox, "MAIL_RECORD"),
            copies(&wake, "MAIL_RECORD")
        ),
        (1, 1)
    );
    f.close().await;
}

// D0 consumes pending input without setting a new obligation.
#[tokio::test]
async fn inject_if_running_consumption_sets_no_flag() {
    let f = Fixture::new().await;
    let finish = f.held().await;
    f.deliver("RUNNING_RECORD").await;
    assert_eq!(f.session.services.monitor_manager.wake_pending(), None);
    finish.notify_one();
    f.complete().await;
    let prompt = f.model().await;
    f.user().await;
    f.complete().await;
    assert_eq!(copies(&prompt, "RUNNING_RECORD"), 1);
    f.close().await;
}

async fn pending_wake(f: &Fixture, marker: &str) -> (u64, Arc<Barrier>) {
    f.mode(ModeKind::Plan).await;
    f.deliver(marker).await;
    let generation = f
        .session
        .services
        .monitor_manager
        .wake_pending()
        .expect("pending generation");
    // Install only after Plan is applied; the next retry is now the controlled wake.
    let after = Arc::new(Barrier::new(2));
    f.session
        .services
        .monitor_manager
        .install_gate_hooks(GateHooks {
            before_start: None,
            after_start: Some(Arc::clone(&after)),
        });
    f.mode(ModeKind::Default).await;
    rendezvous(&after).await;
    (generation, after)
}

async fn queued_delivery(
    session: &Arc<Session>,
    marker: &'static str,
) -> tokio::task::JoinHandle<()> {
    let (arrived, waiting) = tokio::sync::oneshot::channel();
    let session = Arc::clone(session);
    let delivery = tokio::spawn(async move {
        arrived.send(()).unwrap();
        session
            .services
            .monitor_manager
            .deliver(&session, record(marker))
            .await;
    });
    waiting.await.unwrap();
    // The gate is held at after_start; this task cannot finish its delivery.
    tokio::task::yield_now().await;
    assert!(
        !delivery.is_finished(),
        "delivery waits on the in-flight wake gate"
    );
    delivery
}

// Clear precedes the waiting delivery's real PlanMode refusal and generation increment.
#[tokio::test]
async fn delivery_during_in_flight_wake_resets_flag_afterwards() {
    let f = Fixture::new().await;
    let first = f.model().await;
    let (generation, after) = pending_wake(&f, "FIRST_RECORD").await;
    let delivery = queued_delivery(&f.session, "SECOND_RECORD").await;
    f.mode(ModeKind::Plan).await;
    f.complete().await;
    rendezvous(&after).await;
    delivery.await.unwrap();
    assert_eq!(
        f.session.services.monitor_manager.wake_pending(),
        Some(generation + 1)
    );
    assert_eq!(copies(&first, "FIRST_RECORD"), 1);
    let second = f.model().await;
    f.mode(ModeKind::Default).await;
    f.complete().await;
    assert_eq!(
        (
            copies(&second, "FIRST_RECORD"),
            copies(&second, "SECOND_RECORD")
        ),
        (1, 1)
    );
    f.close().await;
}

// The complementary D0 path must not manufacture another wake obligation.
#[tokio::test]
async fn delivery_accepted_into_wake_turn_leaves_flag_clear() {
    let f = Fixture::new().await;
    let first = f.slow_model().await;
    let continuation = f.model().await;
    let (_, after) = pending_wake(&f, "ACCEPT_FIRST").await;
    let delivery = queued_delivery(&f.session, "ACCEPT_SECOND").await;
    timeout(LIMIT, async {
        while first.requests().is_empty() {
            sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("first prompt is in flight before D0 accepts the waiting delivery");
    assert!(f.session.active_turn.lock().await.is_some());
    rendezvous(&after).await;
    delivery.await.unwrap();
    assert_eq!(f.session.services.monitor_manager.wake_pending(), None);
    f.complete().await;
    assert_eq!(copies(&first, "ACCEPT_FIRST"), 1);
    assert_eq!(copies(&continuation, "ACCEPT_SECOND"), 1);
    f.close().await;
}

// Interrupt between the predicate read and admission allows only the accepted single race.
#[tokio::test]
async fn interrupt_racing_start_boundary_at_most_one_turn() {
    let f = Fixture::new().await;
    let prompt = f.model().await;
    let before = Arc::new(Barrier::new(2));
    f.session
        .services
        .monitor_manager
        .install_gate_hooks(GateHooks {
            before_start: Some(Arc::clone(&before)),
            after_start: None,
        });
    let delivery = start_delivery(&f.session, "INTERRUPT_RACE");
    rendezvous(&before).await;
    f.interrupt(TurnAbortReason::Interrupted).await;
    rendezvous(&before).await;
    delivery.await.unwrap();
    f.complete().await;
    f.quiet(Duration::from_millis(1100)).await;
    assert_eq!(copies(&prompt, "INTERRUPT_RACE"), 1);
    assert_eq!(f.session.services.monitor_manager.wake_pending(), None);
    f.close().await;
}

// A fresh held arrival records but cannot initiate an automatic restart.
#[tokio::test]
async fn fresh_arrival_while_interrupted_starts_no_turn() {
    let f = Fixture::new().await;
    f.interrupt(TurnAbortReason::Interrupted).await;
    assert_eq!(f.session.services.monitor_manager.wake_pending(), None);
    let prompt = f.model().await;
    f.deliver("HELD_FRESH").await;
    f.quiet(Duration::from_secs(5)).await;
    assert!(prompt.requests().is_empty());
    f.user().await;
    f.complete().await;
    assert_eq!(copies(&prompt, "HELD_FRESH"), 1);
    f.close().await;
}

// A pending obligation must honor an interrupt introduced after it was recorded.
#[tokio::test]
async fn pending_retry_holds_while_interrupted() {
    let f = Fixture::new().await;
    f.mode(ModeKind::Plan).await;
    f.deliver("HELD_PENDING").await;
    let generation = f.session.services.monitor_manager.wake_pending().unwrap();
    f.interrupt(TurnAbortReason::Interrupted).await;
    f.mode(ModeKind::Default).await;
    let prompt = f.model().await;
    f.quiet(Duration::from_millis(2100)).await;
    assert_eq!(
        f.session.services.monitor_manager.wake_pending(),
        Some(generation)
    );
    assert!(prompt.requests().is_empty());
    f.user().await;
    f.complete().await;
    assert_eq!(copies(&prompt, "HELD_PENDING"), 1);
    f.close().await;
}

// Budget abort is the same transient hold as the user interruption path.
#[tokio::test]
async fn budget_limited_abort_is_held_like_interrupt() {
    let f = Fixture::new().await;
    f.interrupt(TurnAbortReason::BudgetLimited).await;
    let prompt = f.model().await;
    f.deliver("BUDGET_HELD").await;
    f.quiet(Duration::from_millis(2100)).await;
    assert!(prompt.requests().is_empty());
    f.user().await;
    f.complete().await;
    assert_eq!(copies(&prompt, "BUDGET_HELD"), 1);
    f.close().await;
}

// A queued-user carrier through StartIfIdle clears Interrupted before retry.
#[tokio::test]
async fn queued_turn_clears_hold_then_retry_fires() {
    let f = Fixture::new().await;
    f.interrupt(TurnAbortReason::Interrupted).await;
    f.deliver("QUEUE_HELD").await;
    let queued = f.slow_model().await;
    let wake = f.model().await;
    let submission = turn_input::handle(
        &f.session,
        TurnInputRequest::new(SubmittedTurnInput::UserInput {
            content: vec![UserInput::Text {
                text: "queued resume".into(),
                text_elements: Vec::new(),
            }],
            client_id: Some("queued-user-message".into()),
        }),
        TurnInputMode::StartIfIdle,
        new_submission_id(),
    )
    .await
    .expect("queued turn submission");
    assert!(matches!(
        submission,
        codex_protocol::turn_input::TurnInputSubmission::Started { .. }
    ));
    f.complete().await;
    assert!(!f.session.is_interrupted());
    f.complete().await;
    assert_eq!(
        (copies(&queued, "QUEUE_HELD"), copies(&wake, "QUEUE_HELD")),
        (1, 1)
    );
    assert_eq!(f.session.services.monitor_manager.wake_pending(), None);
    f.close().await;
}

// Stopping the real watcher may not clear recorded session-owned work.
#[tokio::test]
#[expect(
    clippy::await_holding_invalid_type,
    reason = "the test holds the real start_task mutex to witness cancellation after reservation"
)]
async fn monitor_stop_leaves_flag() {
    let f = Fixture::new().await;
    f.mode(ModeKind::Plan).await;
    f.monitor(
        serde_json::json!({"action":"start", "description":"stop witness", "command":"sleep 60"}),
    )
    .await;
    let id = f
        .session
        .services
        .monitor_manager
        .list()
        .await
        .pop()
        .unwrap()
        .id;
    f.deliver("STOP_RECORD").await;
    let generation = f.session.services.monitor_manager.wake_pending().unwrap();
    f.monitor(serde_json::json!({"action":"stop", "id":id}))
        .await;
    assert_eq!(
        f.session.services.monitor_manager.wake_pending(),
        Some(generation)
    );
    assert!(f.session.services.monitor_manager.list().await.is_empty());
    let prompt = f.model().await;
    f.mode(ModeKind::Default).await;
    f.complete().await;
    assert_eq!(copies(&prompt, "STOP_RECORD"), 1);
    f.close().await;

    // Removal must not abort admission after it owns the notification.
    let f = Fixture::new().await;
    let prompt = f.slow_model().await;
    let start_guard = f
        .session
        .services
        .guardian_rejection_circuit_breaker
        .lock()
        .await;
    f.monitor(serde_json::json!({"action":"start", "description":"reserved stop", "command":"echo RESERVED_STOP; sleep 60"})).await;
    let id = f
        .session
        .services
        .monitor_manager
        .list()
        .await
        .pop()
        .unwrap()
        .id;
    reserved_without_task(&f.session).await;
    let stop = f.monitor(serde_json::json!({"action":"stop", "id":id}));
    tokio::pin!(stop);
    assert!(
        timeout(Duration::from_millis(100), &mut stop)
            .await
            .is_err(),
        "stop must join admission instead of abandoning a taskless reservation"
    );
    drop(start_guard);
    timeout(LIMIT, &mut stop)
        .await
        .expect("stop settles owned admission");
    f.complete().await;
    assert_eq!(copies(&prompt, "RESERVED_STOP"), 1);
    assert!(f.session.active_turn.lock().await.is_none());
    let user = f.model().await;
    f.user().await;
    f.complete().await;
    assert_eq!(copies(&user, "RESERVED_STOP"), 1);
    f.close().await;
}

// Hold an existing production mutex reached after reservation/input transfer,
// before start_task installs the RegularTask. No synthetic admission is used.
async fn reserved_without_task(session: &Arc<Session>) {
    timeout(LIMIT, async {
        loop {
            if session
                .active_turn
                .lock()
                .await
                .as_ref()
                .is_some_and(|turn| turn.task.is_none())
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("real admission reserved a turn before task installation");
}

// Shutdown must cancel a retry currently suspended inside its gate, not merely clear a flag.
#[tokio::test]
#[expect(
    clippy::await_holding_invalid_type,
    reason = "the test holds the real start_task mutex to witness cancellation after reservation"
)]
async fn shutdown_aborts_retry_task() {
    let f = Fixture::new().await;
    f.mode(ModeKind::Plan).await;
    f.deliver("SHUTDOWN_RECORD").await;
    let before = Arc::new(Barrier::new(2));
    f.session
        .services
        .monitor_manager
        .install_gate_hooks(GateHooks {
            before_start: Some(Arc::clone(&before)),
            after_start: None,
        });
    f.mode(ModeKind::Default).await;
    rendezvous(&before).await;
    timeout(LIMIT, handlers::shutdown_session_runtime(&f.session))
        .await
        .expect("shutdown cancels blocked retry before waiting on its gate");
    assert_eq!(
        (
            Arc::strong_count(&before),
            f.session.services.monitor_manager.wake_pending()
        ),
        (1, None)
    );
    let prompt = f.model().await;
    f.quiet(Duration::from_millis(1100)).await;
    assert!(prompt.requests().is_empty());

    // The retry also owns admission after reservation, when caller cancellation
    // alone cannot unwind the turn. Shutdown must drain, then sweep that task.
    let f = Fixture::new().await;
    f.mode(ModeKind::Plan).await;
    f.deliver("RESERVED_SHUTDOWN").await;
    let prompt = f.slow_model().await;
    let start_guard = f
        .session
        .services
        .guardian_rejection_circuit_breaker
        .lock()
        .await;
    f.mode(ModeKind::Default).await;
    reserved_without_task(&f.session).await;
    let shutdown = handlers::shutdown_session_runtime(&f.session);
    tokio::pin!(shutdown);
    assert!(
        timeout(Duration::from_millis(100), &mut shutdown)
            .await
            .is_err()
    );
    assert!(
        f.session
            .active_turn
            .lock()
            .await
            .as_ref()
            .is_some_and(|turn| turn.task.is_none()),
        "shutdown must not sweep a reservation while its owned admission can still start"
    );
    drop(start_guard);
    timeout(LIMIT, &mut shutdown)
        .await
        .expect("shutdown drains admission then aborts its task");
    assert!(f.session.active_turn.lock().await.is_none());
    assert_eq!(f.session.services.monitor_manager.wake_pending(), None);
    let requests = prompt.requests().len();
    sleep(Duration::from_millis(1100)).await;
    assert_eq!(
        prompt.requests().len(),
        requests,
        "no model task remains after shutdown"
    );
    assert!(requests <= 1);
    assert!(f.session.active_turn.lock().await.is_none());
}

// Several refused deliveries and polls must never reinsert already-recorded items.
#[tokio::test]
async fn record_never_resubmitted() {
    let f = Fixture::new().await;
    f.mode(ModeKind::Plan).await;
    f.deliver("EXACTLY_ONCE_A").await;
    f.deliver("EXACTLY_ONCE_B").await;
    f.quiet(Duration::from_millis(2100)).await;
    let first = f.slow_model().await;
    let continuation = f.model().await;
    f.mode(ModeKind::Default).await;
    timeout(LIMIT, async {
        while first.requests().is_empty() {
            sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    f.deliver("EXACTLY_ONCE_C").await;
    f.complete().await;
    assert_eq!(
        (
            copies(&continuation, "EXACTLY_ONCE_A"),
            copies(&continuation, "EXACTLY_ONCE_B"),
            copies(&continuation, "EXACTLY_ONCE_C")
        ),
        (1, 1, 1)
    );
    f.close().await;
}

// Registry operations and shutdown must remain live alongside gate/session-lock acquisition.
#[tokio::test]
#[expect(
    clippy::await_holding_invalid_type,
    reason = "the test holds the real start_task mutex to witness cancellation after reservation"
)]
#[expect(clippy::print_stderr, reason = "retained stage-specific gate evidence")]
async fn concurrent_start_stop_shutdown_completes_under_5s() {
    let f = Fixture::new().await;
    f.monitor(serde_json::json!({"action":"start", "description":"existing watcher", "command":"sleep 60"})).await;
    let existing = f
        .session
        .services
        .monitor_manager
        .list()
        .await
        .pop()
        .expect("existing monitor")
        .id;
    let prompt = f.model().await;
    let before = Arc::new(Barrier::new(2));
    f.session
        .services
        .monitor_manager
        .install_gate_hooks(GateHooks {
            before_start: Some(Arc::clone(&before)),
            after_start: None,
        });
    let delivery = start_delivery(&f.session, "CONCURRENT_RECORD");
    rendezvous(&before).await;
    timeout(LIMIT, async {
        let start_a = f.monitor_result(
            serde_json::json!({"action":"start", "description":"race a", "command":"sleep 60"}),
        );
        let start_b = f.monitor_result(
            serde_json::json!({"action":"start", "description":"race b", "command":"sleep 60"}),
        );
        let stop = f.monitor(serde_json::json!({"action":"stop", "id":existing}));
        let shutdown = f.session.services.monitor_manager.abort_all();
        let release = async {
            rendezvous(&before).await;
            delivery.await.unwrap();
        };
        let (a, b, (), (), ()) = tokio::join!(start_a, start_b, stop, shutdown, release);
        for result in [a, b] {
            if let Err(error) = result {
                let codex_tools::FunctionCallError::RespondToModel(message) = error else { panic!("unexpected raced start failure: {error:?}"); };
                assert!(!message.contains("eight monitors already reserved"), "raced shutdown must not report capacity: {message}");
                assert!([
                    "failed to start monitor: Unified exec process failed: monitor preparation cancelled",
                    "failed to start monitor: Unified exec process failed: monitor start cancelled before registration",
                    "failed to start monitor: Unified exec process failed: session stopped",
                ].contains(&message.as_str()), "unexpected raced start result: {message}");
                eprintln!("raced start cancelled by shutdown: {message}");
            }
        }
    })
    .await
    .expect("two starts, stop, shutdown and in-flight delivery finish within 5s");
    f.complete().await;
    assert_eq!(copies(&prompt, "CONCURRENT_RECORD"), 1);
    f.close().await;
    assert!(f.session.active_turn.lock().await.is_none());

    // Repeat the lifecycle competition at the real reservation boundary, with
    // shutdown's final task sweep included in the raced operation.
    let f = Fixture::new().await;
    f.monitor(serde_json::json!({"action":"start", "description":"reserved existing", "command":"sleep 60"})).await;
    let existing = f
        .session
        .services
        .monitor_manager
        .list()
        .await
        .pop()
        .unwrap()
        .id;
    let _prompt = f.slow_model().await;
    let start_guard = f
        .session
        .services
        .guardian_rejection_circuit_breaker
        .lock()
        .await;
    let delivery = start_delivery(&f.session, "RESERVED_CONCURRENT");
    reserved_without_task(&f.session).await;
    timeout(LIMIT, async {
        let shutdown = handlers::shutdown_session_runtime(&f.session);
        tokio::pin!(shutdown);
        assert!(
            timeout(Duration::from_millis(100), &mut shutdown)
                .await
                .is_err()
        );
        assert!(
            f.session
                .active_turn
                .lock()
                .await
                .as_ref()
                .is_some_and(|turn| turn.task.is_none())
        );
        let start_a = f.monitor_result(
            serde_json::json!({"action":"start", "description":"reserved a", "command":"sleep 60"}),
        );
        let start_b = f.monitor_result(
            serde_json::json!({"action":"start", "description":"reserved b", "command":"sleep 60"}),
        );
        let stop = f.monitor(serde_json::json!({"action":"stop", "id":existing}));
        let release = async {
            drop(start_guard);
            delivery.await.unwrap();
        };
        let (a, b, (), (), ()) = tokio::join!(start_a, start_b, stop, shutdown, release);
        for result in [a, b] {
            if let Err(error) = result {
                let codex_tools::FunctionCallError::RespondToModel(message) = error else { panic!("unexpected raced start failure: {error:?}"); };
                assert!(!message.contains("eight monitors already reserved"), "raced shutdown must not report capacity: {message}");
                assert!([
                    "failed to start monitor: Unified exec process failed: monitor preparation cancelled",
                    "failed to start monitor: Unified exec process failed: monitor start cancelled before registration",
                    "failed to start monitor: Unified exec process failed: session stopped",
                ].contains(&message.as_str()), "unexpected raced start result: {message}");
                eprintln!("raced start cancelled by shutdown: {message}");
            }
        }
    })
    .await
    .expect("reserved admission and lifecycle operations quiesce within 5s");
    assert!(f.session.active_turn.lock().await.is_none());
    assert!(f.session.services.monitor_manager.list().await.is_empty());
    assert_eq!(f.session.services.monitor_manager.wake_pending(), None);
}
