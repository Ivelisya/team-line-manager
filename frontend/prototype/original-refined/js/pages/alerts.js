// pages/alerts.js — 事项与告警 #/alerts（新页面，数据来自 fleetd：/v2/api/v1/alerts、silences、maintenance，fleet/API.md §4）。
// 版面只用已有组件：页头 → 影子模式提示条 → 4 张 KPI → 事项面板（状态分段 · 分组分段 · 表格，展开行看证据）→ 静默与维护窗口面板。
// 实时：SSE 推 alerts / silences 变化时重取（fleet.js onFleet），不轮询。确认、静默、维护窗口是 fleetd 自己的写（X-Fleet-Request）。
// URL：state= group= open=（展开的事项 id）。
import { fmt } from '../../../shared/index.js';
import {
  esc, num, txt, on, pageHead, watchStuck, freshness, panel, dataTable, empty, note, button, iconButton, tonePill, tagChip,
  segmented, onSegment, kpiTile, kpiValue, countDots, dlRows, openDialog, confirmAction, toast, hydrate, tableSkeleton,
} from '../components.js';
import { refreshBtn } from '../page-tools.js';
import { fleetAvailable, fleetGet, fleetWrite, onFleet, fleetAbsent } from '../fleet.js';
import { guard } from '../live-mode.js';
import { swrGet, swrSet } from '../swr.js';

const SEV = { critical: ['严重', 'bad'], warning: ['警告', 'warn'], info: ['提示', 'info'] };
const STATE = { firing: ['进行中', 'bad'], pending: ['待定', 'warn'], resolved: ['已结束', 'ok'] };
const STATES = [{ value: 'open', label: '进行中' }, { value: 'unacked', label: '未确认' }, { value: 'acked', label: '已确认' }, { value: 'resolved', label: '已结束' }, { value: 'all', label: '全部' }];
const GROUPS = [{ value: '', label: '不分组' }, { value: 'relay', label: '按线路机' }, { value: 'cause', label: '按原因' }];
const RULE = {
  node_health_fail: '节点故障', node_health_degraded: '节点降级', egress_mismatch: '出口变更', egress_route_leak: '出口泄漏到线路机', host_offline: '主机离线', host_resource: '主机资源',
  host_sync_stale: '主机同步停滞', relay_down: '线路机宕机', relay_mgmt_down: '管理通道中断', relay_xray_down: 'Xray 未运行', collector_stale: '采集停滞',
  monitor_stale: '健康评估停滞', correlated_node_failure: '关联故障', renewal_due: '续费到期', quota: '流量配额', legacy_source_down: '数据源不可达', channel_failing: '通知渠道失败',
  // IP 情报与原生探测（fleetd 0.4+）
  ipintel_new_isp: '新运营商', ipintel_new_city: '新城市', ipintel_shared_identity: '多地接入', ipintel_datacenter_source: '机房来源', ipintel_egress_ip_changed: '出口 IP 变更',
  native_host_offline: '主机离线', native_host_resource: '资源紧张', native_cpu_steal: 'CPU 被宿主占用', native_cpu_iowait: '磁盘等待过高',
};
const SUPP = (s) => (!s ? '' : s.startsWith('silence:') ? '已静默' : s.startsWith('maintenance:') ? '维护中' : s.startsWith('inhibited:') ? '被上级事项压住' : s.startsWith('aggregated:') ? '并入关联故障' : s);
const LABEL = { node: '节点', relay: '线路机', relay_id: '线路机 id', provider: '供应商', team: '团队', cause: '原因', kind: '类型', check: '检查项', entry: '入口', channel: '渠道', asset: '资产', threshold: '档位', scope: '范围' };
const readP = (p) => ({ state: STATES.some((s) => s.value === p.get('state')) ? p.get('state') : 'open', group: GROUPS.some((g) => g.value === p.get('group')) ? p.get('group') : '', open: p.get('open') ?? '' });
// 与 fleetd alerts.rs CAUSE_LABELS 同一份词表（接口带 cause_label / rule_label 时以接口为准）
const CAUSE = { relay_down: '线路机宕机', correlated_node_failure: '关联故障', quota: '流量配额', legacy_unreachable: '数据源不可达', komari_unreachable: 'Komari 不可达',
  mgmt_down: '管理通道中断', xray_down: 'Xray 未运行', renewal: '续费到期', collector_stale: '采集停滞', monitor_stale: '健康评估停滞',
  xray_process_down: 'Xray 进程退出', xray_api_unreachable: 'Xray 接口不可达', relayd_silent: 'relayd 无上报', relayd_silent_nodes_failing: 'relayd 无上报',
  timeout: '超时', auth: '认证失败', upstream_down: '上游不通', policy_blocked: '规则阻止', probe_offline: '探针离线', stale: '数据过期', unstable: '不稳定',
  retrying: '重试中', unknown: '未知', mem: '内存', disk: '磁盘', cpu: 'CPU' };
/** 代码 → 中文（未知代码原样返回）。 */
export const codeWord = (c) => (c == null ? null : fmt.REASONS?.[c] ?? CAUSE[c] ?? RULE[c] ?? c);
const causeWord = (a) => { const c = a.labels?.cause; return a.cause_label ?? (c ? codeWord(c) : fmt.checkTitle(a.labels?.check) ?? '—'); };
const ruleWord = (a) => a.rule_label ?? RULE[a.rule] ?? (a.title && a.title !== '告警' ? a.title : null) ?? codeWord(a.rule);
/** 摘要里夹着的英文代码（“降级：upstream_down”）换成中文。 */
export const summaryWord = (t) => String(t ?? '').replace(/\b[a-z][a-z0-9]*(?:_[a-z0-9]+)+\b|(?<=[：:]\s?)\b(timeout|auth|stale|unstable|retrying|unknown)\b/g, (m) => codeWord(m));
const localInput = (t) => { const d = new Date(t * 1000); const p = (x) => String(x).padStart(2, '0'); return `${d.getFullYear()}-${p(d.getMonth() + 1)}-${p(d.getDate())}T${p(d.getHours())}:${p(d.getMinutes())}`; };

export default {
  title: '事项与告警',
  density: true,
  mount({ el, app, params }) {
    let P = readP(params);
    let data = null, windows = null, err = null;
    el.innerHTML = `<div class="page" data-page="alerts">
      ${pageHead({ title: '事项与告警', tools: freshness() + refreshBtn() })}
      <div data-slot="banner"></div>
      <section class="kpi-grid" data-slot="kpis">${['进行中', '未确认', '已压住', '严重'].map((l) => kpiTile({ key: l, label: l, loading: true })).join('')}</section>
      <div data-slot="list">${panel({ title: '事项', flush: true, body: tableSkeleton(6, 5) })}</div>
      <div data-slot="windows"></div>
    </div>`;
    const slot = (k) => el.querySelector(`[data-slot="${k}"]`);
    if (!fleetAvailable()) {
      slot('kpis').remove(); slot('list').innerHTML = fleetAbsent('事项与告警');
      return { unmount() {} };
    }

    async function load() {
      // 先用本机上次的结果画出来（不出骨架），新数据到了再换上
      const key = `alerts:${P.state}`;
      if (!data) { const c = swrGet(key); if (c) { data = c.data; windows = c.windows; if (el.isConnected) render(); } }
      try {
        const [a, s, m] = await Promise.all([fleetGet('/alerts', { state: P.state, limit: 500 }), fleetGet('/silences', { all: 1 }), fleetGet('/maintenance', { all: 1 })]);
        data = a; windows = [...(s?.silences ?? []), ...(m?.maintenance ?? [])].filter((w, i, arr) => arr.findIndex((x) => x.id === w.id && x.kind === w.kind) === i);
        err = null;
        swrSet(key, { data, windows });
      } catch (e) { err = e.message; }
      if (el.isConnected) render();
    }

    function kpis() {
      const c = data?.counts ?? {};
      const unacked = Math.max(0, (c.firing ?? 0) - (c.acked ?? 0));
      return [
        kpiTile({ key: 'firing', label: '进行中', value: kpiValue(fmt.int(c.firing ?? 0)), sub: countDots([['严重', c.critical ?? 0, 'bad'], ['警告', c.warning ?? 0, 'warn']], { hideZero: false }), info: '已触发、还没结束的事项（pending = 条件成立但还没到持续时间，不算在内）' }),
        kpiTile({ key: 'unacked', label: '未确认', value: kpiValue(fmt.int(unacked)), sub: `已确认 ${num(fmt.int(c.acked ?? 0))}`, info: '确认后不再重复提醒（严重 30 分钟、警告 4 小时一次）' }),
        kpiTile({ key: 'supp', label: '已压住', value: kpiValue(fmt.int(c.suppressed ?? 0)), sub: '静默 · 维护 · 上级事项', info: '被静默、维护窗口或上级事项（例如线路机宕机）压住：照常记录，只是不通知' }),
        kpiTile({ key: 'pending', label: '待定', value: kpiValue(fmt.int(c.pending ?? 0)), sub: `提示 ${num(fmt.int(c.info ?? 0))}`, info: '条件已成立、还没到持续时间；提示级只进每天 09:00 的摘要' }),
      ].join('');
    }

    function objectCell(a) {
      const L = a.labels ?? {};
      if (L.node && app.store.node(L.node)) return `<button type="button" class="cell-link" data-act="node" data-id="${esc(L.node)}">${esc(L.node_name ?? app.store.node(L.node).name)}</button>`;
      if (L.relay_id) return `<a class="cell-link" href="#/lines?relay=${esc(L.relay_id)}">${esc(L.relay ?? L.relay_id)}</a>`;
      return esc(L.node_name ?? L.relay ?? L.channel ?? L.asset ?? '全局');
    }
    function stateCell(a) {
      const [w, t] = a.state === 'firing' && a.acked ? ['已确认', 'neutral'] : STATE[a.state] ?? [a.state, 'neutral'];
      return tonePill(w, t, { dot: a.state === 'firing' && !a.acked ? 'pulse' : 'solid' });
    }
    function evidence(a) {
      const ev = Object.entries(a.evidence ?? {}).filter(([, v]) => v != null && typeof v !== 'object').map(([k, v]) => ({ label: k, value: `<span class="mono">${esc(String(v))}</span>` }));
      const labels = Object.entries(a.labels ?? {}).filter(([k, v]) => v != null && LABEL[k]).map(([k, v]) => ({ label: LABEL[k], value: esc(String(v)) }));
      const times = [
        { label: '条件开始', value: esc(fmt.stamp(a.since, { seconds: true }).text) },
        a.fired_at ? { label: '触发', value: esc(fmt.stamp(a.fired_at, { seconds: true }).text) } : null,
        a.ack_at ? { label: '确认', value: `${esc(fmt.stamp(a.ack_at).text)} · ${esc(a.ack_by ?? '')}` } : null,
        a.resolved_at ? { label: '结束', value: esc(fmt.stamp(a.resolved_at, { seconds: true }).text) } : null,
        { label: '通知', value: a.notified ? esc({ firing: '已通知（进行中）', resolved: '已通知（结束）' }[a.notified] ?? a.notified) : '<span class="t-muted">还没通知</span>' },
        a.suppressed ? { label: '压住', value: esc(`${SUPP(a.suppressed)}（${a.suppressed}）`) } : null,
      ];
      return `<div class="dl-grid">${dlRows([...times, ...labels].filter(Boolean))}${dlRows(ev.length ? ev : [{ label: '证据', value: '<span class="t-muted">无</span>' }])}</div>
        <div class="expand-actions"><span class="faint">${esc(summaryWord(a.summary))}</span>${a.labels?.node ? button({ label: '打开节点详情', size: 'sm', attrs: { 'data-act': 'node', 'data-id': a.labels.node } }) : ''}${a.state !== 'resolved' && !a.acked ? button({ label: '确认', size: 'sm', variant: 'primary', attrs: { 'data-act': 'ack', 'data-id': a.id } }) : ''}</div>`;
    }
    const columns = [
      { key: 'sev', label: '级别', width: 80, cell: (a) => { const [w, t] = SEV[a.severity] ?? [a.severity, 'neutral']; return tonePill(w, t); } },
      { key: 'title', label: '事项', cls: 'clip', cell: (a) => `<span class="strong">${esc(ruleWord(a))}</span>`, sub: (a) => esc(summaryWord(a.summary)) },
      { key: 'obj', label: '对象', width: 150, cell: objectCell, sub: (a) => esc(a.labels?.relay && a.labels?.node ? a.labels.relay : a.labels?.team ?? '') },
      { key: 'cause', label: '原因', width: 130, cell: (a) => esc(causeWord(a)), sub: (a) => esc(fmt.checkTitle(a.labels?.check) ?? '') },
      { key: 'since', label: '持续', width: 120, align: 'right', cell: (a) => `<span class="v">${esc(fmt.duration(Math.max(0, (a.resolved_at ?? app.store.now()) - a.since), { units: 1 }).text)}</span>`, sub: (a) => esc(fmt.stamp(a.since).text) },
      { key: 'state', label: '状态', width: 110, cell: stateCell, sub: (a) => (a.suppressed ? esc(SUPP(a.suppressed)) : '') },
      { key: 'act', label: '', width: 72, align: 'right', cell: (a) => (a.state !== 'resolved' && !a.acked ? button({ label: '确认', size: 'sm', attrs: { 'data-act': 'ack', 'data-id': a.id } }) : '') },
    ];
    function render() {
      if (err) { slot('list').innerHTML = panel({ title: '事项', body: note({ tone: 'bad', title: '读取失败', body: esc(err), action: button({ label: '重试', size: 'sm', attrs: { 'data-act': 'refresh' } }) }) }); return; }
      slot('banner').innerHTML = data?.shadow ? note({ tone: 'warn', title: '尚未配置通知渠道：告警只记录不外发', body: '事项照常判定与记录；配置渠道前不会发到手机或群里。', action: button({ label: '查看通知', size: 'sm', attrs: { 'data-act': 'goto', 'data-href': '#/notify' } }) }) : '';
      slot('kpis').innerHTML = kpis();
      const rows = data?.alerts ?? [];
      const keyOf = (a) => (P.group === 'relay' ? a.labels?.relay ?? '不属于线路机' : a.labels?.cause ? causeWord(a) : ruleWord(a));
      let sections = null;
      if (P.group) {
        const m = new Map();
        for (const a of rows) { const k = keyOf(a); if (!m.has(k)) m.set(k, []); m.get(k).push(a); }
        sections = [...m].map(([title, list]) => ({ title, count: list.length, rows: list }));
      }
      const tools = segmented({ name: 'alert-state', size: 'sm', items: STATES, value: P.state, label: '状态' }) + segmented({ name: 'alert-group', size: 'sm', items: GROUPS, value: P.group, label: '分组' });
      slot('list').innerHTML = panel({ title: '事项', count: rows.length, flush: true, tools, cls: 'is-wrap',
        body: dataTable({ columns, rows, sections, getId: (a) => String(a.id), rowLink: false, label: '事项', expand: { isOpen: (a) => String(a.id) === P.open, render: evidence },
          empty: empty({ icon: 'circle-check', cls: 'is-ok', text: P.state === 'open' ? '当前没有进行中的事项' : '没有符合条件的事项' }) }) });
      renderWindows();
      hydrate(el);
    }
    function renderWindows() {
      const list = (windows ?? []).filter((w) => w.status !== 'deleted').sort((a, b) => (a.status === 'active' ? -1 : 0) - (b.status === 'active' ? -1 : 0) || b.starts_at - a.starts_at);
      const cols = [
        { key: 'kind', label: '类型', width: 90, cell: (w) => tagChip(w.kind === 'maintenance' ? '维护窗口' : '静默') },
        { key: 'm', label: '范围', cell: (w) => esc((w.matchers ?? []).map((x) => `${LABEL[x.name] ?? x.name}${x.negate ? ' ≠ ' : ' = '}${x.value}`).join(' 且 ')), sub: (w) => esc(w.comment ?? '') },
        { key: 't', label: '时间', width: 210, cell: (w) => `${esc(fmt.stamp(w.starts_at).text)} – ${esc(fmt.stamp(w.ends_at).text)}`, sub: (w) => esc(w.tz ?? '') },
        { key: 's', label: '状态', width: 90, cell: (w) => tonePill({ active: '生效中', scheduled: '未开始', expired: '已过期', deleted: '已删除' }[w.status] ?? w.status, w.status === 'active' ? 'info' : 'neutral') },
        { key: 'by', label: '创建', width: 110, cell: (w) => esc(w.created_by ?? '—'), sub: (w) => esc(fmt.stamp(w.created_at).text) },
        { key: 'a', label: '', width: 56, align: 'right', cell: (w) => (w.status === 'active' || w.status === 'scheduled' ? iconButton({ icon: 'trash-2', label: '结束并删除', size: 'sm', attrs: { 'data-act': 'del-window', 'data-id': w.id, 'data-kind': w.kind } }) : '') },
      ];
      slot('windows').innerHTML = panel({ title: '静默与维护窗口', count: list.filter((w) => w.status === 'active').length || null, sub: '被匹配的事项照常记录，只是不通知；最长 7 天', flush: true,
        tools: button({ label: '新建静默', icon: 'bell-off', size: 'md', attrs: { 'data-act': 'new-window', 'data-kind': 'silence' } }) + button({ label: '新建维护窗口', icon: 'wrench', size: 'md', attrs: { 'data-act': 'new-window', 'data-kind': 'maintenance' } }),
        body: dataTable({ columns: cols, rows: list, getId: (w) => `${w.kind}:${w.id}`, rowLink: false, label: '静默与维护窗口', empty: empty({ icon: 'bell-off', text: '没有生效或计划中的静默与维护窗口' }) }) });
    }

    async function ack(id, b) {
      if (guard('alerts.ack')) return;
      b?.setAttribute('aria-busy', 'true');
      try { await fleetWrite('POST', `/alerts/${encodeURIComponent(id)}/ack`, {}); toast({ message: '已确认；不再重复提醒，恢复仍由真实测量判断' }); await load(); }
      catch (e) { if (!e.auth) toast({ message: `操作失败：${e.message}`, tone: 'bad' }); }
      finally { b?.removeAttribute('aria-busy'); }
    }
    function newWindow(kind) {
      if (guard(kind === 'maintenance' ? 'alerts.maintenance' : 'alerts.silence')) return;
      const relays = app.store.relays();
      const nodes = app.store.nodes().filter((n) => !n.isRelay);
      const now = Date.now() / 1000;
      openDialog({
        title: kind === 'maintenance' ? '新建维护窗口' : '新建静默', sub: kind === 'maintenance' ? '计划内的停机、换机房：窗口内匹配的事项不通知' : '已知问题暂时不提醒', size: 'md', guard: true,
        body: `<form data-form novalidate><div class="fields">
          <label class="field"><span class="field-label">范围</span><select class="select" name="name">
            <option value="relay">线路机</option><option value="node">节点</option><option value="team">团队</option><option value="provider">供应商</option><option value="cause">原因</option></select></label>
          <label class="field"><span class="field-label">对象</span><select class="select" name="value" data-values></select></label>
          <label class="field"><span class="field-label">开始（北京时间）</span><input class="input" type="datetime-local" name="starts" value="${localInput(now)}"></label>
          <label class="field"><span class="field-label">时长</span><select class="select" name="dur">${[[60, '1 小时'], [120, '2 小时'], [240, '4 小时'], [480, '8 小时'], [1440, '1 天'], [4320, '3 天'], [10080, '7 天']].map(([v, l]) => `<option value="${v}"${v === 120 ? ' selected' : ''}>${l}</option>`).join('')}</select></label>
          <label class="field is-wide"><span class="field-label">说明</span><input class="input" name="comment" maxlength="200" placeholder="例如 线路机1 换机房"></label>
        </div><p class="field-error" data-err hidden></p></form>`,
        foot: button({ label: '取消', attrs: { 'data-dlg': 'cancel' } }) + button({ label: '创建', variant: 'primary', attrs: { 'data-create': '' } }),
        init(dlg, api) {
          const f = dlg.querySelector('[data-form]');
          const opts = {
            relay: relays.map((r) => [r.name, r.name]), node: nodes.map((n) => [n.id, `${n.name}`]),
            team: [...new Set(nodes.map((n) => n.team))].map((t) => [t, t]), provider: [...new Set(app.store.nodes().map((n) => n.provider).filter(Boolean))].map((p) => [p, p]),
            cause: Object.entries(fmt.REASONS ?? {}).map(([k, v]) => [k, v]),
          };
          const fill = () => { f.elements.value.innerHTML = (opts[f.elements.name.value] ?? []).map(([v, l]) => `<option value="${esc(v)}">${esc(l)}</option>`).join(''); };
          fill();
          f.elements.name.addEventListener('change', fill);
          const err = dlg.querySelector('[data-err]');
          const btn = dlg.querySelector('[data-create]');
          btn.addEventListener('click', async () => {
            err.hidden = true;
            const startsAt = f.elements.starts.value ? f.elements.starts.value.replace('T', ' ') : undefined;
            const body = { matchers: [{ name: f.elements.name.value, value: f.elements.value.value, negate: false }], starts_at: startsAt, duration_minutes: Number(f.elements.dur.value), tz: 'Asia/Shanghai', comment: f.elements.comment.value.trim() };
            if (!body.matchers[0].value) { err.hidden = false; err.textContent = '请选择对象'; return; }
            btn.setAttribute('aria-busy', 'true'); btn.disabled = true;
            try {
              await fleetWrite('POST', kind === 'maintenance' ? '/maintenance' : '/silences', body);
              api.markDirty(false); api.close();
              toast({ message: kind === 'maintenance' ? '已新建维护窗口' : '已新建静默' });
              load();
            } catch (e) { if (!e.auth) { err.hidden = false; err.textContent = e.message; } }
            finally { btn.removeAttribute('aria-busy'); btn.disabled = false; }
          });
        },
      });
    }
    async function delWindow(id, kind) {
      if (guard('alerts.deleteWindow')) return;
      const ok = await confirmAction({ title: kind === 'maintenance' ? '结束维护窗口' : '结束静默', body: '结束后匹配的事项恢复正常通知。', confirm: '结束', danger: true, action: 'alerts.deleteWindow' });
      if (!ok) return;
      try { await fleetWrite('DELETE', `/${kind === 'maintenance' ? 'maintenance' : 'silences'}/${encodeURIComponent(id)}`); toast({ message: '已结束' }); load(); }
      catch (e) { if (!e.auth) toast({ message: `操作失败：${e.message}`, tone: 'bad' }); }
    }

    const offs = [watchStuck(el)];
    offs.push(onSegment(el, 'alert-state', (v) => app.setParams({ state: v === 'open' ? null : v, open: null })));
    offs.push(onSegment(el, 'alert-group', (v) => app.setParams({ group: v || null })));
    offs.push(on(el, 'click', '[data-act]', (e, b) => {
      const a = b.dataset.act;
      e.stopPropagation();
      if (a === 'ack') ack(b.dataset.id, b);
      else if (a === 'node') app.openNode(b.dataset.id);
      else if (a === 'new-window') newWindow(b.dataset.kind);
      else if (a === 'del-window') delWindow(b.dataset.id, b.dataset.kind);
      else if (a === 'goto') app.navigate(b.dataset.href);
      else if (a === 'refresh') { b.classList.add('is-busy'); load().finally(() => b.classList.remove('is-busy')); }
    }));
    offs.push(on(el, 'click', '[data-slot="list"] tr[data-row]', (e, tr) => {
      if (e.target.closest('button, a')) return;
      app.setParams({ open: P.open === tr.dataset.row ? null : tr.dataset.row });
    }));
    offs.push(onFleet(({ resource }) => { if (resource === 'alerts' || resource === 'silences') load(); if (resource === 'fleet-down' && el.isConnected) { slot('list').innerHTML = fleetAbsent('事项与告警'); } }));
    // 持续时间每 30 秒走一次（不重取）
    const tick = setInterval(() => { if (data && el.isConnected) render(); }, 30000);
    load();
    return {
      update(p) { const prev = P; P = readP(p); if (prev.state !== P.state) load(); else render(); },
      unmount() { clearInterval(tick); offs.forEach((f) => f()); },
    };
  },
};
