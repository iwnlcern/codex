#!/usr/bin/env bash
set -euo pipefail
# Real production transitions, real git remotes/leases and real Rust test commands.
# Only GitHub's PR/label API is stubbed. Every case has a separate receipt root.
source "$(dirname "$0")/test_rebase_monitor.sh"
fixture "$@"
mkdir "$FIXTURE/bin"
export GH_FIXTURE="$FIXTURE/github.json"
printf '{}\n' > "$GH_FIXTURE"
cat > "$FIXTURE/bin/gh" <<'PY'
#!/usr/bin/env python3
import json,os,pathlib,subprocess,sys
args=sys.argv[1:]; path=pathlib.Path(os.environ['GH_FIXTURE']); db=json.loads(path.read_text())
def opt(key): return args[args.index(key)+1]
def head(branch):
    row=subprocess.check_output(['git','ls-remote','origin','refs/heads/'+branch],text=True).strip()
    if not row: raise SystemExit('missing candidate ref')
    return row.split()[0]
if args[:2]==['label','create']: sys.exit()
if args[:2]==['pr','list']:
    if '--label' not in args:
        assert opt('--state')=='open' and opt('--json')=='labels'
        print(json.dumps([{'labels':[{'name':label}]} for label,p in db.items() if p['state']=='OPEN']))
        sys.exit()
    label=opt('--label'); p=db.get(label)
    print(json.dumps([] if p is None else [dict(state=p['state'],body=p['body'],headRefOid=head(p['branch']),headRefName=p['branch'],isCrossRepository=False)]))
elif args[:2]==['pr','create']:
    label=opt('--label')
    if label in db: raise SystemExit('duplicate PR')
    db[label]=dict(state='OPEN',body=pathlib.Path(opt('--body-file')).read_text(),branch=opt('--head'),creates=1,edits=0)
    path.write_text(json.dumps(db))
elif args[:2]==['pr','edit']:
    matches=[p for p in db.values() if p['branch']==args[2]]
    if len(matches)!=1 or matches[0]['state']!='OPEN': raise SystemExit('not one open candidate')
    matches[0]['body']=pathlib.Path(opt('--body-file')).read_text(); matches[0]['edits']+=1
    path.write_text(json.dumps(db))
else: raise SystemExit('unexpected gh invocation: '+repr(args))
PY
chmod +x "$FIXTURE/bin/gh"
export PATH="$FIXTURE/bin:$PATH"
case_run() {
    local name=$1 expected=$2
    shift 2
    export MONITOR_REBASE_RESULTS="$EVIDENCE/$name-receipts"
    expect_status "$expected" "$EVIDENCE/$name.log" "$@"
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
assert 'Test artifacts: https://' in p['body'] and '#artifacts' in p['body']
assert '```diff' in p['body']
PY
}
case_run discover_skips_carried_tag 0 bash "$SCRIPT" transition discover --tag "$TAG"
[[ $(cat "$GH_FIXTURE") == '{}' ]]
pass discover_skips_carried_tag
# A controlled stable tag, one empty-tree commit newer than the carried upstream.
newbase=$(printf 'simulated upstream\n' | git commit-tree "$TAG^{tree}" -p "$TAG^{commit}")
newtag=rust-v0.154.1
git tag "$newtag" "$newbase"
git push origin "refs/tags/$newtag"
case_run discover_creates_pr_for_simulated_new_tag 0 bash "$SCRIPT" transition discover --tag "$newtag"
head=$(remote_head "refs/heads/rebase/$newtag")
assert_body "$newtag" clean "$head"
pass discover_creates_pr_for_simulated_new_tag
# Fail the real cargo invocation deliberately; do not counterfeit a JUnit receipt.
failedbase=$(printf 'simulated failing upstream\n' | git commit-tree "$TAG^{tree}" -p "$TAG^{commit}")
failedtag=rust-v0.154.2
git tag "$failedtag" "$failedbase"
git push origin "refs/tags/$failedtag"
case_run discover_records_tests_failed_with_artifact_link 0 env RUSTC=/nonexistent-monitor-rebase-negative-control bash "$SCRIPT" transition discover --tag "$failedtag"
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
case_run moved_head_is_landing_ineligible_until_receipt 1 bash "$SCRIPT" landing "$newtag"
pass moved_head_is_landing_ineligible_until_receipt
# Advance main to another valid carried commit while the repair stays unchanged.
origin_monitor=$(python3 - "$newtag" <<'PY'
import json,os,sys
body=json.load(open(os.environ['GH_FIXTURE']))['monitor-rebase/'+sys.argv[1]]['body']
print(body.splitlines()[2].removeprefix('monitor-commit: '))
PY
)
advanced_main=$(with_file "$SQ" "$newtag" advanced-main-fixture 'new carried patch')
git update-ref refs/heads/main "$advanced_main"
git tag v0.154.1-monitor.1 "$advanced_main"
[[ $advanced_main != "$origin_monitor" ]]
[[ $(bash "$SCRIPT" carried "$newtag") == yes ]]
[[ $(remote_head "refs/heads/rebase/$newtag") == "$repaired" ]]
before_main=$(git rev-parse main)
common=$(git rev-parse --git-common-dir)
before_prepare=$(cat "$common/monitor-rebase/prepared")
case_run resume_tests_repaired_head_and_updates_tested_head 0 bash "$SCRIPT" transition resume
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
git update-ref refs/heads/main "$SQ"
python3 - "$newtag" <<'PY'
import json,os,sys
p=os.environ['GH_FIXTURE']; d=json.load(open(p)); d['monitor-rebase/'+sys.argv[1]]['state']='CLOSED'; open(p,'w').write(json.dumps(d))
PY
closed_before=$(cat "$GH_FIXTURE")
for mode in discover resume regenerate; do
    args=(transition "$mode" --tag "$newtag")
    if [[ $mode == regenerate ]]; then args+=(--expected-head "$repaired"); fi
    case_run "closed-$mode" 0 bash "$SCRIPT" "${args[@]}"
done
[[ $(cat "$GH_FIXTURE") == "$closed_before" && $(remote_head "refs/heads/rebase/$newtag") == "$repaired" ]]
pass closed_candidate_is_noop
before_prepare=$(cat "$common/monitor-rebase/prepared")
case_run regenerate_rejects_candidate_ref_naming_live_candidate 1 bash "$SCRIPT" transition regenerate --tag "$TAG" --expected-head "$SQ" --candidate-ref "refs/heads/rebase/$TAG"
[[ $(cat "$common/monitor-rebase/prepared") == "$before_prepare" ]]
[[ -z $(remote_head "refs/heads/rebase/$TAG") ]]
pass regenerate_rejects_candidate_ref_naming_live_candidate
# The real lease proof starts at tag + TWO commits and expects the older SHA.
one=$(with_file "$TAG" "$TAG" lease-one 'one')
two=$(with_file "$one" "$one" lease-two 'two')
proof=refs/heads/rebase/scratch-lease
git push origin "$two:$proof"
case_run regenerate_refuses_stale_expected_head_on_existing_ref 1 bash "$SCRIPT" transition regenerate --tag "$TAG" --candidate-ref "$proof" --expected-head "$one"
grep -q 'stale info' "$EVIDENCE/regenerate_refuses_stale_expected_head_on_existing_ref.log"
[[ $(remote_head "$proof") == "$two" ]]
prepared=$(cat "$common/monitor-rebase/prepared")
[[ $(git -C "$prepared" rev-parse HEAD^) == "$(git rev-parse "$TAG^{commit}")" ]]
pass regenerate_refuses_stale_expected_head_on_existing_ref
case_run regenerate_with_matching_head_succeeds 0 bash "$SCRIPT" transition regenerate --tag "$TAG" --candidate-ref "$proof" --expected-head "$two"
newproof=$(remote_head "$proof")
[[ $newproof != "$two" && $(git rev-parse "$newproof^") == "$(git rev-parse "$TAG^{commit}")" ]]
[[ $(cat "$GH_FIXTURE") == "$closed_before" ]]
pass regenerate_with_matching_head_succeeds
# The reference patch yields the real eight conflicts, and repeated regeneration
# updates the existing PR with a report-only commit through the default ref path.
git update-ref refs/heads/main ae7dbe6
git tag -f v0.142.0-monitor.1 ae7dbe6
case_run initial_conflict 0 bash "$SCRIPT" transition discover --tag "$TAG"
assert_body "$TAG" conflict
old=$(remote_head "refs/heads/rebase/$TAG")
[[ $(git diff-tree --no-commit-id --name-only -r "$old") == REBASE-REPORT.md ]]
case_run regenerate_without_candidate_ref_targets_default_ref 0 bash "$SCRIPT" transition regenerate --tag "$TAG" --expected-head "$old"
latest=$(remote_head "refs/heads/rebase/$TAG")
prepared=$(cat "$common/monitor-rebase/prepared")
[[ $(git rev-parse "$latest^") == "$(git rev-parse "$TAG^{commit}")" ]]
assert_body "$TAG" conflict
pass regenerate_without_candidate_ref_targets_default_ref
python3 - "$TAG" <<'PY'
import json,os,sys
p=json.load(open(os.environ['GH_FIXTURE']))['monitor-rebase/'+sys.argv[1]]
assert p['creates']==1 and p['edits']==1
PY
pass repeated_conflict_run_updates_same_pr
git update-ref refs/heads/main "$SQ"
[[ -z $(git status --porcelain) ]]
echo "Scratch evidence retained at $FIXTURE"
