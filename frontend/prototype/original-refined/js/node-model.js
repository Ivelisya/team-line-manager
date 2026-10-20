// node-model.js — 节点字段的“显示模型”：卡片、抽屉、表格共用同一套取值与格式（全部经过 fmt）。
// 页面代码不要自己从 snapshot 字段拼文字，先看这里有没有现成的。

import { fmt } from '../../shared/index.js';
import { planOf, quotaLimit } from './plans.js';
import { num, esc } from './ui/dom.js';
import { mark } from './ui/controls.js';

const now = () => fmt.now();

// ─── 链路延迟：对应检查失败 / 超时时不显示原始毫秒（监控在超时后仍会带上耗时，例如 path_ms 18028 → “超时”） ───
const LAT_CHECK = { tcp: 'tcp', egress: 'egress', path: 'ingress' };
/** probeFail(n, 'tcp' | 'egress' | 'path') → 'timeout' | 'failed' | null。出口 IP 不符不算（耗时是真实的，另有“不符”标记）。 */
export function probeFail(n, kind) {
  const c = (n?.status?.failed_checks ?? []).find((x) => x.key === LAT_CHECK[kind]);
  if (!c || (c.status !== 'fail' && c.status !== 'degraded') || c.pending) return null;
  if (c.reason === 'egress_mismatch') return null;
  return c.reason === 'timeout' ? 'timeout' : 'failed';
}
/** latencyF(n, kind, ms?) → fmt.latency，失败 / 超时时是“失败” / “超时”（bad）。ms 默认取 n.latency.<kind>_ms。 */
export function latencyF(n, kind, ms = n?.latency?.[`${kind}_ms`]) {
  const s = probeFail(n, kind);
  return fmt.latency(ms, { failed: s === 'failed', timeout: s === 'timeout' });
}

// ─── 在线与鲜度（圆点只表示探针在线，§4.6 S-01） ────────────────────────────

/** 'on' | 'off' | 'unknown' */
export function probeState(n) {
  const o = n.agent?.online;
  return o === true ? 'on' : o === false ? 'off' : 'unknown';
}
/** 最后上报时刻（主机 agent 或代发探针）。 */
export function lastReport(n) { return n.agent?.reported_at ?? n.host?.reported_at ?? null; }
export function probeTip(n) {
  const s = probeState(n);
  const kind = n.isProxy ? '代发探针' : '探针';
  const t = lastReport(n);
  const when = t ? fmt.ago(t).text : null;
  if (s === 'on') return `${kind}在线${when ? ` · ${when}上报` : ''}`;
  if (s === 'off') return `${kind}离线${when ? ` · 最后上报 ${when}` : ''}`;
  return `${kind}状态未知`;
}
/** “离线 12 分钟”（在线时空串）。 */
export function offlineText(n) {
  if (probeState(n) !== 'off') return '';
  const t = lastReport(n);
  return t ? `离线 ${fmt.duration(now() - t, { units: 1 }).text}` : '离线';
}
/** 卡片上的实时数值是否按“数据过期”显示（旧值 + --fg-3）。 */
export function isStale(n) { return probeState(n) === 'off' || (n.hasHost && n.host?.fresh === false); }
export function staleAge(n) { const t = lastReport(n); return t ? now() - t : null; }

/** 健康（状态 pill 只表示健康）。 */
export function healthOf(store, n) { return fmt.health(n.status?.code, { stale: store.staleOf(n) }); }

// ─── 字段 ───────────────────────────────────────────────────────────────────

/** 入口：线路机自身显示 `主节点:20005`（原版写“直接连接”）；未绑定 → 未绑定。 */
export function entryText(store, n) {
  if (n.isRelay) {
    const r = store.relay(n.id);
    const port = r?.entries_summary?.master_port ?? n.relay_port;
    return port ? `主节点:${port}` : '主节点';
  }
  return n.entry_label ?? '未绑定';
}
/** 套餐（规范名；兜底模式带 `原名` 标记）。 */
export function planHtml(n, { button = false } = {}) {
  const p = planOf(n);
  const cls = p.unresolved ? ' is-unresolved' : '';
  const tip = p.unresolved ? '套餐目录里没有这个节点' : p.raw ? '套餐目录未加载，显示原始名称' : '';
  const name = `${esc(p.name)}${p.raw ? ' ' + mark('原名', { warn: true }) : ''}`;
  if (button) return `<button type="button" class="plan-btn${cls}" data-act="plan" data-plan="${esc(p.id)}"${tip ? ` data-tip="${esc(tip)}"` : ''}>${name}</button>`;
  return `<span class="${cls ? 't-muted' : ''}"${tip ? ` data-tip="${esc(tip)}"` : ''}>${name}</span>`;
}
export function bandwidthF(n) { return n.bandwidth?.capacity_bps ? fmt.bandwidth(n.bandwidth.capacity_bps) : fmt.unset(); }
export function utilOf(n) { return fmt.utilPair(n.bandwidth?.util_down_pct, n.bandwidth?.util_up_pct); }
/** 卡片 footer 的带宽 chip：原版写“50 Mbps”，精修把业务利用率并进同一个 chip：“50 Mbps ↓<1% ↑2%”（SPEC §9 Q4 取合并写法）。 */
export function bandwidthChipModel(n) {
  const bw = bandwidthF(n);
  const u = utilOf(n);
  const hasU = u && (u.down.kind !== 'missing' || u.up.kind !== 'missing');
  if (bw.kind !== 'value' && !hasU) return null;
  const text = [bw.kind === 'value' ? bw.text : null, hasU ? u.text : null].filter(Boolean).join(' ');
  const tip = [`带宽 ${bw.text}`, hasU ? `业务利用率 ${u.text}（上下行分别计算）` : null].filter(Boolean).join(' · ');
  return { text, tip };
}
/** 标签：供应商 + Komari 标签 + 线路机名（线路机自身不重复写自己）。 */
export function tagsOf(n) {
  const out = [n.provider, ...(n.tags ?? [])];
  if (n.relay && !n.isRelay) out.push(n.relay);
  return [...new Set(out.filter(Boolean))];
}
export function feeF(n) { return fmt.monthlyFee(n.billing, { perMonth: true }); }
export function dueF(n) { return fmt.due(n.billing); }
/** 到期值 HTML：`20 天`（分级着色）+ 非北京时区的 `LA` 标记。 */
export function dueHtml(n) {
  const f = dueF(n);
  const tip = `${f.exact ?? ''}${n.billing?.cycle_word ? ` · ${n.billing.cycle_word}` : ''}`;
  return `${num(f, { title: false })}${f.badge ? mark(f.badge, { title: tip }) : ''}`;
}
/** 持续运行值（卡内标签“持续运行”+ 值 `10 天`）；无主机 → 无主机（--fg-3）。 */
export function uptimeF(n) {
  if (!n.hasHost) return fmt.na('无主机');
  const s = n.host?.uptime_s;
  if (s == null) return fmt.missing();
  if (s >= 86400) { const d = Math.floor(s / 86400); return new fmt.Fmt(`${d} 天`, { num: String(d), unit: '天', sep: ' ', raw: s, exact: fmt.duration(s, { units: 3 }).text }); }
  // 不到一天：最小单位到分钟（秒级每 2 秒一变，读不清也会抖）
  return s < 3600 ? fmt.duration(s, { units: 1 }) : fmt.duration(Math.floor(s / 60) * 60);
}

// ─── 资源（§3.13） ──────────────────────────────────────────────────────────

/**
 * 四条资源进度线的模型：CPU / 内存 / 磁盘 / 负载。阈值取节点 limits（默认 90 / 90 / 85；负载每核 100）。
 * precise：大卡片按原版显示 1 位小数（6.3%），小卡片、紧凑、表格取整（SPEC §9 Q8 取原版写法，fidelity）。负载一律取整。
 */
export function resourceMeters(n, { precise = false } = {}) {
  const h = n.host;
  if (!h) return null;
  const L = n.limits ?? {};
  const cpuLim = L.cpu_percent ?? 90, memLim = L.memory_percent ?? 90, diskLim = L.disk_percent ?? 85;
  const pair = (u, t) => `${fmt.bytes(u).text} / ${fmt.bytes(t).text}`;
  const load = h.load5_per_core_pct;
  return [
    { key: 'cpu', icon: 'cpu', label: 'CPU', value: fmt.pctResource(h.cpu_pct, { precise }), pct: h.cpu_pct, tone: fmt.meterTone(h.cpu_pct, cpuLim), sub: h.cores ? `${h.cores} 核` : '—', tip: `CPU ${fmt.pctResource(h.cpu_pct, { precise: true }).text} · 阈值 ${cpuLim}%` },
    { key: 'mem', icon: 'memory-stick', label: '内存', value: fmt.pctResource(h.mem_pct, { precise }), pct: h.mem_pct, tone: fmt.meterTone(h.mem_pct, memLim), sub: pair(h.mem_used, h.mem_total), tip: `内存 ${fmt.pctResource(h.mem_pct, { precise: true }).text} · 阈值 ${memLim}%${h.swap_total ? ` · Swap ${pair(h.swap_used, h.swap_total)}` : ''}` },
    { key: 'disk', icon: 'hard-drive', label: '磁盘', value: fmt.pctResource(h.disk_pct, { precise }), pct: h.disk_pct, tone: fmt.meterTone(h.disk_pct, diskLim), sub: pair(h.disk_used, h.disk_total), tip: `磁盘 ${fmt.pctResource(h.disk_pct, { precise: true }).text} · 阈值 ${diskLim}%` },
    { key: 'load', icon: 'gauge', label: '负载', value: fmt.pctLoad(load), pct: Math.min(100, load ?? 0), tone: fmt.meterTone(load, 100), sub: fmt.loadTriple(h.load1, h.load5, h.load15).text, tip: `${fmt.loadLabel(load, { cores: h.cores }).text} · 原始 1 / 5 / 15 分钟均值` },
  ];
}

// ─── 网卡（主机）与入口流量（代理） ─────────────────────────────────────────

export function nic(n) {
  const h = n.host ?? {};
  return { rx: fmt.rate(h.net_rx_bps), tx: fmt.rate(h.net_tx_bps), rxT: fmt.bytes(h.net_total_rx), txT: fmt.bytes(h.net_total_tx) };
}
/** 代理卡入口流量小表：实时 / 今日 / 本月 × ↓ 下行 / ↑ 上行，来源 `Xray · 3 秒前`（过期 warn）。 */
export function entryTraffic(n) {
  const r = n.rate ?? {}, T = n.traffic ?? {};
  const fresh = r.fresh !== false && r.sample_at != null && now() - r.sample_at <= 120;
  const liveD = fresh ? fmt.rate(r.down_bps) : fmt.missing(), liveU = fresh ? fmt.rate(r.up_bps) : fmt.missing();
  const srcAge = r.sample_at ? fmt.ago(r.sample_at).text : '未采样';
  return {
    rows: [
      ['实时', liveD, liveU],
      ['今日', fmt.bytes(T.today?.down), fmt.bytes(T.today?.up)],
      ['本月', fmt.bytes(T.month?.down), fmt.bytes(T.month?.up)],
    ],
    src: `Xray · ${srcAge}`,
    srcTone: fresh ? null : 'warn',
    srcTip: `Xray 入口计数（业务流量，含监控探测）· 采样 ${r.sample_at ? fmt.absolute(r.sample_at) : '—'}；不等于供应商账单`,
  };
}

// ─── 流量上限（R6） ─────────────────────────────────────────────────────────

/**
 * quotaModel(n) → { rem(Fmt|null), used(Fmt), limit(Fmt), pct, tone, tip }
 * 线路机：traffic.quota（上限 + 已用，按账期重置）；主机：网卡累计较大者；代理：入口本月业务流量。
 */
/** 十进制 GB（供应商配额口径）：2000000000000 → “2,000 GB”；悬停写精确字节。 */
function decGB(v) {
  if (v == null) return fmt.bytes(v);
  const g = v / 1e9;
  const t = g >= 100 ? Math.round(g).toLocaleString('en-US') : g >= 10 ? g.toFixed(1) : g.toFixed(2);
  return new fmt.Fmt(`${t} GB`, { num: t, unit: 'GB', raw: v, exact: `${Math.round(v).toLocaleString('en-US')} B（1 GB = 10⁹ B）` });
}
export function quotaModel(n) {
  const q = n.traffic?.quota;
  if (q?.limit_bytes) {
    const used = q.used_bytes ?? 0;
    const pct = (used / q.limit_bytes) * 100;
    // 套餐目录的配额按十进制 GB 计（供应商口径：1 GB = 10^9 B）：上限、已用、剩余都写 GB，与供应商面板读数一致
    const B = q.plan ? decGB : fmt.bytes;
    return {
      rem: B(Math.max(0, q.limit_bytes - used)), used: B(used), limit: B(q.limit_bytes), pct,
      tone: pct >= 100 ? 'bad' : pct >= 90 ? 'warn' : null,
      tip: `${q.type_word ?? ''} · ${q.used_source ?? ''} · 已用 ${fmt.pctResource(pct, { precise: true }).text}`,
    };
  }
  const limit = quotaLimit(n);
  if (n.hasHost) {
    const h = n.host;
    const used = Math.max(h.net_total_rx ?? 0, h.net_total_tx ?? 0);
    return { rem: null, used: fmt.bytes(used), limit, pct: 0, tone: null, tip: '已用取网卡接收、发送累计中较大者（自主机启动起）' };
  }
  const m = n.traffic?.month;
  const used = m ? (m.down ?? 0) + (m.up ?? 0) : null;
  return { rem: null, used: fmt.bytes(used), limit, pct: 0, tone: null, tip: '已用取入口本月业务流量（↓ + ↑，北京时间 1 日起）' };
}
