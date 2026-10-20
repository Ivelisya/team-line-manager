// live-intel.js — IP 情报（运营商 · 城市 · 接入方式）给所有列出来源 IP 的地方（节点行为的来源列表、证据对话框、最近连接）。
// 实时：情报在 store.raw.ipintel.sources（ui/intel.js intelOf 查这里）；没有的 IP 向 fleetd 要：
//   有 ipintel.batch → GET /v2/api/v1/ipintel/batch?ips=…（一次 ≤ 100）；否则 GET /v2/api/v1/ipintel/{ip}（同时 ≤ 3，结果缓存）。
// 线路机自己的 IP 不查：直接写成“线路机2（内部）”（flags internal），从不显示“没有画像”。
import { intelOf } from './ui/intel.js';

const F = () => globalThis.TEAMOPS?.fleet ?? null;
const asked = new Set(); // 已经问过的 IP（成功或失败都不再问）
const COUNTRY = { CN: '中国', US: '美国', HK: '中国香港', TW: '中国台湾', JP: '日本', SG: '新加坡', KR: '韩国', MO: '中国澳门' };

/** fleetd intel → snapshot 来源记录（与 deploy/v2/live/synth.js intelRecord 同一映射）。 */
export function toRecord(ip, x) {
  if (!x) return null;
  const i = x.intel ?? x;
  const relay = i.relay ?? (typeof i.internal === 'string' ? i.internal : null);
  if ((i.flags ?? []).includes('internal') || relay) return { ip, carrier: `${relay ?? '线路机'}（内部）`, flags: ['internal'], internal: relay, conn_type: null, country: null };
  return { ip, carrier: i.isp ?? null, asn: i.asn ?? null, as_org: i.as_org ?? null, country: i.country ?? null, country_name: COUNTRY[i.country] ?? i.country ?? null,
    province: i.region ?? null, city: i.city ?? null, conn_type: i.type ?? null, conn_conf: i.type_confidence ?? null, flags: i.flags ?? [], internal: i.relay ?? null };
}
/** 线路机自身的地址：store 里的线路机 IP → “线路机2（内部）”。 */
export function relayRecord(store, ip) {
  const r = store.relays?.().find((x) => x.ip === ip) ?? store.nodes?.().find((n) => n.isRelay && n.ip === ip);
  if (!r) return null;
  const name = r.name ?? r.id;
  return { ip, carrier: `${name}（内部）`, flags: ['internal'], internal: r.id, conn_type: null, country: null };
}

function merge(store, recs) {
  const raw = store.raw;
  if (!raw || !recs.length) return;
  const seen = new Set(recs.map((x) => x.ip));
  raw.ipintel = { ...(raw.ipintel ?? {}), sources: [...(raw.ipintel?.sources ?? []).filter((x) => !seen.has(x.ip)), ...recs] };
}

let pending = null;
/**
 * ensureIntel(store, ips, onReady) — 让这些 IP 都有情报；缺的去取，取到后调用 onReady（合并成一次，250 ms 内）。
 * 返回 true 表示有 IP 正在取。原型模式、fleetd 不在、没有 ipintel 能力时只处理线路机地址。
 */
export function ensureIntel(store, ips, onReady = () => {}) {
  const want = [...new Set((ips ?? []).filter(Boolean))].filter((ip) => !intelOf(store, ip) && !asked.has(ip));
  if (!want.length) return false;
  const relays = want.map((ip) => relayRecord(store, ip)).filter(Boolean);
  if (relays.length) { merge(store, relays); for (const r of relays) asked.add(r.ip); }
  const rest = want.filter((ip) => !relays.some((r) => r.ip === ip));
  const f = F();
  if (!rest.length || !f?.available || !f.has?.('ipintel')) { if (relays.length) onReady(); return relays.length > 0; }
  for (const ip of rest) asked.add(ip);
  const done = (recs) => { merge(store, recs.filter(Boolean)); clearTimeout(pending); pending = setTimeout(onReady, 250); };
  if (f.has('ipintel.batch')) {
    for (let i = 0; i < rest.length; i += 100) {
      const chunk = rest.slice(i, i + 100);
      f.get('ipintel/batch', { ips: chunk.join(',') }).then((d) => done(chunk.map((ip) => toRecord(ip, d?.[ip]))), () => {});
    }
    return true;
  }
  // one by one, at most 3 in flight (the data layer's scheduler also caps it)
  const queue = rest.slice(0, 200);
  const worker = async () => {
    for (let ip = queue.shift(); ip; ip = queue.shift()) {
      try { const d = await f.get(`ipintel/${encodeURIComponent(ip)}`, null); done([toRecord(ip, d)]); } catch (e) { if (e?.auth) return; }
    }
  };
  for (let k = 0; k < 3; k++) worker();
  return true;
}
