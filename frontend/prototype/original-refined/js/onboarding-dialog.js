// onboarding-dialog.js — “新建批次”向导（SPEC §6.6、§3.17）：lg 对话框，步骤条 `1 录入 ─ 2 预览 ─ 3 选项 ─ 4 开始`。
//   1 录入：类型（单选卡片）+ 粘贴开通邮件或 CSV（逐字识别，实时计数）；
//   2 预览：可编辑表格（IP、服务号、套餐规范名下拉、线路机、到期），出错格 1 px --bad 边 + 下方说明；密码不进表格，只显示 `已提供 / 缺少`；
//   3 选项：金丝雀、线路机分配、重装并发、合批窗口、只演练；显示分配结果与 Xray 重载影响；
//   4 开始：摘要 + 主按钮 `开始上架 N 台` → 新批次出现在批次列表并开始运行（模型里模拟）。
// 底栏固定（取消 · 上一步 · 下一步），体内滚动。密码只在本次识别里判断“有没有”，不保存值、不渲染。

import { fmt } from '../../shared/index.js';
import {
  icon, esc, on, openDialog, button, iconButton, segmented, onSegment, choiceCards, wizardSteps, note, kv, mark, tonePill,
  affix, secretInput, openPopover, closePopover, hydrate, toast, num,
} from './components.js';
import { allPlans, planById, resolvePlan } from './plans.js';
import { createBatch, nextPorts, PROXY_SKIP } from './onboarding-model.js';
import { guard } from './live-mode.js';

// 与接入向导同一种写法：第一步都叫“填写”，带“开始”按钮的那一步都叫“确认”（接入向导 填写 / 确认 / 执行，consistency #5）
const STEPS = ['填写', '预览', '选项', '确认'];
const TYPES = [
  { value: 'residential', label: '家宽节点', icon: 'server', desc: '有主机：预检 → 重装 → SS → 登记 → 线路机 → 验收，16 步' },
  { value: 'proxy', label: '代理产品', icon: 'network', desc: '无主机：只做预检、登记、线路机、探针与验收，7 步' },
  { value: 'relay', label: '线路机', icon: 'server-cog', desc: '新线路机在“线路”页的“新增线路机”里上架', disabled: true },
];
const TZ = { ATTVPS: 'Asia/Shanghai', NodeMach: 'America/Los_Angeles', OhnoICU: 'America/Los_Angeles', GoMami: 'America/Los_Angeles' };

// 演示用的开通邮件（虚构：IP 在 198.51.100.200–254 演示段、服务号接着演示批次往后编、密码是占位串）
const SAMPLE_MAIL = [
  ['ATTVPS', '60398', '198.51.100.209', true],
  ['ATTVPS', '60399', '198.51.100.210', true],
  ['ATTVPS', '60400', '198.51.100.211', false],
  ['ATTVPS', '60401', '198.51.100.212', true],
  ['ATTVPS', '60402', '198.51.100.31', true],
  ['ATTVPS', '60403', '198.51.100.213', true],
  ['NodeMach', '34408', '198.51.100.214', true],
].map(([p, id, ip, pw], i) => p === 'NodeMach'
  ? `From: NodeMach <noreply@nodemach.example>\nSubject: Your proxy is ready #${id}\nService ID: ${id}\nSOCKS5 Host: ${ip}\nPort: 1080\nUsername: nm${id}\nPassword: demo-socks-${i}\nNext Due Date: 2026-10-26`
  : `From: ATTVPS <support@attvps.example>\nSubject: 新服务已开通 #${id}\nService ID: ${id}\nMain IP: ${ip}\nUsername: root${pw ? `\nPassword: demo-root-${i}` : ''}\nDue Date: 2026-10-23`).join('\n\n');
const SAMPLE_CSV = `ip,provider,service_id,plan,relay,team,password
198.51.100.217,ATTVPS,60405,ATTVPS 基础版,线路机2,团队1,demo-root-a
198.51.100.218,ATTVPS,60406,ATTVPS 基础版,线路机3,团队1,demo-root-b
198.51.100.219,ATTVPS,60407,ATTVPS 标准版,线路机1,团队1,
198.51.100.220,NodeMach,34409,NodeMach 高速代理,线路机1,团队1,demo-socks-c`;

// ─── 识别 ───────────────────────────────────────────────────────────────────

function providerOf(store, s) {
  const low = String(s ?? '').toLowerCase();
  return store.providers().find((p) => low.includes(p.name.toLowerCase()) || low === String(p.code).toLowerCase()) ?? null;
}
const IPV4 = /\b((?:25[0-5]|2[0-4]\d|1?\d?\d)(?:\.(?:25[0-5]|2[0-4]\d|1?\d?\d)){3})\b/;

/** 粘贴进来的密码立即遮住（识别只需要知道“有没有”）：邮件里的 Password: 行、CSV 的 password 列。 */
export function maskSecrets(text, source) {
  const DOTS = '••••••••';
  if (source === 'csv') {
    const lines = text.split(/\r?\n/);
    const head = (lines[0] ?? '').toLowerCase().split(',').map((h) => h.trim());
    const k = head.indexOf('password');
    if (k < 0) return text;
    return lines.map((l, i) => { if (!i) return l; const v = l.split(','); if (v[k] && v[k].trim() && v[k].trim() !== DOTS) v[k] = DOTS; return v.join(','); }).join('\n');
  }
  return text.replace(/((?:password|密码)\s*[:：]\s*)(\S+)/gi, (m, a, b) => (b === DOTS ? m : a + DOTS));
}

/** parseInput(store, text, source) → [{ ip, provider, service_id, planText, relay, team, due, hasSecret }] */
export function parseInput(store, text, source) {
  const out = [];
  if (source === 'csv') {
    const lines = text.split(/\r?\n/).map((l) => l.trim()).filter(Boolean);
    if (!lines.length) return out;
    const head = lines[0].toLowerCase().split(',').map((h) => h.trim());
    const hasHead = head.includes('ip');
    const cols = hasHead ? head : ['ip', 'provider', 'service_id', 'plan', 'relay', 'team', 'password'];
    for (const line of lines.slice(hasHead ? 1 : 0)) {
      const v = line.split(',').map((x) => x.trim());
      const g = (k) => v[cols.indexOf(k)] ?? '';
      out.push({ ip: g('ip'), provider: providerOf(store, g('provider'))?.name ?? g('provider'), service_id: g('service_id'), planText: g('plan'), relay: g('relay'), team: g('team'), due: g('due_date'), hasSecret: !!g('password') });
    }
    return out;
  }
  let cur = null;
  const push = () => { if (cur && (cur.ip || cur.service_id)) out.push(cur); cur = null; };
  for (const raw of text.split(/\r?\n/)) {
    const line = raw.trim();
    if (!line) continue;
    if (/^from\s*[:：]/i.test(line)) { push(); cur = { provider: providerOf(store, line)?.name ?? '', ip: '', service_id: '', planText: '', due: '', hasSecret: false }; continue; }
    cur ??= { provider: '', ip: '', service_id: '', planText: '', due: '', hasSecret: false };
    let m;
    if (!cur.provider) { const p = providerOf(store, line); if (p) cur.provider = p.name; }
    if ((m = line.match(/(?:service\s*id|服务号|订单号?)\s*[:：#]?\s*([\w-]+)/i))) cur.service_id ||= m[1];
    else if ((m = line.match(/(?:main ip|ip address|ip|host|地址)[^:：]*[:：]\s*(.+)/i)) && IPV4.test(m[1])) { if (cur.ip) { const p = cur.provider; push(); cur = { provider: p, ip: '', service_id: '', planText: '', due: '', hasSecret: false }; } cur.ip = m[1].match(IPV4)[1]; }
    else if ((m = line.match(/(?:password|密码)\s*[:：]\s*(\S+)/i))) cur.hasSecret = true;
    else if ((m = line.match(/(?:due date|到期)[^:：]*[:：]\s*(\d{4}-\d{2}-\d{2})/i))) cur.due = m[1];
    else if ((m = line.match(/(?:product|产品|plan|套餐)\s*[:：]\s*(.+)/i))) cur.planText = m[1];
  }
  push();
  return out;
}

// ─── 预览行：补全、分配、校验 ───────────────────────────────────────────────

function buildRows(ctx, parsed) {
  const { store, st } = ctx;
  const rows = parsed.map((p, i) => {
    const prov = store.providers().find((x) => x.name === p.provider) ?? null;
    const plan = prov ? resolvePlan(prov.name, p.planText) : null;
    return {
      key: `r${i}`, ip: p.ip, provider: prov?.name ?? p.provider ?? '', provider_code: prov?.code ?? '', service_id: p.service_id,
      plan_id: plan?.id ?? '', planInferred: !!plan?.inferred || !p.planText, relayWanted: p.relay || '', relay: '', relay_port: null,
      team: p.team && store.teams().some((t) => t.name === p.team) ? p.team : '团队1',
      due_date: p.due || '', billing_timezone: TZ[prov?.name] ?? 'Asia/Shanghai',
      mode: prov && prov.host_access === false ? 'proxy' : 'new', hasSecret: p.hasSecret,
    };
  });
  return rows;
}

/** 分配线路机与端口、编号（每次行变化后重算，保持用户手动选过的线路机）。 */
function allocate(ctx, rows, strategy) {
  const { store, st } = ctx;
  const load = {};
  for (const r of store.relays()) load[r.name] = 0;
  for (const n of store.nodes()) if (n.relay && load[n.relay] != null) load[n.relay]++;
  for (const b of st.batches) for (const r of b.rows) if (!r.cancelled && load[r.relay] != null) load[r.relay]++;
  const ports = nextPorts(store, st);
  const nextNo = {};
  for (const n of store.nodes()) nextNo[n.team] = Math.max(nextNo[n.team] ?? 0, n.number ?? 0);
  for (const b of st.batches) for (const r of b.rows) { const m = r.name.match(/^(.+)-(\d+)$/); if (m) nextNo[m[1]] = Math.max(nextNo[m[1]] ?? 0, Number(m[2])); }
  for (const r of rows) {
    const manual = r.relayPinned || (strategy === 'input' && r.relayWanted && load[r.relayWanted] != null);
    if (manual) r.relay = r.relayPinned ? r.relay : r.relayWanted;
    else r.relay = Object.keys(load).sort((a, b) => load[a] - load[b] || a.localeCompare(b, 'zh-CN', { numeric: true }))[0];
    load[r.relay]++;
    ports[r.relay] = (ports[r.relay] ?? 20000) + 1;
    r.relay_port = ports[r.relay];
    nextNo[r.team] = (nextNo[r.team] ?? 0) + 1;
    r.number = nextNo[r.team];
    r.name = `${r.team}-${fmt.pad2(r.number)}`;
    if (!r.due_date) r.due_date = addDays(fmt.date(store.now()).text, 30);
  }
  return rows;
}
function addDays(ymd, n) { const d = new Date(`${ymd}T00:00:00Z`); d.setUTCDate(d.getUTCDate() + n); return d.toISOString().slice(0, 10); }

function validate(ctx, rows) {
  const { store, st } = ctx;
  const fleetIp = new Map(store.nodes().map((n) => [n.ip, n.name]));
  const batchIp = new Map();
  for (const b of st.batches) for (const r of b.rows) if (!r.cancelled) batchIp.set(r.ip, r.name);
  const fleetSvc = new Map(store.nodes().filter((n) => n.billing?.service_id).map((n) => [`${n.provider}|${n.billing.service_id}`, n.name]));
  for (const b of st.batches) for (const r of b.rows) if (!r.cancelled) fleetSvc.set(`${r.provider}|${r.service_id}`, r.name);
  let errors = 0, warns = 0;
  rows.forEach((r, i) => {
    r.err = {}; r.warn = null;
    if (!IPV4.test(r.ip ?? '') || r.ip.match(IPV4)[1] !== r.ip) r.err.ip = '不是有效的 IPv4 地址';
    else if (fleetIp.has(r.ip)) r.err.ip = `与在用节点 ${fleetIp.get(r.ip)} 的 IP 重复`;
    else if (batchIp.has(r.ip)) r.err.ip = `已在上架中的 ${batchIp.get(r.ip)} 里`;
    else { const j = rows.findIndex((x) => x.ip === r.ip); if (j !== i) r.err.ip = `与第 ${j + 1} 行重复`; }
    if (!r.provider || !store.providers().some((p) => p.name === r.provider)) r.err.provider = '供应商不在目录里';
    if (!r.service_id) r.err.service_id = '缺少服务号';
    else if (fleetSvc.has(`${r.provider}|${r.service_id}`)) r.err.service_id = `服务号已登记在 ${fleetSvc.get(`${r.provider}|${r.service_id}`)}`;
    if (!r.plan_id || !planById(r.plan_id)) r.err.plan = '套餐目录里没有这个套餐';
    if (!/^\d{4}-\d{2}-\d{2}$/.test(r.due_date ?? '')) r.err.due = '日期写成 2026-10-23';
    if (!r.hasSecret && r.mode !== 'proxy') r.warn = '缺少 root 密码：开始后会停在“推密钥”等待输入';
    else if (!r.hasSecret && r.mode === 'proxy') r.warn = '缺少代理密码：开始后会停在“预检”等待输入';
    errors += Object.keys(r.err).length;
    if (r.warn) warns++;
  });
  return { errors, warns };
}

// ─── 对话框 ─────────────────────────────────────────────────────────────────

/** openNewBatch({ app, st, onCreated(batch) }) */
export function openNewBatch({ app, st, onCreated }) {
  if (guard('onboarding.batch.new')) return null; // 只读预览：批量上架随新后台上线
  const store = app.store;
  const ctx = { store, st };
  const S = { step: 0, type: 'residential', source: 'mail', text: '', rows: [], strategy: 'balance', canary: true, canaryKey: null, dd: 5, window: 90, dry: false };
  let dlg = null;

  const dlgApi = openDialog({
    title: '新建批次', sub: '粘贴开通邮件或 CSV，预览确认后开始上架', size: 'lg', label: '新建上架批次', guard: true,
    body: '', foot: '<span></span>',
    init(el, api) { dlg = api; wire(el); render(); },
  });

  function parsedRows() { return allocate(ctx, S.rows, S.strategy); }
  function reparse() {
    const parsed = parseInput(store, S.text, S.source);
    const keep = new Map(S.rows.map((r) => [r.ip, r]));
    S.rows = buildRows(ctx, parsed).map((r) => {
      const old = keep.get(r.ip);
      return old ? { ...r, plan_id: old.plan_id, planInferred: old.planInferred, relay: old.relay, relayPinned: old.relayPinned, hasSecret: old.hasSecret || r.hasSecret } : r;
    });
    // CSV 里写了线路机：默认“按录入”（用户动过分配方式就不再改）
    if (!S.strategyTouched) S.strategy = S.source === 'csv' && S.rows.some((r) => r.relayWanted) ? 'input' : 'balance';
    allocate(ctx, S.rows, S.strategy);
  }

  function render() {
    const body = dlg.body;
    const foot = dlg.el.querySelector('.dialog-foot');
    const res = validate(ctx, S.rows);
    const steps = wizardSteps(STEPS, S.step, { label: '新建批次步骤' });
    let html = '', next = '';
    if (S.step === 0) {
      const n = S.rows.length;
      html = `${steps}
        <div class="field"><span class="field-label">类型</span>${choiceCards({ name: 'ob-type', items: TYPES, value: S.type, label: '批次类型' })}</div>
        <div class="field">
          <div class="row-between"><span class="field-label">来源</span><span class="row">${segmented({ name: 'ob-source', size: 'sm', items: [{ value: 'mail', label: '开通邮件' }, { value: 'csv', label: 'CSV' }], value: S.source, label: '来源格式' })}${button({ label: '填入示例', icon: 'clipboard-paste', variant: 'quiet', size: 'sm', attrs: { 'data-nb': 'sample' } })}</span></div>
          <textarea class="textarea ob-paste" data-nb-text spellcheck="false" aria-label="粘贴内容" placeholder="${esc(S.source === 'mail' ? '把供应商的开通邮件整封粘进来，可以一次粘多封。会识别 Service ID、IP、到期日，以及有没有 root 密码（密码粘进来就会遮住）' : '第一行是表头：ip,provider,service_id,plan,relay,team,password（plan 写套餐规范名；relay、team、password 可空）')}">${esc(S.text)}</textarea>
          <span class="field-hint" data-nb-count>${n ? `识别到 ${n} 台${res.errors ? ` · <span class="t-bad">${res.errors} 处需要修正</span>` : ''}${res.warns ? ` · <span class="t-warn">${res.warns} 台缺少密码</span>` : ''}` : '还没有识别到机器'}</span>
        </div>
        ${note({ tone: 'info', body: '密码粘进来就遮住，只用来判断“有没有”：不进预览表格、不写日志、不回显。开始后在后台用一次就丢弃。' })}`;
      next = button({ label: '下一步：预览', iconEnd: 'chevron-right', variant: 'primary', attrs: { 'data-nb': 'next', disabled: !n } });
    } else if (S.step === 1) {
      html = `${steps}${previewSummary(res)}${previewTable()}`;
      next = button({ label: '下一步：选项', iconEnd: 'chevron-right', variant: 'primary', attrs: { 'data-nb': 'next', disabled: !!res.errors || !S.rows.length } });
    } else if (S.step === 2) {
      html = `${steps}${optionsHtml()}`;
      next = button({ label: '下一步：确认', iconEnd: 'chevron-right', variant: 'primary', attrs: { 'data-nb': 'next' } });
    } else {
      html = `${steps}${confirmHtml()}`;
      next = button({ label: `开始上架 ${S.rows.length} 台`, icon: 'play', variant: 'primary', attrs: { 'data-nb': 'run' } });
    }
    const prev = S.step ? button({ label: '上一步', icon: 'chevron-left', attrs: { 'data-nb': 'prev' } }) : '';
    const hint = S.step === 1 && res.errors ? `<span class="field-error ob-foot-hint">先修正 ${res.errors} 处错误</span>` : '';
    body.innerHTML = html;
    foot.innerHTML = `${hint}${button({ label: '取消', attrs: { 'data-dlg': 'cancel' } })}${prev}${next}`;
    hydrate(dlg.el);
  }

  function previewSummary(res) {
    const byPlan = new Map();
    let cents = 0;
    for (const r of S.rows) { const p = planById(r.plan_id); byPlan.set(p?.name ?? '未归类套餐', (byPlan.get(p?.name ?? '未归类套餐') ?? 0) + 1); cents += p?.price_cents ?? 0; }
    const status = res.errors ? tonePill(`${res.errors} 处需要修正`, 'bad') : res.warns ? tonePill(`${res.warns} 台缺少密码`, 'warn') : tonePill('可以继续', 'ok');
    return `<div class="row-between ob-sum"><span class="muted"><span class="num">${S.rows.length}</span> 台 · ${[...byPlan].map(([k, v]) => `${esc(k)} ×${v}`).join(' · ')} · 月费 ${num(fmt.money(cents, { perMonth: true }))}</span>${status}</div>`;
  }

  function previewTable() {
    const plans = allPlans(store).filter((p) => p.category !== 'relay' && !/^gomami/.test(p.id));
    const relays = store.relays().map((r) => r.name);
    const errLine = (msg) => (msg ? `<span class="field-error">${esc(msg)}</span>` : '');
    const rows = S.rows.map((r, i) => {
      const p = planById(r.plan_id);
      const opts = [!p ? '<option value="" selected>选择套餐</option>' : '', ...plans.map((x) => `<option value="${esc(x.id)}"${x.id === r.plan_id ? ' selected' : ''}>${esc(x.name)}</option>`)].join('');
      const relayOpts = relays.map((x) => `<option${x === r.relay ? ' selected' : ''}>${esc(x)}</option>`).join('');
      const cred = r.hasSecret
        ? `<button type="button" class="pill" data-tone="ok" data-nb-cred="${esc(r.key)}" data-tip="已从${S.source === 'mail' ? '邮件' : ' CSV '}读到密码（不显示）；点击替换">已提供</button>`
        : `<button type="button" class="pill" data-tone="warn" data-dot="ring" data-nb-cred="${esc(r.key)}" data-tip="${esc(r.warn ?? '')}">缺少</button>`;
      return `<tr data-nb-row="${esc(r.key)}"${Object.keys(r.err).length ? ' data-attn="bad"' : r.warn ? ' data-attn="warn"' : ''}>
        <td><span class="strong nowrap">${esc(r.name)}</span><span class="sub">${esc(r.team)}${r.mode === 'proxy' ? ' · 代理' : ''}</span></td>
        <td><input class="input input-sm ob-in-ip" data-nb-f="ip" value="${esc(r.ip)}" aria-label="第 ${i + 1} 行 IP"${r.err.ip ? ' aria-invalid="true"' : ''}>${errLine(r.err.ip)}
          <span class="row ob-svc"><span class="muted nowrap">${esc(r.provider || '—')}</span><input class="input input-sm ob-in-svc" data-nb-f="service_id" value="${esc(r.service_id)}" aria-label="第 ${i + 1} 行服务号"${r.err.service_id ? ' aria-invalid="true"' : ''}></span>${errLine(r.err.provider)}${errLine(r.err.service_id)}</td>
        <td><span class="select-wrap"><select class="select select-sm ob-in-plan" data-nb-f="plan_id" aria-label="第 ${i + 1} 行套餐"${r.err.plan ? ' aria-invalid="true"' : ''}>${opts}</select>${iconSvg()}</span>${p ? `<span class="sub"${r.planInferred ? ' data-tip="邮件里没有写套餐，按该供应商最常见的套餐推断；可以改"' : ''}>${r.planInferred ? '推断 · ' : ''}${esc(p.specs?.bandwidth_text ?? '')} · ${esc(fmt.money(p.price_cents, { perMonth: true }).text)}</span>` : ''}${errLine(r.err.plan)}</td>
        <td><span class="row ob-relay"><span class="select-wrap"><select class="select select-sm ob-in-relay" data-nb-f="relay" aria-label="第 ${i + 1} 行线路机">${relayOpts}</select>${iconSvg()}</span><span class="num muted">:${esc(r.relay_port)}</span></span>
          <span class="row ob-due"><input class="input input-sm ob-in-due" data-nb-f="due_date" value="${esc(r.due_date)}" aria-label="第 ${i + 1} 行到期"${r.err.due ? ' aria-invalid="true"' : ''}>${mark(r.billing_timezone === 'Asia/Shanghai' ? '北京' : 'LA', { title: `账期时区 ${r.billing_timezone}` })}</span>${errLine(r.err.due)}</td>
        <td>${cred}</td>
        <td class="r">${iconButton({ icon: 'x', label: `移除第 ${i + 1} 行`, size: 'sm', attrs: { 'data-nb-del': r.key } })}</td>
      </tr>`;
    }).join('');
    return `<div class="table-wrap ob-preview"><table class="dt ob-edit" aria-label="预览"><thead><tr><th>节点</th><th>IP · 服务号</th><th>套餐</th><th>线路机 · 到期</th><th>凭据</th><th class="r"><span class="sr-only">移除</span></th></tr></thead><tbody>${rows}</tbody></table></div>`;
  }
  const iconSvg = () => icon('chevron-down', 14);

  function allocText() {
    const by = new Map();
    for (const r of S.rows) { if (!by.has(r.relay)) by.set(r.relay, []); by.get(r.relay).push(r.relay_port); }
    return [...by].sort((a, b) => a[0].localeCompare(b[0], 'zh-CN', { numeric: true })).map(([k, ps]) => `${esc(k)} ← <span class="num">${ps.length}</span> 台（:${ps[0]}${ps.length > 1 ? `–${ps[ps.length - 1]}` : ''}）`).join(' · ');
  }
  function optionsHtml() {
    if (!S.canaryKey || !S.rows.some((r) => r.key === S.canaryKey)) S.canaryKey = (S.rows.find((r) => r.mode !== 'proxy') ?? S.rows[0])?.key;
    const relaysHit = [...new Set(S.rows.map((r) => r.relay))];
    const canaryOpts = S.rows.map((r) => `<option value="${esc(r.key)}"${r.key === S.canaryKey ? ' selected' : ''}>${esc(r.name)} · ${esc(r.ip)}</option>`).join('');
    return `<div class="fields">
        <div class="field is-wide"><label class="check"><input type="checkbox" data-nb-o="canary"${S.canary ? ' checked' : ''}><span>先跑 1 台金丝雀，通过后再放开其余</span></label>
          <span class="select-wrap ob-canary"><select class="select" data-nb-o="canaryKey" aria-label="金丝雀"${S.canary ? '' : ' disabled'}>${canaryOpts}</select>${iconSvg()}</span>
          <span class="field-hint">金丝雀跑完 16 步（约 13 分钟）才放开其余；关掉则全部同时开始</span></div>
        <div class="field is-wide"><span class="field-label">线路机分配</span>
          <div class="row">${segmented({ name: 'ob-strategy', items: [{ value: 'balance', label: '均衡' }, { value: 'input', label: '按录入' }], value: S.strategy, label: '线路机分配' })}</div>
          <span class="field-hint" data-nb-alloc>${allocText()}</span></div>
        <label class="field"><span class="field-label">重装并发</span>${affix(`<input class="input" type="number" min="1" max="10" data-nb-o="dd" value="${S.dd}">`, '台')}<span class="field-hint">同时在“重装”阶段的最多台数</span></label>
        <label class="field"><span class="field-label">合批窗口</span>${affix(`<input class="input" type="number" min="30" max="300" step="10" data-nb-o="window" value="${S.window}">`, '秒')}<span class="field-hint">窗口内到达同一台线路机的入口合并成一次 Xray 更新</span></label>
        <div class="field is-wide"><label class="check"><input type="checkbox" data-nb-o="dry"${S.dry ? ' checked' : ''}><span>只演练：预检之后停止，不重装、不改线路机</span></label></div>
        <div class="field is-wide"><span class="field-label">配方</span><span class="muted">${esc(st.batches.find((b) => b.demo)?.recipe ?? '—')}</span></div>
      </div>
      ${S.dry ? note({ tone: 'info', body: '只演练不会改动任何线路机和主机。' }) : note({ tone: 'warn', body: `${esc(relaysHit.join('、'))} 各会重载 1 次 Xray（窗口内合并，中断约 1–3 秒）；在用节点的连接会断开一下再自动重连。` })}`;
  }
  function confirmHtml() {
    const prov = new Map();
    for (const r of S.rows) prov.set(r.provider, (prov.get(r.provider) ?? 0) + 1);
    const cents = S.rows.reduce((a, r) => a + (planById(r.plan_id)?.price_cents ?? 0), 0);
    const one = 13 * 60;
    const eta = S.dry ? 60 : S.canary ? one * 2 : one;
    const canaryRow = S.rows.find((r) => r.key === S.canaryKey);
    const missing = S.rows.filter((r) => !r.hasSecret).length;
    return `${kv([
      { label: '类型', value: esc(TYPES.find((t) => t.value === S.type)?.label ?? '—') },
      { label: '台数', value: `<span class="num">${S.rows.length}</span> 台（${[...prov].map(([p, n]) => `${esc(p)} ${n}`).join(' · ')}）` },
      { label: '金丝雀', value: S.canary && canaryRow ? esc(`${canaryRow.name} · ${canaryRow.ip}`) : '不用' },
      { label: '线路机', value: allocText() },
      { label: '预计耗时', value: S.dry ? '约 1 分钟（只演练）' : `约 ${Math.round(eta / 60)} 分钟${S.canary ? '（含金丝雀）' : ''}` },
      { label: '新增月费', value: num(fmt.money(cents, { perMonth: true })) },
      { label: '重装并发 · 合批窗口', value: `<span class="num">${S.dd}</span> 台 · <span class="num">${S.window}</span> 秒` },
      { label: '缺少密码', value: missing ? `<span class="t-warn">${missing} 台，会停在“推密钥”等待输入</span>` : '没有' },
    ], { cols: 2 })}
      ${note({ tone: 'info', body: '开始后可以在矩阵里暂停批次、放弃单行；已完成的步骤不会自动回滚。' })}`;
  }

  function wire(el) {
    on(el, 'click', '[data-nb]', (e, b) => {
      const a = b.dataset.nb;
      if (a === 'sample') { S.text = maskSecrets(S.source === 'mail' ? SAMPLE_MAIL : SAMPLE_CSV, S.source); reparse(); render(); el.querySelector('[data-nb-text]')?.focus(); }
      else if (a === 'next') { if (S.step === 0) reparse(); S.step = Math.min(3, S.step + 1); render(); dlg.body.scrollTop = 0; }
      else if (a === 'prev') { S.step = Math.max(0, S.step - 1); render(); }
      else if (a === 'run') run(b);
    });
    on(el, 'input', '[data-nb-text]', (e, t) => {
      const masked = maskSecrets(t.value, S.source);
      if (masked !== t.value) { const end = t.selectionEnd === t.value.length; t.value = masked; if (end) t.selectionStart = t.selectionEnd = masked.length; }
      S.text = masked;
      reparse();
      const res = validate(ctx, S.rows);
      const n = S.rows.length;
      el.querySelector('[data-nb-count]').innerHTML = n ? `识别到 ${n} 台${res.errors ? ` · <span class="t-bad">${res.errors} 处需要修正</span>` : ''}${res.warns ? ` · <span class="t-warn">${res.warns} 台缺少密码</span>` : ''}` : '还没有识别到机器';
      const nb = el.querySelector('[data-nb="next"]');
      if (nb) nb.disabled = !n;
    });
    on(el, 'change', '[name="ob-type"]', (e, r) => { S.type = r.value; });
    on(el, 'seg-change', '[data-seg="ob-source"]', (e) => { S.source = e.detail.value; S.text = ''; S.rows = []; render(); });
    on(el, 'seg-change', '[data-seg="ob-strategy"]', (e) => { S.strategy = e.detail.value; S.strategyTouched = true; for (const r of S.rows) r.relayPinned = false; allocate(ctx, S.rows, S.strategy); el.querySelector('[data-nb-alloc]').innerHTML = allocText(); });
    on(el, 'change', '[data-nb-f]', (e, input) => {
      const r = S.rows.find((x) => x.key === input.closest('[data-nb-row]').dataset.nbRow);
      const f = input.dataset.nbF;
      r[f] = input.value.trim();
      if (f === 'plan_id') r.planInferred = false;
      if (f === 'relay') r.relayPinned = true;
      allocate(ctx, S.rows, S.strategy);
      const focusKey = `${r.key}|${f}`;
      render();
      el.querySelector(`[data-nb-row="${r.key}"] [data-nb-f="${f}"]`)?.focus();
      void focusKey;
    });
    on(el, 'click', '[data-nb-del]', (e, b) => { S.rows = S.rows.filter((x) => x.key !== b.dataset.nbDel); allocate(ctx, S.rows, S.strategy); render(); });
    on(el, 'click', '[data-nb-cred]', (e, b) => {
      const r = S.rows.find((x) => x.key === b.dataset.nbCred);
      const pop = openPopover(b, {
        width: 280, label: `${r.name} 的密码`, focus: true,
        html: `<div class="pop-title">${esc(r.name)} · ${r.mode === 'proxy' ? '代理密码' : 'root 密码'}</div><div class="stack-sm">${secretInput({ name: 'nb-secret', label: '密码', attrs: { 'data-nb-secret': '' } })}<div class="row-end">${button({ label: '保存', variant: 'primary', size: 'sm', attrs: { 'data-nb-save': '' } })}</div></div>`,
      });
      const save = () => { const v = pop.el.querySelector('[data-nb-secret]').value; if (!v) return; r.hasSecret = true; closePopover(); render(); toast({ message: `${r.name} 的密码已替换（不显示）` }); };
      pop.el.addEventListener('click', (ev) => { if (ev.target.closest('[data-nb-save]')) save(); });
      pop.el.addEventListener('keydown', (ev) => { if (ev.key === 'Enter') save(); });
      setTimeout(() => pop.el.querySelector('[data-nb-secret]')?.focus(), 0);
    });
    on(el, 'change', '[data-nb-o]', (e, input) => {
      const k = input.dataset.nbO;
      if (k === 'canary') { S.canary = input.checked; el.querySelector('[data-nb-o="canaryKey"]').disabled = !S.canary; }
      else if (k === 'canaryKey') S.canaryKey = input.value;
      else if (k === 'dd') S.dd = Math.max(1, Math.min(10, Number(input.value) || 5));
      else if (k === 'window') S.window = Math.max(30, Math.min(300, Number(input.value) || 90));
      else if (k === 'dry') { S.dry = input.checked; render(); }
    });
  }

  function run(btn) {
    btn.disabled = true;
    const canaryRow = S.rows.find((r) => r.key === S.canaryKey);
    const rows = S.rows.map((r) => ({ ...r, secret: r.hasSecret ? 'email' : 'missing' }));
    const batch = createBatch(st, {
      type: S.type, typeWord: TYPES.find((t) => t.value === S.type)?.label ?? '家宽节点', rows,
      options: { canary: S.canary && !!canaryRow, canaryNode: canaryRow?.name, strategy: S.strategy === 'balance' ? '均衡' : '按录入', dd: S.dd, window: S.window, dry: S.dry },
    });
    dlgApi.close();
    onCreated?.(batch);
    toast({ message: `已开始 ${batch.title}${batch.canary.enabled ? ` · 金丝雀 ${batch.canary.node} 先跑` : ''}`, tone: 'ok' });
  }
  return dlgApi;
}
export { SAMPLE_MAIL, SAMPLE_CSV, PROXY_SKIP };
