// pages/plans.js — 套餐与配额 #/plans（新页面，fleetd：/v2/api/v1/plans、assets、quota，fleet/API.md §3）。
// 版面：页头 → 4 张 KPI（套餐 · 资产 · 每月费用 · 7 天内到期）→ 套餐面板（6 个规范套餐，展开看名下资产）
// → 线路机流量配额（额度条 + 口径说明）→ 续费到期（账期时区日历天数，由近到远）。
// fleetd 不在时整页“新后台未上线”。URL：open=（展开的套餐 id）。
import { fmt } from '../../../shared/index.js';
import {
  esc, num, txt, on, pageHead, watchStuck, freshness, panel, dataTable, empty, note, button, tonePill, tagChip,
  kpiTile, kpiValue, quota, quotaTone, hydrate, tableSkeleton,
} from '../components.js';
import { refreshBtn } from '../page-tools.js';
import { fleetAvailable, fleetGet, onFleet, fleetAbsent } from '../fleet.js';
import { stripInternal } from '../plans.js';
import { swrGet, swrSet } from '../swr.js';
// 口径说明里提到目录内部待办（Qn 问题、内部核实、Komari 旧口径）的条目不上屏
const INTERNAL = /Q\d|\u5f85\u6838|\u5f85\u8865|\u5f85\u786e\u8ba4|未答|Komari/;

const readP = (p) => ({ open: p.get('open') ?? '' });
const money = (p) => (p?.amount != null ? fmt.money(Math.round(Number(p.amount) * 100), { currency: p.currency ?? 'USD', perMonth: p.cycle === 'monthly' }) : fmt.missing());
const dueF = (d) => (d?.days == null ? fmt.missing() : fmt.countdown(d.days));
const tierTone = (days) => (days == null ? 'neutral' : days <= 1 ? 'bad' : days <= 7 ? 'warn' : 'neutral');

export default {
  title: '套餐与配额',
  density: true,
  mount({ el, app, params }) {
    let P = readP(params);
    let plans = null, assets = null, quotaData = null, err = null;
    el.innerHTML = `<div class="page" data-page="plans">
      ${pageHead({ title: '套餐与配额', tools: freshness() + refreshBtn() })}
      <section class="kpi-grid" data-slot="kpis">${Array.from({ length: 4 }, () => kpiTile({ key: 'x', label: ' ', loading: true })).join('')}</section>
      <div data-slot="plans">${panel({ title: '套餐', flush: true, body: tableSkeleton(6, 6) })}</div>
      <div data-slot="quota"></div>
      <div data-slot="due"></div>
    </div>`;
    const slot = (k) => el.querySelector(`[data-slot="${k}"]`);
    if (!fleetAvailable()) { slot('kpis').remove(); slot('plans').innerHTML = fleetAbsent('套餐与配额'); return { unmount() {} }; }

    async function load() {
      if (!plans?.length) { const c = swrGet('plans'); if (c) { ({ plans, assets, quotaData } = c); if (el.isConnected) render(); } }
      try {
        const [p, a, q] = await Promise.all([fleetGet('/plans'), fleetGet('/assets'), fleetGet('/quota')]);
        plans = p?.plans ?? []; assets = a?.assets ?? []; quotaData = q ?? {};
        err = null;
        swrSet('plans', { plans, assets, quotaData });
      } catch (e) { err = e.message; }
      if (el.isConnected) render();
    }
    function render() {
      if (err) { slot('plans').innerHTML = panel({ title: '套餐', body: note({ tone: 'bad', title: '读取失败', body: esc(err), action: button({ label: '重试', size: 'sm', attrs: { 'data-act': 'refresh' } }) }) }); return; }
      const monthly = assets.reduce((s, a) => s + (a.price?.cycle === 'monthly' || !a.price?.cycle ? Number(a.price?.amount ?? 0) : 0), 0);
      const due7 = assets.filter((a) => a.due?.days != null && a.due.days <= 7);
      slot('kpis').innerHTML = [
        kpiTile({ key: 'plans', label: '套餐', value: kpiValue(fmt.int(plans.length)), sub: `${num(fmt.int(new Set(plans.map((p) => p.provider)).size))} 个供应商`, info: '规范套餐（套餐目录 catalog.json）；界面只显示规范名，不显示供应商原文' }),
        kpiTile({ key: 'assets', label: '资产', value: kpiValue(fmt.int(assets.length)), sub: `线路机 ${num(fmt.int(assets.filter((a) => a.category === 'relay').length))}`, info: '目录里的资产与旧系统实时账单合并；不在旧系统里的资产标“未登记”' }),
        kpiTile({ key: 'cost', label: '每月费用', value: kpiValue(fmt.money(Math.round(monthly * 100), { currency: 'USD' })), sub: `${num(fmt.int(assets.length))} 项服务`, info: '按资产登记的月付金额求和（目录价与登记价不同时以登记价为准）' }),
        kpiTile({ key: 'due7', label: '7 天内到期', value: kpiValue(fmt.int(due7.length)), sub: due7.length ? esc(due7.slice(0, 3).map((a) => a.short_name).join('、')) : '没有', info: '账期时区的日历天数（与旧界面相同：日期标签，不是按 24 小时取整）' }),
      ].join('');
      const byPlan = new Map();
      for (const a of assets) { if (!byPlan.has(a.plan_id)) byPlan.set(a.plan_id, []); byPlan.get(a.plan_id).push(a); }
      const pcols = [
        { key: 'name', label: '套餐', cell: (p) => `<span class="strong">${esc(p.canonical_name)}</span>`, sub: (p) => esc(p.provider_name ?? p.provider ?? '') },
        { key: 'cat', label: '类型', width: 90, cell: (p) => tagChip(p.category_word ?? p.category ?? '—') },
        { key: 'spec', label: '规格', cls: 'clip', cell: (p) => esc(stripInternal(p.spec_summary)), sub: (p) => esc(p.specs?.traffic_unlimited ? '流量不限' : stripInternal(p.specs?.traffic_text)) },
        { key: 'price', label: '目录价', width: 110, align: 'right', cell: (p) => `<span class="v">${num(money(p.price))}</span>` },
        { key: 'n', label: '资产', width: 72, align: 'right', cell: (p) => `<span class="v">${num(fmt.int(p.asset_count ?? byPlan.get(p.id)?.length ?? 0))}</span>`, sub: () => '台' },
      ];
      const expandPlan = (p) => {
        const list = (byPlan.get(p.id) ?? []).slice().sort((a, b) => (a.due?.days ?? 1e9) - (b.due?.days ?? 1e9));
        if (!list.length) return empty({ text: '这个套餐下没有资产' });
        const rows = list.map((a) => `<tr><td>${app.store.node(a.id) ? `<button type="button" class="cell-link" data-act="node" data-id="${esc(a.id)}">${esc(a.short_name)}</button>` : esc(a.short_name)}<span class="sub">${esc(a.team ?? '')}${a.in_legacy === false ? ' · 未登记' : ''}</span></td>
          <td class="r"><span class="v">${num(money(a.price))}</span>${a.price?.override ? '<span class="sub">登记价与目录价不同</span>' : ''}</td>
          <td class="r">${tonePill(dueF(a.due).text, tierTone(a.due?.days))}<span class="sub">${esc(a.due?.due_date ?? '—')}${a.due?.billing_timezone ? ` · ${esc(fmt.tzBadge?.(a.due.billing_timezone)?.text ?? a.due.billing_timezone)}` : ''}</span></td></tr>`).join('');
        return `<div class="table-wrap"><table class="dt is-two"><thead><tr><th>资产</th><th class="r">月费</th><th class="r">到期</th></tr></thead><tbody>${rows}</tbody></table></div>`;
      };
      slot('plans').innerHTML = panel({ title: '套餐', count: plans.length, sub: '点一行看名下资产', flush: true,
        body: dataTable({ columns: pcols, rows: plans, getId: (p) => p.id, rowLink: false, label: '套餐', expand: { isOpen: (p) => p.id === P.open, render: expandPlan }, empty: empty({ text: '套餐目录为空' }) }) });
      const relays = quotaData.relays ?? [];
      const qcols = [
        { key: 'relay', label: '线路机', width: 150, cell: (r) => `<a class="cell-link" href="#/lines?relay=${esc(r.id)}">${esc(r.relay)}</a>`, sub: (r) => esc(r.canonical_name ?? '') },
        { key: 'bar', label: '本期出站用量', cell: (r) => { const pct = r.used_pct; return `<div class="quota-cell">${quota({ pct: pct ?? 0, tone: quotaTone(pct) })}</div>`; },
          sub: (r) => `${esc(r.used_bytes != null ? `${fmt.num(Math.round(r.used_bytes / 1e9))} GB` : '—')} / ${esc(r.quota_gb != null ? `${fmt.num(r.quota_gb)} GB` : '不限')}${r.used_pct != null ? ` · ${num(fmt.pctResource(r.used_pct))}` : ''}` },
        { key: 'reset', label: '重置', width: 150, cell: (r) => esc(r.reset_day ? `每期第 ${r.reset_day} 天` : '按账期'), sub: (r) => esc(r.timezone ?? '') },
        { key: 'due', label: '到期', width: 90, align: 'right', cell: (r) => tonePill(fmt.countdown(r.due_days).text, tierTone(r.due_days)) },
        { key: 'at', label: '采样', width: 120, align: 'right', cell: (r) => (r.sampled_at ? txt(fmt.ago(Date.parse(r.sampled_at) / 1000)) : '—'), sub: () => '' },
      ];
      slot('quota').innerHTML = panel({ title: '线路机流量配额', count: relays.length, sub: esc(quotaData.alert ?? ''), flush: true,
        body: dataTable({ columns: qcols, rows: relays, getId: (r) => r.id, rowLink: false, label: '线路机流量配额', empty: empty({ text: '没有线路机配额数据' }) })
          + `<div class="panel-body">${note({ tone: 'info', title: '口径', body: `${esc(stripInternal(quotaData.source))}${(quotaData.caveats ?? []).filter((c) => !INTERNAL.test(c)).map((c) => `<br>· ${esc(c)}`).join('')}` })}</div>` });
      const soon = assets.filter((a) => a.due?.days != null).sort((a, b) => a.due.days - b.due.days).slice(0, 12);
      const dcols = [
        { key: 'name', label: '资产', cell: (a) => (app.store.node(a.id) ? `<button type="button" class="cell-link" data-act="node" data-id="${esc(a.id)}" data-tab="billing">${esc(a.short_name)}</button>` : esc(a.short_name)), sub: (a) => esc(a.canonical_name ?? '') },
        { key: 'due', label: '到期', width: 120, align: 'right', cell: (a) => tonePill(dueF(a.due).text, tierTone(a.due.days)), sub: (a) => esc(a.due.due_date ?? '') },
        { key: 'tz', label: '账期时区', width: 160, cell: (a) => esc(a.due.billing_timezone ?? '—'), sub: (a) => esc(a.due.billing_cycle ?? '') },
        { key: 'price', label: '月费', width: 110, align: 'right', cell: (a) => `<span class="v">${num(money(a.price))}</span>` },
      ];
      slot('due').innerHTML = panel({ title: '续费到期', count: soon.length, sub: '由近到远，最近 12 项；登记续费在节点详情的“费用”页签', flush: true,
        body: dataTable({ columns: dcols, rows: soon, getId: (a) => a.id, rowLink: false, label: '续费到期', empty: empty({ icon: 'calendar', text: '没有到期信息' }) }) });
      hydrate(el);
    }
    const offs = [watchStuck(el)];
    offs.push(on(el, 'click', '[data-act]', (e, b) => {
      e.stopPropagation();
      if (b.dataset.act === 'node') app.openNode(b.dataset.id, b.dataset.tab ?? 'overview');
      else if (b.dataset.act === 'refresh') { b.classList.add('is-busy'); load().finally(() => b.classList.remove('is-busy')); }
    }));
    offs.push(on(el, 'click', '[data-slot="plans"] tr[data-row]', (e, tr) => { if (!e.target.closest('button, a')) app.setParams({ open: P.open === tr.dataset.row ? null : tr.dataset.row }); }));
    offs.push(onFleet(({ resource }) => { if (resource === 'fleet-down' && el.isConnected) { slot('kpis')?.remove(); slot('plans').innerHTML = fleetAbsent('套餐与配额'); slot('quota').innerHTML = ''; slot('due').innerHTML = ''; } }));
    // 用量与到期随旧系统数据变（每 5 分钟重取一次，不跟每次轮询）
    const t = setInterval(() => { if (el.isConnected) load(); }, 300000);
    load();
    return {
      update(p) { P = readP(p); render(); },
      unmount() { clearInterval(t); offs.forEach((f) => f()); },
    };
  },
};
