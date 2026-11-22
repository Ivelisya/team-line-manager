import { createElement, Suspense, useRef, useState, type ComponentType, type ReactNode } from 'react';

/** 对话框/抽屉的关闭动画期间还要继续渲染内容：id 清空后返回最近一次有数据时的值，直到下次打开 */
export function useHeld<T>(id: string | null | undefined, data: T | null | undefined): T | undefined {
  const last = useRef<T | undefined>(undefined);
  if (id && data) last.current = data;
  return id ? (data ?? undefined) : last.current;
}

/** 打开次数：每次由关到开加一，关着（含关闭动画期间）不变 */
function useOpenCount(open: boolean): number {
  const [s, setS] = useState({ open, n: 0 });
  // 渲染中跟上：React 丢掉这一遍的结果、立刻按新状态重渲染，子树拿到的已是新的次数
  if (s.open !== open) setS({ open, n: open ? s.n + 1 : s.n });
  return s.n;
}

/** 对话框每次打开都重新挂载（key = 打开次数），表单状态直接从 props 初始化，打开的第一帧就是新值。
 *  不要改成打开后用 effect 重置：旧状态会先提交一帧，偶尔能看到上次的输入闪一下。关闭动画期间 key 不变，内容保持原样 */
export function resetOnOpen<P extends { open: boolean }>(Dialog: ComponentType<P>) {
  return function ResetOnOpen(p: P) {
    return createElement(Dialog, { ...p, key: useOpenCount(p.open) });
  };
}

/** 一旦为真就一直为真：懒加载的浮层第一次需要时挂载，之后保持挂载（关闭动画、再次打开都不用重来） */
export function useEver(v: boolean) {
  const r = useRef(false);
  if (v) r.current = true;
  return r.current;
}

/** 懒加载的对话框（React.lazy）：第一次打开时才挂载、才下载代码，之后一直挂着；下载期间什么也不显示，同外壳里的各个浮层 */
export function LazyMount({ when, children }: { when: boolean; children: ReactNode }) {
  return useEver(when) ? createElement(Suspense, { fallback: null }, children) : null;
}
