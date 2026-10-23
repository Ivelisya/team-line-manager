#!/bin/bash
# 把整个 Rust 工作区（proto + server + agent + migrations + deploy）同步到编译主机，在远端编译/测试。
# 编译主机默认是 fleet-dev（/root/src，rustup）；FLEET_BUILD=docker 时是 docker 编译主机（FLEET_BUILD_HOST:FLEET_BUILD_DIR，
# 命令在 musl 编译容器里跑），配置见 lab/build-lib.sh 与 docs/OPS.md §9。
# 用法：lab/sync-server.sh [远端命令...]   例：lab/sync-server.sh 'cargo test -p fleet-server'
# 集成测试可以直接并发跑（不用 --test-threads=1）：每个测试在 FLEET_TEST_DB（默认 fleet_test 库）里建自己的 schema，
# 结束时删掉，见 crates/fleet-server/tests/it/common/mod.rs。集成测试大多在一个目标 it 里，只跑相关的一套：
# lab/sync-server.sh 'cargo test -p fleet-server --test it lifecycle::'；改 PATH 放假 ssh 的 replace、routers、upgrade 各自一个目标（--test replace）
# docker 方式跑集成测试要连库：FLEET_BUILD_DOCKER_ARGS='--network <网络> -e FLEET_TEST_DB=postgres://…'
set -euo pipefail
. "$(cd "$(dirname "$0")" && pwd)/build-lib.sh"
fb_sync
if [ $# -gt 0 ]; then
  fb_run "$*"
fi
