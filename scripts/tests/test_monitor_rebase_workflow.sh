#!/usr/bin/env bash
set -euo pipefail
# Real production transitions, real git remotes/leases and real Rust test commands.
# Only GitHub's PR/label API is stubbed. Every case has a separate receipt root.
source "$(dirname "$0")/test_rebase_monitor.sh"
HARNESS=$(cd "$(dirname "$0")" && pwd)/$(basename "$0")
fixture_args=("$@")
fixture "$@"
mkdir "$FIXTURE/bin"
export GH_FIXTURE="$FIXTURE/github.json"
printf '{}\n' > "$GH_FIXTURE"
cat > "$FIXTURE/bin/gh" <<'PY'
#!/usr/bin/env python3
import json,os,pathlib,subprocess,sys
args=sys.argv[1:]; path=pathlib.Path(os.environ['GH_FIXTURE']); db=json.loads(path.read_text())
with path.with_suffix('.calls').open('a') as calls: calls.write(json.dumps(args)+'\n')
mode=os.environ.get('GH_FAULT','')
def opt(key): return args[args.index(key)+1]
def body():
    text=pathlib.Path(opt('--body-file')).read_text()
    if len(text)>65536: raise SystemExit('Body is too long (maximum is 65536 characters)')
    return text
def head(branch):
    row=subprocess.check_output(['git','ls-remote','origin','refs/heads/'+branch],text=True).strip()
    if not row: raise SystemExit('missing candidate ref')
    return row.split()[0]
if args[:2]==['label','create']: sys.exit(1 if mode=='label' else 0)
if args[:2]==['pr','list']:
    marker=path.with_suffix('.create-error')
    if mode=='list' and marker.exists(): raise SystemExit('injected list failure')
    # A real second actor changes the remote after reconcile's initial ref read.
    if mode=='move-after-read':
        ref=os.environ['GH_MOVE_REF']; old=head(ref.removeprefix('refs/heads/'))
        subprocess.check_call(['git','push','--force-with-lease='+ref+':'+old,'origin',os.environ['GH_FOREIGN_HEAD']+':'+ref])
    if '--label' not in args:
        assert opt('--state')=='open' and opt('--json')=='labels'
        print(json.dumps([{'labels':[{'name':label}]} for label,p in db.items() if p['state']=='OPEN']))
        sys.exit()
    label=opt('--label'); p=db.get(label)
    print(json.dumps([] if p is None else [dict(state=p['state'],body=p['body'],headRefOid=head(p['branch']),headRefName=p['branch'],isCrossRepository=False,labels=[{'name':label}])]))
elif args[:2]==['pr','create']:
    label=opt('--label'); prepared=body()
    if mode in ('before','list','foreign'):
        path.with_suffix('.create-error').write_text('create failed')
        if mode=='foreign':
            ref='refs/heads/'+opt('--head'); old=head(opt('--head'))
            subprocess.check_call(['git','push','--force-with-lease='+ref+':'+old,'origin',os.environ['GH_FOREIGN_HEAD']+':'+ref])
            db[label]=dict(state='OPEN',body=prepared,branch=opt('--head'),creates=1,edits=0)
            path.write_text(json.dumps(db))
        raise SystemExit('injected create failure before recording')
    if label in db: raise SystemExit('duplicate PR')
    db[label]=dict(state='OPEN',body=prepared,branch=opt('--head'),creates=1,edits=0)
    path.write_text(json.dumps(db))
    if mode=='after': raise SystemExit('injected create failure after recording')
elif args[:2]==['pr','edit']:
    if mode=='edit': raise SystemExit('injected edit failure')
    matches=[p for p in db.values() if p['branch']==args[2]]
    if len(matches)!=1 or matches[0]['state']!='OPEN': raise SystemExit('not one open candidate')
    matches[0]['body']=body(); matches[0]['edits']+=1
    path.write_text(json.dumps(db))
else: raise SystemExit('unexpected gh invocation: '+repr(args))
PY
chmod +x "$FIXTURE/bin/gh"
export PATH="$FIXTURE/bin:$PATH"
case_run() {
    local name=$1 expected=$2 receipt=$3 before after common candidate
    shift 3
    common=$(git rev-parse --git-common-dir)
    before=$(cat "$common/monitor-rebase/prepared" 2>/dev/null || true)
    export MONITOR_REBASE_RESULTS="$EVIDENCE/$name-receipts"
    expect_status "$expected" "$EVIDENCE/$name.log" "$@"
    after=$(cat "$common/monitor-rebase/prepared" 2>/dev/null || true)
    case $receipt in
        clean|tests-failed)
            if [[ -n ${CASE_HEAD:-} ]]; then
                candidate=$CASE_HEAD
                [[ $before == "$after" ]]
            elif [[ $name == resume_tests_repaired_head_and_updates_tested_head ]]; then
                candidate=$repaired
                [[ $before == "$after" ]]
            else
                [[ -n $after && $before != "$after" ]]
                candidate=$(git -C "$after" rev-parse HEAD)
            fi
            assert_receipt "$receipt" "$candidate"
            ;;
        conflict)
            [[ -n $after && $before != "$after" && -f $after/REBASE-REPORT.md ]]
            [[ ! -e $MONITOR_REBASE_RESULTS || -f $MONITOR_REBASE_RESULTS/publication-hold.json ]]
            ;;
        reconcile)
            [[ $before == "$after" ]]
            [[ ! -e $MONITOR_REBASE_RESULTS || -f $MONITOR_REBASE_RESULTS/reconcile.json ]]
            ;;
        none)
            [[ $before == "$after" && ! -e $MONITOR_REBASE_RESULTS ]]
            ;;
        *) echo "unknown receipt expectation: $receipt" >&2; exit 1;;
    esac
}
remote_head() { git ls-remote origin "$1" | cut -f1; }
assert_body() {
    python3 - "$1" "$2" "${3:-}" <<'PY'
import json,os,sys
p=json.load(open(os.environ['GH_FIXTURE']))['monitor-rebase/'+sys.argv[1]]
lines=p['body'].splitlines()
assert [s.split(':')[0] for s in lines[:4]]==['state','upstream','monitor-commit','tested-head']
assert lines[0]=='state: '+sys.argv[2]
if sys.argv[3]: assert lines[3]=='tested-head: '+sys.argv[3]
else: assert lines[3]=='tested-head: none'
assert 'Test artifacts: https://' in p['body'] and '#artifacts' in p['body']
assert len(p['body'])<60000
head=__import__('subprocess').check_output(['git','ls-remote','origin','refs/heads/'+p['branch']],text=True).split()[0]
upstream=lines[1].removeprefix('upstream: ')
assert 'git diff --stat '+upstream+'..'+head in p['body']
assert '/commit/'+head in p['body'] and '/compare/'+sys.argv[1]+'...'+head in p['body']
PY
}
amendment_resume_cases() {
# Resume must refuse unprovenanced heads before reserving a test receipt.
no_trailer=$(printf 'deliberately missing provenance\n' | git commit-tree "$repaired^{tree}" -p "$newtag^{commit}")
git push --force-with-lease="refs/heads/rebase/$newtag:$repaired" origin "$no_trailer:refs/heads/rebase/$newtag"
untouched=$(cat "$GH_FIXTURE")
case_run candidate_without_provenance_trailer_is_refused 1 none bash "$SCRIPT" transition resume --tag "$newtag"
[[ $(cat "$GH_FIXTURE") == "$untouched" && $(remote_head "refs/heads/rebase/$newtag") == "$no_trailer" ]]
grep -q 'Monitor-Commit trailer' "$EVIDENCE/candidate_without_provenance_trailer_is_refused.log"
pass candidate_without_provenance_trailer_is_refused
git push --force-with-lease="refs/heads/rebase/$newtag:$no_trailer" origin "$repaired:refs/heads/rebase/$newtag"
# A failed edit preserves both the ref and the original receipt forever.
CASE_HEAD=$repaired case_run edit_failure_leaves_ref_and_tested_head 1 clean env GH_FAULT=edit bash "$SCRIPT" transition resume --tag "$newtag"
[[ $(cat "$GH_FIXTURE") == "$untouched" && $(remote_head "refs/heads/rebase/$newtag") == "$repaired" ]]
old_receipt=$MONITOR_REBASE_RESULTS
python3 - "$old_receipt" "$EVIDENCE/edit-failure-bytes.json" <<'PY'
import json,pathlib,sys
root=pathlib.Path(sys.argv[1]); pathlib.Path(sys.argv[2]).write_text(json.dumps({str(p.relative_to(root)):p.read_bytes().hex() for p in root.rglob('*') if p.is_file()},sort_keys=True))
PY
pass edit_failure_leaves_ref_and_tested_head
CASE_HEAD=$repaired case_run resume_after_edit_failure_retests_head_into_fresh_attempt_root 0 clean bash "$SCRIPT" transition resume --tag "$newtag"
[[ $old_receipt != "$MONITOR_REBASE_RESULTS" ]]
python3 - "$old_receipt" "$EVIDENCE/edit-failure-bytes.json" <<'PY'
import json,pathlib,sys
root=pathlib.Path(sys.argv[1]); assert {str(p.relative_to(root)):p.read_bytes().hex() for p in root.rglob('*') if p.is_file()}==json.loads(pathlib.Path(sys.argv[2]).read_text())
PY
assert_body "$newtag" clean "$repaired"
pass resume_after_edit_failure_retests_head_into_fresh_attempt_root
}
amendment_regenerate_cases() {
# Regeneration advances provenance even when publishing its fresh receipt fails.
git update-ref refs/heads/main "$advanced_main"
stale_body=$(cat "$GH_FIXTURE")
case_run regenerate_edit_failure 1 clean env GH_FAULT=edit bash "$SCRIPT" transition regenerate --tag "$newtag" --expected-head "$default_head"
newregen=$(remote_head "refs/heads/rebase/$newtag")
[[ $newregen != "$default_head" && $(cat "$GH_FIXTURE") == "$stale_body" ]]
CASE_HEAD=$newregen case_run resume_after_regenerate_edit_failure_publishes_trailer_provenance 0 clean bash "$SCRIPT" transition resume --tag "$newtag"
assert_body "$newtag" clean "$newregen"
python3 - "$GH_FIXTURE" "$newtag" "$advanced_main" "$stale_body" <<'PY'
import json,sys
key='monitor-rebase/'+sys.argv[2]
body=json.load(open(sys.argv[1]))[key]['body']
assert body.splitlines()[2]=='monitor-commit: '+sys.argv[3]
assert json.loads(sys.argv[4])[key]['body'].splitlines()[2]!=body.splitlines()[2]
PY
pass resume_after_regenerate_edit_failure_publishes_trailer_provenance
git update-ref refs/heads/main "$SQ"
}
# Publication witnesses use the real known-conflict patch, so no Rust receipt is
# manufactured or needed. Clean resume witnesses below still run the real suites.
amendment_publication_cases() {
    local ctag=rust-v0.154.0 ref=refs/heads/rebase/rust-v0.154.0 pushed foreign before expected common
    common=$(git rev-parse --git-common-dir)
    # Isolate this lane's PR service and remote; preserve all earlier fixtures.
    git remote rename origin preceding-origin
    git remote add origin "$FIXTURE/publication.git"
    git init --bare "$FIXTURE/publication.git"
    export GH_FIXTURE="$FIXTURE/publication.json"
    printf '{}\n' > "$GH_FIXTURE"
    git update-ref refs/heads/main ae7dbe6
    git tag -f v0.142.0-monitor.1 ae7dbe6
    case_run pr_body_for_real_candidate_stays_under_limit_with_stat_and_links 0 conflict bash "$SCRIPT" transition discover --tag "$ctag"
    assert_body "$ctag" conflict
    pushed=$(remote_head "$ref")
    [[ $(git rev-parse "$pushed^") == "$(git rev-parse "$ctag^{commit}")" ]]
    pass pr_body_for_real_candidate_stays_under_limit_with_stat_and_links
    # Each new create witness receives an empty, owned local remote and PR store.
    publication_reset() {
        local current
        current=$(remote_head "$ref")
        if [[ -n $current ]]; then git push --force-with-lease="$ref:$current" origin ":$ref"; fi
        printf '{}\n' > "$GH_FIXTURE"
        rm -f "${GH_FIXTURE%.json}.create-error"
    }
    publication_reset
    case_run oversized_body_is_refused_before_any_push 1 conflict env GITHUB_SERVER_URL="$(python3 -c "print('x'*70000)")" bash "$SCRIPT" transition discover --tag "$ctag"
    [[ -z $(remote_head "$ref") && $(cat "$GH_FIXTURE") == '{}' ]]
    pass oversized_body_is_refused_before_any_push
    case_run create_error_after_server_created_pr_reconciles_as_published 0 conflict env GH_FAULT=after bash "$SCRIPT" transition discover --tag "$ctag"
    pushed=$(remote_head "$ref"); assert_body "$ctag" conflict
    pass create_error_after_server_created_pr_reconciles_as_published
    publication_reset
    case_run create_failure_with_observed_absence_holds_without_rollback 1 conflict env GH_FAULT=before bash "$SCRIPT" transition discover --tag "$ctag"
    pushed=$(remote_head "$ref"); [[ -n $pushed && $(cat "$GH_FIXTURE") == '{}' ]]
    grep -q 'publication: indeterminate' "$EVIDENCE/create_failure_with_observed_absence_holds_without_rollback.log"
    python3 - "$MONITOR_REBASE_RESULTS/publication-hold.json" "$ctag" "$pushed" <<'PY'
import json,sys
p=json.load(open(sys.argv[1])); assert p['tag']==sys.argv[2] and p['pushed-head']==sys.argv[3]
assert p['outcome']=='indeterminate' and p['command-output'] and p['read-time']
PY
    case_run discover_orphan_ref_lease_refuses 1 conflict bash "$SCRIPT" transition discover --tag "$ctag"
    [[ $(remote_head "$ref") == "$pushed" ]]
    pass create_failure_with_observed_absence_holds_without_rollback
    case_run reconcile_deletes_orphan_ref_at_expected_head_and_restores_none 0 reconcile bash "$SCRIPT" transition reconcile --tag "$ctag" --expected-head "$pushed"
    [[ -z $(remote_head "$ref") && $(bash "$SCRIPT" pr-state "$ctag") == none ]]
    case_run discover_retries_after_reconcile 0 conflict bash "$SCRIPT" transition discover --tag "$ctag"
    pass reconcile_deletes_orphan_ref_at_expected_head_and_restores_none
    pushed=$(remote_head "$ref")
    foreign=$(with_file "$pushed" "$ctag" second-actor 'foreign')
    publication_reset
    case_run create_failure_with_pr_at_foreign_head_holds 1 conflict env GH_FAULT=foreign GH_FOREIGN_HEAD="$foreign" bash "$SCRIPT" transition discover --tag "$ctag"
    [[ $(remote_head "$ref") == "$foreign" ]]
    grep -q 'publication: unresolved' "$EVIDENCE/create_failure_with_pr_at_foreign_head_holds.log"
    pass create_failure_with_pr_at_foreign_head_holds
    publication_reset
    case_run unknown_publication_state_mutates_nothing 1 conflict env GH_FAULT=list bash "$SCRIPT" transition discover --tag "$ctag"
    pushed=$(remote_head "$ref"); [[ -n $pushed && $(cat "$GH_FIXTURE") == '{}' ]]
    grep -q 'publication: unknown' "$EVIDENCE/unknown_publication_state_mutates_nothing.log"
    pass unknown_publication_state_mutates_nothing
    git push --force-with-lease="$ref:$pushed" origin "$foreign:$ref"
    case_run reconcile_refuses_ref_moved_before_its_read 1 reconcile bash "$SCRIPT" transition reconcile --tag "$ctag" --expected-head "$pushed"
    [[ $(remote_head "$ref") == "$foreign" ]]
    grep -q "$pushed" "$EVIDENCE/reconcile_refuses_ref_moved_before_its_read.log"
    grep -q "$foreign" "$EVIDENCE/reconcile_refuses_ref_moved_before_its_read.log"
    pass reconcile_refuses_ref_moved_before_its_read
    git push --force-with-lease="$ref:$foreign" origin "$pushed:$ref"
    case_run reconcile_refuses_ref_moved_after_its_read 1 reconcile env GH_FAULT=move-after-read GH_FOREIGN_HEAD="$foreign" GH_MOVE_REF="$ref" bash "$SCRIPT" transition reconcile --tag "$ctag" --expected-head "$pushed"
    [[ $(remote_head "$ref") == "$foreign" ]]
    grep -q 'stale info' "$EVIDENCE/reconcile_refuses_ref_moved_after_its_read.log"
    pass reconcile_refuses_ref_moved_after_its_read
    git push --force-with-lease="$ref:$foreign" origin "$pushed:$ref"
    # Materialize exactly the body the failed create sent, using the gh API stub.
    body=$(ls -t "$common"/monitor-rebase/body.* | head -1)
    gh pr create --head "rebase/$ctag" --body-file "$body" --label "monitor-rebase/$ctag"
    before=$(cat "$GH_FIXTURE")
    case_run reconcile_observes_delayed_publication_without_editing 0 reconcile bash "$SCRIPT" transition reconcile --tag "$ctag" --expected-head "$pushed"
    [[ $(cat "$GH_FIXTURE") == "$before" ]]
    grep -q 'publication observed' "$EVIDENCE/reconcile_observes_delayed_publication_without_editing.log"
    pass reconcile_observes_delayed_publication_without_editing
    python3 - "$GH_FIXTURE" <<'PY'
import json,sys
p=sys.argv[1]; d=json.load(open(p)); next(iter(d.values()))['body']='malformed'; open(p,'w').write(json.dumps(d))
PY
    before=$(cat "$GH_FIXTURE")
    case_run reconcile_refuses_malformed_or_stale_open_body 1 reconcile bash "$SCRIPT" transition reconcile --tag "$ctag" --expected-head "$pushed"
    [[ $(cat "$GH_FIXTURE") == "$before" && $(remote_head "$ref") == "$pushed" ]]
    grep -q resume "$EVIDENCE/reconcile_refuses_malformed_or_stale_open_body.log"
    pass reconcile_refuses_malformed_or_stale_open_body
    gh pr edit "rebase/$ctag" --body-file "$body"
    python3 - "$GH_FIXTURE" <<'PYCASE'
import json,sys
p=sys.argv[1]; d=json.load(open(p)); pr=next(iter(d.values())); lines=pr['body'].splitlines(); lines[2]='monitor-commit: '+'0'*40; pr['body']='\n'.join(lines)+'\n'; open(p,'w').write(json.dumps(d))
PYCASE
    before=$(cat "$GH_FIXTURE")
    case_run reconcile_stale_provenance 1 reconcile bash "$SCRIPT" transition reconcile --tag "$ctag" --expected-head "$pushed"
    [[ $(cat "$GH_FIXTURE") == "$before" ]]
    grep -q resume "$EVIDENCE/reconcile_stale_provenance.log"
    gh pr edit "rebase/$ctag" --body-file "$body"
    before=$(cat "$GH_FIXTURE")
    case_run reconcile_open_foreign 1 reconcile bash "$SCRIPT" transition reconcile --tag "$ctag" --expected-head "$foreign"
    [[ $(cat "$GH_FIXTURE") == "$before" && $(remote_head "$ref") == "$pushed" ]]
    python3 - "$GH_FIXTURE" <<'PY'
import json,sys
p=sys.argv[1]; d=json.load(open(p)); next(iter(d.values()))['state']='CLOSED'; open(p,'w').write(json.dumps(d))
PY
    before=$(cat "$GH_FIXTURE")
    case_run reconcile_closed 1 reconcile bash "$SCRIPT" transition reconcile --tag "$ctag" --expected-head "$pushed"
    [[ $(cat "$GH_FIXTURE") == "$before" && $(remote_head "$ref") == "$pushed" ]]
    pass reconcile_refuses_open_at_other_head_and_closed
    git update-ref refs/heads/main "$SQ"
    git remote remove origin
    git remote rename preceding-origin origin
    export GH_FIXTURE="$FIXTURE/github.json"
}
tag_version_mismatch_case() {
    local mismatch_tag=rust-v0.154.3 mismatch_base mismatch_head mode name previous_gh=$GH_FIXTURE
    local args
    export GH_FIXTURE="$FIXTURE/version-mismatch-github.json"
    printf '{}\n' > "$GH_FIXTURE"
    mismatch_base=$(with_workspace_version "$TAG" 0.154.1)
    git tag "$mismatch_tag" "$mismatch_base"
    git push origin "refs/tags/$mismatch_tag"
    # A fail-only tripwire prevents a regression from launching a Cargo build.
    # It cannot manufacture listing, JUnit, or a successful test receipt.
    mkdir "$FIXTURE/no-cargo"
    cat > "$FIXTURE/no-cargo/cargo" <<'SHCARGO'
#!/usr/bin/env bash
printf '%s\n' "$*" >> "$MONITOR_CARGO_TRIPWIRE"
echo 'unexpected Cargo invocation in version-identity witness' >&2
exit 97
SHCARGO
    chmod +x "$FIXTURE/no-cargo/cargo"
    for mode in discover resume regenerate; do
        name=tag_version_mismatch_is_tests_failed_before_build
        [[ $mode == discover ]] || name="${name}_$mode"
        args=(transition "$mode" --tag "$mismatch_tag")
        [[ $mode != regenerate ]] || args+=(--expected-head "$mismatch_head")
        if [[ $mode == resume ]]; then export CASE_HEAD=$mismatch_head; fi
        case_run "$name" 0 tests-failed env PATH="$FIXTURE/no-cargo:$PATH" MONITOR_CARGO_TRIPWIRE="$EVIDENCE/$name.cargo-invoked" bash "$SCRIPT" "${args[@]}"
        unset CASE_HEAD
        mismatch_head=$(remote_head "refs/heads/rebase/$mismatch_tag")
        assert_body "$mismatch_tag" tests-failed
        python3 - "$MONITOR_REBASE_RESULTS/$mismatch_head" <<'PYIDENTITY'
import json,pathlib,sys
p=pathlib.Path(sys.argv[1]); d=json.loads((p/'receipt-state.json').read_text())
assert d['failure']=='version-identity', d
assert d['monitor']==dict(inventory='not-run',run='not-run',**{'identity-failure':'not-run','junit-sha256':None}), d
assert d['features']=='not-run' and 'tested-head' not in d, d
assert not any((p/name).exists() for name in ('junit.xml','tested-head','list.json','run.log','features.log'))
PYIDENTITY
        [[ ! -e $EVIDENCE/$name.cargo-invoked ]]
        pass "$name"
    done
    # Keep later scheduled resume cases focused on their own candidate.
    python3 - "$mismatch_tag" <<'PYCLOSE'
import json,os,sys
p=os.environ['GH_FIXTURE']; d=json.load(open(p)); d['monitor-rebase/'+sys.argv[1]]['state']='CLOSED'; open(p,'w').write(json.dumps(d))
PYCLOSE
    export GH_FIXTURE="$previous_gh"
}
if [[ ${MONITOR_REBASE_AMENDMENT_C_ONLY:-0} == 1 ]]; then
    amendment_publication_cases
    exit 0
fi

tag_version_mismatch_case
if [[ ${MONITOR_REBASE_VERSION_ONLY:-0} == 1 ]]; then exit 0; fi

if [[ ${MONITOR_REBASE_AMENDMENT_C_ONLY:-0} == clean ]]; then
    newbase=$(with_workspace_version "$TAG" 0.154.1)
    newtag=rust-v0.154.1
    git tag "$newtag" "$newbase"
    git push origin "refs/tags/$newtag"
    case_run clean_authoring_discover 0 clean bash "$SCRIPT" transition discover --tag "$newtag"
    head=$(remote_head "refs/heads/rebase/$newtag")
    assert_body "$newtag" clean "$head"
    repaired=$(with_file "$head" "$newtag" repaired-fixture repair)
    git push --force-with-lease="refs/heads/rebase/$newtag:$head" origin "$repaired:refs/heads/rebase/$newtag"
    CASE_HEAD=$repaired case_run normal_resume_of_moved_head_tests_current_head 0 clean bash "$SCRIPT" transition resume --tag "$newtag"
    assert_body "$newtag" clean "$repaired"
    pass normal_resume_of_moved_head_tests_current_head
    amendment_resume_cases
    advanced_main=$(with_file "$head" "$newtag" advanced-main-fixture 'new carried patch')
    git tag v0.154.1-monitor.1 "$advanced_main"
    default_head=$repaired
    amendment_regenerate_cases
    exit 0
fi
case_run discover_skips_carried_tag 0 none bash "$SCRIPT" transition discover --tag "$TAG"
[[ $(cat "$GH_FIXTURE") == '{}' ]]
pass discover_skips_carried_tag
# A controlled stable tag with a real workspace manifest version bump.
newbase=$(with_workspace_version "$TAG" 0.154.1)
newtag=rust-v0.154.1
git tag "$newtag" "$newbase"
git push origin "refs/tags/$newtag"
if [[ ${MONITOR_REBASE_HARNESS_FAULT:-0} == 1 ]]; then
    case_run discover_creates_pr_for_simulated_new_tag 0 clean env RUSTC=/nonexistent-monitor-rebase-negative-control bash "$SCRIPT" transition discover --tag "$newtag"
else
    case_run discover_creates_pr_for_simulated_new_tag 0 clean bash "$SCRIPT" transition discover --tag "$newtag"
fi
# The negative subprocess must stop inside the clean receipt assertion above.
if [[ ${MONITOR_REBASE_HARNESS_FAULT:-0} == 1 ]]; then
    printf 'next case started\n' > "$MONITOR_REBASE_NEXT_CASE_SENTINEL"
    exit 0
fi
head=$(remote_head "refs/heads/rebase/$newtag")
assert_body "$newtag" clean "$head"
assert_receipt clean "$head"
pass discover_creates_pr_for_simulated_new_tag
# Fail the real cargo invocation deliberately; do not counterfeit a JUnit receipt.
failedbase=$(with_workspace_version "$TAG" 0.154.2)
failedtag=rust-v0.154.2
git tag "$failedtag" "$failedbase"
git push origin "refs/tags/$failedtag"
case_run discover_records_tests_failed_with_artifact_link 0 tests-failed env RUSTC=/nonexistent-monitor-rebase-negative-control bash "$SCRIPT" transition discover --tag "$failedtag"
assert_body "$failedtag" tests-failed
pass discover_records_tests_failed_with_artifact_link
# Keep scheduling focused on the repaired candidate; archive the failure control.
python3 - "$failedtag" <<'PY'
import json,os,sys
p=os.environ['GH_FIXTURE']; d=json.load(open(p)); d['monitor-rebase/'+sys.argv[1]]['state']='CLOSED'; open(p,'w').write(json.dumps(d))
PY
# Move a previously green candidate to an untested, structurally eligible repair.
repaired=$(with_file "$head" "$newtag" repaired-fixture 'repair')
git push --force-with-lease="refs/heads/rebase/$newtag:$head" origin "$repaired:refs/heads/rebase/$newtag"
case_run moved_head_is_landing_ineligible_until_receipt 1 none bash "$SCRIPT" landing "$newtag"
pass moved_head_is_landing_ineligible_until_receipt
# Advance main to another valid carried commit while the repair stays unchanged.
origin_monitor=$(python3 - "$newtag" <<'PY'
import json,os,sys
body=json.load(open(os.environ['GH_FIXTURE']))['monitor-rebase/'+sys.argv[1]]['body']
print(body.splitlines()[2].removeprefix('monitor-commit: '))
PY
)
advanced_main=$(with_file "$head" "$newtag" advanced-main-fixture 'new carried patch')
git update-ref refs/heads/main "$advanced_main"
git tag v0.154.1-monitor.1 "$advanced_main"
[[ $advanced_main != "$origin_monitor" ]]
[[ $(bash "$SCRIPT" carried "$newtag") == yes ]]
[[ $(remote_head "refs/heads/rebase/$newtag") == "$repaired" ]]
before_main=$(git rev-parse main)
common=$(git rev-parse --git-common-dir)
before_prepare=$(cat "$common/monitor-rebase/prepared")
case_run resume_tests_repaired_head_and_updates_tested_head 0 clean bash "$SCRIPT" transition resume
[[ $(remote_head "refs/heads/rebase/$newtag") == "$repaired" ]]
[[ $(cat "$common/monitor-rebase/prepared") == "$before_prepare" ]]
assert_body "$newtag" clean "$repaired"
bash "$SCRIPT" landing "$newtag"
[[ $(git rev-parse main) == "$before_main" ]]
pass resume_tests_repaired_head_and_updates_tested_head
python3 - "$newtag" "$origin_monitor" <<'PY'
import json,os,sys
body=json.load(open(os.environ['GH_FIXTURE']))['monitor-rebase/'+sys.argv[1]]['body']
assert body.splitlines()[2]=='monitor-commit: '+sys.argv[2]
PY
pass resume_preserves_originating_monitor_after_main_advances
pass scheduled_resume_without_tag_tests_existing_repair
pass normal_resume_of_moved_head_tests_current_head
amendment_resume_cases
git update-ref refs/heads/main "$SQ"
python3 - "$newtag" <<'PY'
import json,os,sys
p=os.environ['GH_FIXTURE']; d=json.load(open(p)); d['monitor-rebase/'+sys.argv[1]]['state']='CLOSED'; open(p,'w').write(json.dumps(d))
PY
closed_before=$(cat "$GH_FIXTURE")
for mode in discover resume regenerate; do
    args=(transition "$mode" --tag "$newtag")
    if [[ $mode == regenerate ]]; then args+=(--expected-head "$repaired"); fi
    case_run "closed-$mode" 0 none bash "$SCRIPT" "${args[@]}"
done
[[ $(cat "$GH_FIXTURE") == "$closed_before" && $(remote_head "refs/heads/rebase/$newtag") == "$repaired" ]]
pass closed_candidate_is_noop
before_prepare=$(cat "$common/monitor-rebase/prepared")
case_run regenerate_rejects_candidate_ref_naming_live_candidate 1 none bash "$SCRIPT" transition regenerate --tag "$TAG" --expected-head "$SQ" --candidate-ref "refs/heads/rebase/$TAG"
[[ $(cat "$common/monitor-rebase/prepared") == "$before_prepare" ]]
[[ -z $(remote_head "refs/heads/rebase/$TAG") ]]
pass regenerate_rejects_candidate_ref_naming_live_candidate
# The real lease proof starts at tag + TWO commits and expects the older SHA.
one=$(with_file "$TAG" "$TAG" lease-one 'one')
two=$(with_file "$one" "$one" lease-two 'two')
proof=refs/heads/rebase/scratch-lease
git push origin "$two:$proof"
case_run regenerate_refuses_stale_expected_head_on_existing_ref 1 clean bash "$SCRIPT" transition regenerate --tag "$TAG" --candidate-ref "$proof" --expected-head "$one"
grep -q 'stale info' "$EVIDENCE/regenerate_refuses_stale_expected_head_on_existing_ref.log"
[[ $(remote_head "$proof") == "$two" ]]
prepared=$(cat "$common/monitor-rebase/prepared")
[[ $(git -C "$prepared" rev-parse HEAD^) == "$(git rev-parse "$TAG^{commit}")" ]]
pass regenerate_refuses_stale_expected_head_on_existing_ref
case_run regenerate_with_matching_head_succeeds 0 clean bash "$SCRIPT" transition regenerate --tag "$TAG" --candidate-ref "$proof" --expected-head "$two"
newproof=$(remote_head "$proof")
assert_receipt clean "$newproof"
[[ $newproof != "$two" && $(git rev-parse "$newproof^") == "$(git rev-parse "$TAG^{commit}")" ]]
[[ $(cat "$GH_FIXTURE") == "$closed_before" ]]
pass regenerate_with_matching_head_succeeds
# Omission of candidate-ref has its own clean, structurally resolvable fixture.
python3 - "$newtag" <<'PY'
import json,os,sys
p=os.environ['GH_FIXTURE']; d=json.load(open(p)); d['monitor-rebase/'+sys.argv[1]]['state']='OPEN'; open(p,'w').write(json.dumps(d))
PY
case_run regenerate_without_candidate_ref_targets_default_ref 0 clean bash "$SCRIPT" transition regenerate --tag "$newtag" --expected-head "$repaired"
default_head=$(remote_head "refs/heads/rebase/$newtag")
assert_body "$newtag" clean "$default_head"
assert_receipt clean "$default_head"
prepared=$(cat "$common/monitor-rebase/prepared")
[[ $(git -C "$prepared" rev-parse HEAD) == "$default_head" ]]
pass regenerate_without_candidate_ref_targets_default_ref
amendment_regenerate_cases
# The reference patch yields the real eight conflicts, and repeated regeneration
# updates the existing PR with a report-only commit through the default ref path.
git update-ref refs/heads/main ae7dbe6
git tag -f v0.142.0-monitor.1 ae7dbe6
case_run initial_conflict 0 conflict bash "$SCRIPT" transition discover --tag "$TAG"
assert_body "$TAG" conflict
old=$(remote_head "refs/heads/rebase/$TAG")
[[ $(git diff-tree --no-commit-id --name-only -r "$old") == REBASE-REPORT.md ]]
pass initial_conflict
case_run repeated_conflict_run_updates_same_pr 0 conflict bash "$SCRIPT" transition regenerate --tag "$TAG" --expected-head "$old"
latest=$(remote_head "refs/heads/rebase/$TAG")
prepared=$(cat "$common/monitor-rebase/prepared")
[[ $(git rev-parse "$latest^") == "$(git rev-parse "$TAG^{commit}")" ]]
assert_body "$TAG" conflict
python3 - "$TAG" <<'PY'
import json,os,sys
p=json.load(open(os.environ['GH_FIXTURE']))['monitor-rebase/'+sys.argv[1]]
assert p['creates']==1 and p['edits']==1
PY
pass repeated_conflict_run_updates_same_pr
git update-ref refs/heads/main "$SQ"
amendment_publication_cases
[[ -z $(git status --porcelain) ]]
negative_evidence="$EVIDENCE/harness-negative"
negative_sentinel="$EVIDENCE/negative-next-case-sentinel"
negative_rc=0
MONITOR_REBASE_HARNESS_FAULT=1 MONITOR_REBASE_NEXT_CASE_SENTINEL="$negative_sentinel"     bash "$HARNESS" "${fixture_args[0]}" "${fixture_args[1]}" "${fixture_args[2]}" "$negative_evidence"     > "$EVIDENCE/harness-negative.log" 2>&1 || negative_rc=$?
[[ $negative_rc != 0 && ! -e $negative_sentinel ]]
grep -q 'receipt assertion: wrong state or stale head' "$EVIDENCE/harness-negative.log"
pass harness_stops_on_unexpected_failed_receipt
echo "Scratch evidence retained at $FIXTURE"
