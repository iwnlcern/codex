use super::*;
use crate::context::ContextualUserFragment;
use crate::context::MonitorNotification;
use pretty_assertions::assert_eq;
use std::sync::atomic::Ordering;
use std::time::Duration;

fn frame(bytes: &[u8]) -> (Vec<Record>, Arc<LossCounters>) {
    let ledger = LossLedger::default();
    let counters = ledger.for_attempt(AttemptNonce::new(1));
    let mut framer = RecordFramer::new(AttemptNonce::new(1));
    let mut input = bytes.to_vec();
    input.push(b'\n');
    (framer.push(Stream::Stdout, &input, &counters), counters)
}

fn text(body: &str, description: &str) -> String {
    MonitorNotification::new(description, body).render()
}

#[test]
fn record_of_4096_bytes_accepted_alone_fits_budget() {
    let bytes = vec![0; 4096];
    let (records, _) = frame(&bytes);
    assert_eq!(
        records,
        vec![Record {
            attempt: AttemptNonce::new(1),
            stream: Stream::Stdout,
            bytes
        }]
    );
    let items = render_notifications("m1", "watch", 1, &records, &[]);
    assert_eq!(items.len(), 1);
    assert!(text(&items[0], "watch").len() <= 8192);
}

#[test]
fn record_of_4097_bytes_dropped_whole_count_1() {
    let (records, counters) = frame(&vec![b'x'; 4097]);
    assert!(records.is_empty());
    assert_eq!(counters.take(), vec![(DropReason::Oversize, 1)]);
    assert!(counters.take().is_empty());
}

#[test]
fn json_record_9011_bytes_dropped_never_split_or_quoted() {
    let row = format!("{{\"cell\":\"{}\"}}", "x".repeat(9000));
    assert_eq!(row.len(), 9011);
    let (records, counters) = frame(row.as_bytes());
    assert!(records.is_empty());
    let items = render_notifications(
        "m1",
        "watch",
        1,
        &records,
        &[Notice::loss(DropReason::Oversize, 1)],
    );
    assert!(!items.concat().contains(&row));
    assert_eq!(counters.take(), vec![(DropReason::Oversize, 1)]);
}

#[test]
fn long_cell_row_under_4k_intact() {
    let row = format!("{{\"cell\":\"{}\"}}", "x".repeat(3800));
    let (records, _) = frame(row.as_bytes());
    assert_eq!(records[0].bytes, row.as_bytes());
    assert!(
        render_notifications("m1", "watch", 1, &records, &[])[0]
            .lines()
            .any(|line| line == row)
    );
}

#[test]
fn code_point_split_across_reads_reassembled() {
    let mut framer = RecordFramer::new(AttemptNonce::new(1));
    let counters = LossCounters::default();
    assert!(framer.push(Stream::Stdout, &[0xe2], &counters).is_empty());
    assert_eq!(
        framer.push(Stream::Stdout, &[0x82, 0xac, b'\n'], &counters)[0].bytes,
        "€".as_bytes()
    );
    assert!(counters.take().is_empty());
    framer.push(Stream::Stdout, b"failed prefix", &counters);
    framer.push(Stream::Stderr, b"failed error", &counters);
    framer.reset();
    assert!(framer.finish().is_none());
    let mut framer = RecordFramer::new(AttemptNonce::new(2));
    assert_eq!(
        framer.push(Stream::Stdout, b"fresh\n", &counters)[0].bytes,
        b"fresh"
    );
    let tail = framer.push(Stream::Stdout, b"partial", &counters);
    assert!(tail.is_empty());
    assert_eq!(framer.attempt, AttemptNonce::new(2));
    let tail = framer.finish().unwrap();
    assert_eq!(tail.stdout, b"partial");
    assert!(tail.stderr.is_empty());
    assert!(framer.finish().is_none());
}

#[test]
fn invalid_utf8_stdout_dropped_stderr_lossy_marked() {
    let mut framer = RecordFramer::new(AttemptNonce::new(1));
    let counters = LossCounters::default();
    assert!(
        framer
            .push(Stream::Stdout, b"bad\xff\n", &counters)
            .is_empty()
    );
    let records = framer.push(Stream::Stderr, b"bad\xff\n", &counters);
    assert!(
        String::from_utf8(records[0].bytes.clone())
            .unwrap()
            .contains("invalid-utf8")
    );
    assert!(render_notifications("m1", "watch", 1, &records, &[])[0].contains("bad�"));
    assert_eq!(counters.take(), vec![(DropReason::InvalidUtf8, 1)]);
    let expanded = framer.push(
        Stream::Stderr,
        &[vec![0xff; 4096], vec![b'\n']].concat(),
        &counters,
    );
    assert!(expanded.is_empty());
    assert_eq!(counters.take(), vec![(DropReason::Oversize, 1)]);
}

#[test]
fn newline_free_1mib_keeps_buffer_under_8k_one_drop() {
    let mut framer = RecordFramer::new(AttemptNonce::new(1));
    let counters = LossCounters::default();
    for _ in 0..256 {
        assert!(
            framer
                .push(Stream::Stdout, &vec![b'x'; 4096], &counters)
                .is_empty()
        );
        assert!(framer.stdout.bytes.capacity() + framer.stderr.bytes.capacity() < 8192);
    }
    assert_eq!(counters.take(), vec![(DropReason::Oversize, 1)]);
    assert_eq!(
        framer.push(Stream::Stdout, b"\nnext\n", &counters)[0].bytes,
        b"next"
    );
    assert!(framer.finish().is_none());
}

#[test]
fn batch_of_200_rows_splits_in_order_each_under_8k() {
    let records: Vec<_> = (0..200)
        .map(|n| Record {
            attempt: AttemptNonce::new(1),
            stream: if n % 2 == 0 {
                Stream::Stdout
            } else {
                Stream::Stderr
            },
            bytes: format!("{n:03}{}", "x".repeat(476)).into_bytes(),
        })
        .collect();
    let items = render_notifications("m1", "watch", 9, &records, &[]);
    assert!(items.len() > 1);
    let actual: Vec<_> = items
        .iter()
        .flat_map(|item| item.lines())
        .filter(|line| line.len() == 479)
        .map(str::as_bytes)
        .collect();
    assert_eq!(
        actual,
        records
            .iter()
            .map(|r| r.bytes.as_slice())
            .collect::<Vec<_>>()
    );
    for (index, item) in items.iter().enumerate() {
        assert!(text(item, "watch").len() <= 8192);
        assert!(item.starts_with(&format!("monitor m1 watch delivery {}\n", 9 + index as u64)));
    }
}

#[test]
fn notice_line_never_parses_as_json() {
    let ledger = LossLedger::default();
    let failed = ledger.for_attempt(AttemptNonce::new(1));
    failed.record(DropReason::ChannelFull);
    let committed = ledger.for_attempt(AttemptNonce::new(2));
    let producer_activity = *committed.last_activity.lock().unwrap();
    committed.record(DropReason::Rate);
    assert_eq!(*committed.last_activity.lock().unwrap(), producer_activity);
    assert!(Arc::ptr_eq(
        &committed,
        &ledger.commit(AttemptNonce::new(2))
    ));
    assert_eq!(ledger.attempts.lock().unwrap().len(), 1);
    assert!(committed.gap_open.load(Ordering::Acquire));
    let notices: Vec<_> = committed
        .take()
        .into_iter()
        .map(|(site, n)| Notice::loss(site, n))
        .collect();
    let items = render_notifications("m1", "watch", 1, &[], &notices);
    assert!(items[0].contains("MONITOR-NOTICE: loss rate records=1"));
    assert!(items[0].contains(REPLAY_INSTRUCTION));
    assert!(!items[0].contains("channel-full"));
    for line in items[0]
        .lines()
        .filter(|line| line.starts_with("MONITOR-NOTICE:"))
    {
        assert!(serde_json::from_str::<serde_json::Value>(line).is_err());
        assert!(line.len() <= 1024);
    }
    let partial = PartialTail {
        stdout: b"row\n\"quoted\"".to_vec(),
        stderr: vec![0xff],
    };
    let notice = Notice::exit(Some(partial));
    let rendered = render_notifications("m1", "watch", 1, &[], &[notice]);
    assert!(rendered[0].contains("MONITOR-NOTICE: exit"));
    assert!(rendered[0].contains("\\n"));
    assert!(!committed.gap_open.load(Ordering::Acquire));
    assert!(
        render_notifications("m1", "watch", 1, &[], &[Notice::flood_stop()])[0]
            .contains("MONITOR-NOTICE: flood-stop")
    );
}

fn exhaust(bucket: &mut RateBucket, now: Instant) {
    for _ in 0..200 {
        assert!(bucket.admit(now));
    }
    assert!(!bucket.admit(now));
}

#[test]
fn refill_pattern_1_3_lossy_2_clean_no_stop_and_1_2_3_stop() {
    let start = Instant::now();
    let mut separated = RateBucket::new(start);
    exhaust(&mut separated, start);
    for _ in 0..200 {
        assert!(separated.admit(start + Duration::from_secs(10)));
    }
    exhaust(&mut separated, start + Duration::from_secs(20));
    assert_eq!(separated.lossy_windows(), 1);
    let mut consecutive = RateBucket::new(start);
    for window in 0..3 {
        exhaust(&mut consecutive, start + Duration::from_secs(window * 10));
    }
    assert_eq!(consecutive.lossy_windows(), 3);
    let mut fractional = RateBucket::new(start);
    for _ in 0..200 {
        assert!(fractional.admit(start));
    }
    assert!(!fractional.admit(start + Duration::from_millis(49)));
    assert!(fractional.admit(start + Duration::from_millis(50)));
    assert!(!fractional.admit(start + Duration::from_millis(50)));
}

#[test]
fn catch_up_burst_of_200_lossless() {
    let start = Instant::now();
    let mut bucket = RateBucket::new(start);
    for _ in 0..200 {
        assert!(bucket.admit(start));
    }
    assert_eq!(bucket.lossy_windows(), 0);
    assert!(bucket.admit(start + Duration::from_secs(10)));
}

#[test]
#[expect(
    clippy::print_stdout,
    reason = "Task4a requires raw measured text and wire sizes in the receipt"
)]
fn max_item_serialized_size_recorded() {
    let description = "d".repeat(256);
    let first = Record {
        attempt: AttemptNonce::new(1),
        stream: Stream::Stdout,
        bytes: vec![0; 4096],
    };
    let probe = render_notifications(
        "m1",
        &description,
        u64::MAX - 200,
        std::slice::from_ref(&first),
        &[],
    );
    let spare = 8192 - text(&probe[0], &description).len() - 1;
    assert!(spare <= 4096);
    let records = [
        first,
        Record {
            attempt: AttemptNonce::new(1),
            stream: Stream::Stdout,
            bytes: vec![0; spare],
        },
    ];
    let items = render_notifications("m1", &description, u64::MAX - 200, &records, &[]);
    let max_text = items
        .iter()
        .map(|item| text(item, &description).len())
        .max()
        .unwrap();
    let max_wire = items
        .iter()
        .map(|item| {
            serde_json::to_vec(&ContextualUserFragment::into(MonitorNotification::new(
                &description,
                item,
            )))
            .unwrap()
            .len()
        })
        .max()
        .unwrap();
    println!(
        "maximum measured fragment text bytes={max_text}; serialized ResponseItem bytes={max_wire}; hard text cap=8192; P0 tokenizer count deferred to Task9"
    );
    assert_eq!(max_text, 8192);
    assert!(max_wire > max_text);
}
