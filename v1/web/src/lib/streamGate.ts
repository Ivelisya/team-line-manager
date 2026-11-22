// 实时流的「已订阅」闸门：首屏的机器类查询（机器列表、单台、线路机、总览）等实时流连上（= 服务端已订阅）再取，取回的快照就不会早于订阅，
// 连上之后不必为了补「快照与订阅之间的变化」再整批重取一次。等不到（连得慢、连不上）最多等 GATE_MS 就照取，连上后由 stream.ts 补取早于订阅的那几份。
// 连接尽量早开（main.tsx 在下载管理端代码之前就调 openEarly）：握手和代码下载、/auth/me、/meta 并行，页面挂上时多半已经连上，闸门不添等待。
// 这个模块不依赖别的（main.tsx 静态引入它，客户页的入口也带着，但客户页不调用）：不引 react-query，查询键按 JSON 记——
// 这几类查询的键只有字符串 / 数字，JSON.stringify 的结果就是 TanStack 的 queryHash。
type QueryKey = readonly unknown[];
const hashKey = (k: QueryKey) => JSON.stringify(k);

/** 等实时流连上最多这么久（从开始连算起）；之后照常取，连上后补取 */
const GATE_MS = 1500;

let openedAt = 0; // 第一次连上的时刻；0 = 还没连上过
let deadline = 0; // 闸门最晚放行的时刻；0 = 没有在连（直接放行）
let release: (() => void) | null = null;
let gate: Promise<void> | null = null;
let timer: ReturnType<typeof setTimeout> | undefined;
/** 各查询最近一次开始取的时刻（hashKey → ms）：连上时据此判断哪几份早于订阅 */
const startedAt = new Map<string, number>();

const open = () => { release?.(); release = null; gate = null; clearTimeout(timer); };

/** 开始连接（stream.ts 每次开新连接前调用）：还没连上过的话，闸门从现在起最多关 GATE_MS */
export function gateConnecting() {
  if (openedAt || gate) return;
  deadline = Date.now() + GATE_MS;
  gate = new Promise((r) => { release = r; });
  timer = setTimeout(open, GATE_MS);
}
/** 连上了（第一次）：放行 */
export function gateOpened() {
  if (openedAt) return;
  openedAt = Date.now();
  open();
}
/** 连接还没连上就断了（未登录的握手 401、网络错误）：不再等它，放行 */
export function gateFailed() { if (!openedAt) open(); }

/** 机器类查询的 queryFn 外面套一层：先过闸门，记下开始取的时刻 */
export async function afterSubscribed<T>(queryKey: QueryKey, fn: () => Promise<T>): Promise<T> {
  const k = hashKey(queryKey);
  if (gate && Date.now() < deadline) {
    // 等在闸门前的不算早于订阅：连上的那一刻 stream.ts 就要判断，这时它还没往下走、没记开始时刻
    startedAt.set(k, Infinity);
    await gate;
  }
  startedAt.set(k, Date.now());
  return fn();
}
/** 这份查询的数据（或在途的这次取）是否早于订阅：记过开始时刻的按它算，没记过的（列表里那份当初始数据）按数据的时刻算 */
export function predatesSubscription(queryHash: string, dataUpdatedAt: number) {
  const t = startedAt.get(queryHash) ?? dataUpdatedAt;
  return t < openedAt;
}

// ---- 提前开的连接：main.tsx 在加载管理端代码前开，stream.ts 的 startStream 第一次连接时接手（连上前收到的消息先存着，接手时补发） ----
type Early = { ws: WebSocket; opened: boolean; closed: boolean; buf: MessageEvent[] };
let early: Early | null = null;
export const streamUrl = () => `${location.protocol === 'https:' ? 'wss' : 'ws'}://${location.host}/api/v1/stream`;

export function openEarly() {
  if (early || typeof WebSocket === 'undefined') return;
  const ws = new WebSocket(streamUrl());
  const e: Early = (early = { ws, opened: false, closed: false, buf: [] });
  gateConnecting();
  ws.onopen = () => { e.opened = true; gateOpened(); };
  ws.onmessage = (m) => { e.buf.push(m); };
  ws.onclose = () => { e.closed = true; gateFailed(); };
}
/** 接手提前开的连接（只给一次）；已经断了的不给 */
export function takeEarly(): Early | null {
  const e = early;
  early = null;
  return e && !e.closed ? e : null;
}

/** 单元测试用：回到「这一页还没连过」的状态 */
export function resetGate() { openedAt = 0; deadline = 0; open(); startedAt.clear(); early = null; }
