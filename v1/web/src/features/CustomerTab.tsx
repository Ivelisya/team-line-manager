// 团队与套餐 → 客户到期：维护各团队的客户批次账（与供应商订阅无关），并管理给客户看的只读链接（/c/:token）。
// 选团队 → 客户页链接（复制、打开、换新链接、停用 / 启用；没有时生成）→ 批次时间轴（同客户页的组件，这里多显示已到期的，灰色）。
// 每个批次「⋯」：续期、编辑（地区 / 编号 / 说明 / 各段）、删除；右上「新建批次」。团队、「只看有效」写进地址（?team= / ?active=1）。
import * as React from 'react';
import { ExternalLink, Link2, MoreHorizontal, Plus, Trash2 } from 'lucide-react';
import { ApiError } from '@/lib/api';
import { useAddBatch, useDelBatch, useDelBatchPeriod, usePatchBatch, usePatchBatchPeriod, usePatchCustomerPage, useRenewCustomerBatch, useRotateCustomerPage, useTeamBatches, useTeams } from '@/lib/queries';
import type { CustomerBatch, CustomerPage } from '@/lib/types';
import { useUrlState } from '@/lib/urlState';
import { resetOnOpen, useHeld } from '@/lib/useHeld';
import * as F from '@/lib/format';
import { Button, IconButton } from '@/components/ui/button';
import { Field, InfoTip, Input, UnitInput } from '@/components/ui/form';
import { DateField } from '@/components/ui/date-field';
import { Select } from '@/components/ui/select';
import { DropdownMenu, DropdownMenuContent, DropdownMenuItem, DropdownMenuSeparator, DropdownMenuTrigger, Segmented, Tip } from '@/components/ui/overlays';
import { toast } from '@/components/ui/toast';
import { CopyButton, Empty, ErrorNote, Panel, Pill } from '@/components/app/primitives';
import { ConfirmDialog, FormDialog } from '@/components/app/dialogs';
import { BatchTimeline, TimelineLegend, type TimelineItem } from './BatchTimeline';

const descOf = (b: CustomerBatch) => [b.summary, b.note].filter(Boolean).join('；');
const toItem = (b: CustomerBatch): TimelineItem => ({ ...b, key: b.id, desc: descOf(b) });
/** 「第05批 · 美区」：对话框标题用 */
const nameOf = (b: Pick<CustomerBatch, 'seq' | 'region'>) => `第${F.pad2(b.seq)}批 · ${b.region}`;
const DATE = /^\d{4}-\d{2}-\d{2}$/;
/** 只留数字（个数、月数、编号的输入框） */
const digits = (s: string) => s.replace(/\D/g, '');
const inRange = (s: string, lo: number, hi: number) => s !== '' && Number(s) >= lo && Number(s) <= hi;
/** 到期日的默认值：开始日期加 months 个日历月，月底对齐（1/31 + 1 个月 = 2/28），同服务端；只做输入框里的预填，不改时不传、由服务端算 */
const autoEnd = (start: string, months: string) => (DATE.test(start) && inRange(months, 1, 36) ? F.addMonths(start, Number(months)) : '');

export function CustomerTab() {
  const teams = useTeams();
  const [sp, upd] = useUrlState();
  // 备用团队不是客户，不列
  const options = React.useMemo(() => [...(teams.data ?? [])].filter((t) => !t.spare).sort((a, b) => a.sort_order - b.sort_order).map((t) => ({ value: t.id, label: t.name })), [teams.data]);
  const want = sp.get('team') ?? '';
  const teamId = options.some((o) => o.value === want) ? want : options[0]?.value ?? '';
  const onTeam = React.useCallback((v: string) => upd({ team: v }), [upd]);
  const onlyActive = sp.get('active') === '1';
  const q = useTeamBatches(teamId);
  const d = q.data?.team.id === teamId ? q.data : undefined;
  const { all, live } = React.useMemo(() => { const a = d?.batches ?? []; return { all: a, live: a.filter((b) => b.state !== 'expired') }; }, [d]);
  const shown = onlyActive ? live : all;
  const items = React.useMemo(() => shown.map(toItem), [shown]);
  const byId = React.useMemo(() => new Map(all.map((b) => [b.id, b])), [all]);
  const seg = React.useMemo(() => [{ value: 'all' as const, label: '全部', count: all.length }, { value: 'active' as const, label: '只看有效', count: live.length }], [all.length, live.length]);
  const onSeg = React.useCallback((v: 'all' | 'active') => upd({ active: v === 'active' ? '1' : '' }), [upd]);

  const [adding, setAdding] = React.useState(false);
  const [renew, setRenew] = React.useState<CustomerBatch | null>(null);
  const [edit, setEdit] = React.useState<CustomerBatch | null>(null);
  const [askDel, setAskDel] = React.useState<CustomerBatch | null>(null);
  const renewHeld = useHeld(renew?.id, renew); const editHeld = useHeld(edit?.id, edit); const delHeld = useHeld(askDel?.id, askDel);
  const delBatch = useDelBatch();

  const actions = (it: TimelineItem) => {
    const b = byId.get(it.key);
    if (!b) return null;
    return (
      <DropdownMenu>
        <DropdownMenuTrigger asChild><IconButton size="sm" label={`${nameOf(b)}的操作`} tip={false}><MoreHorizontal size={16} /></IconButton></DropdownMenuTrigger>
        <DropdownMenuContent align="end">
          <DropdownMenuItem onSelect={() => setRenew(b)}>续期…</DropdownMenuItem>
          <DropdownMenuItem onSelect={() => setEdit(b)}>编辑…</DropdownMenuItem>
          <DropdownMenuSeparator />
          <DropdownMenuItem danger onSelect={() => setAskDel(b)}>删除…</DropdownMenuItem>
        </DropdownMenuContent>
      </DropdownMenu>
    );
  };
  const nodeMonths = live.reduce((s, b) => s + b.node_months, 0);

  return (
    <div className="stack">
      <div className="cxa-bar">
        <Select className="cxa-team" value={teamId} onChange={onTeam} options={options} placeholder="选择团队" aria-label="团队" />
        {d && <PageLink teamId={teamId} teamName={d.team.name} page={d.page} />}
      </div>
      {teams.isError && <ErrorNote error={teams.error} onRetry={() => void teams.refetch()} />}
      {q.isError && <ErrorNote error={q.error} onRetry={() => void q.refetch()} />}
      <Panel title="批次" count={d ? shown.length : undefined}
        sub={d && live.length > 0 && <span className="t-sub">有效 <span className="num">{live.length}</span> 批 · <span className="num">{nodeMonths}</span> 个节点月 <InfoTip text={'节点月 = 个数 × 月数，含已经续期的未来部分\n客户页只显示有效的批次（到期日不早于今天）'} /></span>}
        tools={<>{all.length > 0 && <Segmented sm label="显示哪些批次" value={onlyActive ? 'active' : 'all'} onChange={onSeg} items={seg} />}<Button variant="primary" disabled={!teamId} onClick={() => setAdding(true)}><Plus />新建批次</Button></>}>
        {!d && !q.isError && teamId && <div className="skel" style={{ height: 220 }} />}
        {!teamId && teams.data && <Empty text="还没有团队" />}
        {d && all.length === 0 && <Empty text="还没有批次"><Button variant="primary" onClick={() => setAdding(true)}><Plus />新建批次</Button></Empty>}
        {d && all.length > 0 && shown.length === 0 && <Empty text="没有有效的批次" />}
        {d && shown.length > 0 && (
          <div className="cx-board">
            <TimelineLegend expired={shown.some((b) => b.state === 'expired')} />
            <BatchTimeline items={items} today={d.today} actions={actions} />
          </div>
        )}
      </Panel>

      {d && <NewBatchDialog open={adding} onClose={() => setAdding(false)} teamId={teamId} today={d.today} regions={[...new Set(all.map((b) => b.region))]} nextSeq={Math.max(0, ...all.map((b) => b.seq)) + 1} />}
      {renewHeld && <RenewDialog open={!!renew} onClose={() => setRenew(null)} b={renewHeld} />}
      {editHeld && <EditDialog open={!!edit} onClose={() => setEdit(null)} b={editHeld} />}
      <ConfirmDialog open={!!askDel} onOpenChange={(o) => !o && setAskDel(null)} danger confirmLabel="删除批次" title={`删除 · ${delHeld ? nameOf(delHeld) : ''}`}
        description={<>连同 <span className="num">{delHeld?.periods.length}</span> 段记录一起删除，不能恢复</>}
        onConfirm={async () => { await delBatch.mutateAsync(askDel!.id); toast.ok(`已删除${nameOf(askDel!)}`); }} />
    </div>
  );
}

/* ---------------------------------------------------------------- 客户页链接 */
function PageLink({ teamId, teamName, page }: { teamId: string; teamName: string; page: CustomerPage | null }) {
  const rotate = useRotateCustomerPage(); const patch = usePatchCustomerPage();
  const [askRotate, setAskRotate] = React.useState(false);
  const INFO = '给客户看的只读页面：只显示有效批次的到期时间和节点月，不含 IP、供应商、价格\n客户打开不用登录；链接等同凭据，只发给这个团队的客户';
  if (!page) {
    return (
      <span className="cxa-link">
        <Button loading={rotate.isPending} onClick={() => rotate.mutate(teamId, { onSuccess: () => toast.ok('已生成客户页链接'), onError: toast.err })}><Link2 />生成客户页链接</Button>
        <InfoTip text={INFO} />
      </span>
    );
  }
  const setEnabled = (enabled: boolean) => patch.mutate({ teamId, enabled }, {
    onError: toast.err,
    onSuccess: () => (enabled ? toast.ok('客户页已启用')
      : toast.show({ tone: 'ok', text: '客户页已停用，客户打开会看到「链接无效或已停用」', action: { label: '撤销', onClick: () => patch.mutate({ teamId, enabled: true }, { onSuccess: () => toast.ok('客户页已启用'), onError: toast.err }) } })),
  });
  return (
    <span className="cxa-link" data-off={page.enabled ? undefined : ''}>
      <span className="t-sub">客户页</span>
      <a className="cxa-url ellipsis" href={page.link} target="_blank" rel="noreferrer noopener" data-tip={page.link}>{page.link.replace(/^https?:\/\//, '')}</a>
      {!page.enabled && <Pill tone="neutral">已停用</Pill>}
      <CopyButton text={page.link} label="客户页链接" />
      <Tip text="打开客户页"><a className="icon-btn icon-btn-xs" href={page.link} target="_blank" rel="noreferrer noopener" aria-label="打开客户页"><ExternalLink size={14} /></a></Tip>
      <DropdownMenu>
        <DropdownMenuTrigger asChild><IconButton size="sm" label="客户页链接的操作" tip={false}><MoreHorizontal size={16} /></IconButton></DropdownMenuTrigger>
        <DropdownMenuContent align="end">
          <DropdownMenuItem onSelect={() => setAskRotate(true)}>换新链接…</DropdownMenuItem>
          <DropdownMenuItem disabled={patch.isPending} onSelect={() => setEnabled(!page.enabled)}>{page.enabled ? '停用' : '启用'}</DropdownMenuItem>
        </DropdownMenuContent>
      </DropdownMenu>
      <span className="t-sub cxa-seen" data-tip={page.last_view_at ? `客户上次打开：${F.dtFull(page.last_view_at)}` : undefined}>{page.last_view_at ? `${F.ago(page.last_view_at)}打开过` : '客户还没打开过'}</span>
      <InfoTip text={INFO} />
      <ConfirmDialog open={askRotate} onOpenChange={setAskRotate} title={`换新链接 · ${teamName}`} confirmLabel="换新链接"
        description="旧链接立即失效，要把新链接重新发给客户"
        onConfirm={async () => { await rotate.mutateAsync(teamId); toast.ok('已换新链接，旧链接已失效'); }} />
    </span>
  );
}

/* ---------------------------------------------------------------- 新建批次 */
const NewBatchDialog = resetOnOpen(function NewBatchDialog({ open, onClose, teamId, today, regions, nextSeq }: { open: boolean; onClose: () => void; teamId: string; today: string; regions: string[]; nextSeq: number }) {
  const add = useAddBatch();
  const [f, setF] = React.useState({ region: '', seq: '', start: today, months: '1', count: '', note: '' });
  const [end, setEnd] = React.useState<string | null>(null); // null = 按月数推算
  const auto = autoEnd(f.start, f.months);
  const endShown = end ?? auto;
  const listId = React.useId();
  const errs = {
    seq: f.seq !== '' && !inRange(f.seq, 1, 999) ? '填 1–999' : undefined,
    months: f.months !== '' && !inRange(f.months, 1, 36) ? '填 1–36' : undefined,
    count: f.count !== '' && !inRange(f.count, 1, 999) ? '填 1–999' : undefined,
    end: endShown && f.start && endShown <= f.start ? '要晚于开始日期' : undefined,
    region: [...f.region.trim()].length > 20 ? '最多 20 个字' : undefined,
    note: [...f.note.trim()].length > 60 ? '最多 60 个字' : undefined,
  };
  const valid = f.region.trim() !== '' && DATE.test(f.start) && inRange(f.months, 1, 36) && inRange(f.count, 1, 999) && DATE.test(endShown) && !Object.values(errs).some(Boolean);
  const save = () => add.mutate({
    teamId, region: f.region.trim(), start_date: f.start, months: Number(f.months), count: Number(f.count),
    end_date: end != null && end !== auto ? end : undefined, note: f.note.trim() || undefined, seq: f.seq ? Number(f.seq) : undefined,
  }, { onSuccess: (b) => { toast.ok(`已新建${nameOf(b)}`); onClose(); }, onError: toast.err });
  return (
    <FormDialog size="md" open={open} onOpenChange={(o) => !o && onClose()} title="新建批次" submitLabel="新建" busy={add.isPending} valid={valid}
      dirty={!!(f.region || f.count || f.note || f.seq)} onSubmit={save}>
      <div className="form-grid">
        <Field label="地区" error={errs.region}><Input list={listId} value={f.region} onChange={(e) => setF({ ...f, region: e.target.value })} placeholder="如 美区" autoFocus /></Field>
        <Field label="编号" error={errs.seq}><UnitInput unit="批" width="full" inputMode="numeric" value={f.seq} onChange={(e) => setF({ ...f, seq: digits(e.target.value) })} placeholder={`默认 ${F.pad2(nextSeq)}`} /></Field>
        <Field label="开始日期"><DateField value={f.start} onChange={(v) => setF({ ...f, start: v })} /></Field>
        <Field label="月数" error={errs.months}><UnitInput unit="个月" width="full" inputMode="numeric" value={f.months} onChange={(e) => setF({ ...f, months: digits(e.target.value) })} /></Field>
        <Field label="个数" error={errs.count}><UnitInput unit="个节点" width="full" inputMode="numeric" value={f.count} onChange={(e) => setF({ ...f, count: digits(e.target.value) })} placeholder="如 5" /></Field>
        <Field label="到期日" info="默认是开始日期加月数（月底对齐，1/31 加 1 个月是 2/28），可以改" error={errs.end}
          hint={end != null && end !== auto && auto ? <button type="button" className="btn btn-link btn-sm cxa-reset" onClick={() => setEnd(null)}>改回 {auto}</button> : undefined}>
          <DateField value={endShown} onChange={setEnd} min={f.start ? F.addDays(f.start, 1) : undefined} />
        </Field>
        <Field label="说明（选填）" className="span-2" error={errs.note}><Input value={f.note} onChange={(e) => setF({ ...f, note: e.target.value })} placeholder="客户页标题下一行，如「另1个归团队1」" /></Field>
      </div>
      <datalist id={listId}>{regions.map((r) => <option key={r} value={r} />)}</datalist>
    </FormDialog>
  );
});

/* ---------------------------------------------------------------- 续期 */
const RenewDialog = resetOnOpen(function RenewDialog({ open, onClose, b }: { open: boolean; onClose: () => void; b: CustomerBatch }) {
  const renew = useRenewCustomerBatch();
  const [months, setMonths] = React.useState('1');
  const [count, setCount] = React.useState(String(b.count));
  const [end, setEnd] = React.useState<string | null>(null);
  const auto = autoEnd(b.end_date, months);
  const endShown = end ?? auto;
  const errs = {
    months: months !== '' && !inRange(months, 1, 36) ? '填 1–36' : undefined,
    count: count !== '' && !inRange(count, 1, 999) ? '填 1–999' : undefined,
    end: endShown && endShown <= b.end_date ? `要晚于 ${b.end_date}` : undefined,
  };
  const valid = inRange(months, 1, 36) && inRange(count, 1, 999) && DATE.test(endShown) && !Object.values(errs).some(Boolean);
  const save = () => renew.mutate({ id: b.id, months: Number(months), count: Number(count) !== b.count ? Number(count) : undefined, end_date: end != null && end !== auto ? end : undefined },
    { onSuccess: (r) => { toast.ok(`${nameOf(r)} 已续期到 ${r.end_date}`); onClose(); }, onError: toast.err });
  return (
    <FormDialog open={open} onOpenChange={(o) => !o && onClose()} title={`续期 · ${nameOf(b)}`} submitLabel="续期" busy={renew.isPending} valid={valid}
      desc={<>从 <span className="num">{b.end_date}</span> 接着续，当前 <span className="num">{b.count}</span> 个</>} onSubmit={save}>
      <div className="form-grid">
        <Field label="月数" error={errs.months}><UnitInput unit="个月" width="full" inputMode="numeric" value={months} onChange={(e) => setMonths(digits(e.target.value))} autoFocus /></Field>
        <Field label="个数" error={errs.count}><UnitInput unit="个节点" width="full" inputMode="numeric" value={count} onChange={(e) => setCount(digits(e.target.value))} /></Field>
        <Field label="新的到期日" className="span-2" info="默认是当前到期日加月数（月底对齐），可以改" error={errs.end}
          hint={end != null && end !== auto && auto ? <button type="button" className="btn btn-link btn-sm cxa-reset" onClick={() => setEnd(null)}>改回 {auto}</button> : undefined}>
          <DateField value={endShown} onChange={setEnd} min={F.addDays(b.end_date, 1)} />
        </Field>
      </div>
    </FormDialog>
  );
});

/* ---------------------------------------------------------------- 编辑：地区 / 编号 / 说明 + 各段 */
type Row = { id: string; start_date: string; end_date: string; months: string; count: string };
const rowOf = (p: CustomerBatch['periods'][number]): Row => ({ id: p.id, start_date: p.start_date, end_date: p.end_date, months: String(p.months), count: String(p.count) });
const EditDialog = resetOnOpen(function EditDialog({ open, onClose, b }: { open: boolean; onClose: () => void; b: CustomerBatch }) {
  const patchBatch = usePatchBatch(); const patchPeriod = usePatchBatchPeriod(); const delPeriod = useDelBatchPeriod();
  const [f, setF] = React.useState({ region: b.region, seq: String(b.seq), note: b.note });
  const [rows, setRows] = React.useState<Row[]>(() => b.periods.map(rowOf));
  const [busy, setBusy] = React.useState(false);
  // 已经存进去的样子：保存到一半失败时，下次保存只补没成功的（删过的段不再删、改过的段按新的基准比）
  const base = React.useRef({ head: { region: b.region, seq: String(b.seq), note: b.note }, rows: new Map(b.periods.map((p) => [p.id, rowOf(p)])) });
  const setRow = (id: string, patch: Partial<Row>) => setRows((rs) => rs.map((r) => (r.id === id ? { ...r, ...patch } : r)));
  const rowErr = (r: Row) => (!(DATE.test(r.start_date) && DATE.test(r.end_date)) ? '日期不全' : r.end_date <= r.start_date ? '到期日要晚于开始日期' : !inRange(r.months, 1, 36) ? '月数填 1–36' : !inRange(r.count, 1, 999) ? '个数填 1–999' : null);
  const errs = {
    region: f.region.trim() === '' ? '必填' : [...f.region.trim()].length > 20 ? '最多 20 个字' : undefined,
    seq: !inRange(f.seq, 1, 999) ? '填 1–999' : undefined,
    note: [...f.note.trim()].length > 60 ? '最多 60 个字' : undefined,
  };
  const bad = rows.map(rowErr).find(Boolean);
  const valid = !Object.values(errs).some(Boolean) && !bad && rows.length > 0;
  const dirtyRows = () => rows.flatMap((r) => {
    const o = base.current.rows.get(r.id)!;
    const ch: { id: string; start_date?: string; end_date?: string; months?: number; count?: number } = { id: r.id };
    if (r.start_date !== o.start_date) ch.start_date = r.start_date;
    if (r.end_date !== o.end_date) ch.end_date = r.end_date;
    if (r.months !== o.months) ch.months = Number(r.months);
    if (r.count !== o.count) ch.count = Number(r.count);
    return Object.keys(ch).length > 1 ? [{ ch, r }] : [];
  });
  const removed = () => [...base.current.rows.keys()].filter((id) => !rows.some((r) => r.id === id));
  const h = base.current.head;
  const headDirty = f.region.trim() !== h.region || f.seq !== h.seq || f.note.trim() !== h.note;
  const dirty = headDirty || removed().length > 0 || dirtyRows().length > 0;

  const save = async () => {
    setBusy(true);
    try {
      for (const id of removed()) { await delPeriod.mutateAsync(id); base.current.rows.delete(id); }
      // 改日期时前后两段可能要按某个先后才不重叠：撞了 409 的先放着，别的改完再试；一轮下来一个都没成才报错
      let pending = dirtyRows();
      while (pending.length) {
        const left: typeof pending = []; let last: unknown = null;
        for (const x of pending) {
          try { await patchPeriod.mutateAsync(x.ch); base.current.rows.set(x.r.id, { ...x.r }); } catch (e) {
            if (e instanceof ApiError && e.status === 409) { left.push(x); last = e; } else throw e;
          }
        }
        if (left.length === pending.length) throw last;
        pending = left;
      }
      if (headDirty) {
        await patchBatch.mutateAsync({ id: b.id, region: f.region.trim() !== h.region ? f.region.trim() : undefined, seq: f.seq !== h.seq ? Number(f.seq) : undefined, note: f.note.trim() !== h.note ? f.note.trim() : undefined });
        base.current.head = { region: f.region.trim(), seq: f.seq, note: f.note.trim() };
      }
      toast.ok(`已保存${nameOf({ seq: Number(f.seq), region: f.region.trim() })}`);
      onClose();
    } catch (e) { toast.err(e); } finally { setBusy(false); }
  };

  return (
    <FormDialog size="md" open={open} onOpenChange={(o) => !o && onClose()} title={`编辑 · ${nameOf(b)}`} submitLabel="保存" busy={busy} valid={valid && dirty} dirty={dirty} onSubmit={() => void save()}>
      <div className="form-grid">
        <Field label="地区" error={errs.region}><Input value={f.region} onChange={(e) => setF({ ...f, region: e.target.value })} /></Field>
        <Field label="编号" error={errs.seq}><UnitInput unit="批" width="full" inputMode="numeric" value={f.seq} onChange={(e) => setF({ ...f, seq: digits(e.target.value) })} /></Field>
        <Field label="说明（选填）" className="span-2" error={errs.note}><Input value={f.note} onChange={(e) => setF({ ...f, note: e.target.value })} placeholder="客户页标题下一行" /></Field>
      </div>
      <div className="cxa-periods" role="table" aria-label="各段">
        <div className="cxa-prow is-head" role="row"><span role="columnheader">段</span><span role="columnheader">开始</span><span role="columnheader">到期</span><span role="columnheader">月数</span><span role="columnheader">个数</span><span /></div>
        {rows.map((r, i) => (
          <div key={r.id} className="cxa-prow" role="row">
            <span className="t-sub" role="cell">{i === 0 ? '首购' : `续期${i}`}</span>
            <span role="cell"><DateField value={r.start_date} onChange={(v) => setRow(r.id, { start_date: v })} aria-label={`第${i + 1}段开始日期`} /></span>
            <span role="cell"><DateField value={r.end_date} onChange={(v) => setRow(r.id, { end_date: v })} min={DATE.test(r.start_date) ? F.addDays(r.start_date, 1) : undefined} aria-label={`第${i + 1}段到期日`} /></span>
            <span role="cell"><UnitInput unit="月" width="full" inputMode="numeric" value={r.months} onChange={(e) => setRow(r.id, { months: digits(e.target.value) })} aria-label={`第${i + 1}段月数`} /></span>
            <span role="cell"><UnitInput unit="个" width="full" inputMode="numeric" value={r.count} onChange={(e) => setRow(r.id, { count: digits(e.target.value) })} aria-label={`第${i + 1}段个数`} /></span>
            <span role="cell"><IconButton size="sm" label={rows.length === 1 ? '只剩这一段，要删就删整个批次' : '删掉这一段'} disabled={rows.length === 1} onClick={() => setRows((rs) => rs.filter((x) => x.id !== r.id))}><Trash2 size={14} /></IconButton></span>
          </div>
        ))}
        {bad && <div className="hint is-error">{bad}</div>}
      </div>
    </FormDialog>
  );
});
