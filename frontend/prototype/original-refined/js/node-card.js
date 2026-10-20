// node-card.js — 节点卡片 NodeCard（SPEC §3.12）：一套外壳、三种视图（large 大卡片 / small 小卡片 / compact 紧凑）、
// 三种节点（主机 / 家宽代理 / 线路机）。行序与原版一致（ID-09）；所有动态值放在 data-s 槽位里，实时更新只改变化的槽位。

import { fmt, createPatcher } from '../../shared/index.js';
import { icon } from './icons.js';
import { esc, num, flag, attrs } from './ui/dom.js';
import { iconButton, button, statusPill, tagChip, priceChip, bwChip, mark, copyable, neutralChip, moreChip, dot } from './ui/controls.js';
import { meter, patchMeter, quota, patchQuota, latencyStrip, patchBeat } from './ui/viz.js';
import * as M from './node-model.js';
import { planOf } from './plans.js';

const patcher = createPatcher();

// ─── 动态槽位（首屏渲染与实时补丁用同一份值，保证一致） ──────────────────

function slotValues(store, n, view) {
  const s = {};
  const h = M.healthOf(store, n);
  s.pill = statusPill(h);
  s.util = bwChip(M.bandwidthChipModel(n));
  s.offline = esc(M.offlineText(n));
  if (view === 'large') {
    s.fquota = num(M.quotaModel(n).limit);
    // 家宽代理没有主机（没有运行时长），这一格放原代理卡的“上游 TCP”，信息不减
    s.up = n.hasHost ? runVal(M.uptimeF(n)) : num(M.latencyF(n, 'tcp'), { title: false });
    s.due = M.dueHtml(n);
  }
  if (!n.hasHost && view !== 'large') {
    s.tcp = num(M.latencyF(n, 'tcp'), { title: false });
    s.egr = num(M.latencyF(n, 'egress'), { title: false });
    s.md = num(fmt.bytes(n.traffic?.month?.down)); s.mu = num(fmt.bytes(n.traffic?.month?.up));
  }
  if (n.hasHost) {
    const x = M.nic(n);
    s.rx = rateHtml(x.rx); s.tx = rateHtml(x.tx); s.rxT = num(x.rxT); s.txT = num(x.txT);
  } else {
    const e = M.entryTraffic(n);
    e.rows.forEach(([, d, u], i) => { s[`ed${i}`] = num(d); s[`eu${i}`] = num(u); });
    s.esrc = esc(e.src);
  }
  const q = M.quotaModel(n);
  s.qrem = q.rem ? `剩余 ${num(q.rem)}` : '';
  s.quse = `<b>${num(q.used)}</b> / ${num(q.limit)}`;
  if (view === 'small') {
    s.cdue = M.dueHtml(n);
    s.up2 = M.uptimeF(n).kind === 'na' ? '' : `持续运行 ${num(M.uptimeF(n))}`;
    s.qlim = num(q.limit);
  }
  return s;
}
const rateHtml = (f) => num(f);
const runVal = (f) => (f.kind === 'na' ? `<span class="t-muted txt">${esc(f.text)}</span>` : num(f, { title: false }));
const slot = (key, html, cls = 'lv') => `<span class="${cls}" data-s="${key}">${html ?? ''}</span>`;

// ─── 片段 ───────────────────────────────────────────────────────────────────

function head(store, n, view, s) {
  const st = M.probeState(n);
  const name = n.name;
  // 紧凑视图没有地址行：IP 接在短名后面（原版卡头“团队1-01 · ATT · 198.51.100.11”，fidelity #1），点击复制
  const ip = view === 'compact' && n.ip ? `<span class="ncard-ipsep" aria-hidden="true">·</span>${copyable(n.ip, { cls: 'ncard-ip' })}` : '';
  return `<header class="ncard-head">
    <span class="dot" data-s="dot" data-state="${st}" data-tip="${esc(M.probeTip(n))}"></span>${flag(n.region?.code)}
    <button type="button" class="ncard-name" data-act="open" data-tip="${esc(`打开 ${n.name} 的详情`)}">${esc(name)}</button>${ip}
    <span class="ncard-offline" data-s="offline">${s.offline}</span>
    <div class="ncard-actions">
      ${iconButton({ icon: 'chart-column', label: n.hasHost ? '今日网卡流量' : '今日业务流量', attrs: { 'data-act': 'today', 'aria-haspopup': 'dialog', 'aria-expanded': 'false' } })}
      ${iconButton({ icon: 'panel-right-open', label: '打开详情', attrs: { 'data-act': 'open' } })}
    </div>
  </header>`;
}
/** IP 版本标记；紧凑卡的标签行按原版写短“V4 / V6”（全称在提示里），省出位置让标签全显示（fidelity #6）。 */
function ipMarks(n, { short = false } = {}) {
  const v = [];
  if (n.ip && !n.ip.includes(':')) v.push(mark(short ? 'V4' : 'IPv4', { title: short ? 'IPv4' : undefined }));
  if (n.ipv6 || n.ip?.includes(':')) v.push(mark(short ? 'V6' : 'IPv6', { title: n.ipv6 ?? n.ip }));
  return v.join('');
}
function addr(n) {
  return `<div class="ncard-addr">${copyable(n.ip)}<span class="ncard-marks">${ipMarks(n)}</span></div>`;
}
function fields(store, n, s) {
  // 线路机：套餐下面按原版多一行规格（“2 核 / 4 GB / 60 GB；每账单周期 2000 GB …”），单行省略（fidelity #7）
  const spec = n.isRelay ? planOf(n).specs?.summary : '';
  return `<dl class="ncard-fields">
    <div><dt>入口</dt><dd>${esc(M.entryText(store, n))}</dd></div>
    <div><dt>套餐</dt><dd>${M.planHtml(n, { button: true })}</dd>${spec ? `<dd class="ncard-spec" data-tip="${esc(spec)}">${esc(spec)}</dd>` : ''}</div>
    <div><dt>带宽</dt><dd>${num(M.bandwidthF(n), { plain: true })}</dd></div>
    <div><dt>流量上限</dt><dd data-s="fquota">${s.fquota}</dd></div>
  </dl>`;
}
function meters(n, { sub = true, precise = false } = {}) {
  const ms = M.resourceMeters(n, { precise });
  const stale = M.isStale(n);
  return ms.map((m) => meter({ ...m, slot: `m-${m.key}`, sub: sub ? esc(m.sub) : false, stale })).join('');
}
function nicSeg(s) {
  return `<section class="ncard-seg">
    <div class="ncard-seg-title">网卡</div>
    <div class="nic">
      <div class="nic-rate" data-tip="网卡接收速率">${icon('arrow-down-to-line', 14)}${slot('rx', s.rx)}</div>
      <div class="nic-rate" data-tip="网卡发送速率">${icon('arrow-up-from-line', 14)}${slot('tx', s.tx)}</div>
      <div class="nic-total"><span>接收</span>${slot('rxT', s.rxT)}</div>
      <div class="nic-total"><span>发送</span>${slot('txT', s.txT)}</div>
    </div>
  </section>`;
}
function entrySeg(n, s, { compact = false } = {}) {
  const e = M.entryTraffic(n);
  const rows = compact ? e.rows.slice(0, 2) : e.rows;
  return `<section class="ncard-seg">
    <div class="ncard-seg-title"><span class="etitle">业务流量${compact ? '' : ' ' + neutralChip('无主机', '家宽代理产品没有主机，只有入口业务流量')}</span><span class="esrc${e.srcTone ? ' t-warn' : ''}" data-tip="${esc(e.srcTip)}">${slot('esrc', s.esrc, '')}</span></div>
    <table class="etable"><thead><tr><th>周期</th><th>↓ 下行</th><th>↑ 上行</th></tr></thead>
      <tbody>${rows.map(([w], i) => `<tr><th>${w}</th><td>${slot(`ed${i}`, s[`ed${i}`])}</td><td>${slot(`eu${i}`, s[`eu${i}`])}</td></tr>`).join('')}</tbody></table>
  </section>`;
}
function quotaSeg(n, s, { right } = {}) {
  const q = M.quotaModel(n);
  return `<section class="ncard-seg">
    <div class="qhead"><span class="qlabel">${icon('database', 13)}流量上限 <span class="qrem" data-s="qrem">${s.qrem}</span></span>${right ?? `<span class="quse" data-s="quse" data-tip="${esc(q.tip)}">${s.quse}</span>`}</div>
    ${quota({ pct: q.pct, tone: q.tone, slot: 'qbar' })}
  </section>`;
}
function beatSeg(n) { return `<section class="ncard-seg">${latencyStrip(n)}</section>`; }
function runSeg(n, s) {
  const left = n.hasHost
    ? `<span class="run-label">${icon('refresh-cw', 13)}持续运行</span>`
    : `<span class="run-label" data-tip="线路机到上游代理的 TCP 握手耗时（监控，约 30 秒一次）">${icon('radar', 13)}上游 TCP</span>`;
  return `<section class="ncard-seg run">
    <div class="run-item">${left}<span class="run-val lv" data-s="up">${s.up}</span></div>
    <div class="run-item"><span class="run-label">${icon('calendar', 13)}到期</span><span class="run-val" data-s="due">${s.due}</span></div>
  </section>`;
}
function tagsRow(n, { price = true, withIp = false } = {}) {
  const tags = M.tagsOf(n);
  const chips = [];
  if (withIp) chips.push(ipMarks(n, { short: true }));
  chips.push(...tags.map((t) => tagChip(t)));
  return `<div class="ncard-tags">${price ? priceChip(M.feeF(n)) : ''}<div class="tags-list" data-tags="${esc(tags.join('\n'))}">${chips.join('')}</div></div>`;
}
function tagsRowLarge(n) {
  const tags = M.tagsOf(n);
  return `<div class="ncard-tags"><div class="tags-list" data-tags="${esc(tags.join('\n'))}">${tags.map((t) => tagChip(t)).join('')}</div>${priceChip(M.feeF(n))}</div>`;
}
function foot(n, s, { compact = false } = {}) {
  const textBtns = `<span class="foot-text">${button({ label: '访问记录', variant: 'text', size: 'sm', attrs: { 'data-act': 'access' } })}${button({ label: '管理节点', variant: 'text', size: 'sm', attrs: { 'data-act': 'manage' } })}</span>`;
  const iconBtns = `<span class="foot-icons">${iconButton({ icon: 'history', label: '访问记录', size: 'sm', attrs: { 'data-act': 'access' } })}${iconButton({ icon: 'settings-2', label: '管理节点', size: 'sm', attrs: { 'data-act': 'manage' } })}</span>`;
  return `<footer class="ncard-foot">
    <span data-s="pill">${s.pill}</span>
    <div class="foot-right"><span data-s="util">${s.util}</span>${textBtns}${iconBtns}</div>
  </footer>`;
}

// ─── 三种视图 ───────────────────────────────────────────────────────────────

function large(store, n, s) {
  const mid = n.hasHost
    ? `<div class="ncard-res">${meters(n, { precise: true })}</div>${nicSeg(s)}`
    : entrySeg(n, s);
  return `${head(store, n, 'large', s)}${addr(n)}${fields(store, n, s)}${mid}${quotaSeg(n, s)}${beatSeg(n)}${runSeg(n, s)}${tagsRowLarge(n)}`;
}
function small(store, n, s) {
  const p = planOf(n);
  const meta = `<div class="meta2">
    <div class="meta2-row"><span>${copyable(n.ip)}</span><span class="ncard-marks">${ipMarks(n)}</span></div>
    <div class="meta2-row"><span>${esc(M.entryText(store, n))} · ${M.planHtml(n, { button: true })}</span></div>
  </div>`;
  const res = n.hasHost
    ? `<section class="ncard-seg"><div class="res-compact">${meters(n, { sub: false })}</div></section>`
    : entrySeg(n, s, { compact: true });
  const cells = n.hasHost
    ? `<div class="cells3">
        <div class="cell"><div class="cell-label">网卡速率</div><div class="cell-row">${icon('arrow-down-to-line', 12)}${slot('rx', s.rx)}</div><div class="cell-row">${icon('arrow-up-from-line', 12)}${slot('tx', s.tx)}</div></div>
        <div class="cell"><div class="cell-label">网卡累计</div><div class="cell-row">${icon('arrow-down-to-line', 12)}${slot('rxT', s.rxT)}</div><div class="cell-row">${icon('arrow-up-from-line', 12)}${slot('txT', s.txT)}</div></div>
        <div class="cell"><div class="cell-label">到期 · 月费</div><div class="cell-row">${icon('calendar', 12)}<span data-s="cdue">${s.cdue}</span></div><div class="cell-row">${icon('circle-dollar-sign', 12)}${num(M.feeF(n), { title: false })}</div></div>
      </div>`
    : `<div class="cells3">
        <div class="cell"><div class="cell-label">本月业务</div><div class="cell-row">${icon('arrow-down', 12)}${slot('md', s.md)}</div><div class="cell-row">${icon('arrow-up', 12)}${slot('mu', s.mu)}</div></div>
        <div class="cell"><div class="cell-label">上游 TCP · 出口</div><div class="cell-row">${icon('radar', 12)}${slot('tcp', s.tcp)}</div><div class="cell-row">${icon('globe', 12)}${slot('egr', s.egr)}</div></div>
        <div class="cell"><div class="cell-label">到期 · 月费</div><div class="cell-row">${icon('calendar', 12)}<span data-s="cdue">${s.cdue}</span></div><div class="cell-row">${icon('circle-dollar-sign', 12)}${num(M.feeF(n), { title: false })}</div></div>
      </div>`;
  // 流量上限：左“流量上限 不限”、右“持续运行 9 天”，下一行右对齐“51.8 GiB / 不限”，再是额度条（与原版小卡片同一排法，fidelity #2）
  const q = M.quotaModel(n);
  const quotaSmall = `<section class="ncard-seg">
    <div class="qhead is-tight"><span class="qlabel">${icon('database', 13)}流量上限 <span class="qrem" data-s="qlim">${s.qlim}</span></span><span class="qup lv" data-s="up2" data-tip="主机持续运行时长（自上次开机）">${s.up2}</span></div>
    <div class="quse-row"><span class="quse" data-s="quse" data-tip="${esc(q.tip)}">${s.quse}</span></div>
    ${quota({ pct: q.pct, tone: q.tone, slot: 'qbar' })}
  </section>`;
  return `${head(store, n, 'small', s)}${meta}${res}${cells}${quotaSmall}${beatSeg(n)}`;
}
function compact(store, n, s) {
  const res = n.hasHost
    ? `<section class="ncard-seg"><div class="res-compact">${meters(n, { sub: false })}</div></section>`
    : entrySeg(n, s, { compact: true });
  const flow = n.hasHost
    ? `<section class="ncard-seg"><div class="flow2">
        <div class="flow2-col"><div class="flow2-title">网卡速率</div><div class="flow2-row">${icon('arrow-down-to-line', 12)}${slot('rx', s.rx)}</div><div class="flow2-row">${icon('arrow-up-from-line', 12)}${slot('tx', s.tx)}</div></div>
        <div class="flow2-col"><div class="flow2-title">网卡累计</div><div class="flow2-row">${icon('arrow-down-to-line', 12)}${slot('rxT', s.rxT)}</div><div class="flow2-row">${icon('arrow-up-from-line', 12)}${slot('txT', s.txT)}</div></div>
      </div></section>`
    : `<section class="ncard-seg"><div class="flow2">
        <div class="flow2-col"><div class="flow2-title">本月业务</div><div class="flow2-row">${icon('arrow-down', 12)}${slot('md', s.md)}</div><div class="flow2-row">${icon('arrow-up', 12)}${slot('mu', s.mu)}</div></div>
        <div class="flow2-col"><div class="flow2-title">上游 TCP · 出口</div><div class="flow2-row">${icon('radar', 12)}${slot('tcp', s.tcp)}</div><div class="flow2-row">${icon('globe', 12)}${slot('egr', s.egr)}</div></div>
      </div></section>`;
  return `${head(store, n, 'compact', s)}${tagsRow(n, { withIp: true })}${res}${flow}${beatSeg(n)}`;
}

/**
 * nodeCard(store, n, { view:'large'|'small'|'compact', current }) → <article class="ncard">
 * 交互挂在 data-act 上（open / today / access / manage / plan），由页面委托处理。
 */
export function nodeCard(store, n, { view = 'large', current = false } = {}) {
  const s = slotValues(store, n, view);
  for (const [k, v] of Object.entries(s)) patcher.changed(`${n.id}:${view}:${k}`, v);
  const body = view === 'small' ? small(store, n, s) : view === 'compact' ? compact(store, n, s) : large(store, n, s);
  const h = M.healthOf(store, n);
  const kind = n.isRelay ? 'relay' : n.hasHost ? 'host' : 'proxy';
  return `<article class="ncard${current ? ' is-current' : ''}${M.isStale(n) ? ' is-stale' : ''}"${attrs({ 'data-id': n.id, 'data-view': view, 'data-kind': kind, 'data-health': h.code, 'data-online': M.probeState(n) })}>
    <div class="ncard-body">${body}</div>${foot(n, s)}
  </article>`;
}

/**
 * patchCard(el, store, n) — 实时更新：只写格式化后确实变化的槽位（§5.3 规则 1–2）。
 * 返回是否有状态变化（给调用方做 pill 外圈闪一次）。
 */
export function patchCard(el, store, n) {
  if (!el) return false;
  const view = el.dataset.view;
  const s = slotValues(store, n, view);
  let healthChanged = false;
  for (const [k, v] of Object.entries(s)) {
    if (!patcher.changed(`${n.id}:${view}:${k}`, v)) continue;
    const t = el.querySelector(`[data-s="${k}"]`);
    if (t) t.innerHTML = v;
    if (k === 'pill') healthChanged = true;
  }
  const h = M.healthOf(store, n);
  if (el.dataset.health !== h.code) el.dataset.health = h.code;
  const st = M.probeState(n);
  if (el.dataset.online !== st) el.dataset.online = st;
  const d = el.querySelector('[data-s="dot"]');
  if (d) { if (d.dataset.state !== st) d.dataset.state = st; const tip = M.probeTip(n); if (d.dataset.tip !== tip) d.dataset.tip = tip; }
  el.classList.toggle('is-stale', M.isStale(n));
  for (const m of M.resourceMeters(n, { precise: view === 'large' }) ?? []) {
    const me = el.querySelector(`[data-s="m-${m.key}"]`);
    if (me) patchMeter(me, { ...m, sub: me.querySelector('[data-m="s"]') ? esc(m.sub) : null });
  }
  const q = M.quotaModel(n);
  patchQuota(el.querySelector('[data-s="qbar"]'), q);
  patchBeat(el.querySelector('[data-s="beat"]'), n);
  if (healthChanged) {
    const p = el.querySelector('[data-s="pill"] .pill');
    p?.classList.add('is-flash');
    setTimeout(() => p?.classList.remove('is-flash'), 1300);
  }
  return healthChanged;
}

/** 骨架卡：行结构与真实卡相同（用一个真实节点的结构，把文字换成骨架块）。 */
export function skeletonCard(view = 'large') {
  const line = (w, h = 12) => `<span class="skel" style="width:${w};height:${h}px"></span>`;
  const rows = view === 'large' ? [32, 20, 'seg', 38, 38, 'gap', 42, 42, 'seg', 18, 20, 18, 'seg', 18, 6, 'seg', 18, 12, 'seg', 22, 'gap', 24]
    : view === 'small' ? [32, 40, 'seg', 26, 26, 'gap', 60, 'seg', 18, 6, 'seg', 18, 12]
      : [32, 20, 'seg', 26, 26, 'seg', 18, 20, 20, 'seg', 18, 12];
  const body = rows.map((r) => (r === 'seg' ? '<div class="ncard-seg" style="margin-top:12px;padding-top:0"></div>' : r === 'gap' ? '<div style="height:12px"></div>' : `<div class="skel-line" style="min-height:${r}px">${line(r > 30 ? '70%' : r > 16 ? '55%' : '85%', Math.min(r - 6, 14))}</div>`)).join('');
  // 与真实卡等高（1440 宽实测：大卡片 652、小卡片 460、紧凑 346；精修后重新量过），首屏换成真实卡时不跳（§3.20，修 dna-home J3）
  const h = { large: 652, small: 460, compact: 346 }[view] ?? 652;
  return `<article class="ncard is-skel" aria-hidden="true" style="min-height:${h}px"><div class="ncard-body">${body}</div><footer class="ncard-foot">${line('72px', 20)}${line('140px', 14)}</footer></article>`;
}

/**
 * 标签行溢出：放不下时从尾部把标签换成 +N（价格 chip 在列表外，永不被挤掉，C-01）。
 * 在渲染后、尺寸变化后调用；只处理 root 下的 .tags-list。
 */
export function fitTags(root) {
  for (const list of root.querySelectorAll('.tags-list')) {
    const all = (list.dataset.tags ?? '').split('\n').filter(Boolean);
    const chips = [...list.querySelectorAll(':scope > .chip:not(.chip-more)')];
    list.querySelector(':scope > .chip-more')?.remove();
    chips.forEach((c) => { c.hidden = false; });
    if (list.scrollWidth <= list.clientWidth + 1) continue;
    let hidden = 0;
    for (let i = chips.length - 1; i >= 0; i--) {
      chips[i].hidden = true; hidden++;
      list.querySelector(':scope > .chip-more')?.remove();
      list.insertAdjacentHTML('beforeend', moreChip(hidden, all.slice(all.length - hidden)));
      if (list.scrollWidth <= list.clientWidth + 1) break;
    }
  }
}
export { dot };
