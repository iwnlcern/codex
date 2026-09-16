# Monitor rebase runbook

The carried fork is an upstream stable `rust-vX.Y.Z` tag plus exactly one monitor commit.
`main` has a matching `vX.Y.Z-monitor.N` tag and its parent must equal that upstream tag's peeled commit.
This automation prepares candidates and records receipts; landing and installation require their separate authorization.
It never pushes `main`, merges a PR, moves the fork tag, or builds from the workflow.

## Prerequisites and commands

Use a clean checkout with full history, an authenticated `origin` pointing to the fork, and `upstream` pointing to `https://github.com/openai/codex.git`.
Configure a Git commit identity and authenticated `gh` access to the fork.
Install the pinned Rust toolchain from `rust-toolchain.toml`, `just`, `cargo-nextest`, Python 3, Git, and Bash.
Set `RUSTY_V8_ARCHIVE` and `RUSTY_V8_SRC_BINDING_PATH` to the platform's verified upstream prebuilt V8 artifacts.
The workflow uses the standard hosted `macos-15` ARM64 runner, the repository's setup-ci and checksum-verifying setup-rusty-v8 actions, and the upstream-pinned Rust 1.95.0 and nextest 0.9.103 setup actions.
It verifies the hosted Python, gh, Git, just, and nextest tools before invoking a transition and never invokes the release build recipe.
Set an absolute `CARGO_TARGET_DIR` to reuse compilation artifacts across sequential worktrees.
Concurrent receipt runs sharing a target directory are unsupported because nextest writes one JUnit file per profile.

```bash
bash scripts/rebase-monitor.sh prepare rust-v0.154.0
bash scripts/rebase-monitor.sh prepare rust-v0.154.0 --monitor-commit <sha>
bash scripts/rebase-monitor.sh test <head>
bash scripts/rebase-monitor.sh pr-state rust-v0.154.0
bash scripts/rebase-monitor.sh carried rust-v0.154.0
bash scripts/rebase-monitor.sh landing rust-v0.154.0
bash scripts/rebase-monitor.sh transition discover
bash scripts/rebase-monitor.sh transition resume --tag rust-v0.154.0
bash scripts/rebase-monitor.sh transition regenerate --tag rust-v0.154.0 --expected-head <sha>
bash scripts/rebase-monitor.sh build macos aarch64-apple-darwin
```

`prepare` fetches upstream tags, refuses prereleases and dirty checkouts, creates an owned detached worktree ending in `rebase/<tag>`, and cherry-picks the monitor commit.
An explicit `--monitor-commit` selects that commit without requiring the current `main` fork-tag assertion.
The returned path and monitor SHA are also retained under the Git common directory's `monitor-rebase/prepared` and `monitor-rebase/monitor-commit` files.
Exit status is 0 for a clean pick, 2 for conflicts with a written report, and 1 for errors.
Run `report` from the conflicted worktree to rewrite its `REBASE-REPORT.md` from the unresolved paths.
Worktrees and receipts are retained for inspection; remove only the owned worktrees you have reviewed using `git worktree remove` when their evidence is no longer needed.
`build` delegates its remaining arguments to `scripts/build-release.sh`; that separately authored recipe owns macOS and Linux builds.

## Eligibility and receipts

STRUCTURAL eligibility requires exactly one single-parent commit above a stable upstream tag, `docs/monitor-tool.md`, `Feature::Monitor` in `codex-rs/features/src/lib.rs`, and no `REBASE-REPORT.md`.
`test <head>` resolves the exact commit and tests it in a fresh detached worktree, exiting 3 for structural ineligibility, 1 for test or infrastructure failures, and 0 only after both suites pass.
The independent 69-name inventory is embedded in the script.
The real `cargo nextest list -p codex-core -E 'test(monitor)' --message-format json` selection must match that inventory exactly, with binary identities checked and no ignored or duplicated identities.
The command is `CODEX_MONITOR_TESTS_REQUIRE=1 just test -p codex-core -E 'test(monitor)' --retries 0`, with `STABLE_GIT_COMMIT` equal to the tested head and the `local` nextest profile.
JUnit is copied before `just test -p codex-features --retries 0` overwrites it, and must contain exactly those 69 identities with no failures, errors, skips, or duplicates.
Raw logs retain LEAK observations without retries or a claim about their cause.
No Cargo invocation uses `--locked`.
The retained pre-command lockfile and each post-command lockfile are checked with `scripts/lockfile-delta.py`; only the allowed workspace version deltas permit restoration.
A disallowed delta stops and remains available in its worktree for inspection.

Receipts default to the Git common directory's `monitor-rebase/results/v295-codex-fork/impl/<head>/`.
Set `MONITOR_REBASE_RESULTS` to an absolute external directory to choose a separate evidence root.
Each receipt includes its tested SHA, structured listing, raw command logs, copied JUnit, and lock guards.
Receipt directories are reserved atomically before any evidence write, including attempts that fail during listing; a deliberate later attempt needs a fresh evidence root and its own failure disposition.
Workflow receipt roots and artifact names are distinct per run, attempt, and mode; PR bodies link the workflow run's artifacts.
A listing failure can have no JUnit file and is still reported as `tests-failed`, with the listing stderr retained.

LANDING eligibility adds `state: clean` and exact equality between `tested-head` and the current PR head.
`landing <tag>` is a read-only predicate and never performs landing.
A moved head immediately fails that predicate even when the PR body still says clean.
`resume` can test a structurally repaired head and record the new receipt without changing its commit.

## Transitions and publication

Candidate identity is the single `monitor-rebase/<tag>` label across open and closed PRs.
Ambiguous candidates fail closed.
PR bodies begin with `state`, the peeled `upstream` SHA, `monitor-commit`, and `tested-head`, followed by an artifact link and `git diff <tag>..<candidate>`.
A closed candidate is a no-op in every transition and cannot be regenerated.
`discover` chooses the newest stable fetched tag when no tag is supplied, skips carried tags or any existing candidate, then prepares, tests, and creates one PR.
Its initial push uses a lease requiring an absent destination, preventing replacement of an existing branch.
`resume` requires an open candidate, fetches and tests its current head only, rechecks the PR head after testing, and updates only the body while preserving its validated originating monitor-commit header.
With no tag, it enumerates open candidate labels inside the production script and invokes the same per-tag resume transition for each.
It never prepares or pushes.
`regenerate` is an explicit dispatch operation with a required exact 40-character `expected_head`.
It prepares again and uses `git push --force-with-lease=refs/heads/rebase/<tag>:<expected_head>`; Git enforces the comparison at push time.
A stale lease aborts before any PR-body update.
Every four hours the workflow schedules separate discovery and resume invocations; dispatch selects discovery, resume, or regeneration through the same production `transition` entry point.
There is no workflow-side candidate state machine.

A conflict produces a separate tag-plus-one report-only commit containing only `REBASE-REPORT.md`, while the unresolved cherry-pick worktree remains evidence.
Replace that report-only commit with one resolved commit above the same tag before resuming.
A test failure retains the one patch commit and links its evidence, with an empty `tested-head`.
Nothing becomes landing-eligible until the current exact head has a passing receipt.

`--candidate-ref refs/heads/rebase/scratch-lease` exists only for isolated lease proofs against a scratch remote.
It redirects the regeneration push and suppresses all live-candidate PR writes, while preparation still consumes the real stable tag.
Any override naming `refs/heads/rebase/rust-vX.Y.Z` is rejected before preparation; omit the option for ordinary regeneration.
Never point a lease proof at the live fork remote.

## Known reference conflicts

Cherry-picking `ae7dbe6` from `rust-v0.142.0` onto `rust-v0.154.0` reproduces exactly eight conflicts.
All paths below are relative to `codex-rs/`.

| Path | Resolution carried by the monitor patch |
| --- | --- |
| `core/src/context/contextual_user_message.rs` | Register the monitor contextual fragment using the current matcher API. |
| `core/src/session/handlers.rs` | Attach monitor shutdown to the current session shutdown path. |
| `core/src/session/session.rs` | Adapt delivery to current session APIs; the final implementation uses the monitor delivery gate. |
| `core/src/session/tests.rs` | Populate monitor services in every current `SessionServices` test literal. |
| `core/src/state/service.rs` | Add the monitor manager to the current service structure. |
| `core/src/tools/spec_plan.rs` | Register the monitor tool behind the feature using the current spec builder. |
| `core/src/unified_exec/mod.rs` | Register monitor modules beside the current unified-exec implementation. |
| `features/src/lib.rs` | Keep one stable, default-enabled `Feature::Monitor` entry. |

Additional compile seams included `MonitorNotification.content_kind`, the current `UnifiedExecContext::new` arguments and `ExecCommandArgs.timeout_ms`, and registration of `core/tests/suite/monitor.rs`.
These resolutions describe the carried patch, not a guarantee that a future upstream tag has the same conflicts.

## Acceptance and stop rule

Author the five Task 8 files, check syntax, obtain source review and commit authorization, commit, and only then construct the ephemeral squash from that committed tree.
Publish it locally as `refs/scratch/rebase-candidate`; both scratch-clone suites fetch that ref and prove its tag parent and exact development-tree equality.
Run `scripts/tests/test_rebase_monitor.sh <source-repo> <SQ> <development-head> <evidence-dir>` and the analogous `test_monitor_rebase_workflow.sh` invocation.
The transition suite uses real Git pushes and Rust tests; only `gh` is stubbed.
Its tests-failed control deliberately supplies a nonexistent Rust compiler and does not fabricate JUnit.
The stale-lease control captures Git's `stale info` rejection and unchanged remote SHA; the matching-head control uses the same preparation and push path.
Delete the local scratch publication ref after the tests, preserving the evidence and any failing worktrees.
Stop on any unexpected required-gate failure, preserve its output, and report it before changing the implementation or rerunning.
Do not retry unchanged, weaken an oracle, push the live fork, run GitHub CI, or claim landing, build, or installation from these scratch proofs.
