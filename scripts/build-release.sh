#!/usr/bin/env bash
set -euo pipefail

usage() {
  echo "usage: $0 {macos|linux} <target>" >&2
  exit 2
}

[[ $# -eq 2 ]] || usage
mode=$1
target=$2

root=$(git rev-parse --show-toplevel)
cd "$root"
source_commit=$(git rev-parse HEAD)
upstream_tag=$(git describe --tags --match 'rust-v*' --abbrev=0 "$source_commit")

sha256_file() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | awk '{print $1}'
  else
    shasum -a 256 "$1" | awk '{print $1}'
  fi
}

setup_rusty_v8() {
  local build_root=$1
  local artifact_target=$2
  local artifact_dir=$3
  local version release_tag base_url profile archive_name binding_name checksums_name
  local archive_path binding_path checksums_path trusted_checksums expected actual

  version=$(python3 "$build_root/.github/scripts/rusty_v8_bazel.py" resolved-v8-crate-version)
  release_tag="rusty-v8-v${version}"
  base_url="https://github.com/openai/codex/releases/download/${release_tag}"
  profile=ptrcomp_sandbox_release
  archive_name="librusty_v8_${profile}_${artifact_target}.a.gz"
  binding_name="src_binding_${profile}_${artifact_target}.rs"
  checksums_name="rusty_v8_${profile}_${artifact_target}.sha256"
  archive_path="$artifact_dir/$archive_name"
  binding_path="$artifact_dir/$binding_name"
  checksums_path="$artifact_dir/$checksums_name"
  trusted_checksums="$build_root/third_party/v8/rusty_v8_${version//./_}_release_manifests.sha256"

  mkdir -p "$artifact_dir"
  curl -fsSL "$base_url/$checksums_name" -o "$checksums_path"
  expected=$(awk -v name="$checksums_name" '$2 == name { print $1 }' "$trusted_checksums")
  [[ $expected =~ ^[0-9a-f]{64}$ ]] || {
    echo "Missing unique trusted checksum for $checksums_name" >&2
    return 1
  }
  actual=$(sha256_file "$checksums_path")
  [[ $actual == "$expected" ]] || {
    echo "Checksum mismatch for $checksums_name: expected $expected, got $actual" >&2
    return 1
  }
  curl -fsSL "$base_url/$archive_name" -o "$archive_path"
  curl -fsSL "$base_url/$binding_name" -o "$binding_path"
  [[ $(wc -l < "$checksums_path") -eq 2 ]] || {
    echo "Expected exactly two checksums in $checksums_path" >&2
    return 1
  }
  if command -v sha256sum >/dev/null 2>&1; then
    (cd "$artifact_dir" && tr -d '\r' < "$checksums_path" | sha256sum --check -)
  else
    (cd "$artifact_dir" && tr -d '\r' < "$checksums_path" | shasum -a 256 --check -)
  fi
  export RUSTY_V8_ARCHIVE=$archive_path
  export RUSTY_V8_SRC_BINDING_PATH=$binding_path
}

write_manifest() {
  local destination=$1
  local binary_sha256=$2
  local lockfile_sha256=$3
  local manifest_tmp="$destination.tmp"

  cat > "$manifest_tmp" <<EOF
source_commit=$source_commit
upstream_tag=$upstream_tag
binary_sha256=$binary_sha256
lockfile_sha256=$lockfile_sha256
built_at=$(date -u +%Y-%m-%dT%H:%M:%SZ)
EOF
  mv "$manifest_tmp" "$destination"
}

require_source_bound_checkout() {
  local untracked
  git diff --quiet HEAD -- . || {
    echo "Tracked files must match HEAD before a source-bound native build" >&2
    git status --short --untracked-files=no >&2
    return 1
  }
  untracked=$(git ls-files --others --exclude-standard -- .)
  [[ -z $untracked ]] || {
    echo "Untracked build inputs must be removed before a source-bound native build:" >&2
    printf '%s\n' "$untracked" >&2
    return 1
  }
}

build_macos() (
  [[ $target == *-apple-darwin ]] || {
    echo "macos mode requires an Apple Darwin target" >&2
    exit 2
  }
  require_source_bound_checkout
  [[ -z $(git status --porcelain -- codex-rs/Cargo.lock) ]] || {
    echo "codex-rs/Cargo.lock must be clean before building" >&2
    exit 1
  }

  local before_file lock_result lockfile_sha256 binary binary_sha256 destination rc
  local native_target_dir="$root/codex-rs/target-task9-release"
  before_file=$(mktemp "${TMPDIR:-/tmp}/codex-lock-before.XXXXXX")
  git show HEAD:codex-rs/Cargo.lock > "$before_file"
  # shellcheck disable=SC2329 # Invoked by the EXIT trap below.
  cleanup_lock() {
    rc=$?
    git checkout -- codex-rs/Cargo.lock || rc=1
    rm -f "$before_file"
    exit "$rc"
  }
  trap cleanup_lock EXIT

  if ! command -v rustup >/dev/null 2>&1 && [[ -n ${HOME:-} && -f $HOME/.cargo/env ]]; then
    # rustup's installer records its non-login-shell PATH setup here.
    # shellcheck disable=SC1091 # The user-specific rustup environment is discovered at runtime.
    source "$HOME/.cargo/env"
  fi
  command -v rustup >/dev/null 2>&1 || {
    echo "Native build prerequisite missing: install rustup from https://rustup.rs so \$HOME/.cargo/env exists" >&2
    exit 1
  }
  (cd codex-rs && rustup show active-toolchain)
  [[ $(cd codex-rs && rustc -vV | awk '/^host:/ { print $2 }') == "$target" ]] || {
    echo "macos target $target must match the native rustc host" >&2
    exit 1
  }
  setup_rusty_v8 "$root" "$target" "${RUNNER_TEMP:-${TMPDIR:-/tmp}}/rusty_v8"
  export STABLE_GIT_COMMIT=$source_commit
  export CARGO_PROFILE_RELEASE_SPLIT_DEBUGINFO=packed
  export CARGO_NET_GIT_FETCH_WITH_CLI=true
  export CARGO_TARGET_DIR=$native_target_dir
  (cd codex-rs && cargo build --release --bin codex)

  lock_result=$(python3 scripts/lockfile-delta.py "$before_file" codex-rs/Cargo.lock)
  printf '%s\n' "$lock_result"
  [[ $lock_result =~ ^clean\ sha256=([0-9a-f]{64})$ ]] || {
    echo "Lockfile guard did not return the expected clean receipt" >&2
    exit 1
  }
  lockfile_sha256=${BASH_REMATCH[1]}
  git checkout -- codex-rs/Cargo.lock
  binary="$native_target_dir/release/codex"
  binary_sha256=$(sha256_file "$binary")
  "$binary" --version
  "$binary" features list | grep -E '^monitor[[:space:]]+stable[[:space:]]+true$'
  destination="$root/dist/$target"
  mkdir -p "$destination"
  cp "$binary" "$destination/codex"
  write_manifest "$destination/MANIFEST" "$binary_sha256" "$lockfile_sha256"
  printf 'binary_sha256=%s\n' "$binary_sha256"
  printf 'artifact=%s\n' "$destination/codex"
)

build_linux() (
  local platform_arch zig_arch zig_sha export_dir dist_dir rc
  case $target in
    aarch64-unknown-linux-musl)
      platform_arch=arm64
      zig_arch=aarch64
      zig_sha=ab64e3ea277f6fc5f3d723dcd95d9ce1ab282c8ed0f431b4de880d30df891e4f
      ;;
    x86_64-unknown-linux-musl)
      platform_arch=amd64
      zig_arch=x86_64
      zig_sha=473ec26806133cf4d1918caf1a410f8403a13d979726a9045b421b685031a982
      ;;
    *)
      echo "linux mode requires a supported musl target" >&2
      exit 2
      ;;
  esac

  export_dir=$(mktemp -d "${TMPDIR:-/tmp}/codex-release-export.XXXXXX")
  # shellcheck disable=SC2329 # Invoked by the EXIT trap below.
  cleanup_export() {
    rc=$?
    rm -rf "$export_dir"
    exit "$rc"
  }
  trap cleanup_export EXIT
  git archive HEAD | tar -x -C "$export_dir"
  printf '%s\n' "$source_commit" > "$export_dir/SOURCE_COMMIT"
  dist_dir="$root/dist"
  mkdir -p "$dist_dir"

  docker run --rm --interactive --platform "linux/$platform_arch" \
    -e TARGET="$target" \
    -e UPSTREAM_TAG="$upstream_tag" \
    -e ZIG_ARCH="$zig_arch" \
    -e ZIG_SHA256="$zig_sha" \
    -v "$export_dir:/src:ro" \
    -v "$dist_dir:/out" \
    ubuntu:24.04@sha256:224a1869083a311ef3f13648a154ba79832fbef6364d31493642ca03082da254 \
    bash -s <<'CONTAINER'
set -euo pipefail
export DEBIAN_FRONTEND=noninteractive
apt-get update
apt-get install -y binutils pkg-config libcap-dev curl ca-certificates git xz-utils python3 make
cat > /usr/local/bin/sudo <<'EOF'
#!/usr/bin/env bash
exec "$@"
EOF
chmod +x /usr/local/bin/sudo
cp -r /src /build

export GITHUB_ENV=/tmp/github_env
export RUNNER_TEMP=/tmp
: > "$GITHUB_ENV"
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
  | sh -s -- -y --profile minimal --default-toolchain 1.95.0 --component clippy,rustfmt,rust-src --target "$TARGET"
export PATH=/root/.cargo/bin:$PATH

zig_version=0.14.0
zig_archive="zig-linux-${ZIG_ARCH}-${zig_version}.tar.xz"
curl -fsSL "https://ziglang.org/download/${zig_version}/${zig_archive}" -o "/tmp/${zig_archive}"
echo "${ZIG_SHA256}  /tmp/${zig_archive}" | sha256sum --check -
tar -xJf "/tmp/${zig_archive}" -C /opt
export PATH="/opt/zig-linux-${ZIG_ARCH}-${zig_version}:$PATH"
zig version | grep -Fx "$zig_version"

export TARGET
bash /build/.github/scripts/install-musl-build-tools.sh
while IFS='=' read -r name value; do
  [[ $name =~ ^[A-Za-z_][A-Za-z0-9_]*$ ]] || {
    echo "Invalid environment name from musl helper: $name" >&2
    exit 1
  }
  export "$name=$value"
done < "$GITHUB_ENV"
export AWS_LC_SYS_NO_JITTER_ENTROPY=1
target_no_jitter="AWS_LC_SYS_NO_JITTER_ENTROPY_${TARGET//-/_}"
export "${target_no_jitter}=1"

sha256_file() { sha256sum "$1" | awk '{print $1}'; }
setup_rusty_v8() {
  local version release_tag base_url profile archive_name binding_name checksums_name
  local artifact_dir archive_path binding_path checksums_path trusted_checksums expected actual
  version=$(python3 /build/.github/scripts/rusty_v8_bazel.py resolved-v8-crate-version)
  release_tag="rusty-v8-v${version}"
  base_url="https://github.com/openai/codex/releases/download/${release_tag}"
  profile=ptrcomp_sandbox_release
  archive_name="librusty_v8_${profile}_${TARGET}.a.gz"
  binding_name="src_binding_${profile}_${TARGET}.rs"
  checksums_name="rusty_v8_${profile}_${TARGET}.sha256"
  artifact_dir=/tmp/rusty_v8
  archive_path="$artifact_dir/$archive_name"
  binding_path="$artifact_dir/$binding_name"
  checksums_path="$artifact_dir/$checksums_name"
  trusted_checksums="/build/third_party/v8/rusty_v8_${version//./_}_release_manifests.sha256"
  mkdir -p "$artifact_dir"
  curl -fsSL "$base_url/$checksums_name" -o "$checksums_path"
  expected=$(awk -v name="$checksums_name" '$2 == name { print $1 }' "$trusted_checksums")
  [[ $expected =~ ^[0-9a-f]{64}$ ]]
  actual=$(sha256_file "$checksums_path")
  [[ $actual == "$expected" ]]
  curl -fsSL "$base_url/$archive_name" -o "$archive_path"
  curl -fsSL "$base_url/$binding_name" -o "$binding_path"
  [[ $(wc -l < "$checksums_path") -eq 2 ]]
  (cd "$artifact_dir" && tr -d '\r' < "$checksums_path" | sha256sum --check -)
  export RUSTY_V8_ARCHIVE=$archive_path
  export RUSTY_V8_SRC_BINDING_PATH=$binding_path
}
setup_rusty_v8

export STABLE_GIT_COMMIT
STABLE_GIT_COMMIT=$(cat /build/SOURCE_COMMIT)
export CARGO_TARGET_DIR=/build/target
export CARGO_NET_GIT_FETCH_WITH_CLI=true
(cd /build/codex-rs && cargo build --target "$TARGET" --release --bin codex)
lock_result=$(python3 /build/scripts/lockfile-delta.py /src/codex-rs/Cargo.lock /build/codex-rs/Cargo.lock)
printf '%s\n' "$lock_result"
[[ $lock_result =~ ^clean\ sha256=([0-9a-f]{64})$ ]]
lockfile_sha256=${BASH_REMATCH[1]}
binary="/build/target/${TARGET}/release/codex"
binary_sha256=$(sha256_file "$binary")
"$binary" --version
"$binary" features list | grep -E '^monitor[[:space:]]+stable[[:space:]]+true$'
destination="/out/${TARGET}"
mkdir -p "$destination"
cp "$binary" "$destination/codex"
cat > "$destination/MANIFEST" <<EOF
source_commit=$STABLE_GIT_COMMIT
upstream_tag=$UPSTREAM_TAG
binary_sha256=$binary_sha256
lockfile_sha256=$lockfile_sha256
built_at=$(date -u +%Y-%m-%dT%H:%M:%SZ)
EOF
printf 'binary_sha256=%s\n' "$binary_sha256"
printf 'artifact=%s\n' "$destination/codex"
CONTAINER
)

case $mode in
  macos) build_macos ;;
  linux) build_linux ;;
  *) usage ;;
esac
