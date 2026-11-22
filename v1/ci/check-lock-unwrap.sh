#!/bin/bash
# CI 检查：fleet-server 的源码里不许对 std 的 Mutex / RwLock 直接 `.lock().unwrap()`、`.read().unwrap()`、`.write().unwrap()`
#（`.expect(…)` 同样不许）。锁中毒时这些会 panic，drop 里取锁时还会让展开中的 panic 直接中止进程；
# 一律用 src/lib.rs 的 lock() / rlock() / wlock()（中毒照常取用）。tokio 的锁是 `.lock().await`，不受影响。
# 用法：v1/ci/check-lock-unwrap.sh [目录]（在哪个目录下跑都一样；只读，不改任何文件）。有违规打印位置、退出码 1。
set -euo pipefail
SRC=${1:-$(cd "$(dirname "$0")/.." && pwd)/crates/fleet-server/src}   # 参数：换一个目录查（自测用）
RE='\.(lock|read|write)\(\)[[:space:]]*\.(unwrap|expect)\('
bad=0
# 同一行的
if grep -rnE --include='*.rs' "$RE" "$SRC"; then bad=1; fi
# 拆成两行的（`.lock()` 在行尾、`.unwrap()` 在下一行开头）
while IFS= read -r -d '' f; do
  if grep -qPz '\.(lock|read|write)\(\)\s*\n\s*\.(unwrap|expect)\(' "$f"; then echo "$f：有跨行的 .lock()/.read()/.write() 后接 .unwrap()/.expect()"; bad=1; fi
done < <(find "$SRC" -name '*.rs' -print0)
if [ $bad = 1 ]; then
  echo "fleet-server 里请用 crate::lock() / rlock() / wlock()（src/lib.rs）取 std 的锁，不要 unwrap / expect" >&2
  exit 1
fi
echo "锁检查通过：fleet-server/src 里没有 .lock()/.read()/.write() 直接 unwrap / expect"
