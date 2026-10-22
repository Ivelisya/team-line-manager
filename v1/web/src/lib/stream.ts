// 实时流：WebSocket /api/v1/stream（JSON 文本帧）。
// - metrics：实时指标增量，直接写进查询缓存（live.ts），不重取。
// - 其余是资源变更通知：300ms 合并后按事件类型让对应查询失效（同一类型/同一 id 只算一次）；resync（服务端广播滞后）则全部失效。
//   整表（机器、线路机、总览、作业列表）的失效再合并成最多每 2 s 一次，且不打断在途的重取：批量上架时不至于每秒整表重取、请求被反复取消；
//   事件到达之后才开始、并已成功取回的整表不再重取（它已经包含这次变更）——自己保存时写操作当场重取了一次，事件随后又会引起一次，这样只取一次。
// - 断开后指数退避重连（1s → 30s 封顶，±20% 抖动，中控重启后各页面不同时涌进来），期间不轮询（顶栏显示已断开）；
//   等待重连时网络恢复（online）或页面回到前台，立刻重连，不再等退避到点。首次连上让机器类查询失效一次：增量只带变了的字段，
//   首屏快照若早于订阅，其间变过一次的状态（健康、维护、进行中的作业）不会再推，只能靠这次重取补上；
//   断线重连后让全部查询失效：没连着时错过的事件和增量靠这次重取补上。
// - 页面在后台：失效只标记过期、不重取，metrics 也不写缓存只标记过期（回到前台由 refetchOnWindowFocus 补上）。
import { useEffect } from 'react';
import { useQueryClient, type Query } from '@tanstack/react-query';
import { eventKeys, type StreamEvent } from './queries';
import { applyMetrics, LIVE_KEYS, type MetricsMsg } from './live';
import { setStreamState as setStatus, useStreamState } from './streamState';

const MERGE_MS = 300;
const LIST_MS = 2000;
const LISTS = new Set(['machines', 'relays', 'overview', 'jobs', 'audit']);
const BACKOFF_MAX = 30_000;
// 重连、resync 的全部失效不带上这几类：恢复 / 退役预检会真的 SSH 探测机器（改选项或重开对话框才重查）；
// 集群速率历史只用来垫在实时点前面（live.ts 的 seedFleet 只补现有最早点之前的），重取补不上断线的空档，白取
const NO_RESYNC = new Set(['restore-preview', 'retire-preview', 'overview-series']);
const resyncable = (q: Query) => !NO_RESYNC.has(q.queryKey[0] as string);

export function useStream(enabled: boolean) {
  const qc = useQueryClient();
  const state = useStreamState();
  useEffect(() => {
    if (!enabled) return;
    let ws: WebSocket | null = null;
    let stopped = false;
    let retry = 0;
    let connected = false;
    let retryTimer: ReturnType<typeof setTimeout> | undefined;
    let waiting = false; // 断开后在等退避到点
    let mergeTimer: ReturnType<typeof setTimeout> | undefined;
    let listTimer: ReturnType<typeof setTimeout> | undefined;
    let lastList = 0;
    const pending = new Map<string, StreamEvent>();
    const listKeys = new Map<string, { k: unknown[]; at: number }>();
    let all = false;
    let eventAt = 0; // 最近一条资源事件到达的时刻
    // 各整表查询最近一次开始取的时刻：开始于事件之后、且已成功取回的，内容里已有这次变更
    const fetchedAt = new Map<string, number>();
    const unsub = qc.getQueryCache().subscribe((e) => {
      if (e.type === 'updated' && e.action.type === 'fetch' && LISTS.has(e.query.queryKey[0] as string)) fetchedAt.set(e.query.queryHash, Date.now());
    });
    const fresh = (q: Query, at: number) => { const t = fetchedAt.get(q.queryHash); return t != null && t > at && q.state.dataUpdatedAt >= t; };
    // 往前翻过页的分页列表（操作记录）：失效会把已加载的每一页依次重取，这里只标过期，回到页面 / 切回窗口时再取
    const deep = (q: Query) => ((q.state.data as { pages?: unknown[] } | undefined)?.pages?.length ?? 0) > 1;

    const inv = (queryKey?: unknown[], cancelRefetch = true) => qc.invalidateQueries({ queryKey, predicate: resyncable, refetchType: document.hidden ? 'none' : 'active' }, { cancelRefetch });
    const flush = () => {
      mergeTimer = undefined;
      if (all) inv();
      else {
        // 一批事件里各键只失效一次；['machine'] 已覆盖 ['machine', id]
        const keys = new Map<string, unknown[]>();
        pending.forEach((ev) => eventKeys(ev).forEach((k) => keys.set(JSON.stringify(k), k)));
        keys.forEach((k, s) => {
          if (k.length > 1 && keys.has(JSON.stringify([k[0]]))) return;
          if (LISTS.has(k[0] as string)) listKeys.set(s, { k, at: eventAt }); else inv(k);
        });
        if (listKeys.size) listTimer ??= setTimeout(flushLists, Math.max(0, lastList + LIST_MS - Date.now()));
      }
      pending.clear(); all = false;
    };
    // 正在取的整表不打断，也先不失效（取完会清掉失效标记）：留到它回来后的下一轮，它可能是变更之前的快照
    const flushLists = () => {
      listTimer = undefined; lastList = Date.now();
      listKeys.forEach(({ k, at }, s) => {
        if (qc.isFetching({ queryKey: k })) return;
        void qc.invalidateQueries({ queryKey: k, predicate: (q) => resyncable(q) && !fresh(q, at) && !deep(q), refetchType: document.hidden ? 'none' : 'active' }, { cancelRefetch: false });
        void qc.invalidateQueries({ queryKey: k, predicate: (q) => resyncable(q) && deep(q), refetchType: 'none' });
        listKeys.delete(s);
      });
      if (listKeys.size) listTimer = setTimeout(flushLists, LIST_MS);
    };
    const queue = (ev: StreamEvent) => {
      if (ev.type === 'resync') all = true;
      else pending.set(`${ev.type}:${ev.id ?? ''}`, ev);
      eventAt = Date.now();
      mergeTimer ??= setTimeout(flush, MERGE_MS);
    };
    const onMetrics = (msg: MetricsMsg) => {
      if (document.hidden) LIVE_KEYS.forEach((k) => qc.invalidateQueries({ queryKey: k, refetchType: 'none' }));
      else applyMetrics(qc, msg);
      // 进行中的作业变了（开始、等待 / 运行、结束）：这台的详情重取一次——失败的作业、生命周期原因等不在增量里（服务端不再逐步发事件）
      for (const [id, d] of Object.entries(msg.machines)) if (d.active_job !== undefined) void inv(['machine', id]);
    };

    const open = () => {
      if (stopped) return;
      waiting = false;
      const s = new WebSocket(`${location.protocol === 'https:' ? 'wss' : 'ws'}://${location.host}/api/v1/stream`);
      ws = s;
      // 正在取的不打断重发（首次连上时首屏请求多半还在路上）
      s.onopen = () => {
        retry = 0; setStatus('open');
        if (connected) inv(undefined, false); else LIVE_KEYS.forEach((k) => inv(k, false));
        connected = true;
      };
      s.onmessage = (m) => {
        const ev = JSON.parse(m.data as string) as StreamEvent | MetricsMsg;
        if (ev.type === 'metrics') onMetrics(ev as MetricsMsg);
        else queue(ev as StreamEvent);
      };
      // 出错之后一定会 close；会话失效（4401）也照常重连，握手会 401，登录态由各查询处理
      s.onclose = () => {
        if (stopped) return;
        setStatus('error');
        waiting = true;
        retryTimer = setTimeout(open, Math.min(BACKOFF_MAX, 1000 * 2 ** retry++ * (0.8 + Math.random() * 0.4)));
      };
    };
    const reconnectNow = () => {
      if (!waiting || document.hidden) return;
      clearTimeout(retryTimer); retry = 0; open();
    };
    window.addEventListener('online', reconnectNow);
    document.addEventListener('visibilitychange', reconnectNow);
    setStatus('connecting');
    // 推迟到下一轮再连：StrictMode 开发时挂载→卸载→再挂载，避免先建一个马上关掉的连接
    retryTimer = setTimeout(open, 0);
    return () => {
      stopped = true; ws?.close(); unsub(); clearTimeout(retryTimer); clearTimeout(mergeTimer); clearTimeout(listTimer);
      window.removeEventListener('online', reconnectNow); document.removeEventListener('visibilitychange', reconnectNow);
      setStatus('connecting');
    };
  }, [enabled, qc]);
  return state;
}
