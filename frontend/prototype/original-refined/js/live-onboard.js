// live-onboard.js — 上架流水线接新后台（fleetd /v2/api/v1/onboard/*，fleet/crates/onboard/WIRING.md §2–3）。
// 读：批次列表、批次视图（与 demo_batch 同形，经 onboarding-model.js liveBatch 变成同一个模型）、事件（步骤日志）。
// 写（X-Fleet-Request: 1，经 fleet.js）：建批次（CSV → 计划）、按计划哈希开始、继续、取消（整批 / 一行，逆序补偿）、
// 行操作（补密码 provide_input、DD 人工闸门 continue、重试 retry[+force]）。行操作之后都要 resume，后台才会接着跑。
// 后台每秒看一次各批次的最新事件号，变了就推 SSE `onboard`（path = 那个批次）；本模块收到就重读。SSE 断了的兜底：
// 有批次在跑时 10 秒读一次，没有时 60 秒，页面在后台时不读。
import { fleetAvailable, fleetGet, fleetWrite, onFleet } from './fleet.js';
import { liveBatch, addEvents } from './onboarding-model.js';

const B = (id) => `onboard/batches/${encodeURIComponent(id)}`;
const RUNNING = new Set(['running', 'cancelling']);

/** 新后台上架流水线可用吗：fleetd 在，且 /system 的 features 里有 onboard（不发探测请求）。 */
export function onboardAvailable() {
  return fleetAvailable() && !!globalThis.TEAMOPS?.fleet?.has?.('onboard');
}

/**
 * attach(st, { onChange, onError }) — 给页面用的实时源：
 *   list      批次摘要（GET /batches）       load(id) 取一个批次的完整视图（替换 st.batches 里的那一份）
 *   events(id) 增量事件（抽屉日志）         start / resume / cancel / rowAction / create / plan：写
 *   dispose()  停止轮询与订阅
 * 404（fleetd 没配 onboard）→ state.disabled = true，页面显示“新后台未启用上架流水线”。
 */
export function attachOnboard(st, { onChange = () => {}, onError = () => {}, current = () => null, wantEvents = () => null } = {}) {
  const S = { list: [], loaded: false, disabled: false, error: null, busy: false, disposed: false };
  let timer = null, lastList = 0;

  async function loadList() {
    try {
      const data = await fleetGet('onboard/batches', null);
      S.list = Array.isArray(data) ? data.slice().sort((a, b) => (b.created_at ?? 0) - (a.created_at ?? 0)) : [];
      S.loaded = true; S.error = null; S.disabled = false;
      lastList = Date.now();
    } catch (e) {
      if (e?.status === 404) { S.disabled = true; S.loaded = true; S.error = null; }
      else { S.error = e?.message || '读取失败'; if (e?.auth) throw e; }
    }
    return S.list;
  }
  async function load(id) {
    if (!id) return null;
    const view = await fleetGet(B(id), null);
    return view ? liveBatch(st, view) : null;
  }
  async function events(id) {
    const b = st.batches.find((x) => x.id === id);
    if (!b) return [];
    const list = await fleetGet(`${B(id)}/events`, { after: b.eventsAfter || 0 });
    addEvents(b, list);
    return b.events;
  }

  let again = null;
  async function refresh({ list = true } = {}) {
    if (S.disposed) return;
    if (S.busy) { again = { list: list || !!again?.list }; return; } // 读的过程中又有变化：读完再来一次
    S.busy = true;
    try {
      if (list || !S.loaded || Date.now() - lastList > 30000) await loadList();
      const id = current() ?? S.list[0]?.id;
      if (id && !S.disabled) await load(id);
      const ev = wantEvents();
      if (ev && !S.disabled) await events(ev).catch(() => {});
      onChange();
    } catch (e) { onError(e); }
    finally {
      S.busy = false;
      if (again && !S.disposed) { const a = again; again = null; refresh(a); } else schedule();
    }
  }
  function schedule() {
    clearTimeout(timer);
    if (S.disposed) return;
    const b = st.batches.find((x) => x.id === (current() ?? S.list[0]?.id));
    const running = b ? RUNNING.has(b.apiStatus) : S.list.some((x) => x.status === 'running');
    if (globalThis.document?.visibilityState === 'hidden') return; // 回到前台时 onVis 再读
    timer = setTimeout(() => refresh({ list: false }), running ? 10000 : 60000);
  }
  const offSse = onFleet(({ resource, path }) => {
    if (typeof resource === 'string' && (resource === 'onboard' || resource.startsWith('onboard/'))) {
      // 某个批次的进度：只重读它（列表 30 秒内读过就不读）；建批次 / 写操作（path = 列表）：列表也重读
      const one = /\/onboard\/batches\/([^/?]+)/.exec(path ?? '')?.[1];
      const cur = current() ?? S.list[0]?.id;
      refresh({ list: !one || decodeURIComponent(one) !== cur });
    }
    if (resource === 'fleet-down') { S.disabled = true; onChange(); }
  });
  const onVis = () => { if (globalThis.document?.visibilityState === 'visible') refresh({ list: false }); };
  globalThis.document?.addEventListener?.('visibilitychange', onVis);

  // ─── 写 ───
  const post = (path, body) => fleetWrite('POST', path, body ?? {});
  /** resume：接管过期租约或让停下的行继续；别人正在跑（409）不算失败——正在跑的流程会接着处理排队的行。 */
  async function resume(id) {
    try { return await post(`${B(id)}/resume`); }
    catch (e) { if (e?.status === 409) return { running: true, message: e.message }; throw e; }
  }
  const api = {
    state: S,
    refresh,
    load,
    events,
    /** 建批次：{csv, title?, options?} → 计划（rows / issues / relay_impact / plan_hash / batch_id / created）。422 = 计划有错误。 */
    async create({ csv, title, options }) {
      try { return { ok: true, plan: await post('onboard/batches', { csv, ...(title ? { title } : {}), ...(options ? { options } : {}) }) }; }
      catch (e) { if (e?.status === 422 && e.data) return { ok: false, plan: e.data, error: e.message }; throw e; }
    },
    plan: (id) => fleetGet(`${B(id)}/plan`, null),
    start: (id, planHash) => post(`${B(id)}/start`, { plan_hash: planHash }),
    resume,
    cancel: (id, row) => post(`${B(id)}/cancel`, row ? { row } : {}),
    /** 行操作 + resume。action: provide_input（password）/ continue（DD 人工闸门）/ retry（force：风险停止）。 */
    async rowAction(id, row, action, { password, force } = {}) {
      const r = await post(`${B(id)}/rows/${encodeURIComponent(row)}/actions`, { action, ...(password ? { password } : {}), ...(force ? { force: true } : {}) });
      const res = await resume(id);
      return { ...r, resumed: res };
    },
    dispose() {
      S.disposed = true;
      clearTimeout(timer);
      offSse();
      globalThis.document?.removeEventListener?.('visibilitychange', onVis);
    },
  };
  return api;
}
