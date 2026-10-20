// provision.js — “新增家宽节点”“新增线路机”向导（原版 openProvisionWizard('node' | 'relay')，inventory ops.md §4.1、§4.2）。
// 入口：线路页页头两个按钮、每台线路机入口分段的“新增家宽节点”（预选这台线路机）；节点页、运营总览也可以直接调用。
// 精修（SPEC §3.9、§3.17、§4、§7.3）：
//   - 三步 `1 填写 ─ 2 确认 ─ 3 执行`，底栏固定在对话框底部（修原版“下一步”随表单滚走）；
//   - 去掉“我已确认”勾选框，风险写进确认页的提示条；按钮文字就是动作（开始接入）；
//   - 套餐下拉只列规范名（按供应商过滤），选中后带出带宽和月费；原始套餐字符串不出现；
//   - 客户端校验补上原版缺的：IP 格式、端口范围、编号与 IP 重名、日期格式；错误写在字段下方；
//   - 名称预览按供应商给缩写（修原版 OhnoICU 被预览成 NM，后端实际是 MX）；
//   - 密码只写不回显，确认页写 `已提供 · 未验证`；
//   - 方向词：原版“出站类型”“建入站”“出站与路由”改成 上游协议 / 建入口 / 转发与路由（V4 术语）。
// 原型不真的执行：第 3 步按真实步骤顺序与耗时量级模拟，结束时写明“原型演示，未改数据”。

import { fmt } from '../../shared/index.js';
import {
  esc, icon, on, openDialog, button, select, secretInput, affix, timeline, note, toast, dlRows, wizardSteps,
  copyText, hydrate, neutralChip, mark,
} from './components.js';
import { allPlans, planById } from './plans.js';
import { guard, isLive } from './live-mode.js';

/** 服务端步骤（原版 8 步 / 线路机 11 步）；显示名按术语表改过，key 与快照 onboarding.history[].steps[].key 一致。 */
export const NODE_STEPS = [['check_outbound', '凭据校验'], ['inbound', '建入口'], ['outbound_rule', '转发与路由'], ['enable', '启用核对'], ['verify_path', '端到端'], ['register', '中央登记'], ['probe', '探针扩容'], ['verify', '最终验收']];
export const RELAY_STEPS = [['connect', '连接主机'], ['prepare', '系统准备'], ['xui', '安装 3x-ui 3.7.0'], ['token', '面板令牌'], ['master_inbound', '主节点 Reality'], ['logging', '访问日志'], ['agent', '主机探针'], ['probes', '链路探针骨架'], ['bridge', '中央采集通道'], ['register', '中央登记'], ['verify', '验收']];
/** 步骤 key → 显示名，按任务类型分开（两类都有 register / verify）；接入任务面板用它把快照里的旧名“建入站”“出站与路由”换掉。 */
export const STEP_NAMES = { node: Object.fromEntries(NODE_STEPS), relay: Object.fromEntries(RELAY_STEPS) };

const TZS = [['America/Los_Angeles', '洛杉矶 America/Los_Angeles'], ['America/New_York', '纽约 America/New_York'], ['Asia/Shanghai', '北京 Asia/Shanghai'], ['Europe/London', '伦敦 Europe/London']];
const SS_METHODS = ['aes-256-gcm', 'chacha20-ietf-poly1305', '2022-blake3-aes-256-gcm'];
const LABELS = ['填写', '确认', '执行'];
const IPV4 = /^(25[0-5]|2[0-4]\d|1\d\d|[1-9]?\d)(\.(25[0-5]|2[0-4]\d|1\d\d|[1-9]?\d)){3}$/;
const DATE = /^\d{4}-\d{2}-\d{2}$/;
const DEMO_KEY = 'ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIDemoOnlyNotARealKey0000000000000000000 team-ops-provision（演示）';

const tzLabel = (tz) => fmt.tzBadge(tz) ?? '北京';
const nextPort = (store, relayId) => Math.max(20000, ...store.entriesFor(relayId).map((e) => e.port)) + 1;
const codeOf = (store, provider) => store.providers().find((p) => p.name === provider)?.code ?? '—';

/**
 * openProvisionWizard(app, kind = 'node' | 'relay', { relayId, onJob })
 * onJob(job) 在开始执行和每一步变化时调用（线路页用它把任务放进“接入任务”面板）。
 */
export function openProvisionWizard(app, kind = 'node', { relayId = null, onJob } = {}) {
  // 只读预览：不开向导（运营总览、节点页、线路页的“新增”都汇到这里）
  if (guard(kind === 'relay' ? 'provision.relay' : 'provision.node', { relayId })) return null;
  // 实时 + 新后台开放了上架流水线：新增家宽节点一律走上架流水线（CSV → 计划 → 开始），不再用旧的单台向导
  if (isLive() && kind === 'node') { app.navigate('#/onboarding?new=1'); return null; }
  const store = app.store;
  const isNode = kind === 'node';
  const relays = store.relays();
  const teams = store.teams().map((t) => t.name).filter((t) => t !== '线路机');
  const landingProviders = store.providers().filter((p) => p.kind !== 'relay_vps').map((p) => p.name);
  const relayProviders = store.providers().filter((p) => p.kind === 'relay_vps').map((p) => p.name);
  const plans = allPlans(store);
  const plansFor = (provider, cat) => plans.filter((p) => (cat ? p.category === cat : true) && (!provider || p.provider === provider));

  // ─── 表单状态（预填能预填的，减少重复输入） ───
  const r0 = relays.find((r) => r.id === relayId) ?? relays[0];
  const maxNo = (team) => Math.max(0, ...store.nodes().filter((n) => n.team === team && !n.isRelay).map((n) => n.number ?? 0));
  const v = isNode
    ? {
      relay: r0?.id ?? '', team: teams[0] ?? '', newTeam: '', provider: landingProviders[0] ?? 'ATTVPS', number: String(maxNo(teams[0]) + 1),
      egress: '', upstream: 'shadowsocks', host: '', port: '8388', method: SS_METHODS[0], user: '', password: '', service: '',
      plan: '', bandwidth: '', price: '', due: '', tz: 'America/Los_Angeles', note: '',
    }
    : {
      name: `线路机${relays.length + 1}`, ip: '', ssh: '22', auth: 'password', password: '', provider: relayProviders[0] ?? 'GoMami',
      plan: '', price: '', due: '', reset: '1', tz: 'America/Los_Angeles', note: '',
    };
  const firstPlan = plansFor(v.provider, isNode ? 'landing' : 'relay')[0];
  if (firstPlan) applyPlan(firstPlan.id);
  function applyPlan(id) {
    const p = planById(id);
    v.plan = id;
    if (!p) return;
    if (isNode) v.bandwidth = p.specs?.bandwidth_text ?? v.bandwidth;
    if (p.price_cents != null) v.price = (p.price_cents / 100).toFixed(2);
  }

  let step = 0, errors = {}, job = null, timer = null;
  const title = isNode ? '新增家宽节点' : '新增线路机';
  // guard：填过内容后按 Esc、× 或“取消”先确认“放弃已填写的内容？”，点遮罩不关（craft #2）；开始执行后不再拦
  const dlg = openDialog({
    title, size: 'lg', label: title, guard: true,
    sub: isNode ? '接入一台落地节点：在线路机上建入口、配转发、登记并开始监控' : '部署一台线路机：安装 3x-ui 与探针、建主节点入口、接入中央采集',
    body: '', foot: '<span></span>',
  });
  const bodyEl = dlg.body;
  const footEl = dlg.el.querySelector('.dialog-foot');

  // ─── 字段外形 ───
  const err = (k) => (errors[k] ? `<span class="field-error" id="err-${k}">${esc(errors[k])}</span>` : '');
  const inv = (k) => (errors[k] ? ` aria-invalid="true" aria-describedby="err-${k}"` : '');
  // 单个控件用 <label> 包住；单选组用 role=group（label 里不能套多个 label）
  const field = (k, label, control, { hint = '', wide = false, optional = false, group = false } = {}) => {
    const tag = group ? 'div' : 'label';
    const lab = `${esc(label)}${optional ? '（选填）' : ''}`;
    return `<${tag} class="field${wide ? ' is-wide' : ''}" data-field="${k}"${group ? ` role="group" aria-label="${lab}"` : ''}><span class="field-label">${lab}</span>${control}${hint && !errors[k] ? `<span class="field-hint">${hint}</span>` : ''}${err(k)}</${tag}>`;
  };
  const input = (k, { placeholder = '', type = 'text', mode = '' } = {}) =>
    `<input class="input" name="${k}" type="${type}" value="${esc(v[k] ?? '')}"${placeholder ? ` placeholder="${esc(placeholder)}"` : ''}${mode ? ` inputmode="${mode}"` : ''} autocomplete="off" spellcheck="false"${inv(k)}>`;
  const radios = (k, items) => `<div class="radios" role="radiogroup">${items.map(([val, label]) => `<label class="check radio"><input type="radio" name="${k}" value="${esc(val)}"${v[k] === val ? ' checked' : ''}>${esc(label)}</label>`).join('')}</div>`;
  const sel = (k, options, label) => select({ name: k, value: v[k], options, label, attrs: { name: k, ...(errors[k] ? { 'aria-invalid': 'true' } : {}) } });
  const planHint = () => { const p = planById(v.plan); return p ? esc(p.specs?.summary ?? '') : '不在套餐目录里的套餐，登记后在费用页签补规格'; };

  function formHtml() {
    if (isNode) {
      const r = store.relay(v.relay);
      const plansOpt = plansFor(v.provider, 'landing').map((p) => ({ value: p.id, label: p.name }));
      return `<div class="fields">
        ${field('relay', '线路机', sel('relay', relays.map((x) => ({ value: x.id, label: `${x.name} · 下一个端口 ${nextPort(store, x.id)}` })), '线路机'), { hint: r ? `入口建在 ${esc(r.name)} :${nextPort(store, r.id)}（已用端口的下一个）` : '' })}
        ${field('team', '团队', sel('team', [...teams.map((t) => ({ value: t, label: t })), { value: '__new', label: '新团队…' }], '团队'))}
        ${v.team === '__new' ? field('newTeam', '新团队名称', input('newTeam', { placeholder: '例如 团队3' })) : ''}
        ${field('provider', '供应商', radios('provider', landingProviders.map((p) => [p, p])), { group: true })}
        ${field('number', '节点编号', input('number', { mode: 'numeric', placeholder: '1–999' }), { hint: `名称 ${esc(previewName())}` })}
        ${field('egress', '出口 IP', input('egress', { placeholder: '198.51.100.x', mode: 'decimal' }), { hint: '家宽出口的公网 IPv4，接入时会核对' })}
        ${field('upstream', '上游协议', radios('upstream', [['shadowsocks', 'shadowsocks'], ['socks', 'socks']]), { group: true })}
        ${field('host', '上游服务器地址', input('host', { placeholder: 'NAT 机填映射后的地址' }))}
        ${field('port', '上游端口', input('port', { mode: 'numeric', placeholder: '1–65535' }), { hint: v.provider === 'OhnoICU' ? 'NAT 机填映射后的端口' : '' })}
        ${v.upstream === 'shadowsocks' ? field('method', '加密方式', sel('method', SS_METHODS.map((m) => ({ value: m, label: m })), '加密方式')) : field('user', '用户名', input('user'))}
        ${field('password', '上游密码', secretInput({ name: 'password', label: '上游密码', attrs: errors.password ? { 'aria-invalid': 'true' } : {} }))}
        ${field('service', '服务号', input('service', { placeholder: '供应商后台的服务编号' }), { optional: true })}
        ${field('plan', '套餐', sel('plan', [...plansOpt, { value: '', label: '其他（不在套餐目录）' }], '套餐'), { hint: planHint() })}
        ${field('bandwidth', '带宽', input('bandwidth', { placeholder: '50 Mbps' }), { hint: '用于利用率计算' })}
        ${field('price', '每月费用', affix(input('price', { mode: 'decimal', placeholder: '0.00' }), 'USD'))}
        ${field('due', '到期日', affix(input('due', { placeholder: 'YYYY-MM-DD' }), tzLabel(v.tz)), { hint: '按供应商时区的日历日计算剩余天数' })}
        ${field('tz', '供应商时区', sel('tz', TZS.map(([value, label]) => ({ value, label })), '供应商时区'))}
        ${field('note', '备注', input('note'), { optional: true, wide: true })}
      </div>`;
    }
    const plansOpt = plansFor(null, 'relay').map((p) => ({ value: p.id, label: p.name }));
    return `<div class="fields">
      ${field('name', '名称', input('name'))}
      ${field('ip', '公网 IP', input('ip', { placeholder: '203.0.113.x', mode: 'decimal' }))}
      ${field('ssh', 'SSH 端口', input('ssh', { mode: 'numeric' }))}
      ${field('auth', '认证方式', radios('auth', [['password', 'root 密码'], ['key', '已装公钥']]), { group: true })}
      ${v.auth === 'password'
        ? field('password', 'root 密码', secretInput({ name: 'password', label: 'root 密码', attrs: errors.password ? { 'aria-invalid': 'true' } : {} }), { hint: '只用来装公钥，之后改为密钥登录' })
        : `<div class="field is-wide"><span class="field-label">中控的部署公钥（先装到目标机 root 的 authorized_keys）</span><pre class="codeblock">${esc(DEMO_KEY)}</pre><span>${button({ label: '复制公钥', icon: 'copy', size: 'sm', attrs: { 'data-wz': 'copy-key' } })}</span></div>`}
      ${field('provider', '供应商', sel('provider', [...relayProviders.map((p) => ({ value: p, label: p })), { value: '其他', label: '其他' }], '供应商'))}
      ${field('plan', '套餐', sel('plan', [...plansOpt, { value: '', label: '其他（不在套餐目录）' }], '套餐'), { hint: planHint() })}
      ${field('price', '每月费用', affix(input('price', { mode: 'decimal', placeholder: '0.00' }), 'USD'))}
      ${field('due', '到期日', affix(input('due', { placeholder: 'YYYY-MM-DD' }), tzLabel(v.tz)))}
      ${field('reset', '流量重置日', affix(input('reset', { mode: 'numeric' }), '日'), { hint: '每月这一天按供应商时区重置流量额度' })}
      ${field('tz', '时区', sel('tz', TZS.map(([value, label]) => ({ value, label })), '时区'))}
      ${field('note', '备注', input('note'), { optional: true, wide: true })}
    </div>`;
  }

  // ─── 名称预览与校验 ───
  function teamName() { return v.team === '__new' ? v.newTeam.trim() : v.team; }
  function previewName() {
    const n = Number(v.number);
    return teamName() && Number.isInteger(n) && n > 0 ? fmt.shortName(teamName(), n) : '—';
  }
  function validate() {
    const e = {};
    const need = (k, label) => { if (!String(v[k] ?? '').trim()) e[k] = `请填写${label}`; };
    const port = (k, label) => { const p = Number(v[k]); if (!(Number.isInteger(p) && p >= 1 && p <= 65535)) e[k] = `${label}应为 1–65535 的整数`; };
    const ip = (k) => {
      const s = String(v[k] ?? '').trim();
      if (!s) return;
      if (!IPV4.test(s)) { e[k] = '不是有效的 IPv4 地址'; return; }
      const dup = store.nodes().find((n) => n.ip === s);
      if (dup) e[k] = `已被 ${dup.name} 使用`;
    };
    const date = (k) => {
      const s = String(v[k] ?? '').trim();
      if (!s) return;
      const d = new Date(`${s}T00:00:00Z`);
      if (!DATE.test(s) || Number.isNaN(d.getTime()) || d.toISOString().slice(0, 10) !== s) e[k] = '日期写成 YYYY-MM-DD';
    };
    const money = (k) => { const s = String(v[k] ?? '').trim(); if (s && !/^\d{1,6}(\.\d{1,2})?$/.test(s)) e[k] = '金额最多两位小数'; };
    if (isNode) {
      if (v.team === '__new') { need('newTeam', '新团队名称'); if (['线路机', '全部'].includes(v.newTeam.trim())) e.newTeam = '这个名称保留给系统'; }
      const n = Number(v.number);
      if (!(Number.isInteger(n) && n >= 1 && n <= 999)) e.number = '编号应为 1–999 的整数';
      else if (store.node(fmt.shortName(teamName(), n))) e.number = `${fmt.shortName(teamName(), n)} 已存在`;
      need('egress', '出口 IP'); ip('egress');
      need('host', '上游服务器地址');
      need('port', '上游端口'); if (v.port) port('port', '端口');
      if (v.upstream === 'socks') need('user', '用户名');
      need('password', '上游密码');
      need('price', '每月费用'); money('price');
      need('due', '到期日'); date('due');
    } else {
      need('name', '名称'); if (store.relay(v.name.trim()) || store.node(v.name.trim())) e.name = `${v.name.trim()} 已存在`;
      need('ip', '公网 IP'); ip('ip');
      port('ssh', 'SSH 端口');
      if (v.auth === 'password') need('password', 'root 密码');
      need('price', '每月费用'); money('price');
      need('due', '到期日'); date('due');
      const d = Number(v.reset); if (!(Number.isInteger(d) && d >= 1 && d <= 31)) e.reset = '应为 1–31';
    }
    return e;
  }

  // ─── 确认页 ───
  function confirmHtml() {
    const p = planById(v.plan);
    const priceF = v.price ? fmt.money(Math.round(Number(v.price) * 100), { perMonth: true }).text : '—';
    const due = v.due ? `${esc(v.due)} ${mark(tzLabel(v.tz))}` : '—';
    if (isNode) {
      const r = store.relay(v.relay);
      const full = `${previewName()} · ${codeOf(store, v.provider)} · ${v.egress.trim()}`;
      const left = [
        { label: '名称', value: `<b>${esc(previewName())}</b>`, tip: `完整名称 ${full}` },
        { label: '入口', value: esc(`${r.name}:${nextPort(store, r.id)}`) },
        { label: '团队', value: esc(teamName()) },
        { label: '供应商', value: esc(v.provider) },
        { label: '出口 IP', value: esc(v.egress.trim()) },
        { label: '上游', value: esc(`${v.upstream} ${v.host.trim()}:${v.port}`) },
        { label: v.upstream === 'shadowsocks' ? '加密方式' : '用户名', value: esc(v.upstream === 'shadowsocks' ? v.method : v.user) },
      ];
      const right = [
        { label: '上游密码', value: neutralChip('已提供 · 未验证') },
        { label: '服务号', value: esc(v.service || '—') },
        { label: '套餐', value: esc(p?.name ?? '其他（不在套餐目录）') },
        { label: '带宽', value: esc(v.bandwidth || '—') },
        { label: '每月费用', value: esc(priceF) },
        { label: '到期', value: due },
        { label: '备注', value: esc(v.note || '—') },
      ];
      return `<div class="dl-grid">${dlRows(left)}${dlRows(right)}</div>
        ${note({ tone: 'warn', title: '会短暂打断线路机', body: esc(`转发与路由热应用失败时 3x-ui 会重启 Xray：${r.name} 的 ${store.entriesFor(r.id).length} 个入口中断约 1–3 秒，出口 IP 不变。`) })}
        ${note({ tone: 'info', body: esc(`完成后自动登记为 ${previewName()}，并新建 TCP（30 秒）与出口验证（5 分钟）两个探测。完整名称 ${full}。`) })}`;
    }
    const left = [
      { label: '名称', value: `<b>${esc(v.name.trim())}</b>` },
      { label: '公网 IP', value: esc(v.ip.trim()) },
      { label: 'SSH', value: esc(`:${v.ssh} · ${v.auth === 'password' ? 'root 密码（已提供 · 未验证）' : '已装公钥'}`) },
      { label: '供应商', value: esc(v.provider) },
      { label: '套餐', value: esc(p?.name ?? '其他（不在套餐目录）') },
    ];
    const right = [
      { label: '每月费用', value: esc(priceF) },
      { label: '到期', value: due },
      { label: '流量重置日', value: esc(`每月 ${v.reset} 日`) },
      { label: '时区', value: esc(v.tz) },
      { label: '备注', value: esc(v.note || '—') },
    ];
    return `<div class="dl-grid">${dlRows(left)}${dlRows(right)}</div>
      ${note({ tone: 'warn', title: '会覆盖目标机', body: '目标机上现有的 3x-ui 与 Xray 会被覆盖；要求 x86_64、root、Ubuntu 或 Debian。' })}
      ${note({ tone: 'info', body: esc(`完成后主节点入口在 ${v.ip.trim() || '公网 IP'}:20001，面板凭据保存在该机 /etc/team-ops/bridge.json。`) })}`;
  }

  // ─── 执行页（原型模拟：按真实步骤顺序推进） ───
  function execHtml() {
    const items = job.steps.map((s) => ({
      status: s.status, title: esc(s.name),
      meta: s.duration_s != null ? esc(fmt.duration(s.duration_s).text) : s.status === 'running' ? '进行中' : '',
      desc: s.error ? `<span class="t-bad">${esc(s.error)}</span>` : '',
    }));
    const done = job.status !== 'running';
    const head = done
      ? note({ tone: 'ok', title: isNode ? `已接入 ${job.name}` : `已部署 ${job.name}`, body: `${esc(job.result)}（原型演示：未真正执行，也未改数据）` })
      : note({ tone: 'info', title: `正在执行 · ${job.ok_steps}/${job.total_steps} 步`, body: '可以关掉这个对话框，任务在后台继续，进度在线路页的“接入任务”里。' });
    return `${head}${timeline(items, { label: '执行步骤' })}`;
  }
  function startJob() {
    const r = isNode ? store.relay(v.relay) : null;
    const defs = isNode ? NODE_STEPS : RELAY_STEPS;
    const name = isNode ? previewName() : v.name.trim();
    job = {
      id: `demo-${Date.now().toString(36)}`, demo: true, type: isNode ? 'residential' : 'relay', type_word: isNode ? '家宽节点' : '线路机',
      name, title: isNode ? `${name} · ${codeOf(store, v.provider)} · ${v.egress.trim()}` : name,
      relay: r?.name ?? null, ip: isNode ? v.egress.trim() : v.ip.trim(), status: 'running', status_word: '进行中',
      started_at: store.now(), duration_s: 0, ok_steps: 0, total_steps: defs.length,
      steps: defs.map(([key, label], i) => ({ key, name: label, status: i === 0 ? 'running' : 'queued', duration_s: null, error: null })),
      result: isNode ? `入口 ${r.name}:${nextPort(store, r.id)} → 出口 ${v.egress.trim()}，已登记为 ${name}` : `主节点入口 ${v.ip.trim()}:20001；面板凭据保存在该机 /etc/team-ops/bridge.json`,
    };
    onJob?.(job);
    toast({ message: `已开始${isNode ? '接入' : '部署'} ${name}（原型演示）`, tone: 'info' });
    const t0 = Date.now();
    const tick = () => {
      const i = job.steps.findIndex((s) => s.status === 'running');
      if (i < 0) return;
      const s = job.steps[i];
      s.status = 'succeeded';
      s.duration_s = Math.round((1 + Math.random() * (isNode ? 12 : 18)) * 10) / 10;
      job.ok_steps = i + 1;
      job.duration_s = job.steps.reduce((a, x) => a + (x.duration_s ?? 0), 0);
      if (job.steps[i + 1]) job.steps[i + 1].status = 'running';
      else { job.status = 'succeeded'; job.status_word = '成功'; job.finished_at = store.now(); }
      onJob?.(job);
      if (dlg.el.isConnected && step === 2) { if (job.status === 'running') bodyEl.innerHTML = wizardSteps(LABELS, step, { label: `${title}步骤` }) + execHtml(); else render(); }
      if (job.status === 'running') timer = setTimeout(tick, 380 + Math.random() * 420);
      else if (!dlg.el.isConnected) toast({ message: `${isNode ? '已接入' : '已部署'} ${name}，用时 ${fmt.duration((Date.now() - t0) / 1000).text}（原型演示）` });
    };
    timer = setTimeout(tick, 500);
  }

  // ─── 渲染 ───
  function render({ focusFirstError = false } = {}) {
    const top = `${wizardSteps(LABELS, step, { label: `${title}步骤` })}`;
    let body = '', foot = '';
    if (step === 0) {
      body = `${top}<form class="stack-sm" novalidate>${isNode ? `<div class="row-between"><span class="field-hint">除标“选填”的都要填；密码只写不回显</span>${button({ label: '登记已存在的入口', variant: 'link', size: 'sm', attrs: { 'data-wz': 'enroll' } })}</div>` : ''}${formHtml()}<button type="submit" hidden></button></form>`;
      foot = button({ label: '取消', attrs: { 'data-dlg': 'cancel' } }) + button({ label: '下一步：确认', variant: 'primary', iconEnd: 'chevron-right', attrs: { 'data-wz': 'next' } });
    } else if (step === 1) {
      body = `${top}${confirmHtml()}`;
      foot = button({ label: '上一步', icon: 'chevron-left', attrs: { 'data-wz': 'back' } }) + button({ label: isNode ? '开始接入' : '开始部署', variant: 'primary', attrs: { 'data-wz': 'start' } });
    } else {
      body = `${top}${execHtml()}`;
      foot = job.status === 'running'
        ? button({ label: '在后台继续', attrs: { 'data-dlg': 'cancel' } }) + button({ label: '完成', variant: 'primary', attrs: { disabled: true, 'aria-busy': 'true' } })
        : button({ label: '完成', variant: 'primary', attrs: { 'data-dlg': 'cancel' } });
    }
    bodyEl.innerHTML = body;
    footEl.innerHTML = foot;
    hydrate(dlg.el);
    if (step === 0) {
      const pw = bodyEl.querySelector('input[name="password"]');
      if (pw) pw.value = v.password;
      const f = focusFirstError ? bodyEl.querySelector('[aria-invalid="true"]') : null;
      if (f) { f.focus(); f.scrollIntoView({ block: 'nearest' }); }
    } else if (focusFirstError !== null) {
      footEl.querySelector('.btn-primary:not(:disabled)')?.focus();
    }
  }

  // 表单输入 → 状态；会改变字段结构的输入（团队、供应商、上游协议、认证方式、套餐、时区）重画
  const RESHAPE = new Set(['team', 'provider', 'upstream', 'auth', 'plan', 'tz', 'number', 'newTeam']);
  const offs = [
    on(bodyEl, 'input', 'input, select', (e, x) => {
      if (!x.name || x.type === 'radio') return;
      v[x.name] = x.value;
      if (errors[x.name]) { delete errors[x.name]; const lab = x.closest('.field'); lab?.querySelector('.field-error')?.remove(); x.removeAttribute('aria-invalid'); }
      if (x.name === 'number' || x.name === 'newTeam') { const h = bodyEl.querySelector('[data-field="number"] .field-hint'); if (h) h.textContent = `名称 ${previewName()}`; }
    }),
    on(bodyEl, 'change', 'input, select', (e, x) => {
      if (!x.name) return;
      v[x.name] = x.value;
      if (x.name === 'provider') { const p = plansFor(v.provider, isNode ? 'landing' : 'relay')[0]; if (p) applyPlan(p.id); else v.plan = ''; }
      if (x.name === 'plan') applyPlan(x.value);
      if (x.name === 'team' && isNode && v.team !== '__new') v.number = String(maxNo(v.team) + 1);
      if (x.name === 'upstream') v.port = v.upstream === 'shadowsocks' ? '8388' : '1080';
      if (RESHAPE.has(x.name) && x.name !== 'number' && x.name !== 'newTeam') { render(); (bodyEl.querySelector(`[name="${x.name}"]:checked`) ?? bodyEl.querySelector(`[name="${x.name}"]`))?.focus(); }
    }),
    on(bodyEl, 'submit', 'form', (e) => { e.preventDefault(); next(); }),
    on(bodyEl, 'click', '[data-wz]', (e, b) => {
      if (b.dataset.wz === 'enroll') toast({ message: '“登记已存在的入口”在原型里未包含：它把 3x-ui 里已有、未归属的入口直接登记成节点', tone: 'info' });
      else if (b.dataset.wz === 'copy-key') copyText(DEMO_KEY, b);
    }),
    on(footEl, 'click', '[data-wz]', (e, b) => {
      const k = b.dataset.wz;
      if (k === 'next') next();
      else if (k === 'back') { step = 0; render(); }
      else if (k === 'start') { step = 2; dlg.markDirty(false); startJob(); render(); }
    }),
  ];
  function next() {
    errors = validate();
    if (Object.keys(errors).length) { render({ focusFirstError: true }); return; }
    step = 1;
    render();
  }
  const prevClose = dlg.close;
  dlg.close = (r) => { offs.forEach((f) => f()); prevClose(r); };
  render({ focusFirstError: null });
  requestAnimationFrame(() => bodyEl.querySelector(isNode ? '[name="egress"]' : '[name="ip"]')?.focus());
  return dlg;
}
