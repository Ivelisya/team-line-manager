// stream.ts：事件 300ms 合并、整表最多每 2 s 失效一次且不打断在途的重取、事件之后已取回的整表不再失效、首次连上 / 断线重连的失效范围、退避；
// streamGate.ts：首屏的机器类查询等连上再取（连上后不再补取）、在途的早于订阅的那次取打断重取、等不到连上时 1.5 s 后照取
import { afterSubscribed, openEarly, resetGate } from '@/lib/streamGate';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { QueryClient, QueryObserver } from '@tanstack/react-query';
import { startStream } from '@/lib/stream';
import { must } from './fixtures';

class FakeWS {
  static OPEN = 1;
  static all: FakeWS[] = [];
  readyState = 0;
  onopen: (() => void) | null = null;
  onmessage: ((m: { data: string }) => void) | null = null;
  onclose: ((e: { code: number }) => void) | null = null;
  constructor(public url: string) { FakeWS.all.push(this); }
  close() { this.readyState = 3; }
  /** 服务端接受握手 */
  accept() { this.readyState = 1; this.onopen?.(); }
  send(ev: object) { this.onmessage?.({ data: JSON.stringify(ev) }); }
  drop(code = 1006) { this.readyState = 3; this.onclose?.({ code }); }
}
const last = () => must(FakeWS.all.at(-1));

const KEYS = {
  incidents: ['incidents', 'firing', 0, 50], silences: ['silences'], machines: ['machines', 'live'], relays: ['relays'], overview: ['overview'],
  jobs: ['jobs', 'active', 0, 6, ''], machineA: ['machine', 'a'], retire: ['retire-preview', 'a'],
};
type K = keyof typeof KEYS;
let qc: QueryClient;
let stop: () => void;
const seed = () => { for (const k of Object.values(KEYS)) qc.setQueryData(k, { v: 1 }); };
const stale = (k: K) => !!qc.getQueryCache().find({ queryKey: KEYS[k], exact: true })!.state.isInvalidated;
const staleSet = () => (Object.keys(KEYS) as K[]).filter(stale);
const at = (ms: number) => vi.advanceTimersByTimeAsync(ms);

beforeEach(async () => {
  vi.useFakeTimers();
  vi.setSystemTime(1_800_000_000_000);
  vi.spyOn(Math, 'random').mockReturnValue(0.5); // 退避抖动取中间值：1s、2s…
  FakeWS.all = [];
  const listeners = { addEventListener: () => {}, removeEventListener: () => {} };
  vi.stubGlobal('WebSocket', FakeWS);
  vi.stubGlobal('window', listeners);
  vi.stubGlobal('document', { hidden: false, ...listeners });
  vi.stubGlobal('location', { protocol: 'http:', host: 'fleet.test' });
  resetGate();
  qc = new QueryClient();
  stop = startStream(qc);
  await at(0); // 推迟一轮才连
  last().accept();
  seed(); // 首次连上的失效另有用例；这里从干净的缓存开始
});
afterEach(() => { stop(); vi.unstubAllGlobals(); vi.restoreAllMocks(); vi.useRealTimers(); });

describe('事件合并', () => {
  it('300ms 内的事件合成一批，到点才失效', async () => {
    last().send({ type: 'incidents' });
    await at(100);
    last().send({ type: 'incidents' });
    await at(199);
    expect(staleSet()).toEqual([]);
    await at(1);
    expect(staleSet()).toEqual(['incidents', 'silences']);
  });
  it('resync：全部失效（退役预检除外）', async () => {
    last().send({ type: 'resync' });
    await at(300);
    expect(staleSet()).toEqual(['incidents', 'silences', 'machines', 'relays', 'overview', 'jobs', 'machineA']);
  });
  it('心跳不引起失效', async () => {
    last().send({ type: 'hb' });
    await at(5000);
    expect(staleSet()).toEqual([]);
  });
});

// 整表的失效排在合并到点之后的下一个宏任务（合并回调里 setTimeout 0），所以这里比 300ms 多走 1ms
describe('整表的失效', () => {
  it('最多每 2 秒一次：第二批事件的整表等到上次之后满 2 秒', async () => {
    last().send({ type: 'relay' });
    await at(301);
    expect(staleSet()).toEqual(['machines', 'relays']);
    seed();
    await at(199); // t = 500
    last().send({ type: 'relay' });
    await at(300); // t = 800：合并到点；上一次整表失效在 t = 301（0ms 定时器按 1ms 算），这次要等到 301 + 2000
    expect(staleSet()).toEqual([]);
    await at(1500);
    expect(staleSet()).toEqual([]);
    await at(1);
    expect(staleSet()).toEqual(['machines', 'relays']);
  });
  it('正在取的整表不打断也先不失效，取完后下一轮再失效', async () => {
    let resolve!: (v: unknown) => void;
    const p = qc.fetchQuery({ queryKey: KEYS.machines, queryFn: () => new Promise((r) => { resolve = r; }), staleTime: 0 });
    await at(10);
    last().send({ type: 'relay' }); // 事件在这次取开始之后：在途的那份可能是变更之前的
    await at(301);
    expect(staleSet()).toEqual(['relays']);
    resolve({ v: 2 });
    await p;
    await at(2000);
    expect(staleSet()).toEqual(['machines', 'relays']);
  });
  it('事件之后才开始、并已成功取回的整表不再失效（它已经包含这次变更）', async () => {
    last().send({ type: 'relay' });
    await at(50);
    await qc.fetchQuery({ queryKey: KEYS.machines, queryFn: async () => ({ v: 2 }), staleTime: 0 });
    await at(251);
    expect(staleSet()).toEqual(['relays']);
  });
  it('单台的 machine 事件、没人看这台时：详情与整表都失效', async () => {
    last().send({ type: 'machine', id: 'a' });
    await at(301);
    expect(staleSet()).toEqual(expect.arrayContaining(['machineA', 'machines', 'incidents', 'jobs', 'relays', 'overview']));
    expect(stale('retire')).toBe(false);
  });
});

/** 页面上挂着的机器列表（有观察者 = 正在显示）：挂上就取，queryFn 过闸门，记下每次真正发出的取；resolve 由测试决定什么时候回来 */
function machinesFetch(gated = true) {
  const calls: ((v: unknown) => void)[] = [];
  const fn = () => new Promise((r) => { calls.push(r); });
  const obs = new QueryObserver(qc, { queryKey: KEYS.machines, queryFn: ({ queryKey }) => (gated ? afterSubscribed(queryKey, fn) : fn()), staleTime: Infinity, retry: false });
  obs.subscribe(() => {});
  return { calls };
}
async function fresh() {
  stop();
  resetGate();
  qc = new QueryClient();
  stop = startStream(qc);
}

describe('首屏与订阅', () => {
  it('连上之前要取的机器列表等连上再取，连上后不再补取', async () => {
    await fresh();
    const { calls } = machinesFetch();
    await at(0);
    expect(calls).toHaveLength(0); // 等在闸门前
    last().accept();
    await at(0);
    expect(calls).toHaveLength(1);
    must(calls[0])({ v: 2 });
    await at(0);
    await at(5000);
    expect(calls).toHaveLength(1);
    expect(stale('machines')).toBe(false);
  });
  it('在途的、开始于订阅之前的那次取：连上时打断重取', async () => {
    await fresh();
    const { calls } = machinesFetch(false); // 没过闸门（如旧代码路径）：开始于订阅之前
    await at(10);
    expect(calls).toHaveLength(1);
    last().accept();
    await at(0);
    expect(calls).toHaveLength(2);
    // 打断的那次即使后回来也不算数：缓存里是连上之后取的那份
    must(calls[1])({ v: 'after' });
    must(calls[0])({ v: 'before' });
    await at(0);
    expect(qc.getQueryData(KEYS.machines)).toEqual({ v: 'after' });
    expect(stale('machines')).toBe(false);
  });
  it('在后台连上：在途的早于订阅的那次打断、只标过期，不在后台重取', async () => {
    await fresh();
    const { calls } = machinesFetch(false);
    await at(10);
    vi.stubGlobal('document', { hidden: true, addEventListener: () => {}, removeEventListener: () => {} });
    last().accept();
    must(calls[0])({ v: 'before' });
    await at(0);
    expect(calls).toHaveLength(1);
    expect(qc.getQueryData(KEYS.machines)).toBeUndefined();
    expect(stale('machines')).toBe(true);
  });
  it('等不到连上：1.5 s 后照取；之后连上了，这份早于订阅，补取一次', async () => {
    await fresh();
    const { calls } = machinesFetch();
    await at(1499);
    expect(calls).toHaveLength(0);
    await at(1);
    expect(calls).toHaveLength(1);
    must(calls[0])({ v: 2 });
    await at(0);
    await at(500);
    last().accept();
    expect(stale('machines')).toBe(true);
  });
  it('还没连上就断了（未登录的握手被拒）：不再等，马上照取', async () => {
    await fresh();
    const { calls } = machinesFetch();
    await at(100);
    expect(calls).toHaveLength(0);
    last().drop();
    await at(0);
    expect(calls).toHaveLength(1);
  });
  it('接手提前开的连接：已连上就不再新开，连上之后收到的消息补发', async () => {
    stop();
    resetGate();
    openEarly();
    const early = last();
    early.accept();
    early.send({ type: 'incidents' }); // 接手之前到的事件
    qc = new QueryClient();
    seed();
    stop = startStream(qc);
    const n = FakeWS.all.length;
    await at(0);
    expect(FakeWS.all).toHaveLength(n); // 没有另开一条
    await at(300);
    expect(stale('incidents')).toBe(true);
  });
});

describe('连接', () => {
  it('首次连上只让早于订阅取回的机器类查询失效；断线重连后全部失效（退役预检除外）', async () => {
    stop();
    resetGate();
    qc = new QueryClient();
    seed(); // 连上之前就有的数据（早于订阅）
    stop = startStream(qc);
    await at(10);
    last().accept();
    expect(staleSet()).toEqual(['machines', 'relays', 'overview', 'machineA']);
    seed();
    last().drop();
    await at(999);
    expect(FakeWS.all).toHaveLength(2); // beforeEach 一条、这里一条，还没重连
    await at(1);
    expect(FakeWS.all).toHaveLength(3);
    last().accept();
    expect(staleSet()).toEqual(['incidents', 'silences', 'machines', 'relays', 'overview', 'jobs', 'machineA']);
  });
  it('退避翻倍；连上后清零', async () => {
    last().drop();
    await at(1000);
    last().drop();
    await at(1999);
    expect(FakeWS.all).toHaveLength(2);
    await at(1);
    expect(FakeWS.all).toHaveLength(3);
    last().accept();
    last().drop();
    await at(1000);
    expect(FakeWS.all).toHaveLength(4);
  });
  it('1008（连接数超上限）按最长间隔 30 秒重连', async () => {
    last().drop(1008);
    await at(29_999);
    expect(FakeWS.all).toHaveLength(1);
    await at(1);
    expect(FakeWS.all).toHaveLength(2);
  });
  it('45 秒什么都没收到当作半开连接，关掉重连', async () => {
    const first = last();
    await at(30_000);
    first.send({ type: 'hb' }); // t = 30s 收到心跳：45s 时还不算死
    await at(15_000);
    expect(FakeWS.all).toHaveLength(1);
    await at(30_000); // t = 75s：距离心跳 45s
    expect(first.readyState).toBe(3);
    await at(1000);
    expect(FakeWS.all).toHaveLength(2);
  });
});
