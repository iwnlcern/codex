//! Bounded record framing and delivery policy; orchestration belongs to the pipeline.
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::PoisonError;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::Instant;

use crate::context::ContextualUserFragment;
use crate::context::MonitorNotification;

pub(crate) const MAX_RECORD_BYTES: usize = 4096;
pub(crate) const MAX_NOTIFICATION_BYTES: usize = 8192;
const MAX_NOTICE_BYTES: usize = 1024;
const WINDOW_SECONDS: u64 = 10;
pub(crate) const REPLAY_INSTRUCTION: &str = "Run B2's replay for EVERY binding this session has held (all retained root/seat floors, not only the current binding); for each replayed row, check whether its addressed work has already been handled before acting.";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct AttemptNonce(u64);

impl AttemptNonce {
    pub(crate) fn new(value: u64) -> Self {
        Self(value)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Stream {
    Stdout,
    Stderr,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Record {
    pub(crate) attempt: AttemptNonce,
    pub(crate) stream: Stream,
    pub(crate) bytes: Vec<u8>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DropReason {
    Oversize,
    InvalidUtf8,
    ChannelFull,
    Rate,
}

impl DropReason {
    fn site(self) -> &'static str {
        match self {
            Self::Oversize => "oversize",
            Self::InvalidUtf8 => "invalid-utf8",
            Self::ChannelFull => "channel-full",
            Self::Rate => "rate",
        }
    }
}

pub(crate) struct LossCounters {
    counts: [AtomicU64; 4],
    pub(crate) gap_open: AtomicBool,
    pub(crate) last_activity: Mutex<Instant>,
}

impl Default for LossCounters {
    fn default() -> Self {
        Self {
            counts: std::array::from_fn(|_| AtomicU64::new(0)),
            gap_open: AtomicBool::new(false),
            last_activity: Mutex::new(Instant::now()),
        }
    }
}

impl LossCounters {
    pub(crate) fn activity(&self, now: Instant) {
        *self
            .last_activity
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = now;
    }

    pub(crate) fn record(&self, reason: DropReason) {
        // Serialize gap changes with snapshotting so a concurrent new loss cannot
        // have its gap flag cleared by the delivery task's previous snapshot.
        let _activity = self
            .last_activity
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        self.counts[reason as usize].fetch_add(1, Ordering::Relaxed);
        self.gap_open.store(true, Ordering::Release);
    }

    pub(crate) fn take(&self) -> Vec<(DropReason, u64)> {
        let _activity = self
            .last_activity
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        self.gap_open.store(false, Ordering::Release);
        [
            DropReason::Oversize,
            DropReason::InvalidUtf8,
            DropReason::ChannelFull,
            DropReason::Rate,
        ]
        .into_iter()
        .filter_map(|reason| {
            let count = self.counts[reason as usize].swap(0, Ordering::Relaxed);
            (count != 0).then_some((reason, count))
        })
        .collect()
    }
}

#[derive(Default)]
pub(crate) struct LossLedger {
    attempts: Mutex<HashMap<AttemptNonce, Arc<LossCounters>>>,
}

impl LossLedger {
    pub(crate) fn for_attempt(&self, attempt: AttemptNonce) -> Arc<LossCounters> {
        self.attempts
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .entry(attempt)
            .or_default()
            .clone()
    }

    pub(crate) fn commit(&self, attempt: AttemptNonce) -> Arc<LossCounters> {
        let mut attempts = self.attempts.lock().unwrap_or_else(PoisonError::into_inner);
        attempts.retain(|nonce, _| *nonce == attempt);
        attempts.entry(attempt).or_default().clone()
    }
}

#[derive(Default)]
struct StreamBuffer {
    bytes: Vec<u8>,
    discarding: bool,
}

pub(crate) struct PartialTail {
    pub(crate) stdout: Vec<u8>,
    pub(crate) stderr: Vec<u8>,
}

pub(crate) struct RecordFramer {
    attempt: AttemptNonce,
    stdout: StreamBuffer,
    stderr: StreamBuffer,
}

impl RecordFramer {
    pub(crate) fn new(attempt: AttemptNonce) -> Self {
        Self {
            attempt,
            stdout: StreamBuffer::default(),
            stderr: StreamBuffer::default(),
        }
    }

    pub(crate) fn push(
        &mut self,
        stream: Stream,
        chunk: &[u8],
        counters: &LossCounters,
    ) -> Vec<Record> {
        counters.activity(Instant::now());
        let buffer = match stream {
            Stream::Stdout => &mut self.stdout,
            Stream::Stderr => &mut self.stderr,
        };
        let mut records = Vec::new();
        for &byte in chunk {
            if byte == b'\n' {
                if !buffer.discarding {
                    let bytes = match std::str::from_utf8(&buffer.bytes) {
                        Ok(_) => Some(buffer.bytes.clone()),
                        Err(_) => match stream {
                            Stream::Stdout => {
                                counters.record(DropReason::InvalidUtf8);
                                None
                            }
                            Stream::Stderr => {
                                let decoded = format!(
                                    "[invalid-utf8] {}",
                                    String::from_utf8_lossy(&buffer.bytes)
                                );
                                if decoded.len() <= MAX_RECORD_BYTES {
                                    Some(decoded.into_bytes())
                                } else {
                                    counters.record(DropReason::Oversize);
                                    None
                                }
                            }
                        },
                    };
                    if let Some(bytes) = bytes {
                        records.push(Record {
                            attempt: self.attempt,
                            stream,
                            bytes,
                        });
                    }
                }
                buffer.bytes.clear();
                buffer.discarding = false;
            } else if !buffer.discarding {
                if buffer.bytes.len() == MAX_RECORD_BYTES {
                    buffer.bytes.clear();
                    buffer.discarding = true;
                    counters.record(DropReason::Oversize);
                } else {
                    buffer.bytes.push(byte);
                }
            }
        }
        records
    }

    pub(crate) fn finish(&mut self) -> Option<PartialTail> {
        let tail = PartialTail {
            stdout: self.stdout.bytes.clone(),
            stderr: self.stderr.bytes.clone(),
        };
        self.reset();
        (!tail.stdout.is_empty() || !tail.stderr.is_empty()).then_some(tail)
    }

    pub(crate) fn reset(&mut self) {
        self.stdout.bytes.clear();
        self.stdout.discarding = false;
        self.stderr.bytes.clear();
        self.stderr.discarding = false;
    }
}

pub(crate) struct RateBucket {
    start: Instant,
    last_refill: Instant,
    tokens: f64,
    window: u64,
    window_lossy: bool,
    consecutive: u32,
}

impl RateBucket {
    pub(crate) fn new(start: Instant) -> Self {
        Self {
            start,
            last_refill: start,
            tokens: 200.0,
            window: 0,
            window_lossy: false,
            consecutive: 0,
        }
    }

    pub(crate) fn admit(&mut self, now: Instant) -> bool {
        let window = now.saturating_duration_since(self.start).as_secs() / WINDOW_SECONDS;
        if window > self.window {
            if window > self.window + 1 || !self.window_lossy {
                self.consecutive = 0;
            }
            self.window = window;
            self.window_lossy = false;
        }
        self.tokens = (self.tokens
            + now
                .saturating_duration_since(self.last_refill)
                .as_secs_f64()
                * 20.0)
            .min(200.0);
        self.last_refill = self.last_refill.max(now);
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            true
        } else {
            if !self.window_lossy {
                self.consecutive = self.consecutive.saturating_add(1);
                self.window_lossy = true;
            }
            false
        }
    }

    pub(crate) fn lossy_windows(&self) -> u32 {
        self.consecutive
    }
}

pub(crate) struct Notice {
    text: String,
}

impl Notice {
    pub(crate) fn loss(reason: DropReason, count: u64) -> Self {
        Self {
            text: format!(
                "MONITOR-NOTICE: loss {} records={count}; {REPLAY_INSTRUCTION}",
                reason.site()
            ),
        }
    }

    pub(crate) fn flood_stop() -> Self {
        Self {
            text: "MONITOR-NOTICE: flood-stop".to_string(),
        }
    }

    pub(crate) fn exit(tail: Option<PartialTail>) -> Self {
        let mut text = "MONITOR-NOTICE: exit".to_string();
        if let Some(tail) = tail {
            for (stream, bytes) in [("stdout", tail.stdout), ("stderr", tail.stderr)] {
                if !bytes.is_empty() {
                    // JSON quoting bounds escaped diagnostics, including NULs,
                    // without making a partial stdout tail into a delivered row.
                    let decoded = String::from_utf8_lossy(&bytes);
                    let mut diagnostic = String::new();
                    for character in decoded.chars() {
                        diagnostic.push(character);
                        if serde_json::Value::String(diagnostic.clone())
                            .to_string()
                            .len()
                            > 380
                        {
                            diagnostic.pop();
                            diagnostic.push('…');
                            break;
                        }
                    }
                    text.push_str(&format!(
                        " partial-{stream}={}",
                        serde_json::Value::String(diagnostic.clone())
                    ));
                }
            }
        }
        debug_assert!(text.len() <= MAX_NOTICE_BYTES);
        Self { text }
    }
}

fn close_block(stream: Option<Stream>) -> &'static str {
    match stream {
        Some(Stream::Stdout) => "</stdout>\n",
        Some(Stream::Stderr) => "</stderr>\n",
        None => "",
    }
}

pub(crate) fn render_notifications(
    id: &str,
    description: &str,
    delivery: u64,
    records: &[Record],
    notices: &[Notice],
) -> Vec<String> {
    // Start validates descriptions; bound header fields defensively as well.
    let description: String = description
        .chars()
        .scan(0, |used, ch| {
            *used += ch.len_utf8();
            (*used <= 256).then_some(ch)
        })
        .collect::<String>()
        .replace(['\n', '\r'], " ");
    let id: String = id
        .chars()
        .take(128)
        .collect::<String>()
        .replace(['\n', '\r'], " ");
    let wrapper_bytes = MonitorNotification::new(&description, "").render().len();
    let budget = MAX_NOTIFICATION_BYTES - wrapper_bytes;
    let mut number = delivery;
    let mut header = format!("monitor {id} {description} delivery {number}\n");
    let mut body = header.clone();
    let mut stream = None;
    let mut items = Vec::new();
    for record in records {
        #[expect(
            clippy::expect_used,
            reason = "Only validated RecordFramer records enter the delivery queue"
        )]
        let row = std::str::from_utf8(&record.bytes).expect("framer emits valid UTF-8");
        assert!(
            row.len() <= MAX_RECORD_BYTES,
            "framer emits bounded records"
        );
        let opening = match record.stream {
            Stream::Stdout => "<stdout>\n",
            Stream::Stderr => "<stderr>\n",
        };
        let transition = if stream == Some(record.stream) {
            String::new()
        } else {
            format!("{}{opening}", close_block(stream))
        };
        if body.len() + transition.len() + row.len() + 1 + close_block(Some(record.stream)).len()
            > budget
        {
            body.push_str(close_block(stream));
            items.push(body);
            number = number.saturating_add(1);
            header = format!("monitor {id} {description} delivery {number}\n");
            body = header.clone();
            body.push_str(opening);
        } else {
            body.push_str(&transition);
        }
        body.push_str(row);
        body.push('\n');
        stream = Some(record.stream);
    }
    body.push_str(close_block(stream));
    for notice in notices {
        assert!(notice.text.len() <= MAX_NOTICE_BYTES);
        if body.len() + notice.text.len() + 1 > budget {
            items.push(body);
            number = number.saturating_add(1);
            header = format!("monitor {id} {description} delivery {number}\n");
            body = header.clone();
        }
        body.push_str(&notice.text);
        body.push('\n');
    }
    if body != header {
        items.push(body);
    }
    items
}

#[cfg(test)]
#[path = "monitor_frame_tests.rs"]
mod tests;
