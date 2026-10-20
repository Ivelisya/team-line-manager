// onboarding-model.js — 上架流水线的数据模型与模拟引擎（SPEC §6.6）。
// 数据：store.onboarding() 的 demo_batch（8 行，16 步 P1–P16，7 个阶段）+ history（22 条旧流程任务）。
// 这里把演示批次变成可变的会话内模型：进行中的格子按快照时钟每秒计时，线路机合批窗口倒计时，
// 页面上的操作（提交密码、重试、再等、放弃、改 IP、跳过、暂停、新建批次）会改动模型并推动后续步骤。
// 模型只在本会话内存里（每个 store 一份），换页回来状态还在；刷新页面回到快照。

import { fmt } from '../../shared/index.js';
import { planById, resolvePlan } from './plans.js';

// ─── 词表（行状态、分组桶）与文字清洗 ─────────────────────────────────────────

/**
 * 行状态 → [词, 语气, 分组桶]。桶：done 成功 · running 进行中 · attention 待介入（等待输入、风险停止、失败，都要人来处理）· other
 * 词表按 brief §4.10 流水线步骤（排队 · 进行中 · 成功 · 失败 · 已回滚 · 跳过 · 已取消）；“待介入”与全站“需处理”（monitor.attention）口径不同，
 * 所以不再叫“需处理”（consistency #4）。
 */
export const ROW = {
  active: ['成功', 'ok', 'done'],
  dry_done: ['演练完成', 'ok', 'done'],
  in_relay_batch: ['合批中', 'info', 'running'],
  running: ['进行中', 'info', 'running'],
  queued: ['排队', 'neutral', 'running'],
  waiting_gate: ['等待闸门', 'info', 'running'],
  waiting_input: ['等待输入', 'warn', 'attention'],
  blocked: ['已停止（风险）', 'bad', 'attention'],
  failed: ['失败', 'bad', 'attention'],
  cancelled: ['已取消', 'neutral', 'other'],
};
export const BUCKETS = [
  { value: '', label: '全部' },
  { value: 'attention', label: '待介入' },
  { value: 'running', label: '进行中' },
  { value: 'done', label: '成功' },
];
export const BATCH_WORD = { running: ['进行中', 'info'], paused: ['已暂停', 'warn'], attention: ['待介入', 'warn'], succeeded: ['成功', 'ok'], cancelled: ['已取消', 'neutral'],
  // 实时批次（新后台 onboard 流水线）独有：执行者不在了（崩溃 / 重启）等继续；建好还没确认计划
  interrupted: ['已中断', 'warn'], draft: ['待确认', 'neutral'], cancelling: ['取消中', 'warn'] };
export const GROUP_WORD = { collecting: ['收集中', 'info'], merging: ['合批中', 'info'], partial: ['部分失败', 'warn'], done: ['成功', 'ok'] };
export const TERMINAL = new Set(['succeeded', 'skipped', 'cancelled']);

/** 页面文字清洗：方向词只用于流量（V4），入站 → 入口、出站 → 转发。数据里的步骤名与说明都经过它。 */
export function clean(s) { return s == null ? s : String(s).replace(/入站/g, '入口').replace(/出站/g, '转发'); }

/** 估算文字 → 秒区间：'30–45 s' → [30, 45]；'5–10 分' → [300, 600]；'≈5 s' → [5, 5]；'< 3 s' → [1, 3] */
export function estRange(e) {
  const s = String(e ?? '');
  const mul = /分/.test(s) ? 60 : 1;
  const m = s.match(/(\d+(?:\.\d+)?)\s*[–-]\s*(\d+(?:\.\d+)?)/);
  if (m) return [Number(m[1]) * mul, Number(m[2]) * mul];
  const one = s.match(/(\d+(?:\.\d+)?)/);
  if (!one) return [10, 30];
  const v = Number(one[1]) * mul;
  return /</.test(s) ? [1, v] : [v, v];
}
/** 估算的中文写法：'5–10 分' → '5–10 分钟'，'30–45 s' → '30–45 秒' */
export function estWord(e) { return clean(String(e ?? '—')).replace(/\s*s$/, ' 秒').replace(/分$/, '分钟').replace(/^每批 /, '每批 '); }

// 可重复的伪随机（按行名 + 步骤码）：同一行同一步每次得到同样的耗时
function hash(str) { let h = 2166136261; for (let i = 0; i < str.length; i++) { h ^= str.charCodeAt(i); h = Math.imul(h, 16777619); } return (h >>> 0) / 4294967296; }
const pick = (range, key) => Math.round(range[0] + (range[1] - range[0]) * hash(key));

// ─── 会话状态 ───────────────────────────────────────────────────────────────

const states = new WeakMap();
/** obState(store) → 本会话的流水线状态（批次、历史、引擎、事件）。 */
export function obState(store) {
  if (!states.has(store)) states.set(store, createState(store));
  return states.get(store);
}

function createState(store) {
  const raw = store.onboarding();
  const listeners = new Set();
  const st = {
    store,
    batches: [],
    history: raw.history.map((h) => legacyTask(h)),
    emit(kind = 'change', detail = {}) { for (const fn of listeners) fn(kind, detail); },
    on(fn) { listeners.add(fn); return () => listeners.delete(fn); },
    now: () => store.now(),
  };
  if (raw.batch) st.batches.push(demoBatch(store, raw.batch));
  return st;
}

/** 实时数据：接入任务列表或任务详情（步骤）更新后重读 history。 */
export function refreshHistory(st) {
  st.history = st.store.onboarding().history.map((h) => legacyTask(h));
  return st.history;
}

// ─── 演示批次 → 可变模型 ─────────────────────────────────────────────────────

function stepDefs(b) {
  return b.steps.map((s) => ({ ...s, name: clean(s.name), est: estRange(s.estimate), estWord: estWord(s.estimate) }));
}

function demoBatch(store, b, { live = false } = {}) {
  const asOf = store.now();
  const steps = stepDefs(b);
  const byCode = new Map(steps.map((s) => [s.code, s]));
  const batch = {
    id: b.id, demo: !live, live, title: b.title, type: b.type, type_word: b.type_word,
    paused: false, cancelledAll: false,
    recipe: b.recipe, canary: { ...b.canary }, options: { ...b.options },
    operator: b.operator, started_at: b.started_at, confirmed_count: b.confirmed_count,
    steps, byCode, phases: b.phases.map((p) => ({ ...p })),
    rows: [], relayGroups: [], nextGroupNo: {},
  };
  if (live) Object.assign(batch, liveFields(b));
  for (const r of b.rows) {
    const row = {
      id: r.name, name: r.name, rowId: r.id ?? r.name, provider: r.provider, provider_code: r.provider_code,
      plan_id: r.plan_id, plan: r.plan, price: r.price, currency: r.currency, bandwidth: r.bandwidth,
      relay: r.relay, relay_port: r.relay_port, team: r.team, service_id: r.service_id, ip: r.ip,
      due_date: r.due_date, billing_timezone: r.billing_timezone, mode: r.mode,
      risk: r.risk ?? null, error_class: r.error_class ?? null, secret: live ? null : r.row_state === 'waiting_input' ? 'rejected' : 'email',
      canary: b.canary?.node === r.name, cancelled: live ? r.row_state === 'cancelled' : false, attempts: {},
      ...(live ? { apiState: r.row_state, full_name: r.full_name, ssh_port: r.ssh_port, ss_public_port: r.ss_public_port, nat: !!r.nat, dd: r.dd ?? 'none', current_step: r.current_step, reason: r.reason ?? null } : {}),
      cells: r.cells.map((c) => {
        const cell = { step: c.step, status: c.status, duration_s: c.duration_s ?? null, detail: clean(c.detail ?? ''), since: null, target_s: null, attempt: c.attempt ?? null };
        if (c.status === 'running') { cell.since = asOf - (c.duration_s ?? 0); }
        return cell;
      }),
    };
    row.cellMap = new Map(row.cells.map((c) => [c.step, c]));
    // 团队1-40 的重装验收：参照同批 团队1-37 的实测（402 秒）结束（实时批次不模拟：步骤由后台推进）
    if (!live) for (const c of row.cells) if (c.status === 'running' && c.step !== 'P13') c.target_s = c.step === 'P6' ? 402 : pick(byCode.get(c.step).est, `${row.name}|${c.step}`);
    if (!live && row.cellMap.get('P2')?.status === 'waiting_input') row.attempts.P2 = 1;
    batch.rows.push(row);
  }
  // 线路机合批：收集中的窗口按快照时刻倒计时
  for (const g of b.relay_groups ?? []) {
    const grp = { relay: g.relay, batch_no: g.batch_no, state: g.state, members: [...(g.members ?? [])], xray_reloads: g.xray_reloads ?? 0, xray_restarts: g.xray_restarts ?? null, probe_restarts: g.probe_restarts ?? 0, impact: clean(g.impact), window_s: b.options?.relay_window_s ?? 90, window_end: g.state === 'collecting' ? asOf + (g.window_left_s ?? 0) : null, merging_until: null, left: [] };
    if (g.state === 'partial' && !live) grp.left = g.members.filter((m) => !['active'].includes(rowState(batch, batch.rows.find((x) => x.name === m))));
    batch.relayGroups.push(grp);
    batch.nextGroupNo[g.relay] = Math.max(batch.nextGroupNo[g.relay] ?? 1, g.batch_no + 1);
  }
  return batch;
}

/** 实时批次（新后台 GET /onboard/batches/{id}，与 demo_batch 同形）的额外字段。 */
function liveFields(b) {
  const status = { running: 'running', paused: 'interrupted', attention: 'attention', succeeded: 'succeeded', cancelled: 'cancelled', draft: 'draft', cancelling: 'cancelling' }[b.status] ?? 'draft';
  return { apiStatus: status, status_word: b.status_word, plan_hash: b.plan_hash ?? null, finished_at: b.finished_at ?? null, created_at: b.created_at ?? null,
    error: b.error ?? b.last_error ?? null, events: [], eventsAfter: 0 };
}
/**
 * 新后台的批次视图 → 与演示批次同一个可变模型（live=true：不模拟，步骤由后台推进；页面按接口数据重画）。
 * 同一批次重新取到时整体替换（保留已读到的事件）。
 */
export function liveBatch(st, view) {
  const b = demoBatch(st.store, view, { live: true });
  const old = st.batches.find((x) => x.id === b.id);
  if (old?.live) { b.events = old.events; b.eventsAfter = old.eventsAfter; }
  const i = st.batches.findIndex((x) => x.id === b.id);
  if (i >= 0) st.batches[i] = b; else st.batches.push(b);
  return b;
}
/** 新后台的事件（GET /batches/{id}/events?after=N）并进批次：步骤抽屉的日志用。 */
export function addEvents(batch, list) {
  for (const e of list ?? []) {
    if (!(e?.id > batch.eventsAfter)) continue;
    batch.events.push(e);
    batch.eventsAfter = e.id;
  }
  if (batch.events.length > 2000) batch.events.splice(0, batch.events.length - 2000);
}

// ─── 派生：行状态、阶段格、KPI、预计剩余 ─────────────────────────────────────

export function cellDuration(c, now) {
  if (c.status === 'running' && c.since != null) return Math.max(0, now - c.since);
  return c.duration_s;
}
/** 当前关注的格子：第一个不是成功 / 跳过 / 排队的；都没有时是第一个排队的；全部完成时是最后一步。 */
export function currentCell(row) {
  return row.cells.find((c) => !TERMINAL.has(c.status) && c.status !== 'queued') ?? row.cells.find((c) => c.status === 'queued') ?? row.cells[row.cells.length - 1];
}
export function rowState(batch, row) {
  if (!row) return 'queued';
  if (row.cancelled) return 'cancelled';
  if (batch.live && ROW[row.apiState]) return row.apiState; // 后台给的行状态为准（等待闸门、合批中这些格子上看不出来）
  const st = row.cells.map((c) => c.status);
  if (st.includes('blocked')) return 'blocked';
  if (st.includes('failed')) return 'failed';
  if (st.includes('waiting_input')) return 'waiting_input';
  if (row.dry && st.every((s) => TERMINAL.has(s))) return 'dry_done';
  if (st.every((s) => s === 'succeeded' || s === 'skipped')) return 'active';
  const run = row.cells.find((c) => c.status === 'running');
  if (run?.step === 'P13' && batch.relayGroups.some((g) => g.relay === row.relay && (g.state === 'collecting' || g.state === 'merging') && g.members.includes(row.name))) return 'in_relay_batch';
  if (run) return 'running';
  if (st.includes('waiting_gate')) return 'waiting_gate';
  return 'queued';
}
export function rowWord(batch, row) {
  const s = rowState(batch, row);
  const [word, tone, bucket] = ROW[s] ?? [s, 'neutral', 'other'];
  return { state: s, word, tone, bucket };
}
/** 阶段格：多个步骤汇总（最差或最活跃的胜出，与 shared/data.js aggregateSteps 同一规则），耗时为各步之和。 */
const PRIORITY = ['blocked', 'failed', 'waiting_input', 'running', 'waiting_gate', 'rolled_back', 'cancelled'];
export function phaseCell(row, phase, now) {
  const cells = phase.steps.map((s) => row.cellMap.get(s)).filter(Boolean);
  const st = cells.map((c) => c.status);
  let status = 'queued';
  const p = PRIORITY.find((x) => st.includes(x));
  if (p) status = p;
  else if (st.every((s) => s === 'skipped')) status = 'skipped';
  else if (st.every((s) => s === 'succeeded' || s === 'skipped')) status = 'succeeded';
  else if (st.some((s) => s === 'succeeded')) status = 'running';
  if (status === 'cancelled' && st.some((s) => s === 'succeeded') && st.every((s) => s === 'succeeded' || s === 'skipped' || s === 'cancelled')) status = 'cancelled';
  const dur = cells.reduce((a, c) => a + (cellDuration(c, now) ?? 0), 0);
  const focus = cells.find((c) => c.status === status) ?? cells.find((c) => !TERMINAL.has(c.status)) ?? cells[cells.length - 1];
  return { status, duration_s: dur, cells, focus };
}
/** 单元格文字：成功 = 耗时（1 个单位）；进行中 = 实时计时（2 个单位，每秒变）；等待 / 失败 / 停止 = 状态词。 */
export function cellText(status, dur, detail = '') {
  if (status === 'succeeded') return dur ? fmt.duration(dur, { units: 1 }).text : '';
  if (status === 'running') return fmt.duration(dur ?? 0, { units: 2 }).text;
  if (status === 'waiting_input') return '等待输入';
  if (status === 'waiting_gate') return '等待';
  if (status === 'failed') return /超时/.test(detail) ? '超时' : '失败';
  if (status === 'blocked') return '已停止';
  if (status === 'rolled_back') return '已回滚';
  if (status === 'cancelled') return '已取消';
  return '';
}

export function batchStatus(batch) {
  if (batch.live) return batch.apiStatus;
  if (batch.cancelledAll) return 'cancelled';
  const states_ = batch.rows.map((r) => rowState(batch, r));
  const live = states_.some((s) => ['running', 'in_relay_batch', 'queued', 'waiting_gate'].includes(s));
  if (batch.paused && live) return 'paused';
  if (live) return 'running';
  if (states_.some((s) => ['waiting_input', 'blocked', 'failed'].includes(s))) return 'attention';
  return 'succeeded';
}

/** 预计剩余：还在跑的行各自剩余步骤估算中值之和（进行中的步骤扣掉已用时间），取最大值（行之间并行）。 */
export function etaOf(batch, now) {
  let max = 0;
  for (const row of batch.rows) {
    const s = rowState(batch, row);
    if (!['running', 'in_relay_batch', 'queued', 'waiting_gate'].includes(s)) continue;
    let rest = 0;
    for (const c of row.cells) {
      const def = batch.byCode.get(c.step);
      const mid = (def.est[0] + def.est[1]) / 2;
      if (c.status === 'running') {
        if (c.step === 'P13') { const g = groupOf(batch, row); rest += g?.window_end ? Math.max(0, g.window_end - now) + 20 : mid; }
        else rest += Math.max(c.target_s != null ? c.target_s - cellDuration(c, now) : 0, Math.max(0, mid - cellDuration(c, now)), 5);
      } else if (c.status === 'queued' || c.status === 'waiting_gate') rest += mid;
    }
    max = Math.max(max, rest);
  }
  return Math.round(max);
}

export function kpisOf(batch, now) {
  const rows = batch.rows.map((r) => ({ r, ...rowWord(batch, r) }));
  const count = (b) => rows.filter((x) => x.bucket === b);
  const prov = new Map();
  for (const r of batch.rows) prov.set(r.provider, (prov.get(r.provider) ?? 0) + 1);
  const running = count('running');
  const runWhere = new Map();
  for (const x of running) {
    const c = currentCell(x.r);
    const w = x.state === 'in_relay_batch' ? '线路机合批' : x.state === 'waiting_gate' ? '等待闸门' : x.state === 'queued' ? '排队' : batch.byCode.get(c.step)?.name ?? c.step;
    runWhere.set(w, (runWhere.get(w) ?? 0) + 1);
  }
  const failed = rows.filter((x) => x.state === 'failed');
  const attn = count('attention'); // 含失败
  return {
    total: batch.rows.length, providers: [...prov],
    cancelled: rows.filter((x) => x.state === 'cancelled').length,
    done: count('done'), running, runWhere: [...runWhere],
    attention: attn, waiting: attn.filter((x) => x.state === 'waiting_input'), risk: attn.filter((x) => x.state === 'blocked'),
    failed, eta: etaOf(batch, now), elapsed: batch.started_at ? Math.max(0, (batch.finished_at ?? now) - batch.started_at) : 0,
    canary: batch.canary,
  };
}

export function groupOf(batch, row) {
  return batch.relayGroups.find((g) => g.relay === row.relay && g.members.includes(row.name) && (g.state === 'collecting' || g.state === 'merging'))
    ?? batch.relayGroups.find((g) => g.relay === row.relay && g.members.includes(row.name));
}

/** 线路机下一个可用端口（机队在用端口 + 演示批次已预留的端口之后）。 */
export function nextPorts(store, st) {
  const max = {};
  for (const n of store.nodes()) if (n.relay && n.relay_port) max[n.relay] = Math.max(max[n.relay] ?? 20000, n.relay_port);
  for (const r of store.relays()) max[r.name] ??= 20000;
  for (const b of st.batches) for (const row of b.rows) if (row.relay && row.relay_port) max[row.relay] = Math.max(max[row.relay] ?? 20000, row.relay_port);
  return max;
}

// ─── 引擎：每秒推进一次 ──────────────────────────────────────────────────────

/** tick(st) — 推进所有批次；结构变化（状态、合批、行完成）时发 'change'，否则发 'tick'（只刷新计时文字）。 */
export function tick(st) {
  const now = st.now();
  let changed = false;
  for (const batch of st.batches) changed = advanceBatch(st, batch, now) || changed;
  st.emit(changed ? 'change' : 'tick');
}

function advanceBatch(st, batch, now) {
  if (batch.cancelledAll || batch.live) return false;
  let changed = false;
  // 1) 线路机合批窗口
  for (const g of batch.relayGroups) {
    if (g.state === 'collecting' && g.window_end != null && now >= g.window_end) {
      const ready = g.members.filter((m) => batch.rows.find((r) => r.name === m)?.cellMap.get('P13')?.status === 'running');
      if (!ready.length) continue;
      g.state = 'merging'; g.merging_until = now + 4; changed = true;
      for (const m of ready) { const c = batch.rows.find((r) => r.name === m).cellMap.get('P13'); c.detail = `${g.relay} 批次 #${g.batch_no}：窗口结束，${ready.length} 个入口合并为 1 次 Xray 更新`; }
    } else if (g.state === 'merging' && now >= g.merging_until) {
      const merged = [], left = [];
      for (const m of g.members) {
        const row = batch.rows.find((r) => r.name === m);
        const c = row?.cellMap.get('P13');
        if (c?.status === 'running') { finish(batch, row, c, now, `${g.relay} 批次 #${g.batch_no}：${g.members.length > 1 ? `${countReady(batch, g)} 个入口` : '1 个入口'}合并为 1 次 Xray 重载`); merged.push(m); startNext(st, batch, row, now); }
        else left.push(m);
      }
      g.members = merged.length ? merged : g.members;
      g.left = left;
      g.xray_reloads += 1; g.probe_restarts += 1;
      g.state = left.length ? 'partial' : 'done';
      g.window_end = null;
      g.impact = `新增 ${merged.length} 个入口${portSpan(batch, merged)}，合并为一次 Xray 更新（中断约 1–3 秒）`;
      changed = true;
    }
  }
  // 2) 行
  for (const row of batch.rows) {
    if (row.cancelled) continue;
    const run = row.cells.find((c) => c.status === 'running');
    if (run && run.step !== 'P13' && run.target_s != null && now - run.since >= run.target_s) {
      finish(batch, row, run, now);
      if (run.result === 'blocked') { changed = true; continue; }
      startNext(st, batch, row, now);
      changed = true;
    }
    // 闸门：金丝雀通过、重装名额
    const gate = row.cells.find((c) => c.status === 'waiting_gate');
    if (gate && !batch.paused && gateOpen(batch, row, gate)) { start(st, batch, row, gate, now); changed = true; }
  }
  // 3) 金丝雀
  if (batch.canary?.enabled && !batch.canary.passed) {
    const c = batch.rows.find((r) => r.name === batch.canary.node);
    if (c && rowState(batch, c) === 'active') { batch.canary.passed = true; changed = true; st.emit('canary', { batch, row: c }); }
  }
  return changed;
}
const countReady = (batch, g) => g.members.filter((m) => batch.rows.find((r) => r.name === m)?.cellMap.get('P13')?.status === 'running').length;
function portSpan(batch, names) {
  const ports = names.map((n) => batch.rows.find((r) => r.name === n)?.relay_port).filter(Boolean).sort((a, b) => a - b);
  if (!ports.length) return '';
  return ports.length === 1 ? ` ${ports[0]}` : ` ${ports[0]}–${ports[ports.length - 1]}`;
}

function gateOpen(batch, row, cell) {
  if (cell.step === 'P1') return !batch.canary?.enabled || batch.canary.passed || batch.canary.node === row.name;
  if (cell.step === 'P4') return ddBusy(batch) < (batch.options?.dd_concurrency ?? 5);
  return true;
}
const ddBusy = (batch) => batch.rows.filter((r) => ['P4', 'P5', 'P6'].some((s) => r.cellMap.get(s)?.status === 'running')).length;

function finish(batch, row, cell, now, detail) {
  cell.duration_s = Math.max(1, Math.round(now - cell.since));
  const res = cell.result;
  if (res === 'blocked') {
    cell.status = 'blocked';
    cell.detail = row.risk?.message ?? '主机公钥与上次不一致，已停止，未发送密码';
    cell.target_s = null;
    return;
  }
  cell.status = 'succeeded';
  cell.target_s = null; cell.since = null; cell.result = null;
  cell.detail = detail ?? successDetail(batch, row, cell.step);
}
function successDetail(batch, row, step) {
  return {
    P1: row.mode === 'proxy' ? 'P1ʹ 代理可达性：SOCKS 出口 IP 与导入一致' : '主机公钥一致，出口 IP 与邮件一致',
    P6: `${batch.recipe?.split(' · ')[1] ?? 'Debian 13 genericcloud'}，expected 公钥已出现`,
    P12: '线路机到 SS 的转发校验通过',
    P15: '出口 IP 与登记一致',
    P16: '连接链接已生成（未展示）',
  }[step] ?? '';
}

/** 从 row 的下一个排队步骤开始（跳过已跳过的）；没有了就是交付完成。 */
function startNext(st, batch, row, now) {
  if (row.dry) {
    for (const c of row.cells) if (c.status === 'queued') { c.status = 'skipped'; c.detail = '只演练：预检之后不执行'; }
    return;
  }
  const next = row.cells.find((c) => c.status === 'queued');
  if (!next) return;
  if (batch.paused) return;
  if (next.step === 'P4' && ddBusy(batch) >= (batch.options?.dd_concurrency ?? 5)) { next.status = 'waiting_gate'; next.detail = `等待重装名额（并发 ${batch.options?.dd_concurrency ?? 5}）`; return; }
  start(st, batch, row, next, now);
}

function start(st, batch, row, cell, now) {
  const def = batch.byCode.get(cell.step);
  cell.since = now; cell.duration_s = 0; cell.result = null;
  if (row.secret === 'missing' && ((cell.step === 'P2' && row.mode !== 'proxy') || (cell.step === 'P1' && row.mode === 'proxy'))) {
    cell.status = 'waiting_input'; cell.duration_s = 0; cell.since = null;
    cell.detail = row.mode === 'proxy' ? '导入里没有代理密码；请从供应商面板复制 SOCKS5 密码' : '邮件里没有 root 密码；请从供应商面板复制 root 密码';
    return;
  }
  cell.status = 'running';
  if (cell.step === 'P13') {
    let g = batch.relayGroups.find((x) => x.relay === row.relay && x.state === 'collecting');
    if (!g) {
      const no = batch.nextGroupNo[row.relay] ?? 1;
      batch.nextGroupNo[row.relay] = no + 1;
      g = { relay: row.relay, batch_no: no, state: 'collecting', members: [], xray_reloads: 0, probe_restarts: 0, impact: '', window_s: batch.options?.relay_window_s ?? 90, window_end: now + (batch.options?.relay_window_s ?? 90), merging_until: null, left: [] };
      batch.relayGroups.push(g);
    }
    if (!g.members.includes(row.name)) g.members.push(row.name);
    g.impact = `预留入口${portSpan(batch, g.members)}，窗口结束后合并为一次 Xray 更新`;
    cell.detail = `${row.relay} 批次 #${g.batch_no}：等待窗口，已排入 ${g.members.length} 台`;
    cell.target_s = null;
    return;
  }
  cell.detail = '';
  cell.target_s = pick(def.est, `${row.name}|${cell.step}|${row.attempts[cell.step] ?? 0}`);
}

// ─── 操作 ───────────────────────────────────────────────────────────────────

export const ACTIONS = {
  /** 等待输入的行提交新密码（只写）：推密钥重新开始。 */
  submitSecret(st, batch, row) {
    const c = row.cells.find((x) => x.status === 'waiting_input');
    if (!c) return;
    row.secret = 'given';
    row.attempts[c.step] = (row.attempts[c.step] ?? 0) + 1;
    start(st, batch, row, c, st.now());
    c.detail = '已收到新密码（只写，未回显），重新推送密钥';
    st.emit('change');
  },
  /** 从这一步重试：失败 / 停止 / 已完成的步骤重新执行，后续步骤回到排队。 */
  retry(st, batch, row, step) {
    const i = row.cells.findIndex((x) => x.step === step);
    if (i < 0) return;
    row.attempts[step] = (row.attempts[step] ?? 0) + 1;
    for (let j = i + 1; j < row.cells.length; j++) if (row.cells[j].status !== 'skipped') Object.assign(row.cells[j], { status: 'queued', duration_s: null, since: null, target_s: null, detail: '' });
    const c = row.cells[i];
    c.extended = false;
    start(st, batch, row, c, st.now());
    if (row.risk && step === 'P1') c.result = 'blocked'; // 风险没解除：重新盘点仍会停下
    c.detail = row.risk && step === 'P1' ? '重新盘点：再扫描 10 次主机公钥' : `第 ${row.attempts[step] + 1} 次尝试`;
    st.emit('change');
  },
  /** 再等 10 分钟：失败的重装验收延长等待（原超时 20 分钟）。 */
  waitMore(st, batch, row, step = 'P6') {
    const c = row.cellMap.get(step);
    if (!c) return;
    row.attempts[step] = (row.attempts[step] ?? 0) + 1;
    c.status = 'running'; c.since = st.now() - (c.duration_s ?? 0); c.target_s = (c.duration_s ?? 0) + 180; c.result = null;
    c.detail = '延长等待 10 分钟：继续等 expected 公钥出现';
    c.extended = true;
    st.emit('change');
  },
  /** 跳过这一步（只对可跳过的步骤）。 */
  skip(st, batch, row, step) {
    const c = row.cellMap.get(step);
    if (!c) return;
    c.status = 'skipped'; c.since = null; c.target_s = null; c.detail = '手动跳过（操作者 admin）';
    startNext(st, batch, row, st.now());
    st.emit('change');
  },
  /** 放弃此行（L2）：未完成的步骤全部取消，释放预留端口。 */
  abandon(st, batch, row) {
    row.cancelled = true;
    for (const c of row.cells) if (!TERMINAL.has(c.status)) Object.assign(c, { status: 'cancelled', since: null, target_s: null });
    for (const g of batch.relayGroups) g.members = g.members.filter((m) => m !== row.name);
    st.emit('change');
  },
  /** 改 IP：换成新 IP 后从预检重新开始，风险解除。 */
  changeIp(st, batch, row, ip) {
    row.ip = ip; row.risk = null;
    ACTIONS.retry(st, batch, row, 'P1');
    row.cellMap.get('P1').detail = `IP 已改为 ${ip}，重新预检`;
    st.emit('change');
  },
  pause(st, batch, v) { batch.paused = v; if (!v) { const now = st.now(); for (const row of batch.rows) if (!row.cells.some((c) => c.status === 'running' || c.status === 'waiting_input') && !row.cancelled) startNext(st, batch, row, now); } st.emit('change'); },
  /** 取消未开始的行（L1）：所有步骤都还在排队 / 闸门的行。返回取消的行名。 */
  cancelUnstarted(st, batch) {
    const out = [];
    for (const row of batch.rows) if (!row.cancelled && row.cells.every((c) => c.status === 'queued' || c.status === 'waiting_gate' || c.status === 'skipped')) { ACTIONS.abandon(st, batch, row); out.push(row.name); }
    st.emit('change');
    return out;
  },
};
export const SKIPPABLE = new Set(['P7', 'P11', 'P14']);

// ─── 新建批次 ───────────────────────────────────────────────────────────────

/**
 * createBatch(st, { type, rows, options }) — rows 是预览表里校验过的行：
 * { ip, provider, provider_code, service_id, plan_id, relay, relay_port, team, number, due_date, billing_timezone, secret: 'email'|'missing', mode }
 */
export function createBatch(st, { type, typeWord, rows, options }) {
  const store = st.store;
  const demo = st.batches.find((b) => b.demo);
  const now = st.now();
  const steps = demo.steps;
  const t = fmt.bjParts ? fmt.bjParts(now) : null;
  const hhmm = fmt.clockTime(now).replace(':', '');
  const mmdd = fmt.date(now).text.slice(5);
  const prov = new Map();
  for (const r of rows) prov.set(r.provider, (prov.get(r.provider) ?? 0) + 1);
  const batch = {
    id: `b-${mmdd.replace('-', '')}-${hhmm}`, demo: false, created: true,
    title: `${mmdd} 新机 ×${rows.length}（${[...prov].map(([p, n]) => `${p} ×${n}`).join(' · ')}）`,
    type, type_word: typeWord, paused: false, cancelledAll: false,
    recipe: demo.recipe, canary: { enabled: !!options.canary, passed: false, node: options.canary ? options.canaryNode : null },
    options: { relay_strategy: options.strategy, dd_concurrency: options.dd, relay_window_s: options.window, dry_run: options.dry },
    operator: 'admin', started_at: now, confirmed_count: 0,
    steps, byCode: demo.byCode, phases: demo.phases, rows: [], relayGroups: [], nextGroupNo: {},
  };
  // 线路机批次号接着演示批次往后排
  for (const g of demo.relayGroups) batch.nextGroupNo[g.relay] = Math.max(batch.nextGroupNo[g.relay] ?? 1, g.batch_no + 1);
  for (const r of rows) {
    const p = planById(r.plan_id);
    const row = {
      id: r.name, name: r.name, provider: r.provider, provider_code: r.provider_code, plan_id: r.plan_id, plan: p?.name ?? '未归类套餐',
      price: p?.price ?? null, currency: p?.currency ?? 'USD', bandwidth: p?.specs?.bandwidth_text ?? '—',
      relay: r.relay, relay_port: r.relay_port, team: r.team, service_id: r.service_id, ip: r.ip, due_date: r.due_date, billing_timezone: r.billing_timezone,
      mode: r.mode, risk: null, secret: r.secret, canary: batch.canary.node === r.name, cancelled: false, attempts: {}, dry: !!options.dry,
      cells: steps.map((s) => ({ step: s.code, status: r.mode === 'proxy' && PROXY_SKIP.has(s.code) ? 'skipped' : 'queued', duration_s: null, detail: r.mode === 'proxy' && PROXY_SKIP.has(s.code) ? '代理产品无主机权限，跳过' : '', since: null, target_s: null })),
    };
    row.cellMap = new Map(row.cells.map((c) => [c.step, c]));
    batch.rows.push(row);
  }
  for (const row of batch.rows) {
    const p1 = row.cellMap.get('P1');
    if (batch.canary.enabled && row.name !== batch.canary.node) { p1.status = 'waiting_gate'; p1.detail = `等待金丝雀 ${batch.canary.node} 通过`; }
    else start(st, batch, row, p1, now);
  }
  st.batches.unshift(batch);
  st.emit('change');
  return batch;
}
export const PROXY_SKIP = new Set(['P2', 'P3', 'P4', 'P5', 'P6', 'P7', 'P8', 'P9', 'P11']);

// ─── 旧流程任务（history）：单台也是一个批次 ─────────────────────────────────

function legacyTask(h) {
  const steps = h.steps.map((s) => ({ ...s, name: clean(s.name), error: clean(s.error) }));
  return { ...h, legacy: true, title: clean(h.title), error: clean(h.error), steps };
}

// ─── 日志、输入输出（演示：由步骤状态、说明与行数据推出来） ───────────────────

const mmss = (s) => `${String(Math.floor(s / 60)).padStart(2, '0')}:${String(Math.floor(s % 60)).padStart(2, '0')}`;
/** 某一格的日志脚本：[{ at 秒, text, tone }]；进行中的格子只显示 at ≤ 已用时间的行，所以日志会随时间“长出来”。 */
function script(batch, row, cell) {
  const ip = row.ip, relay = row.relay ?? '线路机', port = row.relay_port;
  const S = (at, text, tone) => ({ at, text, tone });
  const final = cell.status === 'running' ? 0 : (cell.duration_s ?? 0); // 进行中的格子：结束行只在走到估算终点时出现
  const fp = row.risk?.detail?.match(/SHA256:[0-9a-f]+…/g) ?? [];
  switch (cell.step) {
    case 'P1':
      if (row.mode === 'proxy') return [S(0, `连接 SOCKS5 代理 ${ip}（NodeMach · 服务号 ${row.service_id}）`), S(2, '认证通过，经代理访问出口验证站点'), S(4, `出口 IP ${ip} 与导入一致；AS7018 AT&T 住宅`, 'ok')];
      if (cell.status === 'blocked' || cell.result === 'blocked') return [S(0, `开始预检 ${ip}（${row.provider} · 服务号 ${row.service_id}）`), S(3, 'TCP 22 可达，RTT 141 ms'), S(6, '扫描主机公钥 ×10（每次间隔 3 秒）'), S(36, `出现 2 把 ed25519：${fp[0] ?? 'SHA256:6c3f25d848…'} ×6 / ${fp[1] ?? 'SHA256:bfa3b3c393…'} ×4（演示值）`, 'bad'), S(38, '该 IP 上有另一台机器应答：与上次登记的主机公钥不一致', 'bad'), S(41, '已停止：未发送密码，未做任何改动', 'bad')];
      return [S(0, `开始预检 ${ip}（${row.provider} · 服务号 ${row.service_id}）`), S(3, 'TCP 22 可达，RTT 142 ms'), S(6, '扫描主机公钥 ×10'), S(26, '10 次都是同一把 ed25519 公钥', 'ok'), S(30, `出口 IP ${ip} 与邮件一致；AS7018 AT&T 住宅`), S(final || 34, '预检通过', 'ok')];
    case 'P2':
      if (cell.status === 'waiting_input' && row.secret === 'missing') return [S(0, '邮件里没有 root 密码', 'warn'), S(0, '暂停：等待输入 root 密码（只写，不回显）', 'warn')];
      if (row.secret === 'rejected' || (row.attempts.P2 ?? 0) > 0) {
        const base = [S(0, `用邮件里的 root 密码登录 root@${ip}`), S(4, '密码被拒（第 1 次，共可尝试 2 次）', 'warn'), S(9, '暂停：等待输入新密码（请从供应商面板复制 root 密码）', 'warn')];
        if (cell.status === 'waiting_input') return base;
        return [...base.map((l) => ({ ...l, at: 0 })), S(0, '收到新密码（只写，未回显）'), S(2, `用新密码登录 root@${ip} 成功`), S(4, '推送 bootstrap 公钥，改为密钥登录，关闭密码登录'), S(final || 6, '推密钥完成', 'ok')];
      }
      return [S(0, `用邮件里的 root 密码登录 root@${ip}`), S(2, '登录成功，推送 bootstrap 公钥'), S(4, '改为密钥登录，关闭密码登录'), S(final || 6, '推密钥完成', 'ok')];
    case 'P3': return [S(0, '采集主机身份：CPU 2 核 · 内存 4 GiB · 磁盘 50 GiB'), S(2, '供应商镜像 Debian 12，内核 6.1'), S(final || 5, '主机指纹已记录（ed25519，演示值不展示）', 'ok')];
    case 'P4': return [S(0, `下载 ${batch.recipe?.split(' · ')[1] ?? 'Debian 13 genericcloud'} 镜像`), S(12, '校验 sha256 通过'), S(18, '写入 expected 公钥与 cloud-init 配置'), S(final || 27, '重装准备完成', 'ok')];
    case 'P5': return [S(0, '进入安装环境，开始写盘'), S(20, '主机即将重启，SSH 会断开 5–10 分钟'), S(final || 48, '重装已开始，交给重装验收继续等待', 'ok')];
    case 'P6': {
      const lines = [S(0, '等待新系统启动（超时 20 分钟）'), S(5, `轮询 ${ip}:22`), S(40, '安装环境日志：解压写入 0.6/3.0 GiB'), S(95, '安装环境日志：解压写入 1.2/3.0 GiB'), S(150, '安装环境日志：解压写入 1.6/3.0 GiB'), S(190, '安装环境日志：解压写入 1.9/3.0 GiB')];
      const stuck = [S(360, '主机不可达（TCP 22 无响应）', 'warn'), S(600, '仍不可达（10 分钟），继续等待', 'warn'), S(1200, '超时：expected 公钥未出现，主机不可达 14 分钟', 'bad')];
      if (cell.status === 'failed') return [...lines, ...stuck, S(1200, '判断：可能卡在安装环境；建议打开面板 VNC 查看', 'bad')];
      if (cell.extended) return [...lines, ...stuck, S(1200, '操作者选择再等 10 分钟', 'warn'), ...(cell.status === 'succeeded' ? [S(final, 'expected 公钥已出现（延长等待后）', 'ok')] : [])];
      return [...lines, S(240, '安装环境日志：解压写入 2.5/3.0 GiB'), S(290, '写盘完成，重启进入新系统'), S(350, 'SSH 22 可达，等待 expected 公钥'), S(final && final < 400 ? final : 400, 'expected 公钥已出现', 'ok')];
    }
    case 'P7': return [S(0, '系统基线：时区 Asia/Shanghai、NTP、sysctl'), S(12, '开启自动安全更新，关闭不用的服务'), S(final || 31, '系统基线完成', 'ok')];
    case 'P8': return [S(0, `防火墙：只放行 22 与 SS 端口（来源限定 ${relay}）`), S(final || 14, '防火墙规则已生效', 'ok')];
    case 'P9': return [S(0, `安装 ${batch.recipe?.split(' · ')[2] ?? 'ssserver 1.25.0 aes-256-gcm'}`), S(6, 'SS 监听端口已开启，本机自检通过'), S(final || 12, 'Shadowsocks 完成', 'ok')];
    case 'P10': return [S(0, `中央登记：资产 ${row.name}（${row.team} · ${row.plan}）`), S(final || 2, '登记完成', 'ok')];
    case 'P11': return [S(0, `安装 ${batch.recipe?.split(' · ')[3] ?? 'agent 1.5.0'}`), S(14, 'komari-agent 已连上服务端'), S(final || 26, '首次上报成功', 'ok')];
    case 'P12': return [S(0, `校验 ${relay} 到 SS 的转发`), S(final || 3, '转发校验通过', 'ok')];
    case 'P13': {
      const g = groupOf(batch, row);
      const no = g?.batch_no ?? 1;
      const base = [S(0, `排入 ${relay} 批次 #${no}（入口 :${port ?? '待分配'}）`), S(1, `等待合批窗口（${batch.options?.relay_window_s ?? 90} 秒）内的其它节点`)];
      if (cell.status === 'running') return base;
      return [...base, S(Math.max(2, final - 4), '窗口结束，合并为 1 次 Xray 更新'), S(final || 41, `Xray 重载完成（中断 1.4 秒），入口 ${relay}:${port} 已启用`, 'ok')];
    }
    case 'P14': return [S(0, row.mode === 'proxy' ? '添加 agentless 探针：TCP 每 30 秒、出口验证每 300 秒' : '添加探针：TCP 每 30 秒、出口验证每 300 秒'), S(final || 12, '探针首轮结果正常', 'ok')];
    case 'P15': return [S(0, `经入口 ${relay}:${port} 访问验证站点`), S(10, '连通，TCP 42 ms'), S(final || 24, '出口 IP 与登记一致', 'ok')];
    case 'P16': return [S(0, '启用入口，状态改为 在用'), S(final || 2, '连接链接已生成（未展示）', 'ok')];
    default: return [S(0, cell.detail || '—')];
  }
}
/** logFor(batch, row, code, now) → [{ t:'mm:ss', text, tone }] */
export function logFor(batch, row, code, now) {
  const cell = row.cellMap.get(code);
  if (!cell) return [];
  if (batch.live) return liveLog(batch, row, code, cell);
  if (cell.status === 'queued') return [];
  if (cell.status === 'waiting_gate') return [{ t: '00:00', text: cell.detail || '等待闸门', tone: 'warn' }];
  if (cell.status === 'skipped') return [{ t: '00:00', text: clean(cell.detail) || '跳过' }];
  const lines = script(batch, row, cell);
  const el = cell.status === 'running' ? cellDuration(cell, now) : Infinity;
  const out = lines.filter((l) => l.at <= el).map((l) => ({ t: mmss(Math.min(l.at, cell.status === 'running' ? el : l.at)), text: l.text, tone: l.tone }));
  if (cell.status === 'cancelled') out.push({ t: mmss(cellDuration(cell, now) ?? 0), text: '已取消（放弃此行）', tone: 'warn' });
  if (cell.status === 'running' && cell.step === 'P13') {
    const g = groupOf(batch, row);
    if (g?.state === 'collecting' && g.window_end) out.push({ t: mmss(el), text: `窗口剩 ${Math.max(0, Math.ceil(g.window_end - now))} 秒，已排入 ${g.members.length} 台` });
    if (g?.state === 'merging') out.push({ t: mmss(el), text: '窗口结束，正在合并 Xray 更新' });
  }
  return out;
}
/**
 * 实时批次的日志：新后台的事件（step.started / step.finished / node.state，P13 另有 relay_batch.updated），
 * 时间从这一步第一次开始算；还没读到事件时用格子上的说明。
 */
function liveLog(batch, row, code, cell) {
  const mine = batch.events.filter((e) => {
    if (e.node_id === row.rowId) return e.data?.step === code || (e.type === 'node.state' && e.data?.current_step === code);
    return code === 'P13' && e.type === 'relay_batch.updated' && e.data?.relay === row.relay && (e.data?.members ?? []).includes(row.name);
  });
  const t0 = mine.find((e) => e.type === 'step.started')?.ts ?? mine[0]?.ts;
  const at = (e) => mmss(Math.max(0, ((e.ts ?? t0) - t0) / 1000));
  const out = [];
  for (const e of mine) {
    const d = e.data ?? {};
    if (e.type === 'step.started') out.push({ t: at(e), text: d.attempt > 1 ? `开始（第 ${d.attempt} 次）` : '开始' });
    else if (e.type === 'step.finished') {
      const tone = d.status === 'succeeded' ? 'ok' : d.status === 'skipped' ? undefined : ['failed', 'blocked'].includes(d.status) ? 'bad' : 'warn';
      out.push({ t: at(e), text: `${fmt.step(d.status).word}${d.detail ? `：${clean(d.detail)}` : ''}`, tone });
    } else if (e.type === 'node.state') {
      const w = ROW[d.state === 'ready_for_relay' ? 'in_relay_batch' : d.state]?.[0] ?? d.state;
      out.push({ t: at(e), text: `行状态：${w}${d.reason ? ` · ${clean(d.reason)}` : ''}`, tone: ['failed', 'blocked'].includes(d.state) ? 'bad' : d.state === 'waiting_input' ? 'warn' : undefined });
    } else if (e.type === 'relay_batch.updated') {
      const st = { merging: '合并中', done: '完成', failed: '失败', interrupted: '中断' }[d.status] ?? d.status;
      out.push({ t: at(e), text: `${d.relay} 批次 #${d.batch_no}：${st}（${(d.members ?? []).length} 台）`, tone: d.status === 'done' ? 'ok' : d.status === 'failed' ? 'bad' : undefined });
    }
  }
  if (!out.length && cell.status !== 'queued') out.push({ t: '00:00', text: clean(cell.detail) || fmt.step(cell.status).word, tone: ['failed', 'blocked'].includes(cell.status) ? 'bad' : cell.status === 'waiting_input' ? 'warn' : undefined });
  return out;
}

/** 旧流程任务的日志：每步开始 / 结束 / 错误。 */
export function legacyLog(task) {
  const out = [];
  let t = 0;
  for (const s of task.steps) {
    if (s.status === 'skipped') { out.push({ t: mmss(t), text: `${s.name}：未执行` }); continue; }
    out.push({ t: mmss(t), text: `开始 ${s.name}` });
    t += s.duration_s ?? 0;
    if (s.status === 'failed') out.push({ t: mmss(t), text: `${s.name} 失败：${s.error ?? '—'}`, tone: 'bad' });
    else out.push({ t: mmss(t), text: `${s.name} 完成（${fmt.duration(s.duration_s ?? 0).text}）`, tone: 'ok' });
  }
  if (task.status === 'rolled_back') out.push({ t: mmss(t), text: '已回滚：撤销本次对线路机与登记的改动', tone: 'warn' });
  return out;
}

/** 一格的输入输出（kv 用）：只列演示里有的字段；凭据只写状态，从不显示值。 */
export function ioFor(batch, row, code) {
  const cell = row.cellMap.get(code);
  if (batch.live) {
    const input = [
      ['IP', row.ssh_port && row.ssh_port !== 22 ? `${row.ip}:${row.ssh_port}` : row.ip], ['供应商', row.service_id ? `${row.provider} · 服务号 ${row.service_id}` : row.provider], ['套餐', row.plan],
      ['线路机', row.relay ? `${row.relay}${row.relay_port ? `:${row.relay_port}` : ' · 端口待分配'}` : '—'], ['团队', row.team],
      ...(row.nat ? [['SS 公网端口', row.ss_public_port ? String(row.ss_public_port) : '—']] : []),
      ['重装', row.dd === 'manual' ? '人工闸门（在供应商面板重装后继续）' : '不重装'],
      ['凭据', '上架密钥；密码只写，不回显'],
    ];
    const output = [['状态', fmt.step(cell?.status ?? 'queued').word], ...(cell?.attempt > 1 ? [['尝试', `第 ${cell.attempt} 次`]] : []), ...(cell?.detail ? [['说明', clean(cell.detail)]] : [])];
    return { input, output };
  }
  const secretWord = { email: '来自开通邮件 · 已提供', given: '已提供 · 未验证', rejected: '邮件密码被拒', missing: '缺少，需要输入' }[row.secret] ?? '—';
  const input = [
    ['IP', row.ip], ['供应商', `${row.provider} · 服务号 ${row.service_id}`], ['套餐', row.plan],
    ['线路机', row.relay ? `${row.relay}${row.relay_port ? `:${row.relay_port}` : ' · 端口待分配'}` : '—'], ['团队', row.team],
    ['root 密码', row.mode === 'proxy' ? '不需要（代理产品）' : secretWord], ['配方', batch.recipe],
  ];
  const out = {
    P1: [['主机公钥', row.risk ? '2 把（冲突）' : '1 把，一致'], ['出口 IP', row.ip]],
    P3: [['硬件', '2 核 / 4 GiB / 50 GiB'], ['原系统', '供应商镜像 Debian 12']],
    P6: [['expected 公钥', cell?.status === 'succeeded' ? '已出现' : cell?.status === 'failed' ? '未出现' : '等待中'], ['新系统', batch.recipe?.split(' · ')[1] ?? '—']],
    P9: [['SS 加密', 'aes-256-gcm'], ['凭据', '已生成（不展示）']],
    P10: [['资产', row.name]],
    P13: [['入口', row.relay_port ? `${row.relay}:${row.relay_port}` : '待分配'], ['合批', (() => { const g = groupOf(batch, row); return g ? `${g.relay} 批次 #${g.batch_no}` : '—'; })()]],
    P15: [['出口 IP', cell?.status === 'succeeded' ? `${row.ip}（与登记一致）` : '—']],
    P16: [['连接链接', cell?.status === 'succeeded' ? '已生成（未展示）' : '—']],
  }[code] ?? [];
  return { input, output: out };
}
