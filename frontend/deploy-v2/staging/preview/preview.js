// preview.js — staging smoke page: the prototype store on the live adapter, patched by the poller.
import { fmt, loadSnapshot, createPatcher } from './shared/index.js';
import { createLiveFetch, createLivePoller, AUTH_LINK } from './live/adapter.js';

const $ = (id) => document.getElementById(id);
const esc = fmt.esc;
const patch = createPatcher();
let requests = 0;

function showBanner(html) { $('banner').innerHTML = html; $('banner').style.display = 'block'; }

const liveFetch = createLiveFetch({
  fetchImpl: (...a) => { requests++; return fetch(...a); },
  onAuthError: ({ message }) => showBanner(`${esc(message)}：<a href="${AUTH_LINK}">登录管理员账户</a>后刷新本页`),
});

function cell(tr, key, text, tone) {
  const td = tr.querySelector(`[data-k="${key}"]`);
  if (!td) return;
  if (!patch.changed(tr.dataset.id + ':' + key, text + '|' + (tone ?? ''))) return;
  td.textContent = text;
  td.className = (td.dataset.num ? 'num ' : '') + (tone ? 'tone-' + tone : '');
  td.classList.add('flash');
  setTimeout(() => td.classList.remove('flash'), 800);
}

function paintRow(store, n) {
  const tr = document.querySelector(`tr[data-id="${n.id}"]`);
  const h = fmt.health(n.status?.code, { stale: store.staleOf(n) });
  cell(tr, 'status', h.word + (store.attentionOf(n) ? ' · 需处理' : ''), h.tone);
  cell(tr, 'rate', n.rate ? fmt.ratePair(n.rate.down_bps, n.rate.up_bps).text : '—');
  cell(tr, 'today', fmt.bytes((n.traffic?.today?.down ?? 0) + (n.traffic?.today?.up ?? 0)).text);
  const tcp = fmt.latency(n.latency?.tcp_ms);
  cell(tr, 'tcp', tcp.text, fmt.latencyTone(n.latency?.tcp_ms, 'tcp', n.limits) ?? tcp.tone);
  cell(tr, 'egress', fmt.latency(n.latency?.egress_ms).text, fmt.latencyTone(n.latency?.egress_ms, 'egress', n.limits));
  cell(tr, 'cpu', n.host ? fmt.pctResource(n.host.cpu_pct).text : '无主机', n.host ? null : 'muted');
  cell(tr, 'avail', fmt.pctAvail(n.availability?.windows?.['30']?.availability_percent).text);
  const due = fmt.due(n.billing, store.now());
  cell(tr, 'due', due.text, due.tone);
}

function paintHead(store, live) {
  const k = store.kpis();
  $('asof').textContent = `数据时刻 ${fmt.absolute(store.asOf)}`;
  const age = live?.age();
  const f = fmt.freshnessPill(age ?? null, 'monitor');
  $('fresh').textContent = age == null ? '实时' : f.text;
  $('fresh').dataset.tone = age == null ? 'ok' : f.tone ?? '';
  $('req').textContent = `已请求 ${requests} 次`;
  const tiles = [
    ['正常', `${k.normal.ok}/${k.normal.total}`],
    ['需处理', String(k.attention)],
    ['业务速率', fmt.ratePair(k.business_rate.down_bps, k.business_rate.up_bps).text],
    ['今日', fmt.bytes(k.today_business.total).text],
    ['本月', fmt.bytes(k.month_business.total).text],
    ['月费', fmt.money(k.monthly_cost.cents).text],
    ['7 天内到期', String(k.due_7d.count)],
    ['今日访问记录', fmt.int(k.events_today).text],
  ];
  $('kpis').innerHTML = tiles.map(([l, v]) => `<div class="kpi"><b>${esc(v)}</b><span>${esc(l)}</span></div>`).join('');
}

(async function boot() {
  let store;
  try {
    store = await loadSnapshot({ base: new URL('./data/', import.meta.url), fetch: liveFetch });
  } catch (e) {
    if ($('banner').style.display !== 'block') showBanner(`读取失败：${esc(e.message)}`);
    $('asof').textContent = '读取失败';
    return;
  }
  $('rows').innerHTML = store.nodes().map((n) => `<tr data-id="${esc(n.id)}"><td>${esc(n.name)}</td><td data-k="status"></td><td>${esc(n.entry_label ?? '—')}</td>
    <td data-k="rate" data-num="1"></td><td data-k="today" data-num="1"></td><td data-k="tcp" data-num="1"></td><td data-k="egress" data-num="1"></td>
    <td data-k="cpu" data-num="1"></td><td data-k="avail" data-num="1"></td><td data-k="due" data-num="1"></td></tr>`).join('');
  const live = createLivePoller(store, liveFetch, { onError: (e) => { if (!e.auth) showBanner(`刷新失败：${esc(e.message)}（稍后自动重试）`); } }).start();
  for (const n of store.nodes()) paintRow(store, n);
  paintHead(store, live);
  store.on('change', (d) => {
    if (d.fleet?.reload) showBanner('节点或入口有增减，刷新页面以显示新的行');
    for (const id of Object.keys(d.nodes ?? {})) { const n = store.node(id); if (n) paintRow(store, n); }
    paintHead(store, live);
  });
  setInterval(() => paintHead(store, live), 1000);
})();
