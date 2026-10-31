// 客户端订阅的 mock（同服务端 api/client_subs.rs 的规则）：条目按 mock 机器现算，链接只在新建 / 轮换时返回
import type { ClientSub, ClientSubEntry, SubRule } from '@/lib/clientSubs';
import type { MachineView, Team } from '@/lib/types';

type Ctx = { params: string[]; query: URLSearchParams; body: any }; // eslint-disable-line @typescript-eslint/no-explicit-any
type Reg = (method: string, path: string, h: (c: Ctx) => unknown) => void;
interface Env { teams: () => Team[]; machines: () => MachineView[]; bad: (msg: string, status?: number, code?: string) => Error }

const subs: ClientSub[] = [];
const token = () => Array.from(crypto.getRandomValues(new Uint8Array(32)), (b) => 'ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_'[b & 63]).join('').slice(0, 43);

/** 排除 / 另加里引用了这台机器的订阅名（已排序）。替换预览的警告用（同服务端 names_of_machine） */
export const subNamesOfMachine = (id: string) => subs.filter((s) => s.rule.exclude_ids.includes(id) || s.rule.include_ids.includes(id)).map((s) => s.name).sort();

export function installClientSubsMock(R: Reg, env: Env) {
  const picks = (r: SubRule, m: MachineView, direct: boolean) => {
    if (r.exclude_ids.includes(m.id)) return false;
    if (r.include_ids.includes(m.id)) return true;
    if (direct) return r.direct;
    const listed = !!m.team && r.team_ids.includes(m.team.id);
    return r.team_mode === 'only' ? listed : !m.spare && !(r.team_mode === 'except' && listed);
  };
  const entries = (r: SubRule): ClientSubEntry[] => env.machines().filter((m) => m.lifecycle === 'active').flatMap((m) =>
    m.entries.filter((e) => e.lifecycle === 'active' && e.node.id === m.id).filter(() => picks(r, m, m.kind === 'relay')).map((e) => ({
      name: [m.country, m.kind === 'relay' ? `${m.provider?.name ?? ''} ${m.name} 直出`.trim() : `${m.provider?.name ?? ''} 家宽`.trim(), m.kind === 'relay' ? null : m.name, m.ip, 'VLESS Reality'].filter(Boolean).join(' · '),
      entry_id: e.id, relay: e.relay.name, port: e.port,
      // 同服务端：线路机的 AnyTLS 在跑才有（入口端口 + 20000）
      // 同服务端：线路机 AnyTLS 在跑、且入口协议不是只 Reality 才有；只 AnyTLS 的线路机不出 Reality
      ...((r) => ({ anytls_port: r?.agent?.anytls?.running && r.relay?.protocols !== 'reality' ? e.port + 20000 : null, reality: r?.relay?.protocols !== 'anytls' }))(env.machines().find((x) => x.id === e.relay.id)),
    })));
  const names = (ids: string[]) => ids.map((id) => env.teams().find((t) => t.id === id)?.name ?? env.machines().find((m) => m.id === id)?.name ?? '（已删除）').join('、');
  const text = (r: SubRule) => [
    r.team_mode === 'only' ? `只要 ${names(r.team_ids)}` : r.team_mode === 'except' ? `全部团队（不含备用团队），除了 ${names(r.team_ids)}` : '全部团队（不含备用团队）',
    r.direct ? '含线路机直出' : '不含线路机直出', r.include_ids.length ? `另加 ${names(r.include_ids)}` : '', r.exclude_ids.length ? `排除 ${names(r.exclude_ids)}` : '',
  ].filter(Boolean).join('；');
  const view = (s: ClientSub) => ({ ...s, rule_text: text(s.rule), items: entries(s.rule).length });
  const find = (id: string) => { const s = subs.find((x) => x.id === id); if (!s) throw env.bad('没有这个订阅', 404, 'not_found'); return s; };
  const url = () => `${location.origin}/sub/${token()}`;
  R('GET', '/client-subs', () => subs.map(view));
  R('GET', '/client-subs/([^/]+)', ({ params }) => {
    const s = find(params[0]);
    const es = entries(s.rule).map((e) => ({ ...e, anytls_port: s.protocols === 'reality' ? null : e.anytls_port }));
    return { ...view(s), entries: es.filter((e) => (s.protocols !== 'anytls' && e.reality) || e.anytls_port != null) };
  });
  R('POST', '/client-subs', ({ body }) => {
    const name = String(body.name ?? '').trim();
    if (subs.some((s) => s.name === name)) throw env.bad('已经有同名的订阅', 409, 'conflict');
    const now = new Date().toISOString();
    const s: ClientSub = { id: crypto.randomUUID(), name, enabled: true, rule: { team_mode: 'all', team_ids: [], direct: true, include_ids: [], exclude_ids: [], ...body.rule }, rule_text: '', items: 0, protocols: body.protocols ?? 'reality', token_at: now, last_fetch_at: null, last_fetch_ip: null };
    subs.push(s);
    return { ...view(s), url: url() };
  });
  R('PATCH', '/client-subs/([^/]+)', ({ params, body }) => {
    const s = find(params[0]);
    if (body.name !== undefined) s.name = String(body.name).trim();
    if (body.rule) s.rule = body.rule;
    if (body.enabled !== undefined) s.enabled = body.enabled;
    if (body.protocols !== undefined) s.protocols = body.protocols;
    return view(s);
  });
  R('POST', '/client-subs/([^/]+)/rotate', ({ params }) => {
    const s = find(params[0]);
    Object.assign(s, { token_at: new Date().toISOString(), last_fetch_at: null, last_fetch_ip: null });
    return { ...view(s), url: url() };
  });
  R('DELETE', '/client-subs/([^/]+)', ({ params }) => { find(params[0]); subs.splice(subs.findIndex((x) => x.id === params[0]), 1); return { ok: true }; });
}
