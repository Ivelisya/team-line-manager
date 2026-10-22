// 访问监控的 mock：几个节点、几十个域名 / IP，按窗口缩放连接数。数据在模块加载时按种子生成，刷新页面保持稳定。
import type { AccessEvent, AccessNode, AccessSource, AccessTarget, AccessTargetGroup, MachineAccess, NewTarget } from '@/lib/access';
import type { MachineView } from '@/lib/types';
import type { DistItem, IpIntel, IpIntelStatus, KnownItem, SourcesProfile } from '@/lib/ipintel';
import { hashStr, rng } from './fixtures';

type Ctx = { params: string[]; query: URLSearchParams; body: unknown };
type Reg = (method: string, path: string, h: (c: Ctx) => unknown) => void;

const HOUR = 3600_000;
const DAY = 24 * HOUR;
const SECONDS: Record<string, number> = { '1h': 3600, '24h': 86400, '7d': 604800, '30d': 2592000 };
const DOMAINS = [
  'www.google.com', 'www.youtube.com', 'i.ytimg.com', 'api.openai.com', 'chat.openai.com', 'github.com', 'raw.githubusercontent.com', 'api.github.com',
  'www.facebook.com', 'graph.facebook.com', 'twitter.com', 'abs.twimg.com', 'www.netflix.com', 'occ-0-1.1.nflxso.net', 'discord.com', 'cdn.discordapp.com',
  'telegram.org', 'web.telegram.org', 'www.reddit.com', 'i.redd.it', 'en.wikipedia.org', 'upload.wikimedia.org', 'www.amazon.com', 'm.media-amazon.com',
  'www.tiktok.com', 'mon.musical.ly', 'www.instagram.com', 'scontent.cdninstagram.com', 'docs.google.com', 'mail.google.com', 'www.gstatic.com', 'fonts.googleapis.com',
  'registry.npmjs.org', 'pypi.org', 'files.pythonhosted.org', 'hub.docker.com', 'ghcr.io', 'www.cloudflare.com', 'one.one.one.one', 'dns.google',
  'ads.doubleclick.net', 'googleads.g.doubleclick.net', 'analytics.tiktok.com', 'tracker.example-ads.net', 'telemetry.example.io',
  '*.cloudfront.net', '*.akamaized.net', '*.fastly.net', '*.googlevideo.com',
  '1.1.1.1', '8.8.8.8', '142.250.72.14', '104.16.132.229', '151.101.1.69',
];
const BLOCKED = new Set(['ads.doubleclick.net', 'googleads.g.doubleclick.net', 'analytics.tiktok.com', 'tracker.example-ads.net', 'telemetry.example.io']);
const isIp = (h: string) => /^[0-9.]+$/.test(h);

interface NodeData { m: MachineView; targets: (AccessTarget & { base: number })[]; sources: { ip: string; w: number }[]; events: AccessEvent[] }
let cache: NodeData[] | null = null;

function build(machines: MachineView[]): NodeData[] {
  const now = Date.now();
  const nodes = machines.filter((m) => m.kind !== 'relay' && m.lifecycle === 'active').slice(0, 8);
  return nodes.map((m) => {
    const r = rng(hashStr(`access:${m.id}`));
    const pool = [...DOMAINS].sort(() => r() - 0.5).slice(0, 22 + Math.floor(r() * 18));
    const targets = pool.map((h) => {
      const base = Math.max(1, Math.round(Math.pow(r(), 3) * 24)); // 每小时连接数
      const age = r() < 0.18 ? r() * 20 * HOUR : (1 + r() * 25) * DAY; // 少数是最近才出现的
      return {
        node_id: m.id, name: m.name, dest_host: h, kind: (isIp(h) ? 'ip' : 'domain') as 'ip' | 'domain', conns: 0, sources: 0, blocked: 0, base,
        first_seen: new Date(now - age).toISOString(), last_seen: new Date(now - r() * 40 * 60_000).toISOString(),
      };
    });
    const sources = Array.from({ length: 3 + Math.floor(r() * 6) }, (_, i) => ({ ip: `198.51.100.${10 + Math.floor(r() * 200)}`, w: Math.pow(r(), 1.6) + 0.05 + i * 0 })).filter((s, i, all) => all.findIndex((x) => x.ip === s.ip) === i);
    const wsum = sources.reduce((s, x) => s + x.w, 0);
    sources.forEach((s) => { s.w /= wsum; });
    const events: AccessEvent[] = [];
    let t = now - 3000;
    for (let i = 0; i < 160; i++) {
      t -= 800 + r() * 14_000;
      const tg = targets[Math.floor(Math.pow(r(), 1.7) * targets.length)];
      events.push({ ts: new Date(t).toISOString(), node_id: m.id, name: m.name, src_ip: sources[Math.floor(r() * sources.length)].ip, dest_host: tg.dest_host.replace('*.', `${Math.floor(r() * 1e8).toString(16)}.`), dest_port: isIp(tg.dest_host) ? 53 : 443, network: r() < 0.1 ? 'udp' : 'tcp', blocked: BLOCKED.has(tg.dest_host) });
    }
    return { m, targets, sources, events };
  });
}

const like = (s: string, q: string) => s.toLowerCase().includes(q.toLowerCase());

/** 按筛选 + 窗口算出每个节点的目标统计 */
function stats(machines: MachineView[], q: URLSearchParams) {
  cache ??= build(machines);
  const secs = SECONDS[q.get('window') ?? '24h'] ?? 86400;
  const scale = secs / 3600;
  const team = q.get('team') ?? ''; const node = q.get('node') ?? ''; const search = (q.get('search') ?? '').trim();
  const kind = q.get('kind') ?? ''; const action = q.get('action') ?? '';
  const windowStart = Date.now() - secs * 1000;
  return cache.filter((n) => (!team || n.m.team?.id === team) && (!node || n.m.id === node)).map((n) => {
    const targets: AccessTarget[] = [];
    for (const t of n.targets) {
      if (search && !like(t.dest_host, search)) continue;
      if (kind && t.kind !== kind) continue;
      const total = Math.max(1, Math.round(t.base * Math.min(scale, 1 + (Date.now() - new Date(t.first_seen ?? 0).getTime()) / HOUR)));
      const blocked = BLOCKED.has(t.dest_host) ? total : 0;
      const conns = action === 'blocked' ? blocked : action === 'routed' ? total - blocked : total;
      if (conns <= 0 || (action === 'blocked' && !blocked)) continue;
      targets.push({ ...t, conns, blocked: action === 'routed' ? 0 : blocked, sources: Math.min(n.sources.length, 1 + Math.floor(conns / 25)) });
    }
    return { n, targets, windowStart };
  });
}

export function installAccessMock(R: Reg, machines: () => MachineView[]) {
  // 访问记录保留期（设置「数据保留」）
  const retention = { raw_days: 7, hourly_days: 31, min: 1, max: 90, profile_days: 60 };
  R('GET', '/settings/access-retention', () => ({ ...retention }));
  R('PUT', '/settings/access-retention', ({ body }) => {
    const b = body as { raw_days: number; hourly_days: number };
    Object.assign(retention, { raw_days: b.raw_days, hourly_days: b.hourly_days });
    return { ...retention };
  });
  const win = (q: URLSearchParams) => new Date(Date.now() - (SECONDS[q.get('window') ?? '24h'] ?? 86400) * 1000).toISOString();

  R('GET', '/access/summary', ({ query }) => {
    const rows = stats(machines(), query).filter((x) => x.targets.length > 0);
    // 同后端：来源数只在原始记录里算得出；超过 1 小时又带搜索 / 类型 / 结果筛选时给 null
    const noSources = (query.get('window') ?? '24h') !== '1h' && !!((query.get('search') ?? '').trim() || query.get('kind') || query.get('action'));
    const nodes: AccessNode[] = rows.map(({ n, targets }) => ({
      node_id: n.m.id, name: n.m.name, team: n.m.team?.name ?? null, conns: targets.reduce((s, t) => s + t.conns, 0), targets: targets.length,
      domains: targets.filter((t) => t.kind === 'domain').length, sources: noSources ? null : n.sources.length, blocked: targets.reduce((s, t) => s + t.blocked, 0),
      last_ts: targets.map((t) => t.last_seen!).sort().pop() ?? null,
    })).sort((a, b) => b.conns - a.conns);
    return { kpi: { active_nodes: nodes.length, conns: nodes.reduce((s, x) => s + x.conns, 0), targets: nodes.reduce((s, x) => s + x.targets, 0), unique_targets: new Set(rows.flatMap((x) => x.targets.map((t) => t.dest_host))).size, blocked: nodes.reduce((s, x) => s + x.blocked, 0) }, nodes, window_start: win(query) };
  });

  R('GET', '/access/targets', ({ query }) => {
    const target = query.get('target') ?? '';
    const all = stats(machines(), query).flatMap((x) => x.targets).filter((t) => !target || t.dest_host === target);
    const sort = query.get('sort') ?? 'conns';
    const bySort = <T extends { conns: number; first_seen: string | null; last_seen: string | null }>(a: T, b: T) =>
      (sort === 'last' ? b.last_seen!.localeCompare(a.last_seen!) : sort === 'first' ? b.first_seen!.localeCompare(a.first_seen!) : b.conns - a.conns);
    const limit = Math.min(500, +(query.get('limit') ?? 100)); const offset = +(query.get('offset') ?? 0);
    if (query.get('group') === 'target') {
      // 同中控：按目标合并各节点，带总数
      const m = new Map<string, AccessTargetGroup>();
      for (const t of all) {
        const g = m.get(t.dest_host);
        if (!g) { m.set(t.dest_host, { dest_host: t.dest_host, kind: t.kind, nodes: 1, conns: t.conns, blocked: t.blocked, first_seen: t.first_seen, last_seen: t.last_seen }); continue; }
        g.nodes += 1; g.conns += t.conns; g.blocked += t.blocked;
        if (t.first_seen! < g.first_seen!) g.first_seen = t.first_seen;
        if (t.last_seen! > g.last_seen!) g.last_seen = t.last_seen;
      }
      const groups = [...m.values()].sort(bySort);
      const end = Math.min(groups.length, offset + limit);
      return { items: groups.slice(offset, end), total: groups.length, next_offset: end < groups.length ? end : null, window_start: win(query) };
    }
    all.sort(bySort);
    const items = all.slice(offset, offset + limit).map(({ ...t }) => { delete (t as { base?: number }).base; return t; });
    return { items, next_offset: offset + limit < all.length ? offset + limit : null, window_start: win(query) };
  });

  R('GET', '/access/new-targets', ({ query }) => {
    const from = Date.now() - (SECONDS[query.get('window') ?? '24h'] ?? 86400) * 1000;
    const items: NewTarget[] = stats(machines(), query).flatMap((x) => x.targets).filter((t) => new Date(t.first_seen!).getTime() >= from)
      .sort((a, b) => b.first_seen!.localeCompare(a.first_seen!)).map((t) => ({ node_id: t.node_id, name: t.name, dest_host: t.dest_host, kind: t.kind, first_seen: t.first_seen!, last_seen: t.last_seen!, total: t.conns }));
    const limit = +(query.get('limit') ?? 200); const offset = +(query.get('offset') ?? 0);
    const end = Math.min(items.length, offset + limit);
    return { items: items.slice(offset, end), total: items.length, next_offset: end < items.length ? end : null };
  });

  R('GET', '/access/sources', ({ query }) => {
    const search = (query.get('search') ?? '').trim();
    const items: AccessSource[] = stats(machines(), query).filter((x) => x.targets.length > 0).flatMap(({ n, targets }) => {
      const total = targets.reduce((s, t) => s + t.conns, 0);
      return n.sources.filter((s) => !search || like(s.ip, search)).map((s) => ({ node_id: n.m.id, name: n.m.name, src_ip: s.ip, conns: Math.round(total * s.w), last_ts: new Date(Date.now() - Math.random() * 600_000).toISOString(), intel: intelOf(s.ip) }));
    }).sort((a, b) => b.conns - a.conns).slice(0, Math.min(500, +(query.get('limit') ?? 100)));
    return { items };
  });

  R('GET', '/access/events', ({ query }) => {
    cache ??= build(machines());
    const node = query.get('node') ?? ''; const team = query.get('team') ?? ''; const search = (query.get('search') ?? '').trim(); const action = query.get('action') ?? ''; const kind = query.get('kind') ?? '';
    // before 是不透明游标（同服务端：前端原样传回 next_before）；mock 里是 base64 的 [时间, 同一时间已翻过的条数]
    const [bts, bskip] = query.get('before') ? JSON.parse(atob(query.get('before')!)) as [string, number] : ['', 0];
    const limit = Math.min(500, +(query.get('limit') ?? 100));
    const all = cache.filter((n) => (!node || n.m.id === node) && (!team || n.m.team?.id === team)).flatMap((n) => n.events)
      .filter((e) => (!bts || e.ts <= bts) && (!search || like(e.dest_host, search) || like(e.src_ip ?? '', search)) && (!action || (action === 'blocked') === e.blocked) && (!kind || (isIp(e.dest_host) ? 'ip' : 'domain') === kind))
      .sort((a, b) => b.ts.localeCompare(a.ts));
    const rest = all.slice(bskip); // 等于 bts 的排在最前，跳过已翻过的
    const items = rest.slice(0, limit);
    const last = items[items.length - 1];
    const skip = last ? items.filter((e) => e.ts === last.ts).length + (last.ts === bts ? bskip : 0) : 0;
    return { items, next_before: items.length === limit && rest.length > limit ? btoa(JSON.stringify([last.ts, skip])) : null };
  });

  R('GET', '/machines/([^/]+)/access', ({ params, query }) => {
    cache ??= build(machines());
    const id = params[0];
    const m = machines().find((x) => x.id === id);
    const ids = !m ? [] : m.kind === 'relay' ? cache.filter((n) => n.m.entries.some((e) => e.relay.id === id)).map((n) => n.m.id) : [id];
    const q = new URLSearchParams(query); q.delete('node');
    const rows = stats(machines(), q).filter((x) => ids.includes(x.n.m.id));
    const targets = rows.flatMap((x) => x.targets);
    const merged = new Map<string, MachineAccess['top_domains'][number]>();
    for (const t of targets) { const c = merged.get(t.dest_host); if (c) { c.conns += t.conns; c.blocked += t.blocked; } else merged.set(t.dest_host, { dest_host: t.dest_host, kind: t.kind, conns: t.conns, blocked: t.blocked, sources: t.sources }); }
    const top_domains = [...merged.values()].sort((a, b) => b.conns - a.conns).slice(0, 30);
    const total = targets.reduce((s, t) => s + t.conns, 0);
    const top_sources: AccessSource[] = rows.flatMap(({ n }) => n.sources.map((s) => ({ node_id: n.m.id, name: n.m.name, src_ip: s.ip, conns: Math.round(total * s.w / Math.max(1, rows.length)), last_ts: new Date(Date.now() - 90_000).toISOString(), intel: intelOf(s.ip) }))).sort((a, b) => b.conns - a.conns).slice(0, 20);
    const recent = rows.flatMap(({ n }) => n.events).sort((a, b) => b.ts.localeCompare(a.ts)).slice(0, 20);
    const r = rng(hashStr(`series:${id}`));
    const w = query.get('window') ?? '24h';
    const hours = w === '7d' ? 168 : w === '30d' ? 720 : 25;
    const perHour = total / Math.max(1, (SECONDS[w] ?? 86400) / 3600);
    const top = Math.floor(Date.now() / HOUR) * HOUR;
    const hourly = Array.from({ length: hours }, (_, i) => {
      const t = top - (hours - 1 - i) * HOUR;
      const diurnal = 0.55 + 0.45 * Math.sin(((new Date(t).getHours() - 8) / 24) * Math.PI * 2);
      const conns = ids.length ? Math.round(perHour * (0.4 + r() * 0.9) * (0.5 + diurnal)) : 0;
      return { hour: new Date(t).toISOString(), conns, blocked: Math.round(conns * 0.04 * r()) };
    });
    return { window_start: win(query), top_domains, top_sources, recent, hourly };
  });

  // ── 来源 IP 画像 ──
  R('GET', '/ipintel/status', () => IP_STATUS);

  R('GET', '/machines/([^/]+)/sources', ({ params, query }) => {
    cache ??= build(machines());
    const id = params[0];
    const m = machines().find((x) => x.id === id);
    const ids = !m ? [] : m.kind === 'relay' ? cache.filter((n) => n.m.entries.some((e) => e.relay.id === id)).map((n) => n.m.id) : [id];
    const q = new URLSearchParams(); q.set('window', query.get('window') ?? '24h');
    const rows = stats(machines(), q).filter((x) => ids.includes(x.n.m.id));
    const limit = Math.min(500, +(query.get('limit') ?? 50));
    const acc = { isp: new Map<string, Acc>(), city: new Map<string, Acc>(), province: new Map<string, Acc>(), country: new Map<string, Acc>(), conn_type: new Map<string, Acc>() };
    const tops: SourcesProfile['top_sources'] = [];
    let totalConns = 0; let totalSources = 0;
    for (const { n, targets } of rows) {
      const total = targets.reduce((s, t) => s + t.conns, 0);
      for (const s of n.sources) {
        const conns = Math.round(total * s.w);
        const intel = intelOf(s.ip);
        tops.push({ src_ip: s.ip, conns, last_ts: new Date(Date.now() - 90_000).toISOString(), intel });
        if (!intel || conns <= 0) continue;
        totalConns += conns; totalSources += 1;
        bump(acc.isp, intel.isp, intel.isp, conns); bump(acc.city, `${intel.country}|${intel.province ?? ''}|${intel.city ?? ''}`, intel.place, conns);
        if (intel.province) bump(acc.province, `${intel.country}|${intel.province}`, intel.province, conns);
        if (intel.country) bump(acc.country, intel.country, intel.country, conns);
        bump(acc.conn_type, intel.conn_type, intel.conn_label, conns);
      }
    }
    const dist = (a: Map<string, Acc>): DistItem[] => [...a.entries()].map(([value, x]) => ({ value, label: x.label, conns: x.conns, sources: x.sources, pct: totalConns ? Math.round((x.conns / totalConns) * 1000) / 10 : 0 })).sort((p, q2) => q2.conns - p.conns);
    const distribution = { isp: dist(acc.isp), city: dist(acc.city), province: dist(acc.province), country: dist(acc.country), conn_type: dist(acc.conn_type) };
    // 学习期：每 4 个节点里有 1 个还在学习
    const learning = rows.some(({ n }) => hashStr(n.m.id) % 4 === 0)
      ? { started_at: new Date(Date.now() - 2 * DAY - 5 * HOUR).toISOString(), ends_at: new Date(Date.now() + 4 * DAY + 19 * HOUR).toISOString(), learn_days: 7, done: false, remaining_hours: 4 * 24 + 19 }
      : { started_at: new Date(Date.now() - 12 * DAY).toISOString(), ends_at: new Date(Date.now() - 5 * DAY).toISOString(), learn_days: 7, done: true, remaining_hours: 0 };
    const inLearning = !learning.done;
    const known = (d: DistItem[]): KnownItem[] => d.map((x, i) => ({
      value: x.value, label: x.label, first_seen: new Date(Date.now() - (inLearning ? 1 + i * 0.2 : 12 - i * 0.3) * DAY).toISOString(),
      last_seen: new Date(Date.now() - i * 2 * HOUR).toISOString(), conns: x.conns, anomaly: !inLearning && i === d.length - 1 && d.length > 3,
    }));
    const kIsp = known(distribution.isp);
    const bad = kIsp.find((k) => k.anomaly);
    return {
      window: query.get('window') ?? '24h', window_start: win(query), empty: rows.length === 0 || tops.length === 0, degraded: false,
      total_conns: totalConns, total_sources: totalSources, internal_conns: 0, private_conns: 0, unresolved_sources: tops.filter((t) => !t.intel).length,
      distribution, top_sources: tops.sort((a, b) => b.conns - a.conns).slice(0, limit),
      known: { isp: kIsp, city: known(distribution.city), conn_type: known(distribution.conn_type) },
      learning,
      anomalies: bad ? [{ id: `mock-${id}`, rule: 'ipintel_new_isp', rule_name: '来源出现新运营商', severity: 'warning', summary: `${m?.name ?? '节点'}：出现新的来源运营商：${bad.label}（${bad.conns} 次连接）`, opened_at: new Date(Date.now() - 3 * HOUR).toISOString() }] : [],
    } satisfies SourcesProfile;
  });
}

// ── 来源 IP 画像的假数据 ──
type Acc = { label: string; conns: number; sources: number };
function bump(m: Map<string, Acc>, key: string, label: string, conns: number) {
  const x = m.get(key) ?? { label, conns: 0, sources: 0 };
  x.conns += conns; x.sources += 1; m.set(key, x);
}
const CONN_LABEL = { residential: '住宅宽带', mobile: '移动网络', datacenter: '机房', vpn: 'VPN / 代理', unknown: '未知' } as const;
function prof(isp: string, country: string, province: string | null, city: string | null, type: IpIntel['conn_type'], asn: number, org: string, confidence: IpIntel['confidence'], reason: string): IpIntel {
  const place = country === 'CN' ? (province === city ? (city ?? '') : `${province ?? ''}${city ?? ''}`) : `${country} · ${city ?? province ?? ''}`;
  return { isp, asn, as_org: org, country, province, city, place, conn_type: type, conn_label: CONN_LABEL[type], confidence, reason, internal: null, private: false };
}
const PROFILES: IpIntel[] = [
  prof('中国电信', 'CN', '广东省', '广州市', 'residential', 4134, 'CHINANET-BACKBONE', 'low', 'AS4134（中国电信）同时承载家宽和手机流量，离线库没有接入方式，按住宅宽带计'),
  prof('中国电信', 'CN', '上海市', '上海市', 'residential', 4812, 'China Telecom (Group)', 'low', 'AS4812（中国电信）同时承载家宽和手机流量，离线库没有接入方式，按住宅宽带计'),
  prof('中国联通', 'CN', '河南省', '郑州市', 'residential', 4837, 'CHINA UNICOM China169 Backbone', 'low', 'AS4837（中国联通）同时承载家宽和手机流量，离线库没有接入方式，按住宅宽带计'),
  prof('中国联通', 'CN', '北京市', '北京市', 'residential', 4808, 'China Unicom Beijing Province Network', 'low', 'AS4808（中国联通）同时承载家宽和手机流量，离线库没有接入方式，按住宅宽带计'),
  prof('中国移动', 'CN', '浙江省', '杭州市', 'mobile', 56041, 'China Mobile communications corporation', 'high', 'AS56041 是移动运营商 中国移动 的蜂窝网络'),
  prof('中国移动', 'CN', '江苏省', '南京市', 'mobile', 56046, 'China Mobile communications corporation', 'high', 'AS56046 是移动运营商 中国移动 的蜂窝网络'),
  prof('阿里云', 'CN', '浙江省', '杭州市', 'datacenter', 37963, 'Hangzhou Alibaba Advertising Co.,Ltd.', 'high', 'AS37963 属于云厂商 阿里云'),
  prof('M247', 'US', 'Texas', 'Dallas', 'vpn', 9009, 'M247 Europe SRL', 'high', 'AS9009 M247（大量商业 VPN 出口）'),
  prof('Comcast', 'US', 'Washington', 'Seattle', 'residential', 7922, 'COMCAST-7922', 'medium', 'AS7922 是固网运营商（Comcast），不在机房名单'),
  prof('Hetzner', 'DE', 'Bavaria', 'Nuremberg', 'datacenter', 24940, 'Hetzner Online GmbH', 'high', 'AS24940 属于云厂商 Hetzner'),
  prof('未知', 'JP', 'Tokyo', 'Tokyo', 'unknown', 64496, 'EXAMPLE-NET', 'low', 'AS64496 不在任何分类名单'),
];
/** 同一个 IP 永远得到同一个画像；每 11 个里有 1 个「查不到」 */
const intelOf = (ip: string): IpIntel | null => {
  const h = hashStr(`intel:${ip}`);
  return h % 11 === 0 ? null : PROFILES[h % PROFILES.length];
};

const IP_STATUS: IpIntelStatus = {
  enabled: true, installed: true, loaded: 5, expected: 5, db_dir: '/var/lib/fleet/ipintel', version: 'mock', cache_rows: 1284, hint: null, errors: [],
  files: [
    // 同中控：xdb、DB-IP 的 build 是构建时刻（Unix 秒），iptoasn 的 build 是行数；mtime 是文件修改时间（Unix 秒）
    { id: 'ip2region_v4', loaded: true, bytes: 11_000_000, mtime: 1791943867, build: '1791943867' }, { id: 'ip2region_v6', loaded: true, bytes: 38_000_000, mtime: 1791943867, build: '1791943867' },
    { id: 'dbip_city', loaded: true, bytes: 127_000_000, mtime: 1792500000, build: '1792454400' }, { id: 'dbip_asn', loaded: true, bytes: 9_500_000, mtime: 1792500000, build: '1792454400' },
    { id: 'iptoasn', loaded: true, bytes: 46_000_000, mtime: 1792520000, build: '620000 行', entries: 620_000 },
  ],
};

