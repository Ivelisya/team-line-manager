// 回收站（机器页的「回收站」范围）：退役后到期清理、已从面板移走的机器，只读。
// 点一行展开快照的关键字段，没有任何操作按钮。搜索（?q=）与来源（?source=fleet / legacy）记在 URL，服务端筛选、分页。
import * as React from 'react';
import { ChevronDown, ChevronRight, SearchX, X } from 'lucide-react';
import { useRecycleBin, useRecycleDetail } from '@/lib/queries';
import * as F from '@/lib/format';
import { cycleLabel, kindLabel } from '@/lib/derive';
import type { RecycleDetail, RecycleItem } from '@/lib/types';
import { Button } from '@/components/ui/button';
import { SearchInput } from '@/components/ui/form';
import { Select } from '@/components/ui/select';
import { Trunc } from '@/components/ui/overlays';
import { SkeletonRows } from '@/components/ui/skeleton';
import { Chip, Empty, ErrorNote, LabelValue, LabelValueGrid, Num, PageHead, Panel, TableWrap } from '@/components/app/primitives';
import { Pagination } from '@/components/app/Pagination';
import { usePage } from '@/lib/usePage';
import { useUrlSearch } from '@/lib/urlState';

const PAGE_SIZES = [25, 50, 100];
const SOURCE_LABEL: Record<string, string> = { fleet: 'fleet', legacy: '旧面板' };
const SOURCE_OPTS = Object.entries(SOURCE_LABEL).map(([value, label]) => ({ value, label }));
const COLS = ['名称', 'IP', '供应商 · 套餐', '价格 / 周期', '上架 → 退役 → 回收', '原因', '流量', '来源', ''];
const WIDTHS = [160, 136, 140, 116, 232, 150, 80, 84, 36];
const day = (iso: string | null | undefined) => (iso ? F.dtFull(iso).slice(0, 10) : '—');
const traffic = (r: Pick<RecycleItem, 'traffic_up' | 'traffic_down'>) => (r.traffic_up == null && r.traffic_down == null ? null : (r.traffic_up ?? 0) + (r.traffic_down ?? 0));
const price = (r: Pick<RecycleItem, 'price_cents' | 'currency' | 'cycle'>) => (r.price_cents != null && r.currency ? `${F.money(r.price_cents, r.currency)}${r.cycle ? ` / ${cycleLabel(r.cycle)}` : ''}` : null);

/** 展开的一行：这条记录的关键字段 + 快照里挑出来的几项（快照形状不固定，没有就不显示） */
function Detail({ r }: { r: RecycleItem }) {
  const d = useRecycleDetail(r.id);
  const snap: RecycleDetail['snapshot'] = d.data?.snapshot ?? {};
  const note = typeof snap.note === 'string' && snap.note ? snap.note : null;
  const count = (k: string) => (Array.isArray(snap[k]) ? (snap[k] as unknown[]).length : null);
  const entries = count('entries'), renewals = count('renewals');
  const bytes = (n: number | null) => (n == null ? '—' : F.str(F.bytes(n)));
  return (
    <LabelValueGrid>
      <LabelValue label="退役前的名字">{r.name}</LabelValue>
      <LabelValue label="回收时的名字">{r.last_name ?? '—'}</LabelValue>
      <LabelValue label="团队">{r.team ?? '—'}</LabelValue>
      <LabelValue label="宿主机">{r.host_name ?? '—'}</LabelValue>
      <LabelValue label="服务编号">{r.service_ref ?? '—'}</LabelValue>
      <LabelValue label="到期日">{r.due_date ?? '—'}</LabelValue>
      <LabelValue label="回收者">{F.actor(r.actor)}</LabelValue>
      <LabelValue label="来源编号">{r.source_id}</LabelValue>
      <LabelValue label="业务流量 上行 / 下行">{`${bytes(r.traffic_up)} / ${bytes(r.traffic_down)}`}</LabelValue>
      <LabelValue label="网卡 收 / 发">{`${bytes(r.nic_rx)} / ${bytes(r.nic_tx)}`}</LabelValue>
      {entries != null && <LabelValue label="入口历史">{`${entries} 条`}</LabelValue>}
      {renewals != null && <LabelValue label="续费记录">{`${renewals} 次`}</LabelValue>}
      {note && <LabelValue label="备注" wrap>{note}</LabelValue>}
      {d.isError && <ErrorNote error={d.error} onRetry={() => void d.refetch()}>快照加载失败：</ErrorNote>}
    </LabelValueGrid>
  );
}

function Row({ r, open, onToggle }: { r: RecycleItem; open: boolean; onToggle: (id: number | null) => void }) {
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
        <td><span className="cell-main">{day(r.activated_at)} → {day(r.retired_at)}</span><span className="sub">回收 {day(r.recycled_at)}</span></td>
        <td>{r.reason ? <Trunc className="cell-main">{r.reason}</Trunc> : <span className="t-muted">—</span>}</td>
        <td className="r">{t == null || t === 0 ? <span className="t-muted">—</span> : <Num f={F.bytes(t)} />}</td>
        <td><Chip variant="neutral">{SOURCE_LABEL[r.source] ?? r.source}</Chip></td>
        <td className="chev">{open ? <ChevronDown size={14} /> : <ChevronRight size={14} />}</td>
      </tr>
      {open && <tr><td colSpan={COLS.length} style={{ background: 'var(--sunken)', whiteSpace: 'normal' }}><Detail r={r} /></td></tr>}
    </>
  );
}

export function RecycleBin({ sp, upd, tools }: { sp: URLSearchParams; upd: (patch: Record<string, string>) => void; tools: React.ReactNode }) {
  const text = sp.get('q') ?? '';
  const [input, setInput] = useUrlSearch(sp, upd);
  const source = sp.get('source') ?? '';
  const setSource = React.useCallback((v: string) => upd({ source: v }), [upd]);
  const [openId, setOpenId] = React.useState<number | null>(null);
  const [total, setTotal] = React.useState<number | undefined>(undefined);
  const pg = usePage({ total, sizes: PAGE_SIZES, size: 50, reset: [text, source] });
  const q = useRecycleBin({ q: text, source, offset: pg.offset, limit: pg.size });
  if (q.data && q.data.total !== total) setTotal(q.data.total);
  const rows = q.data?.items ?? [];
  const filtered = !!(text || source);
  const clear = () => { setInput(''); upd({ q: '', source: '' }); };

  return (
    <div className="stack">
      <PageHead title="机器" note={q.data ? `${q.data.total} 台` : undefined} tools={tools} />

      <div className="row" role="search" ref={pg.anchor}>
        <SearchInput value={input} onChange={(e) => setInput(e.target.value)} placeholder="搜索名称、IP、团队、供应商…" aria-label="搜索" className="w-full sm:w-[260px]" />
        <div className="w-[130px]"><Select value={source} onChange={setSource} allowEmpty="全部来源" placeholder="来源" options={SOURCE_OPTS} aria-label="来源" /></div>
        {filtered && <Button variant="quiet" size="md" onClick={clear}><X />清除筛选</Button>}
      </div>

      {q.isError && <ErrorNote error={q.error} onRetry={() => void q.refetch()} />}
      <Panel flush>
        <TableWrap>
          <table className="dt" style={{ minWidth: 1134 }}>
            <colgroup>{WIDTHS.map((w, i) => <col key={i} style={{ width: w }} />)}</colgroup>
            <thead><tr>{COLS.map((c, i) => <th key={i} className={c === '流量' ? 'r' : undefined}>{c}</th>)}</tr></thead>
            <tbody>{rows.map((r) => <Row key={r.id} r={r} open={r.id === openId} onToggle={setOpenId} />)}</tbody>
          </table>
        </TableWrap>
        {q.isLoading && <SkeletonRows rows={6} />}
        {!q.isLoading && !q.isError && rows.length === 0 && (
          <Empty icon={<SearchX size={22} />} text={filtered ? '没有符合条件的记录' : '回收站是空的'}>
            {filtered && <Button size="sm" onClick={clear}>清除筛选</Button>}
          </Empty>
        )}
        <Pagination pg={pg} unit="台" />
      </Panel>
    </div>
  );
}
