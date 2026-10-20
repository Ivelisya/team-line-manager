// pages/notify.js — 通知 #/notify（新页面，fleetd：/v2/api/v1/channels、notify-log，fleet/API.md §4）。
// 版面：页头 → 影子模式提示条（没配渠道时）→ 渠道面板（机密已打码，fleetd 只给主机名 / 打码的 chat id）→ 通知记录（状态分段，展开看全文）。
// SSE 推 notify 变化时重取。渠道的增删改在 fleetd 配置文件里（这一页只读）。URL：status= open=。
import { fmt } from '../../../shared/index.js';
import {
  esc, num, txt, on, pageHead, watchStuck, freshness, panel, dataTable, empty, note, button, tonePill, tagChip,
  segmented, onSegment, dlRows, hydrate, tableSkeleton,
} from '../components.js';
import { refreshBtn } from '../page-tools.js';
import { fleetAvailable, fleetGet, onFleet, fleetAbsent } from '../fleet.js';
import { summaryWord } from './alerts.js';
import { swrGet, swrSet } from '../swr.js';

const TYPE = { telegram: 'Telegram', wecom: '企业微信', smtp: '邮件', webhook: 'Webhook', shadow: '影子' };
const SEV = { critical: ['严重', 'bad'], warning: ['警告', 'warn'], info: ['提示', 'info'] };
const KIND = { group: '分组', repeat: '重复提醒', digest: '每日摘要' };
const STATUS = { sent: ['已发送', 'ok'], failed: ['失败', 'bad'], shadow: ['只记录', 'neutral'] };
const FILTERS = [{ value: '', label: '全部' }, { value: 'shadow', label: '只记录' }, { value: 'sent', label: '已发送' }, { value: 'failed', label: '失败' }];
const readP = (p) => ({ status: FILTERS.some((f) => f.value === p.get('status')) ? p.get('status') : '', open: p.get('open') ?? '' });

export default {
  title: '通知',
  density: true,
  mount({ el, app, params }) {
    let P = readP(params);
    let ch = null, log = null, err = null;
    el.innerHTML = `<div class="page" data-page="notify">
      ${pageHead({ title: '通知', tools: freshness() + refreshBtn() })}
      <div data-slot="banner"></div>
      <div data-slot="channels">${panel({ title: '通知渠道', flush: true, body: tableSkeleton(5, 2) })}</div>
      <div data-slot="log">${panel({ title: '通知记录', flush: true, body: tableSkeleton(5, 6) })}</div>
    </div>`;
    const slot = (k) => el.querySelector(`[data-slot="${k}"]`);
    if (!fleetAvailable()) { slot('channels').innerHTML = fleetAbsent('告警通知'); slot('log').innerHTML = ''; return { unmount() {} }; }

    async function load() {
      const key = `notify:${P.status ?? ''}`;
      if (!ch && !log) { const c = swrGet(key); if (c) { ({ ch, log } = c); if (el.isConnected) render(); } }
      try {
        [ch, log] = await Promise.all([fleetGet('/channels'), fleetGet('/notify-log', { limit: 200, ...(P.status ? { status: P.status } : {}) })]);
        err = null;
        swrSet(key, { ch, log });
      } catch (e) { err = e.message; }
      if (el.isConnected) render();
    }
    function render() {
      if (err) { slot('channels').innerHTML = panel({ title: '通知渠道', body: note({ tone: 'bad', title: '读取失败', body: esc(err), action: button({ label: '重试', size: 'sm', attrs: { 'data-act': 'refresh' } }) }) }); slot('log').innerHTML = ''; return; }
      const shadow = ch?.shadow || log?.shadow;
      slot('banner').innerHTML = shadow ? note({ tone: 'warn', title: '尚未配置通知渠道：告警只记录不外发', body: esc(ch?.note ?? '所有通知只写入下面的通知记录（只记录），不会发到手机或群里。') }) : '';
      const ccols = [
        { key: 'name', label: '渠道', cell: (c) => `<span class="strong">${esc(c.name)}</span>`, sub: (c) => esc(TYPE[c.type] ?? c.type) },
        { key: 'target', label: '发往', cell: (c) => `<span class="mono">${esc(c.target ?? '—')}</span>`, sub: () => '机密不在界面显示' },
        { key: 'min', label: '最低级别', width: 100, cell: (c) => { const [w, t] = SEV[c.min_severity] ?? [c.min_severity ?? '全部', 'neutral']; return tonePill(w, t, { dot: 'none' }); } },
        { key: 'st', label: '最近', width: 150, cell: (c) => { const s = c.state ?? {}; const [w, t] = STATUS[s.last_status] ?? ['还没发过', 'neutral']; return tonePill(w, t); }, sub: (c) => (c.state?.last_at ? txt(fmt.ago(c.state.last_at)) : '') },
        { key: 'n', label: '已发送 / 失败', width: 130, align: 'right', cell: (c) => `<span class="v">${num(fmt.int(c.state?.sent ?? 0))}</span> / ${num(fmt.int(c.state?.failed ?? 0))}`, sub: (c) => (c.state?.consecutive_failures ? `<span class="t-bad">连续失败 ${esc(c.state.consecutive_failures)} 次</span>` : '') },
      ];
      slot('channels').innerHTML = panel({ title: '通知渠道', count: ch?.channels?.length ?? 0, sub: ch?.healthchecks ? '死人开关已开启' : '', flush: true,
        body: dataTable({ columns: ccols, rows: ch?.channels ?? [], getId: (c) => c.name, rowLink: false, label: '通知渠道', empty: empty({ icon: 'bell-off', text: '没有配置通知渠道（影子模式）' }) }) });
      const lcols = [
        { key: 'ts', label: '时间', width: 130, cell: (x) => txt(fmt.ago(x.ts)), sub: (x) => esc(fmt.stamp(x.ts, { seconds: true }).text) },
        { key: 'sev', label: '级别', width: 76, cell: (x) => { const [w, t] = SEV[x.severity] ?? [x.severity ?? '—', 'neutral']; return tonePill(w, t); } },
        { key: 'title', label: '内容', cls: 'clip', cell: (x) => `<span class="strong">${esc(summaryWord(x.title))}</span>`, sub: (x) => esc([KIND[x.kind] ?? x.kind, x.alert_ids?.length ? `${x.alert_ids.length} 个事项` : null].filter(Boolean).join(' · ')) },
        { key: 'ch', label: '渠道', width: 120, cell: (x) => esc(x.channel === 'shadow' ? '影子' : x.channel ?? '—') },
        { key: 'st', label: '结果', width: 110, cell: (x) => { const [w, t] = STATUS[x.status] ?? [x.status, 'neutral']; return tonePill(w, t); }, sub: (x) => (x.latency_ms != null ? `${num(fmt.int(x.latency_ms))} ms` : '') },
      ];
      const rows = log?.notifications ?? [];
      slot('log').innerHTML = panel({ title: '通知记录', count: rows.length, flush: true, sub: '保留 90 天',
        tools: segmented({ name: 'notify-status', size: 'sm', items: FILTERS, value: P.status, label: '结果' }),
        body: dataTable({ columns: lcols, rows, getId: (x) => String(x.id), rowLink: false, label: '通知记录',
          expand: { isOpen: (x) => String(x.id) === P.open, render: (x) => `${x.error ? note({ tone: x.status === 'failed' ? 'bad' : 'info', body: esc(x.error) }) : ''}<pre class="codeblock">${esc(x.body ?? '')}</pre>${dlRows([{ label: '分组', value: `<span class="mono">${esc(x.group_key ?? '—')}</span>` }, { label: '事项', value: esc((x.alert_ids ?? []).join('、') || '—') }])}` },
          empty: empty({ icon: 'inbox', text: '还没有通知记录' }) }) });
      hydrate(el);
    }
    const offs = [watchStuck(el)];
    offs.push(onSegment(el, 'notify-status', (v) => app.setParams({ status: v || null, open: null })));
    offs.push(on(el, 'click', '[data-act="refresh"]', (e, b) => { b.classList.add('is-busy'); load().finally(() => b.classList.remove('is-busy')); }));
    offs.push(on(el, 'click', '[data-slot="log"] tr[data-row]', (e, tr) => { if (!e.target.closest('button, a')) app.setParams({ open: P.open === tr.dataset.row ? null : tr.dataset.row }); }));
    offs.push(onFleet(({ resource }) => { if (resource === 'notify') load(); if (resource === 'fleet-down' && el.isConnected) { slot('channels').innerHTML = fleetAbsent('告警通知'); slot('log').innerHTML = ''; } }));
    load();
    void tagChip;
    return {
      update(p) { const prev = P; P = readP(p); if (prev.status !== P.status) load(); else render(); },
      unmount() { offs.forEach((f) => f()); },
    };
  },
};
