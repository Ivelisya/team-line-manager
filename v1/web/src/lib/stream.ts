// 实时流：WebSocket /api/v1/stream（JSON 文本帧）。
// - metrics：实时指标增量，直接写进查询缓存（live.ts），不重取。
// - 其余是资源变更通知：300ms 合并后按事件类型让对应查询失效（同一类型/同一 id 只算一次）；resync（服务端广播滞后）则全部失效。
// - 断开后指数退避重连（1s → 30s 封顶）。每次连上都让机器类查询失效一次：没连着时错过的增量靠这次重取补上。
// - 页面在后台：失效只标记过期、不重取，metrics 也不写缓存只标记过期（回到前台由 refetchOnWindowFocus 补上）。
// - 流正常时不轮询；只有流不是 open 才由各查询自己轮询兜底（见 usePoll）。
import { useEffect } from 'react';
import { useQueryClient } from '@tanstack/react-query';
import { eventKeys, type StreamEvent } from './queries';
import { applyMetrics, type MetricsMsg } from './live';
import { setStreamState as setStatus, useStreamState } from './streamState';

const MERGE_MS = 300;
const BACKOFF_MAX = 30_000;
/** metrics 写的这几类查询 */
const LIVE_KEYS = [['machines'], ['machine'], ['relays'], ['overview']];

export function useStream(enabled: boolean) {
  const qc = useQueryClient();
  const state = useStreamState();
  useEffect(() => {
    if (!enabled) return;
    let ws: WebSocket | null = null;
    let stopped = false;
    let retry = 0;
    let retryTimer: ReturnType<typeof setTimeout> | undefined;
    let mergeTimer: ReturnType<typeof setTimeout> | undefined;
    const pending = new Map<string, StreamEvent>();
    let all = false;

    const inv = (queryKey?: unknown[], cancelRefetch = true) => qc.invalidateQueries({ queryKey, refetchType: document.hidden ? 'none' : 'active' }, { cancelRefetch });
    const flush = () => {
      mergeTimer = undefined;
      if (all) inv();
      else {
        // 一批事件里各键只失效一次；['machine'] 已覆盖 ['machine', id]
        const keys = new Map<string, unknown[]>();
        pending.forEach((ev) => eventKeys(ev).forEach((k) => keys.set(JSON.stringify(k), k)));
        keys.forEach((k) => { if (k.length === 1 || !keys.has(JSON.stringify([k[0]]))) inv(k); });
      }
      pending.clear(); all = false;
    };
    const queue = (ev: StreamEvent) => {
      if (ev.type === 'resync') all = true;
      else pending.set(`${ev.type}:${ev.id ?? ''}`, ev);
      mergeTimer ??= setTimeout(flush, MERGE_MS);
    };
    const onMetrics = (msg: MetricsMsg) => {
      if (document.hidden) LIVE_KEYS.forEach((k) => qc.invalidateQueries({ queryKey: k, refetchType: 'none' }));
      else applyMetrics(qc, msg);
    };

    const open = () => {
      if (stopped) return;
      const s = new WebSocket(`${location.protocol === 'https:' ? 'wss' : 'ws'}://${location.host}/api/v1/stream`);
      ws = s;
      // 正在取的不打断重发（首次连上时首屏请求多半还在路上）
      s.onopen = () => { retry = 0; setStatus('open'); LIVE_KEYS.forEach((k) => inv(k, false)); };
      s.onmessage = (m) => {
        const ev = JSON.parse(m.data as string) as StreamEvent | MetricsMsg;
        if (ev.type === 'metrics') onMetrics(ev as MetricsMsg);
        else queue(ev as StreamEvent);
      };
      // 出错之后一定会 close；会话失效（4401）也照常重连，握手会 401，登录态由各查询处理
      s.onclose = () => {
        if (stopped) return;
        setStatus('error');
        retryTimer = setTimeout(open, Math.min(BACKOFF_MAX, 1000 * 2 ** retry++));
      };
    };
    setStatus('connecting');
    // 推迟到下一轮再连：StrictMode 开发时挂载→卸载→再挂载，避免先建一个马上关掉的连接
    retryTimer = setTimeout(open, 0);
    return () => {
      stopped = true; ws?.close(); clearTimeout(retryTimer); clearTimeout(mergeTimer);
      setStatus('connecting');
    };
  }, [enabled, qc]);
  return state;
}
