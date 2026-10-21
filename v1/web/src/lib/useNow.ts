import { useRef, useState, useSyncExternalStore } from 'react';

// 同一间隔共用一个模块级定时器：订阅者同一时刻一起刷新（首页每张卡一个 60 s 的走表，不必各开一个）；没人订阅时停表
interface Clock { now: number; subs: Set<() => void>; timer?: ReturnType<typeof setInterval>; read: () => number; subscribe: (f: () => void) => () => void }
const clocks = new Map<number, Clock>();
function clockOf(ms: number): Clock {
  let c = clocks.get(ms);
  if (!c) {
    const k: Clock = {
      now: Date.now(), subs: new Set(), read: () => k.now,
      subscribe: (f) => {
        k.subs.add(f);
        if (!k.timer) { k.now = Date.now(); k.timer = setInterval(() => { k.now = Date.now(); k.subs.forEach((s) => s()); }, ms); }
        return () => { k.subs.delete(f); if (!k.subs.size) { clearInterval(k.timer); k.timer = undefined; } };
      },
    };
    clocks.set(ms, (c = k));
  }
  return c;
}
const noop = () => () => {};

/** 每 ms 毫秒刷新一次的当前时间；active 为 false 时不走表（已结束的作业不需要），停在最后一次的值 */
export function useNow(active = true, ms = 1000) {
  const [mounted] = useState(Date.now);
  const c = clockOf(ms);
  const tick = useSyncExternalStore(active ? c.subscribe : noop, c.read);
  // 共用的表可能是停表前的旧值，或比本组件挂载还早一拍：不早于挂载时刻
  const last = useRef(mounted);
  if (active) last.current = Math.max(tick, mounted);
  return last.current;
}
