// 回收站（机器页的「回收站」范围）：退役后到期清理、已从面板移走的机器，只读。
// 点一行展开快照的关键字段，没有任何操作按钮。搜索（?q=）与来源（?source=fleet / legacy）记在 URL，服务端筛选、分页。
// 表头排序只在整个回收站一页放得下时给（排序只作用于已取回的这一页，分页时会误导）；只有一种来源时不显示来源列。
import * as React from 'react';
import { ChevronDown, ChevronRight, SearchX, Trash2, X } from 'lucide-react';
import { useRecycleBin, useRecycleDetail } from '@/lib/queries';
import * as F from '@/lib/format';
import { cycleLabel, kindLabel, orgFmt } from '@/lib/derive';
import type { RecycleDetail, RecycleItem } from '@/lib/types';
import { Button } from '@/components/ui/button';
import { SearchInput } from '@/components/ui/form';
import { Select } from '@/components/ui/select';
import { Trunc } from '@/components/ui/overlays';
import { SkeletonRows } from '@/components/ui/skeleton';
import { Chip, CopyButton, Empty, ErrorNote, LabelValue, LabelValueGrid, Num, PageHead, Panel, SortHead, TableWrap, type SortState } from '@/components/app/primitives';
import { Pagination } from '@/components/app/Pagination';
import { collator } from '@/lib/sort';
import { usePage } from '@/lib/usePage';
import { useUrlSearch } from '@/lib/urlState';

const PAGE_SIZES = [25, 50, 100];
const SOURCE_LABEL: Record<string, string> = { fleet: '本面板', legacy: '旧面板' };
const SOURCE_OPTS = Object.entries(SOURCE_LABEL).map(([value, label]) => ({ value, label }));
type ColDef = { id: string; header: string; width?: number; r?: boolean; first?: 'asc' | 'desc' };
const COLS: ColDef[] = [
  { id: 'name', header: '名称', width: 160, first: 'asc' }, { id: 'ip', header: 'IP', width: 136 }, { id: 'plan', header: '套餐 · 供应商', width: 140 }, { id: 'price', header: '价格 / 周期', width: 116 },
  { id: 'recycled', header: '上架 → 退役 → 回收', width: 164, first: 'desc' }, { id: 'reason', header: '原因' }, { id: 'traffic', header: '业务流量', width: 96, r: true, first: 'desc' },
  { id: 'source', header: '来源', width: 84 }, { id: 'chev', header: '', width: 36 },
];
/** 组织时区的那一天（同一行的原因里的时刻、服务端给的回收日都按组织时区，不用浏览器本机时区） */
const day = (iso: string | null | undefined) => (iso ? orgFmt().format(new Date(iso)) : '—');
/** 上架 / 退役日与回收日同一年时去掉年份（09-30 → 10-24），日期列不用 230 宽；回收日那行与悬停提示仍写全 */
const dayIn = (iso: string | null | undefined, ref: string) => { const s = day(iso); return s.slice(0, 4) === day(ref).slice(0, 4) ? s.slice(5) : s; };
const traffic = (r: Pick<RecycleItem, 'traffic_up' | 'traffic_down'>) => (r.traffic_up == null && r.traffic_down == null ? null : (r.traffic_up ?? 0) + (r.traffic_down ?? 0));
const price = (r: Pick<RecycleItem, 'price_cents' | 'currency' | 'cycle'>) => (r.price_cents != null && r.currency ? `${F.money(r.price_cents, r.currency)}${r.cycle ? ` / ${cycleLabel(r.cycle)}` : ''}` : null);

/** 原因里夹的 ISO 时间（2026-10-08T15:00:00Z）按组织时区写成「2026-10-08 23:00」 */
const ISO_TIME = /\d{4}-\d{2}-\d{2}T\d{2}:\d{2}(?::\d{2}(?:\.\d+)?)?(?:Z|[+-]\d{2}:?\d{2})/g;
const orgTime = (iso: string) => orgFmt({ year: 'numeric', month: '2-digit', day: '2-digit', hour: '2-digit', minute: '2-digit', hour12: false }).format(new Date(iso));
const reasonFull = (s: string) => s.replace(ISO_TIME, orgTime);
/** 列表里的原因只留关键句：旧面板导入的原因全带「旧面板 2026-09-30 标注：」这样的前缀，去掉后各行才看得出差别；全文在展开区。
 *  去掉前缀后：标注写在「」里的，主行是标注、次行是后面的补充（最后流量…）；否则按句号 / 分号切句（括号里的不切），
 *  有「推断…」的那句就留它，没有就留整段开头——不从括号中间切 */
const LEGACY_PREFIX = /^旧面板\s*\d{4}-\d{2}-\d{2}[^：:（(「]*标注[：:]?\s*/;
const QUOTED = /^「([^」]*)」[\s，,；;。]*/;
const SENTENCE_END = new Set(['。', '；', ';']);
function sentences(s: string): string[] {
  const out: string[] = []; let depth = 0; let cur = '';
  for (const c of s) {
    if (c === '（' || c === '(') depth++;
    else if ((c === '）' || c === ')') && depth > 0) depth--;
    if (depth === 0 && SENTENCE_END.has(c)) { out.push(cur); cur = ''; } else cur += c;
  }
  out.push(cur);
  return out.map((x) => x.trim()).filter(Boolean);
}
function reasonParts(s: string): { main: string; sub?: string } {
  const t = reasonFull(s).replace(LEGACY_PREFIX, '');
  const q = QUOTED.exec(t);
  if (q) return { main: q[1], sub: t.slice(q[0].length) || undefined };
  return { main: sentences(t).find((x) => x.includes('推断')) ?? t };
}

/** 展开的一行：这条记录的关键字段 + 快照里挑出来的几项（快照形状不固定，没有就不显示）；名字、团队行里已有，不重复 */
function Detail({ r }: { r: RecycleItem }) {
  const d = useRecycleDetail(r.id);
  const snap: RecycleDetail['snapshot'] = d.data?.snapshot ?? {};
  const note = typeof snap.machine?.note === 'string' && snap.machine.note ? snap.machine.note : null;
  const entries = Array.isArray(snap.entries) ? snap.entries : null, renewals = Array.isArray(snap.renewals) ? snap.renewals : null;
  const out = snap.outbound?.host ? `${snap.outbound.protocol ?? ''} ${snap.outbound.host}${snap.outbound.port ? `:${snap.outbound.port}` : ''}`.trim() : null;
  const bytes = (n: number | null) => (n == null ? '—' : F.str(F.bytes(n)));
  return (
    <LabelValueGrid>
      {r.reason && <LabelValue label="原因" wrap>{reasonFull(r.reason)}</LabelValue>}
      {r.last_name && r.last_name !== r.name && <LabelValue label="回收时的名字">{r.last_name}</LabelValue>}
      {r.host_name && <LabelValue label="宿主机">{r.host_name}</LabelValue>}
      <LabelValue label="服务编号">{r.service_ref ?? '—'}</LabelValue>
      <LabelValue label="到期日">{r.due_date ?? '—'}</LabelValue>
      <LabelValue label="回收者">{F.actor(r.actor)}</LabelValue>
      {(r.traffic_down != null || r.traffic_up != null) && <LabelValue label="业务流量 下载 / 上传">{`${bytes(r.traffic_down)} / ${bytes(r.traffic_up)}`}</LabelValue>}
      {(r.nic_rx != null || r.nic_tx != null) && <LabelValue label="网卡流量 下行 / 上行">{`${bytes(r.nic_rx)} / ${bytes(r.nic_tx)}`}</LabelValue>}
      {out && <LabelValue label="出口" plain>{out}</LabelValue>}
      {entries != null && <LabelValue label="入口历史" wrap>{entries.length ? entries.map((e) => `${e.relay}:${e.port}${e.removed_at ? '（已移除）' : ''}`).join('、') : '无'}</LabelValue>}
      {renewals != null && <LabelValue label="续费记录">{`${renewals.length} 次`}</LabelValue>}
      {note && <LabelValue label="备注" wrap>{note}</LabelValue>}
      <LabelValue label="来源编号"><span className="row" style={{ gap: 4, flexWrap: 'nowrap', minWidth: 0 }}><Trunc className="mono">{r.source_id}</Trunc><CopyButton text={r.source_id} label="来源编号" size={13} /></span></LabelValue>
      {d.isError && <ErrorNote error={d.error} onRetry={() => void d.refetch()}>详情加载失败：</ErrorNote>}
    </LabelValueGrid>
  );
}

function ReasonCell({ reason }: { reason: string }) {
  const { main, sub } = reasonParts(reason);
  return <><Trunc className="cell-main" text={reasonFull(reason)}>{main}</Trunc>{sub && <span className="sub">{sub}</span>}</>;
}

function Row({ r, open, source, span, onToggle }: { r: RecycleItem; open: boolean; source: boolean; span: number; onToggle: (id: number | null) => void }) {
  const t = traffic(r);
  const toggle = () => onToggle(open ? null : r.id);
  return (
    <>
      <tr className="is-link" tabIndex={0} aria-expanded={open} onClick={toggle}
        onKeyDown={(e) => { if (e.key === 'Enter' || e.key === ' ') { e.preventDefault(); toggle(); } }}>
        <td><span className="cell-main strong">{r.name}</span><span className="sub">{kindLabel(r.kind)} · {r.team ?? '—'}</span></td>
        <td><span className="cell-main">{r.ip ?? '—'}</span>{r.host_name && <span className="sub">同机 · {r.host_name}</span>}</td>
        <td>{r.plan ? <Trunc className="cell-main">{r.plan}</Trunc> : <span className="cell-main t-muted">—</span>}<span className="sub">{r.provider ?? '—'}</span></td>
        <td>{price(r) ?? <span className="t-muted">—</span>}</td>
        <td><span className="cell-main" data-tip={`${day(r.activated_at)} → ${day(r.retired_at)}`}>{dayIn(r.activated_at, r.recycled_at)} → {dayIn(r.retired_at, r.recycled_at)}</span><span className="sub">回收 {day(r.recycled_at)}</span></td>
        <td>{r.reason ? <ReasonCell reason={r.reason} /> : <span className="t-muted">—</span>}</td>
        <td className="r">{t == null || t === 0 ? <span className="t-muted">—</span> : <Num f={F.bytes(t)} />}</td>
        {source && <td><Chip variant="neutral">{SOURCE_LABEL[r.source] ?? r.source}</Chip></td>}
        <td className="chev">{open ? <ChevronDown size={14} /> : <ChevronRight size={14} />}</td>
      </tr>
      {open && <tr><td colSpan={span} style={{ background: 'var(--sunken)', whiteSpace: 'normal' }}><Detail r={r} /></td></tr>}
    </>
  );
}

const SORT_KEY: Record<string, (r: RecycleItem) => string | number> = {
  name: (r) => r.name, recycled: (r) => Date.parse(r.recycled_at), traffic: (r) => traffic(r) ?? -1,
};
const cmp = (a: string | number, b: string | number) => (typeof a === 'string' ? collator.compare(a, b as string) : a - (b as number));

export function RecycleBin({ sp, upd, tools }: { sp: URLSearchParams; upd: (patch: Record<string, string>) => void; tools: React.ReactNode }) {
  const text = sp.get('q') ?? '';
  const [input, setInput] = useUrlSearch(sp, upd);
  const source = sp.get('source') ?? '';
  const setSource = React.useCallback((v: string) => upd({ source: v }), [upd]);
  const [openId, setOpenId] = React.useState<number | null>(null);
  const [sort, setSort] = React.useState<SortState | null>(null);
  const [total, setTotal] = React.useState<number | undefined>(undefined);
  const pg = usePage({ total, sizes: PAGE_SIZES, size: 50, reset: [text, source] });
  const q = useRecycleBin({ q: text, source, offset: pg.offset, limit: pg.size });
  if (q.data && q.data.total !== total) setTotal(q.data.total);
  const fetched = q.data?.items;
  const sortable = !!q.data && q.data.total <= pg.size;
  const rows = React.useMemo(() => {
    const key = sort && SORT_KEY[sort.id];
    return fetched && key && sortable ? [...fetched].sort((a, b) => (sort.desc ? -1 : 1) * cmp(key(a), key(b))) : fetched ?? [];
  }, [fetched, sort, sortable]);
  const filtered = !!(text || source);
  const clear = () => { setInput(''); upd({ q: '', source: '' }); };
  // 这一页全是同一种来源时来源列没有信息（筛选了来源、或回收站里只有旧面板导入的）
  const showSource = new Set(rows.map((r) => r.source)).size > 1;
  const cols = COLS.filter((c) => c.id !== 'source' || showSource);

  return (
    <div className="stack">
      <PageHead title="机器" note={q.data ? `回收站 ${q.data.total} 台` : undefined} tools={tools} />

      <div className="row" role="search" ref={pg.anchor}>
        <SearchInput value={input} onChange={(e) => setInput(e.target.value)} placeholder="搜索名称、IP、团队、供应商…" aria-label="搜索" className="w-full sm:w-[260px]" />
        <div className="w-[130px]"><Select value={source} onChange={setSource} allowEmpty="全部来源" placeholder="来源" options={SOURCE_OPTS} aria-label="来源" /></div>
        {filtered && <Button variant="quiet" size="md" onClick={clear}><X />清除筛选</Button>}
      </div>

      {q.isError && <ErrorNote error={q.error} onRetry={() => void q.refetch()} />}
      <Panel flush>
        <TableWrap>
          <table className="dt" style={{ minWidth: 1150 }}>
            <colgroup>{cols.map((c) => <col key={c.id} style={{ width: c.width }} />)}</colgroup>
            <SortHead cols={cols.map((c) => ({ id: c.id, header: c.header, r: c.r, first: sortable ? c.first : undefined }))} sort={sort} onSort={setSort} />
            <tbody>{rows.map((r) => <Row key={r.id} r={r} open={r.id === openId} source={showSource} span={cols.length} onToggle={setOpenId} />)}</tbody>
          </table>
        </TableWrap>
        {q.isLoading && <SkeletonRows rows={6} />}
        {!q.isLoading && !q.isError && rows.length === 0 && (
          <Empty icon={filtered ? <SearchX size={22} /> : <Trash2 size={22} />} text={filtered ? '没有符合条件的记录' : '回收站是空的'}>
            {filtered && <Button size="sm" onClick={clear}>清除筛选</Button>}
          </Empty>
        )}
        <Pagination pg={pg} unit="台" />
      </Panel>
    </div>
  );
}
