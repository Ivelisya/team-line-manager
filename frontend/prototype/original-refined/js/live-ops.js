// live-ops.js — 触及线路机的写（类 b）接新后台（fleet/API.md §9.1）：入口启用 / 停用 / 重置流量 / 改备注 / 轮换 UUID、
// 重启 Xray。都是异步的：fleetd 回 202 {op_id, relay, generation, status:'pending'}，线路机的 relayd 取走期望状态、
// 经 Xray API 生效后回执；进度 GET /v2/api/v1/ops/{op_id}（SSE 资源 ops 推送时立即再读）。
// 页面只调 entryOp / relayOp；确认对话框（停用、重置、轮换要输入对象名）留在页面里，与原型一致。
// 同一条提示显示进度：已提交，等待线路机确认 → 已生效（第 N 代）/ 被拒绝（原因）/ 部分生效 / 线路机没有取走（可重试）。
import { toast } from './ui/layers.js';
import { fleetGet, fleetWrite, onFleet } from './fleet.js';

const F = () => globalThis.TEAMOPS?.fleet ?? null;
const enc = encodeURIComponent;
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const DONE = new Set(['applied', 'rejected', 'partial', 'expired']);
export const OP_WORD = { pending: '等待线路机确认', applied: '已生效', rejected: '被线路机拒绝', partial: '部分生效', expired: '线路机没有取走' };

/** 新后台开放了这个能力吗（/system features 或版本；不发探测请求）。 */
export const fleetHas = (feature) => !!F()?.has?.(feature);

/** 入口动作 → fleetd action 名（§9.1）。 */
export const ENTRY_ACTION = { enable: 'enable', disable: 'disable', reset: 'reset_traffic', rename: 'rename', rotate: 'rotate_uuid' };

/**
 * waitOp(op, { onUpdate, timeoutMs }) — 等 op 结束（applied / rejected / partial / expired）。
 * SSE `ops`（path 指向这个 op）来了立即读，否则 1 s、2 s、3 s…（最多 5 s）读一次；超时返回最后一次读到的状态。
 */
export async function waitOp(op, { onUpdate = () => {}, timeoutMs = 120000 } = {}) {
  let cur = op;
  if (!op?.op_id || DONE.has(op.status)) return cur;
  let poke = null;
  const off = onFleet(({ resource, path }) => {
    if (resource === 'ops' && (!path || String(path).endsWith(`/${op.op_id}`))) poke?.();
  });
  const until = Date.now() + timeoutMs;
  try {
    for (let i = 1; Date.now() < until; i++) {
      await Promise.race([sleep(Math.min(5000, 1000 * i)), new Promise((r) => { poke = r; })]);
      poke = null;
      try { cur = (await fleetGet(`ops/${enc(op.op_id)}`, null)) ?? cur; } catch (e) { if (e?.auth) throw e; continue; }
      onUpdate(cur);
      if (DONE.has(cur.status)) return cur;
    }
    return cur;
  } finally { off(); }
}

/** 回执里的第一条错误（relayd results[].error / commands[].error / error）。 */
export function opError(op) {
  if (!op) return '';
  if (op.error) return String(op.error);
  const r = op.result ?? {};
  const bad = [...(r.results ?? []), ...(r.commands ?? [])].find((x) => x && x.ok === false && x.error);
  return bad ? String(bad.error) : r.error ? String(r.error) : '';
}

/**
 * track(app, op, label) — 一条提示从“已提交”走到结果；成功后让轮询器马上重读。返回最终 op（失败 / 超时也返回）。
 */
export async function track(app, op, label) {
  const t = toast({ message: `${label}：已提交，${OP_WORD.pending}…`, tone: 'info', timeout: 0 });
  let fin = op;
  try {
    fin = await waitOp(op, { onUpdate: (o) => { if (!DONE.has(o.status)) t.update({ message: `${label}：${OP_WORD[o.status] ?? o.status}…`, tone: 'info', timeout: 0 }); } });
  } catch (e) {
    t.update({ message: `${label}：${e.message}`, tone: 'bad' });
    return null;
  }
  const gen = fin.generation != null ? `（${fin.relay ?? ''} 第 ${fin.generation} 代配置）` : '';
  if (fin.status === 'applied') { t.update({ message: `${label}：${OP_WORD.applied}${gen}`, tone: 'ok' }); app?.live?.refresh?.({ force: ['fast', 'minute'] }).catch(() => {}); }
  else if (fin.status === 'partial') t.update({ message: `${label}：${OP_WORD.partial}${opError(fin) ? `，${opError(fin)}` : ''}`, tone: 'bad' });
  else if (fin.status === 'rejected') t.update({ message: `${label}：${OP_WORD.rejected}${opError(fin) ? `，${opError(fin)}` : ''}`, tone: 'bad' });
  else if (fin.status === 'expired') t.update({ message: `${label}：${OP_WORD.expired}（60 秒内），线路机的管理代理可能离线；可以重试`, tone: 'bad' });
  else t.update({ message: `${label}：还在${OP_WORD.pending}，稍后在线路页查看结果`, tone: 'info', timeout: 8000 });
  return fin;
}

/** 入口动作的请求：[路径, 请求体]（fleet/API.md §9.1）。 */
export function entryRequest(entry, act, { remark } = {}) {
  const body = { action: ENTRY_ACTION[act] };
  if (act === 'rename') body.remark = remark ?? '';
  if (act === 'rotate') body.confirm = `${entry.relay}:${entry.port}`;
  return [`relays/${enc(entry.relay)}/entries/${enc(entry.port)}/actions`, body];
}
/**
 * entryOp(app, entry, act, { remark, label }) — act: enable | disable | reset | rename | rotate。
 * rotate 带 confirm = "<线路机名>:<端口>"（页面已让用户输入过同一串）。返回最终 op 或 null（请求被拒）。
 */
export async function entryOp(app, entry, act, { remark, label } = {}) {
  const [path, body] = entryRequest(entry, act, { remark });
  let op;
  try { op = await fleetWrite('POST', path, body); }
  catch (e) { if (!e.auth) toast({ message: `${label}：${e.message}`, tone: 'bad' }); return null; }
  return track(app, op, label);
}

/** relayOp(app, relay, 'restart_xray', { label }) — confirm = 线路机名。 */
export async function relayOp(app, relay, action, { label } = {}) {
  let op;
  try { op = await fleetWrite('POST', `relays/${enc(relay.name)}/actions`, { action, confirm: relay.name }); }
  catch (e) { if (!e.auth) toast({ message: `${label}：${e.message}`, tone: 'bad' }); return null; }
  return track(app, op, label);
}

/** 轮换之后的新分享链接（GET …/share，审计 secret.reveal）。{link | url | share} 任一字段。 */
export async function entryShare(entry) {
  const d = await fleetGet(`relays/${enc(entry.relay)}/entries/${enc(entry.port)}/share`, null);
  return d?.link ?? d?.url ?? d?.share ?? (typeof d === 'string' ? d : null);
}
