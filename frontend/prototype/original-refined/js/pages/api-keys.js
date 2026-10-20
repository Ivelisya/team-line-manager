// pages/api-keys.js — API 密钥 #/api-keys（原版 /admin/api-keys，plugin-live/public/apikeys.js）。
// 版面照原版三块：页头（API 密钥 + 新建密钥 + ↻）→ 密钥列表 → 最近调用 → 接口说明（导出 JSON）。
// 读：旧后端 /api-keys、/api-keys/usage、/schema（当前数据源，fleetd 的 /v2/api/legacy/ 或 /api/team-ops/）。
// 写（类 a，写开关打开时）：POST /api/team-ops/api-keys/create {name, scope, expires_days}、/api-keys/revoke {id}；
// 新密钥只显示这一次（带复制与调用示例），撤销要勾选“我确认”——与原版相同。
import { fmt } from '../../../shared/index.js';
import {
  esc, num, txt, on, pageHead, watchStuck, panel, dataTable, empty, note, button, iconButton, tonePill, copyable,
  openDialog, toast, hydrate, skel, tableSkeleton,
} from '../components.js';
import { refreshBtn } from '../page-tools.js';
import { legacyGet } from '../fleet.js';
import { guard, isLive } from '../live-mode.js';
import { runWrite } from '../live-writes.js';
import { swrGet, swrSet } from '../swr.js';

const SCOPE = { read: ['只读', 'info'], write: ['读写', 'warn'], full: ['完整权限', 'neutral'], admin: ['仅会话', 'neutral'] };
const scopePill = (s) => { const [w, t] = SCOPE[s] ?? [s ?? '—', 'neutral']; return tonePill(w, t, { dot: 'none' }); };
const when = (t) => (t ? `${txt(fmt.ago(t))}<span class="sub">${esc(fmt.stamp(t).text)}</span>` : '<span class="t-muted">—</span>');
const nowS = () => Date.now() / 1000;

export default {
  title: 'API 密钥',
  density: true,
  mount({ el, app }) {
    let keys, usage, schema, err = null;
    el.innerHTML = `<div class="page" data-page="api-keys">
      ${pageHead({ title: 'API 密钥', tools: button({ label: '新建密钥', variant: 'primary', icon: 'plus', attrs: { 'data-act': 'new' } }) + refreshBtn() })}
      <div data-slot="keys">${panel({ title: '密钥列表', flush: true, body: tableSkeleton(6, 4) })}</div>
      <div data-slot="usage">${panel({ title: '最近调用', flush: true, body: tableSkeleton(6, 5, { two: false }) })}</div>
      <div data-slot="schema">${panel({ title: '接口说明', flush: true, body: tableSkeleton(4, 6, { two: false }) })}</div>
    </div>`;
    const slot = (k) => el.querySelector(`[data-slot="${k}"]`);

    async function load() {
      if (!isLive()) { err = '原型没有 API 密钥数据（需要实时数据）'; render(); return; }
      if (!keys) { const c = swrGet('api-keys'); if (c) { ({ keys, usage, schema } = c); if (el.isConnected) render(); } }
      try {
        [keys, usage, schema] = await Promise.all([legacyGet('/api-keys'), legacyGet('/api-keys/usage'), legacyGet('/schema')]);
        err = null;
        swrSet('api-keys', { keys, usage, schema });
      } catch (e) { err = e.message; }
      if (el.isConnected) render();
    }
    function render() {
      if (err) {
        slot('keys').innerHTML = panel({ title: '密钥列表', body: note({ tone: 'bad', title: '读取失败', body: esc(err), action: button({ label: '重试', size: 'sm', attrs: { 'data-act': 'refresh' } }) }) });
        slot('usage').innerHTML = ''; slot('schema').innerHTML = '';
        return;
      }
      const cols = [
        { key: 'name', label: '名称', cell: (k) => `<span class="strong">${esc(k.name)}</span>`, sub: (k) => `<span class="mono">${esc(k.prefix ?? '')}…</span>` },
        { key: 'scope', label: '权限', width: 110, cell: (k) => scopePill(k.scope) },
        { key: 'created', label: '创建', width: 130, cell: (k) => when(k.created_at) },
        { key: 'exp', label: '过期', width: 130, cell: (k) => (k.expires_at == null ? '长期有效' : k.expires_at < nowS() ? tonePill('已过期', 'bad') : `${esc(fmt.date(k.expires_at).text)}<span class="sub">${esc(fmt.ago(k.expires_at).text)}</span>`) },
        { key: 'used', label: '最近使用', align: 'right', width: 150, cell: (k) => (k.last_used_at ? txt(fmt.ago(k.last_used_at)) : '<span class="t-muted">从未使用</span>'), sub: (k) => `${num(fmt.int(k.use_count ?? 0))} 次` },
        { key: 'state', label: '状态', width: 96, cell: (k) => (k.revoked_at ? tonePill('已撤销', 'neutral') : k.expires_at != null && k.expires_at <= nowS() ? tonePill('已过期', 'warn') : tonePill('可使用', 'ok')) },
        { key: 'act', label: '', width: 56, align: 'right', cell: (k) => (k.revoked_at ? '' : iconButton({ icon: 'x', label: `撤销 ${k.name}`, size: 'sm', attrs: { 'data-act': 'revoke', 'data-id': k.id } })) },
      ];
      const live = (keys ?? []).filter((k) => !k.revoked_at).length;
      slot('keys').innerHTML = panel({ title: '密钥列表', count: keys?.length ?? null, sub: keys?.length ? `可使用 ${live}` : '', flush: true,
        body: dataTable({ columns: cols, rows: keys ?? [], getId: (k) => k.id, rowLink: false, label: '密钥列表', empty: empty({ icon: 'key-round', text: '暂无密钥' }) }) });
      const ucols = [
        { key: 'ts', label: '时间', width: 140, cell: (u) => when(u.ts) },
        { key: 'key', label: '密钥', width: 140, cell: (u) => esc(u.name ?? '—') },
        { key: 'm', label: '方法', width: 72, cell: (u) => `<span class="mono">${esc(u.method)}</span>` },
        { key: 'p', label: '路径', cls: 'clip', cell: (u) => `<span class="mono">${esc(u.path)}</span>` },
        { key: 's', label: '状态', width: 72, align: 'right', cell: (u) => (u.status == null ? '—' : tonePill(String(u.status), u.status >= 500 ? 'bad' : u.status >= 400 ? 'warn' : u.status >= 200 && u.status < 300 ? 'ok' : 'neutral', { dot: 'none' })) },
        { key: 'ip', label: '来源 IP', width: 150, cell: (u) => (u.ip ? copyable(u.ip) : '—') },
      ];
      const rows = (usage ?? []).slice(0, 50);
      slot('usage').innerHTML = panel({ title: '最近调用', count: usage?.length ?? null, sub: usage?.length > 50 ? '显示最近 50 条' : '', flush: true,
        body: dataTable({ columns: ucols, rows, getId: (u) => `${u.ts}-${u.method}-${u.path}`, rowLink: false, two: true, label: '最近调用', empty: empty({ icon: 'inbox', text: '暂无记录' }) }) });
      const scols = [
        { key: 'm', label: '方法', width: 72, cell: (e) => `<span class="mono">${esc(e.method)}</span>` },
        { key: 'p', label: '路径', width: 220, cell: (e) => `<span class="mono">${esc(e.path)}</span>`, sub: (e) => (e.params ? `<span class="mono">${esc(e.params)}</span>` : '') },
        { key: 's', label: '权限', width: 110, cell: (e) => scopePill(e.scope) },
        { key: 'd', label: '说明', cls: 'wrap', cell: (e) => esc(e.description ?? '') },
      ];
      const base = schema?.auth?.base_url ? `<span class="mono">${esc(schema.auth.base_url)}</span> · 请求头 <span class="mono">${esc(schema.auth.header ?? 'X-Api-Key')}</span>` : '';
      slot('schema').innerHTML = panel({ title: '接口说明', count: schema?.endpoints?.length ?? null, sub: base, flush: true,
        tools: button({ label: '导出 JSON', icon: 'download', size: 'md', attrs: { 'data-act': 'export' } }),
        body: dataTable({ columns: scols, rows: schema?.endpoints ?? [], getId: (e) => `${e.method} ${e.path}`, rowLink: false, label: '接口说明', empty: empty({ text: '没有接口说明' }) }) });
      hydrate(el);
    }

    function create() {
      if (guard('apikeys.create')) return;
      openDialog({
        title: '新建密钥', size: 'sm', guard: true,
        body: `<form data-form novalidate class="fields">
          <label class="field is-wide"><span class="field-label">名称</span><input class="input" name="name" maxlength="100" placeholder="例如 agent-skill" autocomplete="off"></label>
          <div class="field is-wide"><span class="field-label">权限</span><div class="radios" role="radiogroup">
            ${[['read', '只读', '查看资产、线路、健康、行为、费用'], ['write', '读写', '另可确认故障与行为事件、编辑资产、执行入口操作'], ['full', '完整权限', '另可管理 API 密钥']]
              .map(([v, l, d], i) => `<label class="check radio" data-tip="${esc(d)}"><input type="radio" name="scope" value="${v}"${i === 0 ? ' checked' : ''}>${esc(l)}</label>`).join('')}</div>
            <span class="field-hint">只读：查看资产、线路、健康、行为、费用 · 读写：另可确认故障、编辑资产、执行入口操作 · 完整权限：另可管理 API 密钥</span></div>
          <label class="field is-wide"><span class="field-label">有效期</span><select class="select" name="days"><option value="30">30 天</option><option value="90" selected>90 天</option><option value="180">180 天</option><option value="0">长期有效</option></select></label>
          <p class="field-error" data-err hidden></p></form>`,
        foot: button({ label: '取消', attrs: { 'data-dlg': 'cancel' } }) + button({ label: '创建密钥', variant: 'primary', attrs: { 'data-create': '', disabled: true } }),
        init(dlg, api) {
          const f = dlg.querySelector('[data-form]');
          const btn = dlg.querySelector('[data-create]');
          f.elements.name.addEventListener('input', () => { btn.disabled = !f.elements.name.value.trim(); });
          btn.addEventListener('click', async () => {
            const body = { name: f.elements.name.value.trim(), scope: f.elements.scope.value, expires_days: Number(f.elements.days.value) };
            const k = await runWrite(app, { path: '/api-keys/create', body, busy: btn, refresh: [] });
            if (!k) return;
            api.markDirty(false);
            api.close();
            showKey(k);
            toast({ message: `已创建密钥 ${k.name}` });
            load();
          });
        },
      });
    }
    function showKey(k) {
      openDialog({
        title: `密钥 ${k.name}`, sub: `${SCOPE[k.scope]?.[0] ?? k.scope} · ${k.expires_at ? `到 ${fmt.date(k.expires_at).text}` : '长期有效'}`, size: 'md',
        body: `${note({ tone: 'warn', body: '密钥只显示这一次，关闭后无法再查看。' })}
          <div class="row">${copyable(k.key, { cls: 'mono' })}</div>
          ${schema?.auth?.example ? `<p class="field-label">调用示例</p><pre class="codeblock">${esc(schema.auth.example)}</pre>` : ''}`,
        foot: button({ label: '复制密钥', icon: 'copy', attrs: { 'data-copy-key': '' } }) + button({ label: '关闭', variant: 'primary', attrs: { 'data-dlg': 'cancel' } }),
        init(dlg) {
          dlg.querySelector('[data-copy-key]').addEventListener('click', () => navigator.clipboard?.writeText(k.key).then(() => toast({ message: '已复制密钥' }), () => toast({ message: '复制失败，请手动选择复制', tone: 'bad' })));
        },
      });
    }
    function revoke(id) {
      if (guard('apikeys.revoke')) return;
      const k = keys?.find((x) => String(x.id) === String(id));
      if (!k) return;
      openDialog({
        title: `撤销 ${k.name}`, sub: `${k.prefix ?? ''}… · ${SCOPE[k.scope]?.[0] ?? k.scope}`, size: 'sm',
        body: `<p class="dialog-text">撤销后使用该密钥的调用立即失败，不可恢复。</p><label class="check"><input type="checkbox" data-ok>我确认</label>`,
        foot: button({ label: '取消', attrs: { 'data-dlg': 'cancel' } }) + button({ label: '撤销', variant: 'danger', attrs: { 'data-revoke': '', disabled: true } }),
        focus: 'cancel',
        init(dlg, api) {
          const btn = dlg.querySelector('[data-revoke]');
          dlg.querySelector('[data-ok]').addEventListener('change', (e) => { btn.disabled = !e.target.checked; });
          btn.addEventListener('click', async () => {
            const ok = await runWrite(app, { path: '/api-keys/revoke', body: { id: k.id }, busy: btn, refresh: [], ok: `已撤销 ${k.name}` });
            if (ok) { api.close(); load(); }
          });
        },
      });
    }
    function exportSchema() {
      if (!schema) return;
      const url = URL.createObjectURL(new Blob([JSON.stringify(schema, null, 2)], { type: 'application/json' }));
      const a = document.createElement('a');
      a.href = url; a.download = '团队线路管理-接口说明.json'; a.click();
      setTimeout(() => URL.revokeObjectURL(url), 1000);
    }

    const offs = [watchStuck(el)];
    offs.push(on(el, 'click', '[data-act]', (e, b) => {
      const a = b.dataset.act;
      if (a === 'new') create();
      else if (a === 'revoke') revoke(b.dataset.id);
      else if (a === 'export') exportSchema();
      else if (a === 'refresh') { b.classList.add('is-busy'); load().finally(() => b.classList.remove('is-busy')); }
    }));
    load();
    void skel;
    return { unmount() { offs.forEach((f) => f()); } };
  },
};
