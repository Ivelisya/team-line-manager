// 实时流连接状态的小仓库：queries 里的轮询兜底据此开关，顶栏圆点据此着色。
import { useSyncExternalStore } from 'react';

export type StreamState = 'connecting' | 'open' | 'error';

let status: StreamState = 'connecting';
const subs = new Set<() => void>();
export const setStreamState = (s: StreamState) => { if (s !== status) { status = s; subs.forEach((f) => f()); } };
const subscribe = (f: () => void) => { subs.add(f); return () => { subs.delete(f); }; };

export const useStreamState = () => useSyncExternalStore(subscribe, () => status, () => 'connecting' as StreamState);
/** 轮询间隔：实时流正常时返回 false（不轮询），断开/连接中才按 ms 兜底 */
export function usePoll(ms: number): number | false {
  return useStreamState() === 'open' ? false : ms;
}
