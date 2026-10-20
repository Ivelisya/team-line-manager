// pages/account.js — 账户与安全 #/account（新后台 fleetd 账户，fleet/API.md §8；替代原版 Komari 的“账户”页）。
// 版面：页头 → 登录账户（kv）→ 修改密码 → 两步验证（开启：生成密钥 → 验证器里输入 → 6 位码确认；关闭：6 位码）→ 登录会话（可退出单个会话）。
// 读：GET /auth/me、/auth/sessions。写（写开关打开时；只允许 fleetd 会话，经原版会话登录时 403 → 说明）：
//   POST /auth/password {old_password, new_password}（≥ 10 字符，其他会话全部作废）、/auth/totp/setup、/auth/totp/enable {code}、
//   /auth/totp/disable {code}、DELETE /auth/sessions/{id}。退出当前会话 = 顶栏“退出登录”。
import { fmt } from '../../../shared/index.js';
import {
  esc, txt, on, pageHead, panel, dataTable, empty, note, button, tonePill, copyable, kv, toast, hydrate, tableSkeleton, confirmAction,
} from '../components.js';
import { refreshBtn } from '../page-tools.js';
import { fleetGet, fleetWrite } from '../fleet.js';
import { guard, isLive, fleetHas } from '../live-mode.js';
import { logout } from '../login.js';

const VIA = { fleetd: '新后台账户', komari: '原版后台会话（过渡期）', local: '本机', anonymous: '—' };
// 会话时间是新后台的真实时钟（不是数据快照的时钟）：相对时间按本机现在算
const when = (t) => (t ? `${txt(fmt.ago(t, { now: Date.now() / 1000 }))}<span class="sub">${esc(fmt.stamp(t).text)}</span>` : '<span class="t-muted">—</span>');
/** 浏览器 UA → 短名（Chrome 129 · macOS），只为认出是哪台设备。 */
export function uaShort(ua) {
  const s = String(ua ?? '');
  if (!s) return '—';
  const b = /Edg\/(\d+)/.exec(s) ? `Edge ${/Edg\/(\d+)/.exec(s)[1]}` : /Chrome\/(\d+)/.exec(s) ? `Chrome ${/Chrome\/(\d+)/.exec(s)[1]}` : /Firefox\/(\d+)/.exec(s) ? `Firefox ${/Firefox\/(\d+)/.exec(s)[1]}` : /Safari\//.test(s) ? 'Safari' : /curl\//.test(s) ? 'curl' : s.slice(0, 24);
  const os = /Mac OS X|Macintosh/.test(s) ? 'macOS' : /Windows/.test(s) ? 'Windows' : /Android/.test(s) ? 'Android' : /iPhone|iPad/.test(s) ? 'iOS' : /Linux/.test(s) ? 'Linux' : '';
  return os ? `${b} · ${os}` : b;
}
const group4 = (k) => String(k ?? '').replace(/(.{4})/g, '$1 ').trim();

export default {
  title: '账户与安全',
  mount({ el }) {
    let me = null, sessions = null, err = null, setup = null;
    el.innerHTML = `<div class="page" data-page="account">
      ${pageHead({ title: '账户与安全', tools: refreshBtn() })}
      <div data-slot="me">${panel({ title: '登录账户', body: tableSkeleton(2, 2, { two: false }) })}</div>
      <div data-slot="pw"></div>
      <div data-slot="totp"></div>
      <div data-slot="sessions"></div>
    </div>`;
    const slot = (k) => el.querySelector(`[data-slot="${k}"]`);

    async function load() {
      if (!isLive() || !fleetHas('auth')) { err = '账户由新后台管理；这个环境没有新后台的账户接口'; render(); return; }
      try {
        [me, sessions] = await Promise.all([fleetGet('auth/me', null), fleetGet('auth/sessions', null).catch(() => null)]);
        err = null;
      } catch (e) { err = e.auth ? null : e.message; }
      if (el.isConnected) render();
    }
    const fleetSession = () => me?.via === 'fleetd';
    const komariNote = () => note({ tone: 'info', body: esc('你现在是用原版后台（Komari）的会话登录的。修改密码、两步验证只对新后台账户生效：请退出后用新后台账户登录（同一个用户名和密码）。') });

    function render() {
      if (err) {
        slot('me').innerHTML = panel({ title: '登录账户', body: note({ tone: 'bad', title: '读取失败', body: esc(err), action: button({ label: '重试', size: 'sm', attrs: { 'data-act': 'refresh' } }) }) });
        for (const k of ['pw', 'totp', 'sessions']) slot(k).innerHTML = '';
        return;
      }
      slot('me').innerHTML = panel({ title: '登录账户', tools: button({ label: '退出登录', icon: 'log-out', size: 'md', attrs: { 'data-act': 'logout' } }),
        body: kv([
          { label: '用户名', value: esc(me?.username ?? '—') },
          { label: '登录方式', value: esc(VIA[me?.via] ?? me?.via ?? '—') },
          { label: '两步验证', value: me?.totp_enabled ? tonePill('已开启', 'ok') : tonePill('未开启', 'warn') },
          { label: '会话', value: '闲置 24 小时或登录满 7 天后需要重新登录' },
        ], { cols: 2 }) });
      // 修改密码
      slot('pw').innerHTML = panel({ title: '修改密码', sub: '至少 10 个字符；改完后其他设备上的会话全部退出',
        body: fleetSession() ? `<form class="stack-sm account-form" data-form="pw" novalidate>
            <label class="field"><span class="field-label">当前密码</span><input class="input" type="password" name="old" autocomplete="current-password" maxlength="256"></label>
            <label class="field"><span class="field-label">新密码</span><input class="input" type="password" name="new" autocomplete="new-password" maxlength="256"></label>
            <label class="field"><span class="field-label">再输一次新密码</span><input class="input" type="password" name="again" autocomplete="new-password" maxlength="256"></label>
            <p class="field-error" data-err="pw" hidden></p>
            <div class="row">${button({ label: '修改密码', variant: 'primary', attrs: { 'data-act': 'pw' } })}</div>
          </form>` : komariNote() });
      // 两步验证
      let totpBody;
      if (!fleetSession()) totpBody = komariNote();
      else if (me?.totp_enabled) {
        totpBody = `<div class="stack-sm"><p class="dialog-text">登录时除了密码，还要输入验证器里的 6 位数字。</p>
          <form class="row" data-form="totp-off" novalidate><input class="input num account-code" name="code" inputmode="numeric" autocomplete="one-time-code" maxlength="6" placeholder="6 位验证码" aria-label="关闭前输入验证码">${button({ label: '关闭两步验证', cls: 'is-danger', attrs: { 'data-act': 'totp-off' } })}</form>
          <p class="field-error" data-err="totp" hidden></p></div>`;
      } else if (setup) {
        totpBody = `<div class="stack-sm">${note({ tone: 'info', body: esc('在验证器（1Password、Google Authenticator 等）里手动添加：账户名任意，密钥如下，类型“基于时间”，6 位、30 秒。添加后输入它显示的 6 位数字完成开启。') })}
          ${kv([{ label: '密钥', value: copyable(setup.secret, { display: group4(setup.secret), cls: 'mono' }) }, { label: '添加链接', value: copyable(setup.otpauth_uri, { display: '复制 otpauth:// 链接', cls: 'mono' }) }], { cols: 1 })}
          <form class="row" data-form="totp-on" novalidate><input class="input num account-code" name="code" inputmode="numeric" autocomplete="one-time-code" maxlength="6" placeholder="6 位验证码" aria-label="验证码">${button({ label: '确认开启', variant: 'primary', attrs: { 'data-act': 'totp-on' } })}${button({ label: '取消', attrs: { 'data-act': 'totp-cancel' } })}</form>
          <p class="field-error" data-err="totp" hidden></p></div>`;
      } else {
        totpBody = `<div class="stack-sm"><p class="dialog-text">开启后，登录时除了密码还要输入验证器里的 6 位数字。建议每个运维账户都开启。</p><div class="row">${button({ label: '开启两步验证', variant: 'primary', icon: 'lock', attrs: { 'data-act': 'totp-setup' } })}</div><p class="field-error" data-err="totp" hidden></p></div>`;
      }
      slot('totp').innerHTML = panel({ title: '两步验证', tools: me?.totp_enabled ? tonePill('已开启', 'ok') : '', body: totpBody });
      // 会话
      const list = sessions?.sessions ?? (Array.isArray(sessions) ? sessions : null);
      const cols = [
        { key: 'dev', label: '设备', cell: (x) => `<span class="strong">${esc(uaShort(x.ua))}</span>${x.current ? ` ${tonePill('当前', 'info', { dot: 'none' })}` : ''}`, sub: (x) => (x.ip ? `<span class="mono">${esc(x.ip)}</span>` : '') },
        { key: 'created', label: '登录', width: 140, cell: (x) => when(x.created_at) },
        { key: 'seen', label: '最近活动', width: 140, cell: (x) => when(x.last_seen) },
        { key: 'exp', label: '到期', width: 140, cell: (x) => (x.expires_at ? `${esc(fmt.stamp(x.expires_at).text)}` : '—') },
        { key: 'act', label: '', width: 120, align: 'right', cell: (x) => (x.current ? '' : button({ label: '退出这个会话', size: 'sm', attrs: { 'data-act': 'revoke', 'data-id': x.id } })) },
      ];
      slot('sessions').innerHTML = !fleetSession() ? '' : panel({ title: '登录会话', count: list?.length ?? null, sub: '新后台账户在各设备上的登录', flush: true,
        body: list ? dataTable({ columns: cols, rows: list, getId: (x) => String(x.id), rowLink: false, two: true, label: '登录会话', empty: empty({ icon: 'users', text: '没有其它会话' }) }) : `<div class="panel-body">${empty({ icon: 'users', text: '读不到会话列表' })}</div>` });
      hydrate(el);
    }

    const showErr = (k, msg) => { const e = el.querySelector(`[data-err="${k}"]`); if (e) { e.hidden = !msg; e.textContent = msg || ''; } };
    async function write(key, method, path, body, btn) {
      if (guard(key)) return null;
      btn?.setAttribute('aria-busy', 'true'); if (btn) btn.disabled = true;
      try { return await fleetWrite(method, path, body); }
      finally { btn?.removeAttribute('aria-busy'); if (btn) btn.disabled = false; }
    }
    const offs = [on(el, 'click', '[data-act]', async (e, b) => {
      const a = b.dataset.act;
      if (a === 'refresh') { b.classList.add('is-busy'); await load(); b.classList.remove('is-busy'); }
      else if (a === 'logout') logout();
      else if (a === 'pw') {
        const f = el.querySelector('[data-form="pw"]');
        const [o, n, again] = ['old', 'new', 'again'].map((k) => f.elements[k].value);
        if (!o) return showErr('pw', '请输入当前密码');
        if ([...n].length < 10) return showErr('pw', '新密码至少 10 个字符');
        if (n !== again) return showErr('pw', '两次输入的新密码不一样');
        showErr('pw', '');
        try {
          const r = await write('account.password', 'POST', 'auth/password', { old_password: o, new_password: n }, b);
          if (!r) return;
          f.reset();
          toast({ message: '密码已修改；其他设备上的会话已退出' });
          load();
        } catch (x) { if (!x.auth) showErr('pw', x.message); }
      } else if (a === 'totp-setup') {
        try { const r = await write('account.totp', 'POST', 'auth/totp/setup', {}, b); if (r) { setup = r; render(); el.querySelector('[data-form="totp-on"] input')?.focus(); } }
        catch (x) { if (!x.auth) showErr('totp', x.message); }
      } else if (a === 'totp-cancel') { setup = null; render(); }
      else if (a === 'totp-on' || a === 'totp-off') {
        const code = el.querySelector(`[data-form="${a}"] input`).value.replace(/\s+/g, '');
        if (!/^\d{6}$/.test(code)) return showErr('totp', '验证码是 6 位数字');
        try {
          const r = await write('account.totp', 'POST', a === 'totp-on' ? 'auth/totp/enable' : 'auth/totp/disable', { code }, b);
          if (!r) return;
          setup = null;
          toast({ message: a === 'totp-on' ? '两步验证已开启；下次登录要输入验证码' : '两步验证已关闭' });
          load();
        } catch (x) { if (!x.auth) showErr('totp', x.message); }
      } else if (a === 'revoke') {
        const x = (sessions?.sessions ?? sessions ?? []).find((s) => String(s.id) === b.dataset.id);
        if (!x) return;
        const yes = await confirmAction({ action: 'account.sessionRevoke', title: '退出这个会话', sub: `${uaShort(x.ua)}${x.ip ? ` · ${x.ip}` : ''}`, body: '那台设备会立即退出登录，需要重新输入密码。', confirm: '退出会话', danger: true });
        if (!yes) return;
        try { const r = await write('account.sessionRevoke', 'DELETE', `auth/sessions/${encodeURIComponent(x.id)}`, undefined, b); if (r) { toast({ message: '已退出这个会话' }); load(); } }
        catch (y) { if (!y.auth) toast({ message: `操作失败：${y.message}`, tone: 'bad' }); }
      }
    }), on(el, 'submit', 'form', (e) => { e.preventDefault(); e.target.querySelector('[data-act]')?.click(); })];
    load();
    return { unmount() { offs.forEach((f) => f()); } };
  },
};
