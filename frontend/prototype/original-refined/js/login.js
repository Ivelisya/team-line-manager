// login.js — v2 登录页（新后台 fleetd 账户，fleet/fleetd/src/accounts.rs；原版登录页的样子：SPEC A-21 / dna §5.4）。
//   卡片 = 对话框外观（圆角、阴影、内边距同 .dialog），品牌在卡片里（修 N9：不被遮罩压住），提交按钮满宽带 LogIn 图标，
//   两步验证码按需出现（后台回 totp_required 才显示）。成功后整页重新加载（数据层按新会话重新开始）。
//   退出：POST /auth/logout（清 fleet_session）。原版后台（Komari）已下线：不再有“用原版账户登录”的入口。
// 只在实时构建、且新后台在这个域名上时出现；原型模式与旧后台（/api/team-ops）时仍是原来的“去登录”对话框。
import { esc } from './ui/dom.js';
import { icon } from './icons.js';
import { note } from './ui/layout.js';
import { swrClearAll } from './swr.js';

const API = '/v2/api/v1/auth/';
const T = () => globalThis.TEAMOPS ?? null;
/** 新后台在不在（401 也算在：只是没登录）。 */
export const fleetLogin = () => !!T()?.fleet?.available || !!T()?.fleet?.detected;

async function post(path, body) {
  const res = await fetch(API + path, {
    method: 'POST', credentials: 'same-origin', cache: 'no-store',
    headers: { 'Content-Type': 'application/json', Accept: 'application/json', 'X-Fleet-Request': '1' },
    body: JSON.stringify(body ?? {}),
  });
  let j = null;
  try { j = await res.json(); } catch { j = null; }
  return { status: res.status, ok: res.ok, body: j };
}

let shown = false;
/**
 * showLogin({ expired }) — 整屏登录页（盖住应用外壳；外壳设 inert）。expired：会话中途失效（说明一句）。
 */
export function showLogin({ expired = false } = {}) {
  if (shown || typeof document === 'undefined') return;
  shown = true;
  swrClearAll(); // 会话失效：本机缓存的数据不再用来打开页面
  document.getElementById('app')?.setAttribute('inert', '');
  document.querySelectorAll('.dialog, .dialog-scrim, .drawer, .drawer-scrim').forEach((x) => x.remove());
  const root = document.createElement('div');
  root.className = 'login-screen';
  root.innerHTML = `<form class="dialog login-card" novalidate aria-labelledby="login-title">
    <div class="dialog-head"><div class="login-head"><div class="login-brand">团队线路管理</div><h1 class="dialog-title" id="login-title">登录</h1><div class="dialog-sub">登录到团队线路管理控制面板。</div></div></div>
    <div class="dialog-body">
      ${expired ? note({ tone: 'warn', body: esc('登录状态已失效，实时数据已停止刷新。重新登录后回到这一页。') }) : ''}
      <label class="field"><span class="field-label">用户名</span><input class="input" name="username" autocomplete="username" autocapitalize="off" spellcheck="false" maxlength="64"></label>
      <label class="field"><span class="field-label">密码</span><input class="input" type="password" name="password" autocomplete="current-password" maxlength="256"></label>
      <label class="field" data-login-totp hidden><span class="field-label">两步验证码</span><input class="input num" name="totp" inputmode="numeric" autocomplete="one-time-code" maxlength="6" spellcheck="false"><span class="field-hint">打开验证器，输入当前的 6 位数字</span></label>
      <div class="field-error" data-login-err role="alert" hidden></div>
      <button type="submit" class="btn btn-primary login-submit">${icon('log-in', 14)}<span>登录</span></button>
    </div>
  </form>`;
  document.body.appendChild(root);
  const form = root.querySelector('form');
  const $ = (n) => form.querySelector(`[name="${n}"]`);
  const err = form.querySelector('[data-login-err]');
  const totpBox = form.querySelector('[data-login-totp]');
  const submit = form.querySelector('.login-submit');
  const showErr = (msg, field) => {
    err.hidden = !msg; err.textContent = msg || '';
    for (const n of ['username', 'password', 'totp']) $(n).removeAttribute('aria-invalid');
    if (field) { $(field).setAttribute('aria-invalid', 'true'); $(field).focus(); }
  };
  form.addEventListener('input', () => { if (!err.hidden) showErr(''); });
  form.addEventListener('submit', async (e) => {
    e.preventDefault();
    const username = $('username').value.trim();
    const password = $('password').value;
    const totp = totpBox.hidden ? '' : $('totp').value.replace(/\s+/g, '');
    if (!username) return showErr('请输入用户名', 'username');
    if (!password) return showErr('请输入密码', 'password');
    if (!totpBox.hidden && !/^\d{6}$/.test(totp)) return showErr('两步验证码是 6 位数字', 'totp');
    submit.disabled = true; submit.classList.add('is-busy');
    let r;
    try { r = await post('login', { username, password, ...(totp ? { totp } : {}) }); } catch { r = null; }
    submit.disabled = false; submit.classList.remove('is-busy');
    if (!r) return showErr('无法连接新后台，请稍后重试');
    if (r.ok) { $('password').value = ''; location.reload(); return; }
    const msg = r.body?.error || `登录失败（${r.status}）`;
    if (r.status === 401 && r.body?.totp_required) {
      const first = totpBox.hidden;
      totpBox.hidden = false;
      if (first) { showErr(''); $('totp').focus(); return; }
      $('totp').value = '';
      return showErr(msg, 'totp');
    }
    if (r.status === 401) { $('password').value = ''; return showErr(msg, 'password'); }
    return showErr(msg);
  });
  setTimeout(() => $('username').focus(), 0);
}

/** 退出登录：新后台会话（原版后台已下线，没有别的会话要退），然后回到登录页。 */
export async function logout() {
  try { await post('logout', {}); } catch { /* 已经失效也一样回登录页 */ }
  // 退出后不再用本机缓存的数据打开页面
  swrClearAll();
  location.reload();
}
