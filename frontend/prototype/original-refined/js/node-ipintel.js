// node-ipintel.js — 节点抽屉里的 IP 情报（SPEC §6.3 抽屉；数据见 data/README.md “IP 情报”）：
//   · 出口画像（连接质量页签）：snapshot nodes[].ipintel.egress —— 出口 IP、运营商 / ASN、接入方式与置信度、
//     主地点与两库分歧、IPv6、观测次数与来源；detail.json 的 egress_history.ips 列出每个观测到的出口 IP。
//   · 来源画像（访问记录页签）：snapshot nodes[].ipintel.summary / breakdown（24 小时 / 14 天切换）+ 相关的 IP 情报提醒；
//     detail.json 的 ipintel.sources 逐个来源 IP，情报字段按 ip 到 snapshot ipintel.sources 里查。
// 只组合现有组件（dz-card、kv、meter、pill、mark、copyable、note、dataTable 的 .dt 外形），不写新样式；
// 来源 IP 单元格用行为页同一种写法（.src + 次行 .src-line：运营商 · 城市 · 接入方式）。

import { fmt } from '../../shared/index.js';
import { esc, num, txt } from './ui/dom.js';
import { tonePill, copyable, mark, infoButton, segmented, onSegment, disclosure, tagChip, hydrate } from './ui/controls.js';
import { kv, dlRows, note, empty, skel } from './ui/layout.js';
import { meter } from './ui/viz.js';
import { intelOf, sourceCell, sourceLineHtml } from './ui/intel.js';

const dictOf = (store) => store.raw?.ipintel?.dict ?? {};
const CONN_SHORT = { mobile: '移动网络', broadband: '固定宽带', datacenter: '云机房', hosting: 'IDC', education: '教育科研网', private: '内网地址', unknown: '未知' };
const connWord = (store, t) => dictOf(store).conn_type_short?.[t] ?? CONN_SHORT[t] ?? (t || '—');
const confWord = (store, c) => dictOf(store).confidence?.[c] ?? c ?? '—';
const flagWord = (store, f) => dictOf(store).flag?.[f] ?? f;

/** 连接占比：1 位小数；不到 100 的不显示成 100（>99.9%），极小值 <0.1%。 */
function share(v) {
  if (v == null || !Number.isFinite(v)) return fmt.missing();
  if (v >= 100) return new fmt.Fmt('100%', { num: '100', unit: '%', raw: v });
  if (v > 99.9) return new fmt.Fmt('>99.9%', { prefix: '>', num: '99.9', unit: '%', raw: v, exact: `${fmt.num(v, 2)}%` });
  return fmt.pctLoss(v);
}
const card = (title, body, tools = '', attrs = '') => `<section class="dz-card"${attrs}><div class="dz-head"><h3 class="dz-title">${esc(title)}</h3><div class="dz-tools">${tools}</div></div>${body}</section>`;
const kvSub = (html) => `<span class="kv-sub">${html}</span>`;
const place = (...parts) => parts.filter(Boolean).join(' · ');

// ─── 出口画像 ───────────────────────────────────────────────────────────────

/** 出口画像卡（连接质量页签，放在“连接路径”后面）。egress 历史表由 fillEgressIps 在 detail.json 到了之后补上。 */
export function egressCard(store, n) {
  const e = n.ipintel?.egress;
  if (!e) return card('出口画像', empty({ icon: 'globe', text: '没有出口 IP 记录' }));
  const h = e.history ?? {};
  const changes = h.changes ?? [];
  // 出口只看连通（fleetd 0.13.3）：IP 换过只作信息，不是告警
  const pill = changes.length ? tonePill(`出口 IP 换过 ${changes.length} 次`, 'neutral', { dot: 'none' }) : tonePill('出口 IP 稳定', 'ok');
  const L = n.latency ?? {};
  const ledger = L.egress_expected_ip;
  const ledgerSub = ledger && ledger !== e.ip ? `台账记录 ${esc(ledger)}（仅供参考）` : '';
  const r2 = e.dbs?.ip2region ?? {};
  const alt = place(r2.region_zh ?? r2.region, r2.city_zh ?? r2.city);
  const geoConflict = (e.flags ?? []).includes('geo_conflict');
  const basis = dictOf(store).geo_basis?.[e.geo_basis] ?? e.geo_basis ?? '';
  const placeSub = geoConflict && alt ? `以 DB-IP 为准；ip2region 记为${esc(alt)}` : esc(basis ? `依据 ${basis}` : '');
  const via = Object.entries(h.via ?? {}).map(([k, v]) => `${k} ${v}`).join(' · ');
  const items = [
    { label: '出口 IP', value: `${copyable(e.ip)}${kvSub(ledgerSub)}` },
    { label: '运营商', value: `${esc(e.isp ?? '—')}${kvSub(esc(place(e.asn ? `AS${e.asn}` : null, e.as_handle)))}`, tip: e.as_org },
    { label: '接入方式', value: `${esc(connWord(store, e.conn_type))}${e.residential === true ? ' <span class="t-secondary">住宅</span>' : e.residential === false ? ' <span class="t-secondary">机房</span>' : ''}${kvSub(`置信度 ${esc(confWord(store, e.conn_conf))}`)}`, tip: e.conn_basis },
    { label: 'IPv6', value: e.v6?.ip ? `${copyable(e.v6.ip, { display: fmt.ip(e.v6.ip).text })}${kvSub(esc(place(e.v6.isp, e.v6.asn ? `AS${e.v6.asn}` : null)))}` : '<span class="t-muted">无</span>' },
    { label: '观测', value: `${num(fmt.int(h.observations))} <span class="t-secondary">次</span>${kvSub(esc(h.first_seen ? `${fmt.stamp(h.first_seen).text} 起 · 最近 ${fmt.ago(h.last_seen).text}` : '—'))}`, tip: via ? `记录来源：${via}` : undefined },
  ];
  const where = [{ label: '位置', value: `${esc(place(e.country_name, e.region, e.city) || '—')}${geoConflict ? ' ' + tonePill('两库不一致', 'warn', { dot: 'none', title: `ip2region：${alt || '—'}；DB-IP：${place(e.dbs?.dbip?.region_zh, e.dbs?.dbip?.city_zh) || '—'}` }) : ''}${kvSub(placeSub)}` }];
  const changeNotes = changes.map((c) => note({
    tone: 'warn',
    title: `${fmt.stamp(c.at).text} 出口 IP 更换`,
    body: esc(`${c.from} → ${c.to}${c.same_asn ? ' · 同一 ASN' : ' · ASN 不同'}${c.same_place ? ' · 同一地点' : ''}${c.evidence?.length ? ` · 证据：上架任务 ${c.evidence.map((x) => `${x.job} ${x.status_word ?? ''}`.trim()).join('、')}` : ''}`),
  })).join('');
  const info = infoButton('出口 IP 由监控、上架任务与主机探针观测；运营商、ASN、位置来自离线库（ip2region、DB-IP Lite、iptoasn），IP 没有发往任何在线服务。中国地址以 ip2region 为准，境外地址以 DB-IP 为准；住宅 / 机房按 ASN 判断。');
  return card('出口画像', `<div class="dz">
      <div class="card-readings">${items.map((it) => kv([it], { cols: 1 })).join('')}</div>
      ${kv(where, { cols: 1 })}
      ${changeNotes}
      <div class="segment" data-egress-ips="${esc(n.id)}"><div class="segment-head"><span>观测到的出口 IP</span><span class="segment-meta">${esc(`${h.distinct_ips ?? 1} 个`)}</span></div>${skel('60%', 12)}</div>
    </div>`, `${pill}${info}`);
}

/** detail.json 到了之后：每个观测到的出口 IP 一行（首末时间、次数、上架任务、是否与台账一致）。 */
export function fillEgressIps(root, d, id) {
  const box = root.querySelector('[data-egress-ips]');
  if (!box || (id && box.dataset.egressIps !== id)) return; // 已切到别的节点：丢弃迟到的结果
  const head = box.querySelector('.segment-head')?.outerHTML ?? '';
  const ips = d?.ipintel?.egress_history?.ips ?? [];
  if (!ips.length) { box.innerHTML = `${head}<div class="muted">没有观测记录</div>`; return; }
  const rows = ips.slice().sort((a, b) => (b.last_seen ?? 0) - (a.last_seen ?? 0)).map((x) => ({
    label: x.ip,
    value: `${esc(`${fmt.stamp(x.first_seen).text} – ${fmt.ago(x.last_seen).text} · ${fmt.int(x.observations).text} 次${x.jobs?.length ? ` · 上架任务 ${x.jobs.map((j) => j.status_word ?? j.status).join('、')}` : ''}`)}`,
    tip: Object.entries(x.via ?? {}).map(([k, v]) => `${k} ${v}`).join(' · '),
  }));
  box.innerHTML = `${head}${dlRows(rows)}`;
}

// ─── 来源画像 ───────────────────────────────────────────────────────────────

const RANGES = [{ value: 'h24', label: '24 小时' }, { value: 'd14', label: '14 天' }];

// 实时：来源画像来自新后台（节点详情 detail.ipintel，抽屉打开后才读），到之前显示骨架而不是“没有来源连接”
const liveI = new Map(); // node id → detail.ipintel
const intelOfNode = (n) => n.ipintel ?? liveI.get(n.id) ?? null;
function sourcesSummary(store, n, range) {
  const I = intelOfNode(n) ?? {};
  if (!intelOfNode(n) && globalThis.TEAMOPS_LIVE === true) return `<div class="stack-sm">${skel('60%', 14)}${skel('100%', 12)}${skel('80%', 12)}</div>`;
  const S = I.summary ?? {};
  const B = I.breakdown?.[range] ?? {};
  const T = B.total ?? {};
  const w = range === 'h24' ? '24 小时' : '14 天';
  if (!T.connections) {
    return empty({ icon: 'radar', text: `近 ${w}没有来源连接${range === 'h24' && S.sources?.d14 ? `；14 天内有 ${S.sources.d14} 个来源，切到“14 天”查看` : ''}` });
  }
  const top = (k) => S[k]?.[range];
  const readings = `<div class="card-readings">
    ${kv([{ label: '来源 IP', value: `${num(fmt.int(S.sources?.[range]))} <span class="unit">个</span>${kvSub(esc(range === 'h24' ? `新增 ${S.new_sources_h24 ?? 0} · 14 天共 ${S.sources?.d14 ?? 0} 个` : `连接 ${fmt.int(T.connections).text} 次 · 会话 ${fmt.int(T.sessions).text}`))}` }], { cols: 1, big: true })}
    ${kv([{ label: '首位运营商', value: `${esc(top('top_carrier')?.k ?? '—')}${kvSub(`连接占比 ${esc(share(top('top_carrier')?.pct).text)}`)}` }], { cols: 1, big: true })}
    ${kv([{ label: '首位地区', value: `${esc(top('top_city')?.k ?? '—')}${kvSub(`连接占比 ${esc(share(top('top_city')?.pct).text)} · 移动网络 ${esc(share(S.mobile_pct?.[range]).text)}`)}` }], { cols: 1, big: true })}
  </div>`;
  const dist = (title, rows, label = (r) => r.k) => `<div><div class="dz-group-title">${esc(title)}</div><div class="stack-sm">${(rows ?? []).slice(0, 3).map((r) => meter({
    label: String(label(r)), value: share(r.pct), pct: r.pct,
    sub: esc(`${fmt.int(r.c).text} 次连接 · ${fmt.int(r.s).text} 个来源`),
  })).join('') || '<span class="t-muted">—</span>'}${(rows?.length ?? 0) > 3 ? `<div class="muted">另有 ${rows.length - 3} 项</div>` : ''}</div></div>`;
  const dists = `<div class="segment"><div class="dz-groups">
    ${dist('运营商', B.carrier)}
    ${dist('地区', B.city ?? B.province)}
    ${dist('接入方式', B.conn_type, (r) => connWord(store, r.k))}
  </div></div>`;
  const flags = (S.flags ?? []).length ? `<div class="chip-wrap">${S.flags.map((f) => tagChip(flagWord(store, f))).join('')}</div>` : '';
  const all = store.raw?.ipintel?.anomalies ?? [];
  const anomalies = (S.anomaly_ids ?? []).map((id) => all.find((a) => a.id === id)).filter(Boolean);
  const sev = (s) => dictOf(store).severity?.[s] ?? s;
  const anomalyList = anomalies.length ? `<div class="segment">${disclosure({
    title: 'IP 情报提醒', meta: (() => { const w = anomalies.filter((a) => a.severity === 'warn').length; return `${anomalies.length} 条${w ? ` · 其中提醒 ${w}` : ''}`; })(),
    body: dlRows(anomalies.map((a) => ({ label: `${sev(a.severity)} · ${fmt.stamp(a.ts).text}`, value: esc(a.title), tip: a.title }))),
  })}</div>` : '';
  return `<div class="dz">${readings}${flags}${dists}${anomalyList}</div>`;
}

/** 来源画像卡（访问记录页签）：头部 24 小时 / 14 天小号分段；切换只重画卡体。 */
export function sourcesCard(store, n, range = 'h24') {
  const tools = `${segmented({ name: 'src-range', size: 'sm', items: RANGES, value: range, label: '时间范围' })}${infoButton('来源 = 连到这个节点入口的客户端 IP（Xray 访问日志）。运营商、地区、接入方式来自离线库；中国三大运营商的骨干网 ASN 家宽、手机混用，接入方式只能给低到中置信度。14 天窗口从 09-14 23:35 活动表开始记录起算，实际约 8.7 天。')}`;
  return card('来源画像', `<div data-src-body>${sourcesSummary(store, n, range)}</div>`, tools, ' data-src-card');
}

/** 来源 IP 表占位（flush 卡），detail.json 到了之后由 fillSourcesTable 填。 */
export function sourcesTableCard(n) {
  return `<section class="dz-card-flush" data-src-table="${esc(n.id)}"><div class="panel-head"><h3 class="panel-title">来源 IP</h3><span class="panel-sub" data-src-count></span><div class="panel-tools"></div></div><div class="panel-body">${skel('100%', 12)}</div></section>`;
}

export function fillSourcesTable(root, store, n, d) {
  const box = root.querySelector('[data-src-table]');
  if (!box || box.dataset.srcTable !== n.id) return; // 已切到别的节点：丢弃迟到的结果
  if (d?.ipintel && !n.ipintel) {
    // 实时：画像 + 逐个来源的情报（让 intelOf 在全站都能查到这些 IP），然后重画来源画像卡
    liveI.set(n.id, d.ipintel);
    const raw = store.raw ?? null;
    if (raw && d.ipintel.intel?.length) {
      const old = raw.ipintel?.sources ?? [];
      const seen = new Set(d.ipintel.intel.map((x) => x.ip));
      raw.ipintel = { ...(raw.ipintel ?? {}), sources: [...old.filter((x) => !seen.has(x.ip)), ...d.ipintel.intel] };
    }
    const c = root.querySelector('[data-src-card] [data-src-body]');
    if (c) { const range = root.querySelector('[data-src-card] [data-seg="src-range"] [aria-checked="true"], [data-src-card] [data-seg="src-range"] [aria-selected="true"]')?.dataset.value ?? 'h24'; c.innerHTML = sourcesSummary(store, n, range); hydrate(c); }
  }
  if (!d?.ipintel && !n.ipintel && globalThis.TEAMOPS_LIVE === true) return; // 详情还没补全：保留骨架，补全后抽屉重画
  const list = (d?.ipintel?.sources ?? []).slice().sort((a, b) => (b.connections?.h24 ?? 0) - (a.connections?.h24 ?? 0) || (b.last_seen ?? 0) - (a.last_seen ?? 0));
  const count = box.querySelector('[data-src-count]');
  if (count) count.textContent = list.length ? `14 天 ${list.length} 个 · 24 小时 ${list.filter((s) => s.connections?.h24).length} 个` : '';
  const body = box.querySelector('[data-src-list]') ?? box.querySelector('.panel-body');
  if (!body) return;
  if (!list.length) { body.outerHTML = `<div data-src-list>${empty({ icon: 'radar', text: '近 14 天没有来源连接' })}</div>`; return; }
  const rows = list.map((s) => {
    const x = intelOf(store, s.ip);
    const pending = (s.new_source_events ?? []).filter((ev) => !ev.acknowledged);
    const state = pending.length
      ? tonePill('新来源 · 未确认', 'warn', { title: pending[0].message })
      : s.blocked_d14 ? tonePill(`已阻止 ${s.blocked_d14}`, 'bad') : (s.connections?.h24 ? tonePill('活跃', 'ok') : tonePill('近 24 小时无连接', 'neutral', { dot: 'ring' }));
    return `<tr>
      <td>${sourceCell(s.ip, x, { relayName: (id) => store.node(id)?.name })}<span class="sub">${sourceLineHtml(x, { ip: s.ip }) || '—'}</span></td>
      <td class="nowrap">${txt(fmt.ago(s.last_seen))}<span class="sub">首次 ${esc(fmt.stamp(s.first_seen).text)}</span></td>
      <td class="r v">${num(fmt.int(s.connections?.h24))}<span class="sub">14 天 ${esc(fmt.int(s.connections?.d14).text)}</span></td>
      <td>${state}${s.clients?.length ? `<span class="sub">${esc(s.clients.join('、'))}</span>` : ''}</td>
    </tr>`;
  }).join('');
  body.outerHTML = `<div data-src-list><div class="table-wrap"><table class="dt is-two" aria-label="来源 IP"><thead><tr><th>来源 IP</th><th>最近 / 首次</th><th class="r">连接 24 小时</th><th>状态 / 客户端</th></tr></thead><tbody>${rows}</tbody></table></div></div>`;
}

/** 渲染后接线：来源画像的时间范围切换（监听挂在卡片自己身上，页签重画时随卡片一起回收）。 */
export function bindIpintel(root, store, n) {
  const c = root.querySelector('[data-src-card]');
  if (!c) return;
  onSegment(c, 'src-range', (v) => {
    const b = c.querySelector('[data-src-body]');
    if (b) { b.innerHTML = sourcesSummary(store, n, v); hydrate(b); }
  });
}

/**
 * 来源数据超时（新后台 8 秒没给）：用这个节点最近的访问记录顶上（来源 IP 与次数），说明来源并给“重试”；
 * 画像卡写“来源画像读取超时”。记下是哪个请求慢，便于排查。
 */
export function sourcesTimeout(root, store, n, retry) {
  const card = root.querySelector('[data-src-card] [data-src-body]');
  const box = root.querySelector('[data-src-table]');
  const stillSkel = (el) => el && el.querySelector('.skel');
  if (!stillSkel(card) && !stillSkel(box)) return;
  const pending = [...(globalThis.TEAMOPS?.source?.inflight?.keys?.() ?? [])].filter((u) => /\/sources|\/node\?|\/behavior\?|\/activity\?/.test(u));
  console.warn('[来源画像] 8 秒没有读到来源数据', n.id, pending);
  const rows = store.access(n.id);
  const by = new Map();
  for (const r of rows) { if (!r.source_ip) continue; const x = by.get(r.source_ip) ?? { ip: r.source_ip, first_seen: r.ts, last_seen: r.ts, connections: { h24: 0, d14: 0 }, clients: [], new_source_events: [] }; x.connections.h24++; x.connections.d14++; x.first_seen = Math.min(x.first_seen, r.ts); x.last_seen = Math.max(x.last_seen, r.ts); if (r.client && !x.clients.includes(r.client)) x.clients.push(r.client); by.set(r.source_ip, x); }
  const again = `<button type="button" class="btn btn-secondary btn-sm" data-src-retry>重试</button>`;
  if (stillSkel(card)) card.innerHTML = note({ tone: 'warn', title: '来源画像读取超时', body: esc('新后台 8 秒内没有返回这个节点的来源统计；下面先列出最近访问记录里的来源 IP。'), action: again });
  if (stillSkel(box)) {
    const fake = { ipintel: { sources: [...by.values()] } };
    liveI.delete(n.id);
    const count = box.querySelector('[data-src-count]');
    fillSourcesTable(root, store, { ...n, ipintel: { summary: {}, breakdown: {} } }, fake);
    if (count) count.textContent = by.size ? `最近访问记录里 ${by.size} 个` : '';
  }
  root.querySelector('[data-src-retry]')?.addEventListener('click', (e) => { e.currentTarget.disabled = true; retry?.(); });
  hydrate(root);
}
