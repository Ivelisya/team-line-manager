// 模块级的小仓库：一个值 + 订阅者，组件用 use() 订阅（useSyncExternalStore）。
// 命令面板开关、实时流连接状态、菜单触发的对话框目标都是“几处都能改、几处要跟着变”的单个值，共用这一个实现。
import { useSyncExternalStore } from 'react';

export function createStore<T>(init: T) {
  let value = init;
  const subs = new Set<() => void>();
  const subscribe = (f: () => void) => { subs.add(f); return () => { subs.delete(f); }; };
  return {
    set: (v: T) => { if (v !== value) { value = v; subs.forEach((f) => f()); } },
    use: () => useSyncExternalStore(subscribe, () => value, () => init),
  };
}
