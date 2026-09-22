# Codex monitor fork release

This release carries the stable, default-enabled `monitor` tool on top of `rust-v0.154.0`.

## Artifact provenance

Each binary is accompanied by a `MANIFEST` with the following source-bound fields:

```text
source_commit=<final Task 10 squashed commit>
upstream_tag=rust-v0.154.0
binary_sha256=<sha256 of the adjacent codex binary>
lockfile_sha256=<sha256 of the effective validated Cargo.lock>
built_at=<UTC RFC 3339 timestamp>
```

A binary is an artifact of the exact `source_commit` named in its own `MANIFEST`.
Artifacts built on earlier Task 9 heads are development artifacts and are not release artifacts for this release.

## Artifacts

| Target | Binary SHA-256 | Manifest SHA-256 |
| --- | --- | --- |
| `aarch64-apple-darwin` | `<sha256>` | `<sha256>` |
| `aarch64-unknown-linux-musl` | `<sha256>` | `<sha256>` |

## Verification

Verify the downloaded binary against its adjacent manifest before installation:

```bash
expected=$(awk -F= '$1 == "binary_sha256" { print $2 }' MANIFEST)
actual=$(shasum -a 256 codex | awk '{print $1}')
test "$actual" = "$expected"
./codex --version
./codex features list | grep -E '^monitor[[:space:]]+stable[[:space:]]+true$'
```

Linux users may use `sha256sum codex` in place of `shasum -a 256 codex`.

Install by replacing only the matching npm platform package's `vendor/<target>/bin/codex` slot, as described in `docs/install.md`.
