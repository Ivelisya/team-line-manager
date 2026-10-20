#!/bin/bash
# sync.sh — 只把 fleet/crates/probe 与 fleet/crates/health 同步到 a395 的 ~/team-ops-eval/fleet-probe-build/src，
# 并在构建目录生成 musl 交叉编译配置（与 fleet-edge 同一配方）。两个 crate 各自是独立 workspace，共用 target 目录：
#   cd ~/team-ops-eval/fleet-probe-build/src
#   CARGO_TARGET_DIR=../target cargo test --manifest-path probe/Cargo.toml
#   CARGO_TARGET_DIR=../target cargo test --manifest-path health/Cargo.toml
#   CARGO_TARGET_DIR=../target cargo build --release --target x86_64-unknown-linux-musl --manifest-path probe/Cargo.toml
set -euo pipefail
A395=${A395:-kismet@10.172.17.213}
KEY=${KEY:-$HOME/.ssh/a395_kismet}
HERE=$(cd "$(dirname "$0")" && pwd)
export COPYFILE_DISABLE=1
ssh -i "$KEY" "$A395" 'mkdir -p ~/team-ops-eval/fleet-probe-build/src/.cargo'
tar --no-xattrs -C "$HERE" --exclude=target --exclude='*.local' -czf - probe health sync.sh | ssh -i "$KEY" "$A395" 'cd ~/team-ops-eval/fleet-probe-build/src && tar -xzf -'
ssh -i "$KEY" "$A395" 'cat > ~/team-ops-eval/fleet-probe-build/src/.cargo/config.toml' <<'CFG'
# a395（Arch，无 musl-gcc）交叉编译 x86_64-unknown-linux-musl：系统 gcc 编 ring 与 bundled SQLite，关掉 LFS64 别名
[env]
CC_x86_64_unknown_linux_musl = "gcc"
CFLAGS_x86_64_unknown_linux_musl = "-std=gnu11 -U_FORTIFY_SOURCE"
LIBSQLITE3_FLAGS = "-DSQLITE_DISABLE_LFS -DSQLITE_DEFAULT_WAL_SYNCHRONOUS=2"
CFG
echo synced
