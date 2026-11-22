// live.ts：metrics 增量写进缓存（没变的机器引用不变）、在途重取期间的增量在响应回来后补上、实时迹线隔太久就重新攒
import { beforeEach, describe, expect, it, vi } from 'vitest';
import { QueryClient } from '@tanstack/react-query';
import type { MachineView } from '@/lib/types';
import type * as Live from '@/lib/live';
import { machine, entry, probe, must } from './fixtures';

// live.ts 有模块级状态（全站一个 QueryClient 的订阅、迹线缓冲），每个用例重新加载一份
let live: typeof Live;
beforeEach(async () => {
  vi.resetModules();
  live = await import('@/lib/live');
});

const LIST = ['machines', 'live'];
type Msg = Parameters<typeof Live.applyMetrics>[1];
const msg = (machines: Msg['machines'], entries: Msg['entries'] = {}, overview?: Msg['overview']): Msg => ({ type: 'metrics', machines, entries, overview });
/** 主机读数元组：[ts 秒, cpu, load1, mem_used, mem_total, disk_used, disk_total, rx, tx, conns] */
const host = (ts: number, rx = 100, tx = 200) => [ts, 1, 0.1, 1, 2, 1, 2, rx, tx, 3] as never;
const deferred = <T,>() => { let resolve!: (v: T) => void; const promise = new Promise<T>((r) => { resolve = r; }); return { promise, resolve }; };

describe('applyMetrics：写增量', () => {
  it('只改有增量的机器和入口；没变的机器引用不变', () => {
    const qc = new QueryClient();
    const a = machine({ id: 'a', entries: [entry({ id: 'ea' })] });
    const b = machine({ id: 'b' });
    qc.setQueryData(LIST, [a, b]);
    qc.setQueryData(['machine', 'a'], a);
    live.applyMetrics(qc, msg({ a: { maintenance: true, health: { status: 'down', reasons: [] } } }, { ea: { tcp: [false, null, 1_700_000_000, 'refused'] } }));
    const [na0, nb] = qc.getQueryData<MachineView[]>(LIST)!;
    const na = must(na0);
    expect(na.maintenance).toBe(true);
    expect(na.health.status).toBe('down');
    expect(must(na.entries[0]).tcp).toMatchObject({ ok: false, detail: 'refused', ts: new Date(1_700_000_000_000).toISOString() });
    expect(nb).toBe(b);
    expect(qc.getQueryData<MachineView>(['machine', 'a'])!.maintenance).toBe(true);
  });
  it('一条都没改到时列表引用不变；overview 整个替换', () => {
    const qc = new QueryClient();
    const list = [machine({ id: 'a' })];
    qc.setQueryData(LIST, list);
    qc.setQueryData(['overview'], { net_rx_bps: 0 });
    live.applyMetrics(qc, msg({ zzz: { maintenance: true } }, {}, { net_rx_bps: 5, net_tx_bps: 6 } as never));
    expect(qc.getQueryData(LIST)).toBe(list);
    expect(qc.getQueryData(['overview'])).toEqual({ net_rx_bps: 5, net_tx_bps: 6 });
  });
  it('写之前已被失效的查询，写完仍是失效的（推送不带的字段还旧）', async () => {
    const qc = new QueryClient();
    qc.setQueryData(LIST, [machine({ id: 'a' })]);
    await qc.invalidateQueries({ queryKey: ['machines'], refetchType: 'none' });
    live.applyMetrics(qc, msg({ a: { maintenance: true } }));
    expect(qc.getQueryCache().find({ queryKey: LIST })!.state.isInvalidated).toBe(true);
  });
});

describe('applyMetrics：在途重取期间的增量', () => {
  it('重取回来的快照早于增量时，按顺序把取的期间收到的增量补上', async () => {
    const qc = new QueryClient();
    qc.setQueryData(LIST, [machine({ id: 'a' })]);
    const d = deferred<MachineView[]>();
    const p = qc.fetchQuery({ queryKey: LIST, queryFn: () => d.promise, staleTime: 0 });
    live.applyMetrics(qc, msg({ a: { maintenance: true } }));
    live.applyMetrics(qc, msg({ a: { due_days: 3 } }));
    // 响应是这两条增量之前的快照
    d.resolve([machine({ id: 'a', maintenance: false, subscription: { due_days: 9 } as never })]);
    await p;
    const a = must(qc.getQueryData<MachineView[]>(LIST)![0]);
    expect(a.maintenance).toBe(true);
    expect(a.subscription!.due_days).toBe(3);
  });
  it('重取失败就丢掉记录；下一次重取不会补上上一次的增量', async () => {
    const qc = new QueryClient();
    qc.setQueryData(LIST, [machine({ id: 'a' })]);
    const p1 = qc.fetchQuery({ queryKey: LIST, queryFn: () => Promise.reject(new Error('x')), staleTime: 0, retry: false });
    live.applyMetrics(qc, msg({ a: { maintenance: true } }));
    await expect(p1).rejects.toThrow('x');
    await qc.fetchQuery({ queryKey: LIST, queryFn: async () => [machine({ id: 'a', maintenance: false })], staleTime: 0 });
    expect(must(qc.getQueryData<MachineView[]>(LIST)![0]).maintenance).toBe(false);
  });
  it('没在取的时候来的增量不会在之后的重取上再补一遍', async () => {
    const qc = new QueryClient();
    qc.setQueryData(LIST, [machine({ id: 'a' })]);
    live.applyMetrics(qc, msg({ a: { maintenance: true } }));
    await qc.fetchQuery({ queryKey: LIST, queryFn: async () => [machine({ id: 'a', maintenance: false })], staleTime: 0 });
    expect(must(qc.getQueryData<MachineView[]>(LIST)![0]).maintenance).toBe(false);
  });
});

describe('实时迹线', () => {
  const tick = (qc: QueryClient, ts: number, rx = 100) => live.applyMetrics(qc, msg({ a: { host: host(ts, rx) } }));
  it('按上报时间记点；同一份上报在几份缓存里各 patch 一次也只记一次', () => {
    const qc = new QueryClient();
    qc.setQueryData(LIST, [machine({ id: 'a' })]);
    qc.setQueryData(['machine', 'a'], machine({ id: 'a' }));
    tick(qc, 1000); tick(qc, 1010); tick(qc, 1010);
    expect(live.peekLiveSeries('a').map((p) => p.t)).toEqual([1_000_000, 1_010_000]);
  });
  it('与上一个点隔了 30 秒以上：断档前的点作废，从这个点重新攒', () => {
    const qc = new QueryClient();
    qc.setQueryData(LIST, [machine({ id: 'a' })]);
    tick(qc, 1000); tick(qc, 1010); tick(qc, 1040);
    expect(live.peekLiveSeries('a')).toHaveLength(3); // 隔 30 秒整：不算断档
    tick(qc, 1071);
    expect(live.peekLiveSeries('a').map((p) => p.t)).toEqual([1_071_000]);
  });
  it('最多留 30 个点；agent 离线的机器不记', () => {
    const qc = new QueryClient();
    qc.setQueryData(LIST, [machine({ id: 'a' }), machine({ id: 'q', agent: { online: false } as never })]);
    for (let i = 0; i < 40; i++) tick(qc, 1000 + i * 10, i);
    const s = live.peekLiveSeries('a');
    expect(s).toHaveLength(live.SERIES_N);
    expect(must(s[0]).rx).toBe(10);
    live.applyMetrics(qc, msg({ q: { host: host(5000) } }));
    expect(live.peekLiveSeries('q')).toHaveLength(0);
  });
});

describe('refreshMachine：只取一台拼回列表', () => {
  it('形状没变时替换列表里这一台，取的期间来的增量补上', async () => {
    const qc = new QueryClient();
    const a = machine({ id: 'a', entries: [entry({ id: 'ea', tcp: probe(true, 10) })] });
    const b = machine({ id: 'b' });
    qc.setQueryData(LIST, [a, b]);
    const d = deferred<Response>();
    vi.stubGlobal('fetch', vi.fn(() => d.promise));
    const done = live.refreshMachine(qc, 'a');
    live.applyMetrics(qc, msg({ a: { maintenance: true } }));
    d.resolve(new Response(JSON.stringify({ data: { ...a, label: "新备注", ip_nodes: [] } }), { headers: { 'content-type': 'application/json' } }));
    expect(await done).toBe(true);
    const [na0, nb] = qc.getQueryData<MachineView[]>(LIST)!;
    const na = must(na0);
    expect(na.label).toBe('新备注');
    expect(na.maintenance).toBe(true);
    expect(nb).toBe(b);
    vi.unstubAllGlobals();
  });
  it('改了名（形状变了）拼不上，交回调用方整表失效', async () => {
    const qc = new QueryClient();
    qc.setQueryData(LIST, [machine({ id: 'a' })]);
    vi.stubGlobal('fetch', vi.fn(async () => new Response(JSON.stringify({ data: { ...machine({ id: "a", name: "改了名" }), ip_nodes: [] } }), { headers: { 'content-type': 'application/json' } })));
    expect(await live.refreshMachine(qc, 'a')).toBe(false);
    vi.unstubAllGlobals();
  });
});
