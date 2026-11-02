// 团队与套餐 → 计费台账：按节点数向团队收费。读者是不懂技术的人，只回答一件事——这个月哪些节点收了、哪些没收。
// 口径（服务端 batches.rs）：每个节点从开始收费日起每满一个月收一次（一期），按月看 = 每个节点起点在这个月的那一期；每一期只有已收 / 没收，手动标。
// 选团队 → 客户页链接 → 月份切换 + 一句话 → 「没收」「已收」两块清单（每行 节点 · 地区 · 这一期的区间 · 标记已收）→ 增减记录。
// 改开始收费日、地区备注、停止收费、删除收在每行的「⋯」里，手动添加节点在月份旁的「⋯」里。团队、月份写进地址（?team= / ?month=）。
import * as React from 'react';
import { Check, ExternalLink, Link2, MoreHorizontal } from 'lucide-react';
import { useAddBillingSlot, useBillingEvents, useBillingMonth, useDelBillingSlot, useMarkPaid, usePatchBillingSlot, usePatchCustomerPage, useRotateCustomerPage, useTeams } from '@/lib/queries';
import type { BillingMonth, BillingMonthNode, CustomerPage } from '@/lib/types';
import { useUrlState } from '@/lib/urlState';
import { openDrawer } from '@/lib/drawer';
import { resetOnOpen, useHeld } from '@/lib/useHeld';
import * as F from '@/lib/format';
import { Button, IconButton } from '@/components/ui/button';
import { Field, InfoTip, Input, UnitInput } from '@/components/ui/form';
import { DateField } from '@/components/ui/date-field';
import { Select } from '@/components/ui/select';
import { DropdownMenu, DropdownMenuContent, DropdownMenuItem, DropdownMenuSeparator, DropdownMenuTrigger, Tip } from '@/components/ui/overlays';
import { toast } from '@/components/ui/toast';
import { CopyButton, Empty, ErrorNote, Panel, Pill } from '@/components/app/primitives';
import { ConfirmDialog, FormDialog } from '@/components/app/dialogs';
import { flushAfterMenuClosed, runAfterMenuClosed } from '@/components/app/AnchoredDropdown';
import { MonthSwitch, cnDate, cnMonth, cnSpan, regionLabel } from './billing';

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
/** 「团队1-01、团队1-02」，多于 n 个只列前 n 个再写「等」 */
const nameList = (ns: BillingMonthNode[], n = 5) => `${ns.slice(0, n).map((x) => x.name).join('、')}${ns.length > n ? ' 等' : ''}`;
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
      {d && <div className="stack" data-stale={stale} inert={stale || undefined}><Board d={d} /></div>}
    </div>
  );
}

function Board({ d }: { d: BillingMonth }) {
  const [, upd] = useUrlState();
  const [adding, setAdding] = React.useState(false);
  // 还没开始记账：整页收成一句提示
  if (!d.initialized) return <Empty text="还没开始记账" hint={<>在中控上运行 <code>fleetctl billing init</code></>} />;
  const thisMonth = d.today.slice(0, 7);
  const regions = [...new Set(d.nodes.map((n) => regionLabel(n.region)))];
  return (
    <>
      <div className="cxa-month">
        <MonthSwitch month={d.month} months={d.months} onChange={(m) => upd({ month: m === thisMonth ? '' : m })} />
        <DropdownMenu>
          <DropdownMenuTrigger asChild><IconButton size="sm" label="更多" tip={false}><MoreHorizontal size={16} /></IconButton></DropdownMenuTrigger>
          {/* 菜单完全关上再开对话框：关菜单时焦点会先回到「⋯」，对话框里自动聚焦的名字框被它抢走一次，会被当成「离开过」而提前标红「必填」 */}
          <DropdownMenuContent align="end" onCloseAutoFocus={(e) => { if (flushAfterMenuClosed()) e.preventDefault(); }}><DropdownMenuItem onSelect={() => runAfterMenuClosed(() => setAdding(true))}>手动添加节点…</DropdownMenuItem></DropdownMenuContent>
        </DropdownMenu>
      </div>
      <Lead d={d} />
      <Lists d={d} regions={regions} />
      <EventsPanel teamId={d.team.id} />
      <AddSlotDialog open={adding} onClose={() => setAdding(false)} teamId={d.team.id} today={d.today} regions={regions} />
    </>
  );
}

/* ---------------------------------------------------------------- 一句话 */
function Lead({ d }: { d: BillingMonth }) {
  const m = cnMonth(d.month, d.today);
  const unpaid = d.nodes.filter((n) => !n.paid);
  const n = d.nodes.length;
  const p = d.pending;
  // 还没到的期：起点在今天以后。整月都是这样的月份（下个月）不写「没收」，免得像漏收
  const early = unpaid.filter((x) => x.upcoming).length;
  const allEarly = n > 0 && d.nodes.every((x) => x.upcoming);
  return (
    <p className="cxa-lead">
      {n === 0 ? <>{m}：{d.team.name} 没有要收钱的节点。</>
        : allEarly ? <>{m}：<b>{n} 个节点</b>都还没到收费的日子，可以提前标记已收{n > unpaid.length && <>（已提前收 {n - unpaid.length} 个）</>}。</>
          : unpaid.length === 0 ? <>{m}：{d.team.name} 的 <b>{n} 个节点</b><span className="cx-done">都收齐了 ✓</span></>
            : <>{m}：{d.team.name} 要收 <b>{n} 个节点</b>的钱，已收 <b>{n - unpaid.length} 个</b>，没收 <b>{unpaid.length} 个</b>{early > 0 ? `（其中 ${early} 个还没到）` : unpaid.length <= 5 && <>（{nameList(unpaid)}）</>}。</>}
      {p.length > 0 && <> 另有 {p.length} 个新节点刚加入（{p.map((x) => x.machine.name).join('、')}），{p.length === 1 ? `${when(p[0].decide_at)} 后` : '满 24 小时后'}确定是补位还是新增 <InfoTip text={'加入满 24 小时后确定：这期间有节点离开，它就补上那个位置，不多收；没有就算新增，从加入那天开始收费'} />。</>}
    </p>
  );
}

/* ---------------------------------------------------------------- 没收 / 已收 两块清单 */
/** 全部标记已收的确认框里最多列几个名字，其余悬停看 */
const ALL_NAMES = 6;
type Act = { kind: 'start' | 'edit' | 'end' | 'del'; n: BillingMonthNode };

function Lists({ d, regions }: { d: BillingMonth; regions: string[] }) {
  const mark = useMarkPaid();
  const patch = usePatchBillingSlot(); const delSlot = useDelBillingSlot();
  // 全部标记已收：打开确认框时的名单（确认的就是框里列出的这些，中途刷新进来的新节点不会被一起标）
  const [askAll, setAskAll] = React.useState<BillingMonthNode[] | null>(null);
  const heldAll = useHeld(askAll ? 'all' : null, askAll);
  // 正在提交的节点（按 slot 记：连点两行时前一行的加载状态不会被后一次覆盖）
  const [loading, setLoading] = React.useState<string[]>([]);
  const [act, setAct] = React.useState<Act | null>(null);
  const held = useHeld(act ? `${act.kind}:${act.n.slot_id}` : null, act);
  const m = cnMonth(d.month, d.today);
  const unpaid = d.nodes.filter((n) => !n.paid);
  const paid = d.nodes.filter((n) => n.paid);
  const teamId = d.team.id;
  const set = async (ns: BillingMonthNode[], on: boolean) => {
    const ids = ns.map((n) => n.slot_id);
    setLoading((l) => [...l, ...ids]);
    try { return await mark.mutateAsync({ teamId, month: d.month, slot_ids: ids, paid: on }); } finally { setLoading((l) => l.filter((x) => !ids.includes(x))); }
  };
  // 撤销只撤自己这次改的：点的时候这个节点本来就是目标状态（别人刚标过）就是空操作，不给撤销，免得把别人标的改回去
  const one = (n: BillingMonthNode, on: boolean) => set([n], on).then(
    () => toast.show({ tone: 'ok', text: `${n.name} ${m}${on ? '已收' : '改回没收'}`, action: n.paid !== on ? { label: '撤销', onClick: () => void set([n], !on).catch(toast.err) } : undefined }),
    toast.err,
  );
  const busy = (n: BillingMonthNode) => loading.includes(n.slot_id);
  const reopen = (n: BillingMonthNode) => patch.mutate({ id: n.slot_id, end_date: null }, { onSuccess: () => toast.ok(`${n.name} 已恢复收费`), onError: toast.err });

  const menu = (n: BillingMonthNode) => (
    <DropdownMenu>
      <DropdownMenuTrigger asChild><IconButton size="sm" label={`${n.name}的更多操作`} tip={false}><MoreHorizontal size={16} /></IconButton></DropdownMenuTrigger>
      <DropdownMenuContent align="end">
        {n.paid && <><DropdownMenuItem disabled={mark.isPending} onSelect={() => void one(n, false)}>改回没收</DropdownMenuItem><DropdownMenuSeparator /></>}
        <DropdownMenuItem onSelect={() => setAct({ kind: 'start', n })}>改开始收费日…</DropdownMenuItem>
        <DropdownMenuItem onSelect={() => setAct({ kind: 'edit', n })}>改地区 / 备注…</DropdownMenuItem>
        {n.manual && n.ended && <DropdownMenuItem disabled={patch.isPending} onSelect={() => reopen(n)}>恢复收费</DropdownMenuItem>}
        {((n.manual && !n.ended) || n.vacated) && <DropdownMenuItem onSelect={() => setAct({ kind: 'end', n })}>停止收费…</DropdownMenuItem>}
        {n.manual && <><DropdownMenuSeparator /><DropdownMenuItem danger onSelect={() => setAct({ kind: 'del', n })}>删除…</DropdownMenuItem></>}
      </DropdownMenuContent>
    </DropdownMenu>
  );

  return (
    <>
      {d.nodes.length === 0 && <Panel><Empty text={`${m}没有要收钱的节点`} /></Panel>}
      {unpaid.length > 0 && (
        <Panel title="没收" count={<span className="cxa-due-count">{unpaid.length}</span>} flush
          tools={unpaid.length > 1 ? <Button size="sm" onClick={() => setAskAll(unpaid)}>全部标记已收</Button> : undefined}>
          <ul className="bm-list">
            {unpaid.map((n) => <Row key={n.slot_id} n={n} today={d.today} side={<>
              <Button size="sm" loading={busy(n)} onClick={() => void one(n, true)}>标记已收</Button>
              {menu(n)}
            </>} />)}
          </ul>
        </Panel>
      )}
      {paid.length > 0 && (
        <Panel title="已收" count={paid.length} flush>
          <ul className="bm-list">
            {paid.map((n) => <Row key={n.slot_id} n={n} today={d.today} side={<>
              <span className="bm-paid" data-tip={n.paid_at ? `${F.dtFull(n.paid_at)} 标的` : undefined}><Check />已收</span>
              {menu(n)}
            </>} />)}
          </ul>
        </Panel>
      )}

      <ConfirmDialog open={!!askAll} onOpenChange={(o) => !o && setAskAll(null)} title={`全部标记已收 · ${m}`} confirmLabel="标记已收"
        description={heldAll ? <>把 {m}没收的 {heldAll.length} 个节点都记为已收：<span data-tip={heldAll.length > ALL_NAMES ? heldAll.map((n) => n.name).join('、') : undefined}>{heldAll.slice(0, ALL_NAMES).map((n, i) => <React.Fragment key={n.slot_id}>{i > 0 && '、'}<span className="cxa-nm">{n.name}</span></React.Fragment>)}{heldAll.length > ALL_NAMES && ` 等 ${heldAll.length} 个`}</span>。</> : undefined}
        onConfirm={async () => {
          const ns = heldAll!;
          // 撤销只撤这次由没收变成已收的（打开框之后别人已经标了的不动）
          const flipped = ns.filter((x) => d.nodes.find((y) => y.slot_id === x.slot_id)?.paid === false);
          await set(ns, true);
          toast.show({ tone: 'ok', text: `${m}的 ${ns.length} 个节点已标记已收`, action: flipped.length ? { label: '撤销', onClick: () => void set(flipped, false).catch(toast.err) } : undefined });
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

/** 一行：节点名（点开节点抽屉）与标签；小字 地区 · 这一期的区间；右边收了没有 */
function Row({ n, today, side }: { n: BillingMonthNode; today: string; side: React.ReactNode }) {
  return (
    <li className="bm-row">
      <div className="bm-main">
        <span className="bm-name">
          {n.holder_id ? <button type="button" className="btn btn-link cxa-node" onClick={() => openDrawer(n.holder_id!)}>{n.name}</button> : n.name}
          {n.manual && <span className="bm-tag" data-tip="不在面板里的节点，不会自动增减，不收钱了要手动停止">手动添加</span>}
          {n.vacated && <span className="bm-tag" data-tone="warn" data-tip="节点离开了：24 小时内有新节点加入，就由它补上，照常收费；没有的话，之后的月份不再收">已离开，等补位</span>}
          {n.ended && <span className="bm-tag">已停止收费</span>}
          {n.upcoming && <span className="bm-tag">还没到</span>}
        </span>
        <span className="bm-sub">{regionLabel(n.region)} · <span className="num">{cnSpan(n.period_start, n.period_end, today)}</span></span>
      </div>
      <span className="bm-side">{side}</span>
    </li>
  );
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
      <Field label="开始收费日" info="改了以后每一期的起止都跟着变；原来标的「已收」按第几期跟着挪到新的期" hint={DATE.test(v) ? firstSpan(v, today) : undefined}>
        <DateField value={v} onChange={setV} max={F.addMonths(today, 1)} />
      </Field>
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
      <Field label="地区" error={errR} info="默认按节点所在的国家，不知道国家的留空，显示「未标地区」"><Input list={listId} value={region} onChange={(e) => setRegion(e.target.value)} placeholder="未标地区" autoFocus /></Field>
      <Field label="备注（选填）" error={errN} hint={n.manual ? '客户页会显示这个名字' : undefined}><Input value={note} onChange={(e) => setNote(e.target.value)} placeholder={n.manual ? '如 墨西哥家宽' : '只在这里看，客户看不到'} /></Field>
      <datalist id={listId}>{regions.map((x) => <option key={x} value={x} />)}</datalist>
    </FormDialog>
  );
});

/** 停止收费：手动添加的节点（不再收它的钱）或等补位的节点（不等 24 小时，直接算离开）。起点早于这天的期照收 */
const EndSlotDialog = resetOnOpen(function EndSlotDialog({ open, onClose, n, today }: { open: boolean; onClose: () => void; n: BillingMonthNode; today: string }) {
  const patch = usePatchBillingSlot();
  const [v, setV] = React.useState(today);
  const ok = DATE.test(v) && v > n.start_date && (n.manual || v <= today);
  return (
    <ConfirmDialog asForm open={open} onOpenChange={(o) => !o && onClose()} title={`停止收费 · ${n.name}`} confirmLabel="停止收费" confirmDisabled={!ok}
      description={n.vacated ? '不等 24 小时，现在就算它离开了' : undefined}
      onConfirm={async () => { await patch.mutateAsync({ id: n.slot_id, end_date: v }); toast.ok(`${n.name} 收费到 ${cnDate(billedUntil(n.start_date, v), today)}`); }}>
      <Field label="从哪天起不再收费" info="这天以前已经开始的那一期照收，之后不再收"
        error={DATE.test(v) && !ok ? (v <= n.start_date ? `要晚于开始收费日（${cnDate(n.start_date, today)}）` : '不能晚于今天') : undefined}
        hint={ok ? `会收费到 ${cnDate(billedUntil(n.start_date, v), today)}` : undefined}>
        <DateField value={v} onChange={setV} min={F.addDays(n.start_date, 1)} />
      </Field>
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
        <Field label="名字" error={shown('note')} hint="客户页会显示这个名字"><Input value={f.note} onChange={(e) => setF({ ...f, note: e.target.value })} onBlur={() => setSeen((s) => ({ ...s, note: true }))} placeholder="如 墨西哥家宽" autoFocus /></Field>
        <Field label="地区" error={shown('region')}><Input list={listId} value={f.region} onChange={(e) => setF({ ...f, region: e.target.value })} onBlur={() => setSeen((s) => ({ ...s, region: true }))} placeholder="如 墨西哥" /></Field>
        <Field label="开始收费日" hint={DATE.test(f.start) ? firstSpan(f.start, today) : undefined}><DateField value={f.start} onChange={(v) => setF({ ...f, start: v })} max={F.addMonths(today, 1)} /></Field>
        <Field label="个数" error={errs.count}><UnitInput unit="个节点" width="full" inputMode="numeric" value={f.count} onChange={(e) => setF({ ...f, count: digits(e.target.value) })} /></Field>
      </div>
      <datalist id={listId}>{regions.map((x) => <option key={x} value={x} />)}</datalist>
    </FormDialog>
  );
});

/* ---------------------------------------------------------------- 增减记录 */
function EventsPanel({ teamId }: { teamId: string }) {
  const q = useBillingEvents(teamId);
  const list = React.useMemo(() => q.data?.pages.flat() ?? [], [q.data]);
  return (
    <Panel title="增减记录" sub={<InfoTip text="节点加入、离开、补位、标记已收和手动修改，都记在这里" />}>
      {q.isError && <ErrorNote error={q.error} onRetry={() => void q.refetch()} />}
      {q.isLoading && <div className="skel" style={{ height: 120 }} />}
      <div data-stale={q.isPlaceholderData} inert={q.isPlaceholderData || undefined}>
        {q.data && list.length === 0 && <Empty text="还没有记录" />}
        {list.length > 0 && (
          <ul className="cxa-events">
            {list.map((e) => <li key={e.id}><span className="num t-sub" data-tip={F.dtFull(e.at)}>{when(e.at)}</span><span>{e.text}</span></li>)}
          </ul>
        )}
        {q.hasNextPage && <div className="audit-more"><Button size="sm" loading={q.isFetchingNextPage} onClick={() => void q.fetchNextPage()}>加载更早的</Button></div>}
      </div>
    </Panel>
  );
}

/* ---------------------------------------------------------------- 客户页链接 */
function PageLink({ teamId, teamName, page, stale }: { teamId: string; teamName: string; page: CustomerPage | null; stale: boolean }) {
  const rotate = useRotateCustomerPage(); const patch = usePatchCustomerPage();
  const [askRotate, setAskRotate] = React.useState(false);
  const INFO = '给客户看的页面：按月列出这个团队每个节点（名字、地区、这一期的起止）已付还是待付，不含 IP、供应商、价格\n客户打开不用登录；拿到链接的人都能看，只发给这个团队';
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
