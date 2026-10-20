#!/bin/bash
# 把整个 Rust 工作区（proto + server + agent + migrations + deploy）同步到 fleet-dev:/root/src，在远端编译/测试。
# 用法：lab/sync-server.sh [远端命令...]   例：lab/sync-server.sh 'cargo test -p fleet-server'
set -euo pipefail
ROOT=$(cd "$(dirname "$0")/.." && pwd)
SSH="ssh -F $HOME/.ssh/fleetlab_config fleet-dev"
tar -C "$ROOT" -czf - --exclude=target Cargo.toml migrations deploy crates \
  | $SSH 'mkdir -p /root/src && cd /root/src && rm -rf crates migrations deploy && tar --no-same-owner -xzf -'
if [ $# -gt 0 ]; then
  $SSH "cd /root/src && export PATH=\$HOME/.cargo/bin:\$PATH && $*"
fi
