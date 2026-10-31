// mock 的客户批次账与客户到期页（同服务端规格：管理端 /teams/{id}/batches 等，公开 /public/customer/{token}）。
// 示例数据是用户参考图里的团队2（9 个批次），「今天」固定 2026-10-21，样子与参考图对得上；公开页的演示令牌是 demo。
// 派生字段（start / end / count / months / node_months / renewals / state / summary / title / desc / regions）都在这里按规格算。
import type { BatchState, CustomerBatch, CustomerPage, PublicCustomer, Team } from '@/lib/types';
import { addMonths } from '@/lib/format';

type Reg = (method: string, path: string, h: (c: { params: string[]; body: any }) => unknown) => void; // eslint-disable-line @typescript-eslint/no-explicit-any
type Bad = (msg: string, status?: number, code?: string) => Error;

const TODAY = '2026-10-21';
interface MPeriod { id: string; start_date: string; end_date: string; months: number; count: number }
interface MBatch { id: string; team_id: string; seq: number; region: string; note: string; periods: MPeriod[] }
interface MPage { token: string; enabled: boolean; created_at: string; token_at: string; last_view_at: string | null }

let seq = 0;
const nid = (p: string) => `${p}${++seq}`;
const p2 = (n: number) => String(n).padStart(2, '0');
const DATE = /^\d{4}-\d{2}-\d{2}$/;
const validDate = (s: unknown): s is string => typeof s === 'string' && DATE.test(s) && !Number.isNaN(Date.parse(`${s}T00:00:00Z`)) && new Date(`${s}T00:00:00Z`).toISOString().slice(0, 10) === s;
const mmdd = (s: string) => `${s.slice(5, 7)}/${s.slice(8, 10)}`;
const token32 = () => btoa(String.fromCharCode(...crypto.getRandomValues(new Uint8Array(32)))).replace(/\+/g, '-').replace(/\//g, '_').replace(/=+$/, '');

// 参考图的示例：[编号, 地区, [[起, 止, 月数, 个数]…], 说明]（2026 年）
const SAMPLE: [number, string, [string, string, number, number][], string][] = [
  [1, '美区', [['07-01', '08-01', 1, 4]], '首批4个统一到期'],
  [2, '美区', [['07-26', '08-26', 1, 6], ['08-26', '09-26', 1, 6], ['09-26', '10-26', 1, 6]], ''],
  [3, '美区', [['08-11', '09-11', 1, 2], ['09-11', '10-11', 1, 2]], ''],
  [4, '美区', [['09-13', '10-12', 1, 12]], ''],
  [5, '美区', [['09-15', '10-15', 1, 5], ['10-15', '11-14', 1, 4]], '另1个归团队1'],
  [6, '美区', [['09-18', '10-18', 1, 5], ['10-18', '11-17', 1, 5]], ''],
  [7, '墨西哥', [['09-22', '10-21', 1, 1]], ''],
  [8, '美区', [['09-22', '10-21', 1, 5]], ''],
  [9, '菲律宾', [['09-27', '10-25', 1, 1]], '起点采用购买日'],
];

const batches: MBatch[] = SAMPLE.map(([n, region, ps, note]) => ({
  id: nid('cb'), team_id: 't2', seq: n, region, note,
  periods: ps.map(([a, z, months, count]) => ({ id: nid('cp'), start_date: `2026-${a}`, end_date: `2026-${z}`, months, count })),
}));
const pages = new Map<string, MPage>([['t2', { token: 'demo', enabled: true, created_at: '2026-10-20T10:12:00Z', token_at: '2026-10-20T10:12:00Z', last_view_at: '2026-10-21T01:40:00Z' }]]);

const sorted = (b: MBatch) => [...b.periods].sort((x, y) => (x.start_date < y.start_date ? -1 : x.start_date > y.start_date ? 1 : 0));
/** 「含2次续期」/「初始5个 → 续期4个」（相邻相同的合并）；只有一段为空 */
function summaryOf(ps: MPeriod[]): string {
  if (ps.length < 2) return '';
  const counts = ps.map((p) => p.count);
  if (counts.every((c) => c === counts[0])) return `含${ps.length - 1}次续期`;
  const chain = counts.filter((c, i) => i === 0 || c !== counts[i - 1]);
  return chain.map((c, i) => `${i === 0 ? '初始' : '续期'}${c}个`).join(' → ');
}
function view(b: MBatch): CustomerBatch {
  const ps = sorted(b);
  const start = ps[0].start_date;
  const end = ps.reduce((m, p) => (p.end_date > m ? p.end_date : m), ps[0].end_date);
  const count = ps[ps.length - 1].count;
  const state: BatchState = end < TODAY ? 'expired' : end === TODAY ? 'today' : 'active';
  const head = `第${p2(b.seq)}批 · ${b.region}`;
  return {
    id: b.id, seq: b.seq, region: b.region, note: b.note, summary: summaryOf(ps),
    title: state === 'expired' ? `${head} · 原${count}个（已到期）` : `${head} · 当前有效${count}个`,
    start_date: start, end_date: end, count, months: ps.reduce((s, p) => s + p.months, 0),
    node_months: ps.reduce((s, p) => s + p.count * p.months, 0), renewals: ps.length - 1, state,
    periods: ps.map((p) => ({ ...p })),
  };
}
const descOf = (b: CustomerBatch) => [b.summary, b.note].filter(Boolean).join('；');

export function installCustomerMock(R: Reg, o: { teams: () => Team[]; bad: Bad }) {
  const { bad } = o;
  const team = (id: string) => { const t = o.teams().find((x) => x.id === id); if (!t) throw bad('团队不存在', 404, 'not_found'); return t; };
  const batch = (id: string) => { const b = batches.find((x) => x.id === id); if (!b) throw bad('批次不存在', 404, 'not_found'); return b; };
  const pageView = (tid: string): CustomerPage | null => {
    const p = pages.get(tid);
    return p ? { enabled: p.enabled, link: `${location.origin}/c/${p.token}`, created_at: p.created_at, token_at: p.token_at, last_view_at: p.last_view_at } : null;
  };
  const int = (v: unknown, lo: number, hi: number, what: string) => {
    if (!Number.isInteger(v) || (v as number) < lo || (v as number) > hi) throw bad(`${what}要是 ${lo}–${hi} 的整数`);
    return v as number;
  };
  const region = (v: unknown) => { const s = String(v ?? '').trim(); if (!s || [...s].length > 20) throw bad('地区填 1–20 个字'); return s; };
  const note = (v: unknown) => { const s = String(v ?? '').trim(); if ([...s].length > 60) throw bad('说明最多 60 个字'); return s; };
  const date = (v: unknown, what: string) => { if (!validDate(v)) throw bad(`${what}不是有效日期（YYYY-MM-DD）`); return v; };
  const seqFree = (tid: string, n: number, self?: string) => { if (batches.some((b) => b.team_id === tid && b.seq === n && b.id !== self)) throw bad(`第${p2(n)}批已经有了，换一个编号`, 409, 'conflict'); };
  /** 同一批次的段不得重叠：409 写明哪两段 */
  const checkOverlap = (b: MBatch) => {
    const ps = sorted(b);
    for (let i = 1; i < ps.length; i++) {
      if (ps[i].start_date < ps[i - 1].end_date) throw bad(`第${i}段（${mmdd(ps[i - 1].start_date)}–${mmdd(ps[i - 1].end_date)}）和第${i + 1}段（${mmdd(ps[i].start_date)}–${mmdd(ps[i].end_date)}）重叠`, 409, 'conflict');
    }
  };
  const ofPeriod = (id: string) => { for (const b of batches) { const p = b.periods.find((x) => x.id === id); if (p) return { b, p }; } throw bad('这一段不存在', 404, 'not_found'); };

  R('GET', '/teams/([^/]+)/batches', ({ params }) => {
    const t = team(params[0]);
    return { team: { id: t.id, name: t.name }, today: TODAY, page: pageView(t.id), batches: batches.filter((b) => b.team_id === t.id).sort((a, b) => a.seq - b.seq).map(view) };
  });
  R('POST', '/teams/([^/]+)/batches', ({ params, body }) => {
    const t = team(params[0]);
    const start = date(body.start_date, '开始日期');
    const months = int(body.months, 1, 36, '月数'); const count = int(body.count, 1, 999, '个数');
    const end = body.end_date == null || body.end_date === '' ? addMonths(start, months) : date(body.end_date, '到期日');
    if (end <= start) throw bad('到期日要晚于开始日期');
    const n = body.seq == null ? Math.max(0, ...batches.filter((b) => b.team_id === t.id).map((b) => b.seq)) + 1 : int(body.seq, 1, 999, '编号');
    seqFree(t.id, n);
    const b: MBatch = { id: nid('cb'), team_id: t.id, seq: n, region: region(body.region), note: note(body.note), periods: [{ id: nid('cp'), start_date: start, end_date: end, months, count }] };
    batches.push(b);
    return view(b);
  });
  R('PATCH', '/batches/([^/]+)', ({ params, body }) => {
    const b = batch(params[0]);
    const next = { region: body.region === undefined ? b.region : region(body.region), note: body.note === undefined ? b.note : note(body.note), seq: body.seq === undefined ? b.seq : int(body.seq, 1, 999, '编号') };
    seqFree(b.team_id, next.seq, b.id);
    Object.assign(b, next);
    return view(b);
  });
  R('DELETE', '/batches/([^/]+)', ({ params }) => { const b = batch(params[0]); batches.splice(batches.indexOf(b), 1); return { id: b.id }; });
  R('POST', '/batches/([^/]+)/renew', ({ params, body }) => {
    const b = batch(params[0]); const v = view(b);
    const months = int(body.months, 1, 36, '月数');
    const count = body.count == null ? v.count : int(body.count, 1, 999, '个数');
    const end = body.end_date == null || body.end_date === '' ? addMonths(v.end_date, months) : date(body.end_date, '到期日');
    if (end <= v.end_date) throw bad(`新的到期日要晚于当前到期日 ${v.end_date}`);
    b.periods.push({ id: nid('cp'), start_date: v.end_date, end_date: end, months, count });
    return view(b);
  });
  R('PATCH', '/batch-periods/([^/]+)', ({ params, body }) => {
    const { b, p } = ofPeriod(params[0]);
    const next = {
      start_date: body.start_date === undefined ? p.start_date : date(body.start_date, '开始日期'),
      end_date: body.end_date === undefined ? p.end_date : date(body.end_date, '到期日'),
      months: body.months === undefined ? p.months : int(body.months, 1, 36, '月数'),
      count: body.count === undefined ? p.count : int(body.count, 1, 999, '个数'),
    };
    if (next.end_date <= next.start_date) throw bad('到期日要晚于开始日期');
    const old = { ...p };
    Object.assign(p, next);
    try { checkOverlap(b); } catch (e) { Object.assign(p, old); throw e; }
    return view(b);
  });
  R('DELETE', '/batch-periods/([^/]+)', ({ params }) => {
    const { b, p } = ofPeriod(params[0]);
    if (b.periods.length === 1) throw bad('只剩这一段，要删就删整个批次', 409, 'conflict');
    b.periods.splice(b.periods.indexOf(p), 1);
    return view(b);
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
    const live = batches.filter((b) => b.team_id === tid).sort((a, b) => a.seq - b.seq).map(view).filter((b) => b.state !== 'expired');
    const regions: PublicCustomer['regions'] = [];
    for (const b of live) {
      let r = regions.find((x) => x.region === b.region);
      if (!r) regions.push((r = { region: b.region, counts: [], total: 0, today_count: 0 }));
      r.counts.push(b.count); r.total += b.count;
      if (b.state === 'today') r.today_count += b.count;
    }
    const out: PublicCustomer = {
      team: team(tid).name, today: TODAY, regions, node_months: live.reduce((s, b) => s + b.node_months, 0),
      batches: live.map((b) => ({
        seq: b.seq, region: b.region, title: b.title, desc: descOf(b), start_date: b.start_date, end_date: b.end_date,
        count: b.count, months: b.months, node_months: b.node_months, state: b.state,
        periods: b.periods.map(({ start_date, end_date, months, count }) => ({ start_date, end_date, months, count })),
      })),
    };
    return out;
  });
}
