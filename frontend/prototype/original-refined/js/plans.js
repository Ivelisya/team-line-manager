// plans.js — SPEC §7：套餐只用规范名称。
// 加载 data/plan-catalog.json，按 by_node_id → by_asset → 推导别名表 的顺序解析每个节点的套餐，
// 然后就地替换：n.plan_id、n.plan（目录对象）、n.billing.plan = 规范名；relays[].plan、流水线演示行同样替换。
// 替换后 shared/data.js 的套餐列、套餐 facet、plan: 搜索 token、搜索 haystack 自动都是规范名，
// 供应商原文从内存里消失，不可能被渲染出来。
// 目录加载失败（兜底）：保留原文，plan.raw = true，界面在名称后加 `原名` 标记（--warn-text），控制台警告一次。

import { fmt } from '../../shared/index.js';

const CATALOG_URL = new URL('../../data/plan-catalog.json', import.meta.url);

let catalog = null;
let byId = new Map();
let fallback = false;
const aliasMap = new Map(); // provider|供应商原文 → plan id（applyPlans 建立；resolvePlan 用；不显示）
const rawNames = new Map(); // 供应商原文 → 规范名（只在内存里用于 scrubPlans，不显示）
const UNRESOLVED = { id: 'unresolved', name: '未归类套餐', unresolved: true, specs: {}, pending: [] };

/** 规格摘要里去掉供应商原文括注（如“（原文「50M」）”），原文一律不显示。 */
/** 目录文字里的内部备注（核实说明、补截图说明、Qn 问题）不上屏。 */
export function stripInternal(s) {
  if (/^\s*(?:\u5f85\u6838|\u5f85\u8865|\u5f85\u786e\u8ba4)/.test(String(s ?? ''))) return ''; // 整条就是核实备注
  return String(s ?? '')
    .replace(/[（(][^）)]*(?:\u5f85\u6838|\u5f85\u8865|\u5f85\u786e\u8ba4|截图|Q\d)[^）)]*[）)]/g, '')
    .replace(/(^|[；;，,])\s*[^；;，,]*(?:\u5f85\u6838|\u5f85\u8865|\u5f85\u786e\u8ba4)[^；;，,]*/g, '$1')
    .replace(/^[；;，,\s]+|[；;，,\s]+$/g, '')
    .replace(/([；;，,])\s*(?=[；;，,])/g, '')
    .replace(/(\S)\/ /g, '$1 / ');
}
function cleanSummary(s) {
  return stripInternal(s)
    .replace(/（原文[^）]*）/g, '')
    .replace(/出站/g, '发送')          // 网卡方向词统一为 接收 / 发送（§4.3 D-02）
    .replace(/\s{2,}/g, ' ').trim();
}

/**
 * 加载目录并就地替换 store 里的套餐字段。返回 { catalog, fallback }。
 * 在 loadSnapshot() 之后、首屏渲染之前调用一次。
 */
export async function applyPlans(store, { url = CATALOG_URL } = {}) {
  // 本机缓存的目录（上次读到的）→ 立即套用，不等网络；同时在后台取新的存起来，下次打开用
  const KEY = 'teamops:catalog:v1';
  let cached = null;
  try { cached = JSON.parse(globalThis.localStorage?.getItem(KEY) ?? 'null'); } catch { cached = null; }
  const fetchCatalog = async () => {
    // 套餐目录是静态文件：5 秒读不到就退回原始套餐字符串，不挡首屏
    const ctl = typeof AbortController === 'function' ? new AbortController() : null;
    const t = ctl ? setTimeout(() => ctl.abort(), 5000) : null;
    let res;
    try { res = await fetch(url, ctl ? { signal: ctl.signal } : {}); } finally { clearTimeout(t); }
    if (!res.ok) throw new Error(`HTTP ${res.status}`);
    const j = await res.json();
    try { globalThis.localStorage?.setItem(KEY, JSON.stringify(j)); } catch { /* optional */ }
    return j;
  };
  try {
    if (cached?.plans && globalThis.TEAMOPS_LIVE === true) { catalog = cached; fetchCatalog().catch(() => {}); }
    else catalog = await fetchCatalog();
  } catch (e) {
    catalog = null;
    fallback = true;
    console.warn('[plans] 套餐目录加载失败，退回显示原始套餐字符串：', e.message);
  }
  if (catalog) {
    byId = new Map(catalog.plans.map((p) => [p.id, {
      ...p,
      specs: { ...Object.fromEntries(Object.entries(p.specs ?? {}).map(([k, v]) => [k, typeof v === 'string' ? stripInternal(v) : v])), summary: cleanSummary(p.specs?.summary) },
      // 目录里的内部待办（questions / pending / 核实备注）只给维护目录的人看，界面从不显示
      questions: [], pending: [],
    }]));
    // 目录里的供应商原文（不显示），给 scrubPlans 用：审计详情、备注这类自由文本里出现时换成规范名
    for (const p of catalog.plans) for (const s of [p.supplier_plan_name, p.product_line]) if (s) rawNames.set(s, p.name);
  }
  // 1) 节点：by_node_id → by_asset
  const alias = aliasMap; // provider|原文 → plan id（只在内存里用于匹配，不显示）
  alias.clear();
  for (const n of store.nodes()) {
    const raw = n.billing?.plan ?? null;
    let pid = catalog?.by_node_id?.[n.id] ?? catalog?.by_asset?.[n.name] ?? null;
    if (pid && raw) alias.set(`${n.provider}|${raw}`, pid);
    assign(n, pid, raw);
  }
  // 2) 线路机实体（billing 与节点行共享，已替换；这里只替换 relays[].plan 原文）
  for (const r of store.relays()) {
    const n = store.node(r.id);
    r.plan = n?.plan?.name ?? r.plan;
    r.plan_id = n?.plan_id ?? null;
  }
  // 3) 流水线演示行：provider + 原文 → 推导别名表
  const ob = store.raw.onboarding?.demo_batch;
  for (const row of ob?.rows ?? []) {
    const pid = alias.get(`${row.provider}|${row.plan}`) ?? null;
    const p = pid ? byId.get(pid) : null;
    if (p) { row.plan_id = p.id; row.plan = p.name; }
    else if (!fallback) { row.plan_id = UNRESOLVED.id; row.plan = UNRESOLVED.name; }
  }
  for (const h of store.raw.onboarding?.history ?? []) {
    if (h.plan) { const pid = alias.get(`${h.provider}|${h.plan}`); if (pid) h.plan = byId.get(pid).name; }
  }
  // 4) 搜索 haystack 用规范名重建（data.js 在建 store 时已经用原文算过一次）
  for (const n of store.nodes()) {
    n.haystack = [
      n.name, n.full_name, n.id, n.number != null ? fmt.pad2(n.number) : '', n.ip, n.ipv6, n.team, n.relay, n.entry_label,
      n.provider, n.provider_code, n.region?.code, n.region?.name, n.billing?.service_id, n.plan?.name, n.plan_id,
      n.connection?.client_label, ...(n.tags ?? []),
    ].filter(Boolean).join(' ').toLowerCase();
  }
  return { catalog, fallback };
}

function assign(n, pid, raw) {
  let p = pid ? byId.get(pid) : null;
  if (!p && fallback && raw) p = { id: `raw:${raw}`, name: raw, raw: true, specs: {}, pending: [], questions: [] };
  if (!p) p = UNRESOLVED;
  if (raw && !p.raw && !p.unresolved) rawNames.set(raw, p.name);
  n.plan_id = p.id;
  n.plan = p;
  if (n.billing) n.billing.plan = p.name;
}

/** planOf(node) → 目录对象 { id, name, provider, specs, price_cents, cycle_word, pending, questions, raw?, unresolved? } */
export function planOf(n) { return n?.plan ?? UNRESOLVED; }
/** 目录里的全部套餐（目录顺序）。兜底模式下按节点原文聚合。 */
export function allPlans(store) {
  if (catalog) return [...byId.values()];
  const seen = new Map();
  for (const n of store.nodes()) if (n.plan && !seen.has(n.plan_id)) seen.set(n.plan_id, n.plan);
  return [...seen.values()];
}
export function planById(id) { return byId.get(id) ?? null; }

/**
 * resolvePlan(provider, text) → 目录对象或 null（上架流水线“新建批次”把录入的套餐文字解析成规范套餐）。
 * text 可以是套餐 id、规范名，或供应商原文（经推导别名表匹配，原文只在这里用于匹配、不显示）。
 * text 为空时取该供应商在机队里最常见的套餐（ATTVPS → ATTVPS 基础版），并在结果上标 inferred: true。
 */
export function resolvePlan(provider, text = '') {
  const t = String(text ?? '').trim();
  if (t) {
    if (byId.has(t)) return byId.get(t);
    const low = t.toLowerCase();
    for (const p of byId.values()) if (p.name.toLowerCase() === low) return p;
    const pid = aliasMap.get(`${provider}|${t}`);
    if (pid) return byId.get(pid) ?? null;
    for (const [k, id] of aliasMap) if (k.toLowerCase() === `${String(provider).toLowerCase()}|${low}`) return byId.get(id) ?? null;
    return null;
  }
  const count = new Map();
  for (const [k, id] of aliasMap) if (k.split('|')[0] === provider) count.set(id, (count.get(id) ?? 0) + (byId.get(id)?.member_count ?? 1));
  const best = [...count].sort((a, b) => b[1] - a[1])[0]?.[0];
  return best ? { ...byId.get(best), inferred: true } : null;
}
export const plansFallback = () => fallback;

/**
 * 节点“流量上限”字段（SPEC §7.3）：节点自身配置的上限优先；否则 traffic_unlimited 为真 → 不限；
 * 目录里没有定论 → 未设置（中性缺省；目录的内部待办问题从不上屏）；无目录 → 未设置。返回 Fmt。
 */
export function quotaLimit(n) {
  const q = n.traffic?.quota;
  if (q?.limit_bytes) return fmt.bytes(q.limit_bytes);
  const p = planOf(n);
  if (p.specs?.traffic_unlimited === true) return fmt.unlimited();
  return fmt.unset();
}

/** 同套餐台数（按当前节点集合）。 */
export function membersOf(store, planId) { return store.nodes().filter((n) => n.plan_id === planId); }

/** 套餐弹层 / 筛选 chip tooltip 的一句话：规格摘要；价格。 */
export function planLine(p) {
  if (!p || p.unresolved) return '套餐目录里没有这个节点';
  const price = p.price_cents != null ? fmt.money(p.price_cents, { currency: p.currency, perMonth: true }).text : '—';
  return `${p.specs?.summary || '—'} · ${price}`;
}

/**
 * 自由文本里的套餐原文换成规范名（SPEC §7：原始套餐字符串任何地方都不显示）。
 * 用在审计详情、资产备注这类原样透传的文本：`来源：ohno.icu 账单「[ACA] Acapulco, …」` → `…「OhnoICU NAT Small」`。
 * 原文本身就是规范名一部分的（`高速代理` ⊂ `NodeMach 高速代理`）不替换，避免重复；长的先换。
 */
let scrubRe = null, scrubKey = 0;
export function scrubPlans(text) {
  if (text == null || !rawNames.size) return text;
  if (!scrubRe || scrubKey !== rawNames.size) {
    const keys = [...rawNames.keys()].filter((k) => !rawNames.get(k).includes(k)).sort((a, b) => b.length - a.length);
    scrubRe = keys.length ? new RegExp(keys.map((k) => k.replace(/[.*+?^${}()|[\]\\]/g, '\\$&')).join('|'), 'g') : null;
    scrubKey = rawNames.size;
  }
  return scrubRe ? String(text).replace(scrubRe, (m) => rawNames.get(m)) : text;
}
