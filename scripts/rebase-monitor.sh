#!/usr/bin/env bash
# Production rebase state machine. All tests run in detached, owned worktrees.
set -euo pipefail
ROOT=$(git rev-parse --show-toplevel)
cd "$ROOT"
SELF="$ROOT/scripts/rebase-monitor.sh"
COMMON=$(cd "$(git rev-parse --git-common-dir)" && pwd)
STORE="$COMMON/monitor-rebase"
mkdir -p "$STORE"
fail() { echo "$*" >&2; exit 1; }
stable() { [[ $1 =~ ^rust-v[0-9]+\.[0-9]+\.[0-9]+$ ]]; }
sha() { git rev-parse --verify "$1^{commit}"; }
clean() { [[ -z $(git status --porcelain) ]] || fail 'worktree must be clean'; }
carried() {
    stable "$1" || fail 'not a stable tag'
    if [[ $(sha 'main^') == "$(sha "refs/tags/$1")" ]]; then echo yes; else echo no; fi
}
monitor_commit() {
    local fork_tag upstream
    fork_tag=$(git tag --points-at main --list 'v*-monitor.*' --sort=-version:refname | sed -n '1p')
    [[ $fork_tag =~ ^v([0-9]+\.[0-9]+\.[0-9]+)-monitor\.[0-9]+$ ]] || fail 'main needs a current fork tag'
    upstream="rust-v${BASH_REMATCH[1]}"
    [[ $(sha 'main^') == "$(sha "refs/tags/$upstream")" ]] || fail 'main is not tag plus one commit'
    sha main
}
report() {
    { printf '# Rebase conflict report\n\n'; git diff --name-only --diff-filter=U; } > REBASE-REPORT.md
}
prepare() {
    local tag=${1:?tag required} monitor dir rc
    shift
    stable "$tag" || fail 'prerelease or invalid tag refused'
    clean
    git fetch upstream --tags >&2 || return 1
    if [[ $# != 0 ]]; then
        [[ $# == 2 && $1 == --monitor-commit ]] || fail 'invalid prepare arguments'
        monitor=$(sha "$2") || return 1
    else
        monitor=$(monitor_commit) || return 1
    fi
    sha "refs/tags/$tag" >/dev/null || return 1
    dir=$(mktemp -d "$STORE/prepare.XXXXXX") || return 1
    mkdir "$dir/rebase" || return 1
    dir="$dir/rebase/$tag"
    git worktree add --detach "$dir" "refs/tags/$tag" >&2 || return 1
    printf '%s\n' "$dir" > "$STORE/prepared" || return 1
    printf '%s\n' "$monitor" > "$STORE/monitor-commit" || return 1
    rc=0
    git -C "$dir" cherry-pick "$monitor" >&2 || rc=$?
    if [[ $rc != 0 ]]; then
        [[ -n $(git -C "$dir" diff --name-only --diff-filter=U) ]] || return 1
        (cd "$dir" && report) || return 1
        printf '%s\n' "$dir"
        return 2
    fi
    printf '%s\n' "$dir"
}
structural() {
    local head=$1 tag=${2:-} parent
    head=$(sha "$head") || return 1
    parent=$(git rev-list --parents -n 1 "$head")
    [[ $(wc -w <<< "$parent" | tr -d ' ') == 2 ]] || return 1
    if [[ -n $tag ]]; then
        stable "$tag" && [[ $(sha "$head^") == "$(sha "refs/tags/$tag")" ]] || return 1
    else
        tag=$(git tag --points-at "$head^" --list 'rust-v*' | python3 -c 'import re,sys; print(next((s.strip() for s in sys.stdin if re.fullmatch(r"rust-v[0-9]+\.[0-9]+\.[0-9]+\n?",s)),""))')
        [[ -n $tag ]] || return 1
    fi
    git cat-file -e "$head:docs/monitor-tool.md" 2>/dev/null || return 1
    git show "$head:codex-rs/features/src/lib.rs" | grep 'Feature::Monitor' >/dev/null || return 1
    ! git cat-file -e "$head:REBASE-REPORT.md" 2>/dev/null
}
pr_state() {
    local tag=$1 json
    stable "$tag" || fail 'not a stable tag'
    json=$(gh pr list --state all --label "monitor-rebase/$tag" --limit 1000 --json state,body,headRefOid,headRefName,isCrossRepository)
    python3 - "$(sha "refs/tags/$tag")" "$json" "$tag" "${2:-state}" <<'PY'
import json,re,sys
ps=json.loads(sys.argv[2])
if not ps: print('none'); sys.exit()
if len(ps)!=1: raise SystemExit('ambiguous candidates')
p=ps[0]
if p['state']!='OPEN': print('closed'); sys.exit()
if p['headRefName']!='rebase/'+sys.argv[3] or p['isCrossRepository']: raise SystemExit('candidate branch mismatch')
d={}
for line in p['body'].splitlines()[:4]:
    k,sep,v=line.partition(': ')
    if k in ('state','upstream','monitor-commit','tested-head') and sep:
        if k in d: raise SystemExit('duplicate PR header')
        d[k]=v
if set(d)!=set(('state','upstream','monitor-commit','tested-head')) or d['upstream']!=sys.argv[1]: raise SystemExit('invalid PR headers')
if d['state'] not in ('clean','conflict','tests-failed'): raise SystemExit('invalid state')
if not re.fullmatch('[0-9a-f]{40}',d['monitor-commit']) or not re.fullmatch('[0-9a-f]{40}',p['headRefOid']): raise SystemExit('invalid commit header')
if d['tested-head'] and not re.fullmatch('[0-9a-f]{40}',d['tested-head']): raise SystemExit('invalid tested head')
print('open:'+d['state']+':'+d['tested-head']+':'+p['headRefOid'])
if sys.argv[4]=='with-monitor': print(d['monitor-commit'])
PY
}
landing() {
    local tag=$1 state head tested
    state=$(pr_state "$tag")
    [[ $state == open:clean:* ]] || return 1
    IFS=: read -r _ _ tested head <<< "$state"
    [[ $tested == "$head" ]] && structural "$head" "$tag"
}
receipt_check() {
    python3 - "$1" "$2" <<'PY'
import json,sys,xml.etree.ElementTree as ET
expected=set('''
session::monitor_delivery_tests::budget_limited_abort_is_held_like_interrupt
session::monitor_delivery_tests::completion_between_not_idle_and_next_poll_wakes
session::monitor_delivery_tests::concurrent_start_stop_shutdown_completes_under_5s
session::monitor_delivery_tests::delivery_accepted_into_wake_turn_leaves_flag_clear
session::monitor_delivery_tests::delivery_during_in_flight_wake_resets_flag_afterwards
session::monitor_delivery_tests::exit_while_refused_then_mode_change_still_wakes
session::monitor_delivery_tests::fresh_arrival_while_interrupted_starts_no_turn
session::monitor_delivery_tests::inject_if_running_consumption_sets_no_flag
session::monitor_delivery_tests::interrupt_racing_start_boundary_at_most_one_turn
session::monitor_delivery_tests::mailbox_competition_defers_then_wakes
session::monitor_delivery_tests::monitor_stop_leaves_flag
session::monitor_delivery_tests::pending_retry_holds_while_interrupted
session::monitor_delivery_tests::plan_mode_arrival_records_then_wakes_after_settings_only_default
session::monitor_delivery_tests::queued_turn_clears_hold_then_retry_fires
session::monitor_delivery_tests::record_never_resubmitted
session::monitor_delivery_tests::shutdown_aborts_retry_task
session::monitor_delivery_tests::wakes_idle_default_mode_without_user_input
suite::monitor::monitor_delivers_exit_notice_when_command_ends
suite::monitor::monitor_delivers_unterminated_final_line
suite::monitor::monitor_self_prunes_from_registry_when_command_exits
suite::monitor::monitor_stderr_output_wakes_agent
suite::monitor::monitor_stdout_output_wakes_agent
suite::monitor::monitor_truncates_a_newline_free_flood
suite::monitor_pool::combined_mode_unchanged_for_ordinary_exec
suite::monitor_pool::explicit_stop_cleans_record
suite::monitor_pool::immediate_output_and_exit_delivers_and_releases
suite::monitor_pool::monitor_child_env_has_codex_thread_id_and_policy
suite::monitor_pool::network_denial_terminates_and_unregisters
suite::monitor_pool::sandbox_denial_retry_records_only_final_attempt
suite::monitor_pool::shutdown_with_three_monitors_leaves_nothing
suite::monitor_pool::sixty_four_ordinary_processes_do_not_evict_or_count_a_monitor
suite::monitor_pool::tagged_mode_keeps_streams_separate
suite::monitor_pool::tagged_output_task_fills_diagnostic_buffer_and_closes
suite::monitor_surface::description_over_256_bytes_rejected
suite::monitor_surface::list_shows_started_watch_with_command
suite::monitor_surface::stop_unknown_id_reports_not_found
suite::monitor_surface::tool_absent_when_feature_disabled
suite::monitor_surface::tool_spec_matches_frozen_surface
unified_exec::monitor::monitor_pool_tests::approval_cancelled_during_preparation_leaves_no_process_and_free_slot
unified_exec::monitor::monitor_pool_tests::failure_before_registry_insert_kills_process_releases_slot
unified_exec::monitor::monitor_pool_tests::late_denial_after_saturated_tagged_channel_is_classified
unified_exec::monitor::monitor_pool_tests::ninth_start_refused_before_spawn
unified_exec::monitor::monitor_pool_tests::no_denial_saturation_control_drains_and_closes
unified_exec::monitor::monitor_pool_tests::pipeline_reader_runs_before_any_attempt_is_spawned
unified_exec::monitor::monitor_pool_tests::reader_resets_framer_and_ledger_on_new_attempt_nonce
unified_exec::monitor::monitor_pool_tests::remote_attempt_reports_unsupported
unified_exec::monitor::monitor_pool_tests::spawn_failure_releases_slot_no_registry_entry
unified_exec::monitor::monitor_pool_tests::two_concurrent_starts_race_last_slot_exactly_one_spawns
unified_exec::monitor::monitor_pool_tests::unattached_reader_control_stalls_ingestion_before_denial_line
unified_exec::monitor::tests::deregister_self_removes_entry_without_aborting_its_task
unified_exec::monitor::tests::registry_tracks_insert_list_and_remove
unified_exec::monitor_frame::path_tests::drop_then_2s_silence_emits_notice
unified_exec::monitor_frame::path_tests::final_record_drop_then_exit_emits_notice_in_drain
unified_exec::monitor_frame::path_tests::partial_final_record_rides_exit_notice_not_record
unified_exec::monitor_frame::path_tests::rate_drop_then_channel_drop_two_notices_correct_sites
unified_exec::monitor_frame::path_tests::stalled_reader_backpressures_producer_no_chunk_loss
unified_exec::monitor_frame::path_tests::stderr_forwarded_in_tagged_block_unaltered
unified_exec::monitor_frame::tests::batch_of_200_rows_splits_in_order_each_under_8k
unified_exec::monitor_frame::tests::catch_up_burst_of_200_lossless
unified_exec::monitor_frame::tests::code_point_split_across_reads_reassembled
unified_exec::monitor_frame::tests::invalid_utf8_stdout_dropped_stderr_lossy_marked
unified_exec::monitor_frame::tests::json_record_9011_bytes_dropped_never_split_or_quoted
unified_exec::monitor_frame::tests::long_cell_row_under_4k_intact
unified_exec::monitor_frame::tests::max_item_serialized_size_recorded
unified_exec::monitor_frame::tests::newline_free_1mib_keeps_buffer_under_8k_one_drop
unified_exec::monitor_frame::tests::notice_line_never_parses_as_json
unified_exec::monitor_frame::tests::record_of_4096_bytes_accepted_alone_fits_budget
unified_exec::monitor_frame::tests::record_of_4097_bytes_dropped_whole_count_1
unified_exec::monitor_frame::tests::refill_pattern_1_3_lossy_2_clean_no_stop_and_1_2_3_stop
'''.split())
assert len(expected)==69
if sys.argv[1]=='list':
    data=json.load(open(sys.argv[2])); actual=[]
    for binary,suite in data['rust-suites'].items():
        for name,t in suite['testcases'].items():
            if t['filter-match']['status']=='matches':
                assert not t['ignored']
                assert binary==('codex-core::all' if name.startswith('suite::') else 'codex-core')
                actual.append(name)
else:
    root=ET.parse(sys.argv[2]).getroot(); actual=[]
    for t in root.iter('testcase'):
        assert not any(t.find(x) is not None for x in ('failure','error','skipped'))
        name=t.attrib['name']
        assert t.attrib['classname']==('codex-core::all' if name.startswith('suite::') else 'codex-core')
        actual.append(name)
assert len(actual)==len(set(actual))==69, actual
assert set(actual)==expected, {'missing':sorted(expected-set(actual)),'extra':sorted(set(actual)-expected)}
print('exact 69 identities: '+sys.argv[1])
PY
}
test_head() (
    local head dir out target rc=0 guard=0
    head=$(sha "$1")
    structural "$head" || exit 3
    out="${MONITOR_REBASE_RESULTS:-$STORE/results/v295-codex-fork/impl}/$head"
    mkdir -p "$(dirname "$out")"
    # Reserve every attempt, including failures before the test run starts.
    mkdir "$out" || fail 'receipt already exists; preserve it and select a fresh results root'
    out=$(cd "$out" && pwd)
    dir=$(mktemp -d "$STORE/test.XXXXXX")
    git worktree add --detach "$dir/source" "$head"
    printf '%s\n' "$head" > "$out/tested-head"
    cd "$dir/source"
    git show HEAD:codex-rs/Cargo.lock > "$out/Cargo.lock.before"
    # Validate even when a command fails; never restore an unexplained lock delta.
    finish() {
        python3 scripts/lockfile-delta.py "$out/Cargo.lock.before" codex-rs/Cargo.lock > "$out/lock-final.txt" || guard=$?
        if [[ $guard == 0 ]]; then git restore -- codex-rs/Cargo.lock; fi
        [[ $guard == 0 ]] || exit 1
    }
    trap finish EXIT
    export CODEX_MONITOR_TESTS_REQUIRE=1 STABLE_GIT_COMMIT="$head" NEXTEST_PROFILE=local
    target=${CARGO_TARGET_DIR:-$PWD/codex-rs/target}
    mkdir -p "$target"
    target=$(cd "$target" && pwd)
    export CARGO_TARGET_DIR="$target"
    cd codex-rs
    cargo nextest list -p codex-core -E 'test(monitor)' --message-format json > "$out/list.json" 2> "$out/list.stderr" || rc=$?
    cd ..
    python3 scripts/lockfile-delta.py "$out/Cargo.lock.before" codex-rs/Cargo.lock > "$out/lock-list.txt" || exit 1
    [[ $rc == 0 ]] || exit 1
    receipt_check list "$out/list.json"
    rm -f "$target/nextest/local/junit.xml"
    (cd codex-rs; just test -p codex-core -E 'test(monitor)' --retries 0) > "$out/run.log" 2>&1 || rc=$?
    cat "$out/run.log"
    python3 scripts/lockfile-delta.py "$out/Cargo.lock.before" codex-rs/Cargo.lock > "$out/lock-monitor.txt" || exit 1
    [[ -f $target/nextest/local/junit.xml ]] || exit 1
    cp "$target/nextest/local/junit.xml" "$out/junit.xml"
    [[ $rc == 0 ]] || exit 1
    receipt_check junit "$out/junit.xml"
    (cd codex-rs; just test -p codex-features --retries 0) > "$out/features.log" 2>&1 || rc=$?
    cat "$out/features.log"
    [[ $rc == 0 ]]
)
transition() {
    local mode=${1:?mode required} tag='' expected='' candidate='' state snapshot head tested='' dir rc=0 monitor body artifact
    shift
    while [[ $# -gt 0 ]]; do
        [[ $# -ge 2 ]] || fail 'missing option value'
        case $1 in --tag) tag=$2;; --expected-head) expected=$2;; --candidate-ref) candidate=$2;; *) fail 'unknown option';; esac
        shift 2
    done
    case $mode in discover|resume|regenerate) ;; *) fail 'invalid mode';; esac
    [[ $mode == regenerate || ( -z $expected && -z $candidate ) ]] || fail 'lease options require regenerate'
    if [[ -n $candidate ]]; then
        [[ $candidate == refs/heads/rebase/* ]] && git check-ref-format "$candidate" || fail 'invalid proof ref'
        [[ ! ${candidate#refs/heads/rebase/} =~ ^rust-v[0-9]+\.[0-9]+\.[0-9]+$ ]] || fail 'proof ref cannot name a live candidate'
    fi
    clean
    if [[ $mode == resume && -z $tag ]]; then
        # Scheduled resume enumerates here, then uses the same per-tag transition.
        local prs tags
        prs=$(gh pr list --state open --limit 1000 --json labels)
        tags=$(python3 - "$prs" <<'PY'
import json,re,sys
prs=json.loads(sys.argv[1]); tags=[]
if len(prs)>=1000: raise SystemExit('open PR enumeration limit reached')
for pr in prs:
    for label in pr['labels']:
        name=label['name']
        if name.startswith('monitor-rebase/'):
            tag=name.removeprefix('monitor-rebase/')
            if not re.fullmatch(r'rust-v[0-9]+\.[0-9]+\.[0-9]+',tag): raise SystemExit('invalid candidate label')
            tags.append(tag)
if len(tags)!=len(set(tags)): raise SystemExit('ambiguous candidates')
print('\n'.join(sorted(tags)))
PY
)
        while IFS= read -r tag; do
            [[ -n $tag ]] || continue
            bash "$SELF" transition resume --tag "$tag"
        done <<< "$tags"
        return 0
    fi
    git fetch upstream --tags
    if [[ -z $tag ]]; then
        [[ $mode == discover ]] || fail 'tag required'
        tag=$(git tag --list 'rust-v*' --sort=-version:refname | python3 -c 'import re,sys; print(next((s.strip() for s in sys.stdin if re.fullmatch(r"rust-v[0-9]+\.[0-9]+\.[0-9]+\n?",s)),""))')
    fi
    stable "$tag" || fail 'not a stable tag'
    snapshot=$(pr_state "$tag" with-monitor)
    state=${snapshot%%$'\n'*}
    [[ $state != closed ]] || return 0
    if [[ $mode == discover ]]; then
        [[ $(carried "$tag") != yes && $state == none ]] || return 0
    fi
    if [[ $mode == resume ]]; then
        [[ $state == open:* ]] || fail 'resume requires an open candidate'
        IFS=: read -r _ _ _ head <<< "$state"
        git fetch origin "refs/heads/rebase/$tag"
        [[ $(sha FETCH_HEAD) == "$head" ]] || fail 'candidate moved before test'
        # Retain provenance from the same validated candidate snapshot.
        monitor=${snapshot#*$'\n'}
    else
        if [[ $mode == regenerate ]]; then
            [[ $expected =~ ^[0-9a-f]{40}$ ]] || fail 'exact expected head required'
            [[ $state == open:* || -n $candidate ]] || fail 'regenerate requires an open candidate'
        fi
        bash "$SELF" prepare "$tag" || rc=$?
        [[ $rc == 0 || $rc == 2 ]] || return 1
        dir=$(cat "$STORE/prepared")
        monitor=$(cat "$STORE/monitor-commit")
        if [[ $rc == 2 ]]; then
            # Preserve the unresolved worktree and publish a separate report-only head.
            body=$(mktemp "$STORE/conflict.XXXXXX")
            cp "$dir/REBASE-REPORT.md" "$body"
            dir=$(mktemp -d "$STORE/conflict-tree.XXXXXX")
            git worktree add --detach "$dir/source" "refs/tags/$tag"
            dir="$dir/source"
            cp "$body" "$dir/REBASE-REPORT.md"
            git -C "$dir" add REBASE-REPORT.md
            git -C "$dir" commit -m "Report monitor rebase conflicts onto $tag"
        fi
        head=$(git -C "$dir" rev-parse HEAD)
    fi
    if [[ $rc == 2 ]]; then
        state=conflict
    elif bash "$SELF" test "$head"; then
        state=clean; tested=$head
    else
        state=tests-failed
    fi
    # Recheck GitHub state after tests, before any push or PR write.
    local current
    current=$(pr_state "$tag")
    [[ $current != closed ]] || return 0
    if [[ $mode == resume ]]; then
        [[ $current == open:* && ${current##*:} == "$head" ]] || fail 'candidate moved during test'
    elif [[ $mode == discover ]]; then
        [[ $current == none ]] || fail 'candidate appeared during discovery'
        git push --force-with-lease="refs/heads/rebase/$tag:" origin "$head:refs/heads/rebase/$tag"
    else
        candidate=${candidate:-refs/heads/rebase/$tag}
        git push --force-with-lease="$candidate:$expected" origin "$head:$candidate"
        # Proof refs never update the live candidate PR body or label.
        [[ $candidate == "refs/heads/rebase/$tag" ]] || return 0
    fi
    body=$(mktemp "$STORE/body.XXXXXX")
    artifact="${GITHUB_SERVER_URL:-https://github.com}/${GITHUB_REPOSITORY:-iwnlcern/codex}/actions/runs/${GITHUB_RUN_ID:-local}#artifacts"
    printf 'state: %s\nupstream: %s\nmonitor-commit: %s\ntested-head: %s\n\nTest artifacts: %s\n' "$state" "$(sha "refs/tags/$tag")" "$monitor" "$tested" "$artifact" > "$body"
    printf '\n```diff\n' >> "$body"
    git diff "refs/tags/$tag..$head" >> "$body"
    printf '```\n' >> "$body"
    if [[ $mode == discover ]]; then
        gh label create "monitor-rebase/$tag" --force
        gh pr create --base main --head "rebase/$tag" --title "Monitor rebase onto $tag" --body-file "$body" --label "monitor-rebase/$tag"
    else
        gh pr edit "rebase/$tag" --body-file "$body"
    fi
}
case ${1:-} in
    prepare) shift; prepare "$@";;
    test) [[ $# == 2 ]] || fail 'test requires head'; test_head "$2";;
    report) report;;
    carried) carried "${2:?tag required}";;
    pr-state) pr_state "${2:?tag required}";;
    landing) landing "${2:?tag required}";;
    transition) shift; transition "$@";;
    build) shift; exec bash "$ROOT/scripts/build-release.sh" "$@";;
    *) fail 'usage: rebase-monitor.sh prepare|test|build|report|pr-state|carried|landing|transition';;
esac
