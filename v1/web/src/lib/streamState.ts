// 实时流连接状态的小仓库：顶栏指示据此显示。断开期间不轮询，重连后由 stream.ts 让全部查询失效补上。
import { useSyncExternalStore } from 'react';

export type StreamState = 'connecting' | 'open' | 'error';

let status: StreamState = 'connecting';
const subs = new Set<() => void>();
export const setStreamState = (s: StreamState) => { if (s !== status) { status = s; subs.forEach((f) => f()); } };
const subscribe = (f: () => void) => { subs.add(f); return () => { subs.delete(f); }; };

export const useStreamState = () => useSyncExternalStore(subscribe, () => status, () => 'connecting' as StreamState);
