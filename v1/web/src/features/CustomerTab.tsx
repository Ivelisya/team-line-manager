// 团队台账：按月汇总待支付节点；选中月份的全部期次在同一张清单中维护。
// 改开始收费日、地区备注、停止收费、删除收在每行的「⋯」里，手动添加节点在月份旁的「⋯」里。团队、月份写进地址（?team= / ?month=）。
import { Skeleton } from '@/components/ui/skeleton';
import * as React from 'react';
import { Check, ExternalLink, Link2, MoreHorizontal } from 'lucide-react';
import { BILLING_EVENT_PAGE_SIZE, useAddBillingSlot, useBillingEvents, useBillingMonth, useDelBillingSlot, useMarkPaid, useOutstanding, usePatchBillingSlot, usePatchCustomerPage, useRotateCustomerPage, useTeams } from '@/lib/queries';
import type { BillingMonth, BillingMonthNode, CustomerPage } from '@/lib/types';
import { useUrlState } from '@/lib/urlState';
import { compareBillingEffectiveDate } from '@/lib/sort';
import { openDrawer } from '@/lib/drawer';
import { resetOnOpen, useHeld } from '@/lib/useHeld';
import * as F from '@/lib/format';
import { Button, IconButton } from '@/components/ui/button';
import { FormField, UnitInput } from '@/components/ui/form';
import { InfoTip } from '@/components/ui/info-tip';
import { Input } from '@/components/ui/input';
import { DateField } from '@/components/ui/date-field';
import { DropdownMenu, DropdownMenuContent, DropdownMenuItem, DropdownMenuSeparator, DropdownMenuTrigger } from '@/components/ui/dropdown-menu';
import { toast } from '@/components/ui/toast';
import { CopyButton, Empty, ErrorNote, Panel, Pill } from '@/components/app/primitives';
import { CursorPager } from '@/components/app/Pagination';
import { ConfirmDialog, FormDialog } from '@/components/app/dialogs';
import { dropAfterMenuClosed, menuClosed, menuReturnFocus, runAfterMenuClosed } from '@/components/app/AnchoredDropdown';
import { BillingNodeRow, MonthSummary, MonthSwitch, OutstandingHeading, PeriodLegend, cnDate, cnMonth, cnSpan, regionLabel } from './billing';

const DATE = /^\d{4}-\d{2}-\d{2}$/;
const digits = (s: string) => s.replace(/\D/g, '');
/** 时刻（本机时区）写成「11 月 1 日 09:12」 */
const when = (iso: string) => { const d = new Date(iso); return `${d.getMonth() + 1} 月 ${d.getDate()} 日 ${F.hm(iso)}`; };
/** 停止收费的节点最后收到哪天（同服务端 batches::billed_until）：起点早于停止日的最后一期，它的最后一天 */
function billedUntil(start: string, end: string) {
  let k = 0;
  while (F.addMonths(start, k + 1) < end) k++;
  return F.addDays(F.addMonths(start, k + 1), -1);
}
/** 地区输入框里的值 → 存进库的值：留空或写「未标地区」都存「未知」（库里的口径不变，只是给人看的写法统一） */
const storedRegion = (r: string) => (r === '' || r === '未标地区' ? '未知' : r);

export function CustomerTab() {
  const teams = useTeams();
  const [sp, upd] = useUrlState();
  // 备用团队不收钱，不列
  const options = React.useMemo(() => [...(teams.data ?? [])].filter((t) => !t.spare).sort((a, b) => a.sort_order - b.sort_order).map((t) => ({ value: t.id, label: t.name })), [teams.data]);
  const want = sp.get('team') ?? '';
  const teamId = options.some((o) => o.value === want) ? want : options[0]?.value ?? '';
  const month = /^\d{4}-\d{2}$/.test(sp.get('month') ?? '') ? sp.get('month')! : '';
  const onTeam = React.useCallback((v: string) => upd({ team: v, month: '' }), [upd]);
  const q = useBillingMonth(teamId, month);
  const d = q.data;
  // 换团队、翻月、新数据未到时先留着上一份（调淡、不可点），不整页塌成骨架
  const stale = q.isPlaceholderData;
  const selectMonth = (target: string) => {
    upd({ month: target === d?.today.slice(0, 7) ? '' : target });
  };
  return (
    <div className="stack billing-ledger">
      <div className="bm-team-choices" role="group" aria-label="选择团队">
        {options.map(team => <button key={team.value} type="button" className="bm-team-choice"
          aria-pressed={team.value === teamId} onClick={() => onTeam(team.value)}>{team.label}</button>)}
      </div>
      {d && <div className="cxa-share"><PageLink teamId={d.team.id} teamName={d.team.name} page={d.page} stale={stale} /></div>}
      {teams.isError && <ErrorNote error={teams.error} onRetry={() => void teams.refetch()} />}
      {q.isError && <ErrorNote error={q.error} onRetry={() => void q.refetch()} />}
      {!teamId && teams.data && <Empty text="还没有团队" />}
      {!d && !q.isError && teamId && <Skeleton className="rounded-float h-[260px]" />}
      {teamId && d?.initialized && <OutstandingPanel key={teamId} teamId={teamId} month={month} onSelect={selectMonth} />}
      {d && <div role="region" aria-label="月份明细" className="stack" data-stale={stale} inert={stale || undefined}><Board key={d.team.id} d={d} /></div>}
    </div>
  );
}

function OutstandingPanel({ teamId, month, onSelect }: { teamId: string; month: string; onSelect: (month: string) => void }) {
  const q = useOutstanding(teamId);
  const counts = new Map<string, number>();
  for (const node of q.data?.nodes ?? []) {
    const month = node.period_start.slice(0, 7);
    counts.set(month, (counts.get(month) ?? 0) + 1);
  }
  const selected = month || q.data?.today.slice(0, 7);
  return <section className="panel bm-outstanding" aria-label="待支付">
    <OutstandingHeading />
    {q.isLoading && <Skeleton className="h-[56px]" />}
    {q.isError && <ErrorNote error={q.error} onRetry={() => void q.refetch()} />}
    {q.data && (counts.size > 0
      ? <MonthSummary counts={counts} selected={selected} today={q.data.today} onSelect={onSelect} />
      : <Empty text="暂无待支付节点" />)}
  </section>;
}

function Board({ d }: { d: BillingMonth }) {
  // 还没开始记账：整页收成一句提示
  if (!d.initialized) return <Empty text="还没开始记账" hint={<>在中控上运行 <code>fleetctl billing init</code></>} />;
  return (
    <>
      <MonthList key={d.month} d={d} />
      <EventsPanel teamId={d.team.id} />
    </>
  );
}

/** 全部标记已支付的确认框里最多列几个名字，其余悬停看 */
const ALL_NAMES = 6;
type Act = { kind: 'start' | 'edit' | 'end' | 'del'; n: BillingMonthNode };

function MonthList({ d }: { d: BillingMonth }) {
  const [, upd] = useUrlState();
  const [adding, setAdding] = React.useState(false);
  const regions = [...new Set(d.nodes.map(n => regionLabel(n.region)))];
  const mark = useMarkPaid();
  const patch = usePatchBillingSlot(); const delSlot = useDelBillingSlot();
  // 全部标记已支付：打开确认框时的名单（确认的就是框里列出的这些，中途刷新进来的新节点不会被一起标）
  const [askAll, setAskAll] = React.useState<BillingMonthNode[] | null>(null);
  const heldAll = useHeld(askAll ? 'all' : null, askAll);
  // 正在提交的节点（按 slot 记：连点两行时前一行的加载状态不会被后一次覆盖）
  const [loading, setLoading] = React.useState<string[]>([]);
  const [act, setAct] = React.useState<Act | null>(null);
  const held = useHeld(act ? `${act.kind}:${act.n.slot_id}` : null, act);
  const m = cnMonth(d.month, d.today);
  const due = d.nodes.filter((n) => !n.paid && !n.upcoming);
  const monthPopup = React.useRef<HTMLDivElement | null>(null);
  const rows = [...d.nodes].sort(compareBillingEffectiveDate);
  const teamId = d.team.id;
  const set = async (ns: BillingMonthNode[], on: boolean) => {
    const ids = ns.map((n) => n.slot_id);
    setLoading((l) => [...l, ...ids]);
    try { return await mark.mutateAsync({ teamId, month: d.month, slot_ids: ids, paid: on }); } finally { setLoading((l) => l.filter((x) => !ids.includes(x))); }
  };
  // 撤销只撤自己这次改的：点的时候这个节点本来就是目标状态（别人刚标过）就是空操作，不给撤销，免得把别人标的改回去
  const one = (n: BillingMonthNode, on: boolean) => set([n], on).then(
    () => toast.show({ tone: 'ok', text: `${n.name} ${m}${on ? '已支付' : '改回待支付'}`, action: n.paid !== on ? { label: '撤销', onClick: () => void set([n], !on).catch(toast.err) } : undefined }),
    toast.err,
  );
  const busy = (n: BillingMonthNode) => loading.includes(n.slot_id);
  const reopen = (n: BillingMonthNode) => patch.mutate({ id: n.slot_id, end_date: null }, { onSuccess: () => toast.ok(`${n.name} 已恢复收费`), onError: toast.err });

  const menu = (n: BillingMonthNode) => (
    <DropdownMenu>
      <DropdownMenuTrigger render={<IconButton size="icon-sm" label={`${n.name}的更多操作`} tip={false}><MoreHorizontal size={16} /></IconButton>} />
      <DropdownMenuContent align="end">
        {n.paid && <><DropdownMenuItem disabled={mark.isPending} onClick={() => void one(n, false)}>改回待支付</DropdownMenuItem><DropdownMenuSeparator /></>}
        <DropdownMenuItem onClick={() => setAct({ kind: 'start', n })}>改开始收费日…</DropdownMenuItem>
        <DropdownMenuItem onClick={() => setAct({ kind: 'edit', n })}>改地区 / 备注…</DropdownMenuItem>
        {n.manual && n.ended && <DropdownMenuItem disabled={patch.isPending} onClick={() => reopen(n)}>恢复收费</DropdownMenuItem>}
        {n.manual && !n.ended && <DropdownMenuItem onClick={() => setAct({ kind: 'end', n })}>停止收费…</DropdownMenuItem>}
        {n.manual && <><DropdownMenuSeparator /><DropdownMenuItem danger onClick={() => setAct({ kind: 'del', n })}>删除…</DropdownMenuItem></>}
      </DropdownMenuContent>
    </DropdownMenu>
  );

  return (
    <>
      <Panel className="bm-month-panel" flush
        title={<MonthSwitch month={d.month} months={d.months} onChange={month => upd({ month: month === d.today.slice(0, 7) ? '' : month })} />}
        tools={<DropdownMenu onOpenChange={(o) => { if (o) dropAfterMenuClosed(); }} onOpenChangeComplete={menuClosed}>
          <DropdownMenuTrigger render={<IconButton size="icon-sm" label="月份操作"><MoreHorizontal size={16} /></IconButton>} />
          <DropdownMenuContent ref={monthPopup} align="end" finalFocus={() => menuReturnFocus(monthPopup)}>
            <DropdownMenuItem onClick={() => runAfterMenuClosed(() => setAdding(true))}>手动添加节点…</DropdownMenuItem>
            {due.length > 1 && <DropdownMenuItem onClick={() => runAfterMenuClosed(() => setAskAll(due))}>待支付节点全部标记已支付…</DropdownMenuItem>}
          </DropdownMenuContent>
        </DropdownMenu>}>
        {rows.length ? <>
          <PeriodLegend />
          <ul className="bm-list" aria-label={`${m}节点清单`}>
            {rows.map(n => <Row key={n.slot_id} n={n} today={d.today} side={<>
              {!n.upcoming && <Button variant="outline" size="sm" className="min-w-[6em] aria-pressed:bg-ok-bg aria-pressed:text-ok-text" aria-pressed={n.paid} loading={busy(n)}
                aria-label={`${n.name}，${n.paid ? '已支付，点击改回待支付' : '标记已支付'}`}
                data-tip={n.paid ? [n.paid_at && `${F.dtFull(n.paid_at)} 标记已支付`, '点击改回待支付'].filter(Boolean).join(' · ') : undefined}
                onClick={() => void one(n, !n.paid)}>
                {n.paid ? <><Check size={14} />已支付</> : '标记已支付'}
              </Button>}
              {menu(n)}
            </>} />)}
          </ul>
        </> : <Empty text={`${m}没有要收钱的节点`} />}
      </Panel>
      <AddSlotDialog open={adding} onClose={() => setAdding(false)} teamId={teamId} today={d.today} regions={regions} />

      <ConfirmDialog open={!!askAll} onOpenChange={(o) => !o && setAskAll(null)} title={`全部标记已支付 · ${m}`} confirmLabel="标记已支付"
        description={heldAll ? <>把 {m}选中的 {heldAll.length} 个节点都记为已支付：<span data-tip={heldAll.length > ALL_NAMES ? heldAll.map((n) => n.name).join('、') : undefined}>{heldAll.slice(0, ALL_NAMES).map((n, i) => <React.Fragment key={n.slot_id}>{i > 0 && '、'}<span className="cxa-nm">{n.name}</span></React.Fragment>)}{heldAll.length > ALL_NAMES && ` 等 ${heldAll.length} 个`}</span>。</> : undefined}
        onConfirm={async () => {
          const ns = heldAll!;
          // 撤销只撤这次由待支付变成已支付的（打开框之后别人已经标了的不动）
          const flipped = ns.filter((x) => d.nodes.find((y) => y.slot_id === x.slot_id)?.paid === false);
          await set(ns, true);
          toast.show({ tone: 'ok', text: `${m}的 ${ns.length} 个节点已标记已支付`, action: flipped.length ? { label: '撤销', onClick: () => void set(flipped, false).catch(toast.err) } : undefined });
        }} />
      {held?.kind === 'start' && <StartDialog open={act?.kind === 'start'} onClose={() => setAct(null)} n={held.n} today={d.today} />}
      {held?.kind === 'edit' && <EditSlotDialog open={act?.kind === 'edit'} onClose={() => setAct(null)} n={held.n} regions={regions} />}
      {held?.kind === 'end' && <EndSlotDialog open={act?.kind === 'end'} onClose={() => setAct(null)} n={held.n} today={d.today} />}
      <ConfirmDialog open={act?.kind === 'del'} onOpenChange={(o) => !o && setAct(null)} danger confirmLabel="删除" title={`删除 · ${held?.n.name ?? ''}`}
        description="录错了才删，删掉后它的收费记录一起去掉。不想再收钱请用「停止收费」"
        onConfirm={async () => { await delSlot.mutateAsync(held!.n.slot_id); toast.ok(`已删除 ${held!.n.name}`); }} />
    </>
  );
}

function Row({ n, today, side }: { n: BillingMonthNode; today: string; side: React.ReactNode }) {
  return <BillingNodeRow n={n} today={today} side={side} nameContent={n.holder_id
    ? <Button variant="link" size="inline" className="font-normal text-inherit hover:text-brand hover:no-underline" onClick={() => openDrawer(n.holder_id!)}>{n.name}</Button>
    : n.name} />;
}

/* ---------------------------------------------------------------- 维护：改开始收费日、地区备注、停止收费、手动添加 */
/** 第一期的区间，填日子时给个直观的样子 */
const firstSpan = (v: string, today: string) => `第一期 ${cnSpan(v, F.addMonths(v, 1), today)}，之后每满一个月收一次`;

const StartDialog = resetOnOpen(function StartDialog({ open, onClose, n, today }: { open: boolean; onClose: () => void; n: BillingMonthNode; today: string }) {
  const patch = usePatchBillingSlot();
  const [v, setV] = React.useState(n.start_date);
  return (
    <FormDialog open={open} onOpenChange={(o) => !o && onClose()} title={`改开始收费日 · ${n.name}`} submitLabel="保存" busy={patch.isPending} valid={DATE.test(v) && v !== n.start_date && v <= F.addMonths(today, 1)} dirty={v !== n.start_date}
      onSubmit={() => patch.mutate({ id: n.slot_id, start_date: v }, { onSuccess: () => { toast.ok(`开始收费日已改为 ${cnDate(v, today)}`); onClose(); }, onError: toast.err })}>
      <FormField label="开始收费日" info="改了以后每一期的起止都跟着变；原来标的「已支付」按第几期跟着挪到新的期" hint={DATE.test(v) ? firstSpan(v, today) : undefined}>
        <DateField value={v} onChange={setV} max={F.addMonths(today, 1)} />
      </FormField>
    </FormDialog>
  );
});

const EditSlotDialog = resetOnOpen(function EditSlotDialog({ open, onClose, n, regions }: { open: boolean; onClose: () => void; n: BillingMonthNode; regions: string[] }) {
  const patch = usePatchBillingSlot();
  const [region, setRegion] = React.useState(n.region === '未知' ? '' : n.region);
  const [note, setNote] = React.useState(n.note);
  const listId = React.useId();
  const r = region.trim(); const t = note.trim();
  const stored = storedRegion(r);
  const dirty = stored !== n.region || t !== n.note;
  const errR = [...r].length > 20 ? '最多 20 个字' : undefined;
  const errN = [...t].length > 60 ? '最多 60 个字' : undefined;
  return (
    <FormDialog open={open} onOpenChange={(o) => !o && onClose()} title={`改地区 / 备注 · ${n.name}`} submitLabel="保存" busy={patch.isPending} valid={dirty && !errR && !errN} dirty={dirty}
      onSubmit={() => patch.mutate({ id: n.slot_id, region: stored !== n.region ? stored : undefined, note: t !== n.note ? t : undefined }, { onSuccess: () => { toast.ok('已保存'); onClose(); }, onError: toast.err })}>
      <FormField label="地区" error={errR} info="默认按节点所在的国家，不知道国家的留空，显示「未标地区」"><Input list={listId} value={region} onChange={(e) => setRegion(e.target.value)} placeholder="未标地区" autoFocus /></FormField>
      <FormField label="备注（选填）" error={errN} hint={n.manual ? '客户页会显示这个名字' : undefined}><Input value={note} onChange={(e) => setNote(e.target.value)} placeholder={n.manual ? '如 墨西哥家宽' : '只在这里看，客户看不到'} /></FormField>
      <datalist id={listId}>{regions.map((x) => <option key={x} value={x} />)}</datalist>
    </FormDialog>
  );
});

/** 停止收费：手动添加的节点（不再收它的钱；面板里的节点离开团队就自动停止）。起点早于这天的期照收 */
const EndSlotDialog = resetOnOpen(function EndSlotDialog({ open, onClose, n, today }: { open: boolean; onClose: () => void; n: BillingMonthNode; today: string }) {
  const patch = usePatchBillingSlot();
  const [v, setV] = React.useState(today);
  const ok = DATE.test(v) && v > n.start_date;
  return (
    <ConfirmDialog asForm open={open} onOpenChange={(o) => !o && onClose()} title={`停止收费 · ${n.name}`} confirmLabel="停止收费" confirmDisabled={!ok}
      onConfirm={async () => { await patch.mutateAsync({ id: n.slot_id, end_date: v }); toast.ok(`${n.name} 收费到 ${cnDate(billedUntil(n.start_date, v), today)}`); }}>
      <FormField label="从哪天起不再收费" info="这天以前已经开始的那一期照收，之后不再收"
        error={DATE.test(v) && !ok ? `要晚于开始收费日（${cnDate(n.start_date, today)}）` : undefined}
        hint={ok ? `会收费到 ${cnDate(billedUntil(n.start_date, v), today)}` : undefined}>
        <DateField value={v} onChange={setV} min={F.addDays(n.start_date, 1)} />
      </FormField>
    </ConfirmDialog>
  );
});

/** 手动添加节点：系统外的节点（如家宽），不随对账自动增减，只能手动停止收费 */
const AddSlotDialog = resetOnOpen(function AddSlotDialog({ open, onClose, teamId, today, regions }: { open: boolean; onClose: () => void; teamId: string; today: string; regions: string[] }) {
  const add = useAddBillingSlot();
  const [f, setF] = React.useState({ region: '', start: today, count: '1', note: '' });
  const listId = React.useId();
  // 必填的提示等离开那一项再出现（一打开就满屏红字像是已经出错了）；超长的照旧立即显示
  const [seen, setSeen] = React.useState({ region: false, note: false });
  const r = f.region.trim(); const t = f.note.trim();
  const errs = {
    region: r === '' ? '必填' : [...r].length > 20 ? '最多 20 个字' : undefined,
    count: f.count !== '' && !(Number(f.count) >= 1 && Number(f.count) <= 50) ? '填 1–50' : undefined,
    note: t === '' ? '必填' : [...t].length > 60 ? '最多 60 个字' : undefined,
  };
  const valid = DATE.test(f.start) && f.start <= F.addMonths(today, 1) && f.count !== '' && !Object.values(errs).some(Boolean);
  const shown = (k: 'region' | 'note') => (errs[k] === '必填' && !seen[k] ? undefined : errs[k]);
  return (
    <FormDialog open={open} onOpenChange={(o) => !o && onClose()} title="手动添加节点" submitLabel="添加" busy={add.isPending} valid={valid} dirty={!!(f.region || f.note) || f.start !== today || f.count !== '1'}
      desc={<>不在面板里、但要收钱的节点 <InfoTip text="比如家宽。不会随节点增减自动变化，不收钱了要手动停止收费" /></>}
      onSubmit={() => add.mutate({ teamId, region: storedRegion(r), start_date: f.start, count: Number(f.count), note: t }, { onSuccess: () => { toast.ok(`已添加 ${f.count} 个节点`); onClose(); }, onError: toast.err })}>
      <div className="form-grid">
        <FormField label="名字" error={shown('note')} hint="客户页会显示这个名字"><Input value={f.note} onChange={(e) => setF({ ...f, note: e.target.value })} onBlur={() => setSeen((s) => ({ ...s, note: true }))} placeholder="如 墨西哥家宽" autoFocus /></FormField>
        <FormField label="地区" error={shown('region')}><Input list={listId} value={f.region} onChange={(e) => setF({ ...f, region: e.target.value })} onBlur={() => setSeen((s) => ({ ...s, region: true }))} placeholder="如 墨西哥" /></FormField>
        <FormField label="开始收费日" hint={DATE.test(f.start) ? firstSpan(f.start, today) : undefined}><DateField value={f.start} onChange={(v) => setF({ ...f, start: v })} max={F.addMonths(today, 1)} /></FormField>
        <FormField label="个数" error={errs.count}><UnitInput unit="个节点" width="full" inputMode="numeric" value={f.count} onChange={(e) => setF({ ...f, count: digits(e.target.value) })} /></FormField>
      </div>
      <datalist id={listId}>{regions.map((x) => <option key={x} value={x} />)}</datalist>
    </FormDialog>
  );
});

/* ---------------------------------------------------------------- 操作记录 */
function EventsPanel({ teamId }: { teamId: string }) {
  const q = useBillingEvents(teamId);
  const [selectedPage, setPage] = React.useState(0);
  const pages = q.data?.pages ?? [];
  const page = Math.min(selectedPage, Math.max(0, pages.length - 1));
  const list = pages[page]?.items ?? [];
  const hasOlder = page < pages.length - 1 || q.hasNextPage;
  const older = async () => {
    if (q.isFetchingNextPage) return;
    if (page < pages.length - 1) { setPage(page + 1); return; }
    const next = await q.fetchNextPage();
    if (!next.isError && next.data && next.data.pages.length > page + 1) setPage(page + 1);
  };
  return (
    <Panel title="操作记录" sub={<InfoTip text="节点加入、离开、替换补位、标记已支付和手动修改，都记在这里" />}>
      {q.isError && <ErrorNote error={q.error} onRetry={() => void (q.isFetchNextPageError ? q.fetchNextPage() : q.refetch())} />}
      {q.isLoading && <Skeleton className="h-[120px]" />}
      <div data-stale={q.isPlaceholderData} inert={q.isPlaceholderData || undefined}>
        {q.data && list.length === 0 && <Empty text="还没有记录" />}
        {list.length > 0 && (
          <ul className="cxa-events">
            {list.map((e) => <li key={e.id}><span className="num t-sub" data-tip={F.dtFull(e.at)}>{when(e.at)}</span><span>{e.text}</span></li>)}
          </ul>
        )}
        <CursorPager page={page + 1} size={BILLING_EVENT_PAGE_SIZE} loading={q.isFetchingNextPage}
          onNewer={page > 0 ? () => setPage(page - 1) : undefined}
          onOlder={hasOlder ? () => void older() : undefined} />
      </div>
    </Panel>
  );
}

/* ---------------------------------------------------------------- 客户页链接 */
function PageLink({ teamId, teamName, page, stale }: { teamId: string; teamName: string; page: CustomerPage | null; stale: boolean }) {
  const rotate = useRotateCustomerPage(); const patch = usePatchCustomerPage();
  const [askRotate, setAskRotate] = React.useState(false);
  const INFO = '给客户看的页面：按月列出这个团队每个节点（名字、地区、这一期的起止）已付还是待付，不含 IP、供应商、价格\n客户需输入固定查看密码。链接和密码长期有效，可手动换新或停用。';
  if (!page?.link || !page.password) {
    return (
      <span className="cxa-link" data-stale={stale} inert={stale || undefined}>
        <Button variant="outline" size="lg" loading={rotate.isPending} onClick={() => rotate.mutate(teamId, { onSuccess: () => toast.ok('已生成客户页链接'), onError: toast.err })}><Link2 />{page ? '重新生成客户页链接' : '生成客户页链接'}</Button>
        {page && <span className="t-sub">{!page.password ? '旧链接需换新' : '链接无法读取'}</span>}
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
    <section className="panel bm-share-card" aria-label="客户页分享" data-stale={stale} inert={stale || undefined}>
      <header className="bm-share-header">
        <span className="bm-share-title"><Link2 size={16} />客户页分享</span>
        <Pill tone={page.enabled ? 'ok' : 'neutral'}>{page.enabled ? '已启用' : '已停用'}</Pill>
        <div className="bm-share-actions">
          <IconButton label="打开客户页" size="icon-xs" render={<a href={page.link} target="_blank" rel="noreferrer noopener" />}><ExternalLink size={14} /></IconButton>
          <DropdownMenu>
            <DropdownMenuTrigger render={<IconButton size="icon-sm" label="客户页链接的操作" tip={false}><MoreHorizontal size={16} /></IconButton>} />
            <DropdownMenuContent align="end">
              <DropdownMenuItem onClick={() => setAskRotate(true)}>换新链接…</DropdownMenuItem>
              <DropdownMenuItem disabled={patch.isPending} onClick={() => setEnabled(!page.enabled)}>{page.enabled ? '停用' : '启用'}</DropdownMenuItem>
            </DropdownMenuContent>
          </DropdownMenu>
        </div>
      </header>
      <div className="bm-share-fields">
        <div className="bm-share-field"><span className="t-sub">分享链接</span>
          <div className="bm-share-value"><a className="ellipsis" href={page.link} target="_blank" rel="noreferrer noopener">{page.link.replace(/^https?:\/\//, '')}</a><CopyButton text={page.link} label="客户页链接" /></div>
        </div>
        <div className="bm-share-field"><span className="t-sub">查看密码</span>
          <div className="bm-share-value"><code>{page.password}</code><CopyButton text={page.password} label="查看密码" /></div>
        </div>
      </div>
      <footer className="bm-share-footer">
        <span>{page.enabled ? '链接和密码固定有效，可手动换新' : '启用后沿用当前链接和密码'}</span>
        <span data-tip={page.last_view_at ? F.dtFull(page.last_view_at) : undefined}>{page.last_view_at ? `客户 ${F.ago(page.last_view_at)}查看过` : '客户尚未查看'}</span>
      </footer>
      <ConfirmDialog open={askRotate} onOpenChange={setAskRotate} title={`换新链接 · ${teamName}`} confirmLabel="换新链接"
        description="旧链接马上失效，请把新链接和新密码发给客户。链接和密码会保持有效，直到再次换新或停用。"
        onConfirm={async () => { await rotate.mutateAsync(teamId); toast.ok('已换新链接，旧链接已失效'); }} />
    </section>
  );
}
