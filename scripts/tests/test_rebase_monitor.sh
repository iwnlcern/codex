#!/usr/bin/env bash
set -euo pipefail
# Usage: test_rebase_monitor.sh SOURCE SQ DEVELOPMENT_HEAD EVIDENCE
# The workflow suite sources these fixture helpers; no production code is replaced.
fixture() {
    local source=$1 sq=$2 development=$3 evidence=$4
    mkdir -p "$evidence"
    EVIDENCE=$(cd "$evidence" && pwd)
    FIXTURE=$(mktemp -d "$EVIDENCE/scratch.XXXXXX")
    git init --bare "$FIXTURE/remote.git"
    git clone --shared --no-checkout "$source" "$FIXTURE/clone"
    cd "$FIXTURE/clone"
    git config user.name 'Monitor rebase test'
    git config user.email 'monitor-test@example.invalid'
    git fetch origin refs/scratch/rebase-candidate
    [[ $(git rev-parse FETCH_HEAD) == "$sq" ]]
    [[ $(git rev-parse "$sq^") == "$(git rev-parse rust-v0.154.0^{commit})" ]]
    [[ $(git rev-parse "$sq^{tree}") == "$(git rev-parse "$development^{tree}")" ]]
    git checkout --detach "$sq"
    git remote rename origin source
    git remote add origin "$FIXTURE/remote.git"
    git remote add upstream "$FIXTURE/remote.git"
    git branch -f main "$sq"
    git tag -f v0.154.0-monitor.1 "$sq"
    git push origin main refs/tags/rust-v0.154.0 refs/tags/rust-v0.142.0
    export MONITOR_REBASE_RESULTS="$EVIDENCE/receipts"
    SCRIPT="$PWD/scripts/rebase-monitor.sh"
    SQ=$sq
    TAG=rust-v0.154.0
    pass candidate_fetched_has_tag_parent_and_head_tree
}
pass() { printf 'PASS %s\n' "$1"; }
expect_status() {
    local expected=$1 log=$2 rc=0
    shift 2
    "$@" > "$log" 2>&1 || rc=$?
    cat "$log"
    [[ $rc == "$expected" ]] || { echo "expected $expected, got $rc" >&2; exit 1; }
}
# Construct scratch fixture commits without editing the checkout.
with_file() {
    local base=$1 parent=$2 path=$3 content=$4 idx blob tree
    idx=$(mktemp "$FIXTURE/index.XXXXXX"); rm "$idx"
    blob=$(printf '%s\n' "$content" | git hash-object -w --stdin)
    GIT_INDEX_FILE="$idx" git read-tree "$base"
    GIT_INDEX_FILE="$idx" git update-index --add --cacheinfo "100644,$blob,$path"
    tree=$(GIT_INDEX_FILE="$idx" git write-tree)
    rm "$idx"
    printf 'scratch fixture %s\n' "$path" | git commit-tree "$tree" -p "$parent^{commit}"
}
if [[ ${BASH_SOURCE[0]} == "$0" ]]; then
    fixture "$@"
    expect_status 1 "$EVIDENCE/prerelease.log" bash "$SCRIPT" prepare rust-v0.155.0-alpha.1 --monitor-commit "$SQ"
    pass prepare_refuses_prerelease_tag
    printf dirty > dirty-fixture
    expect_status 1 "$EVIDENCE/dirty.log" bash "$SCRIPT" prepare "$TAG" --monitor-commit "$SQ"
    rm dirty-fixture
    pass prepare_refuses_dirty_worktree
    expect_status 0 "$EVIDENCE/clean-prepare.log" bash "$SCRIPT" prepare "$TAG" --monitor-commit "$SQ"
    common=$(git rev-parse --git-common-dir)
    prepared=$(cat "$common/monitor-rebase/prepared")
    head=$(git -C "$prepared" rev-parse HEAD)
    [[ $(git -C "$prepared" rev-parse HEAD^) == "$(git rev-parse "$TAG^{commit}")" ]]
    pass prepare_onto_current_tag_is_clean
    expect_status 2 "$EVIDENCE/reference-prepare.log" bash "$SCRIPT" prepare "$TAG" --monitor-commit ae7dbe6
    conflicted=$(cat "$common/monitor-rebase/prepared")
    python3 - "$conflicted/REBASE-REPORT.md" <<'PY'
import sys
actual=set(open(sys.argv[1]).read().splitlines()[2:])
expected={'codex-rs/'+p for p in ('core/src/context/contextual_user_message.rs','core/src/session/handlers.rs','core/src/session/session.rs','core/src/session/tests.rs','core/src/state/service.rs','core/src/tools/spec_plan.rs','core/src/unified_exec/mod.rs','features/src/lib.rs')}
assert actual==expected, actual
PY
    pass prepare_reference_onto_tag_reproduces_eight_conflicts
    (cd "$conflicted"; bash "$SCRIPT" report)
    [[ $(grep -c '^codex-rs/' "$conflicted/REBASE-REPORT.md") == 8 ]]
    pass report_summarizes_conflicts
    bad=$(with_file "$SQ" "$TAG" REBASE-REPORT.md 'unresolved')
    [[ $(git rev-parse "$bad^") == "$(git rev-parse "$TAG^{commit}")" ]]
    pass constructed_fixture_parent_is_peeled_tag
    expect_status 3 "$EVIDENCE/ineligible.log" bash "$SCRIPT" test "$bad"
    pass test_refuses_ineligible_head
    # A listing-stage failure reserves its directory before any receipt write.
    failed_root="$EVIDENCE/listing-failure-receipts"
    expect_status 1 "$EVIDENCE/first-listing-failure.log" env MONITOR_REBASE_RESULTS="$failed_root" RUSTC=/nonexistent-monitor-rebase-negative-control bash "$SCRIPT" test "$head"
    [[ ! -e $failed_root/$head/run.log ]]
    grep -q '/nonexistent-monitor-rebase-negative-control' "$failed_root/$head/list.stderr"
    python3 - "$failed_root/$head" "$EVIDENCE/listing-failure-bytes.json" <<'PY'
import json,pathlib,sys
root=pathlib.Path(sys.argv[1])
files={str(p.relative_to(root)):p.read_bytes().hex() for p in root.rglob('*') if p.is_file()}
assert {'tested-head','Cargo.lock.before','list.json','list.stderr','lock-list.txt','lock-final.txt'} <= files.keys()
pathlib.Path(sys.argv[2]).write_text(json.dumps(files,sort_keys=True))
PY
    expect_status 1 "$EVIDENCE/second-listing-refused.log" env MONITOR_REBASE_RESULTS="$failed_root" RUSTC=/nonexistent-monitor-rebase-negative-control bash "$SCRIPT" test "$head"
    grep -q 'receipt already exists' "$EVIDENCE/second-listing-refused.log"
    python3 - "$failed_root/$head" "$EVIDENCE/listing-failure-bytes.json" <<'PY'
import json,pathlib,sys
root=pathlib.Path(sys.argv[1])
actual={str(p.relative_to(root)):p.read_bytes().hex() for p in root.rglob('*') if p.is_file()}
assert actual==json.loads(pathlib.Path(sys.argv[2]).read_text()), 'failed receipt bytes changed'
PY
    pass listing_failure_receipt_is_exclusively_preserved
    # This clean-head run also discriminates workspace-local JUnit from a shared
    # external Cargo build cache; it is not a separate plain-target witness.
    shared_target=${CARGO_TARGET_DIR:-$FIXTURE/shared-build-cache}
    mkdir -p "$shared_target"
    shared_target=$(cd "$shared_target" && pwd)
    expect_status 0 "$EVIDENCE/clean-test.log" env CARGO_TARGET_DIR="$shared_target" bash "$SCRIPT" test "$head"
    pass test_passes_on_clean_head
    python3 - "$MONITOR_REBASE_RESULTS/$head" "$shared_target" <<'PY'
import json,pathlib,sys,xml.etree.ElementTree as ET
receipt=pathlib.Path(sys.argv[1]); shared=pathlib.Path(sys.argv[2]).resolve()
listing=json.loads((receipt/'list.json').read_text())
workspace=pathlib.Path(listing['rust-suites']['codex-core']['cwd']).parent.resolve()
assert pathlib.Path(listing['rust-build-meta']['target-directory']).resolve()==shared
assert not shared.is_relative_to(workspace), 'cache must be outside candidate workspace'
selected={(binary,name) for binary,suite in listing['rust-suites'].items() for name,t in suite['testcases'].items() if t['filter-match']['status']=='matches'}
cases=list(ET.parse(receipt/'junit.xml').iter('testcase'))
actual=[(t.attrib['classname'],t.attrib['name']) for t in cases]
assert len(actual)==len(set(actual))==len(selected)==69 and set(actual)==selected
assert all(t.find(kind) is None for t in cases for kind in ('failure','error','skipped'))
PY
    pass shared_external_cache_preserves_receipt_copy_and_identity
    [[ $(bash "$SCRIPT" carried "$TAG") == yes ]]
    pass carried_yes_when_main_parent_is_tag
    git update-ref refs/heads/main "$(git rev-parse "$TAG^{commit}")"
    [[ $(bash "$SCRIPT" carried "$TAG") == no ]]
    pass carried_no_otherwise
    git update-ref refs/heads/main "$SQ"
    [[ -z $(git status --porcelain) ]]
    echo "Scratch evidence retained at $FIXTURE"
fi
