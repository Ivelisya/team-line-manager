#!/bin/bash
# 把整个 Rust 工作区（proto + server + agent + migrations + deploy）同步到 fleet-dev:/root/src，在远端编译/测试。
# 用法：lab/sync-server.sh [远端命令...]   例：lab/sync-server.sh 'cargo test -p fleet-server'
# 集成测试可以直接并发跑（不用 --test-threads=1）：每个测试在 FLEET_TEST_DB（默认 fleet_test 库）里建自己的 schema，
# 结束时删掉，见 crates/fleet-server/tests/common/mod.rs。只跑相关的一套：lab/sync-server.sh 'cargo test -p fleet-server --test lifecycle'
set -euo pipefail
ROOT=$(cd "$(dirname "$0")/.." && pwd)
SSH="ssh -F $HOME/.ssh/fleetlab_config fleet-dev"
tar -C "$ROOT" -czf - --exclude=target Cargo.toml Cargo.lock migrations deploy crates \
  | $SSH 'mkdir -p /root/src && cd /root/src && rm -rf crates migrations deploy && tar --no-same-owner -xzf -'
if [ $# -gt 0 ]; then
  $SSH "cd /root/src && export PATH=\$HOME/.cargo/bin:\$PATH && $*"
fi
