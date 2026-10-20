// 访问监控的 mock：几个节点、几十个域名 / IP，按窗口缩放连接数。数据在模块加载时按种子生成，刷新页面保持稳定。
import type { AccessEvent, AccessNode, AccessSource, AccessTarget, MachineAccess, NewTarget } from '@/lib/access';
import type { MachineView } from '@/lib/types';
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
    const sources = Array.from({ length: 3 + Math.floor(r() * 6) }, (_, i) => ({ ip: `198.51.100.${10 + Math.floor(r() * 200)}`, w: Math.pow(r(), 1.6) + 0.05 + i * 0 }));
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
  const win = (q: URLSearchParams) => new Date(Date.now() - (SECONDS[q.get('window') ?? '24h'] ?? 86400) * 1000).toISOString();

  R('GET', '/access/summary', ({ query }) => {
    const rows = stats(machines(), query).filter((x) => x.targets.length > 0);
    const nodes: AccessNode[] = rows.map(({ n, targets }) => ({
      node_id: n.m.id, name: n.m.name, team: n.m.team?.name ?? null, conns: targets.reduce((s, t) => s + t.conns, 0), targets: targets.length,
      domains: targets.filter((t) => t.kind === 'domain').length, sources: n.sources.length, blocked: targets.reduce((s, t) => s + t.blocked, 0),
      last_ts: targets.map((t) => t.last_seen!).sort().pop() ?? null,
    })).sort((a, b) => b.conns - a.conns);
    return { kpi: { active_nodes: nodes.length, conns: nodes.reduce((s, x) => s + x.conns, 0), targets: nodes.reduce((s, x) => s + x.targets, 0), blocked: nodes.reduce((s, x) => s + x.blocked, 0) }, nodes, window_start: win(query) };
  });

  R('GET', '/access/targets', ({ query }) => {
    const all = stats(machines(), query).flatMap((x) => x.targets);
    const sort = query.get('sort') ?? 'conns';
    all.sort((a, b) => (sort === 'last' ? b.last_seen!.localeCompare(a.last_seen!) : sort === 'first' ? b.first_seen!.localeCompare(a.first_seen!) : b.conns - a.conns));
    const limit = Math.min(500, +(query.get('limit') ?? 100)); const offset = +(query.get('offset') ?? 0);
    const items = all.slice(offset, offset + limit).map(({ ...t }) => { delete (t as { base?: number }).base; return t; });
    return { items, next_offset: offset + limit < all.length ? offset + limit : null, window_start: win(query) };
  });

  R('GET', '/access/new-targets', ({ query }) => {
    const from = Date.now() - (SECONDS[query.get('window') ?? '24h'] ?? 86400) * 1000;
    const items: NewTarget[] = stats(machines(), query).flatMap((x) => x.targets).filter((t) => new Date(t.first_seen!).getTime() >= from)
      .sort((a, b) => b.first_seen!.localeCompare(a.first_seen!)).map((t) => ({ node_id: t.node_id, name: t.name, dest_host: t.dest_host, kind: t.kind, first_seen: t.first_seen!, last_seen: t.last_seen!, total: t.conns }));
    return { items: items.slice(0, +(query.get('limit') ?? 200)), next_offset: null };
  });

  R('GET', '/access/sources', ({ query }) => {
    const search = (query.get('search') ?? '').trim();
    const items: AccessSource[] = stats(machines(), query).filter((x) => x.targets.length > 0).flatMap(({ n, targets }) => {
      const total = targets.reduce((s, t) => s + t.conns, 0);
      return n.sources.filter((s) => !search || like(s.ip, search)).map((s) => ({ node_id: n.m.id, name: n.m.name, src_ip: s.ip, conns: Math.round(total * s.w), last_ts: new Date(Date.now() - Math.random() * 600_000).toISOString() }));
    }).sort((a, b) => b.conns - a.conns).slice(0, Math.min(500, +(query.get('limit') ?? 100)));
    return { items };
  });

  R('GET', '/access/events', ({ query }) => {
    cache ??= build(machines());
    const node = query.get('node') ?? ''; const team = query.get('team') ?? ''; const search = (query.get('search') ?? '').trim(); const action = query.get('action') ?? ''; const kind = query.get('kind') ?? '';
    const before = query.get('before');
    const limit = Math.min(500, +(query.get('limit') ?? 100));
    const all = cache.filter((n) => (!node || n.m.id === node) && (!team || n.m.team?.id === team)).flatMap((n) => n.events)
      .filter((e) => (!before || e.ts < before) && (!search || like(e.dest_host, search) || like(e.src_ip ?? '', search)) && (!action || (action === 'blocked') === e.blocked) && (!kind || (isIp(e.dest_host) ? 'ip' : 'domain') === kind))
      .sort((a, b) => b.ts.localeCompare(a.ts));
    const items = all.slice(0, limit);
    return { items, next_before: items.length === limit && all.length > limit ? items[items.length - 1].ts : null };
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
    const top_sources: AccessSource[] = rows.flatMap(({ n }) => n.sources.map((s) => ({ node_id: n.m.id, name: n.m.name, src_ip: s.ip, conns: Math.round(total * s.w / Math.max(1, rows.length)), last_ts: new Date(Date.now() - 90_000).toISOString() }))).sort((a, b) => b.conns - a.conns).slice(0, 20);
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
}
