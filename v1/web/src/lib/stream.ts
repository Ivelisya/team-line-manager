// 实时流：EventSource /api/v1/stream，按事件类型让对应查询失效。
// - 事件 300ms 合并后一次性失效（同一类型/同一 id 只算一次）。
// - 浏览器自动重连只覆盖“连接断开”；服务端回非 200 时 EventSource 直接进入 CLOSED，所以这里自己指数退避重开（1s → 30s 封顶）。
// - 每次“出错之后重新 open”都全量失效一次，补上断线期间错过的事件。
// - 服务端滞后广播 {type:"resync"} 时同样全量失效。
// - 流正常时不再 30 秒轮询；只有流不是 open 才由各查询自己轮询兜底（见 usePoll）。
import { useEffect } from 'react';
import { useQueryClient } from '@tanstack/react-query';
import { invalidateFor, type StreamEvent } from './queries';
import { setStreamState as setStatus, useStreamState } from './streamState';

const MERGE_MS = 300;
const BACKOFF_MAX = 30_000;

export function useStream(enabled: boolean) {
  const qc = useQueryClient();
  const state = useStreamState();
  useEffect(() => {
    if (!enabled) return;
    let es: EventSource | null = null;
    let stopped = false;
    let retry = 0;
    let dirty = false; // 出过错：下次 open 要补数据
    let retryTimer: ReturnType<typeof setTimeout> | undefined;
    let mergeTimer: ReturnType<typeof setTimeout> | undefined;
    const pending = new Map<string, StreamEvent>();
    let all = false;

    const flush = () => {
      mergeTimer = undefined;
      if (all) qc.invalidateQueries();
      else pending.forEach((ev) => invalidateFor(qc, ev));
      pending.clear(); all = false;
    };
    const queue = (ev: StreamEvent) => {
      if (ev.type === 'resync') all = true;
      else pending.set(`${ev.type}:${ev.id ?? ''}`, ev);
      mergeTimer ??= setTimeout(flush, MERGE_MS);
    };

    const open = () => {
      if (stopped) return;
      const s = new EventSource('/api/v1/stream');
      es = s;
      s.onopen = () => {
        retry = 0; setStatus('open');
        if (dirty) { dirty = false; qc.invalidateQueries(); }
      };
      s.onerror = () => {
        setStatus('error'); dirty = true;
        // CLOSED(2)：浏览器放弃了（非 200 / 非 event-stream），要自己重开；CONNECTING 则是浏览器在自动重连
        if (s.readyState === 2) {
          s.close();
          retryTimer = setTimeout(open, Math.min(BACKOFF_MAX, 1000 * 2 ** retry++));
        }
      };
      s.onmessage = (m) => {
        setStatus('open');
        try { queue(JSON.parse(m.data) as StreamEvent); } catch { /* 忽略非 JSON 心跳 */ }
      };
    };
    setStatus('connecting');
    open();
    return () => {
      stopped = true; es?.close(); clearTimeout(retryTimer); clearTimeout(mergeTimer);
      setStatus('connecting');
    };
  }, [enabled, qc]);
  return state;
}
