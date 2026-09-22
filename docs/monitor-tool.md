# Monitor tool

The `monitor` tool runs a shell command as a long-lived background watcher and delivers its stdout and stderr records to the model as notifications.
The frozen surface has the required `action` parameter with the values `start`, `stop`, and `list`; `command` and `description` are required by `start`, and `id` is required by `stop`.
Descriptions are limited to 256 UTF-8 bytes.
`list` reports each active monitor's id, description, command, and stored start timestamp.

The configuration key is `features.monitor`, a boolean.
This fork carries `monitor` at `Stage::Stable` and enables it by default.
Stable/on is an intentional deviation from the reference implementation's stage and default.

## Arming and readiness

There is no fork probe and no tool-error discriminator.
On a decided Codex host, the consumer emits its arming context unless the operator sets the explicit stock override `ADT_CODEX_FORK=0`.
The boot turn consults the complete capability surface advertised to the model, including nested code-mode definitions, and uses the advertised invocation form.

The arming context instructs the boot turn, in this order: if the `monitor` tool is present in the session's advertised tool surface (the complete capability surface exposed to the model, including nested code-mode definitions), call it with action `start`, the exact follow command, and the fixed description, then action `list` to confirm the watch id, then run `relay-monitor.py status --session <id>` through the shell and quote its line; if `monitor` is absent from the advertised surface, make no call against it, report `unavailable: monitor-tool` as the delivery state and `fallback: pointer` in the acknowledgment, and use the pointer block.
The resume variant is unchanged: on `source: resume` the context asks for `list` first and `start` only when no live monitor's command equals the expected `relay-monitor.py follow ... --session <this session id>` invocation.
Absence of the tool is never claimed as stock identity; a successful `monitor start` is the only capability evidence; install-byte identity (`codex features list` on the installed binary, the sha256 against the release notes) is install evidence in the recipe and never gates arming.

`monitor list` proves only that the process is registered in the monitor pool.
Readiness is the consumer's judgment from `relay-monitor.py status --session <id>` after the start or matched resume watch.
The acknowledgment quotes the status state verbatim: `armed`, `waiting-for-binding`, `unavailable: <reason>`, or `fallback: pointer`.
A live but unbound monitor and a standby are not armed.
After the first outgoing relay binds the watcher, readiness requires a new `status` observation; filing alone does not establish it.

## Delivery limits and recovery

All budgets are measured in UTF-8 bytes of model-visible text rather than wire JSON bytes.
A stdout or stderr record is at most 4 KiB and is framed on LF before entering the bounded record queue.
An accepted notification is at most 8 KiB including its header, stream markers, records, and notices.
A notice is at most 1 KiB, and a description is at most 256 bytes.
The per-monitor rate bucket has capacity 200 records and refills at 20 records per second.
Three consecutive lossy 10-second windows stop the monitor with `MONITOR-NOTICE: flood-stop`.

The notification envelope starts with `monitor <id> <description> delivery <n>` and may contain separately tagged stdout and stderr blocks.
Loss notices use the exact grammar `MONITOR-NOTICE: loss <site> records=<n>` and carry no replay locator.
An exit or flood-stop condition is also reported on a `MONITOR-NOTICE:` line, never as a consumer row.
Only accepted stdout records are consumer rows; tagged stderr blocks and `MONITOR-NOTICE:` lines are never parsed as rows.
Rows over 4 KiB are unsupported for delivery, are dropped whole and counted, and are recovered through replay.

After a loss notice, run the consumer's `replay` for every retained `(root, seat)` binding in the session, not only the current binding.
For every replayed row, check whether its addressed work has already been handled before acting.
Replay fixes an upper bound without advancing progress.


## P0 token measurement

Task 9 measured the maximum-size fixture `max_item_serialized_size_recorded` from `codex-rs/core/src/unified_exec/monitor_frame_tests.rs` at source commit `b451f852c7f998e5b1a2243b72b1849e806d9687`.
The model-visible text is exactly 8,192 UTF-8 bytes, including `MonitorNotification` markers, its description prefix, the delivery header, stdout markers, and record separators.
The fixture uses monitor id `m1`, a 256-byte description of repeated `d`, delivery sequence `18446744073709551415`, and NUL records of 4,096 and 3,471 bytes.
With Python 3.14.6 and `tiktoken` 0.14.0, byte-level BPE counts are 3,944 tokens for `o200k_base` and 7,727 tokens for `cl100k_base`, both below 8,192.
These counts describe this fixture under the named encodings, not a universal token bound for every allowed notification or model tokenizer.
The measured text SHA-256 is `0549f2cd5f4b3c4cb592818ef9eca810dfbd6f4d232aa8f99e82a8ffe8178169`.
The measurement counts actual NUL bytes in rendered text, not the 46,203-byte serialized ResponseItem JSON.

The kit sprint retains `results/v295-codex-fork/impl/task9-p0-measure.py` and the source-bound `impl/b451f852c7f998e5b1a2243b72b1849e806d9687/task9/p0/` evidence directory beneath `results/v295-codex-fork/`.
That directory contains the exact text, source snapshots and hashes, tokenizer metadata, token-id arrays, and an independent Rust rendering witness that matches the reconstructed text byte for byte.
Both token decodings also round-trip to the exact measured bytes.

## Sandbox

Monitor commands use the same shared preparation and selected sandbox policy as shell-tool children, including the policy environment and `CODEX_THREAD_ID`.
The default `workspace-write` policy permits the relay watcher to read `<root>/INDEX.md` and write its note under `${TMPDIR:-/tmp}/adt-relay-monitor/`.
That default is a scoped assumption rather than a promise for every policy.
If the active policy denies either access, report the watcher's own `unavailable` state, use the pointer fallback, and do not broaden the sandbox policy for the monitor.

## Installation

The npm slot is the only supported layout.
Build the fork from the same upstream version as the installed npm package, then replace `node_modules/@openai/codex-darwin-arm64/vendor/aarch64-apple-darwin/bin/codex`, or the corresponding platform binary in that package's vendor slot.
This preserves the upstream `codex-code-mode-host`, `rg`, and zsh companion resources beside the binary and lets them resolve through the upstream layout.
A lone fork binary placed earlier on `PATH` is unsupported because companion-resource resolution from an unrelated directory is unverified.

Verify that `codex features list` reports `monitor stable true` and that the installed binary's SHA-256 matches the release notes.
Then let a boot turn start a monitor and make one code-mode tool call while observing `codex-code-mode-host` running from the npm vendor path with a host process listing.
A plain shell call does not prove companion-resource resolution.
