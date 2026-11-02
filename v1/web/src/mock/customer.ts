// mock 的按节点数计费（同服务端规格：计费名额、待定节点、增减记录、派生批次、月度应收）与客户到期页（公开 /public/customer/{token}）。
// 静态数据 + 写操作改内存，不模拟每分钟的对账。数据是生产的真实形状：团队1 九个 10-01 起算（团队1-01..09）+ 团队1-10 10-30 起算；
// 团队2 十九个 10-01 起算，10-28 有两次替换，另有一个刚加入、还在待定的节点。「今天」固定 2026-10-31；公开页的演示令牌是 demo。
// 派生（批次的期、start / end / count / months / node_months / renewals / state / summary / title、应收）都在这里按规格算。
import type { BatchPeriod, BatchState, BillingEvent, BillingEventKind, BillingSlot, CustomerBatch, CustomerPage, MachineView, PublicCustomer, Receivable, Team } from '@/lib/types';
import { addDays, addMonths } from '@/lib/format';
const batchName = (b: { seq: number; region: string }) => `第${String(b.seq).padStart(2, '0')}批 · ${b.region}`;

type Reg = (method: string, path: string, h: (c: { params: string[]; query: URLSearchParams; body: any }) => unknown) => void; // eslint-disable-line @typescript-eslint/no-explicit-any
type Bad = (msg: string, status?: number, code?: string) => Error;

const TODAY = '2026-10-31';
const GRACE = 86_400_000;
interface MSlot { id: string; team_id: string; region: string; start_date: string; holder: { id: string; name: string } | null; last_holder_name: string | null; vacated_at: string | null; end_date: string | null; manual: boolean; note: string }
interface MPending { team_id: string; machine: { id: string; name: string }; seen_at: string }
interface MEvent extends BillingEvent { team_id: string }
interface MPage { token: string; enabled: boolean; created_at: string; token_at: string; last_view_at: string | null }

let seq = 0;
const nid = (p: string) => `${p}${(++seq).toString(16).padStart(8, '0')}`;
const p2 = (n: number) => String(n).padStart(2, '0');
const DATE = /^\d{4}-\d{2}-\d{2}$/;
const validDate = (s: unknown): s is string => typeof s === 'string' && DATE.test(s) && !Number.isNaN(Date.parse(`${s}T00:00:00Z`)) && new Date(`${s}T00:00:00Z`).toISOString().slice(0, 10) === s;
const md = (s: string) => s.slice(5, 10);
/** 时刻的北京日期 */
const bjDate = (iso: string) => new Date(Date.parse(iso) + 8 * 3600_000).toISOString().slice(0, 10);
const token32 = () => btoa(String.fromCharCode(...crypto.getRandomValues(new Uint8Array(32)))).replace(/\+/g, '-').replace(/\//g, '_').replace(/=+$/, '');
const cmp = (a: string, b: string) => (a < b ? -1 : a > b ? 1 : 0);

// ---------- 初始数据 ----------
const slots: MSlot[] = [];
const pending: MPending[] = [];
const events: MEvent[] = [];
const ev = (team_id: string, slot_id: string | null, at: string, kind: BillingEventKind, machine_name: string | null, other_name: string | null, text: string) => {
  events.push({ id: nid('ev'), team_id, slot_id, at, kind, machine_name, other_name, text });
};
const holder = (team: 't1' | 't2', n: number) => ({ id: `m-${team}-${p2(n)}`, name: `${team === 't1' ? '团队1' : '团队2'}-${p2(n)}` });
for (const [tid, n] of [['t1', 9], ['t2', 19]] as const) {
  for (let i = 1; i <= n; i++) {
    const h = holder(tid, i);
    const s: MSlot = { id: nid('sl'), team_id: tid, region: '美区', start_date: '2026-10-01', holder: h, last_holder_name: null, vacated_at: null, end_date: null, manual: false, note: '' };
    slots.push(s);
    ev(tid, s.id, '2026-10-01T00:00:00+08:00', 'init', h.name, null, `${h.name} 计入计费节点（10-01 起算）`);
  }
}
{
  const h = holder('t1', 10);
  const s: MSlot = { id: nid('sl'), team_id: 't1', region: '美区', start_date: '2026-10-30', holder: h, last_holder_name: null, vacated_at: null, end_date: null, manual: false, note: '' };
  slots.push(s);
  ev('t1', s.id, '2026-10-30T14:26:00+08:00', 'add', h.name, null, `${h.name} 加入，新增计费节点（10-30 起算）`);
  // 10-28 团队2 的两次替换：换下来的旧节点进了备用（改名 备用-NN），新节点接过编号，名额与起算日不变
  for (const [n, spare, at] of [[7, '备用-01', '2026-10-28T10:05:00+08:00'], [12, '备用-02', '2026-10-28T16:40:00+08:00']] as const) {
    const sl = slots.find((x) => x.team_id === 't2' && x.holder?.name === `团队2-${p2(n)}`)!;
    ev('t2', sl.id, at, 'swap', sl.holder!.name, spare, `${sl.holder!.name} → ${spare} 替换，计费不变`);
  }
  pending.push({ team_id: 't2', machine: { id: 'm-t2-20', name: '团队2-20' }, seen_at: '2026-10-31T09:12:00+08:00' });
}
const pages = new Map<string, MPage>([['t2', { token: 'demo', enabled: true, created_at: '2026-10-20T10:12:00Z', token_at: '2026-10-20T10:12:00Z', last_view_at: '2026-10-31T01:40:00Z' }]]);

// ---------- 派生 ----------
const slotState = (s: MSlot): BillingSlot['state'] => (s.end_date ? 'ended' : !s.holder && !s.manual ? 'vacated' : 'active');
/** 名额在「起点为 ps 的那一期」计费 ⇔ ps < end_date（没结束为无穷） */
const bills = (s: MSlot, ps: string) => !s.end_date || ps < s.end_date;
/** 「含2次续期」/「初始5个 → 续期4个」（相邻相同的合并）；只有一期为空 */
function summaryOf(ps: BatchPeriod[]): string {
  if (ps.length < 2) return '';
  const counts = ps.map((p) => p.count);
  if (counts.every((c) => c === counts[0])) return `含${ps.length - 1}次续期`;
  return counts.filter((c, i) => i === 0 || c !== counts[i - 1]).map((c, i) => `${i === 0 ? '初始' : '续期'}${c}个`).join(' → ');
}
interface Derived { batches: (CustomerBatch & { slots: MSlot[] })[] }
function derive(tid: string): Derived {
  const groups = new Map<string, MSlot[]>();
  for (const s of slots.filter((x) => x.team_id === tid)) {
    const k = `${s.start_date}|${s.region}`;
    groups.set(k, [...(groups.get(k) ?? []), s]);
  }
  const keys = [...groups.keys()].sort(cmp);
  const batches = keys.map((key, i) => {
    const ss = groups.get(key)!;
    const [start, region] = key.split('|');
    const open = ss.some((s) => !s.end_date);
    let last = 0;
    if (open) { while (addMonths(start, last + 1) <= TODAY) last++; } else {
      const maxEnd = ss.reduce((m, s) => (s.end_date! > m ? s.end_date! : m), ss[0].end_date!);
      const lastDay = addDays(maxEnd, -1);
      while (addMonths(start, last + 1) <= lastDay) last++;
    }
    const periods: BatchPeriod[] = [];
    for (let k = 0; k <= last; k++) {
      const ps = addMonths(start, k);
      periods.push({ start_date: ps, end_date: addMonths(start, k + 1), months: 1, count: ss.filter((s) => bills(s, ps)).length });
    }
    const end = periods[periods.length - 1].end_date;
    const count = periods[periods.length - 1].count;
    const state: BatchState = end < TODAY ? 'expired' : end === TODAY ? 'today' : 'active';
    const head = `第${p2(i + 1)}批 · ${region}`;
    return {
      key, seq: i + 1, region, summary: summaryOf(periods),
      title: state === 'expired' ? `${head} · 原${count}个（已到期）` : `${head} · 当前有效${count}个`,
      start_date: start, end_date: end, count, months: periods.length, node_months: periods.reduce((a, p) => a + p.count, 0),
      renewals: periods.length - 1, state, periods, slots: ss,
    };
  });
  return { batches };
}
const strip = ({ slots: _s, ...b }: Derived['batches'][number]): CustomerBatch => b; // eslint-disable-line @typescript-eslint/no-unused-vars
const slotName = (s: MSlot) => s.holder?.name ?? (s.manual ? `手动${s.note ? ` · ${s.note}` : ''}` : s.last_holder_name ? `${s.last_holder_name}（已离开）` : '空出的名额');
function slotView(s: MSlot, d: Derived): BillingSlot {
  const b = d.batches.find((x) => x.slots.includes(s))!;
  return { id: s.id, batch_seq: b.seq, region: s.region, start_date: s.start_date, state: slotState(s), holder: s.holder, last_holder_name: s.manual ? null : s.last_holder_name, vacated_at: s.vacated_at, end_date: s.end_date, manual: s.manual, note: s.note };
}
const eventView = ({ team_id: _t, ...e }: MEvent): BillingEvent => e; // eslint-disable-line @typescript-eslint/no-unused-vars
const teamEvents = (tid: string) => events.filter((e) => e.team_id === tid).sort((a, b) => Date.parse(b.at) - Date.parse(a.at) || cmp(b.id, a.id));
/** 某天 0 点时有效的名额数 */
const validAt = (tid: string, day: string) => slots.filter((s) => s.team_id === tid && s.start_date <= day && (!s.end_date || s.end_date > day)).length;

export function installCustomerMock(R: Reg, o: { teams: () => Team[]; machines: () => MachineView[]; bad: Bad; emit: (e: { type: string; id?: string }) => void }) {
  const { bad } = o;
  const billing = (tid: string) => o.emit({ type: 'billing', id: tid });
  for (const m of o.machines()) if (m.kind !== 'relay') m.billing_exempt = false;
  const team = (id: string) => { const t = o.teams().find((x) => x.id === id); if (!t) throw bad('团队不存在', 404, 'not_found'); return t; };
  const slot = (id: string) => { const s = slots.find((x) => x.id === id); if (!s) throw bad('计费名额不存在', 404, 'not_found'); return s; };
  const pageView = (tid: string): CustomerPage | null => {
    const p = pages.get(tid);
    return p ? { enabled: p.enabled, link: `${location.origin}/c/${p.token}`, created_at: p.created_at, token_at: p.token_at, last_view_at: p.last_view_at } : null;
  };
  const region = (v: unknown) => { const s = String(v ?? '').trim(); if (!s || [...s].length > 20) throw bad('地区填 1–20 个字'); return s; };
  const note = (v: unknown) => { const s = String(v ?? '').trim(); if ([...s].length > 60) throw bad('备注最多 60 个字'); return s; };
  const date = (v: unknown, what: string) => { if (!validDate(v)) throw bad(`${what}不是有效日期（YYYY-MM-DD）`); return v; };

  R('GET', '/teams/([^/]+)/batches', ({ params }) => {
    const t = team(params[0]);
    const d = derive(t.id);
    const ss = slots.filter((s) => s.team_id === t.id).sort((a, b) => cmp(a.start_date, b.start_date) || cmp(slotName(a), slotName(b)));
    const pend = pending.filter((p) => p.team_id === t.id);
    return {
      team: { id: t.id, name: t.name }, today: TODAY, page: pageView(t.id), batches: d.batches.map(strip),
      slots: ss.map((s) => slotView(s, d)),
      pending: pend.map((p) => ({ machine: p.machine, seen_at: p.seen_at, decide_at: new Date(Date.parse(p.seen_at) + GRACE).toISOString() })),
      totals: { active: ss.filter((s) => slotState(s) === 'active').length, pending: pend.length, vacated: ss.filter((s) => slotState(s) === 'vacated').length },
    };
  });
  R('GET', '/teams/([^/]+)/billing-events', ({ params, query }) => {
    const t = team(params[0]);
    const limit = Math.min(500, Number(query.get('limit') ?? 200) || 200);
    let list = teamEvents(t.id);
    const before = query.get('before');
    if (before) { const i = list.findIndex((e) => e.id === before); list = i < 0 ? [] : list.slice(i + 1); }
    return list.slice(0, limit).map(eventView);
  });
  R('GET', '/teams/([^/]+)/receivable', ({ params, query }) => {
    const t = team(params[0]);
    const month = query.get('month') ?? TODAY.slice(0, 7);
    if (!/^\d{4}-\d{2}$/.test(month)) throw bad('month 要是 YYYY-MM');
    const first = `${month}-01`; const next = addMonths(first, 1);
    const lines: Receivable['lines'] = [];
    for (const b of derive(t.id).batches) {
      // 起点落在这个月的期（不受「最后一期」限制：本月、下月的照样算出来）
      for (let k = 0; ; k++) {
        const ps = addMonths(b.start_date, k);
        if (ps >= next) break;
        if (ps < first) continue;
        const count = b.slots.filter((s) => bills(s, ps)).length;
        if (count > 0) lines.push({ batch_seq: b.seq, region: b.region, period_start: ps, period_end: addMonths(b.start_date, k + 1), count });
      }
    }
    const endDay = addDays(next, -1) < TODAY ? addDays(next, -1) : TODAY;
    const out: Receivable = {
      month, lines, total: lines.reduce((a, l) => a + l.count, 0), start_count: validAt(t.id, first), end_count: first > TODAY ? 0 : validAt(t.id, endDay),
      events: teamEvents(t.id).filter((e) => ['add', 'remove', 'swap'].includes(e.kind) && bjDate(e.at) >= first && bjDate(e.at) < next).map(eventView),
    };
    return out;
  });
  R('PATCH', '/teams/([^/]+)/batches/([^/]+)', ({ params, body }) => {
    const t = team(params[0]);
    const b = derive(t.id).batches.find((x) => x.key === params[1]); if (!b) throw bad('批次不存在', 404, 'not_found');
    const v = date(body.start_date, '起算日');
    const bad1 = b.slots.find((s) => s.end_date && s.end_date <= v);
    if (bad1) throw bad(`起算日 ${v} 要早于已结束名额的结束日 ${bad1.end_date}`);
    if (v !== b.start_date) {
      for (const s of b.slots) s.start_date = v;
      ev(t.id, null, new Date().toISOString(), 'edit', null, null, `${batchName(b)}：起算日 ${md(b.start_date)} → ${md(v)}（整批 ${b.slots.length} 个）`);
      billing(t.id);
    }
    const nb = derive(t.id).batches.find((x) => x.key === `${v}|${b.region}`)!;
    return strip(nb);
  });
  R('POST', '/teams/([^/]+)/billing-slots', ({ params, body }) => {
    const t = team(params[0]);
    const start = date(body.start_date, '起算日'); const r = region(body.region); const n = note(body.note);
    const count = body.count == null ? 1 : Number(body.count);
    if (!Number.isInteger(count) || count < 1 || count > 99) throw bad('个数要是 1–99 的整数');
    for (let i = 0; i < count; i++) {
      const s: MSlot = { id: nid('sl'), team_id: t.id, region: r, start_date: start, holder: null, last_holder_name: null, vacated_at: null, end_date: null, manual: true, note: n };
      slots.push(s);
      ev(t.id, s.id, new Date().toISOString(), 'manual_add', null, null, `手动添加计费节点${n ? `「${n}」` : ''}（${r}，${md(start)} 起算）`);
    }
    billing(t.id);
    return { count };
  });
  R('PATCH', '/billing-slots/([^/]+)', ({ params, body }) => {
    const s = slot(params[0]); const name = slotName(s);
    const changes: string[] = [];
    if (body.start_date !== undefined) { const v = date(body.start_date, '起算日'); if (v !== s.start_date) { changes.push(`起算日 ${md(s.start_date)} → ${md(v)}`); s.start_date = v; } }
    if (body.region !== undefined) { const v = region(body.region); if (v !== s.region) { changes.push(`地区 ${s.region} → ${v}`); s.region = v; } }
    if (body.note !== undefined) { const v = note(body.note); if (v !== s.note) { changes.push('备注'); s.note = v; } }
    if (body.end_date !== undefined) {
      if (body.end_date === null) {
        if (!s.manual) throw bad('只有手动添加的计费节点能恢复', 409, 'conflict');
        s.end_date = null; ev(s.team_id, s.id, new Date().toISOString(), 'edit', null, null, `${name} 恢复计费`);
      } else {
        if (s.holder) throw bad('节点还在团队里；不收这个节点的钱请把节点设为不计费', 409, 'conflict');
        const v = date(body.end_date, '结束日');
        if (v <= s.start_date) throw bad('结束日要晚于起算日');
        if (!s.manual && v > TODAY) throw bad('空出的计费节点结束日不能晚于今天');
        s.end_date = v;
        ev(s.team_id, s.id, new Date().toISOString(), 'manual_end', null, null, `${name} 结束计费（算到 ${md(v)}）`);
      }
    }
    if (changes.length) ev(s.team_id, s.id, new Date().toISOString(), 'edit', s.holder?.name ?? null, null, `${name} ${changes.join('，')}`);
    billing(s.team_id);
    return { id: s.id };
  });
  R('DELETE', '/billing-slots/([^/]+)', ({ params }) => {
    const s = slot(params[0]);
    if (!s.manual) throw bad('只有手动添加的计费节点能删除；节点离开团队后会自动结束', 409, 'conflict');
    slots.splice(slots.indexOf(s), 1);
    billing(s.team_id);
    return { id: s.id };
  });
  R('PUT', '/machines/([^/]+)/billing-exempt', ({ params, body }) => {
    const m = o.machines().find((x) => x.id === params[0]); if (!m) throw bad('机器不存在', 404, 'not_found');
    if (typeof body.exempt !== 'boolean') throw bad('exempt 要是 true / false');
    if (m.billing_exempt === body.exempt) return { exempt: body.exempt };
    m.billing_exempt = body.exempt;
    const now = new Date().toISOString();
    const held = slots.find((s) => s.holder?.id === m.id && !s.end_date);
    const tid = m.team?.id ?? held?.team_id;
    // 设不计费后服务端马上对账：名额立刻空出（之后照样走 24 小时配对）
    if (body.exempt && held) { held.last_holder_name = held.holder!.name; held.holder = null; held.vacated_at = now; }
    const pi = pending.findIndex((p) => p.machine.id === m.id);
    if (body.exempt && pi >= 0) pending.splice(pi, 1);
    if (!body.exempt && m.team && !held && pi < 0) pending.push({ team_id: m.team.id, machine: { id: m.id, name: m.name }, seen_at: now });
    if (tid) ev(tid, held?.id ?? null, now, 'exempt', m.name, null, body.exempt ? `${m.name} 设为不计费${held ? '（名额空出，24 小时内有节点加入就算替换）' : ''}` : `${m.name} 恢复计费（24 小时内有名额空出就算替换，否则新增）`);
    if (tid) billing(tid);
    return { exempt: body.exempt };
  });

  R('POST', '/teams/([^/]+)/customer-page', ({ params }) => {
    const t = team(params[0]); const now = new Date().toISOString();
    const old = pages.get(t.id);
    pages.set(t.id, { token: token32(), enabled: old?.enabled ?? true, created_at: old?.created_at ?? now, token_at: now, last_view_at: old?.last_view_at ?? null });
    return pageView(t.id);
  });
  R('PATCH', '/teams/([^/]+)/customer-page', ({ params, body }) => {
    const p = pages.get(team(params[0]).id); if (!p) throw bad('还没有客户页链接', 404, 'not_found');
    if (typeof body.enabled !== 'boolean') throw bad('enabled 要是 true / false');
    p.enabled = body.enabled;
    return pageView(params[0]);
  });
  R('DELETE', '/teams/([^/]+)/customer-page', ({ params }) => { const t = team(params[0]); pages.delete(t.id); return { team_id: t.id }; });

  R('GET', '/public/customer/([^/]+)', ({ params }) => {
    const hit = [...pages.entries()].find(([, p]) => p.token === params[0] && p.enabled);
    if (!hit) throw bad('没有这个页面', 404, 'not_found');
    const [tid, p] = hit;
    if (!p.last_view_at || Date.now() - Date.parse(p.last_view_at) > 600_000) p.last_view_at = new Date().toISOString();
    const live = derive(tid).batches.filter((b) => b.state !== 'expired');
    const regions: PublicCustomer['regions'] = [];
    for (const b of live) {
      let r = regions.find((x) => x.region === b.region);
      if (!r) regions.push((r = { region: b.region, counts: [], total: 0, today_count: 0 }));
      r.counts.push(b.count); r.total += b.count;
      if (b.state === 'today') r.today_count += b.count;
    }
    const out: PublicCustomer = {
      team: team(tid).name, today: TODAY, regions, node_months: live.reduce((s, b) => s + b.node_months, 0),
      batches: live.map((b) => ({ seq: b.seq, region: b.region, title: b.title, desc: b.summary, start_date: b.start_date, end_date: b.end_date, count: b.count, months: b.months, node_months: b.node_months, state: b.state, periods: b.periods })),
    };
    return out;
  });
}
