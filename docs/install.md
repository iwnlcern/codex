## Installing the monitor-enabled development artifact

The monitor-enabled binary produced before the final Task 10 squash is a development artifact.
Its identity is the exact commit in the adjacent `MANIFEST` `source_commit` field, regardless of the branch or filename used to distribute it.
Only binaries built by Task 10 from the final squashed head are release artifacts.

Build a native macOS development artifact from the repository root with:

```bash
scripts/build-release.sh macos aarch64-apple-darwin
```

Build the digest-pinned Linux musl development artifact from the repository root with:

```bash
scripts/build-release.sh linux aarch64-unknown-linux-musl
```

The Linux recipe requires at least 16 GiB of Docker daemon memory; 24 GiB is recommended.
In Docker Desktop, open Settings → Resources → Memory and allocate at least 16 GiB before running the recipe.
The host preflight records daemon memory and CPU capacity, the count and names of other running containers (`docker ps`), and their memory usage (`docker stats --no-stream`).
These container snapshots are informational and may change during the build; build when the daemon is otherwise idle.
Inside the container, Cargo jobs are `min(nproc, max(1, floor((MemTotal GiB - 2) / 7)))`, with the memory, CPU count, and selected job count printed in the transcript.
With sufficient CPUs, 16 GiB selects two jobs, 24 GiB selects three, and 32 GiB selects four.

The memory policy uses the Planner's native release-build measurements below as a sizing reference.
A 12.8 GB final link and a 7.1 GB library compile motivate the minimum and per-job allowance; these sampled native peaks are not Linux measurements or a guarantee against exhaustion on a contended daemon.

| Crate | Measured peak resident memory (MB, as reported) |
| --- | ---: |
| `codex` (final thin-LTO link) | 12,845 |
| `codex_tui` | 7,085 |
| `codex_core` | 5,956 |
| `codex_app_server` | 4,401 |
| `codex_exec` | 3,652 |
| `codex_app_server_protocol` | 2,377 |

Provenance: the v2.9.5 sprint's `results/v295-codex-fork/task9-release-peak-rss/` directory in the `agentic-dev-team-skills` repository retains `README.txt`, `rss-wrapper.sh`, `rss-release.log`, `rss-release.status`, and `build-log-tail.txt`.
The Planner measured a scratch worktree at `f0490b73`'s tree, whose Rust sources match the Task 9 heads, using `cargo build --release --bin codex -j 6`, `CARGO_PROFILE_RELEASE_SPLIT_DEBUGINFO=packed`, and upstream V8 artifacts.
Its `RUSTC_WRAPPER` sampled each rustc's resident set every 0.5 seconds across 1,282 invocations.
The governing disposition is `v295-b3/SITREP-pair-planner-20260918-154511.md` in that sprint's `.relays/v295/` tree.

The recipes write `dist/<target>/codex` and `dist/<target>/MANIFEST`.
They intentionally use Cargo without `--locked`, validate that the effective lockfile differs only by the expected workspace version lines, and record its SHA-256.

### Replace only the npm binary slot

Locate the platform package inside the active Codex npm installation.
For Apple silicon, the binary slot is:

```text
node_modules/@openai/codex-darwin-arm64/vendor/aarch64-apple-darwin/bin/codex
```

The corresponding Linux ARM64 package uses:

```text
node_modules/@openai/codex-linux-arm64/vendor/aarch64-unknown-linux-musl/bin/codex
```

Back up the existing `codex` file, then copy `dist/<target>/codex` into that exact slot and preserve its executable bit.
Replace only the `codex` binary.
Keep the npm package's `codex-code-mode-host`, `rg`, and zsh companion resources in place.

### Verify the installed bytes

Set `slot` to the replaced npm vendor path and `manifest` to the matching development or release `MANIFEST`.
Then verify the binary bytes and feature state:

```bash
expected=$(awk -F= '$1 == "binary_sha256" { print $2 }' "$manifest")
actual=$(shasum -a 256 "$slot" | awk '{print $1}')
test "$actual" = "$expected"
"$slot" --version
"$slot" features list | grep -E '^monitor[[:space:]]+stable[[:space:]]+true$'
```

On Linux, use `sha256sum "$slot"` in place of `shasum -a 256 "$slot"` when `shasum` is unavailable.
Record the `MANIFEST`, the resolved npm vendor path, and the successful command output with the install evidence.

The companion witness is a code-mode tool call made through the installed CLI while a host process listing shows `codex-code-mode-host` running from the same npm vendor tree.
This confirms that replacing the binary slot preserved and selected the npm package's companion executable.

### Measured build timings

The stock `rust-v0.154.0` Apple silicon baseline was 21 minutes 19 seconds for a cold build.
Task 9 records the recipe's native macOS cold and warm timings and its digest-pinned Linux timing here only after the reviewed recipe is committed and executed.
The native recipe uses the dedicated `codex-rs/target-task9-release` cache for both the build and binary lookup.
Before the cold timing, verify that this path is absent; preserve it after the cold build and run the same command again for the warm timing.
If the path already exists before the cold timing, stop and select a reviewed fresh-cache disposition instead of deleting or reusing it as a cold cache.

| Build | Source | Elapsed time |
| --- | --- | --- |
| Stock tag, macOS cold | `rust-v0.154.0` baseline | 21 min 19 s |
| Monitor candidate, macOS cold | Pending reviewed recipe execution | Pending |
| Monitor candidate, macOS warm | Pending reviewed recipe execution | Pending |
| Monitor candidate, Linux ARM64 musl | Pending reviewed recipe execution | Pending |

## Building the upstream project

### System requirements

| Requirement                 | Details                                                         |
| --------------------------- | --------------------------------------------------------------- |
| Operating systems           | macOS 12+, Ubuntu 20.04+/Debian 10+, or Windows 11 **via WSL2** |
| Git (optional, recommended) | 2.23+ for built-in PR helpers                                   |
| RAM                         | 4-GB minimum (8-GB recommended)                                 |
| Rust toolchain manager      | `rustup` installed from `rustup.rs`, with `$HOME/.cargo/env`     |

### DotSlash

The GitHub Release also contains a [DotSlash](https://dotslash-cli.com/) file for the Codex CLI named `codex`. Using a DotSlash file makes it possible to make a lightweight commit to source control to ensure all contributors use the same version of an executable, regardless of what platform they use for development.

### Build from source

```bash
# Clone the repository and navigate to the root of the Cargo workspace.
git clone https://github.com/openai/codex.git
cd codex/codex-rs

# Install the Rust toolchain, if necessary.
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
source "$HOME/.cargo/env"
rustup component add rustfmt
rustup component add clippy
# Install helper tools used by the workspace justfile:
cargo install --locked just
# DotSlash fetches pinned development tools such as buildifier on first use.
cargo install --locked dotslash
# Install nextest for the `just test` helper.
cargo install --locked cargo-nextest

# Build Codex.
cargo build

# Launch the TUI with a sample prompt.
cargo run --bin codex -- "explain this codebase to me"

# After making changes, use the root justfile helpers (they default to codex-rs):
just fmt
just fix -p <crate-you-touched>

# Run the relevant tests (project-specific is fastest), for example:
just test -p codex-tui
# `just test` runs the test suite via nextest:
just test
# Avoid `--all-features` for routine local runs because it increases build
# time and `target/` disk usage by compiling additional feature combinations.
```

## Tracing / verbose logging

Codex is written in Rust, so it honors the `RUST_LOG` environment variable to configure its logging behavior.

The TUI records diagnostics in bounded local stores by default. Set `log_dir` explicitly to enable a plaintext TUI log for a run:

```bash
codex -c log_dir=./.codex-log
tail -F ./.codex-log/codex-tui.log
```

The non-interactive mode (`codex exec`) defaults to `RUST_LOG=error`, but messages are printed inline, so there is no need to monitor a separate file.

See the Rust documentation on [`RUST_LOG`](https://docs.rs/env_logger/latest/env_logger/#enabling-logging) for more information on the configuration options.
