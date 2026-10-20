// swr.js — stale-while-revalidate for page data that is not in the core lists (事项与告警、API 密钥、套餐与配额、通知…）:
// the last response per key in localStorage (namespace teamops:page:), read synchronously for the first paint, replaced
// by the fresh response.  Only in live mode; ≤ 400 KB per entry; cleared on logout (login.js) / 401 (with the boot cache).
const NS = 'teamops:page:';
const live = () => globalThis.TEAMOPS_LIVE === true;
export function swrGet(key) {
  if (!live()) return null;
  try { const v = JSON.parse(globalThis.localStorage?.getItem(NS + key) ?? 'null'); return v && Date.now() - v.at < 24 * 3600e3 ? v.data : null; } catch { return null; }
}
export function swrSet(key, data) {
  if (!live()) return;
  try { const s = JSON.stringify({ at: Date.now(), data }); if (s.length < 400_000) globalThis.localStorage?.setItem(NS + key, s); } catch { /* quota: optional */ }
}
/** Remove every teamops:* entry (boot cache, system, catalog, page caches). */
export function swrClearAll() {
  try {
    const ls = globalThis.localStorage;
    if (!ls) return;
    const keys = [];
    for (let i = 0; i < ls.length; i++) { const k = ls.key(i); if (k?.startsWith('teamops:')) keys.push(k); }
    for (const k of keys) ls.removeItem(k);
  } catch { /* ignore */ }
}
