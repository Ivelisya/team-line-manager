#!/bin/bash
# CI 检查：面板的生成类型 v1/web/src/lib/types.gen.ts 跟得上 fleet-server 的 Rust 响应类型（ts-rs，见 crates/fleet-server/tests/types_gen.rs）。
# 重新生成一遍，和仓库里的有差别（或文件还没提交）就失败：改了响应结构体却没重新生成、或手改了生成的文件。
# 用法：v1/ci/check-types-gen.sh（在哪个目录下跑都一样）。会覆盖工作区里的 types.gen.ts（就是重新生成），不改别的文件。
# 本机修好：cd v1 && FLEET_TYPES_GEN=write cargo test -p fleet-server --test types_gen，再把 web/src/lib/types.gen.ts 一起提交
set -euo pipefail
cd "$(dirname "$0")/.."
F=web/src/lib/types.gen.ts
FLEET_TYPES_GEN=write cargo test -p fleet-server --locked --test types_gen --quiet
# 单独赋值：git 自己失败（不在仓库里、工作树指向别处）时 set -e 让脚本失败，而不是当成「没有差别」
st=$(git status --porcelain -- "$F")
if [ -n "$st" ]; then
  git --no-pager diff --stat -- "$F" || true
  git --no-pager diff -- "$F" | head -100 || true
  echo "$F 和 Rust 类型对不上（或没提交）：cd v1 && FLEET_TYPES_GEN=write cargo test -p fleet-server --test types_gen，再把它一起提交" >&2
  exit 1
fi
echo "生成类型检查通过：$F 与 Rust 类型一致"
