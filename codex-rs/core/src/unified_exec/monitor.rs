//! Monitor-owned process reservations, framing pipeline, and session delivery.
use super::async_watcher::TRAILING_OUTPUT_GRACE;
use super::monitor_frame::AttemptNonce;
use super::monitor_frame::DropReason;
use super::monitor_frame::LossCounters;
use super::monitor_frame::LossLedger;
use super::monitor_frame::Notice;
use super::monitor_frame::PartialTail;
use super::monitor_frame::RateBucket;
use super::monitor_frame::Record;
use super::monitor_frame::RecordFramer;
use super::monitor_frame::Stream;
use super::monitor_frame::render_notifications;
use super::process::UnifiedExecProcess;
use crate::context::ContextualUserFragment;
use crate::context::MonitorNotification;
use crate::session::session::Session;
use chrono::DateTime;
use chrono::Utc;
use codex_protocol::models::ResponseItem;
use codex_protocol::turn_input::TurnInput as SubmittedTurnInput;
use codex_protocol::turn_input::TurnInputSubmission;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::Weak;
use std::time::Duration;
use tokio::sync::Mutex;
use tokio::sync::Semaphore;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tokio::time::sleep_until;
use tokio_util::sync::CancellationToken;
use tokio_util::task::AbortOnDropHandle;
use tokio_util::task::TaskTracker;

pub(crate) struct MonitorInfo {
    pub id: String,
    pub description: String,
    pub command: String,
    pub started_at: DateTime<Utc>,
}
struct MonitorRecord {
    description: String,
    command: String,
    started_at: DateTime<Utc>,
    process_id: i32,
    process: Option<MonitorProcess>,
    pipeline: Option<MonitorPipeline>,
    task: Option<AbortOnDropHandle<()>>,
}
pub(crate) struct MonitorManager {
    monitors: Mutex<HashMap<String, MonitorRecord>>,
    slots: Arc<Semaphore>,
    shutdown: CancellationToken,
    gate: Mutex<()>,
    wake: std::sync::Mutex<WakeState>,
    attached: OnceLock<()>,
    retry_task: std::sync::Mutex<Option<AbortOnDropHandle<()>>>,
    stopped: std::sync::Mutex<bool>,
    admissions: TaskTracker,
    starts: TaskTracker,
    cleanups: TaskTracker,
    #[cfg(test)]
    cleanup_gate: std::sync::Mutex<Option<(Arc<tokio::sync::Notify>, Arc<tokio::sync::Notify>)>>,
    #[cfg(test)]
    registered_approvals: std::sync::Mutex<Vec<String>>,
    #[cfg(test)]
    unregistered_approvals: std::sync::Mutex<Vec<String>>,
    #[cfg(test)]
    gate_hooks: std::sync::Mutex<Option<crate::session::GateHooks>>,
}
impl Default for MonitorManager {
    fn default() -> Self {
        Self {
            monitors: Mutex::default(),
            slots: Arc::new(Semaphore::new(8)),
            shutdown: CancellationToken::new(),
            gate: Mutex::default(),
            wake: std::sync::Mutex::default(),
            attached: OnceLock::new(),
            retry_task: std::sync::Mutex::default(),
            stopped: std::sync::Mutex::new(false),
            admissions: TaskTracker::new(),
            starts: TaskTracker::new(),
            cleanups: TaskTracker::new(),
            #[cfg(test)]
            cleanup_gate: std::sync::Mutex::default(),
            #[cfg(test)]
            registered_approvals: std::sync::Mutex::default(),
            #[cfg(test)]
            unregistered_approvals: std::sync::Mutex::default(),
            #[cfg(test)]
            gate_hooks: std::sync::Mutex::default(),
        }
    }
}
#[derive(Default)]
struct WakeState {
    generation: u64,
    pending: bool,
}
impl MonitorManager {
    #[cfg(test)]
    pub(crate) fn registered_approval_ids(&self) -> Vec<String> {
        self.registered_approvals
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
    #[cfg(test)]
    pub(crate) fn unregistered_approval_ids(&self) -> Vec<String> {
        self.unregistered_approvals
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
    #[cfg(test)]
    fn record_unregistered_approval(&self, id: &str) {
        self.unregistered_approvals
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(id.to_owned());
    }

    pub(crate) fn new() -> Self {
        Self::default()
    }

    #[cfg(test)]
    pub(crate) fn install_gate_hooks(&self, hooks: crate::session::GateHooks) {
        *self
            .gate_hooks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(hooks);
    }

    pub(crate) fn wake_pending(&self) -> Option<u64> {
        let wake = self
            .wake
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        wake.pending.then_some(wake.generation)
    }

    /// Attach after construction, so the session owns the retry without a cycle.
    pub(crate) fn attach(&self, session: Weak<Session>) {
        assert!(
            self.attached.set(()).is_ok(),
            "monitor manager attached twice"
        );
        let task = tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(1)).await;
                let Some(session) = session.upgrade() else {
                    break;
                };
                let manager = &session.services.monitor_manager;
                if manager.wake_pending().is_some() {
                    manager.delivery_gate(&session, None).await;
                }
            }
        });
        *self
            .retry_task
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) =
            Some(AbortOnDropHandle::new(task));
    }

    /// Consume one notification exactly once and service an existing obligation.
    pub(crate) async fn deliver(&self, session: &Arc<Session>, item: ResponseItem) {
        debug_assert!(
            self.attached.get().is_some(),
            "delivery before session attachment"
        );
        self.delivery_gate(session, Some(item)).await;
    }

    async fn delivery_gate(&self, session: &Arc<Session>, item: Option<ResponseItem>) {
        let cancelled = CancellationToken::new();
        let _cancel_on_drop = cancelled.clone().drop_guard();
        let operation = {
            let stopped = self
                .stopped
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if *stopped {
                return;
            }
            let session = Arc::clone(session);
            self.admissions.spawn(async move {
                session
                    .services
                    .monitor_manager
                    .run_delivery_gate(&session, item, &cancelled)
                    .await;
            })
        };
        // Unlike AbortOnDropHandle, dropping this waiter does not abort the
        // manager-owned transaction. Admission and its bookkeeping settle
        // together even when the watcher or retry has been stopped.
        if let Err(error) = operation.await {
            std::panic::resume_unwind(error.into_panic());
        }
    }

    #[expect(
        clippy::await_holding_invalid_type,
        reason = "monitor admission and generation updates are serialized across session awaits"
    )]
    async fn run_delivery_gate(
        &self,
        session: &Arc<Session>,
        item: Option<ResponseItem>,
        cancelled: &CancellationToken,
    ) {
        // Lock order: this gate precedes every session lock in admission and
        // recording. No caller may enter it while holding a session lock.
        let _gate = self.gate.lock().await;
        if let Some(generation) = self.wake_pending()
            && !session.is_interrupted()
        {
            let submission = self
                .attempt_start(
                    session,
                    SubmittedTurnInput::UserInput {
                        content: Vec::new(),
                        client_id: None,
                    },
                    cancelled,
                )
                .await;
            if matches!(submission, Some(Ok(TurnInputSubmission::Started { .. }))) {
                let mut wake = self
                    .wake
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if wake.generation == generation {
                    wake.pending = false;
                }
            }
        }
        let Some(item) = item else { return };
        if !session.is_interrupted() {
            let Err(items) = session.inject_if_running(vec![item]).await else {
                return;
            };
            let Some(item) = items.into_iter().next() else {
                unreachable!("inject_if_running returned an empty refused notification");
            };
            if matches!(
                self.attempt_start(
                    session,
                    SubmittedTurnInput::ResponseItem(item.clone()),
                    cancelled
                )
                .await,
                Some(Ok(TurnInputSubmission::Started { .. }))
            ) {
                return;
            }
            session.inject_no_new_turn(vec![item], None).await;
        } else {
            session.inject_no_new_turn(vec![item], None).await;
        }
        let mut wake = self
            .wake
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert!(wake.generation < u64::MAX, "monitor generation overflow");
        wake.generation += 1;
        wake.pending = true;
    }

    // Called only within delivery_gate, after its interrupt hold check.
    async fn attempt_start(
        &self,
        session: &Arc<Session>,
        input: SubmittedTurnInput,
        cancelled: &CancellationToken,
    ) -> Option<codex_protocol::error::Result<TurnInputSubmission>> {
        #[cfg(test)]
        let hooks = self
            .gate_hooks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        #[cfg(test)]
        if let Some(barrier) = hooks.as_ref().and_then(|hooks| hooks.before_start.as_ref()) {
            tokio::select! {
                _ = cancelled.cancelled() => return None,
                _ = async { barrier.wait().await; barrier.wait().await; } => {}
            }
        }
        if cancelled.is_cancelled() {
            return None;
        }
        // StartIfIdle is not cancellation-safe after reserving active_turn.
        // From here through recording/flag updates, this owned task must finish.
        let submission = session.start_turn_if_idle_automatic(input).await;
        #[cfg(test)]
        if let Some(barrier) = hooks.as_ref().and_then(|hooks| hooks.after_start.as_ref()) {
            tokio::select! {
                _ = cancelled.cancelled() => {},
                _ = async { barrier.wait().await; barrier.wait().await; } => {}
            }
        }
        Some(submission)
    }

    pub(crate) fn reserve(&self) -> Option<MonitorSlot> {
        let stopped = self
            .stopped
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if *stopped {
            return None;
        }
        self.slots
            .clone()
            .try_acquire_owned()
            .ok()
            .map(|permit| MonitorSlot { _permit: permit })
    }

    pub(crate) async fn start_with_pipeline(
        &self,
        session: &Arc<Session>,
        context: &super::UnifiedExecContext,
        request: super::ExecCommandRequest,
        description: String,
        pipeline: MonitorPipeline,
    ) -> Result<MonitorId, super::UnifiedExecError> {
        let slot = self.reserve().ok_or_else(|| {
            let stopped = *self
                .stopped
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            super::UnifiedExecError::process_failed(
                if stopped {
                    "session stopped"
                } else {
                    "eight monitors already reserved"
                }
                .into(),
            )
        })?;
        let cancellation = context.cancellation_token.child_token();
        let _cancel_on_drop = cancellation.clone().drop_guard();
        let context = super::UnifiedExecContext::new(
            Arc::clone(session),
            Arc::clone(&context.step_context),
            cancellation,
            format!("monitor-preparation-{}", uuid::Uuid::new_v4()),
        );
        let task = {
            let stopped = self
                .stopped
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if *stopped {
                return Err(super::UnifiedExecError::process_failed(
                    "session stopped".into(),
                ));
            }
            let session = Arc::clone(session);
            self.starts.spawn(async move {
                session
                    .services
                    .monitor_manager
                    .start_reserved(&session, &context, request, description, pipeline, slot)
                    .await
            })
        };
        // Dropping the caller cancels its token, never the runtime's approval
        // cleanup future. Shutdown joins these owned preparations explicitly.
        task.await
            .map_err(|error| super::UnifiedExecError::process_failed(error.to_string()))?
    }

    async fn start_reserved(
        &self,
        session: &Arc<Session>,
        context: &super::UnifiedExecContext,
        request: super::ExecCommandRequest,
        description: String,
        mut pipeline: MonitorPipeline,
        slot: MonitorSlot,
    ) -> Result<MonitorId, super::UnifiedExecError> {
        if let super::UnifiedExecOutputMode::Tagged { sink } = &request.output_mode {
            debug_assert!(
                pipeline
                    .channel
                    .upgrade()
                    .is_some_and(|channel| channel.same_channel(sink))
            );
        } else {
            return Err(super::UnifiedExecError::unsupported("untagged-monitor"));
        }
        let command = request.hook_command.clone();
        let opening = session
            .services
            .unified_exec_manager
            .exec_monitor_command(request, context, slot);
        tokio::pin!(opening);
        // Keep the runtime alive through its network-approval cleanup. User
        // command approvals do not observe cancellation themselves, so resolve
        // only our private preparation ID while continuing to poll the runtime.
        let opened = tokio::select! {
            biased;
            _ = self.shutdown.cancelled() => { context.cancellation_token.cancel(); None }
            _ = context.cancellation_token.cancelled() => None,
            result = &mut opening => Some(result),
        };
        let opened = match opened {
            Some(result) => result,
            None => {
                let mut abort_approval = tokio::time::interval(Duration::from_millis(20));
                loop {
                    tokio::select! {
                        biased;
                        result = &mut opening => break result,
                        _ = abort_approval.tick() => {
                            session.notify_approval(&context.call_id, codex_protocol::protocol::ReviewDecision::Abort).await;
                        }
                    }
                }
            }
        };
        let mut process = opened.map_err(|(error, _slot)| error)?;
        // open_session_with_sandbox returned a registered deferred approval:
        // begin_network_approval awaited register_call before returning it.
        #[cfg(test)]
        if let Some(approval) = &process.network_approval {
            self.registered_approvals
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(approval.registration_id().to_owned());
        }
        let counters = pipeline.commit(process.committed);
        let id = format!("mon_{}", uuid::Uuid::new_v4());
        let mut monitors = self.monitors.lock().await;
        let stopped = *self
            .stopped
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if stopped || context.cancellation_token.is_cancelled() {
            drop(monitors);
            process.cleanup().await;
            return Err(super::UnifiedExecError::process_failed(
                "monitor start cancelled before registration".into(),
            ));
        }
        let (_, empty) = mpsc::channel(1);
        let records = std::mem::replace(&mut pipeline.records, empty);
        #[cfg(test)]
        let unbounded = pipeline.unbounded.take();
        #[cfg(test)]
        let delivery_gate = pipeline.delivery_gate.take();
        #[cfg(test)]
        let lossless = pipeline.lossless;
        #[cfg(test)]
        let path_probe = pipeline.path_probe();
        let child = Arc::clone(&process.process);
        let committed = process.committed;
        let tail = Arc::clone(&pipeline.tail);
        monitors.insert(
            id.clone(),
            MonitorRecord {
                description: description.clone(),
                command,
                started_at: Utc::now(),
                process_id: 0,
                process: Some(process),
                pipeline: Some(pipeline),
                task: None,
            },
        );
        // No await between insertion and installing the task while holding this
        // short registry guard: stop cannot remove an incompletely owned record.
        let task = tokio::spawn(delivery_loop(
            Arc::downgrade(session),
            id.clone(),
            description,
            child,
            committed,
            records,
            counters,
            tail,
            #[cfg(test)]
            unbounded,
            #[cfg(test)]
            delivery_gate,
            #[cfg(test)]
            lossless,
            #[cfg(test)]
            path_probe,
        ));
        if let Some(record) = monitors.get_mut(&id) {
            record.task = Some(AbortOnDropHandle::new(task));
        }
        Ok(id)
    }

    // Compatibility for the two pre-existing registry-only controls. Production
    // registers complete process/pipeline ownership exclusively in start_with_pipeline.
    #[cfg(test)]
    pub(crate) async fn insert(
        &self,
        id: String,
        process_id: i32,
        description: String,
        command: String,
        task: JoinHandle<()>,
    ) {
        self.monitors.lock().await.insert(
            id,
            MonitorRecord {
                description,
                command,
                started_at: Utc::now(),
                process_id,
                process: None,
                pipeline: None,
                task: Some(AbortOnDropHandle::new(task)),
            },
        );
    }

    // The registry lock serializes transfer to cleanup ownership with shutdown.
    // Acquire the token before removing a record; dropping any caller cannot
    // drop the process, approval, pipeline, or slot out of the manager's census.
    async fn take_for_cleanup(&self, id: &str, abort_delivery: bool) -> Option<JoinHandle<i32>> {
        let mut records = self.monitors.lock().await;
        let token = self.cleanups.token();
        let record = records.remove(id)?;
        Some(Self::spawn_cleanup(
            record,
            token,
            self.shutdown.clone(),
            abort_delivery,
        ))
    }

    fn spawn_cleanup(
        mut record: MonitorRecord,
        token: tokio_util::task::task_tracker::TaskTrackerToken,
        shutdown: CancellationToken,
        abort_delivery: bool,
    ) -> JoinHandle<i32> {
        tokio::spawn(async move {
            let _ownership = token;
            if let Some(mut task) = record.task.take() {
                if abort_delivery {
                    task.abort();
                    let _ = task.await;
                } else {
                    // Self-deregister returns before this join. Shutdown can
                    // still abort that caller, including a final admission wait.
                    tokio::select! {
                        biased;
                        _ = shutdown.cancelled() => {
                            task.abort();
                            let _ = task.await;
                        }
                        _ = &mut task => {}
                    }
                }
            }
            if let Some(process) = record.process.as_mut() {
                process.cleanup().await;
            }
            record.pipeline.take();
            let process_id = record.process_id;
            drop(record); // Release the slot before the cleanup tracker token.
            process_id
        })
    }

    pub(crate) async fn remove(&self, id: &str) -> Option<i32> {
        let cleanup = self.take_for_cleanup(id, true).await?;
        let process_id = match cleanup.await {
            Ok(process_id) => Some(process_id),
            Err(error) => {
                tracing::warn!(%error, "monitor cleanup task failed");
                None
            }
        };
        // Cleanup joins the delivery caller first. Its already-owned admission
        // remains independent and settles before this stop returns.
        self.admissions.close();
        self.admissions.wait().await;
        process_id
    }

    pub(crate) async fn deregister_self(&self, id: &str) {
        // Do not await our own delivery handle. The manager-owned cleanup joins
        // it after this function returns and the final notice has been delivered.
        self.take_for_cleanup(id, false).await;
    }

    pub(crate) async fn list(&self) -> Vec<MonitorInfo> {
        self.monitors
            .lock()
            .await
            .iter()
            .map(|(id, record)| MonitorInfo {
                id: id.clone(),
                description: record.description.clone(),
                command: record.command.clone(),
                started_at: record.started_at,
            })
            .collect()
    }

    pub(crate) async fn abort_all(&self) {
        *self
            .stopped
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = true;
        self.shutdown.cancel();
        self.starts.close();
        self.starts.wait().await;
        let retry = self
            .retry_task
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if let Some(retry) = retry {
            retry.abort();
            let _ = retry.await;
        }
        {
            let mut records = self.monitors.lock().await;
            for (_, record) in records.drain() {
                let token = self.cleanups.token();
                Self::spawn_cleanup(record, token, self.shutdown.clone(), true);
            }
            // No starts remain and registration is closed. Every removed record
            // is now tracked, including concurrent stop/self-deregister transfers.
            self.cleanups.close();
        }
        self.cleanups.wait().await;
        self.admissions.close();
        self.admissions.wait().await;
        let _gate = self.gate.lock().await;
        self.wake
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .pending = false;
    }
}

pub(crate) type MonitorId = String;
pub(crate) struct MonitorSlot {
    _permit: tokio::sync::OwnedSemaphorePermit,
}

pub(crate) struct MonitorProcess {
    pub(crate) slot: MonitorSlot,
    pub(crate) process: Arc<UnifiedExecProcess>,
    pub(crate) committed: AttemptNonce,
    pub(crate) cancellation: CancellationToken,
    pub(crate) network_approval: Option<crate::tools::network_approval::DeferredNetworkApproval>,
    pub(crate) denial_watcher: Option<JoinHandle<()>>,
    pub(crate) session: Weak<Session>,
}
impl MonitorProcess {
    async fn cleanup(&mut self) {
        self.cancellation.cancel();
        self.process.terminate();
        #[cfg(test)]
        if let Some(session) = self.session.upgrade() {
            let hook = session
                .services
                .monitor_manager
                .cleanup_gate
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone();
            if let Some((reached, release)) = hook {
                reached.notify_one();
                release.notified().await;
            }
        }
        if let Some(task) = self.denial_watcher.take() {
            task.abort();
            let _ = task.await;
        }
        if let Some(approval) = self.network_approval.take()
            && let Some(session) = self.session.upgrade()
        {
            session
                .services
                .network_approval
                .unregister_call(approval.registration_id())
                .await;
            #[cfg(test)]
            session
                .services
                .monitor_manager
                .record_unregistered_approval(approval.registration_id());
        }
    }
}
impl Drop for MonitorProcess {
    fn drop(&mut self) {
        // Own the gap between successful spawn and registry insertion, including
        // cancellation of the caller while it waits for the registry lock.
        let _ = &self.slot;
        self.cancellation.cancel();
        self.process.terminate();
        if let Some(task) = self.denial_watcher.take() {
            task.abort();
        }
        if let Some(approval) = self.network_approval.take()
            && let Some(session) = self.session.upgrade()
        {
            tokio::spawn(async move {
                session
                    .services
                    .network_approval
                    .unregister_call(approval.registration_id())
                    .await;
                #[cfg(test)]
                session
                    .services
                    .monitor_manager
                    .record_unregistered_approval(approval.registration_id());
            });
        }
    }
}

#[derive(Debug)]
pub(crate) struct TaggedChunk {
    pub(crate) attempt: AttemptNonce,
    pub(crate) stream: Stream,
    pub(crate) bytes: Vec<u8>,
}

type Tail = Arc<std::sync::Mutex<Option<(AttemptNonce, PartialTail)>>>;
pub(crate) struct MonitorPipeline {
    reader: JoinHandle<()>,
    records: mpsc::Receiver<Record>,
    ledger: Arc<LossLedger>,
    channel: mpsc::WeakSender<TaggedChunk>,
    tail: Tail,
    #[cfg(test)]
    unbounded: Option<mpsc::UnboundedReceiver<Record>>,
    #[cfg(test)]
    delivery_gate: Option<Arc<tokio::sync::Notify>>,
    #[cfg(test)]
    lossless: bool,
    #[cfg(test)]
    path_probe: Arc<PipelinePathProbe>,
}

#[cfg(test)]
#[derive(Default)]
pub(crate) struct PipelineHooks {
    pub(crate) reader_gate: Option<Arc<tokio::sync::Notify>>,
    pub(crate) delivery_gate: Option<Arc<tokio::sync::Notify>>,
    pub(crate) lossless_downstream: bool,
}

// Supplemental test observer: reads the actual ledger and records consumed from
// the lossless collector; controls only scheduling, never admission policy.
#[cfg(test)]
pub(crate) struct PipelinePathProbe {
    pub(crate) ledger: Arc<LossLedger>,
    reader: tokio::task::AbortHandle,
    pub(crate) collected: std::sync::Mutex<Vec<Record>>,
    hold_receive: std::sync::atomic::AtomicBool,
    final_loss_parked: std::sync::atomic::AtomicBool,
    final_loss_released: tokio::sync::Notify,
    requested: std::sync::atomic::AtomicBool,
    parked: std::sync::atomic::AtomicBool,
    remaining: std::sync::atomic::AtomicUsize,
    changed: tokio::sync::Notify,
    released: tokio::sync::Notify,
    full_at: std::sync::Mutex<std::time::Instant>,
}
#[cfg(test)]
impl PipelinePathProbe {
    // Hold only normal receipt: the real child exit and grace timer must drive
    // the existing final drain, whose receiver and counter collection remain real.
    pub(crate) fn hold_receive_for_final_loss(&self) {
        self.hold_receive
            .store(true, std::sync::atomic::Ordering::Release);
    }
    pub(crate) async fn wait_final_loss(&self) {
        while !self
            .final_loss_parked
            .load(std::sync::atomic::Ordering::Acquire)
        {
            tokio::task::yield_now().await;
        }
    }
    pub(crate) fn resume_final_loss(&self) {
        self.final_loss_released.notify_one();
    }
    async fn before_final_loss(&self) {
        if self.hold_receive.load(std::sync::atomic::Ordering::Acquire) {
            self.final_loss_parked
                .store(true, std::sync::atomic::Ordering::Release);
            self.final_loss_released.notified().await;
        }
    }

    pub(crate) fn reader_finished(&self) -> bool {
        self.reader.is_finished()
    }
    pub(crate) async fn pause(&self) {
        self.requested
            .store(true, std::sync::atomic::Ordering::Release);
        self.changed.notify_one();
        self.wait_parked().await;
    }
    pub(crate) async fn wait_parked(&self) {
        while !self.parked.load(std::sync::atomic::Ordering::Acquire) {
            tokio::task::yield_now().await;
        }
    }
    pub(crate) async fn refill(&self) {
        assert!(self.parked.load(std::sync::atomic::Ordering::Acquire));
        let at = *self
            .full_at
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        tokio::time::sleep_until(Instant::from_std(at)).await;
    }
    pub(crate) fn resume(&self, records_before_pause: usize) {
        self.remaining
            .store(records_before_pause, std::sync::atomic::Ordering::Release);
        self.parked
            .store(false, std::sync::atomic::Ordering::Release);
        self.released.notify_one();
    }
    async fn checkpoint(&self, bucket: &RateBucket) {
        if self
            .requested
            .swap(false, std::sync::atomic::Ordering::AcqRel)
        {
            *self
                .full_at
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = bucket.full_refill_at();
            self.parked
                .store(true, std::sync::atomic::Ordering::Release);
            self.released.notified().await;
        }
    }
    fn consumed(&self) {
        let remaining = self.remaining.load(std::sync::atomic::Ordering::Acquire);
        if remaining != usize::MAX
            && self
                .remaining
                .fetch_sub(1, std::sync::atomic::Ordering::AcqRel)
                == 1
        {
            self.requested
                .store(true, std::sync::atomic::Ordering::Release);
        }
    }
}

impl MonitorPipeline {
    #[cfg(test)]
    pub(crate) fn path_probe(&self) -> Arc<PipelinePathProbe> {
        Arc::clone(&self.path_probe)
    }

    pub(crate) fn new() -> (Self, mpsc::Sender<TaggedChunk>) {
        #[cfg(test)]
        {
            Self::new_with_hooks(PipelineHooks::default())
        }
        #[cfg(not(test))]
        {
            Self::build()
        }
    }
    #[cfg(test)]
    pub(crate) fn new_with_hooks(hooks: PipelineHooks) -> (Self, mpsc::Sender<TaggedChunk>) {
        Self::build(hooks)
    }

    fn build(#[cfg(test)] hooks: PipelineHooks) -> (Self, mpsc::Sender<TaggedChunk>) {
        let (sink, tagged_rx) = mpsc::channel(128);
        let (records_tx, records) = mpsc::channel(256);
        let ledger = Arc::new(LossLedger::default());
        let tail = Arc::new(std::sync::Mutex::new(None));
        let output = RecordOutput::Bounded(records_tx);
        #[cfg(test)]
        let (output, unbounded) = if hooks.lossless_downstream {
            let (tx, rx) = mpsc::unbounded_channel();
            (RecordOutput::Unbounded(tx), Some(rx))
        } else {
            (output, None)
        };
        let reader = spawn_reader_with_state(
            tagged_rx,
            Arc::clone(&ledger),
            output,
            Arc::clone(&tail),
            #[cfg(test)]
            hooks.reader_gate,
        );
        #[cfg(test)]
        let reader_handle = reader.abort_handle();
        (
            Self {
                reader,
                records,
                ledger: Arc::clone(&ledger),
                channel: sink.downgrade(),
                tail,
                #[cfg(test)]
                unbounded,
                #[cfg(test)]
                delivery_gate: hooks.delivery_gate,
                #[cfg(test)]
                lossless: hooks.lossless_downstream,
                #[cfg(test)]
                path_probe: Arc::new(PipelinePathProbe {
                    ledger: Arc::clone(&ledger),
                    reader: reader_handle,
                    collected: std::sync::Mutex::new(Vec::new()),
                    hold_receive: std::sync::atomic::AtomicBool::new(false),
                    final_loss_parked: std::sync::atomic::AtomicBool::new(false),
                    final_loss_released: tokio::sync::Notify::new(),
                    requested: std::sync::atomic::AtomicBool::new(false),
                    parked: std::sync::atomic::AtomicBool::new(false),
                    remaining: std::sync::atomic::AtomicUsize::new(usize::MAX),
                    changed: tokio::sync::Notify::new(),
                    released: tokio::sync::Notify::new(),
                    full_at: std::sync::Mutex::new(std::time::Instant::now()),
                }),
            },
            sink,
        )
    }
    pub(crate) fn commit(&self, attempt: AttemptNonce) -> Arc<LossCounters> {
        self.ledger.commit(attempt)
    }
}
impl Drop for MonitorPipeline {
    fn drop(&mut self) {
        self.reader.abort();
    }
}

enum RecordOutput {
    Bounded(mpsc::Sender<Record>),
    #[cfg(test)]
    Unbounded(mpsc::UnboundedSender<Record>),
}
impl RecordOutput {
    fn send(&self, record: Record) -> bool {
        match self {
            Self::Bounded(tx) => tx.try_send(record).is_ok(),
            #[cfg(test)]
            Self::Unbounded(tx) => tx.send(record).is_ok(),
        }
    }
}

pub(crate) fn spawn_reader(
    tagged_rx: mpsc::Receiver<TaggedChunk>,
    ledger: Arc<LossLedger>,
    records_tx: mpsc::Sender<Record>,
) -> JoinHandle<()> {
    spawn_reader_with_state(
        tagged_rx,
        ledger,
        RecordOutput::Bounded(records_tx),
        Arc::new(std::sync::Mutex::new(None)),
        #[cfg(test)]
        None,
    )
}

fn spawn_reader_with_state(
    mut tagged_rx: mpsc::Receiver<TaggedChunk>,
    ledger: Arc<LossLedger>,
    output: RecordOutput,
    tail: Tail,
    #[cfg(test)] reader_gate: Option<Arc<tokio::sync::Notify>>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        #[cfg(test)]
        if let Some(gate) = reader_gate {
            gate.notified().await;
        }
        let mut attempt = AttemptNonce::new(0);
        let mut framer = RecordFramer::new(attempt);
        let mut counters = ledger.for_attempt(attempt);
        while let Some(chunk) = tagged_rx.recv().await {
            if chunk.attempt != attempt {
                framer.reset();
                attempt = chunk.attempt;
                framer = RecordFramer::new(attempt);
                counters = ledger.for_attempt(attempt);
            }
            for record in framer.push(chunk.stream, &chunk.bytes, &counters) {
                if !output.send(record) {
                    counters.record(DropReason::ChannelFull);
                }
            }
        }
        *tail
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) =
            framer.finish().map(|tail| (attempt, tail));
    })
}

#[expect(
    clippy::too_many_arguments,
    reason = "owned pipeline state crosses the delivery task boundary together"
)]
async fn delivery_loop(
    session: Weak<Session>,
    id: String,
    description: String,
    process: Arc<UnifiedExecProcess>,
    committed: AttemptNonce,
    mut records: mpsc::Receiver<Record>,
    counters: Arc<LossCounters>,
    tail: Tail,
    #[cfg(test)] mut unbounded: Option<mpsc::UnboundedReceiver<Record>>,
    #[cfg(test)] delivery_gate: Option<Arc<tokio::sync::Notify>>,
    #[cfg(test)] lossless: bool,
    #[cfg(test)] path_probe: Arc<PipelinePathProbe>,
) {
    #[cfg(test)]
    if let Some(gate) = delivery_gate {
        gate.notified().await;
    }
    let mut bucket = RateBucket::new(std::time::Instant::now());
    let mut number = 1;
    let mut pending = Vec::new();
    let mut notices = Vec::new();
    let mut flush_at = None;
    let mut closing_at = None;
    let exit = process.cancellation_token();
    let mut activity_check = tokio::time::interval(Duration::from_millis(50));
    loop {
        #[cfg(test)]
        path_probe.checkpoint(&bucket).await;
        let loss_at = counters
            .gap_open
            .load(std::sync::atomic::Ordering::Acquire)
            .then(|| {
                Instant::from_std(
                    *counters
                        .last_activity
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        + Duration::from_secs(2),
                )
            });
        let received = async {
            #[cfg(test)]
            if path_probe
                .hold_receive
                .load(std::sync::atomic::Ordering::Acquire)
            {
                std::future::pending::<()>().await;
            }
            #[cfg(test)]
            if let Some(rx) = unbounded.as_mut() {
                return rx.recv().await;
            }
            records.recv().await
        };
        tokio::select! {
            _ = async {
                #[cfg(test)] { path_probe.changed.notified().await; }
                #[cfg(not(test))] { std::future::pending::<()>().await; }
            } => {}
            _ = activity_check.tick() => {}
            record = received => {
                let Some(record) = record else { break; };
                if record.attempt != committed { continue; }
                #[cfg(test)]
                {
                    path_probe.consumed();
                    if lossless {
                        path_probe.collected.lock().unwrap_or_else(std::sync::PoisonError::into_inner).push(Record {
                            attempt: record.attempt, stream: record.stream, bytes: record.bytes.clone(),
                        });
                    }
                }
                #[cfg(test)] let admitted = lossless || bucket.admit(std::time::Instant::now());
                #[cfg(not(test))] let admitted = bucket.admit(std::time::Instant::now());
                if admitted {
                    notices.extend(counters.take().into_iter().map(|(reason, count)| Notice::loss(reason, count)));
                    pending.push(record);
                    flush_at.get_or_insert(Instant::now() + Duration::from_millis(200));
                } else {
                    counters.record(DropReason::Rate);
                    if bucket.lossy_windows() >= 3 {
                        notices.push(Notice::flood_stop());
                        process.terminate();
                        break;
                    }
                }
            }
            () = wait_until(flush_at) => {
                flush(&session, &id, &description, &mut number, &mut pending, &mut notices).await;
                flush_at = None;
            }
            () = wait_until(loss_at) => {
                notices.extend(counters.take().into_iter().map(|(reason, count)| Notice::loss(reason, count)));
                flush(&session, &id, &description, &mut number, &mut pending, &mut notices).await;
                flush_at = None;
            }
            () = exit.cancelled(), if closing_at.is_none() => { closing_at = Some(Instant::now() + TRAILING_OUTPUT_GRACE); }
            () = wait_until(closing_at) => break,
        }
    }
    // The reader closes records after draining the tagged source and saving the
    // partial diagnostic tail. A grace expiry still drains already framed rows.
    loop {
        #[cfg(test)]
        let record = match unbounded.as_mut() {
            Some(rx) => rx.try_recv().ok(),
            None => records.try_recv().ok(),
        };
        #[cfg(not(test))]
        let record = records.try_recv().ok();
        let Some(record) = record else {
            break;
        };
        if record.attempt != committed {
            continue;
        }
        #[cfg(test)]
        if lossless {
            path_probe
                .collected
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(Record {
                    attempt: record.attempt,
                    stream: record.stream,
                    bytes: record.bytes.clone(),
                });
        }
        #[cfg(test)]
        let admitted = lossless || bucket.admit(std::time::Instant::now());
        #[cfg(not(test))]
        let admitted = bucket.admit(std::time::Instant::now());
        if admitted {
            pending.push(record);
        } else {
            counters.record(DropReason::Rate);
        }
    }
    #[cfg(test)]
    path_probe.before_final_loss().await;
    notices.extend(
        counters
            .take()
            .into_iter()
            .map(|(reason, count)| Notice::loss(reason, count)),
    );
    flush(
        &session,
        &id,
        &description,
        &mut number,
        &mut pending,
        &mut notices,
    )
    .await;
    let partial = tail
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take()
        .and_then(|(attempt, tail)| (attempt == committed).then_some(tail));
    notices.push(Notice::exit(partial));
    if let Some(session) = session.upgrade() {
        session.services.monitor_manager.deregister_self(&id).await;
    }
    flush(
        &session,
        &id,
        &description,
        &mut number,
        &mut pending,
        &mut notices,
    )
    .await;
}

async fn flush(
    session: &Weak<Session>,
    id: &str,
    description: &str,
    number: &mut u64,
    records: &mut Vec<Record>,
    notices: &mut Vec<Notice>,
) {
    for body in render_notifications(id, description, *number, records, notices) {
        *number = number.saturating_add(1);
        let Some(session) = session.upgrade() else {
            break;
        };
        session
            .services
            .monitor_manager
            .deliver(
                &session,
                ContextualUserFragment::into(MonitorNotification::new(description, body)),
            )
            .await;
    }
    records.clear();
    notices.clear();
}
async fn wait_until(deadline: Option<Instant>) {
    match deadline {
        Some(at) => sleep_until(at).await,
        None => std::future::pending().await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[tokio::test]
    async fn registry_tracks_insert_list_and_remove() {
        let manager = MonitorManager::new();
        let task = tokio::spawn(std::future::pending::<()>());
        let handle = task.abort_handle();
        manager
            .insert(
                "mon_a".to_string(),
                1,
                "watch a".to_string(),
                "cmd a".to_string(),
                task,
            )
            .await;
        manager
            .insert(
                "mon_b".to_string(),
                2,
                "watch b".to_string(),
                "cmd b".to_string(),
                tokio::spawn(async {}),
            )
            .await;

        assert_eq!(manager.list().await.len(), 2);

        // `remove` returns the process id so the caller can terminate it; a
        // second remove of the same id is a no-op.
        assert_eq!(manager.remove("mon_a").await, Some(1));
        assert!(handle.is_finished(), "remove must join the aborted task");
        assert_eq!(manager.remove("mon_a").await, None);

        let remaining = manager.list().await;
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].id, "mon_b");

        manager.abort_all().await;
        assert!(manager.list().await.is_empty());
    }

    #[tokio::test]
    async fn deregister_self_removes_entry_without_aborting_its_task() {
        let manager = MonitorManager::new();
        // A task that runs until aborted, so we can observe whether it survives.
        let task = tokio::spawn(std::future::pending::<()>());
        let handle = task.abort_handle();
        manager
            .insert(
                "mon_x".to_string(),
                7,
                "watch".to_string(),
                "cmd".to_string(),
                task,
            )
            .await;
        assert_eq!(manager.list().await.len(), 1);

        manager.deregister_self("mon_x").await;
        assert!(manager.list().await.is_empty(), "entry pruned");
        // Unlike `remove`, deregister_self must NOT abort the entry's task: the
        // loop removing itself still has its final exit notice to deliver.
        tokio::task::yield_now().await;
        assert!(
            !handle.is_finished(),
            "deregister_self must not abort the entry's task"
        );

        // Deregistering an absent id is a no-op.
        manager.deregister_self("mon_x").await;
        assert!(manager.list().await.is_empty());

        handle.abort();
        manager.abort_all().await;
        assert!(handle.is_finished(), "shutdown joins the self-pruned task");
        assert!(manager.cleanups.is_empty(), "cleanup ownership is drained");
    }
}

#[cfg(test)]
#[path = "monitor_pool_tests.rs"]
mod monitor_pool_tests;
