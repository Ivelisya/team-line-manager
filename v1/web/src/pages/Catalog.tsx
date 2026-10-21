// 团队与套餐：团队（改名、排序、重新编号预览→应用）、供应商、套餐。
import * as React from 'react';
import { useQuery } from '@tanstack/react-query';
import { useSearchParams } from 'react-router-dom';
import { Archive, ArchiveRestore, ArrowRight, ExternalLink, ListOrdered, Pencil, Plus } from 'lucide-react';
import { fetchRenumber, useAddPlan, useAddProvider, useAddTeam, usePatchPlan, usePatchProvider, usePatchTeam, usePlans, useProviders, useRenumber, useTeams } from '@/lib/queries';
import type { Plan, Provider, Team } from '@/lib/types';
import * as F from '@/lib/format';
import { Button } from '@/components/ui/button';
import { CheckRow, Field, Input } from '@/components/ui/form';
import { Select } from '@/components/ui/select';
import { Tabs, TabsList, TabsTrigger, TabsContent } from '@/components/ui/overlays';
import { Dialog, DialogContent, DialogDescription, DialogFooter, DialogTitle, DialogHeader } from '@/components/ui/dialog';
import { toast } from '@/components/ui/toast';
import { Chip, Empty, ErrorNote, Note, Num, PageHead, Panel } from '@/components/app/primitives';

function FormDialog({ open, onOpenChange, title, desc, children, onSubmit, submitLabel, busy, valid = true, size }: {
  open: boolean; onOpenChange: (o: boolean) => void; title: string; desc?: React.ReactNode; children: React.ReactNode; onSubmit: () => void; submitLabel: string; busy?: boolean; valid?: boolean; size?: 'sm' | 'md';
}) {
  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent size={size}>
        <form style={{ display: 'grid', gap: 16 }} onSubmit={(e) => { e.preventDefault(); if (valid && !busy) onSubmit(); }}>
          <DialogHeader><DialogTitle>{title}</DialogTitle>{desc && <DialogDescription>{desc}</DialogDescription>}</DialogHeader>
          {children}
          <DialogFooter><Button type="button" onClick={() => onOpenChange(false)}>取消</Button><Button type="submit" variant="primary" loading={busy} disabled={!valid}>{submitLabel}</Button></DialogFooter>
        </form>
      </DialogContent>
    </Dialog>
  );
}

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
  const q = useTeams(); const add = useAddTeam(); const patch = usePatchTeam();
  const [edit, setEdit] = React.useState<Team | 'new' | null>(null);
  const [renum, setRenum] = React.useState<Team | null>(null);
  const [name, setName] = React.useState(''); const [order, setOrder] = React.useState('');
  React.useEffect(() => { if (edit && edit !== 'new') { setName(edit.name); setOrder(String(edit.sort_order)); } else { setName(''); setOrder(''); } }, [edit]);
  const list = [...(q.data ?? [])].sort((a, b) => a.sort_order - b.sort_order);
  const isNew = edit === 'new';
  const save = () => {
    const sort_order = order.trim() === '' ? undefined : Number(order);
    const ok = () => { toast.ok(isNew ? '团队已创建' : '团队已保存'); setEdit(null); };
    if (isNew) add.mutate({ name: name.trim(), sort_order }, { onSuccess: ok, onError: toast.err });
    else if (edit) patch.mutate({ id: edit.id, name: name.trim() !== edit.name ? name.trim() : undefined, sort_order }, { onSuccess: ok, onError: toast.err });
  };
  return (
    <>
      {q.isError && <ErrorNote error={q.error} onRetry={() => void q.refetch()} />}
      <Panel title="团队" count={list.length} sub="首页与表格按“排序”从小到大排列" tools={<Button variant="primary" size="md" onClick={() => setEdit('new')}><Plus />新建团队</Button>} flush>
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
          {q.data && list.length === 0 && <Empty text="还没有团队" />}
        </div>
      </Panel>

      <FormDialog open={edit != null} onOpenChange={(o) => !o && setEdit(null)} title={isNew ? '新建团队' : `修改 ${(edit as Team | null)?.name ?? ''}`} submitLabel="保存" busy={add.isPending || patch.isPending} valid={name.trim().length > 0}
        desc={!isNew && '改团队名会让该团队所有节点的显示名一起变（线路机上的备注自动同步，不换令牌、不断连接）。'} onSubmit={save}>
        <Field label="团队名"><Input value={name} onChange={(e) => setName(e.target.value)} autoFocus /></Field>
        <Field label="排序" hint="数字越小越靠前，留空放在最后"><Input inputMode="numeric" value={order} onChange={(e) => setOrder(e.target.value.replace(/\D/g, ''))} /></Field>
      </FormDialog>
      {renum && <RenumberDialog team={renum} onClose={() => setRenum(null)} />}
    </>
  );
}

/** 重新编号：先预览差异，再一次性应用（token 防止预览后数据变了） */
function RenumberDialog({ team, onClose }: { team: Team; onClose: () => void }) {
  const pv = useQuery({ queryKey: ['renumber', team.id], queryFn: () => fetchRenumber(team.id), gcTime: 0, staleTime: 0 });
  const apply = useRenumber();
  const n = pv.data?.changes.length ?? 0;
  return (
    <Dialog open onOpenChange={(o) => !o && onClose()}>
      <DialogContent size="md">
        <DialogHeader><DialogTitle>重新编号 · {team.name}</DialogTitle>
          <DialogDescription>把在用节点压缩成 01..N（按现有编号先后）。退役时已自动压缩；这里用于手工指定过跳号的情况。</DialogDescription></DialogHeader>
        {pv.isLoading && <div className="skel" style={{ height: 80 }} />}
        {pv.isError && <Note tone="bad">{(pv.error as Error).message}</Note>}
        {pv.data && (n === 0
          ? <Note tone="ok">编号已经连续，不需要调整。</Note>
          : <div className="table-wrap" style={{ maxHeight: 300, overflow: 'auto', border: '1px solid var(--line)', borderRadius: 'var(--r-float)' }}>
            <table className="dt is-one"><colgroup><col /><col style={{ width: 36 }} /><col /></colgroup>
              <thead><tr><th>当前名称</th><th /><th>调整后</th></tr></thead>
              <tbody>{pv.data.changes.map((c) => <tr key={c.machine_id}><td className="num">{c.old_name}</td><td><ArrowRight size={14} style={{ color: 'var(--fg-3)' }} /></td><td className="strong num">{c.new_name}</td></tr>)}</tbody></table>
          </div>)}
        <DialogFooter>
          <Button onClick={onClose}>取消</Button>
          <Button variant="primary" disabled={!pv.data || n === 0} loading={apply.isPending}
            onClick={() => apply.mutate({ id: team.id, token: pv.data!.token }, { onSuccess: () => { toast.ok(`已重新编号：${n} 台`); onClose(); }, onError: (e) => { toast.err(e); void pv.refetch(); } })}>应用 {n > 0 ? `${n} 项改名` : ''}</Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}

/* ---------------------------------------------------------------- 供应商 */
const EMPTY_PROVIDER = { name: '', code: '', panel_url: '', timezone: 'Asia/Shanghai' };
function ProvidersTab() {
  const q = useProviders(); const add = useAddProvider(); const patch = usePatchProvider();
  const [edit, setEdit] = React.useState<Provider | 'new' | null>(null);
  const [f, setF] = React.useState(EMPTY_PROVIDER);
  React.useEffect(() => {
    if (edit && edit !== 'new') setF({ name: edit.name, code: edit.code, panel_url: edit.panel_url ?? '', timezone: edit.timezone ?? '' });
    else setF(EMPTY_PROVIDER);
  }, [edit]);
  const list = q.data ?? [];
  // 面板地址、时区留空：服务端分别按清空、列默认值（UTC）处理
  const save = () => {
    const body = { name: f.name.trim(), code: f.code.trim().toUpperCase(), panel_url: f.panel_url.trim() || null, timezone: f.timezone.trim() || null };
    const ok = () => { toast.ok(edit === 'new' ? '供应商已添加' : '供应商已保存'); setEdit(null); };
    if (edit === 'new') add.mutate(body, { onSuccess: ok, onError: toast.err });
    else if (edit) patch.mutate({ id: edit.id, ...body }, { onSuccess: ok, onError: toast.err });
  };
  return (
    <>
      {q.isError && <ErrorNote error={q.error} onRetry={() => void q.refetch()} />}
      <Panel title="供应商" count={list.length} tools={<Button variant="primary" onClick={() => setEdit('new')}><Plus />新增供应商</Button>} flush>
        <div className="table-wrap">
          <table className="dt is-one" style={{ minWidth: 640 }}>
            <colgroup><col style={{ width: 180 }} /><col style={{ width: 90 }} /><col style={{ width: 180 }} /><col /><col style={{ width: 112 }} /></colgroup>
            <thead><tr><th>名称</th><th>代码</th><th>时区</th><th>面板</th><th /></tr></thead>
            <tbody>{list.map((p) => (
              <tr key={p.id}><td className="strong">{p.name}</td><td><Chip>{p.code}</Chip></td><td>{p.timezone ?? '—'}</td>
                <td>{p.panel_url ? <a href={p.panel_url} target="_blank" rel="noreferrer noopener" className="btn-link btn" style={{ gap: 4 }}>{p.panel_url.replace(/^https?:\/\//, '')}<ExternalLink size={12} /></a> : <span className="t-muted">—</span>}</td>
                <td className="r"><Button size="sm" variant="quiet" onClick={() => setEdit(p)}><Pencil size={13} />编辑</Button></td></tr>
            ))}</tbody>
          </table>
          {q.data && list.length === 0 && <Empty text="还没有供应商" />}
        </div>
      </Panel>
      <FormDialog open={edit != null} onOpenChange={(o) => !o && setEdit(null)} title={edit === 'new' ? '新增供应商' : '编辑供应商'} submitLabel="保存" busy={add.isPending || patch.isPending} valid={f.name.trim() !== '' && f.code.trim() !== ''}
        desc={edit !== 'new' && '改时区只影响之后上架的机器；已上架机器的账单时区在各自的“费用”页修改。'} onSubmit={save}>
        <div className="form-grid">
          <Field label="名称"><Input value={f.name} onChange={(e) => setF({ ...f, name: e.target.value })} autoFocus /></Field>
          <Field label="代码" hint="卡片上显示的缩写，如 BWH"><Input value={f.code} onChange={(e) => setF({ ...f, code: e.target.value })} /></Field>
          <Field label="面板地址（选填）" className="span-2"><Input value={f.panel_url} onChange={(e) => setF({ ...f, panel_url: e.target.value })} placeholder="https://" /></Field>
          <Field label="账单时区" hint="IANA 时区名；留空 = UTC" className="span-2"><Input value={f.timezone} onChange={(e) => setF({ ...f, timezone: e.target.value })} placeholder="UTC" /></Field>
        </div>
      </FormDialog>
    </>
  );
}

/* ---------------------------------------------------------------- 套餐 */
/** 规格：核数 / 内存，没填的部分显示 “—”；都没填只显示 “—” */
const planSpec = (p: Plan) => (!p.cpu_cores && !p.mem_mb ? '—' : `${p.cpu_cores || '—'} 核 / ${p.mem_mb ? +(p.mem_mb / 1024).toFixed(1) : '—'} GB`);
const EMPTY_PLAN = { name: '', provider_id: '', cpu_cores: '', mem_gb: '', disk_gb: '', bandwidth_mbps: '', traffic_gb: '', price: '', currency: 'USD', cycle: 'monthly', notes: '' };
function PlansTab() {
  const q = usePlans(); const providers = useProviders().data ?? [];
  const add = useAddPlan(); const patch = usePatchPlan();
  const [edit, setEdit] = React.useState<Plan | 'new' | null>(null);
  const [showArchived, setShowArchived] = React.useState(false);
  const [f, setF] = React.useState(EMPTY_PLAN);
  React.useEffect(() => {
    // 规格字段可能为空：空着显示，别变成 "null" 再存成 0
    const s = (v: number | null) => (v == null ? '' : String(v));
    if (edit && edit !== 'new') setF({ name: edit.name, provider_id: edit.provider_id, cpu_cores: s(edit.cpu_cores), mem_gb: s(edit.mem_mb == null ? null : edit.mem_mb / 1024), disk_gb: s(edit.disk_gb), bandwidth_mbps: s(edit.bandwidth_mbps), traffic_gb: s(edit.traffic_gb), price: String(edit.price_cents / 100), currency: edit.currency, cycle: edit.cycle, notes: edit.notes ?? '' });
    else setF(EMPTY_PLAN);
  }, [edit]);
  const list = (q.data ?? []).filter((p) => showArchived || !p.archived_at);
  const pname = (id: string) => providers.find((p) => p.id === id)?.name ?? '—';
  // 留空 = 不填（null）；PATCH 里 null 会清掉原值
  const n = (s: string) => (s.trim() === '' ? null : Number(s) || 0);
  const mem = n(f.mem_gb);
  const body = () => ({ name: f.name.trim(), provider_id: f.provider_id, cpu_cores: n(f.cpu_cores), mem_mb: mem == null ? null : Math.round(mem * 1024), disk_gb: n(f.disk_gb), bandwidth_mbps: n(f.bandwidth_mbps), traffic_gb: n(f.traffic_gb) || null, price_cents: Math.round((n(f.price) ?? 0) * 100), currency: f.currency, cycle: f.cycle, notes: f.notes.trim() });
  const save = () => {
    const ok = () => { toast.ok('套餐已保存'); setEdit(null); };
    if (edit === 'new') add.mutate(body(), { onSuccess: ok, onError: toast.err });
    else if (edit) patch.mutate({ id: edit.id, ...body() }, { onSuccess: ok, onError: toast.err });
  };
  const num = (k: keyof typeof EMPTY_PLAN, label: string, hint?: string) => <Field label={label} hint={hint}><Input inputMode="decimal" value={f[k]} onChange={(e) => setF({ ...f, [k]: e.target.value.replace(/[^\d.]/g, '') })} /></Field>;
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
                  <td>{planSpec(p)}<span className="sub">{p.disk_gb ? `磁盘 ${p.disk_gb} GB` : ''}</span></td>
                  <td className="r"><Num f={F.mbps(p.bandwidth_mbps)} /></td><td className="r"><Num f={p.traffic_gb ? F.gb(p.traffic_gb) : { v: '不限', u: '' }} /></td>
                  <td className="r"><span className="num">{F.money(p.price_cents, p.currency)}</span><span className="sub">/ {F.cycleLabel(p.cycle)}</span></td>
                  <td className="r num">{p.machine_count}</td>
                  <td className="r"><span className="row" style={{ justifyContent: 'flex-end', flexWrap: 'nowrap' }}>
                    <Button size="sm" variant="quiet" onClick={() => setEdit(p)}><Pencil size={13} />编辑</Button>
                    <Button size="sm" variant="quiet" aria-label={p.archived_at ? '取消归档' : '归档'} disabled={patch.isPending} onClick={() => patch.mutate({ id: p.id, archived: !p.archived_at }, { onSuccess: () => toast.ok(p.archived_at ? '已取消归档' : '已归档'), onError: toast.err })}>{p.archived_at ? <ArchiveRestore size={13} /> : <Archive size={13} />}</Button></span></td>
                </tr>
              ))}
            </tbody>
          </table>
          {q.data && list.length === 0 && <Empty text="还没有套餐" />}
        </div>
      </Panel>
      <FormDialog size="md" open={edit != null} onOpenChange={(o) => !o && setEdit(null)} title={edit === 'new' ? '新增套餐' : '编辑套餐'} submitLabel="保存" busy={add.isPending || patch.isPending} valid={f.name.trim() !== '' && f.provider_id !== ''} onSubmit={save}>
        <div className="form-grid">
          <Field label="套餐名"><Input value={f.name} onChange={(e) => setF({ ...f, name: e.target.value })} autoFocus /></Field>
          <Field label="供应商"><Select value={f.provider_id} onChange={(v) => setF({ ...f, provider_id: v })} options={providers.map((p) => ({ value: p.id, label: p.name }))} /></Field>
          {num('cpu_cores', 'CPU 核数')}{num('mem_gb', '内存（GB）')}{num('disk_gb', '磁盘（GB）')}{num('bandwidth_mbps', '带宽（Mbps）')}
          {num('traffic_gb', '流量（GiB / 周期）', '留空或 0 = 不限')}{num('price', '价格')}
          <Field label="币种"><Select value={f.currency} onChange={(v) => setF({ ...f, currency: v })} options={['CNY', 'USD', 'EUR', 'HKD', 'JPY'].map((c) => ({ value: c, label: c }))} /></Field>
          <Field label="计费周期"><Select value={f.cycle} onChange={(v) => setF({ ...f, cycle: v })} options={F.CYCLE_OPTIONS} /></Field>
          <Field label="备注" className="span-2"><Input value={f.notes} onChange={(e) => setF({ ...f, notes: e.target.value })} /></Field>
        </div>
      </FormDialog>
    </>
  );
}
