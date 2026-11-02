// 团队与套餐 → 计费台账：按节点数向团队收费。读者是不懂技术的人，界面上一律说大白话：「收费的节点」「N 个节点的月费」「每月 D 日续费」，
// 不出现「名额 / 批次 / 节点月 / 起算日」（代码里的标识符照旧）。服务端按「计费名额」派生批次：同一天开始收费、同地区的算一批，每月同日续一期；
// 节点加入 / 离开 / 替换由中控每分钟对账自动记下，这里只看与修正：
// 选团队 → 客户页链接 → 一句话总结 + 三张数字卡（现在收费的节点、这个月要收、下次续费）→ 续费日程（时间轴，同客户页的组件；行菜单「改开始收费的日子」）
// → 收费的节点（含新加入待确认的；改日子 / 地区备注 / 停止收费 / 删除手动节点；手动添加节点）→ 每月要收（按月翻）与增减记录。
// 团队、「只看在收费的」、应收的月份写进地址（?team= / ?active=1 / ?month=）。
import * as React from 'react';
import { CalendarClock, CalendarDays, ChevronLeft, ChevronRight, ExternalLink, Link2, MoreHorizontal, Plus, Server } from 'lucide-react';
import { useAddBillingSlot, useBillingEvents, useDelBillingSlot, usePatchBatch, usePatchBillingSlot, usePatchCustomerPage, useReceivable, useRotateCustomerPage, useTeamBatches, useTeams } from '@/lib/queries';
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
import { Chip, CopyButton, Empty, ErrorNote, Panel, Pill, Stat, TableWrap } from '@/components/app/primitives';
import { ConfirmDialog, FormDialog } from '@/components/app/dialogs';
import { BatchTimeline, TimelineLegend, cnDate, cnSpan, currentPeriod, daysBetween, regionLabel, renewDay, type TimelineItem } from './BatchTimeline';

const DATE = /^\d{4}-\d{2}-\d{2}$/;
const digits = (s: string) => s.replace(/\D/g, '');
const p2 = F.pad2;
/** 一组的叫法：「每月 1 日续费 · 美区」 */
const groupName = (b: Pick<CustomerBatch, 'start_date' | 'region'>) => `${renewDay(b.start_date).text}续费 · ${regionLabel(b.region)}`;
/** 节点在列表里的名字：节点名；手动添加的写备注；离开的「节点名（已离开）」或「节点名（已设为不收费）」；都没有的「离开的节点」 */
const slotName = (s: BillingSlot) => s.holder?.name ?? (s.manual ? s.note || '手动添加的节点' : s.last_holder_name ? `${s.last_holder_name}（${s.left_reason === 'exempt' ? '已设为不收费' : '已离开'}）` : '离开的节点');
const monthLabel = (m: string) => `${m.slice(0, 4)} 年 ${Number(m.slice(5, 7))} 月`;
const shiftMonth = (m: string, n: number) => F.addMonths(`${m}-01`, n).slice(0, 7);
/** 时刻（本机时区）写成「11 月 1 日 09:12」 */
const when = (iso: string) => { const d = new Date(iso); return `${d.getMonth() + 1} 月 ${d.getDate()} 日 ${F.hm(iso)}`; };
/** 停止收费的节点最后收费到哪天（同服务端 batches::billed_until）：起点早于结束日的最后一期，它的最后一天 */
function billedUntil(start: string, end: string) {
  let k = 0;
  while (F.addMonths(start, k + 1) < end) k++;
  return F.addDays(F.addMonths(start, k + 1), -1);
}
const PAIR_TIP = '节点离开后（退役、换团队、进备用、设为不收费），先等 24 小时：这期间有新节点加入，就由它补上，照常收费、不多收；没有的话，从离开那天起不再续费。\n新加入的节点也先等 24 小时：这期间有节点离开就算补位，否则算新增，从加入那天开始收费。';
const PENDING_TIP = '加入满 24 小时后确定：这期间有节点离开，它就补上那个位置，不多收；没有就算新增，从加入那天开始收费';
const MONTH_TIP = '1 个节点用 1 个月 = 1 份月费；这个月里到了续费日的每个节点各算 1 份\n还没到的续费日按现在的节点数估算';

export function CustomerTab() {
  const teams = useTeams();
  const [sp, upd] = useUrlState();
  // 备用团队不收钱，不列
  const options = React.useMemo(() => [...(teams.data ?? [])].filter((t) => !t.spare).sort((a, b) => a.sort_order - b.sort_order).map((t) => ({ value: t.id, label: t.name })), [teams.data]);
  const want = sp.get('team') ?? '';
  const teamId = options.some((o) => o.value === want) ? want : options[0]?.value ?? '';
  const onTeam = React.useCallback((v: string) => upd({ team: v, month: '' }), [upd]);
  const q = useTeamBatches(teamId);
  const d = q.data;
  // 换团队、新数据未到时先留着上一个团队的（调淡、不可点），不整页塌成骨架
  const stale = q.isPlaceholderData;
  return (
    <div className="stack">
      <div className="cxa-bar">
        <Select className="cxa-team" value={teamId} onChange={onTeam} options={options} placeholder="选择团队" aria-label="团队" />
        {d && <PageLink teamId={d.team.id} teamName={d.team.name} page={d.page} stale={stale} />}
      </div>
      {teams.isError && <ErrorNote error={teams.error} onRetry={() => void teams.refetch()} />}
      {q.isError && <ErrorNote error={q.error} onRetry={() => void q.refetch()} />}
      {!teamId && teams.data && <Empty text="还没有团队" />}
      {!d && !q.isError && teamId && <div className="skel is-float" style={{ height: 260 }} />}
      {d && <div className="stack" data-stale={stale} inert={stale || undefined}><Board d={d} teamId={d.team.id} /></div>}
    </div>
  );
}

function Board({ d, teamId }: { d: TeamBatches; teamId: string }) {
  const [sp, upd] = useUrlState();
  const month = /^\d{4}-\d{2}$/.test(sp.get('month') ?? '') ? sp.get('month')! : d.today.slice(0, 7);
  const events = useBillingEvents(teamId);
  // 还没初始化计费：整页收成一句提示
  if (!d.initialized) {
    return <Empty text="还没开始记账" hint={<>在中控上运行 <code>fleetctl billing init</code></>} />;
  }
  return (
    <>
      <Figures d={d} teamId={teamId} />
      <Timeline d={d} teamId={teamId} />
      <Slots d={d} teamId={teamId} />
      <div className="cols-2">
        <MonthPanel d={d} teamId={teamId} month={month} setMonth={(m) => upd({ month: m === d.today.slice(0, 7) ? '' : m })} />
        <EventsPanel q={events} />
      </div>
    </>
  );
}

/* ---------------------------------------------------------------- 一句话总结 + 顶部数字 */
function Figures({ d, teamId }: { d: TeamBatches; teamId: string }) {
  const thisMonth = d.today.slice(0, 7);
  const cur = useReceivable(teamId, thisMonth);
  // 下次续费：在收费的组按续费日排，最近两个日子；个数按这一组里到那天还要续的节点数
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
  const mon = `${Number(thisMonth.slice(5))} 月`;
  const day = (s: string) => cnDate(s, d.today);
  const side = [t.pending > 0 && `另有 ${t.pending} 个新加入`, t.vacated > 0 && `${t.vacated} 个等补位`].filter(Boolean).join(' · ');
  const decide = d.pending.length === 1 ? when(d.pending[0].decide_at) : null;
  return (
    <>
      <p className="cxa-lead">
        {t.active > 0 ? <>{d.team.name} 现在有 <b>{t.active} 个节点</b>在收费。</> : <>{d.team.name} 现在没有在收费的节点。</>}
        {cur.data && (cur.data.total > 0 ? <>{mon}要收 <b>{cur.data.total} 个节点的月费</b>。</> : <>{mon}没有要收的月费。</>)}
        {next.length > 0 && <>下次续费：{next.map(([s, n], i) => <React.Fragment key={s}>{i > 0 && '，'}<span><b>{day(s)}</b>续 {n} 个</span></React.Fragment>)}。</>}
        {t.pending > 0 && <> 另有 {t.pending} 个新节点刚加入，{decide ? `${decide} ` : '满 24 小时后'}确定是补位还是新增。</>}
        {t.vacated > 0 && <> {t.vacated} 个节点离开了，在等新节点补位。</>}
      </p>
      <section className="cxa-figs" aria-label="收费概况">
        <Stat tone="blue" icon={<Server />} label="现在收费的节点" info={PAIR_TIP} value={t.active} unit="个" caption={side || undefined} />
        <Stat tone="violet" icon={<CalendarDays />} label="这个月要收" info={MONTH_TIP}
          value={<span data-stale={cur.isPlaceholderData}>{cur.data ? cur.data.total : '—'}</span>} unit="个节点的月费" caption={monthLabel(thisMonth)} />
        <Stat tone="amber" icon={<CalendarClock />} label="下次续费"
          value={next.length ? <span className="num">{day(next[0][0])}</span> : <span className="t-muted">—</span>} unit={next.length ? `续 ${next[0][1]} 个` : undefined}
          caption={next[1] ? <>之后 <span className="num">{day(next[1][0])}</span>续 {next[1][1]} 个</> : undefined} />
      </section>
    </>
  );
}

/* ---------------------------------------------------------------- 续费日程（时间轴） */
type SeqSeg = 'all' | 'active';
function Timeline({ d, teamId }: { d: TeamBatches; teamId: string }) {
  const [sp, upd] = useUrlState();
  const onlyActive = sp.get('active') === '1';
  const live = React.useMemo(() => d.batches.filter((b) => b.state !== 'expired'), [d]);
  const shown = onlyActive ? live : d.batches;
  // ends：这一组的节点都停止收费了，本期结束后不再续费
  const items = React.useMemo<TimelineItem[]>(() => shown.map((b) => ({ ...b, ends: !d.slots.some((s) => s.batch_seq === b.seq && !s.end_date) })), [shown, d.slots]);
  const seg = React.useMemo(() => [{ value: 'all' as SeqSeg, label: '全部', count: d.batches.length }, { value: 'active' as SeqSeg, label: '在收费的', count: live.length }], [d.batches.length, live.length]);
  const onSeg = React.useCallback((v: SeqSeg) => upd({ active: v === 'active' ? '1' : '' }), [upd]);
  const [edit, setEdit] = React.useState<CustomerBatch | null>(null);
  const patchBatch = usePatchBatch();
  const held = useHeld(edit?.key, edit);
  const actions = (it: TimelineItem) => {
    const b = d.batches.find((x) => x.key === it.key);
    if (!b) return null;
    return (
      <DropdownMenu>
        <DropdownMenuTrigger asChild><IconButton size="sm" label={`「${groupName(b)}」这一组的操作`} tip={false}><MoreHorizontal size={16} /></IconButton></DropdownMenuTrigger>
        <DropdownMenuContent align="end"><DropdownMenuItem onSelect={() => setEdit(b)}>改开始收费的日子…</DropdownMenuItem></DropdownMenuContent>
      </DropdownMenu>
    );
  };
  const slotsOf = (b: CustomerBatch) => d.slots.filter((s) => s.batch_seq === b.seq);
  return (
    <Panel title="续费日程" count={shown.length}
      sub={<InfoTip text={'同一天开始收费、同一地区的节点放在一组，每月同一天一起续费\n客户页只显示还在收费的组'} />}
      tools={d.batches.length > 0 ? <Segmented sm label="显示哪些组" value={onlyActive ? 'active' : 'all'} onChange={onSeg} items={seg} /> : undefined}>
      {d.batches.length === 0 && <Empty text="还没有收费的节点" />}
      {d.batches.length > 0 && shown.length === 0 && <Empty text="没有在收费的组" />}
      {shown.length > 0 && (
        <div className="cx-board">
          <TimelineLegend expired={shown.some((b) => b.state === 'expired')} />
          <BatchTimeline items={items} today={d.today} actions={actions} />
        </div>
      )}
      {held && <StartDialog open={!!edit} onClose={() => setEdit(null)} title={`改开始收费的日子 · ${groupName(held)}`} start={held.start_date} today={d.today}
        save={(v) => patchBatch.mutateAsync({ teamId, key: held.key, start_date: v })} desc={`这一组（第${p2(held.seq)}批）的 ${slotsOf(held).length} 个节点一起改`} />}
    </Panel>
  );
}

/* ---------------------------------------------------------------- 收费的节点 */
/** 节点的本期与下次续费（同一组的各期相同）：收费中 / 等补位的是这一组的本期；停止收费的是它最后收费的那一期（起点早于结束日的最后一期）。
 *  next：下次续费日（本期终点），本期结束后不再续的为 null */
function slotTerm(s: BillingSlot, batches: CustomerBatch[]) {
  const ps = batches.find((b) => b.seq === s.batch_seq)?.periods.filter((p) => !s.end_date || p.start_date < s.end_date) ?? [];
  const p = ps.length ? currentPeriod(ps) : null;
  return { p, next: p && s.state !== 'ended' && (!s.end_date || s.end_date > p.end_date) ? p.end_date : null };
}

function SlotStateText({ s, today }: { s: BillingSlot; today: string }) {
  if (s.state === 'ended' && s.end_date) {
    const until = billedUntil(s.start_date, s.end_date);
    return <span className="t-muted">{until < today ? '已停止收费' : '不再续费'}（算到 <span className="num">{cnDate(until, today)}</span>）</span>;
  }
  if (s.state === 'vacated') {
    const until = s.vacated_at ? when(new Date(Date.parse(s.vacated_at) + 86_400_000).toISOString()) : '';
    return <span className="cxa-warn" data-tip="节点离开了。到这个时间前有新节点加入，就由它补上，照常收费、不多收；没有的话，从离开那天起不再续费">等补位{until && <>（至 <span className="num">{until}</span>）</>}</span>;
  }
  if (s.end_date) return <>收费中，算到 <span className="num">{cnDate(billedUntil(s.start_date, s.end_date), today)}</span></>;
  return <>收费中</>;
}

/** 开始收费的日子；悬停写每月几号续费、是哪一批 */
function StartCell({ s, today }: { s: BillingSlot; today: string }) {
  const r = renewDay(s.start_date);
  return <span data-tip={[`以后${r.text}续费 · 第${p2(s.batch_seq)}批`, r.tip].filter(Boolean).join('\n')}>{cnDate(s.start_date, today)}</span>;
}

/** 还有几天：3 天以内用提示色 */
const Days = ({ n }: { n: number }) => <span className={n <= 3 ? 'cxa-warn' : undefined}>{n} 天</span>;

function Slots({ d, teamId }: { d: TeamBatches; teamId: string }) {
  const [showEnded, setShowEnded] = React.useState(false);
  const [adding, setAdding] = React.useState(false);
  const [act, setAct] = React.useState<{ kind: 'start' | 'edit' | 'end' | 'del'; s: BillingSlot } | null>(null);
  const held = useHeld(act ? `${act.kind}:${act.s.id}` : null, act);
  const patch = usePatchBillingSlot(); const delSlot = useDelBillingSlot();
  const ended = d.slots.filter((s) => s.state === 'ended').length;
  // 按下次续费日从近到远排，同一天按名字；不再续费的、已停止的排在后面
  const list = React.useMemo(() => d.slots.filter((s) => showEnded || s.state !== 'ended').map((s) => ({ s, ...slotTerm(s, d.batches) }))
    .sort((a, b) => (a.s.state === 'ended' ? 1 : 0) - (b.s.state === 'ended' ? 1 : 0) || (a.next ?? '9').localeCompare(b.next ?? '9') || slotName(a.s).localeCompare(slotName(b.s), 'zh-CN', { numeric: true })), [d, showEnded]);
  const regions = [...new Set(d.slots.map((s) => s.region))];
  const reopen = (s: BillingSlot) => patch.mutate({ id: s.id, end_date: null }, { onSuccess: () => toast.ok(`${slotName(s)} 已恢复收费`), onError: toast.err });
  return (
    <Panel title="收费的节点" count={d.totals.active} flush
      sub={ended > 0 ? <CheckRow className="is-label" checked={showEnded} onChange={setShowEnded}>显示已停止收费的（{ended}）</CheckRow> : undefined}
      tools={<Button size="sm" onClick={() => setAdding(true)}><Plus />手动添加节点</Button>}>
      <TableWrap>
        <table className="dt cxa-slots">
          <colgroup><col /><col style={{ width: 88 }} /><col style={{ width: 104 }} /><col style={{ width: 184 }} /><col style={{ width: 104 }} /><col style={{ width: 76 }} /><col style={{ width: 216 }} /><col style={{ width: 52 }} /></colgroup>
          <thead><tr><th>节点</th><th>地区</th><th>开始收费</th><th>本期</th><th>下次续费</th><th className="r">还有几天</th><th>状态</th><th /></tr></thead>
          <tbody>
            {list.map(({ s, p, next }) => (
              <tr key={s.id} className={s.state === 'ended' ? 'is-retired' : undefined}>
                <td className="strong">
                  {s.holder
                    ? <button type="button" className="btn btn-link cxa-node" onClick={() => openDrawer(s.holder!.id)}>{s.holder.name}</button>
                    : <span className={s.manual ? undefined : 't-sub'}>{slotName(s)}</span>}
                  {s.manual && <Chip variant="neutral" className="cxa-manual" tip="不在面板里的节点，不会自动增减，不收钱了要手动停止">手动添加</Chip>}
                </td>
                <td>{regionLabel(s.region)}</td>
                <td className="num"><StartCell s={s} today={d.today} /></td>
                <td className="num">{p ? cnSpan(p.start_date, p.end_date, d.today) : <span className="t-muted">—</span>}</td>
                <td className="num">{next ? cnDate(next, d.today) : <span className="t-muted">{s.state === 'ended' ? '—' : '不再续费'}</span>}</td>
                <td className="num r">{next ? <Days n={daysBetween(d.today, next)} /> : <span className="t-muted">—</span>}</td>
                <td><SlotStateText s={s} today={d.today} /></td>
                <td className="c">
                  <DropdownMenu>
                    <DropdownMenuTrigger asChild><IconButton size="sm" label={`${slotName(s)}的操作`} tip={false}><MoreHorizontal size={16} /></IconButton></DropdownMenuTrigger>
                    <DropdownMenuContent align="end">
                      <DropdownMenuItem onSelect={() => setAct({ kind: 'start', s })}>改开始收费的日子…</DropdownMenuItem>
                      <DropdownMenuItem onSelect={() => setAct({ kind: 'edit', s })}>改地区 / 备注…</DropdownMenuItem>
                      {s.state === 'ended'
                        ? s.manual && <DropdownMenuItem disabled={patch.isPending} onSelect={() => reopen(s)}>恢复收费</DropdownMenuItem>
                        : <DropdownMenuItem disabled={!s.manual && s.state !== 'vacated'} onSelect={() => setAct({ kind: 'end', s })}>{!s.manual && s.state !== 'vacated' ? '停止收费（节点还在团队里）' : '停止收费…'}</DropdownMenuItem>}
                      {s.manual && <><DropdownMenuSeparator /><DropdownMenuItem danger onSelect={() => setAct({ kind: 'del', s })}>删除…</DropdownMenuItem></>}
                    </DropdownMenuContent>
                  </DropdownMenu>
                </td>
              </tr>
            ))}
            {d.pending.length > 0 && (
              <tr className="cxa-group"><td colSpan={8}>新加入的节点 <span className="num">{d.pending.length}</span> 个 <InfoTip text={PENDING_TIP} /></td></tr>
            )}
            {d.pending.map((p) => (
              <tr key={p.machine.id}>
                <td className="strong"><button type="button" className="btn btn-link cxa-node" onClick={() => openDrawer(p.machine.id)}>{p.machine.name}</button></td>
                <td colSpan={6}><span className="cxa-warn" data-tip={`${when(p.seen_at)} 加入`}>新加入，<span className="num">{when(p.decide_at)}</span> 确定是补位还是新增</span></td>
                <td />
              </tr>
            ))}
          </tbody>
        </table>
        {list.length === 0 && d.pending.length === 0 && <Empty text="还没有收费的节点" />}
      </TableWrap>

      <AddSlotDialog open={adding} onClose={() => setAdding(false)} teamId={teamId} today={d.today} regions={regions} />
      {held?.kind === 'start' && <StartDialog open={act?.kind === 'start'} onClose={() => setAct(null)} title={`改开始收费的日子 · ${slotName(held.s)}`} start={held.s.start_date} today={d.today} save={(v) => patch.mutateAsync({ id: held.s.id, start_date: v })} />}
      {held?.kind === 'edit' && <EditSlotDialog open={act?.kind === 'edit'} onClose={() => setAct(null)} s={held.s} regions={regions} />}
      {held?.kind === 'end' && <EndSlotDialog open={act?.kind === 'end'} onClose={() => setAct(null)} s={held.s} today={d.today} />}
      <ConfirmDialog open={act?.kind === 'del'} onOpenChange={(o) => !o && setAct(null)} danger confirmLabel="删除" title={`删除 · ${held ? slotName(held.s) : ''}`}
        description="录错了才删，删掉后它的收费记录一起去掉。不想再收钱请用「停止收费」"
        onConfirm={async () => { await delSlot.mutateAsync(held!.s.id); toast.ok(`已删除${slotName(held!.s)}`); }} />
    </Panel>
  );
}

/** 改开始收费的日子：一个节点，或一整组（save 由调用方给，整组是服务端一个事务）；失败时显示服务端的原话 */
const StartDialog = resetOnOpen(function StartDialog({ open, onClose, title, desc, start, today, save: apply }: { open: boolean; onClose: () => void; title: string; desc?: string; start: string; today: string; save: (v: string) => Promise<unknown> }) {
  const [v, setV] = React.useState(start);
  const [busy, setBusy] = React.useState(false);
  const save = async () => {
    setBusy(true);
    try {
      await apply(v);
      toast.ok(`开始收费的日子已改为 ${cnDate(v, today)}`); onClose();
    } catch (e) { toast.err(e); } finally { setBusy(false); }
  };
  return (
    <FormDialog open={open} onOpenChange={(o) => !o && onClose()} title={title} desc={desc} submitLabel="保存" busy={busy} valid={DATE.test(v) && v !== start} dirty={v !== start} onSubmit={() => void save()}>
      <Field label="开始收费的日子" info="以后每月这一天续费（没有这一天的月份在月底续）；日子和地区都一样的节点归为一组"
        hint={DATE.test(v) ? `以后${renewDay(v).text}续费` : undefined}><DateField value={v} onChange={setV} /></Field>
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
      <Field label="地区" error={errR} info="客户页按地区分别统计；默认按节点所在的国家，不知道国家的显示「未标地区」"><Input list={listId} value={region} onChange={(e) => setRegion(e.target.value)} autoFocus /></Field>
      <Field label="备注（选填）" error={errN}><Input value={note} onChange={(e) => setNote(e.target.value)} placeholder="只在这里看，客户看不到" /></Field>
      <datalist id={listId}>{regions.map((x) => <option key={x} value={x} />)}</datalist>
    </FormDialog>
  );
});

/** 停止收费：手动添加的节点（不再收它的钱）或等补位的节点（不等 24 小时，直接算离开）。默认从今天 / 离开那天起不再续费 */
const EndSlotDialog = resetOnOpen(function EndSlotDialog({ open, onClose, s, today }: { open: boolean; onClose: () => void; s: BillingSlot; today: string }) {
  const patch = usePatchBillingSlot();
  const bj = (iso: string) => new Date(Date.parse(iso) + 8 * 3600_000).toISOString().slice(0, 10);
  const [v, setV] = React.useState(s.state === 'vacated' && s.vacated_at ? bj(s.vacated_at) : today);
  const ok = DATE.test(v) && v > s.start_date;
  return (
    <ConfirmDialog asForm open={open} onOpenChange={(o) => !o && onClose()} title={`停止收费 · ${slotName(s)}`} confirmLabel="停止收费" confirmDisabled={!ok}
      description={s.state === 'vacated' ? '不等 24 小时，现在就算它离开了' : undefined}
      onConfirm={async () => { await patch.mutateAsync({ id: s.id, end_date: v }); toast.ok(`${slotName(s)} 收费到 ${cnDate(billedUntil(s.start_date, v), today)}`); }}>
      <Field label="从哪天起不再续费" info="这天和以后的续费日都不再收；这天之前已经续上的那个月照常收"
        error={DATE.test(v) && !ok ? `要晚于开始收费的日子（${cnDate(s.start_date, today)}）` : undefined}
        hint={ok ? `会收费到 ${cnDate(billedUntil(s.start_date, v), today)}` : undefined}>
        <DateField value={v} onChange={setV} min={F.addDays(s.start_date, 1)} />
      </Field>
    </ConfirmDialog>
  );
});

/** 手动添加节点：系统外的节点（如家宽），不随对账自动增减，只能手动停止收费 */
const AddSlotDialog = resetOnOpen(function AddSlotDialog({ open, onClose, teamId, today, regions }: { open: boolean; onClose: () => void; teamId: string; today: string; regions: string[] }) {
  const add = useAddBillingSlot();
  const [f, setF] = React.useState({ region: '', start: today, count: '1', note: '' });
  const listId = React.useId();
  const r = f.region.trim(); const n = f.note.trim();
  const errs = {
    region: r === '' ? '必填' : [...r].length > 20 ? '最多 20 个字' : undefined,
    count: f.count !== '' && !(Number(f.count) >= 1 && Number(f.count) <= 99) ? '填 1–99' : undefined,
    note: n === '' ? '必填' : [...n].length > 60 ? '最多 60 个字' : undefined,
  };
  const valid = DATE.test(f.start) && f.count !== '' && !Object.values(errs).some(Boolean);
  return (
    <FormDialog open={open} onOpenChange={(o) => !o && onClose()} title="手动添加节点" submitLabel="添加" busy={add.isPending} valid={valid} dirty={!!(f.region || f.note) || f.start !== today || f.count !== '1'}
      desc={<>不在面板里、但要收钱的节点 <InfoTip text="比如家宽。不会随节点增减自动变化，不收钱了要手动停止收费" /></>}
      onSubmit={() => add.mutate({ teamId, region: r, start_date: f.start, count: Number(f.count), note: n }, { onSuccess: () => { toast.ok(`已添加 ${f.count} 个手动节点`); onClose(); }, onError: toast.err })}>
      <div className="form-grid">
        <Field label="地区" error={errs.region}><Input list={listId} value={f.region} onChange={(e) => setF({ ...f, region: e.target.value })} placeholder="如 墨西哥" autoFocus /></Field>
        <Field label="个数" error={errs.count}><UnitInput unit="个节点" width="full" inputMode="numeric" value={f.count} onChange={(e) => setF({ ...f, count: digits(e.target.value) })} /></Field>
        <Field label="开始收费的日子" hint={DATE.test(f.start) ? `以后${renewDay(f.start).text}续费` : undefined}><DateField value={f.start} onChange={(v) => setF({ ...f, start: v })} /></Field>
        <Field label="备注" error={errs.note}><Input value={f.note} onChange={(e) => setF({ ...f, note: e.target.value })} placeholder="如 墨西哥家宽" /></Field>
      </div>
      <datalist id={listId}>{regions.map((x) => <option key={x} value={x} />)}</datalist>
    </FormDialog>
  );
});

/* ---------------------------------------------------------------- 每月要收 */
function MonthPanel({ d, teamId, month, setMonth }: { d: TeamBatches; teamId: string; month: string; setMonth: (m: string) => void }) {
  const q = useReceivable(teamId, month);
  const r = q.data; // 翻月时 r 是上一个月的（placeholder，调淡），文字都按 r.month
  const first = d.slots.reduce((m, s) => (s.start_date.slice(0, 7) < m ? s.start_date.slice(0, 7) : m), d.today.slice(0, 7));
  const last = shiftMonth(d.today.slice(0, 7), 1);
  const isCur = r?.month === d.today.slice(0, 7);
  const day = (s: string) => cnDate(s, d.today);
  return (
    <Panel title="每月要收" className="cxa-month"
      tools={<span className="cxa-mswitch">
        <IconButton size="sm" label="上个月" disabled={month <= first} onClick={() => setMonth(shiftMonth(month, -1))}><ChevronLeft size={16} /></IconButton>
        <span className="num cxa-mlabel">{monthLabel(month)}</span>
        <IconButton size="sm" label="下个月" disabled={month >= last} onClick={() => setMonth(shiftMonth(month, 1))}><ChevronRight size={16} /></IconButton>
      </span>}>
      {q.isError && <ErrorNote error={q.error} onRetry={() => void q.refetch()} />}
      {!r && !q.isError && <div className="skel" style={{ height: 120 }} />}
      {r && (
        <div className="cxa-rcv" data-stale={q.isPlaceholderData} inert={q.isPlaceholderData || undefined}>
          <div className="cxa-rcv-total">
            <span>{isCur ? '这个月' : `${Number(r.month.slice(5))} 月`}要收 <b className="num">{r.total}</b> 个节点的月费</span>
            <InfoTip text={MONTH_TIP} />
          </div>
          {/* 以后的月份没有月初 / 月末个数：行留着占高度，翻月不跳 */}
          <div className="t-sub cxa-rcv-sub">{r.start_count != null ? <>月初 <span className="num">{r.start_count}</span> 个节点 · {isCur ? '现在' : '月末'} <span className="num">{r.end_count}</span> 个节点</> : ' '}</div>
          {r.lines.length > 0
            ? <ul className="cxa-lines">{r.lines.map((l) => {
              const b = d.batches.find((x) => x.seq === l.batch_seq);
              return (
                <li key={`${l.batch_seq}-${l.period_start}`}>
                  <span><span className="num">{day(l.period_start)}</span>{b?.start_date === l.period_start ? '开始收费' : '续费'} · {regionLabel(l.region)}</span>
                  <span className="t-sub num">{l.projected ? '预计（还没到续费日）' : `用到 ${day(F.addDays(l.period_end, -1))}`}</span>
                  <span className="num cxa-lines-n">{l.count} 个</span>
                </li>
              );
            })}</ul>
            : <div className="t-sub">这个月没有到续费日的节点</div>}
          {r.events.length > 0 && <>
            <div className="cxa-sub-h">这个月的增减</div>
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
      {events.map((e) => <li key={e.id}><span className="num t-sub" data-tip={F.dtFull(e.at)}>{when(e.at)}</span><span>{e.text}</span></li>)}
    </ul>
  );
}
function EventsPanel({ q }: { q: ReturnType<typeof useBillingEvents> }) {
  const list = React.useMemo(() => q.data?.pages.flat() ?? [], [q.data]);
  return (
    <Panel title="增减记录" sub={<InfoTip text="节点加入、离开、补位和手动修改，都记在这里" />}>
      {q.isError && <ErrorNote error={q.error} onRetry={() => void q.refetch()} />}
      {q.isLoading && <div className="skel" style={{ height: 120 }} />}
      <div data-stale={q.isPlaceholderData} inert={q.isPlaceholderData || undefined}>
        {q.data && list.length === 0 && <Empty text="还没有记录" />}
        {list.length > 0 && <EventList events={list} />}
        {q.hasNextPage && <div className="audit-more"><Button size="sm" loading={q.isFetchingNextPage} onClick={() => void q.fetchNextPage()}>加载更早的</Button></div>}
      </div>
    </Panel>
  );
}

/* ---------------------------------------------------------------- 客户页链接 */
function PageLink({ teamId, teamName, page, stale }: { teamId: string; teamName: string; page: CustomerPage | null; stale: boolean }) {
  const rotate = useRotateCustomerPage(); const patch = usePatchCustomerPage();
  const [askRotate, setAskRotate] = React.useState(false);
  const INFO = '给客户看的页面：列出这个团队在收费的每个节点（名字、地区、本期、下次续费日），不含 IP、供应商、价格\n客户打开不用登录；拿到链接的人都能看，只发给这个团队';
  if (!page) {
    return (
      <span className="cxa-link" data-stale={stale} inert={stale || undefined}>
        <Button size="lg" loading={rotate.isPending} onClick={() => rotate.mutate(teamId, { onSuccess: () => toast.ok('已生成客户页链接'), onError: toast.err })}><Link2 />生成客户页链接</Button>
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
    <span className="cxa-link" data-off={page.enabled ? undefined : ''} data-stale={stale} inert={stale || undefined}>
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
      <span className="t-sub cxa-seen" data-tip={page.last_view_at ? `客户上次打开：${F.dtFull(page.last_view_at)}` : undefined}>{page.last_view_at ? `客户 ${F.ago(page.last_view_at)}打开过` : '客户还没打开过'}</span>
      <InfoTip text={INFO} />
      <ConfirmDialog open={askRotate} onOpenChange={setAskRotate} title={`换新链接 · ${teamName}`} confirmLabel="换新链接"
        description="旧链接马上失效，要把新链接重新发给客户"
        onConfirm={async () => { await rotate.mutateAsync(teamId); toast.ok('已换新链接，旧链接已失效'); }} />
    </span>
  );
}
