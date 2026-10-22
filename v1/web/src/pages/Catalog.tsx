// 团队与套餐：团队（改名、排序、重新编号预览→应用）、供应商、套餐。
import * as React from 'react';
import { useQuery } from '@tanstack/react-query';
import { Link, useSearchParams } from 'react-router-dom';
import { Archive, ArchiveRestore, ArrowRight, ExternalLink, ListOrdered, Pencil, Plus, Trash2 } from 'lucide-react';
import { fetchRenumber, useAddPlan, useAddProvider, useAddTeam, useDelPlan, useDelProvider, useDelTeam, usePatchPlan, usePatchProvider, usePatchTeam, usePlans, useProviders, useRenumber, useTeams, useTeamsCompact } from '@/lib/queries';
import type { Plan, Provider, Team, TrafficReset, TrafficRule } from '@/lib/types';
import { currencyOptions, cycleLabel, cycleOptions, limitGb, meta, trafficResetLabel, trafficRuleLabel } from '@/lib/derive';
import { collator } from '@/lib/sort';
import { tzLabel } from '@/lib/tz';
import { useUrlSort } from '@/lib/urlState';
import * as F from '@/lib/format';
import { Button, IconButton } from '@/components/ui/button';
import { CheckRow, Field, InfoTip, Input, UnitInput } from '@/components/ui/form';
import { Select } from '@/components/ui/select';
import { Tabs, TabsList, TabsTrigger, TabsContent } from '@/components/ui/overlays';
import { Dialog, DialogContent, DialogDescription, DialogFooter, DialogTitle, DialogHeader } from '@/components/ui/dialog';
import { SkeletonRows } from '@/components/ui/skeleton';
import { toast } from '@/components/ui/toast';
import { resetOnOpen, useHeld } from '@/lib/useHeld';
import { Chip, Empty, ErrorNote, Note, Num, PageHead, Panel, SortHead, TableWrap, type SortState } from '@/components/app/primitives';
import { ConfirmDialog, FormDialog } from '@/components/app/dialogs';
import { TzInput } from '@/components/app/TzInput';

const SECTIONS = ['teams', 'providers', 'plans'];

export function CatalogPage() {
  // 子页写进 URL 的 section（同设置页），刷新 / 分享链接停在原处；换子页时排序（?sort=）不带过去
  const [sp, setSp] = useSearchParams();
  const tab = SECTIONS.find((s) => s === sp.get('section')) ?? 'teams';
  return (
    <div className="stack">
      <PageHead title="团队与套餐" />
      <Tabs value={tab} onValueChange={(v) => setSp((p) => { const n = new URLSearchParams(p); n.set('section', v); n.delete('sort'); return n; }, { replace: true })}>
        <TabsList><TabsTrigger value="teams">团队</TabsTrigger><TabsTrigger value="providers">供应商</TabsTrigger><TabsTrigger value="plans">套餐</TabsTrigger></TabsList>
        <div style={{ marginTop: 16 }}>
          <TabsContent value="teams"><TeamsTab /></TabsContent>
          <TabsContent value="providers"><ProvidersTab /></TabsContent>
          <TabsContent value="plans"><PlansTab /></TabsContent>
        </div>
      </Tabs>
    </div>
  );
}

/** 按列取值排序：数字按大小，文字按中文名称（编号按数值）；没有排序时保持原顺序 */
function sortRows<T>(list: T[], sort: SortState | null, key: Record<string, (x: T) => number | string>): T[] {
  const f = sort && key[sort.id];
  if (!f) return list;
  return [...list].sort((a, b) => {
    const x = f(a); const y = f(b);
    const c = typeof x === 'number' && typeof y === 'number' ? x - y : collator.compare(String(x), String(y));
    return sort.desc ? -c : c;
  });
}
/** 删除确认（团队、供应商、套餐共用）：还有机器、套餐用着时中控回 409，说明留在通知里，对话框不关 */
function DeleteDialog({ what, item, onClose, run }: { what: string; item: { id: string; name: string } | null; onClose: () => void; run: (id: string) => Promise<unknown> }) {
  const held = useHeld(item?.id, item);
  return (
    <ConfirmDialog open={!!item} onOpenChange={(o) => !o && onClose()} danger confirmLabel="删除" title={`删除${what}「${held?.name ?? ''}」？`}
      description={<>删了不能恢复 <InfoTip text={`还有机器${what === '供应商' ? '、套餐' : ''}用着（含已退役的）时删不掉，会提示还有哪些`} /></>}
      onConfirm={async () => { await run(item!.id); toast.ok(`${what}「${item!.name}」已删除`); }} />
  );
}

/** 机器数：点了去机器页看这些机器（筛选参数同机器页：team / q） */
const CountLink = ({ n, to }: { n: number; to: string }) => (n > 0 ? <Link className="num-link" to={to}>{n}</Link> : <span className="t-muted">0</span>);

/* ---------------------------------------------------------------- 团队 */
const TEAM_COLS = [
  { id: 'name', header: '名称', first: 'asc' as const }, { id: 'order', header: '排序', r: true, first: 'asc' as const },
  { id: 'nodes', header: '在用节点', r: true, first: 'desc' as const }, { id: 'act', header: '' },
];
const TEAM_KEY: Record<string, (t: Team) => number | string> = { name: (t) => t.name, order: (t) => t.sort_order, nodes: (t) => t.node_count };
function TeamsTab() {
  const q = useTeams();
  const compact = useTeamsCompact().data;
  const [sort, setSort] = useUrlSort();
  const [edit, setEdit] = React.useState<Team | 'new' | null>(null);
  const [renum, setRenum] = React.useState<Team | null>(null);
  const [askDel, setAskDel] = React.useState<Team | null>(null);
  const delTeam = useDelTeam();
  // 关闭动画期间沿用最近的对象，标题与内容不闪变
  const held = useHeld(edit === 'new' ? 'new' : edit?.id, edit);
  const renumHeld = useHeld(renum?.id, renum);
  const list = sortRows([...(q.data ?? [])].sort((a, b) => a.sort_order - b.sort_order), sort, TEAM_KEY);
  return (
    <>
      {q.isError && <ErrorNote error={q.error} onRetry={() => void q.refetch()} />}
      <Panel title="团队" count={list.length} sub={<InfoTip text="首页与表格按「排序」从小到大排列" />} tools={<Button variant="primary" size="md" onClick={() => setEdit('new')}><Plus />新建团队</Button>} flush>
        <TableWrap>
          <table className="dt is-one" style={{ minWidth: 520 }}>
            <colgroup><col /><col style={{ width: 90 }} /><col style={{ width: 100 }} /><col style={{ width: 256 }} /></colgroup>
            <SortHead cols={TEAM_COLS} sort={sort} onSort={setSort} />
            <tbody>
              {list.map((t) => {
                // 编号已经连续（或团队里还没有节点）就没有可调的；机器列表还没到时先不禁用
                const done = compact ? compact[t.id] ?? true : false;
                return (
                  <tr key={t.id}><td className="strong">{t.name}</td><td className="r num">{t.sort_order}</td><td className="r num"><CountLink n={t.node_count} to={`/nodes?team=${t.id}`} /></td>
                    <td className="r"><span className="row" style={{ justifyContent: 'flex-end', flexWrap: 'nowrap' }}>
                      <Button size="sm" variant="quiet" onClick={() => setEdit(t)}><Pencil size={13} />改名 / 排序</Button>
                      {/* aria-disabled 而不是 disabled：禁用的按钮收不到悬停，看不到为什么不能点 */}
                      <Button size="sm" aria-disabled={done || undefined} data-tip={done ? '编号已连续' : undefined} onClick={() => { if (!done) setRenum(t); }}><ListOrdered size={13} />重新编号</Button>
                      <IconButton size="sm" label="删除团队" onClick={() => setAskDel(t)}><Trash2 size={14} /></IconButton></span></td></tr>
                );
              })}
            </tbody>
          </table>
          {q.isLoading && <SkeletonRows rows={3} one />}
          {q.data && list.length === 0 && <Empty text="还没有团队" />}
        </TableWrap>
      </Panel>

      {held && <TeamDialog team={held} open={edit != null} onClose={() => setEdit(null)} />}
      {renumHeld && <RenumberDialog team={renumHeld} open={!!renum} onClose={() => setRenum(null)} />}
      <DeleteDialog what="团队" item={askDel} onClose={() => setAskDel(null)} run={(id) => delTeam.mutateAsync(id)} />
    </>
  );
}

/** 新建 / 改名与排序。每次打开重新挂载（resetOnOpen）：字段直接按这次的团队初始化 */
const TeamDialog = resetOnOpen(function TeamDialog({ team, open, onClose }: { team: Team | 'new'; open: boolean; onClose: () => void }) {
  const add = useAddTeam(); const patch = usePatchTeam();
  const [name, setName] = React.useState(team === 'new' ? '' : team.name);
  const [order, setOrder] = React.useState(team === 'new' ? '' : String(team.sort_order));
  const save = () => {
    const sort_order = order.trim() === '' ? undefined : Number(order);
    const ok = () => { toast.ok(team === 'new' ? '团队已创建' : '团队已保存'); onClose(); };
    if (team === 'new') add.mutate({ name: name.trim(), sort_order }, { onSuccess: ok, onError: toast.err });
    else patch.mutate({ id: team.id, name: name.trim() !== team.name ? name.trim() : undefined, sort_order }, { onSuccess: ok, onError: toast.err });
  };
  return (
    <FormDialog open={open} onOpenChange={(o) => !o && onClose()} title={team === 'new' ? '新建团队' : `修改 ${team.name}`} submitLabel="保存" busy={add.isPending || patch.isPending} valid={name.trim().length > 0}
      dirty={team === 'new' ? !!(name.trim() || order) : name !== team.name || order !== String(team.sort_order)}
      desc={team !== 'new' && <>所有节点的显示名会一起变 <InfoTip text="线路机上的备注自动同步，不换令牌、不断连接。" /></>} onSubmit={save}>
      <Field label="团队名"><Input value={name} onChange={(e) => setName(e.target.value)} autoFocus /></Field>
      <Field label="排序"><Input inputMode="numeric" placeholder="越小越靠前，留空放最后" value={order} onChange={(e) => setOrder(e.target.value.replace(/\D/g, ''))} /></Field>
    </FormDialog>
  );
});

/** 重新编号：先预览差异，再一次性应用（token 防止预览后数据变了）。关闭时播完退场动画。
 *  每次打开重新挂载（resetOnOpen），只认这次打开后取到的预览：缓存里还留着上一次的（关着时对话框也挂着，观察者没走，gcTime 0 回收不了） */
const RenumberDialog = resetOnOpen(function RenumberDialog({ team, open, onClose }: { team: Team; open: boolean; onClose: () => void }) {
  const pv = useQuery({ queryKey: ['renumber', team.id], queryFn: () => fetchRenumber(team.id), gcTime: 0, staleTime: 0, enabled: open });
  const apply = useRenumber();
  const d = pv.isFetchedAfterMount && pv.isSuccess ? pv.data : undefined;
  const n = d?.changes.length ?? 0;
  return (
    <Dialog open={open} onOpenChange={(o) => !o && !apply.isPending && onClose()}>
      <DialogContent size="md">
        <DialogHeader><DialogTitle>重新编号 · {team.name}</DialogTitle>
          <DialogDescription>去掉空号，按原顺序排成 01、02、03… <InfoTip text="退役时已自动补齐空号；这里用于手工指定过跳号的情况。" /></DialogDescription></DialogHeader>
        {!pv.isFetchedAfterMount && <div className="skel" style={{ height: 80 }} />}
        {pv.isFetchedAfterMount && pv.isError && <Note tone="bad">{(pv.error as Error).message}</Note>}
        {d && (n === 0
          ? <Note tone="ok">编号已经连续</Note>
          : <div className="table-wrap" style={{ maxHeight: 300, overflow: 'auto', border: '1px solid var(--line)', borderRadius: 'var(--r-float)' }}>
            <table className="dt is-one"><colgroup><col /><col style={{ width: 36 }} /><col /></colgroup>
              <thead><tr><th>当前名称</th><th /><th>调整后</th></tr></thead>
              <tbody>{d.changes.map((c) => <tr key={c.machine_id}><td className="num">{c.old_name}</td><td><ArrowRight size={14} style={{ color: 'var(--fg-3)' }} /></td><td className="strong num">{c.new_name}</td></tr>)}</tbody></table>
          </div>)}
        <DialogFooter>
          <Button onClick={onClose} disabled={apply.isPending}>取消</Button>
          {/* 应用失败后会重取预览（切回窗口也会）：重取完之前不让应用 */}
          <Button variant="primary" disabled={!d || n === 0 || pv.isFetching} loading={apply.isPending}
            onClick={() => apply.mutate({ id: team.id, token: d!.token }, { onSuccess: () => { toast.ok(`已重新编号：${n} 台`); onClose(); }, onError: (e) => { toast.err(e); void pv.refetch(); } })}>应用 {n > 0 ? `${n} 项改名` : ''}</Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
});

/* ---------------------------------------------------------------- 供应商 */
/** 新供应商的初值：时区留空 = 跟随组织时区，其余取 /meta 的默认值 */
const emptyProvider = () => {
  const d = meta().defaults;
  return { name: '', code: '', panel_url: '', timezone: '', traffic_rule: d.traffic_rule, traffic_reset: d.traffic_reset, remind_days: String(d.remind_days) };
};
/** 流量计量下拉：供应商怎么算本周期用量（网卡入、出总量） */
const ruleOptions = () => Object.entries(meta().traffic_rules).map(([value, label]) => ({ value, label }));
/** 流量重置：每个流量周期从哪天起（括注只是说明） */
const RESET_NOTE: Record<string, string> = { month: '（每月 1 号）', billing: '（到期日那天）' };
const resetOptions = () => Object.entries(meta().traffic_resets).map(([value, label]) => ({ value, label: `按${label}${RESET_NOTE[value] ?? ''}` }));
/** 留空的账单时区跟随组织时区：「跟随组织 · 上海（UTC+8）」 */
const orgTz = () => `跟随组织 · ${tzLabel(meta().org_timezone)}`;
function ProvidersTab() {
  const q = useProviders();
  const [edit, setEdit] = React.useState<Provider | 'new' | null>(null);
  const held = useHeld(edit === 'new' ? 'new' : edit?.id, edit);
  const [askDel, setAskDel] = React.useState<Provider | null>(null);
  const delProvider = useDelProvider();
  const list = q.data ?? [];
  // 一个面板地址都没填时不占一列
  const panel = list.some((p) => p.panel_url);
  return (
    <>
      {q.isError && <ErrorNote error={q.error} onRetry={() => void q.refetch()} />}
      <Panel title="供应商" count={list.length} sub={<InfoTip text="名下机器的账单时区、流量重置、到期提醒跟随这里" />} tools={<Button variant="primary" onClick={() => setEdit('new')}><Plus />新建供应商</Button>} flush>
        <TableWrap>
          <table className="dt is-one" style={{ minWidth: panel ? 1100 : 960 }}>
            <colgroup><col style={{ width: 170 }} /><col style={{ width: 84 }} /><col style={{ width: 190 }} /><col style={{ width: 170 }} /><col style={{ width: 110 }} /><col style={{ width: 96 }} />{panel && <col />}<col style={{ width: 128 }} /></colgroup>
            <thead><tr><th>名称</th><th>代码</th><th>账单时区</th><th>流量计量</th><th>流量重置</th><th>到期提醒</th>{panel && <th>面板</th>}<th /></tr></thead>
            <tbody>{list.map((p) => (
              <tr key={p.id}><td className="strong">{p.name}</td><td><Chip>{p.code}</Chip></td>
                <td data-tip={p.timezone ?? meta().org_timezone}>{p.timezone ? tzLabel(p.timezone) : <span className="t-muted">{orgTz()}</span>}</td>
                <td>{trafficRuleLabel(p.traffic_rule)}</td>
                <td data-tip={resetOptions().find((o) => o.value === (p.traffic_reset ?? meta().defaults.traffic_reset))?.label}>按{trafficResetLabel(p.traffic_reset ?? meta().defaults.traffic_reset)}</td>
                <td>提前 <span className="num">{p.remind_days ?? meta().defaults.remind_days}</span> 天</td>
                {panel && <td>{p.panel_url && /^https?:\/\//i.test(p.panel_url) ? <a href={p.panel_url} target="_blank" rel="noreferrer noopener" className="btn-link btn" style={{ gap: 4, maxWidth: '100%' }}><span className="ellipsis">{p.panel_url.replace(/^https?:\/\//, '')}</span><ExternalLink size={12} /></a> : (p.panel_url || <span className="t-muted">—</span>)}</td>}
                <td className="r"><span className="row" style={{ justifyContent: 'flex-end', flexWrap: 'nowrap' }}>
                  <Button size="sm" variant="quiet" onClick={() => setEdit(p)}><Pencil size={13} />编辑</Button>
                  <IconButton size="sm" label="删除供应商" onClick={() => setAskDel(p)}><Trash2 size={14} /></IconButton></span></td></tr>
            ))}</tbody>
          </table>
          {q.isLoading && <SkeletonRows rows={3} one />}
          {q.data && list.length === 0 && <Empty text="还没有供应商" />}
        </TableWrap>
      </Panel>
      {held && <ProviderDialog provider={held} open={edit != null} onClose={() => setEdit(null)} />}
      <DeleteDialog what="供应商" item={askDel} onClose={() => setAskDel(null)} run={(id) => delProvider.mutateAsync(id)} />
    </>
  );
}

/** 新建 / 编辑供应商。每次打开重新挂载（resetOnOpen）：字段直接按这次的供应商初始化 */
const ProviderDialog = resetOnOpen(function ProviderDialog({ provider, open, onClose }: { provider: Provider | 'new'; open: boolean; onClose: () => void }) {
  const add = useAddProvider(); const patch = usePatchProvider();
  const [f0] = React.useState(() => (provider === 'new' ? emptyProvider() : {
    name: provider.name, code: provider.code, panel_url: provider.panel_url ?? '', timezone: provider.timezone ?? '', traffic_rule: provider.traffic_rule,
    traffic_reset: provider.traffic_reset ?? meta().defaults.traffic_reset, remind_days: String(provider.remind_days ?? meta().defaults.remind_days),
  }));
  const [f, setF] = React.useState(f0);
  const dirty = (Object.keys(f) as (keyof typeof f)[]).some((k) => f[k] !== f0[k]);
  const remindBad = !(f.remind_days !== '' && Number(f.remind_days) <= 60);
  // 面板地址、时区留空都存成 null：面板地址清空，时区跟随组织时区
  const save = () => {
    const body = { name: f.name.trim(), code: f.code.trim().toUpperCase(), panel_url: f.panel_url.trim() || null, timezone: f.timezone.trim() || null, traffic_rule: f.traffic_rule, traffic_reset: f.traffic_reset, remind_days: Number(f.remind_days) };
    const ok = () => { toast.ok(provider === 'new' ? '供应商已创建' : '供应商已保存'); onClose(); };
    if (provider === 'new') add.mutate(body, { onSuccess: ok, onError: toast.err });
    else patch.mutate({ id: provider.id, ...body }, { onSuccess: ok, onError: toast.err });
  };
  return (
    <FormDialog dirty={dirty} open={open} onOpenChange={(o) => !o && onClose()} title={provider === 'new' ? '新建供应商' : '编辑供应商'} submitLabel="保存" busy={add.isPending || patch.isPending} valid={f.name.trim() !== '' && f.code.trim() !== '' && !remindBad}
      desc={provider !== 'new' && <>改了立即对名下机器生效 <InfoTip text="账单时区、流量重置、到期提醒是名下机器订阅的模板；在「费用」页单独设置过的除外。" /></>} onSubmit={save}>
      <div className="form-grid">
        <Field label="名称"><Input value={f.name} onChange={(e) => setF({ ...f, name: e.target.value })} autoFocus /></Field>
        <Field label="代码" info="卡片上显示的缩写"><Input value={f.code} onChange={(e) => setF({ ...f, code: e.target.value })} placeholder="如 BWH" /></Field>
        <Field label="面板地址（选填）" className="span-2"><Input value={f.panel_url} onChange={(e) => setF({ ...f, panel_url: e.target.value })} placeholder="https://" /></Field>
        <Field label="账单时区" info="到期日、流量周期按这个时区的日期算" className="span-2"><TzInput value={f.timezone} onChange={(v) => setF({ ...f, timezone: v })} emptyLabel={orgTz()} aria-label="账单时区" /></Field>
        <Field label="流量计量" info="供应商面板怎么算本周期用量（如 GOMAMI 是取较小值）；不清楚就选取较大值，不会少算" className="span-2"><Select value={f.traffic_rule} onChange={(v) => setF({ ...f, traffic_rule: v as TrafficRule })} options={ruleOptions()} /></Field>
        <Field label="流量重置" info="每个流量周期从哪天起（按账单时区）：按账单日 = 每台机器到期日是几号就几号，月份没有这一天按月末"><Select value={f.traffic_reset} onChange={(v) => setF({ ...f, traffic_reset: v as TrafficReset })} options={resetOptions()} /></Field>
        <Field label="到期提醒" info="到期前多少天开始提醒" error={remindBad ? '填 0–60' : undefined}><UnitInput unit="天前" width={120} inputMode="numeric" value={f.remind_days} onChange={(e) => setF({ ...f, remind_days: e.target.value.replace(/\D/g, '') })} /></Field>
      </div>
    </FormDialog>
  );
});

/* ---------------------------------------------------------------- 套餐 */
/** 规格：核数 / 内存，没填的部分显示 “—”；都没填只显示 “—” */
// 与全站一致用 1024 进位的 GiB / MiB（256 MB 内存写 256 MiB，不写 0.3 GB）
const planSpec = (p: Plan) => (!p.cpu_cores && !p.mem_mb ? '—' : `${p.cpu_cores || '—'} 核 / ${p.mem_mb ? F.str(F.mem(p.mem_mb * 1024 * 1024)) : '—'}`);
const emptyPlan = () => ({ name: '', provider_id: '', cpu_cores: '', mem_gb: '', disk_gb: '', bandwidth_mbps: '', traffic_gb: '', price: '', currency: meta().defaults.currency, cycle: meta().defaults.cycle, notes: '' });
type PlanForm = ReturnType<typeof emptyPlan>;
const PLAN_COLS = [
  { id: 'name', header: '套餐', first: 'asc' as const }, { id: 'provider', header: '供应商', first: 'asc' as const }, { id: 'spec', header: '规格' },
  { id: 'bw', header: '带宽', r: true, first: 'desc' as const }, { id: 'traffic', header: '流量', r: true, first: 'desc' as const },
  { id: 'price', header: '价格', r: true, first: 'desc' as const }, { id: 'machines', header: '机器', r: true, first: 'desc' as const }, { id: 'act', header: '' },
];
function PlansTab() {
  const q = usePlans(); const providers = useProviders().data ?? [];
  const patch = usePatchPlan();
  const [sort, setSort] = useUrlSort();
  const [edit, setEdit] = React.useState<Plan | 'new' | null>(null);
  const [askArchive, setAskArchive] = React.useState<Plan | null>(null);
  const [askDel, setAskDel] = React.useState<Plan | null>(null);
  const delPlan = useDelPlan();
  const archiveHeld = useHeld(askArchive?.id, askArchive);
  const [showArchived, setShowArchived] = React.useState(false);
  const held = useHeld(edit === 'new' ? 'new' : edit?.id, edit);
  const pname = (id: string) => providers.find((p) => p.id === id)?.name ?? '—';
  // 流量不限（0 / 空）排在最大
  const list = sortRows((q.data ?? []).filter((p) => showArchived || !p.archived_at), sort, {
    name: (p) => p.name, provider: (p) => pname(p.provider_id), bw: (p) => p.bandwidth_mbps ?? -1, traffic: (p) => p.traffic_gb || Infinity, price: (p) => p.price_cents, machines: (p) => p.machine_count,
  });
  // 归档 / 取消归档：通知里带「撤销」，点了改回去
  const setArchived = (p: Plan, archived: boolean) => patch.mutateAsync({ id: p.id, archived }).then(() => toast.show({
    tone: 'ok', text: `已${archived ? '归档' : '取消归档'}「${p.name}」`,
    action: { label: '撤销', onClick: () => patch.mutate({ id: p.id, archived: !archived }, { onSuccess: () => toast.ok(`已${archived ? '取消归档' : '归档'}「${p.name}」`), onError: toast.err }) },
  }));
  return (
    <>
      {q.isError && <ErrorNote error={q.error} onRetry={() => void q.refetch()} />}
      <Panel title="套餐" count={list.length} sub={<InfoTip text="同一套餐的机器共用这里的流量配额、价格、周期" />} tools={<><CheckRow className="is-label" checked={showArchived} onChange={setShowArchived}>显示已归档</CheckRow><Button variant="primary" onClick={() => setEdit('new')}><Plus />新建套餐</Button></>} flush>
        <TableWrap>
          <table className="dt" style={{ minWidth: 880 }}>
            <colgroup><col /><col style={{ width: 130 }} /><col style={{ width: 120 }} /><col style={{ width: 96 }} /><col style={{ width: 96 }} /><col style={{ width: 110 }} /><col style={{ width: 76 }} /><col style={{ width: 196 }} /></colgroup>
            <SortHead cols={PLAN_COLS} sort={sort} onSort={setSort} />
            <tbody>
              {list.map((p) => (
                <tr key={p.id} className={p.archived_at ? 'is-retired' : undefined}>
                  <td><span className="cell-main strong">{p.name}</span><span className="sub" data-tip={p.archived_at ? undefined : p.notes ?? undefined}>{p.archived_at ? '已归档' : p.notes ?? ''}</span></td>
                  <td>{pname(p.provider_id)}</td>
                  <td>{planSpec(p)}<span className="sub">{p.disk_gb ? `磁盘 ${p.disk_gb} GiB` : ''}</span></td>
                  <td className="r"><Num f={F.mbps(p.bandwidth_mbps)} /></td><td className="r"><Num f={limitGb(p.traffic_gb)} /></td>
                  <td className="r"><span className="num">{F.money(p.price_cents, p.currency)}</span><span className="sub">/ {cycleLabel(p.cycle)}</span></td>
                  <td className="r num"><CountLink n={p.machine_count} to={`/nodes?q=${encodeURIComponent(p.name)}`} /></td>
                  <td className="r"><span className="row" style={{ justifyContent: 'flex-end', flexWrap: 'nowrap' }}>
                    <Button size="sm" variant="quiet" onClick={() => setEdit(p)}><Pencil size={13} />编辑</Button>
                    {p.archived_at
                      ? <Button size="sm" variant="quiet" disabled={patch.isPending} onClick={() => void setArchived(p, false).catch(toast.err)}><ArchiveRestore size={13} />取消归档</Button>
                      : <IconButton size="sm" label="归档" disabled={patch.isPending} onClick={() => (p.machine_count > 0 ? setAskArchive(p) : void setArchived(p, true).catch(toast.err))}><Archive size={14} /></IconButton>}
                    <IconButton size="sm" label="删除套餐" onClick={() => setAskDel(p)}><Trash2 size={14} /></IconButton>
                  </span></td>
                </tr>
              ))}
            </tbody>
          </table>
          {q.isLoading && <SkeletonRows rows={3} />}
          {q.data && list.length === 0 && (q.data.length ? <Empty text="没有在用的套餐" hint="可勾选「显示已归档」" /> : <Empty text="还没有套餐" />)}
        </TableWrap>
      </Panel>
      {held && <PlanDialog plan={held} open={edit != null} onClose={() => setEdit(null)} />}
      <ConfirmDialog open={!!askArchive} onOpenChange={(o) => !o && setAskArchive(null)} title={`归档「${archiveHeld?.name ?? ''}」？`} confirmLabel="归档"
        description={<>有 {archiveHeld?.machine_count} 台机器在用 <InfoTip text="在用的机器不受影响，只是上架时不再列出这个套餐；可以随时取消归档。" /></>}
        onConfirm={() => setArchived(askArchive!, true)} />
      <DeleteDialog what="套餐" item={askDel} onClose={() => setAskDel(null)} run={(id) => delPlan.mutateAsync(id)} />
    </>
  );
}

/** 新建 / 编辑套餐。每次打开重新挂载（resetOnOpen）：字段直接按这次的套餐初始化，f0 记下初值用来判断有没有改动 */
const PlanDialog = resetOnOpen(function PlanDialog({ plan, open, onClose }: { plan: Plan | 'new'; open: boolean; onClose: () => void }) {
  const providers = useProviders().data ?? [];
  const add = useAddPlan(); const patch = usePatchPlan();
  const [f0] = React.useState(() => {
    // 规格字段可能为空：空着显示，别变成 "null" 再存成 0
    const s = (v: number | null) => (v == null ? '' : String(v));
    return plan === 'new' ? emptyPlan() : { name: plan.name, provider_id: plan.provider_id, cpu_cores: s(plan.cpu_cores), mem_gb: s(plan.mem_mb == null ? null : plan.mem_mb / 1024), disk_gb: s(plan.disk_gb), bandwidth_mbps: s(plan.bandwidth_mbps), traffic_gb: s(plan.traffic_gb), price: String(plan.price_cents / 100), currency: plan.currency, cycle: plan.cycle, notes: plan.notes ?? '' };
  });
  const [f, setF] = React.useState(f0);
  const dirty = (Object.keys(f) as (keyof PlanForm)[]).some((k) => f[k] !== f0[k]);
  // 留空 = 不填（null）；PATCH 里 null 会清掉原值
  const n = (s: string) => (s.trim() === '' ? null : Number(s) || 0);
  const mem = n(f.mem_gb);
  const body = () => ({ name: f.name.trim(), provider_id: f.provider_id, cpu_cores: n(f.cpu_cores), mem_mb: mem == null ? null : Math.round(mem * 1024), disk_gb: n(f.disk_gb), bandwidth_mbps: n(f.bandwidth_mbps), traffic_gb: n(f.traffic_gb) || null, price_cents: Math.round((n(f.price) ?? 0) * 100), currency: f.currency, cycle: f.cycle, notes: f.notes.trim() });
  const save = () => {
    const ok = () => { toast.ok(plan === 'new' ? '套餐已创建' : '套餐已保存'); onClose(); };
    if (plan === 'new') add.mutate(body(), { onSuccess: ok, onError: toast.err });
    else patch.mutate({ id: plan.id, ...body() }, { onSuccess: ok, onError: toast.err });
  };
  const num = (k: keyof PlanForm, label: string, ph?: string, info?: string) => <Field label={label} info={info}><Input inputMode="decimal" placeholder={ph} value={f[k]} onChange={(e) => setF({ ...f, [k]: e.target.value.replace(/[^\d.]/g, '') })} /></Field>;
  return (
    <FormDialog size="md" dirty={dirty} open={open} onOpenChange={(o) => !o && onClose()} title={plan === 'new' ? '新建套餐' : '编辑套餐'} submitLabel="保存" busy={add.isPending || patch.isPending} valid={f.name.trim() !== '' && f.provider_id !== ''} onSubmit={save}
      desc={plan !== 'new' && <>{plan.machine_count ? `${plan.machine_count} 台在用，` : ''}改了立即生效 <InfoTip text="套餐是模板：同一套餐的机器共用这里的配额、价格、周期；在「费用」页单独设置过的除外。" /></>}>
      <div className="form-grid">
        <Field label="套餐名"><Input value={f.name} onChange={(e) => setF({ ...f, name: e.target.value })} autoFocus /></Field>
        {/* 机器的供应商就是套餐的：有机器用着时不能换（服务端也拦） */}
        <Field label="供应商" hint={plan !== 'new' && plan.machine_count ? '有机器在用，不能换' : undefined} info={plan !== 'new' && plan.machine_count ? '要换供应商就给别的供应商另建套餐' : undefined}>
          <Select value={f.provider_id} onChange={(v) => setF({ ...f, provider_id: v })} disabled={plan !== 'new' && plan.machine_count > 0} options={providers.map((p) => ({ value: p.id, label: p.name }))} /></Field>
        {num('cpu_cores', 'CPU 核数')}{num('mem_gb', '内存（GiB）')}{num('disk_gb', '磁盘（GiB）')}{num('bandwidth_mbps', '带宽（Mbps）')}
        {num('traffic_gb', '流量（GB / 每个流量周期）', '留空或 0 = 不限', '流量周期按供应商的「流量重置」')}{num('price', '价格（每个计费周期）')}
        <Field label="币种"><Select value={f.currency} onChange={(v) => setF({ ...f, currency: v })} options={F.withCurrent(currencyOptions(), f.currency)} /></Field>
        <Field label="计费周期"><Select value={f.cycle} onChange={(v) => setF({ ...f, cycle: v })} options={cycleOptions()} /></Field>
        <Field label="备注" className="span-2"><Input value={f.notes} onChange={(e) => setF({ ...f, notes: e.target.value })} /></Field>
      </div>
    </FormDialog>
  );
});
