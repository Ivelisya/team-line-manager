// 团队与套餐 → 客户到期：按节点数向团队收费。批次由「计费名额」自动派生（同一天加入、同地区的算一批，从加入那天起每月同日续一期），
// 节点加入 / 离开 / 替换由中控每分钟对账自动记下，这里只看与修正：
// 选团队 → 客户页链接 → 顶部数字（当前计费节点、本月应收、下次续费）→ 批次时间轴（同客户页的组件，已到期的灰色；行菜单「改起算日」）
// → 计费节点列表（含待定的；改起算日 / 地区备注 / 结束 / 删除手动名额；添加手动计费节点）→ 月度应收（按月翻）与增减记录。
// 团队、「只看有效」、应收的月份写进地址（?team= / ?active=1 / ?month=）。
import * as React from 'react';
import { ChevronLeft, ChevronRight, ExternalLink, Link2, MoreHorizontal, Plus } from 'lucide-react';
import { useAddBillingSlot, useBillingEvents, useDelBillingSlot, usePatchBillingSlot, usePatchCustomerPage, useReceivable, useRotateCustomerPage, useTeamBatches, useTeams } from '@/lib/queries';
import type { BillingSlot, CustomerBatch, CustomerPage, TeamBatches } from '@/lib/types';
import { useUrlState } from '@/lib/urlState';
import { openDrawer } from '@/lib/drawer';
import { resetOnOpen, useHeld } from '@/lib/useHeld';
import * as F from '@/lib/format';
import { Button, IconButton } from '@/components/ui/button';
import { CheckRow, Field, InfoTip, Input, UnitInput } from '@/components/ui/form';
import { DateField } from '@/components/ui/date-field';
import { Select } from '@/components/ui/select';
import { DropdownMenu, DropdownMenuContent, DropdownMenuItem, DropdownMenuSeparator, DropdownMenuTrigger, Segmented, Tip } from '@/components/ui/overlays';
import { toast } from '@/components/ui/toast';
import { CopyButton, Empty, ErrorNote, Panel, Pill, TableWrap } from '@/components/app/primitives';
import { ConfirmDialog, FormDialog } from '@/components/app/dialogs';
import { BatchTimeline, TimelineLegend, type TimelineItem } from './BatchTimeline';

const toItem = (b: CustomerBatch): TimelineItem => ({ ...b, desc: b.summary });
const nameOf = (b: Pick<CustomerBatch, 'seq' | 'region'>) => `第${F.pad2(b.seq)}批 · ${b.region}`;
const DATE = /^\d{4}-\d{2}-\d{2}$/;
const digits = (s: string) => s.replace(/\D/g, '');
const md = (d: string) => d.slice(5, 10);
/** 名额在列表里的名字：节点名；手动名额「手动 · 备注」；空出的「空出的名额」 */
const slotName = (s: BillingSlot) => s.holder?.name ?? (s.manual ? `手动${s.note ? ` · ${s.note}` : ''}` : '空出的名额');
const monthLabel = (m: string) => `${m.slice(0, 4)}年${Number(m.slice(5, 7))}月`;
const shiftMonth = (m: string, n: number) => F.addMonths(`${m}-01`, n).slice(0, 7);
const PAIR_TIP = '节点离开（退役、换团队、进备用、设为不计费）后名额先空出 24 小时：这期间有节点加入就算替换，接过名额和起算日，计费不变；没有就算减少，算到离开那天。\n新加入的节点同样先待定 24 小时：有名额空出就算替换，否则算新增，从加入那天起算。';

export function CustomerTab() {
  const teams = useTeams();
  const [sp, upd] = useUrlState();
  // 备用团队不收钱，不列
  const options = React.useMemo(() => [...(teams.data ?? [])].filter((t) => !t.spare).sort((a, b) => a.sort_order - b.sort_order).map((t) => ({ value: t.id, label: t.name })), [teams.data]);
  const want = sp.get('team') ?? '';
  const teamId = options.some((o) => o.value === want) ? want : options[0]?.value ?? '';
  const onTeam = React.useCallback((v: string) => upd({ team: v, month: '' }), [upd]);
  const q = useTeamBatches(teamId);
  const d = q.data?.team.id === teamId ? q.data : undefined;
  return (
    <div className="stack">
      <div className="cxa-bar">
        <Select className="cxa-team" value={teamId} onChange={onTeam} options={options} placeholder="选择团队" aria-label="团队" />
        {d && <PageLink teamId={teamId} teamName={d.team.name} page={d.page} />}
      </div>
      {teams.isError && <ErrorNote error={teams.error} onRetry={() => void teams.refetch()} />}
      {q.isError && <ErrorNote error={q.error} onRetry={() => void q.refetch()} />}
      {!teamId && teams.data && <Empty text="还没有团队" />}
      {!d && !q.isError && teamId && <div className="skel is-float" style={{ height: 260 }} />}
      {d && <Board d={d} teamId={teamId} />}
    </div>
  );
}

function Board({ d, teamId }: { d: TeamBatches; teamId: string }) {
  const [sp, upd] = useUrlState();
  const month = /^\d{4}-\d{2}$/.test(sp.get('month') ?? '') ? sp.get('month')! : d.today.slice(0, 7);
  return (
    <>
      <Figures d={d} teamId={teamId} />
      <Timeline d={d} />
      <Slots d={d} teamId={teamId} />
      <div className="cols-2">
        <MonthPanel d={d} teamId={teamId} month={month} setMonth={(m) => upd({ month: m === d.today.slice(0, 7) ? '' : m })} />
        <EventsPanel teamId={teamId} />
      </div>
    </>
  );
}

/* ---------------------------------------------------------------- 顶部数字 */
function Figures({ d, teamId }: { d: TeamBatches; teamId: string }) {
  const cur = useReceivable(teamId, d.today.slice(0, 7));
  // 下次续费：有效批次按续费日排，最近两个日期；个数按这批里到那天还在计费的名额数
  const next = React.useMemo(() => {
    const by = new Map<string, number>();
    for (const b of d.batches) {
      if (b.state === 'expired') continue;
      const n = d.slots.filter((s) => s.batch_seq === b.seq && (!s.end_date || s.end_date > b.end_date)).length;
      if (n > 0) by.set(b.end_date, (by.get(b.end_date) ?? 0) + n);
    }
    return [...by.entries()].sort(([a], [b]) => (a < b ? -1 : 1)).slice(0, 2);
  }, [d]);
  const t = d.totals;
  const side = [t.pending > 0 && `待定 ${t.pending} 个`, t.vacated > 0 && `空出待定 ${t.vacated} 个`].filter(Boolean).join(' · ');
  return (
    <section className="cxa-figs" aria-label="计费概况">
      <div className="cxa-fig">
        <span className="cxa-fig-k">当前计费节点 <InfoTip text={PAIR_TIP} /></span>
        <span className="cxa-fig-v">{t.active}<small>个</small></span>
        <span className="cxa-fig-s">{side || ' '}</span>
      </div>
      <div className="cxa-fig">
        <span className="cxa-fig-k">本月应收 <InfoTip text={'这个月里开始的每一期按个数计，1 个节点 1 期 = 1 个节点月\n明细见下面「月度应收」'} /></span>
        <span className="cxa-fig-v">{cur.data ? cur.data.total : '—'}<small>个节点月</small></span>
        <span className="cxa-fig-s">{monthLabel(d.today.slice(0, 7))}</span>
      </div>
      <div className="cxa-fig">
        <span className="cxa-fig-k">下次续费</span>
        {next.length ? <>
          <span className="cxa-fig-v"><span className="num">{md(next[0][0])}</span><small>续 {next[0][1]} 个</small></span>
          <span className="cxa-fig-s">{next[1] ? <>之后 <span className="num">{md(next[1][0])}</span> 续 {next[1][1]} 个</> : ' '}</span>
        </> : <><span className="cxa-fig-v t-muted">—</span><span className="cxa-fig-s"> </span></>}
      </div>
    </section>
  );
}

/* ---------------------------------------------------------------- 批次时间轴 */
type SeqSeg = 'all' | 'active';
function Timeline({ d }: { d: TeamBatches }) {
  const [sp, upd] = useUrlState();
  const onlyActive = sp.get('active') === '1';
  const live = React.useMemo(() => d.batches.filter((b) => b.state !== 'expired'), [d]);
  const shown = onlyActive ? live : d.batches;
  const items = React.useMemo(() => shown.map(toItem), [shown]);
  const seg = React.useMemo(() => [{ value: 'all' as SeqSeg, label: '全部', count: d.batches.length }, { value: 'active' as SeqSeg, label: '只看有效', count: live.length }], [d.batches.length, live.length]);
  const onSeg = React.useCallback((v: SeqSeg) => upd({ active: v === 'active' ? '1' : '' }), [upd]);
  const [edit, setEdit] = React.useState<CustomerBatch | null>(null);
  const held = useHeld(edit?.key, edit);
  const actions = (it: TimelineItem) => {
    const b = d.batches.find((x) => x.key === it.key);
    if (!b) return null;
    return (
      <DropdownMenu>
        <DropdownMenuTrigger asChild><IconButton size="sm" label={`${nameOf(b)}的操作`} tip={false}><MoreHorizontal size={16} /></IconButton></DropdownMenuTrigger>
        <DropdownMenuContent align="end"><DropdownMenuItem onSelect={() => setEdit(b)}>改起算日…</DropdownMenuItem></DropdownMenuContent>
      </DropdownMenu>
    );
  };
  const slotsOf = (b: CustomerBatch) => d.slots.filter((s) => s.batch_seq === b.seq);
  return (
    <Panel title="批次" count={shown.length}
      sub={<InfoTip text={'同一天加入、同一地区的计费节点算一批，从起算日起每月同日续一期\n客户页只显示有效的批次（到期日不早于今天）'} />}
      tools={d.batches.length > 0 ? <Segmented sm label="显示哪些批次" value={onlyActive ? 'active' : 'all'} onChange={onSeg} items={seg} /> : undefined}>
      {d.batches.length === 0 && <Empty text="还没有计费节点" />}
      {d.batches.length > 0 && shown.length === 0 && <Empty text="没有有效的批次" />}
      {shown.length > 0 && (
        <div className="cx-board">
          <TimelineLegend expired={shown.some((b) => b.state === 'expired')} />
          <BatchTimeline items={items} today={d.today} actions={actions} />
        </div>
      )}
      {held && <StartDialog open={!!edit} onClose={() => setEdit(null)} title={`改起算日 · ${nameOf(held)}`} start={held.start_date} ids={slotsOf(held).map((s) => s.id)}
        desc={`这一批的 ${slotsOf(held).length} 个计费节点一起改`} />}
    </Panel>
  );
}

/* ---------------------------------------------------------------- 计费节点 */
function SlotStateText({ s }: { s: BillingSlot }) {
  if (s.state === 'ended') return <span className="t-muted">已结束 <span className="num">{s.end_date ? md(s.end_date) : ''}</span></span>;
  if (s.state === 'vacated') {
    const until = s.vacated_at ? F.dt(Date.parse(s.vacated_at) + 86_400_000) : '';
    return <span className="cxa-warn" data-tip="占用的节点离开了：到这个时间前有节点加入就算替换，否则算减少">空出待定{until && <>（至 <span className="num">{until}</span>）</>}</span>;
  }
  return <>在用</>;
}
function Slots({ d, teamId }: { d: TeamBatches; teamId: string }) {
  const [showEnded, setShowEnded] = React.useState(false);
  const [adding, setAdding] = React.useState(false);
  const [act, setAct] = React.useState<{ kind: 'start' | 'edit' | 'end' | 'del'; s: BillingSlot } | null>(null);
  const held = useHeld(act ? `${act.kind}:${act.s.id}` : null, act);
  const patch = usePatchBillingSlot(); const delSlot = useDelBillingSlot();
  const ended = d.slots.filter((s) => s.state === 'ended').length;
  const list = d.slots.filter((s) => showEnded || s.state !== 'ended');
  const regions = [...new Set(d.slots.map((s) => s.region))];
  const reopen = (s: BillingSlot) => patch.mutate({ id: s.id, end_date: null }, { onSuccess: () => toast.ok(`${slotName(s)} 已恢复计费`), onError: toast.err });
  return (
    <Panel title="计费节点" count={d.totals.active} flush
      sub={ended > 0 ? <CheckRow className="is-label" checked={showEnded} onChange={setShowEnded}>显示已结束（{ended}）</CheckRow> : undefined}
      tools={<Button onClick={() => setAdding(true)}><Plus />添加手动计费节点</Button>}>
      <TableWrap>
        <table className="dt" style={{ minWidth: 640 }}>
          <colgroup><col /><col style={{ width: 100 }} /><col style={{ width: 96 }} /><col style={{ width: 84 }} /><col style={{ width: 200 }} /><col style={{ width: 52 }} /></colgroup>
          <thead><tr><th>节点</th><th>地区</th><th>起算日</th><th>批次</th><th>状态</th><th /></tr></thead>
          <tbody>
            {list.map((s) => (
              <tr key={s.id} className={s.state === 'ended' ? 'is-retired' : undefined}>
                <td className="strong">{s.holder ? <button type="button" className="btn btn-link cxa-node" onClick={() => openDrawer(s.holder!.id)}>{s.holder.name}</button> : <span className={s.manual ? undefined : 't-sub'}>{slotName(s)}</span>}</td>
                <td>{s.region}</td>
                <td className="num">{s.start_date.slice(5)}</td>
                <td className="num">第{F.pad2(s.batch_seq)}批</td>
                <td><SlotStateText s={s} /></td>
                <td className="c">
                  <DropdownMenu>
                    <DropdownMenuTrigger asChild><IconButton size="sm" label={`${slotName(s)}的操作`} tip={false}><MoreHorizontal size={16} /></IconButton></DropdownMenuTrigger>
                    <DropdownMenuContent align="end">
                      <DropdownMenuItem onSelect={() => setAct({ kind: 'start', s })}>改起算日…</DropdownMenuItem>
                      <DropdownMenuItem onSelect={() => setAct({ kind: 'edit', s })}>改地区 / 备注…</DropdownMenuItem>
                      {s.state === 'ended'
                        ? s.manual && <DropdownMenuItem disabled={patch.isPending} onSelect={() => reopen(s)}>恢复计费</DropdownMenuItem>
                        : <DropdownMenuItem disabled={!s.manual && s.state !== 'vacated'} onSelect={() => setAct({ kind: 'end', s })}>{!s.manual && s.state !== 'vacated' ? '结束（节点还在团队里）' : '结束…'}</DropdownMenuItem>}
                      {s.manual && <><DropdownMenuSeparator /><DropdownMenuItem danger onSelect={() => setAct({ kind: 'del', s })}>删除…</DropdownMenuItem></>}
                    </DropdownMenuContent>
                  </DropdownMenu>
                </td>
              </tr>
            ))}
            {d.pending.length > 0 && (
              <tr className="cxa-group"><td colSpan={6}>待定 <span className="num">{d.pending.length}</span> 个 <InfoTip text={'刚加入的节点：24 小时内有节点离开就算替换（接过名额和起算日），否则算新增，从加入那天起算'} /></td></tr>
            )}
            {d.pending.map((p) => (
              <tr key={p.machine.id}>
                <td className="strong"><button type="button" className="btn btn-link cxa-node" onClick={() => openDrawer(p.machine.id)}>{p.machine.name}</button></td>
                <td className="t-muted">—</td>
                <td className="t-muted">—</td>
                <td className="t-muted">—</td>
                <td><span className="cxa-warn" data-tip={`${F.dtFull(p.seen_at)} 加入`}>待定（<span className="num">{F.dt(p.decide_at)}</span> 定）</span></td>
                <td />
              </tr>
            ))}
          </tbody>
        </table>
        {list.length === 0 && d.pending.length === 0 && <Empty text="还没有计费节点" />}
      </TableWrap>

      <AddSlotDialog open={adding} onClose={() => setAdding(false)} teamId={teamId} today={d.today} regions={regions} />
      {held?.kind === 'start' && <StartDialog open={act?.kind === 'start'} onClose={() => setAct(null)} title={`改起算日 · ${slotName(held.s)}`} start={held.s.start_date} ids={[held.s.id]} />}
      {held?.kind === 'edit' && <EditSlotDialog open={act?.kind === 'edit'} onClose={() => setAct(null)} s={held.s} regions={regions} />}
      {held?.kind === 'end' && <EndSlotDialog open={act?.kind === 'end'} onClose={() => setAct(null)} s={held.s} today={d.today} />}
      <ConfirmDialog open={act?.kind === 'del'} onOpenChange={(o) => !o && setAct(null)} danger confirmLabel="删除" title={`删除 · ${held ? slotName(held.s) : ''}`}
        description="录错了才删；删掉后它的计费记录一起去掉。不再收钱请用「结束」"
        onConfirm={async () => { await delSlot.mutateAsync(held!.s.id); toast.ok(`已删除${slotName(held!.s)}`); }} />
    </Panel>
  );
}

/** 改起算日：一个名额，或一批的全部名额（逐个改） */
const StartDialog = resetOnOpen(function StartDialog({ open, onClose, title, desc, start, ids }: { open: boolean; onClose: () => void; title: string; desc?: string; start: string; ids: string[] }) {
  const patch = usePatchBillingSlot();
  const [v, setV] = React.useState(start);
  const [busy, setBusy] = React.useState(false);
  const save = async () => {
    setBusy(true);
    try {
      for (const id of ids) await patch.mutateAsync({ id, start_date: v });
      toast.ok(`起算日已改为 ${v}`); onClose();
    } catch (e) { toast.err(e); } finally { setBusy(false); }
  };
  return (
    <FormDialog open={open} onOpenChange={(o) => !o && onClose()} title={title} desc={desc} submitLabel="保存" busy={busy} valid={DATE.test(v) && v !== start} dirty={v !== start} onSubmit={() => void save()}>
      <Field label="起算日" info="从这天起每月同日续一期；同一天、同一地区的算一批"><DateField value={v} onChange={setV} /></Field>
    </FormDialog>
  );
});

const EditSlotDialog = resetOnOpen(function EditSlotDialog({ open, onClose, s, regions }: { open: boolean; onClose: () => void; s: BillingSlot; regions: string[] }) {
  const patch = usePatchBillingSlot();
  const [region, setRegion] = React.useState(s.region);
  const [note, setNote] = React.useState(s.note);
  const listId = React.useId();
  const r = region.trim(); const n = note.trim();
  const dirty = r !== s.region || n !== s.note;
  const errR = r === '' ? '必填' : [...r].length > 20 ? '最多 20 个字' : undefined;
  const errN = [...n].length > 60 ? '最多 60 个字' : undefined;
  return (
    <FormDialog open={open} onOpenChange={(o) => !o && onClose()} title={`改地区 / 备注 · ${slotName(s)}`} submitLabel="保存" busy={patch.isPending} valid={dirty && !errR && !errN} dirty={dirty}
      onSubmit={() => patch.mutate({ id: s.id, region: r !== s.region ? r : undefined, note: n !== s.note ? n : undefined }, { onSuccess: () => { toast.ok('已保存'); onClose(); }, onError: toast.err })}>
      <Field label="地区" error={errR} info="客户页按地区分批、分行统计；节点的地区默认按它的国家"><Input list={listId} value={region} onChange={(e) => setRegion(e.target.value)} autoFocus /></Field>
      <Field label="备注（选填）" error={errN}><Input value={note} onChange={(e) => setNote(e.target.value)} placeholder="只在这里看，不上客户页" /></Field>
      <datalist id={listId}>{regions.map((x) => <option key={x} value={x} />)}</datalist>
    </FormDialog>
  );
});

/** 结束：手动名额（不再收这个节点的钱）或空出待定的名额（不等 24 小时，直接算减少）。默认算到今天 / 离开那天 */
const EndSlotDialog = resetOnOpen(function EndSlotDialog({ open, onClose, s, today }: { open: boolean; onClose: () => void; s: BillingSlot; today: string }) {
  const patch = usePatchBillingSlot();
  const bj = (iso: string) => new Date(Date.parse(iso) + 8 * 3600_000).toISOString().slice(0, 10);
  const [v, setV] = React.useState(s.state === 'vacated' && s.vacated_at ? bj(s.vacated_at) : today);
  return (
    <ConfirmDialog asForm open={open} onOpenChange={(o) => !o && onClose()} title={`结束计费 · ${slotName(s)}`} confirmLabel="结束" confirmDisabled={!DATE.test(v) || v <= s.start_date}
      description={s.state === 'vacated' ? '不等 24 小时，直接算减少' : undefined}
      onConfirm={async () => { await patch.mutateAsync({ id: s.id, end_date: v }); toast.ok(`${slotName(s)} 算到 ${v}`); }}>
      <Field label="算到哪天" info="这天之后开始的期不再计费；这天之前已经开始的期照常算" error={DATE.test(v) && v <= s.start_date ? '要晚于起算日' : undefined}>
        <DateField value={v} onChange={setV} min={F.addDays(s.start_date, 1)} />
      </Field>
    </ConfirmDialog>
  );
});

/** 添加手动计费节点：系统外的节点（如家宽），不随对账自动增减，只能手动结束 */
const AddSlotDialog = resetOnOpen(function AddSlotDialog({ open, onClose, teamId, today, regions }: { open: boolean; onClose: () => void; teamId: string; today: string; regions: string[] }) {
  const add = useAddBillingSlot();
  const [f, setF] = React.useState({ region: '', start: today, count: '1', note: '' });
  const listId = React.useId();
  const r = f.region.trim(); const n = f.note.trim();
  const errs = {
    region: [...r].length > 20 ? '最多 20 个字' : undefined,
    count: f.count !== '' && !(Number(f.count) >= 1 && Number(f.count) <= 99) ? '填 1–99' : undefined,
    note: [...n].length > 60 ? '最多 60 个字' : undefined,
  };
  const valid = r !== '' && DATE.test(f.start) && f.count !== '' && n !== '' && !Object.values(errs).some(Boolean);
  return (
    <FormDialog open={open} onOpenChange={(o) => !o && onClose()} title="添加手动计费节点" submitLabel="添加" busy={add.isPending} valid={valid} dirty={!!(f.region || f.note)}
      desc={<>不在面板里的节点 <InfoTip text="系统外的节点（如家宽）：不会随节点增减自动变化，不收钱了要手动结束" /></>}
      onSubmit={() => add.mutate({ teamId, region: r, start_date: f.start, count: Number(f.count), note: n }, { onSuccess: () => { toast.ok(`已添加 ${f.count} 个手动计费节点`); onClose(); }, onError: toast.err })}>
      <div className="form-grid">
        <Field label="地区" error={errs.region}><Input list={listId} value={f.region} onChange={(e) => setF({ ...f, region: e.target.value })} placeholder="如 墨西哥" autoFocus /></Field>
        <Field label="个数" error={errs.count}><UnitInput unit="个节点" width="full" inputMode="numeric" value={f.count} onChange={(e) => setF({ ...f, count: digits(e.target.value) })} /></Field>
        <Field label="起算日"><DateField value={f.start} onChange={(v) => setF({ ...f, start: v })} /></Field>
        <Field label="备注" error={errs.note}><Input value={f.note} onChange={(e) => setF({ ...f, note: e.target.value })} placeholder="如 墨西哥家宽" /></Field>
      </div>
      <datalist id={listId}>{regions.map((x) => <option key={x} value={x} />)}</datalist>
    </FormDialog>
  );
});

/* ---------------------------------------------------------------- 月度应收 */
function MonthPanel({ d, teamId, month, setMonth }: { d: TeamBatches; teamId: string; month: string; setMonth: (m: string) => void }) {
  const q = useReceivable(teamId, month);
  const r = q.data?.month === month ? q.data : undefined;
  const first = d.slots.reduce((m, s) => (s.start_date.slice(0, 7) < m ? s.start_date.slice(0, 7) : m), d.today.slice(0, 7));
  const last = shiftMonth(d.today.slice(0, 7), 1);
  return (
    <Panel title="月度应收" className="cxa-month"
      tools={<span className="cxa-mswitch">
        <IconButton size="sm" label="上个月" disabled={month <= first} onClick={() => setMonth(shiftMonth(month, -1))}><ChevronLeft size={16} /></IconButton>
        <span className="num cxa-mlabel">{monthLabel(month)}</span>
        <IconButton size="sm" label="下个月" disabled={month >= last} onClick={() => setMonth(shiftMonth(month, 1))}><ChevronRight size={16} /></IconButton>
      </span>}>
      {q.isError && <ErrorNote error={q.error} onRetry={() => void q.refetch()} />}
      {!r && !q.isError && <div className="skel" style={{ height: 120 }} />}
      {r && (
        <div className="cxa-rcv" data-stale={q.isPlaceholderData || undefined}>
          <div className="cxa-rcv-total">
            <span>{month === d.today.slice(0, 7) ? '本月应收' : `${Number(month.slice(5))}月应收`} <b className="num">{r.total}</b> 个节点月</span>
            <InfoTip text={'起点落在这个月的每一期按个数计：1 个节点 1 期 = 1 个节点月\n还没到的月份按现在的节点数预估'} />
          </div>
          {month <= d.today.slice(0, 7) && <div className="t-sub cxa-rcv-sub">月初 <span className="num">{r.start_count}</span> 个 · {month === d.today.slice(0, 7) ? '今天' : '月末'} <span className="num">{r.end_count}</span> 个</div>}
          {r.lines.length > 0
            ? <ul className="cxa-lines">{r.lines.map((l) => (
              <li key={`${l.batch_seq}-${l.period_start}`}>
                <span>第{F.pad2(l.batch_seq)}批 · {l.region}</span>
                <span className="t-sub num">本期 {md(l.period_start)} → {md(l.period_end)}</span>
                <span className="num cxa-lines-n">{l.count}个</span>
              </li>
            ))}</ul>
            : <div className="t-sub">这个月没有要收的期</div>}
          {r.events.length > 0 && <>
            <div className="cxa-sub-h">本月增减</div>
            <EventList events={r.events} />
          </>}
        </div>
      )}
    </Panel>
  );
}

/* ---------------------------------------------------------------- 增减记录 */
function EventList({ events }: { events: { id: string; at: string; text: string; kind: string }[] }) {
  return (
    <ul className="cxa-events">
      {events.map((e) => <li key={e.id} data-kind={e.kind}><span className="num t-sub" data-tip={F.dtFull(e.at)}>{F.dt(e.at)}</span><span>{e.text}</span></li>)}
    </ul>
  );
}
function EventsPanel({ teamId }: { teamId: string }) {
  const q = useBillingEvents(teamId);
  const list = React.useMemo(() => q.data?.pages.flat() ?? [], [q.data]);
  return (
    <Panel title="增减记录">
      {q.isError && <ErrorNote error={q.error} onRetry={() => void q.refetch()} />}
      {q.isLoading && <div className="skel" style={{ height: 120 }} />}
      {q.data && list.length === 0 && <Empty text="还没有记录" />}
      {list.length > 0 && <EventList events={list} />}
      {q.hasNextPage && <div className="audit-more"><Button size="sm" loading={q.isFetchingNextPage} onClick={() => void q.fetchNextPage()}>加载更早的</Button></div>}
    </Panel>
  );
}

/* ---------------------------------------------------------------- 客户页链接 */
function PageLink({ teamId, teamName, page }: { teamId: string; teamName: string; page: CustomerPage | null }) {
  const rotate = useRotateCustomerPage(); const patch = usePatchCustomerPage();
  const [askRotate, setAskRotate] = React.useState(false);
  const INFO = '给客户看的只读页面：只显示有效批次的到期时间和节点月，不含节点名、IP、供应商、价格\n客户打开不用登录；链接等同凭据，只发给这个团队';
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
