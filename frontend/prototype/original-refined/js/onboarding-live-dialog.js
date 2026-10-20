// onboarding-live-dialog.js — 实时“新建批次”（新后台 fleetd 上架流水线，fleet/crates/onboard/WIRING.md §0、§2–3）。
// 与原型向导同一个外壳（lg 对话框 + 步骤条，底栏固定），三步：
//   1 填写：粘贴或上传 CSV（fleetctl 同一种格式），选项（金丝雀、并发、合批窗口）→ POST /batches {csv, options} 生成计划；
//   2 计划：后台校验与放置的结果（行、问题、线路机影响、需要人工重装的台数），有错误不能继续；
//   3 确认：计划哈希 + 勾选核对 → POST /batches/{id}/start {plan_hash}（哈希不符 409：计划变了，回到第 2 步重新生成）。
// 密码不进 CSV（有 password 列直接拒绝）：需要密码的机器开始后停在“推密钥”，在矩阵里输入（只写，不回显）。
// confirmStart()：已建好但还没开始的批次（待确认）→ 重新计算计划（GET /plan）→ 同一个确认步骤。
import {
  esc, on, openDialog, button, note, kv, tonePill, affix, hydrate, toast, wizardSteps, dataTable,
} from './components.js';

const STEPS = ['填写', '计划', '确认'];
const MAX_CSV = 60 * 1024; // fleetd 写请求体 ≤ 64 KB
const HEAD = 'provider,ip,ssh_port,auth,plan,team,number,relay,ss_port,dd,mac,service_id,note';
const SAMPLE = `${HEAD}\nATTVPS,198.51.100.21,22,key,ATTVPS 基础版,团队1,41,,,none,,60410,\nOhnoICU,203.0.113.7,2201,key,MX-NAT-1G,团队2,12,线路机3,20801,manual,,MX-8812,NAT 端口 20801`;
const DD_WORD = { none: '不重装', manual: '人工重装' };

/** CSV 的本地检查（后台还会完整校验）：表头必需列、不许有密码列、大小。返回错误文案或 ''。 */
export function csvProblem(text) {
  const t = String(text ?? '').trim();
  if (!t) return '还没有内容';
  if (t.length > MAX_CSV) return `CSV 太大（${Math.ceil(t.length / 1024)} KB），一次最多 60 KB，请分批`;
  const head = t.split(/\r?\n/)[0].toLowerCase().split(',').map((h) => h.trim());
  if (head.includes('password')) return 'CSV 里不要放密码列：需要密码的机器开始后会停在“推密钥”，在矩阵里输入（只写）';
  const miss = ['provider', 'ip', 'ssh_port', 'auth', 'plan', 'team', 'number'].filter((k) => !head.includes(k));
  if (miss.length) return `表头缺少 ${miss.join('、')}（第一行必须是表头）`;
  if (t.split(/\r?\n/).filter((l) => l.trim()).length < 2) return '只有表头，没有机器';
  return '';
}

function issuesHtml(plan) {
  const issues = plan?.issues ?? [];
  if (!issues.length) return '';
  const byLevel = (lv) => issues.filter((i) => i.level === lv);
  const line = (i) => `<li>${i.seq ? `第 ${esc(i.seq)} 行：` : ''}${esc(i.msg)}</li>`;
  const err = byLevel('error'), warn = issues.filter((i) => i.level !== 'error');
  // 标题 + 列表：标题写进正文（note 的 title 后面会接“ · ”，列表前不需要）
  const block = (title, list) => `<span class="note-title">${esc(title)}</span><ul class="ob-issues">${list.map(line).join('')}</ul>`;
  return `${err.length ? note({ tone: 'bad', body: block(`${err.length} 个错误，修正后才能开始`, err) }) : ''}
    ${warn.length ? note({ tone: 'warn', body: block(`${warn.length} 条提醒`, warn) }) : ''}`;
}
function rowsTable(plan) {
  const rows = plan?.rows ?? [];
  if (!rows.length) return '';
  const cols = [
    { key: 'name', label: '节点', cell: (r) => `<span class="strong">${esc(r.name)}</span>`, sub: (r) => `第 ${esc(r.seq)} 行 · ${esc(r.team)}` },
    { key: 'ip', label: 'IP', cell: (r) => `<span class="num">${esc(r.ip)}${r.ssh_port && r.ssh_port !== 22 ? `:${esc(r.ssh_port)}` : ''}</span>`, sub: (r) => (r.nat ? `NAT · SS 公网端口 ${esc(r.ss_public_port ?? '—')}` : '') },
    { key: 'plan', label: '供应商 · 套餐', cell: (r) => esc(`${r.provider} · ${r.plan ?? '未归类套餐'}`), sub: (r) => (r.price != null ? esc(`${r.currency ?? 'USD'} ${r.price}/月`) : '') },
    { key: 'relay', label: '线路机', cell: (r) => esc(r.relay ?? '自动分配') },
    { key: 'dd', label: '重装', cell: (r) => (r.dd === 'manual' ? tonePill(DD_WORD.manual, 'warn', { dot: 'none' }) : esc(DD_WORD[r.dd] ?? r.dd ?? '—')) },
  ];
  return `<div class="table-wrap ob-preview">${dataTable({ columns: cols, rows, getId: (r) => String(r.seq), rowLink: false, two: true, label: '计划中的机器' })}</div>`;
}
function impactHtml(plan) {
  const list = plan?.relay_impact ?? [];
  if (!list.length) return '';
  // 端口预览 “20001–20001（实际以登记时的预留为准）” → “端口 20001（实际以登记时的预留为准）”
  const ports = (p) => String(p ?? '—').replace(/^(\d+)–\1(?!\d)/, '$1');
  return kv(list.map((x) => ({ label: String(x.relay ?? ''), value: `现有 <span class="num">${esc(x.entries_now)}</span> 个入口 · 新增 <span class="num">${esc(x.adding)}</span> 个 · 端口 ${esc(ports(x.ports_preview))}<span class="kv-sub">${esc(x.impact ?? '')}</span>` })), { cols: 1 });
}
function confirmBody(plan, { created, batchId } = {}) {
  const n = plan?.rows?.length ?? 0;
  const opt = plan?.options ?? {};
  const dd = plan?.destructive_count ?? 0;
  return `${kv([
    { label: '批次', value: `<span class="mono">${esc(batchId ?? plan?.batch_id ?? '—')}</span>` },
    { label: '台数', value: `<span class="num">${n}</span> 台` },
    { label: '金丝雀', value: opt.canary === false ? '不用（全部同时开始）' : `先跑第 1 台，走完 ${esc(opt.canary_gate ?? 'P16')} 再放开其余` },
    { label: '并发 · 合批窗口', value: `<span class="num">${esc(opt.concurrency ?? 5)}</span> 台 · <span class="num">${esc(opt.relay_window_s ?? 30)}</span> 秒` },
    { label: '人工重装', value: dd ? `<span class="t-warn">${dd} 台，会停在“重装准备”等你在供应商面板重装</span>` : '没有' },
    { label: '计划哈希', value: `<span class="mono">${esc(plan?.plan_hash ?? '—')}</span>` },
  ], { cols: 2 })}
    ${impactHtml(plan)}
    ${created === false ? note({ tone: 'info', body: esc('同一份 CSV 之前已经建过这个批次（按内容识别，重复提交不会多建）：开始会按这个批次执行，已完成的行不会重做。') }) : ''}
    ${note({ tone: 'warn', body: esc('开始后会登录这些机器、安装 Shadowsocks、在线路机上经 Xray API 加入口（不重启 Xray，现有入口不受影响）。可以放弃单行或取消批次，已完成的步骤会逆序补偿。') })}
    <label class="check"><input type="checkbox" data-ob-ok>我已核对计划 <span class="mono">${esc(plan?.plan_hash ?? '')}</span>，确认开始上架</label>
    <p class="field-error" data-ob-err hidden></p>`;
}

/** 开始：POST start {plan_hash}。409 = 计划变了（返回 'stale'），其他错误抛出。 */
async function start(ob, id, hash) {
  try { await ob.start(id, hash); return 'ok'; }
  catch (e) { if (e?.status === 409 && /哈希/.test(e.message)) return 'stale'; throw e; }
}

/** openLiveBatch({ app, ob, onCreated(batchId) }) */
export function openLiveBatch({ app, ob, onCreated }) {
  const S = { step: 0, csv: '', title: '', canary: true, concurrency: 5, window: 30, plan: null, ok: false, created: null, error: '', busy: false };
  let dlg = null;
  const api = openDialog({
    title: '新建批次', sub: '粘贴或上传 CSV，后台生成计划，核对计划哈希后开始上架', size: 'lg', label: '新建上架批次', guard: true,
    body: '', foot: '<span></span>',
    init(el, a) { dlg = a; wire(el); render(); },
  });
  void app;

  function render() {
    const body = dlg.body;
    const foot = dlg.el.querySelector('.dialog-foot');
    const steps = wizardSteps(STEPS, S.step, { label: '新建批次步骤' });
    let html = '', next = '';
    if (S.step === 0) {
      const prob = S.csv ? csvProblem(S.csv) : '';
      const lines = S.csv.trim() ? S.csv.trim().split(/\r?\n/).filter((l) => l.trim()).length - 1 : 0;
      html = `${steps}
        <div class="field">
          <div class="row-between"><span class="field-label">CSV</span><span class="row">${button({ label: '上传 CSV 文件', icon: 'upload', variant: 'quiet', size: 'sm', attrs: { 'data-ob': 'upload' } })}${button({ label: '填入表头', icon: 'clipboard-paste', variant: 'quiet', size: 'sm', attrs: { 'data-ob': 'head' } })}</span></div>
          <input type="file" accept=".csv,text/csv,text/plain" data-ob-file hidden>
          <textarea class="textarea ob-paste mono" data-ob-text spellcheck="false" aria-label="CSV 内容" placeholder="${esc(`第一行是表头：\n${HEAD}\n\n例如：\n${SAMPLE.split('\n').slice(1).join('\n')}`)}">${esc(S.csv)}</textarea>
          <span class="field-hint" data-ob-count>${prob ? `<span class="t-bad">${esc(prob)}</span>` : lines > 0 ? `<span class="num">${lines}</span> 台` : '必需列：provider、ip、ssh_port、auth、plan、team、number；可选 relay、ss_port（NAT 必填）、dd（none / manual）、mac、service_id、note'}</span>
        </div>
        ${note({ tone: 'info', body: esc('auth 写 key（上架公钥已在 root 的 authorized_keys，例如供应商开机注入）。需要密码的机器开始后停在“推密钥”，在矩阵里输入密码（只写，不回显，不进 CSV）。同一份 CSV 重复提交不会多建批次。') })}
        <div class="fields">
          <label class="field is-wide"><span class="field-label">批次名称（选填）</span><input class="input" data-ob-o="title" value="${esc(S.title)}" maxlength="80" autocomplete="off" placeholder="留空按供应商与台数自动命名"></label>
          <div class="field is-wide"><label class="check"><input type="checkbox" data-ob-o="canary"${S.canary ? ' checked' : ''}><span>先跑第 1 台金丝雀，走完全链路后再放开其余</span></label></div>
          <label class="field"><span class="field-label">并发</span>${affix(`<input class="input" type="number" min="1" max="10" data-ob-o="concurrency" value="${S.concurrency}">`, '台')}<span class="field-hint">同时在执行的最多台数</span></label>
          <label class="field"><span class="field-label">合批窗口</span>${affix(`<input class="input" type="number" min="10" max="300" step="5" data-ob-o="window" value="${S.window}">`, '秒')}<span class="field-hint">窗口内到达同一台线路机的入口合并成一次 Xray API 变更</span></label>
        </div>
        ${S.error ? `<p class="field-error">${esc(S.error)}</p>` : ''}`;
      next = button({ label: S.busy ? '正在生成计划' : '生成计划', iconEnd: 'chevron-right', variant: 'primary', attrs: { 'data-ob': 'plan', disabled: S.busy || !S.csv.trim() || !!prob, 'aria-busy': S.busy ? 'true' : null } });
    } else if (S.step === 1) {
      const p = S.plan;
      const errs = (p?.issues ?? []).filter((i) => i.level === 'error').length;
      const status = errs ? tonePill(`${errs} 个错误`, 'bad') : tonePill('可以继续', 'ok');
      html = `${steps}
        <div class="row-between ob-sum"><span class="muted"><span class="num">${p?.rows?.length ?? 0}</span> 台 · 批次 <span class="mono">${esc(p?.batch_id ?? '—')}</span>${p?.destructive_count ? ` · <span class="t-warn">${p.destructive_count} 台人工重装</span>` : ''}</span>${status}</div>
        ${issuesHtml(p)}${rowsTable(p)}${impactHtml(p)}`;
      next = button({ label: '下一步：确认', iconEnd: 'chevron-right', variant: 'primary', attrs: { 'data-ob': 'next', disabled: !S.ok } });
    } else {
      html = `${steps}${confirmBody(S.plan, { created: S.created, batchId: S.plan?.batch_id })}`;
      next = button({ label: `开始上架 ${S.plan?.rows?.length ?? 0} 台`, icon: 'play', variant: 'primary', attrs: { 'data-ob': 'start', disabled: true } });
    }
    const prev = S.step ? button({ label: '上一步', icon: 'chevron-left', attrs: { 'data-ob': 'prev' } }) : '';
    body.innerHTML = html;
    foot.innerHTML = `${button({ label: '取消', attrs: { 'data-dlg': 'cancel' } })}${prev}${next}`;
    hydrate(dlg.el);
  }

  async function makePlan() {
    S.busy = true; S.error = ''; render();
    try {
      const out = await ob.create({ csv: S.csv.trim() + '\n', title: S.title.trim() || undefined, options: { canary: S.canary, concurrency: S.concurrency, relay_window_s: S.window } });
      S.plan = out.plan; S.ok = out.ok; S.created = out.plan?.created ?? null;
      S.step = 1;
    } catch (e) {
      S.error = e.auth ? '' : `生成计划失败：${e.message}`;
    } finally { S.busy = false; render(); }
  }

  function wire(el) {
    on(el, 'click', '[data-ob]', async (e, b) => {
      const a = b.dataset.ob;
      if (a === 'upload') el.querySelector('[data-ob-file]')?.click();
      else if (a === 'head') { if (!S.csv.trim()) { S.csv = `${HEAD}\n`; render(); const t = el.querySelector('[data-ob-text]'); t.focus(); t.selectionStart = t.selectionEnd = t.value.length; } }
      else if (a === 'plan') makePlan();
      else if (a === 'next') { S.step = 2; render(); }
      else if (a === 'prev') { S.step = Math.max(0, S.step - 1); S.error = ''; render(); }
      else if (a === 'start') {
        b.disabled = true; b.setAttribute('aria-busy', 'true');
        const err = el.querySelector('[data-ob-err]');
        try {
          const r = await start(ob, S.plan.batch_id, S.plan.plan_hash);
          if (r === 'stale') { err.hidden = false; err.textContent = '计划已变化（线路机状态或选项变了）：回到上一步重新生成计划后再确认'; return; }
          api.markDirty(false); api.close();
          toast({ message: `已开始上架 ${S.plan.rows?.length ?? 0} 台${S.canary ? '，金丝雀先跑' : ''}` });
          onCreated?.(S.plan.batch_id);
        } catch (x) {
          if (!x.auth) { err.hidden = false; err.textContent = `开始失败：${x.message}`; }
        } finally { if (b.isConnected) { b.removeAttribute('aria-busy'); b.disabled = !el.querySelector('[data-ob-ok]')?.checked; } }
      }
    });
    on(el, 'change', '[data-ob-ok]', (e, x) => { const b = el.querySelector('[data-ob="start"]'); if (b) b.disabled = !x.checked; });
    on(el, 'input', '[data-ob-text]', (e, t) => {
      S.csv = t.value;
      dlg.markDirty?.(true);
      const prob = csvProblem(S.csv);
      const lines = S.csv.trim() ? S.csv.trim().split(/\r?\n/).filter((l) => l.trim()).length - 1 : 0;
      el.querySelector('[data-ob-count]').innerHTML = prob ? `<span class="t-bad">${esc(prob)}</span>` : `<span class="num">${lines}</span> 台`;
      const nb = el.querySelector('[data-ob="plan"]');
      if (nb) nb.disabled = !!prob;
    });
    on(el, 'change', '[data-ob-file]', (e, input) => {
      const f = input.files?.[0];
      input.value = '';
      if (!f) return;
      if (f.size > MAX_CSV) { toast({ message: `文件太大（${Math.ceil(f.size / 1024)} KB），一次最多 60 KB`, tone: 'bad' }); return; }
      const rd = new FileReader();
      rd.onload = () => { S.csv = String(rd.result ?? '').replace(/^﻿/, ''); render(); };
      rd.onerror = () => toast({ message: '读取文件失败', tone: 'bad' });
      rd.readAsText(f);
    });
    on(el, 'change', '[data-ob-o]', (e, x) => {
      const k = x.dataset.obO;
      if (k === 'title') S.title = x.value;
      else if (k === 'canary') S.canary = x.checked;
      else if (k === 'concurrency') { S.concurrency = Math.max(1, Math.min(10, Math.round(Number(x.value)) || 5)); x.value = S.concurrency; }
      else if (k === 'window') { S.window = Math.max(10, Math.min(300, Math.round(Number(x.value)) || 30)); x.value = S.window; }
    });
  }
  return api;
}

/** confirmStart({ ob, batch, onStarted }) — 待确认的批次：重新计算计划 → 核对哈希 → 开始。 */
export function confirmStart({ ob, batch, onStarted }) {
  let plan = null;
  const api = openDialog({
    title: `确认计划 · ${batch.title}`, sub: batch.id, size: 'lg',
    body: '<p class="dialog-text muted">正在重新计算计划（会读取线路机的当前配置）…</p>',
    foot: button({ label: '取消', attrs: { 'data-dlg': 'cancel' } }) + button({ label: `开始上架 ${batch.rows.length} 台`, icon: 'play', variant: 'primary', attrs: { 'data-ob': 'start', disabled: true } }),
    init(el) {
      ob.plan(batch.id).then((p) => {
        plan = p;
        const errs = (p?.issues ?? []).filter((i) => i.level === 'error').length;
        api.body.innerHTML = `${issuesHtml(p)}${errs ? '' : confirmBody(p, { batchId: batch.id })}`;
        hydrate(api.el);
      }, (e) => { api.body.innerHTML = note({ tone: 'bad', body: esc(`读取计划失败：${e.message}`) }); });
      on(el, 'change', '[data-ob-ok]', (e, x) => { el.querySelector('[data-ob="start"]').disabled = !x.checked; });
      on(el, 'click', '[data-ob="start"]', async (e, b) => {
        if (!plan) return;
        b.disabled = true; b.setAttribute('aria-busy', 'true');
        const err = el.querySelector('[data-ob-err]');
        try {
          const r = await start(ob, batch.id, plan.plan_hash);
          if (r === 'stale') { err.hidden = false; err.textContent = '计划刚刚又变了：关闭后重新确认'; return; }
          api.close();
          toast({ message: `已开始 ${batch.title}` });
          onStarted?.();
        } catch (x) { if (!x.auth) { err.hidden = false; err.textContent = `开始失败：${x.message}`; } }
        finally { if (b.isConnected) b.removeAttribute('aria-busy'); }
      });
    },
  });
  return api;
}
