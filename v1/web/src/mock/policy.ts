// 访问规则的 mock：草稿 / 预览 / 发布 / 回滚都在内存里走一遍，规则与服务端一致（有效策略 = 节点 > 团队 > 默认）。
// 首次访问时放几份演示数据：团队1 黑名单（带一份未发布的草稿）、团队2 白名单、一个暂停的节点，以及三个历史版本。
import type { MachineView, Team } from '@/lib/types';
import type { PolicyBody, PolicyChange, PolicyList, PolicyMode, PolicyPreview, PolicyVersion, PreviewNode, ScopePolicy, VersionStatus } from '@/lib/policy';
import { parseDomainText } from '@/lib/policyDomains';
import { hashStr, rng } from './fixtures';

type Ctx = { params: string[]; query: URLSearchParams; body: any }; // eslint-disable-line @typescript-eslint/no-explicit-any
type Reg = (method: string, path: string, h: (c: Ctx) => unknown) => void;
interface Env {
  teams: () => Team[]; machines: () => MachineView[]; emit: (e: { type: string; id?: string }) => void; user: () => string;
  bad: (msg: string, status?: number, code?: string) => Error;
}

type Scope = 'team' | 'node';
interface Row { scope: Scope; id: string; cur: PolicyBody; draft: PolicyBody | null; revision: number; updated_by: string | null; updated_at: string | null }
interface Ver { id: string; seq: number; kind: 'publish' | 'rollback'; rollback_of: number | null; actor: string; created_at: number; snapshot: Row[]; changes: PolicyChange[]; relays: { id: string; name: string; at: number; fail?: boolean }[] }

const HOUR = 3600_000;
const iso = (t = Date.now()) => new Date(t).toISOString();
const HOSTS = [
  'www.google.com', 'www.youtube.com', 'api.openai.com', 'github.com', 'www.facebook.com', 'twitter.com', 'www.netflix.com', 'discord.com', 'telegram.org', 'www.reddit.com',
  'en.wikipedia.org', 'www.amazon.com', 'www.tiktok.com', 'www.instagram.com', 'registry.npmjs.org', 'ghcr.io', 'www.cloudflare.com', 'ads.doubleclick.net',
  'googleads.g.doubleclick.net', 'analytics.tiktok.com', 'tracker.example-ads.net', 'telemetry.example.io', 'cdn.example-ads.net', 'metrics.example.io', '8.8.8.8', '1.1.1.1', '142.250.72.14',
];

const defaults = (s: Scope): PolicyBody => ({ mode: s === 'node' ? 'inherit' : 'observe', domains: [], note: '' });
const same = (a: PolicyBody, b: PolicyBody) => a.mode === b.mode && a.note === b.note && JSON.stringify(a.domains) === JSON.stringify(b.domains);
const clone = <T,>(x: T): T => JSON.parse(JSON.stringify(x));
const uses = (m: PolicyMode) => m === 'allowlist' || m === 'denylist';

/** 与服务端 policy.rs 的匹配一致：`=x` 只等值，否则等值或子域 */
function listMatches(domains: string[], host: string): boolean {
  return domains.some((p) => {
    const exact = p.startsWith('='); const d = exact ? p.slice(1) : p;
    return host === d || (!exact && host.endsWith(`.${d}`));
  });
}
function wouldBlock(b: { mode: PolicyMode; domains: string[] }, host: string): boolean {
  if (b.mode === 'blocked') return true;
  if (b.mode === 'allowlist') return !listMatches(b.domains, host);
  if (b.mode === 'denylist') return listMatches(b.domains, host);
  return false;
}

export function installPolicyMock(R: Reg, env: Env) {
  const rows = new Map<string, Row>();
  const versions: Ver[] = [];
  let seq = 0; let seeded = false;
  const key = (s: Scope, id: string) => `${s}:${id}`;

  const nodes = () => env.machines().filter((m) => m.kind !== 'relay' && m.lifecycle !== 'retired' && m.team).sort((a, b) => (a.number ?? 0) - (b.number ?? 0));
  const relays = () => env.machines().filter((m) => m.kind === 'relay' && m.lifecycle === 'active');
  const nameOf = (s: Scope, id: string) => (s === 'team' ? env.teams().find((t) => t.id === id)?.name : env.machines().find((m) => m.id === id)?.name) ?? id;

  const get = (s: Scope, id: string): Row => rows.get(key(s, id)) ?? { scope: s, id, cur: defaults(s), draft: null, revision: 0, updated_by: null, updated_at: null };
  const put = (r: Row) => { rows.set(key(r.scope, r.id), r); return r; };

  /** 有效策略：节点（非 inherit）> 团队 > 默认 */
  function effective(node: MachineView, useDraft: boolean) {
    const pick = (r: Row) => (useDraft ? r.draft ?? r.cur : r.cur);
    const n = pick(get('node', node.id));
    if (n.mode !== 'inherit') return { ...n, mode: n.mode as Exclude<PolicyMode, 'inherit'>, source: 'node' as const };
    const t = node.team ? pick(get('team', node.team.id)) : null;
    if (t && t.mode !== 'inherit') return { ...t, mode: t.mode as Exclude<PolicyMode, 'inherit'>, source: 'team' as const };
    return { mode: 'observe' as const, domains: [], note: '', source: 'default' as const };
  }
  const compiled = (e: { mode: PolicyMode; domains: string[] }) => (e.mode === 'observe' || e.mode === 'inherit' ? '' : e.mode === 'blocked' ? 'blocked' : `${e.mode}:${e.domains.join(',')}`);

  function view(r: Row): ScopePolicy {
    return { scope: r.scope, id: r.id, ...clone(r.cur), draft: r.draft ? clone(r.draft) : null, revision: r.revision, updated_by: r.updated_by, updated_at: r.updated_at };
  }
  const drafts = () => [...rows.values()].filter((r) => r.draft).sort((a, b) => key(a.scope, a.id).localeCompare(key(b.scope, b.id)));
  const draftHash = () => (drafts().length ? hashStr(JSON.stringify(drafts().map((r) => [r.scope, r.id, r.draft]))).toString(16).padStart(8, '0').repeat(8) : null);

  /** 把一组策略行换成「生效」后，哪些线路机的规则会变（有节点的编译结果不同）。这里按行快照临时替换来比较 */
  function affected(before: Map<string, Row>, after: Map<string, Row>): MachineView[] {
    const snap = (m: Map<string, Row>) => { const keep = new Map(rows); rows.clear(); m.forEach((v, k) => rows.set(k, v)); const out = new Map<string, string>(); for (const n of nodes()) out.set(n.id, compiled(effective(n, false))); rows.clear(); keep.forEach((v, k) => rows.set(k, v)); return out; };
    const a = snap(before); const b = snap(after);
    const ids = new Set<string>();
    for (const n of nodes()) if (a.get(n.id) !== b.get(n.id)) n.entries.forEach((e) => ids.add(e.relay.id));
    return relays().filter((r) => ids.has(r.id));
  }

  function diff(before: Map<string, Row>, after: Map<string, Row>): PolicyChange[] {
    const out: PolicyChange[] = [];
    for (const k of new Set([...before.keys(), ...after.keys()])) {
      const [s, id] = k.split(':') as [Scope, string];
      const a = before.get(k)?.cur ?? defaults(s); const b = after.get(k)?.cur ?? defaults(s);
      if (!same(a, b)) out.push({ scope: s, id, name: nameOf(s, id), from: { mode: a.mode, domains: a.domains.length }, to: { mode: b.mode, domains: b.domains.length } });
    }
    return out;
  }
  const curMap = (): Map<string, Row> => new Map([...rows].map(([k, r]) => [k, { ...clone(r), draft: null }]));

  function record(kind: 'publish' | 'rollback', rollbackOf: number | null, before: Map<string, Row>, at = Date.now(), actor = env.user()): Ver {
    const after = curMap();
    const rel = affected(before, after);
    const v: Ver = {
      id: `pv${++seq}`, seq, kind, rollback_of: rollbackOf, actor, created_at: at, snapshot: [...after.values()].filter((r) => !same(r.cur, defaults(r.scope))),
      changes: diff(before, after), relays: rel.map((r) => ({ id: r.id, name: r.name, at: Date.now() + 3500 + Math.random() * 2500 })),
    };
    versions.unshift(v);
    return v;
  }

  function seed() {
    if (seeded) return; seeded = true;
    const now = Date.now();
    const ts = env.teams();
    const t1 = ts[0]; const t2 = ts[1] ?? ts[0];
    const ns = nodes();
    const n2 = ns.filter((n) => n.team?.id === t2.id);
    const base: PolicyBody = { mode: 'denylist', domains: ['ads.doubleclick.net', 'analytics.tiktok.com', 'googleads.g.doubleclick.net', 'telemetry.example.io', 'tracker.example-ads.net'], note: '广告与遥测域名' };
    const history: [number, () => void, 'publish' | 'rollback', number | null][] = [
      [now - 52 * HOUR, () => { put({ scope: 'team', id: t1.id, cur: base, draft: null, revision: 2, updated_by: 'admin', updated_at: iso(now - 52 * HOUR) }); }, 'publish', null],
      [now - 20 * HOUR, () => {
        put({ scope: 'team', id: t2.id, cur: { mode: 'allowlist', domains: ['github.com', 'api.openai.com', 'registry.npmjs.org', 'ghcr.io', '=hub.docker.com'], note: '只放行研发用到的站点' }, draft: null, revision: 2, updated_by: 'ops', updated_at: iso(now - 20 * HOUR) });
        if (n2[1]) put({ scope: 'node', id: n2[1].id, cur: { mode: 'blocked', domains: [], note: '怀疑被滥用，先暂停' }, draft: null, revision: 2, updated_by: 'ops', updated_at: iso(now - 20 * HOUR) });
      }, 'publish', null],
      [now - 5 * HOUR, () => { if (n2[1]) put({ scope: 'node', id: n2[1].id, cur: defaults('node'), draft: null, revision: 3, updated_by: 'admin', updated_at: iso(now - 5 * HOUR) }); }, 'publish', null],
    ];
    for (const [at, apply, kind, of] of history) {
      const before = curMap(); apply();
      const v = record(kind, of, before, at, at === history[1][0] ? 'ops' : 'admin');
      v.relays.forEach((r) => { r.at = 0; });
    }
    // 一份未发布的草稿：团队1 黑名单加两个域名
    const r1 = get('team', t1.id);
    put({ ...r1, draft: { mode: 'denylist', domains: [...base.domains, 'cdn.example-ads.net', 'metrics.example.io'].sort(), note: '广告与遥测域名；补两个新发现的' }, revision: r1.revision + 1, updated_by: 'admin', updated_at: iso(now - 12 * 60_000) });
    // 并发演示用：团队2-01 想改成黑名单的草稿
    const first2 = n2[0];
    if (first2) put({ ...get('node', first2.id), draft: { mode: 'denylist', domains: ['www.tiktok.com', 'www.instagram.com'], note: '' }, revision: 1, updated_by: 'ops', updated_at: iso(now - 40 * 60_000) });
  }

  R('GET', '/policies', () => {
    seed();
    const list: PolicyList = {
      draft_hash: draftHash(), draft_count: drafts().length,
      teams: [...env.teams()].sort((a, b) => a.sort_order - b.sort_order).map((t) => ({
        id: t.id, name: t.name, policy: view(get('team', t.id)),
        nodes: nodes().filter((n) => n.team?.id === t.id).map((n) => ({
          id: n.id, name: n.name, number: n.number, has_entry: n.entries.length > 0, policy: view(get('node', n.id)),
          effective: effNoCompile(effective(n, false)), draft_effective: effNoCompile(effective(n, true)),
        })),
      })),
    };
    return list;
  });
  const effNoCompile = (e: ReturnType<typeof effective>) => ({ mode: e.mode, domains: [...e.domains], note: e.note, source: e.source });

  R('PUT', '/policies/(team|node)/([^/]+)', ({ params, body }) => {
    seed();
    const s = params[0] as Scope; const id = params[1];
    if (s === 'team' ? !env.teams().some((t) => t.id === id) : !env.machines().some((m) => m.id === id && m.kind !== 'relay')) throw env.bad(s === 'team' ? '没有这个团队' : '没有这台节点', 404, 'not_found');
    const text = Array.isArray(body.domains) ? body.domains.join('\n') : String(body.domains ?? '');
    const p = parseDomainText(text);
    if (p.issues.length) throw env.bad(p.issues[0].error);
    if (p.overall) throw env.bad(p.overall);
    const mode = body.mode as PolicyMode;
    if (!['observe', 'allowlist', 'denylist', 'blocked', 'inherit'].includes(mode)) throw env.bad('模式只能是 observe、allowlist、denylist、blocked、inherit');
    if (mode === 'inherit' && s === 'team') throw env.bad('团队没有可继承的上一级，模式不能是 inherit');
    if (uses(mode) && p.domains.length === 0) throw env.bad('启用白名单或黑名单前必须填写域名');
    const note = String(body.note ?? '').trim();
    if ([...note].length > 500) throw env.bad('备注最多 500 个字符');
    const r = get(s, id);
    if (r.revision !== Number(body.expected_revision)) throw env.bad('草稿已被修改，请重新加载', 409, 'conflict');
    // 同中控：不用域名的模式不留域名
    const next: PolicyBody = { mode, domains: uses(mode) ? p.domains : [], note };
    const saved = put({ ...r, draft: same(next, r.cur) ? null : next, revision: r.revision + 1, updated_by: env.user(), updated_at: iso() });
    env.emit({ type: 'policies' });
    return { policy: view(saved), draft_hash: draftHash(), draft_count: drafts().length };
  });

  R('POST', '/policies/preview', () => {
    seed();
    const ds = drafts();
    const cur = nodes().map((n) => ({ n, c: effective(n, false), d: effective(n, true) }));
    const out: PreviewNode[] = [];
    for (const { n, c, d } of cur) {
      const changed = compiled(c) !== compiled(d);
      if (d.mode === 'observe' && !changed) continue;
      const rr = rng(hashStr(`pv:${n.id}`));
      const hosts = HOSTS.map((h) => ({ host: h, conns: Math.max(1, Math.round(Math.pow(rr(), 3) * 900)) }));
      const total = hosts.reduce((s, x) => s + x.conns, 0);
      const bl = hosts.filter((x) => wouldBlock(d, x.host)).sort((a, b) => b.conns - a.conns);
      out.push({ node_id: n.id, name: n.name, team: n.team?.name ?? null, mode: d.mode, source: d.source, changed, total_conns: total, would_block: bl.reduce((s, x) => s + x.conns, 0), current_blocked: hosts.filter((x) => wouldBlock(c, x.host)).reduce((s, x) => s + x.conns, 0), domains: bl.slice(0, 20) });
    }
    out.sort((a, b) => b.would_block - a.would_block || a.name.localeCompare(b.name, 'zh'));
    const after = new Map(rows); ds.forEach((r) => after.set(key(r.scope, r.id), { ...r, cur: r.draft! }));
    const p: PolicyPreview = {
      draft_hash: draftHash(), draft_count: ds.length, window_hours: 24, affected_relays: affected(curMap(), new Map([...after].map(([k, r]) => [k, { ...r, draft: null }]))).length,
      changes: diff(curMap(), new Map([...after].map(([k, r]) => [k, { ...r, draft: null }]))), nodes: out,
    };
    return p;
  });

  const needHash = (h: unknown) => {
    const cur = draftHash();
    if (!cur) throw env.bad('没有未发布的草稿', 409, 'conflict');
    if (cur !== h) throw env.bad('草稿已变化，请重新预览', 409, 'conflict');
  };

  R('POST', '/policies/publish', ({ body }) => {
    seed(); needHash(body.draft_hash);
    const before = curMap();
    drafts().forEach((r) => put({ ...r, cur: r.draft!, draft: null, revision: r.revision + 1, updated_by: env.user(), updated_at: iso() }));
    const v = record('publish', null, before);
    env.emit({ type: 'policies' });
    return versionView(v);
  });

  R('POST', '/policies/discard', ({ body }) => {
    seed(); needHash(body.draft_hash);
    const ds = drafts();
    ds.forEach((r) => put({ ...r, draft: null, revision: r.revision + 1, updated_by: env.user(), updated_at: iso() }));
    env.emit({ type: 'policies' });
    return { discarded: ds.length };
  });

  function versionView(v: Ver): PolicyVersion & { target: Map<string, Row> } {
    const now = Date.now();
    const rel = v.relays.map((r) => {
      const status: VersionStatus = r.fail ? 'failed' : now >= r.at ? 'applied' : 'applying';
      return { relay_id: r.id, name: r.name, generation: 1, applied_generation: status === 'applied' ? 1 : 0, status, error: status === 'failed' ? 'xray -test 失败：模拟' : null };
    });
    const status: VersionStatus = rel.some((r) => r.status === 'failed') ? 'failed' : rel.some((r) => r.status === 'applying') ? 'applying' : 'applied';
    const curSnap = JSON.stringify([...curMap().values()].filter((r) => !same(r.cur, defaults(r.scope))).map((r) => [r.scope, r.id, r.cur]).sort());
    const isCur = JSON.stringify(v.snapshot.map((r) => [r.scope, r.id, r.cur]).sort()) === curSnap;
    const target = new Map(v.snapshot.map((r) => [key(r.scope, r.id), r]));
    return {
      id: v.id, seq: v.seq, kind: v.kind, rollback_of_seq: v.rollback_of, actor: v.actor, created_at: iso(v.created_at), status, changes: v.changes, relays: rel,
      is_latest: versions[0] === v, is_current: isCur, rollbackable: !isCur, target,
    };
  }

  R('GET', '/policies/versions', () => { seed(); return versions.slice(0, 30).map((v) => { const { target: _t, ...view } = versionView(v); return view; }); });

  R('GET', '/policies/versions/([^/]+)/rollback', ({ params }) => {
    seed();
    const v = versions.find((x) => x.id === params[0]); if (!v) throw env.bad('没有这个版本', 404, 'not_found');
    const { rollbackable, target } = versionView(v);
    return { seq: v.seq, rollbackable, changes: rollbackable ? diff(curMap(), target) : [] };
  });

  R('POST', '/policies/versions/([^/]+)/rollback', ({ params }) => {
    seed();
    const v = versions.find((x) => x.id === params[0]); if (!v) throw env.bad('没有这个版本', 404, 'not_found');
    const view0 = versionView(v);
    if (!view0.rollbackable) throw env.bad(`版本 ${v.seq} 和当前生效的规则一致，不需要回滚`, 409, 'conflict');
    const before = curMap();
    for (const r of [...rows.values()]) {
      const snap = v.snapshot.find((x) => x.scope === r.scope && x.id === r.id);
      put({ ...r, cur: snap ? clone(snap.cur) : defaults(r.scope), revision: r.revision + 1, updated_by: env.user(), updated_at: iso() });
    }
    for (const snap of v.snapshot) if (!rows.has(key(snap.scope, snap.id))) put({ ...snap, draft: null, revision: 1, updated_by: env.user(), updated_at: iso() });
    const nv = record('rollback', v.seq, before);
    env.emit({ type: 'policies' });
    return versionView(nv);
  });
}
