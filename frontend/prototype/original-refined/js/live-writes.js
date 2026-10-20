// live-writes.js — 类 a 写操作（只动运营数据库：资产、续费、故障 / 行为确认、监控设置、API 密钥）。
//
// 只在实时模式且构建开关 TEAMOPS_WRITES 打开时生效（deploy/v2/build.sh 用 WRITES=1 构建）；默认构建仍是只读预览。
// 去哪儿（deploy/v2/LIVE-MODE.md §13、§18）：
//   · 新后台已开放对应的原生写（fleet/API.md §9.2，TEAMOPS.fleet.has(feature)）→ fleetd /v2/api/v1/…，头 X-Fleet-Request: 1；
//   · 否则、且旧系统还没下线 → 旧后台 POST /api/team-ops/<path>，头 X-Ops-Request: 1（旧运营页的写请求标记）；
//   · 旧系统已下线（legacy_source = native）而新后台还没开放 → 不发请求，提示“新后台尚未开放”。
// 响应 { data } / { error }；401 → 登录页；409（版本已变化）原文提示；成功后立即让轮询器重读一次。
import { toast } from './ui/layers.js';
import { writesEnabled, showAuth } from './live-mode.js';

export { writesEnabled };

const F = () => globalThis.TEAMOPS?.fleet ?? null;
const enc = encodeURIComponent;
const cents = (price) => (price === '' || price == null ? undefined : Math.round(Number(price) * 100));
/**
 * 旧接口 → 新后台原生写（fleet/API.md §9.2）：[feature, method, path, body]；不认识的路径返回 null（只走旧接口）。
 */
export function nativeRoute(path, body = {}) {
  const b = body ?? {};
  switch (String(path).replace(/^\//, '')) {
    case 'assets/edit':
      if (b.renewal) return ['renewals', 'POST', `assets/${enc(b.uuid)}/renewals`, { new_due: b.due_date, amount_cents: cents(b.price), note: b.renewal_note ?? '' }];
      return ['assets.write', 'PATCH', `assets/${enc(b.uuid)}`, { expected_revision: b.expected_revision, name: b.name, price: b.price, due_date: b.due_date, note: b.note ?? '' }];
    case 'node/profile':
      return ['profile', 'PUT', `nodes/${enc(b.uuid)}/profile`, { expected_revision: b.expected_revision, maintenance_until: b.profile?.maintenance_until ?? 0, thresholds: b.profile?.thresholds ?? {} }];
    case 'incidents/ack':
      return ['incidents.ack', 'POST', `incidents/${enc(b.id)}/ack`, { note: b.note ?? '' }];
    case 'behavior/ack':
      return ['behavior.ack', 'POST', 'behavior/ack', b];
    case 'api-keys/create':
      return ['api-keys', 'POST', 'api-keys', { name: b.name, scope: b.scope, expires_days: b.expires_days }];
    case 'api-keys/revoke':
      return ['api-keys', 'POST', `api-keys/${enc(b.id)}/revoke`, {}];
    default:
      return null;
  }
}
/** 旧系统下线后 fleetd 原样接手的旧写接口（fleet/API.md §12.2 POST /v2/api/v1/legacy-write/<旧路径>，请求体与返回同旧接口）。 */
export const LEGACY_WRITE = new Set(['assets/edit', 'node/profile', 'incidents/ack', 'behavior/ack', 'api-keys/create', 'api-keys/revoke', 'policies/draft', 'policies/discard', 'policies/apply']);
/** 这个写会去哪儿：'legacy-write'（fleetd 接手的旧接口）| 'native'（§9.2）| 'legacy'（旧系统还在）| null（都没有）。 */
export function writeTarget(path, body) {
  const f = F();
  const p = String(path).replace(/^\//, '');
  if (LEGACY_WRITE.has(p) && f?.has?.('legacy.write')) return 'legacy-write';
  const r = nativeRoute(path, body);
  if (r && f?.has?.(r[0])) return 'native';
  return globalThis.TEAMOPS?.decommissioned ? null : 'legacy';
}

/** postOps('/assets/edit', body) → data；失败抛 Error（message 为服务端 error 文案）。 */
export async function postOps(path, body) {
  if (!writesEnabled()) throw new Error('只读预览：写操作未开启');
  const target = writeTarget(path, body);
  if (target === null) throw new Error('新后台尚未开放这个操作');
  if (target === 'legacy-write') {
    try { return await F().write('POST', `legacy-write/${String(path).replace(/^\//, '')}`, body ?? {}); }
    catch (e) {
      if (e?.status === 401) showAuth({});
      if (e?.status === 0 || e instanceof TypeError) throw new Error('新后台暂不可用，请稍后重试');
      throw e;
    }
  }
  if (target === 'native') {
    const [, method, p, b] = nativeRoute(path, body);
    try { return await F().write(method, p, b); }
    catch (e) {
      if (e?.status === 401) showAuth({});
      if (e?.status === 0 || e instanceof TypeError) throw new Error('新后台暂不可用，请稍后重试');
      throw e;
    }
  }
  let res;
  try {
    res = await fetch('/api/team-ops/' + String(path).replace(/^\//, ''), {
      method: 'POST', credentials: 'same-origin', cache: 'no-store',
      headers: { 'Content-Type': 'application/json', Accept: 'application/json', 'X-Ops-Request': '1' },
      body: JSON.stringify(body ?? {}),
    });
  } catch {
    throw new Error('运营服务暂不可用，请稍后重试');
  }
  let out = null;
  try { out = await res.json(); } catch { out = null; }
  if (res.status === 401) { showAuth({}); throw Object.assign(new Error(out?.error || '请先登录管理员账户'), { auth: true, status: 401 }); }
  if (!res.ok) throw Object.assign(new Error(out?.error || `操作失败（${res.status}）`), { status: res.status });
  return out?.data ?? out;
}

/**
 * runWrite(app, { path, body, ok, busy }) — 发写请求 + 统一反馈：成功 toast（ok 文案）并让轮询器马上重读；
 * 失败 toast（bad，不自动消失，写服务端原文）。busy：按钮元素，请求期间 aria-busy。返回 data 或 null。
 */
export async function runWrite(app, { path, body, ok, busy = null, refresh = ['minute', 'slow'] }) {
  busy?.setAttribute('aria-busy', 'true');
  if (busy) busy.disabled = true;
  try {
    const data = await postOps(path, body);
    // /assets/edit and /node/profile return the updated asset: keep it until the next read catches up (fleetd lags ≤ 1 poll)
    if (data?.uuid && Number.isFinite(data?.revision)) globalThis.TEAMOPS?.source?.noteAsset?.(data);
    if (ok) toast({ message: typeof ok === 'function' ? ok(data) : ok });
    app?.live?.refresh?.({ force: refresh }).catch(() => {});
    return data ?? {};
  } catch (e) {
    if (!e.auth) toast({ message: `操作失败：${e.message}`, tone: 'bad' });
    return null;
  } finally {
    busy?.removeAttribute('aria-busy');
    if (busy) busy.disabled = false;
  }
}
