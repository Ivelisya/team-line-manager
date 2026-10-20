// pages/home.js — 首页 #/home（SPEC §6.1）。区块顺序与原版一致（ID-08）：
//   页头（首页 + 鲜度 + 大卡片/小卡片/紧凑）→ 4 张 KPI → 分组分段 + 排序 → 筛选条（地区 + 套餐）→ 卡片网格。
// 状态：视图存 localStorage（refined:home:view）；分组、地区、套餐、排序、分组方式存 URL。

import { fmt } from '../../../shared/index.js';
import {
  icon, esc, num, flag, storage, on, debounce, rafBatch,
  pageHead, watchStuck, freshness, segmented, onSegment, setSegmented, setSegmentCounts, hydrate,
  kpiTile, kpiValue, patchKpi, countDots, fchip, button, empty,
  openMenu, openPopover, closePopover, currentPopover, dot,
} from '../components.js';
import { nodeCard, patchCard, skeletonCard, fitTags } from '../node-card.js';
import { allPlans, planOf, planById, planLine, membersOf } from '../plans.js';
import { todayNic } from '../node-drawer.js';
import * as M from '../node-model.js';
import { HEAD_TOOLS, HOME_VIEWS as VIEWS } from '../page-tools.js';

const SORTS = { default: { label: '默认', dir: 1 }, name: { label: '名称', dir: 1 }, netrate: { label: '网卡速率', dir: -1 }, nettotal: { label: '网卡累计', dir: -1 } };
const GROUPS = { '': '不分组', plan: '按套餐', relay: '按线路机' };
const VIEW_KEY = 'refined:home:view';

function readState(params) {
  const csv = (k) => new Set((params.get(k) ?? '').split(',').filter(Boolean));
  const rawSort = params.get('sort') ?? 'default';
  const key = rawSort.replace(/^-/, '');
  return {
    team: params.get('team') ?? '',
    region: csv('region'),
    plan: csv('plan'),
    sort: SORTS[key] ? key : 'default',
    dir: rawSort.startsWith('-') ? -1 : 1,
    group: GROUPS[params.get('group') ?? ''] ? params.get('group') ?? '' : '',
  };
}
function writeState(st) {
  const sortVal = st.sort === 'default' ? null : `${st.dir === -1 ? '-' : ''}${st.sort}`;
  return {
    team: st.team || null,
    region: st.region.size ? [...st.region].join(',') : null,
    plan: st.plan.size ? [...st.plan].join(',') : null,
    sort: sortVal,
    group: st.group || null,
  };
}

export default {
  title: '首页',
  mount(ctx) {
    const { el, app } = ctx;
    const store = app.store;
    let st = readState(ctx.params);
    let view = storage.get(VIEW_KEY, 'large');
    if (!VIEWS.some((v) => v.value === view)) view = 'large';
    let order = [];            // 当前网格里的节点顺序（给抽屉 [ ] 与 j/k）
    let lastResort = 0, resortTimer = null, hovering = false;
    const cardEls = new Map();

    // ─── 数据选择 ───
    const matchTeam = (n) => !st.team || n.team === st.team;
    const matchRegion = (n) => !st.region.size || st.region.has(n.region?.code);
    const matchPlan = (n) => !st.plan.size || st.plan.has(n.plan_id);
    const visible = () => store.nodes().filter((n) => matchTeam(n) && matchRegion(n) && matchPlan(n));
    const filtered = () => !!(st.team || st.region.size || st.plan.size);

    function sortKey(n) {
      const h = n.host ?? {};
      if (st.sort === 'name') return n.name;
      if (st.sort === 'netrate') return n.hasHost ? (h.net_rx_bps ?? 0) + (h.net_tx_bps ?? 0) : null;
      if (st.sort === 'nettotal') return n.hasHost ? (h.net_total_rx ?? 0) + (h.net_total_tx ?? 0) : null;
      return n.teamOrder * 10000 + (n.number ?? 0);
    }
    function sorted(list) {
      const coll = new Intl.Collator('zh-CN', { numeric: true });
      return list.slice().sort((a, b) => {
        const offA = M.probeState(a) === 'off', offB = M.probeState(b) === 'off';
        if (offA !== offB) return offA ? 1 : -1; // 离线节点始终沉底（原版规则）
        const ka = sortKey(a), kb = sortKey(b);
        if (ka == null && kb != null) return 1;
        if (kb == null && ka != null) return -1;
        let c = typeof ka === 'string' ? coll.compare(ka, kb) : (ka ?? 0) - (kb ?? 0);
        if (c === 0) c = (a.teamOrder * 10000 + a.number) - (b.teamOrder * 10000 + b.number);
        return c * (st.sort === 'default' ? 1 : st.dir);
      });
    }
    function groupsOf(list) {
      if (!st.group) return [{ key: '', name: '', rows: list }];
      const by = new Map();
      if (st.group === 'plan') for (const p of allPlans(store)) by.set(p.id, { key: p.id, name: p.name, rows: [] });
      for (const n of list) {
        const k = st.group === 'plan' ? n.plan_id : n.relay ?? '未绑定';
        if (!by.has(k)) by.set(k, { key: k, name: st.group === 'plan' ? planOf(n).name : k, rows: [] });
        by.get(k).rows.push(n);
      }
      return [...by.values()].filter((g) => g.rows.length);
    }

    // ─── KPI（按当前分组、地区、套餐筛选后的节点计算，与原版“可见节点”口径一致） ───
    function kpiModel() {
      const list = visible();
      let on_ = 0, off = 0, unk = 0, rx = 0, tx = 0, trx = 0, ttx = 0, cents = 0, svc = 0, hosts = 0;
      for (const n of list) {
        const s = M.probeState(n);
        if (s === 'on') on_++; else if (s === 'off') off++; else unk++;
        if (n.hasHost) { hosts++; rx += n.host.net_rx_bps ?? 0; tx += n.host.net_tx_bps ?? 0; trx += n.host.net_total_rx ?? 0; ttx += n.host.net_total_tx ?? 0; }
        if (n.billing) { cents += n.billing.monthly_cents ?? 0; svc++; }
      }
      // 筛选后没有主机：网卡两项是“缺测”而不是真实的 0（§4.1 F-12）
      const noHost = '<span class="t-muted">筛选内的节点都没有主机</span>';
      return {
        probe: { value: kpiValue(String(on_), `/ ${list.length}`), sub: countDots([['离线', off, 'bad'], ['未知', unk, 'unknown']]) },
        // 窄屏（≤420）换短写法，副行保持一行（fidelity #15）
        rate: hosts ? { value: kpiValue(fmt.rate(rx + tx)), sub: `接收 ${num(fmt.rate(rx))} · 发送 ${num(fmt.rate(tx))}`, subShort: shortPair(fmt.rate(rx), fmt.rate(tx)) } : { value: kpiValue(null), sub: noHost },
        total: hosts ? { value: kpiValue(fmt.bytes(trx + ttx)), sub: `接收 ${num(fmt.bytes(trx))} · 发送 ${num(fmt.bytes(ttx))}`, subShort: shortPair(fmt.bytes(trx), fmt.bytes(ttx)) } : { value: kpiValue(null), sub: noHost },
        cost: { value: kpiValue(fmt.money(cents)), sub: `${num(fmt.int(svc, '项服务'))}` },
      };
    }
    /** 短写法：两个值同单位时只写一次单位 “↓42.2 ↑41.4 Mbps”（接收 ↓ / 发送 ↑ 用网卡图标的方向）。 */
    function shortPair(a, b) {
      if (a.kind === 'missing' || b.kind === 'missing') return '';
      return a.unit === b.unit ? `<span class="num">↓${esc(a.num)} ↑${esc(b.num)}</span> <span class="unit">${esc(a.unit)}</span>` : `${num(a)} ${num(b)}`;
    }
    function kpisHtml() {
      const m = kpiModel();
      const f = filtered();
      return [
        kpiTile({ key: 'probe', label: '探针在线', filtered: f, info: '探针（Komari agent，或线路机代发的连通性探针）与服务端保持连接的节点数，含 8 个家宽代理的代发探针；不代表业务可用。', ...m.probe }),
        kpiTile({ key: 'rate', label: '网卡速率', filtered: f, infoAlways: true, info: '所有主机网卡接收 + 发送之和（原名“主机传输速度”）。线路机与落地机的网卡相加，同一份业务会计两次；业务口径见运营总览“业务速率”。', ...m.rate }),
        kpiTile({ key: 'total', label: '网卡累计流量', filtered: f, infoAlways: true, info: '各主机网卡计数器自启动以来的累计（原名“主机累计流量”），起点各不相同；线路机与落地机相加，同一份业务会计两次。', action: { icon: 'chart-no-axes-column', label: '按本月流量看节点', href: '#/nodes?sort=-month' }, ...m.total }),
        kpiTile({ key: 'cost', label: '每月费用', filtered: f, info: '折算成月付的合计，币种 USD；按节点实际登记的月费求和。', action: { icon: 'circle-dollar-sign', label: '按月费看节点', href: '#/nodes?sort=-cost' }, ...m.cost }),
      ].join('');
    }

    // ─── 控制条与筛选条 ───
    function teamItems() {
      const base = store.nodes().filter((n) => matchRegion(n) && matchPlan(n));
      return [{ value: '', label: '全部', count: base.length }, ...store.teams().map((t) => ({ value: t.name, label: t.name, count: base.filter((n) => n.team === t.name).length }))];
    }
    function sortButton() {
      const s = SORTS[st.sort];
      const arrow = st.sort === 'default' ? 'arrow-up-down' : st.dir === -1 ? 'arrow-down-wide-narrow' : 'arrow-up-narrow-wide';
      return `<button type="button" class="btn btn-secondary" data-act="sort" aria-haspopup="menu" aria-expanded="false">${icon(arrow, 14)}<span class="btn-label">${esc(s.label)}${st.group ? ` · ${esc(GROUPS[st.group])}` : ''}</span>${icon('chevron-down', 12, 'btn-caret')}</button>`;
    }
    function fbarHtml() {
      const regions = new Map();
      const plans = new Map();
      for (const n of store.nodes()) {
        if (matchTeam(n) && matchPlan(n)) { const c = n.region?.code; if (c) regions.set(c, (regions.get(c) ?? 0) + 1); }
        if (matchTeam(n) && matchRegion(n)) plans.set(n.plan_id, (plans.get(n.plan_id) ?? 0) + 1);
      }
      const allRegions = [...new Set(store.nodes().map((n) => n.region?.code).filter(Boolean))];
      const regionGroup = allRegions.length > 1
        ? `<div class="fbar-group" role="group" aria-label="地区"><span class="fbar-label">地区</span><div class="fbar-chips">${allRegions.sort((a, b) => (regions.get(b) ?? 0) - (regions.get(a) ?? 0)).map((c) => fchip({ label: c, value: c, group: 'region', count: regions.get(c) ?? 0, pressed: st.region.has(c), flagHtml: flag(c), tip: store.nodes().find((n) => n.region?.code === c)?.region?.name ?? c, cls: 'fchip-code' })).join('')}</div></div>`
        : '';
      // 套餐收成一个“套餐 ▾”多选菜单（与节点页同一种筛选按钮），筛选条保持原版地区条的一行高（fidelity #3、consistency #1）
      const sel = [...st.plan];
      const planLabel = sel.length === 1 ? `套餐：${planById(sel[0])?.name ?? sel[0]}` : sel.length ? `套餐 · ${sel.length}` : '套餐';
      const planBtn = button({ label: planLabel, caret: true, size: 'md', attrs: { 'data-act': 'plan-menu', 'aria-haspopup': 'menu', 'aria-expanded': 'false', 'aria-pressed': sel.length ? 'true' : 'false' } });
      const clear = st.region.size || st.plan.size ? button({ label: '清除', variant: 'quiet', size: 'md', attrs: { 'data-act': 'clear' } }) : '';
      planCounts = plans;
      return `${regionGroup}<span class="fbar-end">${planBtn}${clear}</span>`;
    }
    let planCounts = new Map();
    function planMenu(b) {
      const items = [
        { group: '套餐（可多选）' },
        ...allPlans(store).map((p) => ({ value: p.id, label: p.name, checked: st.plan.has(p.id), count: planCounts.get(p.id) ?? 0 })),
        ...(st.plan.size ? [{ sep: true }, { value: '__clear', label: '清除套餐筛选', icon: 'x' }] : []),
      ];
      openMenu(b, {
        label: '套餐', multi: true, minWidth: 240, items,
        onSelect: (v) => {
          if (v === '__clear') st.plan.clear();
          else if (st.plan.has(v)) st.plan.delete(v); else st.plan.add(v);
          commit();
        },
      });
    }

    // ─── 卡片网格 ───
    function gridHtml() {
      const list = sorted(visible());
      order = list.map((n) => n.id);
      if (!list.length) return '';
      const cur = app.drawerNodeId?.();
      return groupsOf(list).map((g) => {
        const headRow = g.name ? `<div class="group-row" role="presentation"><span class="group-name">${esc(g.name)}</span><span class="group-meta">${g.rows.length} 台 · ${esc(fmt.money(g.rows.reduce((a, n) => a + (n.billing?.monthly_cents ?? 0), 0), { perMonth: true }).text)}</span></div>` : '';
        return headRow + g.rows.map((n) => nodeCard(store, n, { view, current: n.id === cur })).join('');
      }).join('');
    }
    function renderGrid() {
      const grid = el.querySelector('[data-grid]');
      grid.dataset.view = view;
      const html = gridHtml();
      const emptyEl = el.querySelector('[data-empty]');
      if (!html) {
        grid.innerHTML = '';
        emptyEl.hidden = false;
        emptyEl.innerHTML = store.nodes().length ? empty({ icon: 'inbox', text: '没有符合筛选条件的节点', action: { label: '清除筛选', attrs: { 'data-act': 'clear-all' } } }) : empty({ text: '还没有节点', action: { label: '去上架流水线', attrs: { 'data-act': 'go-onboarding' } } });
      } else {
        emptyEl.hidden = true;
        grid.innerHTML = html;
      }
      cardEls.clear();
      grid.querySelectorAll('.ncard[data-id]').forEach((c) => cardEls.set(c.dataset.id, c));
      hydrate(grid);
      fitTags(grid);
      lastResort = Date.now();
    }

    // ─── 首屏 ───
    el.innerHTML = `<div class="page" data-page="home">
      ${pageHead({ title: '首页', tools: HEAD_TOOLS.home({ view }) })}
      <section class="kpi-grid" data-kpis aria-label="概览">${kpisHtml()}</section>
      <div class="home-bar">${segmented({ name: 'team', items: teamItems(), value: st.team, label: '分组', disableZero: true })}<span data-sortbtn>${sortButton()}</span></div>
      <div class="fbar" data-fbar role="group" aria-label="筛选">${fbarHtml()}</div>
      <div class="home-empty" data-empty hidden></div>
      <div class="cards" data-grid data-view="${view}" aria-label="节点"></div>
    </div>`;
    const gridEl = el.querySelector('[data-grid]');
    // 首次加载：与真实卡等高的骨架（§3.20），下一帧换成真实卡
    gridEl.innerHTML = Array.from({ length: 6 }, () => skeletonCard(view)).join('');
    hydrate(el);
    const offStuck = watchStuck(el);
    requestAnimationFrame(() => renderGrid());

    // ─── 交互 ───
    let renderedKey = JSON.stringify(writeState(st)); // 最近一次渲染对应的 URL 状态（处理函数先改 st 再 commit，不能拿 st 比）
    const offs = [offStuck];
    offs.push(onSegment(el, 'view', (v) => switchView(v)));
    offs.push(onSegment(el, 'team', (v) => { st.team = v; commit(); }));
    offs.push(on(el, 'click', '[data-fgroup]', (e, b) => {
      const set = st[b.dataset.fgroup];
      const v = b.dataset.fvalue;
      if (set.has(v)) set.delete(v); else set.add(v);
      commit();
    }));
    offs.push(on(el, 'click', '[data-act]', (e, b) => {
      const act = b.dataset.act;
      const card = b.closest('.ncard');
      const id = card?.dataset.id;
      if (act === 'clear') { st.region.clear(); st.plan.clear(); commit(); }
      else if (act === 'clear-all') { st.region.clear(); st.plan.clear(); st.team = ''; commit(); }
      else if (act === 'go-onboarding') app.navigate('#/onboarding');
      else if (act === 'sort') sortMenu(b);
      else if (act === 'plan-menu') planMenu(b);
      else if (act === 'open') app.openNode(id, 'overview');
      else if (act === 'access') app.openNode(id, 'access');
      else if (act === 'manage') app.openNode(id, 'manage');
      else if (act === 'plan') planPopover(b);
      else if (act === 'today') { e.stopPropagation(); todayPopover(b, store.node(id), true); }
      else if (act === 'only-plan') { closePopover(); st.plan = new Set([b.dataset.plan]); commit(); }
      else if (act === 'today-detail') { closePopover(); app.openNode(b.dataset.id, 'traffic'); }
    }));
    // 今日流量弹层：精确指针悬停 150 ms 预览，点击固定（§3.18）
    let hoverT = null;
    const fine = matchMedia('(hover: hover) and (pointer: fine)').matches;
    offs.push(on(el, 'pointerover', '[data-act="today"]', (e, b) => {
      if (!fine || currentPopover()?.anchor === b) return;
      clearTimeout(hoverT);
      hoverT = setTimeout(() => todayPopover(b, store.node(b.closest('.ncard').dataset.id), false), 150);
    }));
    offs.push(on(el, 'pointerout', '[data-act="today"]', (e, b) => {
      clearTimeout(hoverT);
      if (b.contains(e.relatedTarget)) return;
      const p = currentPopover();
      if (p && p.anchor === b && !p.pinned) setTimeout(() => { if (p === currentPopover() && !p.pinned && !p.el.matches(':hover')) p.close(); }, 150);
    }));
    // 实时排序时：鼠标在网格上或键盘焦点在里面，暂停重排（§5.3 规则 3）
    offs.push(on(gridEl, 'pointerenter', null, () => { hovering = true; }));
    offs.push(on(gridEl, 'pointerleave', null, () => { hovering = false; if (resortTimer == null && isLiveSort()) scheduleResort(); }));

    const ro = new ResizeObserver(rafBatch(() => fitTags(gridEl)));
    ro.observe(gridEl);

    function commit() {
      app.setParams(writeState(st), { replace: true });
    }
    function sortMenu(b) {
      const arrowFor = (k) => (st.sort === k && k !== 'default' ? icon(st.dir === -1 ? 'arrow-down' : 'arrow-up', 14) : st.sort === k ? icon('check', 14) : '');
      openMenu(b, {
        label: '排序与分组',
        items: [
          { group: '排序' },
          ...Object.entries(SORTS).map(([k, s]) => ({ value: `sort:${k}`, label: s.label, checked: st.sort === k, end: arrowFor(k) })),
          { sep: true },
          { group: '分组' },
          ...Object.entries(GROUPS).map(([k, w]) => ({ value: `group:${k}`, label: w, checked: st.group === k })),
        ],
        onSelect: (v) => {
          const [kind, k] = v.split(':');
          if (kind === 'sort') { if (st.sort === k && k !== 'default') st.dir = -st.dir; else { st.sort = k; st.dir = SORTS[k].dir; } }
          else st.group = k;
          commit();
        },
      });
    }
    function planPopover(b) {
      const p = planById(b.dataset.plan) ?? planOf(store.node(b.closest('.ncard')?.dataset.id));
      if (currentPopover()?.anchor === b) { closePopover(); return; }
      const same = membersOf(store, p.id).length;
      const price = p.price_cents != null ? fmt.money(p.price_cents, { currency: p.currency, perMonth: true }).text : '—';
      openPopover(b, {
        width: 280, label: '套餐',
        html: `<div class="pop-row"><b class="pop-val" style="font-family:var(--font-sans)">${esc(p.name)}</b></div><div class="muted">${esc(p.specs?.summary ?? '套餐目录里没有这个节点')}</div><div class="pop-sep"></div><div class="pop-row"><span class="pop-key">${esc(price)} · ${esc(p.cycle_word ?? '—')}</span><span class="pop-key">同套餐 ${same} 台</span></div><div class="pop-foot"><span>${esc(p.provider ?? '')}</span>${p.unresolved ? '' : `<button type="button" class="btn btn-link btn-sm" data-act="only-plan" data-plan="${esc(p.id)}">只看这个套餐</button>`}</div>`,
      });
      currentPopover().el.addEventListener('click', (e) => { const x = e.target.closest('[data-act="only-plan"]'); if (x) { closePopover(); st.plan = new Set([x.dataset.plan]); commit(); } });
    }
    function todayPopover(b, n, pin) {
      const p = currentPopover();
      if (p && p.anchor === b) { if (pin) { if (p.pinned) p.close(); else p.pinned = true; } return; }
      const api = openPopover(b, { width: 248, placement: 'bottom-end', label: n.hasHost ? '今日网卡流量' : '今日业务流量' });
      api.pinned = pin;
      api.el.addEventListener('pointerleave', () => setTimeout(() => { if (!api.pinned && currentPopover() === api && !b.matches(':hover')) api.close(); }, 150));
      api.el.addEventListener('click', (e) => { const x = e.target.closest('[data-act="today-detail"]'); if (x) { api.close(); app.openNode(n.id, 'traffic'); } });
      const detail = `<button type="button" class="btn btn-link btn-sm" data-act="today-detail">明细</button>`;
      if (!n.hasHost) {
        const T = n.traffic ?? {}, pk = T.peak_24h;
        api.update(`<div class="pop-title">今日业务流量</div>
          <div class="pop-row"><span class="pop-key">${icon('arrow-down', 14)}↓ 下行</span><span class="pop-val">${num(fmt.bytes(T.today?.down))}</span></div>
          <div class="pop-row"><span class="pop-key">${icon('arrow-up', 14)}↑ 上行</span><span class="pop-val">${num(fmt.bytes(T.today?.up))}</span></div>
          <div class="pop-sep"></div>
          <div class="pop-row"><span class="pop-key">近 24 小时峰值</span><span>${pk ? `<b class="num">${esc(fmt.rate((pk.down * 8) / (pk.step ?? 300)).text)}</b> · ${esc(fmt.clockTime(pk.t))}` : '—'}</span></div>
          <div class="pop-foot"><span>Xray 入口计数 · 更新 ${esc(fmt.clockTime(n.rate?.sample_at ?? store.now()))}</span>${detail}</div>`);
        return;
      }
      api.update(`<div class="pop-title">今日网卡流量</div><div class="pop-skel" style="width:80%"></div><div class="pop-skel" style="width:70%"></div><div class="pop-skel" style="width:60%"></div>`);
      todayNic(store, n).then((m) => {
        if (currentPopover() !== api) return;
        if (!m || !m.samples) { api.update(`<div class="pop-title">今日网卡流量</div><div class="muted">今日没有采样</div><div class="pop-foot"><span>按 5 分钟采样</span>${detail}</div>`); return; }
        api.update(`<div class="pop-title">今日网卡流量</div>
          <div class="pop-row"><span class="pop-key">${icon('arrow-down-to-line', 14)}接收</span><span class="pop-val">${num(fmt.bytes(m.rx))}</span></div>
          <div class="pop-row"><span class="pop-key">${icon('arrow-up-from-line', 14)}发送</span><span class="pop-val">${num(fmt.bytes(m.tx))}</span></div>
          <div class="pop-sep"></div>
          <div class="pop-row"><span class="pop-key">采样峰值</span><span><b class="num">${esc(fmt.rate(m.peak?.v).text)}</b> · ${esc(m.peak ? fmt.clockTime(m.peak.t) : '—')}</span></div>
          <div class="pop-foot"><span>按 5 分钟采样 · 更新 ${esc(fmt.clockTime(m.last))}</span>${detail}</div>`);
      }).catch(() => {
        if (currentPopover() !== api) return;
        api.update(`<div class="pop-title">今日网卡流量</div><div class="note" data-tone="bad">${icon('circle-x', 16)}<div class="note-body">今日流量加载失败</div></div>`);
      });
    }

    // ─── 实时更新：只补丁变化的卡片与 KPI（§5.3） ───
    const isLiveSort = () => st.sort === 'netrate' || st.sort === 'nettotal';
    function scheduleResort() {
      clearTimeout(resortTimer);
      const wait = Math.max(0, 5000 - (Date.now() - lastResort));
      resortTimer = setTimeout(() => {
        resortTimer = null;
        if (hovering || gridEl.contains(document.activeElement) || currentPopover()) { scheduleResort(); return; }
        resortFlip();
      }, wait);
    }
    function resortFlip() {
      const list = sorted(visible());
      const ids = list.map((n) => n.id);
      if (ids.join() === order.join()) { lastResort = Date.now(); return; }
      if (st.group) { renderGrid(); return; }
      const first = new Map([...cardEls].map(([id, c]) => [id, c.getBoundingClientRect()]));
      ids.forEach((id) => { const c = cardEls.get(id); if (c) gridEl.appendChild(c); });
      order = ids;
      lastResort = Date.now();
      if (matchMedia('(prefers-reduced-motion: reduce)').matches) return;
      for (const [id, c] of cardEls) {
        const a = first.get(id), b = c.getBoundingClientRect();
        const dx = a.left - b.left, dy = a.top - b.top;
        if (!dx && !dy) continue;
        c.animate([{ transform: `translate(${dx}px, ${dy}px)` }, { transform: 'none' }], { duration: 220, easing: 'cubic-bezier(.4,0,.2,1)' });
      }
    }
    const offLive = store.on('change', (diff) => {
      if (!el.isConnected) return;
      const m = kpiModel();
      const k = el.querySelector('[data-kpis]');
      for (const key of Object.keys(m)) patchKpi(k, key, m[key]);
      for (const [id, c] of cardEls) {
        if (diff?.replay !== undefined || diff?.nodes?.[id] || diff?.relays?.[id]) patchCard(c, store, store.node(id));
      }
      if (isLiveSort() && resortTimer == null) scheduleResort();
    });
    offs.push(offLive);
    // 相对时间（“离线 12 分钟”、“Xray · 3 秒前”）每 30 秒刷新一次
    const tick = setInterval(() => { for (const [id, c] of cardEls) patchCard(c, store, store.node(id)); }, 30000);

    // ─── 键盘：1/2/3 切视图，j/k 在卡片间移动（§5.4） ───
    const unbind = app.keys.bindAll([
      { combo: '1', run: () => switchView('large'), desc: '大卡片', group: '首页' },
      { combo: '2', run: () => switchView('small'), desc: '小卡片', group: '首页' },
      { combo: '3', run: () => switchView('compact'), desc: '紧凑', group: '首页' },
      { combo: 'j', run: () => moveFocus(1), desc: '下一张卡', group: '首页', when: () => !app.drawerOpen() },
      { combo: 'k', run: () => moveFocus(-1), desc: '上一张卡', group: '首页', when: () => !app.drawerOpen() },
      { combo: 'o', run: () => document.activeElement.closest('.ncard')?.querySelector('.ncard-name')?.click(), desc: '打开焦点所在卡片的详情', group: '首页', when: () => !app.drawerOpen() && gridEl.contains(document.activeElement) },
      { combo: 'ArrowDown', run: () => moveFocus(1), hidden: true, when: () => !app.drawerOpen() && gridEl.contains(document.activeElement) },
      { combo: 'ArrowUp', run: () => moveFocus(-1), hidden: true, when: () => !app.drawerOpen() && gridEl.contains(document.activeElement) },
    ]);
    /** 切视图时留在同一张卡：记下视口里第一张可见卡片和它离视口顶的距离，重画后滚回去（craft #11）。 */
    function switchView(v) {
      if (v === view) return;
      const top = (parseInt(getComputedStyle(document.documentElement).getPropertyValue('--topbar-h'), 10) || 56) + (el.querySelector('.page-head')?.offsetHeight ?? 0);
      const anchor = [...cardEls.values()].find((c) => c.getBoundingClientRect().bottom > top + 8);
      // 卡片顶已经滚出页头时，切换后把它的顶对齐到页头下沿（新视图的卡更矮，按原偏移会整张滚出视口）
      const offset = anchor ? Math.max(anchor.getBoundingClientRect().top, top + 8) : 0;
      view = v; storage.set(VIEW_KEY, v);
      setSegmented(el.querySelector('[data-seg="view"]'), v);
      renderGrid();
      if (anchor && scrollY > 0) {
        const again = cardEls.get(anchor.dataset.id);
        if (again) window.scrollTo(0, Math.max(0, scrollY + again.getBoundingClientRect().top - offset));
      }
    }
    function moveFocus(d) {
      const names = [...gridEl.querySelectorAll('.ncard-name')];
      if (!names.length) return;
      const i = names.indexOf(document.activeElement);
      const next = names[i < 0 ? (d > 0 ? 0 : names.length - 1) : Math.max(0, Math.min(names.length - 1, i + d))];
      next.focus();
      next.closest('.ncard').scrollIntoView({ block: 'nearest', behavior: matchMedia('(prefers-reduced-motion: reduce)').matches ? 'auto' : 'smooth' });
    }

    return {
      nodeOrder: () => order,
      /** 抽屉打开 / 切换时高亮对应卡片（边线 --accent）。 */
      onDrawer(id) { for (const [cid, c] of cardEls) c.classList.toggle('is-current', cid === id); },
      update(params) {
        st = readState(params);
        const key = JSON.stringify(writeState(st));
        if (key === renderedKey) return;
        renderedKey = key;
        el.querySelector('[data-kpis]').innerHTML = kpisHtml();
        const teamSeg = el.querySelector('[data-seg="team"]');
        setSegmentCounts(teamSeg, Object.fromEntries(teamItems().map((t) => [t.value, t.count])));
        setSegmented(teamSeg, st.team);
        el.querySelector('[data-sortbtn]').innerHTML = sortButton();
        el.querySelector('[data-fbar]').innerHTML = fbarHtml();
        renderGrid();
      },
      unmount() { offs.forEach((f) => f()); ro.disconnect(); clearInterval(tick); clearTimeout(resortTimer); unbind(); },
    };
  },
};
