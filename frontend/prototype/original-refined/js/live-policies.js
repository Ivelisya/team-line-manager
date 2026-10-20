// live-policies.js — 访问规则（节点行为 › 访问规则）的实时数据与写操作。
// 读：新后台开放了 policies（fleet/API.md §9.1）→ GET /v2/api/v1/policies；否则旧接口 /policies（经数据层，只读）。
//     两边形状相同（旧插件的 policy_state）：{ active: {scope: {mode, domains[], note}}, drafts: {scope: {policy, revision}},
//     versions: [{id, ts, status, result}], draft_hash, revision? }；scope = 'team:<团队>' | 'node:<资产 uuid>'。
// 写（只在新后台开放 policies 时）：PUT /policies/draft、POST /policies/preview（读也可以用旧 GET /policies/preview）、
//     POST /policies/apply {draft_hash} → {version_id, ops}（每台线路机一个 op，live-ops.js 等回执）、
//     POST /policies/discard {draft_hash}、POST /policies/rollback {version_id}。
import { fleetGet, fleetWrite, legacyGet } from './fleet.js';
import { toast } from './ui/layers.js';
import { waitOp, opError } from './live-ops.js';

const F = () => globalThis.TEAMOPS?.fleet ?? null;
/** 旧系统下线后：fleetd 原样接手旧的草稿 / 丢弃 / 发布接口（POST /v2/api/v1/legacy-write/policies/*，§12.2） */
const legacyWrite = () => !!F()?.has?.('legacy.write');
export const policiesNative = () => !legacyWrite() && !!F()?.has?.('policies');
export const canRollback = () => !!F()?.has?.('policies.rollback');

/** 规则范围在接口里的键：团队按名字，节点按资产 uuid（旧后台的 asset id）。 */
export function scopeKey(sc) {
  if (sc.team) return `team:${sc.team}`;
  const u = globalThis.TEAMOPS?.assetOf?.(sc.node?.id) ?? null;
  return u ? `node:${u}` : `node:${sc.node?.id}`;
}
/** 域名列表：接口里是数组（旧插件）或多行文本，统一成多行文本给文本框。 */
export const domainsText = (d) => (Array.isArray(d) ? d.join('\n') : String(d ?? ''));
const domainsList = (t) => String(t ?? '').split(/\n+/).map((x) => x.trim()).filter(Boolean);

export async function loadPolicies() {
  const s = policiesNative() ? await fleetGet('policies', null) : await legacyGet('/policies');
  return { active: s?.active ?? {}, drafts: s?.drafts ?? {}, versions: s?.versions ?? [], draft_hash: s?.draft_hash ?? null, revision: s?.revision ?? null };
}
export async function loadPreview(state) {
  if (policiesNative()) return fleetWrite('POST', 'policies/preview', state?.draft_hash ? { draft_hash: state.draft_hash } : {});
  return legacyGet('/policies/preview');
}
export function saveDraft(state, key, { mode, domains, note }) {
  const rev = state?.drafts?.[key]?.revision ?? 0;
  if (legacyWrite()) return fleetWrite('POST', 'legacy-write/policies/draft', { scope: key, expected_revision: rev, policy: { mode, domains: domainsText(domains), note: note ?? '' } });
  return fleetWrite('PUT', 'policies/draft', { scope: key, policy: { mode, domains: domainsList(domains), note: note ?? '' }, base_rev: state?.revision ?? rev, expected_revision: rev });
}
export const discardDrafts = (state) => fleetWrite('POST', legacyWrite() ? 'legacy-write/policies/discard' : 'policies/discard', { draft_hash: state?.draft_hash ?? null });
export const applyDrafts = (preview) => fleetWrite('POST', legacyWrite() ? 'legacy-write/policies/apply' : 'policies/apply', { draft_hash: preview?.draft_hash ?? null, ...(preview?.routing_hashes ? { routing_hashes: preview.routing_hashes } : {}) });
export const rollbackTo = (versionId) => fleetWrite('POST', 'policies/rollback', { version_id: versionId });

/**
 * trackOps(app, ops, label) — 一条提示跟踪多台线路机的回执：“应用到 3 台线路机：1/3 已生效…” → 全部生效 / 有失败（列出线路机与原因）。
 */
export async function trackOps(app, ops, label) {
  const list = (ops ?? []).filter((o) => o?.op_id);
  const n = list.length;
  if (!n) { toast({ message: `${label}：已提交` }); app?.live?.refresh?.({ force: ['minute'] }).catch(() => {}); return []; }
  const t = toast({ message: `${label}：已提交到 ${n} 台线路机，等待确认…`, tone: 'info', timeout: 0 });
  const done = new Map();
  const paint = () => t.update({ message: `${label}：${[...done.values()].filter((o) => o.status === 'applied').length}/${n} 台线路机已生效…`, tone: 'info', timeout: 0 });
  const fin = await Promise.all(list.map((o) => waitOp(o, { onUpdate: (x) => { done.set(o.op_id, x); paint(); } }).then((x) => { done.set(o.op_id, x); paint(); return x; }, () => o)));
  const bad = fin.filter((o) => o.status !== 'applied');
  if (!bad.length) { t.update({ message: `${label}：${n} 台线路机全部生效`, tone: 'ok' }); app?.live?.refresh?.({ force: ['minute'] }).catch(() => {}); }
  else t.update({ message: `${label}：${n - bad.length}/${n} 台生效；${bad.map((o) => `${o.relay ?? '线路机'} ${o.status === 'pending' ? '仍在等待' : opError(o) || o.status}`).join('；')}`, tone: 'bad' });
  return fin;
}
