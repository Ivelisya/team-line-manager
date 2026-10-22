// 团队与套餐：团队（改名、排序、重新编号预览→应用）、供应商、套餐。
import * as React from 'react';
import { useQuery } from '@tanstack/react-query';
import { useSearchParams } from 'react-router-dom';
import { Archive, ArchiveRestore, ArrowRight, ExternalLink, ListOrdered, Pencil, Plus } from 'lucide-react';
import { fetchRenumber, useAddPlan, useAddProvider, useAddTeam, usePatchPlan, usePatchProvider, usePatchTeam, usePlans, useProviders, useRenumber, useTeams } from '@/lib/queries';
import type { Plan, Provider, Team, TrafficRule } from '@/lib/types';
import { limitGb, RULE_LABEL } from '@/lib/derive';
import * as F from '@/lib/format';
import { Button, IconButton } from '@/components/ui/button';
import { CheckRow, Field, Input } from '@/components/ui/form';
import { Select } from '@/components/ui/select';
import { Tabs, TabsList, TabsTrigger, TabsContent } from '@/components/ui/overlays';
import { Dialog, DialogContent, DialogDescription, DialogFooter, DialogTitle, DialogHeader } from '@/components/ui/dialog';
import { SkeletonRows } from '@/components/ui/skeleton';
import { toast } from '@/components/ui/toast';
import { resetOnOpen, useHeld } from '@/lib/useHeld';
import { Chip, Empty, ErrorNote, Note, Num, PageHead, Panel } from '@/components/app/primitives';
import { FormDialog } from '@/components/app/dialogs';

const SECTIONS = ['teams', 'providers', 'plans'];

export function CatalogPage() {
  // 子页写进 URL 的 section（同设置页），刷新 / 分享链接停在原处
  const [sp, setSp] = useSearchParams();
  const tab = SECTIONS.find((s) => s === sp.get('section')) ?? 'teams';
  return (
    <div className="stack">
      <PageHead title="团队与套餐" />
      <Tabs value={tab} onValueChange={(v) => setSp((p) => { const n = new URLSearchParams(p); n.set('section', v); return n; }, { replace: true })}>
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

/* ---------------------------------------------------------------- 团队 */
function TeamsTab() {
  const q = useTeams();
  const [edit, setEdit] = React.useState<Team | 'new' | null>(null);
  const [renum, setRenum] = React.useState<Team | null>(null);
  // 关闭动画期间沿用最近的对象，标题与内容不闪变
  const held = useHeld(edit === 'new' ? 'new' : edit?.id, edit);
  const renumHeld = useHeld(renum?.id, renum);
  const list = [...(q.data ?? [])].sort((a, b) => a.sort_order - b.sort_order);
  return (
    <>
      {q.isError && <ErrorNote error={q.error} onRetry={() => void q.refetch()} />}
      <Panel title="团队" count={list.length} sub="首页与表格按「排序」从小到大排列" tools={<Button variant="primary" size="md" onClick={() => setEdit('new')}><Plus />新建团队</Button>} flush>
        <div className="table-wrap">
          <table className="dt is-one" style={{ minWidth: 520 }}>
            <colgroup><col /><col style={{ width: 90 }} /><col style={{ width: 90 }} /><col style={{ width: 220 }} /></colgroup>
            <thead><tr><th>名称</th><th className="r">排序</th><th className="r">在用节点</th><th /></tr></thead>
            <tbody>
              {list.map((t) => (
                <tr key={t.id}><td className="strong">{t.name}</td><td className="r num">{t.sort_order}</td><td className="r num">{t.node_count}</td>
                  <td className="r"><span className="row" style={{ justifyContent: 'flex-end', flexWrap: 'nowrap' }}>
                    <Button size="sm" variant="quiet" onClick={() => setEdit(t)}><Pencil size={13} />改名 / 排序</Button>
                    <Button size="sm" onClick={() => setRenum(t)}><ListOrdered size={13} />重新编号</Button></span></td></tr>
              ))}
            </tbody>
          </table>
          {q.isLoading && <SkeletonRows rows={3} one />}
          {q.data && list.length === 0 && <Empty text="还没有团队" />}
        </div>
      </Panel>

      {held && <TeamDialog team={held} open={edit != null} onClose={() => setEdit(null)} />}
      {renumHeld && <RenumberDialog team={renumHeld} open={!!renum} onClose={() => setRenum(null)} />}
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
      desc={team !== 'new' && '改团队名会让该团队所有节点的显示名一起变（线路机上的备注自动同步，不换令牌、不断连接）。'} onSubmit={save}>
      <Field label="团队名"><Input value={name} onChange={(e) => setName(e.target.value)} autoFocus /></Field>
      <Field label="排序" hint="数字越小越靠前，留空放在最后"><Input inputMode="numeric" value={order} onChange={(e) => setOrder(e.target.value.replace(/\D/g, ''))} /></Field>
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
          <DialogDescription>把在用节点压缩成 01..N（按现有编号先后）。退役时已自动压缩；这里用于手工指定过跳号的情况。</DialogDescription></DialogHeader>
        {!pv.isFetchedAfterMount && <div className="skel" style={{ height: 80 }} />}
        {pv.isFetchedAfterMount && pv.isError && <Note tone="bad">{(pv.error as Error).message}</Note>}
        {d && (n === 0
          ? <Note tone="ok">编号已经连续，不需要调整。</Note>
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
const EMPTY_PROVIDER = { name: '', code: '', panel_url: '', timezone: 'Asia/Shanghai', traffic_rule: 'max' as TrafficRule };
/** 流量计量下拉：供应商怎么算本周期用量（网卡入、出总量） */
const RULE_OPTIONS = (['min', 'max', 'sum', 'out'] as const).map((value) => ({ value, label: RULE_LABEL[value] }));
function ProvidersTab() {
  const q = useProviders();
  const [edit, setEdit] = React.useState<Provider | 'new' | null>(null);
  const held = useHeld(edit === 'new' ? 'new' : edit?.id, edit);
  const list = q.data ?? [];
  return (
    <>
      {q.isError && <ErrorNote error={q.error} onRetry={() => void q.refetch()} />}
      <Panel title="供应商" count={list.length} tools={<Button variant="primary" onClick={() => setEdit('new')}><Plus />新增供应商</Button>} flush>
        <div className="table-wrap">
          <table className="dt is-one" style={{ minWidth: 780 }}>
            <colgroup><col style={{ width: 180 }} /><col style={{ width: 90 }} /><col style={{ width: 180 }} /><col style={{ width: 130 }} /><col /><col style={{ width: 112 }} /></colgroup>
            <thead><tr><th>名称</th><th>代码</th><th>时区</th><th>流量计量</th><th>面板</th><th /></tr></thead>
            <tbody>{list.map((p) => (
              <tr key={p.id}><td className="strong">{p.name}</td><td><Chip>{p.code}</Chip></td><td>{p.timezone ?? '—'}</td><td>{RULE_LABEL[p.traffic_rule] ?? p.traffic_rule}</td>
                <td>{p.panel_url ? <a href={p.panel_url} target="_blank" rel="noreferrer noopener" className="btn-link btn" style={{ gap: 4 }}>{p.panel_url.replace(/^https?:\/\//, '')}<ExternalLink size={12} /></a> : <span className="t-muted">—</span>}</td>
                <td className="r"><Button size="sm" variant="quiet" onClick={() => setEdit(p)}><Pencil size={13} />编辑</Button></td></tr>
            ))}</tbody>
          </table>
          {q.isLoading && <SkeletonRows rows={3} one />}
          {q.data && list.length === 0 && <Empty text="还没有供应商" />}
        </div>
      </Panel>
      {held && <ProviderDialog provider={held} open={edit != null} onClose={() => setEdit(null)} />}
    </>
  );
}

/** 新增 / 编辑供应商。每次打开重新挂载（resetOnOpen）：字段直接按这次的供应商初始化 */
const ProviderDialog = resetOnOpen(function ProviderDialog({ provider, open, onClose }: { provider: Provider | 'new'; open: boolean; onClose: () => void }) {
  const add = useAddProvider(); const patch = usePatchProvider();
  const [f, setF] = React.useState(() => (provider === 'new' ? EMPTY_PROVIDER : { name: provider.name, code: provider.code, panel_url: provider.panel_url ?? '', timezone: provider.timezone ?? '', traffic_rule: provider.traffic_rule }));
  // 面板地址、时区留空：服务端分别按清空、列默认值（UTC）处理
  const save = () => {
    const body = { name: f.name.trim(), code: f.code.trim().toUpperCase(), panel_url: f.panel_url.trim() || null, timezone: f.timezone.trim() || null, traffic_rule: f.traffic_rule };
    const ok = () => { toast.ok(provider === 'new' ? '供应商已添加' : '供应商已保存'); onClose(); };
    if (provider === 'new') add.mutate(body, { onSuccess: ok, onError: toast.err });
    else patch.mutate({ id: provider.id, ...body }, { onSuccess: ok, onError: toast.err });
  };
  return (
    <FormDialog open={open} onOpenChange={(o) => !o && onClose()} title={provider === 'new' ? '新增供应商' : '编辑供应商'} submitLabel="保存" busy={add.isPending || patch.isPending} valid={f.name.trim() !== '' && f.code.trim() !== ''}
      desc={provider !== 'new' && '改时区只影响之后上架的机器；已上架机器的账单时区在各自的「费用」页修改。'} onSubmit={save}>
      <div className="form-grid">
        <Field label="名称"><Input value={f.name} onChange={(e) => setF({ ...f, name: e.target.value })} autoFocus /></Field>
        <Field label="代码" hint="卡片上显示的缩写，如 BWH"><Input value={f.code} onChange={(e) => setF({ ...f, code: e.target.value })} /></Field>
        <Field label="面板地址（选填）" className="span-2"><Input value={f.panel_url} onChange={(e) => setF({ ...f, panel_url: e.target.value })} placeholder="https://" /></Field>
        <Field label="账单时区" hint="IANA 时区名；留空 = UTC" className="span-2"><Input value={f.timezone} onChange={(e) => setF({ ...f, timezone: e.target.value })} placeholder="UTC" /></Field>
        <Field label="流量计量" hint="供应商面板怎么算本周期用量（如 GOMAMI 是入出取小）；不清楚选取大，不会少算" className="span-2"><Select value={f.traffic_rule} onChange={(v) => setF({ ...f, traffic_rule: v as TrafficRule })} options={RULE_OPTIONS} /></Field>
      </div>
    </FormDialog>
  );
});

/* ---------------------------------------------------------------- 套餐 */
/** 规格：核数 / 内存，没填的部分显示 “—”；都没填只显示 “—” */
// 与全站一致用 1024 进位的 GiB / MiB（256 MB 内存写 256 MiB，不写 0.3 GB）
const planSpec = (p: Plan) => (!p.cpu_cores && !p.mem_mb ? '—' : `${p.cpu_cores || '—'} 核 / ${p.mem_mb ? F.str(F.bytes(p.mem_mb * 1024 * 1024)) : '—'}`);
const EMPTY_PLAN = { name: '', provider_id: '', cpu_cores: '', mem_gb: '', disk_gb: '', bandwidth_mbps: '', traffic_gb: '', price: '', currency: 'USD', cycle: 'monthly', notes: '' };
function PlansTab() {
  const q = usePlans(); const providers = useProviders().data ?? [];
  const patch = usePatchPlan();
  const [edit, setEdit] = React.useState<Plan | 'new' | null>(null);
  const [showArchived, setShowArchived] = React.useState(false);
  const held = useHeld(edit === 'new' ? 'new' : edit?.id, edit);
  const list = (q.data ?? []).filter((p) => showArchived || !p.archived_at);
  const pname = (id: string) => providers.find((p) => p.id === id)?.name ?? '—';
  return (
    <>
      {q.isError && <ErrorNote error={q.error} onRetry={() => void q.refetch()} />}
      <Panel title="套餐" count={list.length} tools={<><CheckRow className="is-label" checked={showArchived} onChange={setShowArchived}>显示已归档</CheckRow><Button variant="primary" onClick={() => setEdit('new')}><Plus />新增套餐</Button></>} flush>
        <div className="table-wrap">
          <table className="dt" style={{ minWidth: 860 }}>
            <colgroup><col style={{ width: 200 }} /><col style={{ width: 120 }} /><col style={{ width: 150 }} /><col style={{ width: 100 }} /><col style={{ width: 100 }} /><col style={{ width: 110 }} /><col style={{ width: 70 }} /><col style={{ width: 120 }} /></colgroup>
            <thead><tr><th>套餐</th><th>供应商</th><th>规格</th><th className="r">带宽</th><th className="r">流量</th><th className="r">价格</th><th className="r">机器</th><th /></tr></thead>
            <tbody>
              {list.map((p) => (
                <tr key={p.id} className={p.archived_at ? 'is-retired' : undefined}>
                  <td><span className="cell-main strong">{p.name}</span><span className="sub">{p.archived_at ? '已归档' : p.notes ?? ''}</span></td>
                  <td>{pname(p.provider_id)}</td>
                  <td>{planSpec(p)}<span className="sub">{p.disk_gb ? `磁盘 ${p.disk_gb} GiB` : ''}</span></td>
                  <td className="r"><Num f={F.mbps(p.bandwidth_mbps)} /></td><td className="r"><Num f={limitGb(p.traffic_gb)} /></td>
                  <td className="r"><span className="num">{F.money(p.price_cents, p.currency)}</span><span className="sub">/ {F.cycleLabel(p.cycle)}</span></td>
                  <td className="r num">{p.machine_count}</td>
                  <td className="r"><span className="row" style={{ justifyContent: 'flex-end', flexWrap: 'nowrap' }}>
                    <Button size="sm" variant="quiet" onClick={() => setEdit(p)}><Pencil size={13} />编辑</Button>
                    <IconButton size="sm" label={p.archived_at ? '取消归档' : '归档'} disabled={patch.isPending} onClick={() => patch.mutate({ id: p.id, archived: !p.archived_at }, { onSuccess: () => toast.ok(p.archived_at ? '已取消归档' : '已归档'), onError: toast.err })}>{p.archived_at ? <ArchiveRestore size={14} /> : <Archive size={14} />}</IconButton></span></td>
                </tr>
              ))}
            </tbody>
          </table>
          {q.isLoading && <SkeletonRows rows={3} />}
          {q.data && list.length === 0 && <Empty text="还没有套餐" />}
        </div>
      </Panel>
      {held && <PlanDialog plan={held} open={edit != null} onClose={() => setEdit(null)} />}
    </>
  );
}

/** 新增 / 编辑套餐。每次打开重新挂载（resetOnOpen）：字段直接按这次的套餐初始化，f0 记下初值用来判断有没有改动 */
const PlanDialog = resetOnOpen(function PlanDialog({ plan, open, onClose }: { plan: Plan | 'new'; open: boolean; onClose: () => void }) {
  const providers = useProviders().data ?? [];
  const add = useAddPlan(); const patch = usePatchPlan();
  const [f0] = React.useState(() => {
    // 规格字段可能为空：空着显示，别变成 "null" 再存成 0
    const s = (v: number | null) => (v == null ? '' : String(v));
    return plan === 'new' ? EMPTY_PLAN : { name: plan.name, provider_id: plan.provider_id, cpu_cores: s(plan.cpu_cores), mem_gb: s(plan.mem_mb == null ? null : plan.mem_mb / 1024), disk_gb: s(plan.disk_gb), bandwidth_mbps: s(plan.bandwidth_mbps), traffic_gb: s(plan.traffic_gb), price: String(plan.price_cents / 100), currency: plan.currency, cycle: plan.cycle, notes: plan.notes ?? '' };
  });
  const [f, setF] = React.useState(f0);
  const dirty = (Object.keys(f) as (keyof typeof EMPTY_PLAN)[]).some((k) => f[k] !== f0[k]);
  // 留空 = 不填（null）；PATCH 里 null 会清掉原值
  const n = (s: string) => (s.trim() === '' ? null : Number(s) || 0);
  const mem = n(f.mem_gb);
  const body = () => ({ name: f.name.trim(), provider_id: f.provider_id, cpu_cores: n(f.cpu_cores), mem_mb: mem == null ? null : Math.round(mem * 1024), disk_gb: n(f.disk_gb), bandwidth_mbps: n(f.bandwidth_mbps), traffic_gb: n(f.traffic_gb) || null, price_cents: Math.round((n(f.price) ?? 0) * 100), currency: f.currency, cycle: f.cycle, notes: f.notes.trim() });
  const save = () => {
    const ok = () => { toast.ok('套餐已保存'); onClose(); };
    if (plan === 'new') add.mutate(body(), { onSuccess: ok, onError: toast.err });
    else patch.mutate({ id: plan.id, ...body() }, { onSuccess: ok, onError: toast.err });
  };
  const num = (k: keyof typeof EMPTY_PLAN, label: string, hint?: string) => <Field label={label} hint={hint}><Input inputMode="decimal" value={f[k]} onChange={(e) => setF({ ...f, [k]: e.target.value.replace(/[^\d.]/g, '') })} /></Field>;
  return (
    <FormDialog size="md" dirty={dirty} open={open} onOpenChange={(o) => !o && onClose()} title={plan === 'new' ? '新增套餐' : '编辑套餐'} submitLabel="保存" busy={add.isPending || patch.isPending} valid={f.name.trim() !== '' && f.provider_id !== ''} onSubmit={save}>
      <div className="form-grid">
        <Field label="套餐名"><Input value={f.name} onChange={(e) => setF({ ...f, name: e.target.value })} autoFocus /></Field>
        <Field label="供应商"><Select value={f.provider_id} onChange={(v) => setF({ ...f, provider_id: v })} options={providers.map((p) => ({ value: p.id, label: p.name }))} /></Field>
        {num('cpu_cores', 'CPU 核数')}{num('mem_gb', '内存（GiB）')}{num('disk_gb', '磁盘（GiB）')}{num('bandwidth_mbps', '带宽（Mbps）')}
        {num('traffic_gb', '流量（GB / 周期）', '留空或 0 = 不限')}{num('price', '价格')}
        <Field label="币种"><Select value={f.currency} onChange={(v) => setF({ ...f, currency: v })} options={F.withCurrent(F.CURRENCY_OPTIONS, f.currency)} /></Field>
        <Field label="计费周期"><Select value={f.cycle} onChange={(v) => setF({ ...f, cycle: v })} options={F.CYCLE_OPTIONS} /></Field>
        <Field label="备注" className="span-2"><Input value={f.notes} onChange={(e) => setF({ ...f, notes: e.target.value })} /></Field>
      </div>
    </FormDialog>
  );
});
