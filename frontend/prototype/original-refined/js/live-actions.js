// live-actions.js — 已接上旧后台的写操作（类 a：只动 team-ops 数据库），表单、确认、成功与失败文案照旧运营页。
// 只有 live-mode.js isWired(key) 为真（实时数据 + 构建开关 TEAMOPS_WRITES）时由页面调用；原型模式、只读构建都走不到这里。
// 字段与接口见 deploy/v2/LIVE-MODE.md §13：
//   编辑资产 / 登记续费  POST /assets/edit   {uuid, expected_revision, name, price, due_date, note, renewal, renewal_note}
//   监控与维护          POST /node/profile  {uuid, expected_revision, profile: {thresholds, maintenance_until}}
//   故障已知悉          POST /incidents/ack {id, note: ''}
//   行为事件已查看      POST /behavior/ack  {id}
import { esc } from './ui/dom.js';
import { button } from './ui/controls.js';
import { openDialog, toast, confirmAction } from './ui/layers.js';
import { runWrite } from './live-writes.js';
import { entryOp, entryShare } from './live-ops.js';
import { note } from './ui/layout.js';

const T = () => globalThis.TEAMOPS;
const field = (label, input, hint = '', wide = false) => `<label class="field${wide ? ' is-wide' : ''}"><span class="field-label">${esc(label)}</span>${input}${hint ? `<span class="field-hint">${esc(hint)}</span>` : ''}</label>`;
const input = (name, value, attrs = '') => `<input class="input" name="${name}" value="${esc(value ?? '')}" ${attrs}>`;
const errBox = '<p class="field-error" data-err hidden></p>';

/** 下个月同一天（没有这一天取月末）：旧运营页“登记续费”的默认新到期日。 */
export function nextMonth(ymd) {
  const m = /^(\d{4})-(\d{2})-(\d{2})$/.exec(ymd ?? '');
  if (!m) return '';
  const y = Number(m[1]), mo = Number(m[2]), d = Number(m[3]);
  const ny = mo === 12 ? y + 1 : y, nm = mo === 12 ? 1 : mo + 1;
  const last = new Date(Date.UTC(ny, nm, 0)).getUTCDate();
  return `${ny}-${String(nm).padStart(2, '0')}-${String(Math.min(d, last)).padStart(2, '0')}`;
}
/** 本地时区的 datetime-local 值 ↔ Unix 秒（旧运营页同一写法：留空 = 结束维护）。 */
export function localInput(ts) {
  if (!ts) return '';
  const d = new Date(ts * 1000);
  const p = (x) => String(x).padStart(2, '0');
  return `${d.getFullYear()}-${p(d.getMonth() + 1)}-${p(d.getDate())}T${p(d.getHours())}:${p(d.getMinutes())}`;
}
const fromLocal = (v) => (v ? Math.floor(new Date(v).getTime() / 1000) : 0);

/** 表单对话框：提交时 collect() → body（抛错 = 表单校验失败，写在对话框里），成功才关闭。 */
function formDialog(app, { title, sub, size = 'md', body, submit, path, collect, ok }) {
  openDialog({
    title, sub, size, guard: true,
    body: `<form data-live-form novalidate>${body}${errBox}</form>`,
    foot: button({ label: '取消', attrs: { 'data-dlg': 'cancel' } }) + button({ label: submit, variant: 'primary', attrs: { 'data-live-submit': '' } }),
    init(el, api) {
      const form = el.querySelector('[data-live-form]');
      const err = el.querySelector('[data-err]');
      const btn = el.querySelector('[data-live-submit]');
      const go = async () => {
        err.hidden = true;
        let payload;
        try { payload = collect(form); } catch (e) { err.hidden = false; err.textContent = e.message; return; }
        const data = await runWrite(app, { path, body: payload, ok, busy: btn });
        if (data) { api.markDirty(false); api.close(); }
      };
      btn.addEventListener('click', go);
      form.addEventListener('submit', (e) => { e.preventDefault(); go(); });
    },
  });
}

/** 节点 → 它的入口（线路机名 + 入站端口）：线路机自身是主节点入口。没绑定入口返回 null。 */
export function entryFor(store, n) {
  if (n.isRelay) {
    const r = store.relay(n.id);
    const port = r?.entries_summary?.master_port ?? n.relay_port;
    return r && port ? { relay: r.name, port, master: true } : null;
  }
  return n.relay && n.relay_port ? { relay: n.relay, port: n.relay_port } : null;
}
const entryLabel = (e) => `${e.relay}:${e.port}`;

/**
 * 更换连接凭据（轮换 UUID）的确认：要输入“线路机名:端口”（与 fleetd 的 confirm 同一串），说明客户端订阅会变。
 * 生效后读新分享链接（审计 secret.reveal）并只显示这一次。页面（线路页、抽屉）共用。
 */
export async function rotateEntry(app, e, { name, key }) {
  const label = entryLabel(e);
  const ok = await confirmAction({
    action: key, title: `更换连接凭据 ${label}`, sub: `${name} · 客户端 UUID`,
    body: '旧 UUID 立即失效：这个入口的客户端订阅 / 分享链接会变，用户要换成新链接才能连上。',
    impact: '更换后把新链接发给这个节点的用户；新链接只在这里显示一次（之后可以在审计里看到谁取过）。', confirm: '更换', danger: true, typeToConfirm: label,
  });
  if (!ok) return null;
  const op = await entryOp(app, e, 'rotate', { label: `更换连接凭据 ${label}` });
  if (op?.status !== 'applied') return op;
  let link = null;
  try { link = await entryShare(e); } catch (x) { toast({ message: `读取新链接失败：${x.message}；可以稍后在线路页再取`, tone: 'bad' }); return op; }
  if (!link) return op;
  openDialog({
    title: '新连接链接', sub: `${name} · ${label}`, size: 'md',
    body: `${note({ tone: 'warn', body: esc('新链接只显示这一次；旧链接已经失效。') })}<textarea class="textarea mono" rows="4" readonly data-share>${esc(link)}</textarea>`,
    foot: button({ label: '复制链接', icon: 'copy', attrs: { 'data-copy-share': '' } }) + button({ label: '完成', variant: 'primary', attrs: { 'data-dlg': 'cancel' } }),
    init(el) {
      el.querySelector('[data-copy-share]').addEventListener('click', () => navigator.clipboard?.writeText(link).then(() => toast({ message: '已复制新链接' }), () => { el.querySelector('[data-share]').select(); toast({ message: '复制失败，已选中，请手动复制', tone: 'bad' }); }));
    },
  });
  return op;
}

/** 类 b（入口）在抽屉里：停用 / 启用 / 重置 / 改备注 / 更换凭据 → 新后台 op（live-ops.js）。 */
async function drawerEntryAction(app, n, k) {
  const e = entryFor(app.store, n);
  if (!e) { toast({ message: `${n.name} 没有绑定入口`, tone: 'info' }); return true; }
  const label = entryLabel(e);
  const key = `drawer.${k}`;
  if (k === 'disable') {
    const ok = await confirmAction({ action: key, title: `停用入口 ${label}`, sub: n.name, body: '该节点的用户会立即断开，出口 IP 不变。可以随时重新启用。', impact: `影响 1 个入口；当前 ${n.rate?.tcp_connections ?? 0} 个连接会断开。`, confirm: '停用', danger: true });
    if (ok) await entryOp(app, e, 'disable', { label: `停用 ${label}` });
  } else if (k === 'enable') {
    await entryOp(app, e, 'enable', { label: `启用 ${label}` });
  } else if (k === 'reset') {
    const ok = await confirmAction({ action: key, title: `重置累计用量 ${label}`, sub: n.name, body: '入口累计上下行会清零，无法恢复。本月业务流量统计不受影响。', confirm: '重置', danger: true, typeToConfirm: n.name });
    if (ok) await entryOp(app, e, 'reset', { label: `重置 ${label} 的累计用量` });
  } else if (k === 'rotate') {
    await rotateEntry(app, e, { name: n.name, key });
  } else if (k === 'remark') {
    const cur = app.store.entriesFor?.(app.store.relays().find((r) => r.name === e.relay)?.id)?.find((x) => x.port === e.port)?.remark ?? '';
    openDialog({
      title: '编辑备注', sub: label, size: 'sm', guard: true,
      body: `<label class="field"><span class="field-label">备注</span><input class="input" data-remark-in value="${esc(cur)}" maxlength="64" autocomplete="off"><span class="field-hint">入口备注只给运维看，不影响连接；最多 64 个字符</span></label>`,
      foot: button({ label: '取消', attrs: { 'data-dlg': 'cancel' } }) + button({ label: '保存', variant: 'primary', attrs: { 'data-remark-save': '' } }),
      init(el, api) {
        const go = () => { const v = el.querySelector('[data-remark-in]').value.trim(); api.markDirty(false); api.close(); entryOp(app, e, 'rename', { remark: v, label: `更新备注 ${label}` }); };
        el.querySelector('[data-remark-save]').addEventListener('click', go);
        el.querySelector('[data-remark-in]').addEventListener('keydown', (ev) => { if (ev.key === 'Enter' && !ev.isComposing) go(); });
      },
    });
  } else return false;
  return true;
}

/** 节点抽屉里接上的操作：edit | renew | monitor | ack | ack-all（类 a）；disable | enable | reset | remark | rotate（类 b）。返回 true 表示已处理。 */
export async function drawerAction(app, n, k, b) {
  if (['disable', 'enable', 'reset', 'remark', 'rotate'].includes(k)) return drawerEntryAction(app, n, k);
  const t = T();
  const a = t?.asset?.(n.id);
  if (!a) { toast({ message: '没有找到这个节点的资产记录，请刷新页面后重试', tone: 'bad' }); return true; }
  if (k === 'edit' || k === 'renew') {
    const renew = k === 'renew';
    formDialog(app, {
      title: renew ? '登记续费' : '编辑资产信息', sub: a.name, submit: renew ? '确认登记续费' : '保存并同步', path: '/assets/edit',
      body: `<div class="fields">
        ${renew ? '' : field('名称', input('name', a.name, 'maxlength="100" autocomplete="off"'), n.isRelay ? '线路机名称是管理通道标识，不能修改' : '', true)}
        ${field('月付金额（USD）', input('price', a.price ?? '', 'inputmode="decimal" autocomplete="off"'), '最多两位小数')}
        ${field(renew ? '新到期日' : '到期日', input('due_date', renew ? nextMonth(a.due_date) : a.due_date, 'type="date"'), renew ? `原到期 ${a.due_date ?? '—'}，新到期日必须更晚` : `按 ${a.billing_timezone ?? 'Asia/Shanghai'} 日历日计算`)}
        ${renew ? field('续费备注（选填）', '<textarea class="textarea" name="renewal_note" maxlength="500"></textarea>', '例如账单号', true)
        : field('备注（选填）', `<textarea class="textarea" name="note" maxlength="500">${esc(a.note ?? '')}</textarea>`, '', true)}
      </div>`,
      collect(f) {
        const v = (name) => f.elements[name]?.value?.trim() ?? '';
        const price = v('price');
        if (!/^\d+(\.\d{1,2})?$/.test(price)) throw new Error('请输入有效的 USD 月付金额，最多两位小数');
        const due = v('due_date');
        if (!/^\d{4}-\d{2}-\d{2}$/.test(due)) throw new Error('请选择到期日');
        if (renew && a.due_date && due <= a.due_date) throw new Error('登记续费时，新到期日必须晚于原到期日');
        const name = renew ? a.name : v('name');
        if (!name) throw new Error('名称不能为空');
        return { uuid: a.uuid, expected_revision: a.revision ?? 1, name, price, due_date: due, note: renew ? (a.note ?? '') : v('note'), renewal: renew, renewal_note: renew ? v('renewal_note') : '' };
      },
      ok: renew ? `已登记续费 · ${n.name}` : `已保存 · ${n.name}`,
    });
    return true;
  }
  if (k === 'monitor') {
    let rec = null;
    try { rec = (await t.nodeRecord(n.id))?.asset ?? null; } catch { rec = null; }
    const cur = rec ?? a;
    const p = cur.profile ?? {};
    const L = { cpu_percent: 90, memory_percent: 90, disk_percent: 85, latency_ms: 3000, ...(n.limits ?? {}), ...(p.thresholds ?? {}) };
    const until = p.maintenance_until > Date.now() / 1000 ? p.maintenance_until : 0;
    const TH = [['cpu_percent', 'CPU 提醒 %', 1, 100], ['memory_percent', '内存提醒 %', 1, 100], ['disk_percent', '磁盘提醒 %', 1, 100], ['latency_ms', '出口请求耗时提醒 ms', 100, 60000]];
    formDialog(app, {
      title: '节点监控与维护', sub: cur.name, submit: '保存监控设置', path: '/node/profile',
      body: `<div class="fields">${TH.map(([key, label, min, max]) => field(label, input(key, L[key], `type="number" min="${min}" max="${max}" step="1" inputmode="numeric"`))).join('')}
        ${field('维护截止时间（本地时区）', input('maintenance_until', localInput(until), 'type="datetime-local"'), n.status?.maintenance && !until ? '当前处于维护中；留空保存会结束维护' : '填写未来 7 天内的时间；留空表示不在维护', true)}</div>`,
      collect(f) {
        const thresholds = {};
        for (const [key, label, min, max] of TH) {
          const x = Number(f.elements[key].value);
          if (!Number.isFinite(x) || x < min || x > max) throw new Error(`${label} 须在 ${min}–${max} 之间`);
          thresholds[key] = x;
        }
        const mu = fromLocal(f.elements.maintenance_until.value);
        if (mu && !(mu > Date.now() / 1000 && mu <= Date.now() / 1000 + 7 * 86400)) throw new Error('维护窗口须为未来 7 天内；留空表示结束维护');
        return { uuid: cur.uuid, expected_revision: cur.revision ?? 1, profile: { thresholds, maintenance_until: mu } };
      },
      ok: '监控设置已保存',
    });
    return true;
  }
  if (k === 'ack' || k === 'ack-all') {
    const ids = k === 'ack' ? [Number(b.dataset.inc)] : app.store.incidents({ node: n.id, open: true }).filter((i) => !i.ack_at && !i.acked).map((i) => Number(i.id));
    const valid = ids.filter((x) => Number.isFinite(x));
    if (!valid.length) { toast({ message: '没有需要确认的故障', tone: 'info' }); return true; }
    let done = 0;
    for (const id of valid) if (await runWrite(app, { path: '/incidents/ack', body: { id, note: '' }, busy: b })) done++;
    if (done) {
      toast({ message: '已标记知悉；恢复仍由真实测量确认' });
      if (b.isConnected) b.outerHTML = '<span class="muted">已知悉</span>';
    }
    return true;
  }
  return false;
}

/** 行为页“标记已查看”：POST /behavior/ack {id}（旧运营页不弹 toast，只把这一行置为已查看）。 */
export async function behaviorAck(app, id, b) {
  const data = await runWrite(app, { path: '/behavior/ack', body: { id: Number(id) }, busy: b, refresh: ['minute'] });
  return !!data;
}

