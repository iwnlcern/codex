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
# Verify the tested candidate independently of transition exit/publication success.
assert_receipt() {
    python3 - "$MONITOR_REBASE_RESULTS" "$1" "$2" <<'PY'
import hashlib,json,pathlib,re,sys,xml.etree.ElementTree as ET
root=pathlib.Path(sys.argv[1]); state,head=sys.argv[2:]
def require(ok,reason):
    if not ok: raise SystemExit('receipt assertion: '+reason)
require(re.fullmatch('[0-9a-f]{40}',head), 'invalid expected head')
require(root.is_dir() and sorted(p.name for p in root.iterdir())==[head], 'missing or extra candidate receipt')
p=root/head
try:
    d=json.loads((p/'receipt-state.json').read_text())
    require(d['state']==state and d['head']==head, 'wrong state or stale head')
    require(set(d)=={'state','head','monitor','features','failure'} | ({'tested-head'} if state=='clean' else set()), 'malformed state keys')
    require((p/'head').read_text().strip()==head, 'head file mismatch')
    m=d['monitor']
    require(set(m)=={'inventory','run','identity-failure','junit-sha256'}, 'malformed monitor proof')
    require(all(m[k] in ('passed','failed','not-run') for k in ('inventory','run','identity-failure')), 'invalid monitor verdict')
    require(d['features'] in ('passed','failed','not-run'), 'invalid features verdict')
    junit=p/'junit.xml'
    if junit.exists():
        require(m['junit-sha256']==hashlib.sha256(junit.read_bytes()).hexdigest(), 'JUnit digest mismatch')
    else:
        require(m['junit-sha256'] is None and m['identity-failure']=='not-run', 'invented JUnit proof')
    if m['inventory']!='passed':
        require(all(m[k]=='not-run' for k in ('run','identity-failure')) and d['features']=='not-run', 'execution after failed inventory')
    if m['run']=='not-run':
        require(not junit.exists() and m['identity-failure']=='not-run', 'JUnit proof without monitor run')
    if m['identity-failure']!='not-run':
        require(junit.is_file() and m['run']!='not-run', 'identity verdict without JUnit')
    if m['run']!='passed' or m['identity-failure']!='passed':
        require(d['features']=='not-run', 'features executed before monitor success')
    if m['identity-failure']=='passed':
        listed=[]
        for binary,suite in json.loads((p/'list.json').read_text())['rust-suites'].items():
            for name,t in suite['testcases'].items():
                if t['filter-match']['status']=='matches':
                    require(not t['ignored'], 'ignored inventory member')
                    require(binary==('codex-core::all' if name.startswith('suite::') else 'codex-core'), 'wrong inventory binary')
                    listed.append((binary,name))
        cases=[]
        for t in ET.parse(junit).getroot().iter('testcase'):
            require(not any(t.find(k) is not None for k in ('failure','error','skipped')), 'failed or skipped JUnit member')
            cases.append((t.attrib['classname'],t.attrib['name']))
        require(len(listed)==len(set(listed))==len(cases)==len(set(cases))==72 and set(listed)==set(cases), 'inventory/JUnit mismatch')
    if d['features']!='not-run':
        require((p/'features.log').is_file(), 'missing features proof')
    if state=='clean':
        require(d['tested-head']==head and (p/'tested-head').read_text().strip()==head, 'tested-head mismatch')
        require(d['failure'] is None and all(m[k]=='passed' for k in ('inventory','run','identity-failure')) and d['features']=='passed', 'contradictory clean verdict')
    else:
        require(state=='tests-failed' and not (p/'tested-head').exists(), 'failure claims tested-head')
        require(isinstance(d['failure'],str) and bool(d['failure']), 'missing failure reason')
        require(not (all(m[k]=='passed' for k in ('inventory','run','identity-failure')) and d['features']=='passed' and d['failure']!='lock-final'), 'contradictory failure verdict')
except (OSError,ValueError,KeyError,TypeError,ET.ParseError) as error:
    raise SystemExit('receipt assertion: malformed or missing artifact: '+str(error))
print('receipt assertion: '+state+' '+head)
PY
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
    { printf 'scratch fixture %s\n\n' "$path"; git log -1 --format=%B "$base" | sed -n '/^Monitor-Commit: /p'; } | git commit-tree "$tree" -p "$parent^{commit}"

}
# These constructed lockfiles exercise version normalization without invoking Cargo.
lockfile_guard_cases() {
    local root=$1 evidence=$2 selected=${3:-all}
    mkdir -p "$evidence"
    python3 - "$root" "$evidence" "$selected" <<'PYGUARD'
import json,pathlib,re,subprocess,sys,tomllib
root=pathlib.Path(sys.argv[1]); out=pathlib.Path(sys.argv[2]); selected=sys.argv[3]
before=subprocess.check_output(['git','-C',str(root),'show','rust-v0.154.0:codex-rs/Cargo.lock']).decode()
parts=before.split('[[package]]')
bumped=[]
for block in parts[1:]:
    package=tomllib.loads('[[package]]'+block)['package'][0]
    if 'source' not in package and package['version']=='0.0.0':
        block=re.sub(r'(?m)^version = "0\.0\.0"$', 'version = "0.154.1"', block, count=1)
    bumped.append(block)
after=parts[0]+''.join('[[package]]'+block for block in bumped)
changed=list(bumped)
for i,block in enumerate(changed):
    package=tomllib.loads('[[package]]'+block)['package'][0]
    if package.get('source','').startswith('registry+'):
        changed[i]=re.sub(r'(?m)^checksum = "[^"]+"$', 'checksum = "deliberately changed dependency"', block, count=1)
        assert changed[i]!=block
        break
else: raise AssertionError('real base has no registry package')
for name,text in {'before.lock':before,'bump.lock':after,'dependency.lock':parts[0]+''.join('[[package]]'+block for block in changed),
                  'bumped.toml':'[workspace.package]\nversion = "0.154.1"\n',
                  'old.toml':'[workspace.package]\nversion = "0.154.0"\n'}.items():
    (out/name).write_text(text)
# A recorded target version is already legal unchanged data; cancel identical
# records with their exact multiplicity before admitting directional bumps.
workspace_index=next(i for i,block in enumerate(parts[1:])
                     if 'source' not in tomllib.loads('[[package]]'+block)['package'][0]
                     and tomllib.loads('[[package]]'+block)['package'][0]['version']=='0.0.0')
target=list(parts[1:]); target[workspace_index]=bumped[workspace_index]
other=list(target); other[workspace_index]=target[workspace_index].replace('version = "0.154.1"','version = "0.153.0"',1)
def lockfile(blocks): return parts[0]+''.join('[[package]]'+block for block in blocks)
for name,text in {'target.lock':lockfile(target), 'other.lock':lockfile(other),
                  'removed.lock':lockfile(target[:workspace_index]+target[workspace_index+1:]),
                  'duplicate.lock':lockfile(target+[target[workspace_index]]),
                  'mixed-duplicate-before.lock':lockfile(target+[parts[1:][workspace_index]]),
                  'mixed-duplicate-after.lock':lockfile(target+[target[workspace_index]]),
                  'non-package.lock':before.replace('version = 4','version = 3',1)}.items():
    (out/name).write_text(text)
assert (out/'non-package.lock').read_text()!=before
cases=[('lockfile_guard_accepts_workspace_bump_named_by_manifest','before.lock','bump.lock','bumped.toml',0,'clean sha256='),
       ('lockfile_guard_rejects_bump_not_named_by_manifest','before.lock','bump.lock','old.toml',1,'lockfile delta rejected'),
       ('lockfile_guard_rejects_dependency_change_under_bump','before.lock','dependency.lock','bumped.toml',1,'lockfile delta rejected'),
       ('lockfile_guard_requires_manifest','before.lock','before.lock',None,1,'usage:'),
       ('lockfile_guard_accepts_unchanged_target_version','target.lock','target.lock','bumped.toml',0,'clean sha256='),
       ('lockfile_guard_accepts_mixed_unchanged_target_and_bump','target.lock','bump.lock','bumped.toml',0,'clean sha256='),
       ('lockfile_guard_accepts_unchanged_target_multiplicity','duplicate.lock','duplicate.lock','bumped.toml',0,'clean sha256='),
       ('lockfile_guard_accepts_mixed_duplicate_records','mixed-duplicate-before.lock','mixed-duplicate-after.lock','bumped.toml',0,'clean sha256='),
       ('lockfile_guard_accepts_unchanged_other_version','other.lock','other.lock','bumped.toml',0,'clean sha256='),
       ('lockfile_guard_rejects_nonzero_to_target','other.lock','bump.lock','bumped.toml',1,'lockfile delta rejected'),
       ('lockfile_guard_rejects_reverse_bump','bump.lock','before.lock','bumped.toml',1,'lockfile delta rejected'),
       ('lockfile_guard_rejects_mixed_duplicate_reverse','mixed-duplicate-after.lock','mixed-duplicate-before.lock','bumped.toml',1,'lockfile delta rejected'),
       ('lockfile_guard_rejects_removed_package','target.lock','removed.lock','bumped.toml',1,'lockfile delta rejected'),
       ('lockfile_guard_rejects_duplicate_package','target.lock','duplicate.lock','bumped.toml',1,'lockfile delta rejected'),
       ('lockfile_guard_rejects_non_package_change','before.lock','non-package.lock','bumped.toml',1,'lockfile delta rejected')]
# Even an unchanged lockfile cannot hide a missing or malformed version input.
invalid={'missing-file':None,'unparsable':'[workspace', 'workspace-type':'workspace = 1',
         'package-type':'[workspace]\npackage = []', 'missing-version':'[workspace.package]',
         'version-type':'[workspace.package]\nversion = 154',
         'empty-version':'[workspace.package]\nversion = ""',
         'invalid-version':'[workspace.package]\nversion = "not-a-version"'}
for name,content in invalid.items():
    file='invalid-'+name+'.toml'
    if content is not None: (out/file).write_text(content+'\n')
    cases.append(('lockfile_guard_rejects_'+name.replace('-','_'),'before.lock','before.lock',file,1,'error:'))
results=[]
for name,before_file,after_file,manifest,expected,diagnostic in cases:
    if selected not in ('all',name): continue
    command=[sys.executable,str(root/'scripts/lockfile-delta.py'),str(out/before_file),str(out/after_file)]
    if manifest: command.append(str(out/manifest))
    result=subprocess.run(command,text=True,capture_output=True)
    (out/(name+'.stdout')).write_text(result.stdout); (out/(name+'.stderr')).write_text(result.stderr)
    ok=result.returncode==expected and diagnostic in result.stdout+result.stderr
    results.append(dict(name=name,command=command,returncode=result.returncode,expected=expected,passed=ok))
    print(('PASS ' if ok else 'FAIL ')+name,flush=True)
(out/'results.json').write_text(json.dumps(results,indent=2)+'\n')
assert results and all(r['passed'] for r in results), 'constructed lockfile guard witnesses failed'
PYGUARD
}
# A fixture tag is a real one-line manifest bump, without dependency changes.
with_workspace_version() {
    local base=$1 version=$2 manifest
    manifest=$(git show "$base:codex-rs/Cargo.toml" | python3 -c '
import re,sys,tomllib
text=sys.stdin.read()
section=re.search(r"(?ms)^\[workspace\.package\]\n(.*?)(?=^\[|\Z)",text)
assert section
body,count=re.subn(r"(?m)^version = \"[^\"]+\"$", "version = \""+sys.argv[1]+"\"", section[1],count=1)
assert count==1
text=text[:section.start(1)]+body+text[section.end(1):]
assert tomllib.loads(text)["workspace"]["package"]["version"]==sys.argv[1]
print(text,end="")' "$version")
    with_file "$base" "$base" codex-rs/Cargo.toml "$manifest"
}
if [[ ${BASH_SOURCE[0]} == "$0" ]]; then
    lockfile_guard_cases "$(git rev-parse --show-toplevel)" "$4/lockfile-guards"
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
    MONITOR_REBASE_RESULTS="$failed_root" assert_receipt tests-failed "$head"
    [[ ! -e $failed_root/$head/run.log ]]
    grep -q '/nonexistent-monitor-rebase-negative-control' "$failed_root/$head/list.stderr"
    python3 - "$failed_root/$head" "$EVIDENCE/listing-failure-bytes.json" <<'PY'
import json,pathlib,sys
root=pathlib.Path(sys.argv[1])
files={str(p.relative_to(root)):p.read_bytes().hex() for p in root.rglob('*') if p.is_file()}
assert {'head','receipt-state.json','Cargo.lock.before','list.json','list.stderr','lock-list.txt','lock-final.txt'} <= files.keys()
assert 'tested-head' not in files
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
    assert_receipt clean "$head"
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
assert len(actual)==len(set(actual))==len(selected)==72 and set(actual)==selected
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
