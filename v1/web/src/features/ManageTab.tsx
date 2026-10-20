// 抽屉“管理”页：改名/改编号（带实时预览）、入口操作、下线（退役）、彻底删除（仅已退役）。
import * as React from 'react';
import { Link } from 'react-router-dom';
import { ArrowRight, Plus, Trash2 } from 'lucide-react';
import type { MachineView } from '@/lib/types';
import { useAddEntry, usePatchMachine, usePurge, useRelays, useRetire, useRetirePreview, useTeams, useUpgradeAgent } from '@/lib/queries';
import * as F from '@/lib/format';
import { Button } from '@/components/ui/button';
import { CheckRow, Field, Input } from '@/components/ui/form';
import { Select } from '@/components/ui/select';
import { toast } from '@/components/ui/toast';
import { entryState, Note, Section, SectionHeader } from '@/components/app/primitives';
import { TypedConfirmDialog } from '@/components/app/dialogs';
import { EntryActions, entryLabel } from '@/components/app/EntryActions';

export function ManageTab({ m, onClosed }: { m: MachineView; onClosed: () => void }) {
  const retired = m.lifecycle === 'retired';
  return (
    <>
      {!retired && <RenameSection m={m} />}
      {m.entries.length > 0 && !retired && <EntrySection m={m} />}
      {m.agent && m.lifecycle === 'active' && <AgentSection m={m} />}
      <DangerSection m={m} onClosed={onClosed} />
    </>
  );
}

/** 改名 / 改编号：团队 + 编号（节点）或名称（线路机），实时预览新显示名 */
function RenameSection({ m }: { m: MachineView }) {
  const teams = useTeams().data ?? [];
  const patch = usePatchMachine();
  const isRelay = m.kind === 'relay';
  const [team, setTeam] = React.useState(m.team?.id ?? '');
  const [num, setNum] = React.useState(m.number != null ? String(m.number) : '');
  const [label, setLabel] = React.useState(m.label ?? '');
  React.useEffect(() => { setTeam(m.team?.id ?? ''); setNum(m.number != null ? String(m.number) : ''); setLabel(m.label ?? ''); }, [m.id, m.revision, m.team?.id, m.number, m.label]);

  const teamName = teams.find((t) => t.id === team)?.name;
  const n = Number(num);
  const numOk = Number.isInteger(n) && n >= 1 && n <= 999;
  const preview = isRelay ? label.trim() : teamName && numOk ? F.displayName(teamName, n) : '';
  const changed = isRelay ? label.trim() !== (m.label ?? '') : team !== (m.team?.id ?? '') || (numOk && n !== m.number);
  const valid = isRelay ? label.trim().length > 0 : !!team && numOk;

  const save = () => {
    const body = isRelay ? { label: label.trim() } : { team_id: team, number: n };
    patch.mutate({ id: m.id, revision: m.revision, ...body }, { onSuccess: () => toast.ok(`已改名为 ${preview}`), onError: toast.err });
  };
  return (
    <Section>
      <SectionHeader title={isRelay ? '改名' : '改名 / 改编号'} />
      <div className="form-grid">
        {isRelay ? (
          <Field label="线路机名称" className="span-2"><Input value={label} onChange={(e) => setLabel(e.target.value)} /></Field>
        ) : (
          <>
            <Field label="团队"><Select value={team} onChange={setTeam} options={teams.map((t) => ({ value: t.id, label: t.name }))} /></Field>
            <Field label="编号" hint="编号在团队内连续；已被占用时会提示" error={num && !numOk ? '编号应为 1–99 的整数' : undefined}>
              <Input inputMode="numeric" value={num} onChange={(e) => setNum(e.target.value.replace(/\D/g, ''))} aria-invalid={!!num && !numOk} />
            </Field>
          </>
        )}
        <div className="span-2 row row-between">
          <span className="row" style={{ gap: 8 }}>
            <span className="t-sub">新显示名</span>
            <span className="preview-name">{m.name}<ArrowRight size={14} style={{ color: 'var(--fg-3)' }} />{preview || '—'}</span>
          </span>
          <Button variant="primary" disabled={!changed || !valid} loading={patch.isPending} onClick={save}>保存</Button>
        </div>
        <div className="hint span-2">改名只改显示名与线路机上的备注，不换令牌，不会断开现有连接。</div>
      </div>
    </Section>
  );
}

function EntrySection({ m }: { m: MachineView }) {
  const relays = useRelays().data ?? [];
  const add = useAddEntry();
  const [relay, setRelay] = React.useState('');
  const used = new Set(m.entries.map((e) => e.relay.id));
  const opts = relays.filter((r) => !used.has(r.id)).map((r) => ({ value: r.id, label: r.name }));
  return (
    <Section>
      <SectionHeader title="入口" meta="停用 / 启用 / 轮换凭据 / 分享链接" />
      <div className="stack-sm">
        {m.entries.map((e) => (
          <div key={e.id} className="manage-row" style={{ border: '1px solid var(--line)', borderRadius: 'var(--r-float)', padding: '8px 12px' }}>
            <div><div className="t">{m.kind === 'relay' ? `${e.port} · ${e.node.name}` : entryLabel(e)}</div><div className="hint">{e.apply_error ?? (e.applied ? '已下发到线路机' : '等待下发')}</div></div>
            <span className="row" style={{ gap: 8 }}>{entryState(e)}<EntryActions entry={e} /></span>
          </div>
        ))}
        {m.kind !== 'relay' && opts.length > 0 && (
          <div className="row">
            <div style={{ width: 200 }}><Select value={relay} onChange={setRelay} options={opts} placeholder="再挂到另一台线路机" /></div>
            <Button disabled={!relay} loading={add.isPending} onClick={() => add.mutate({ id: m.id, relay_id: relay }, { onSuccess: () => { toast.ok('已提交，新入口正在下发'); setRelay(''); }, onError: toast.err })}><Plus />添加入口</Button>
          </div>
        )}
      </div>
    </Section>
  );
}

/** agent：版本、最近上报；升级 = 推送中控当前版本并重启（作业） */
function AgentSection({ m }: { m: MachineView }) {
  const up = useUpgradeAgent();
  const a = m.agent!;
  return (
    <Section>
      <SectionHeader title="agent" meta={a.online ? '在线' : '离线'} />
      <div className="manage-row">
        <div>
          <div className="t">版本 {a.version ?? '—'}</div>
          <div className="hint">最近上报 {a.last_report_at ? F.ago(a.last_report_at) : '从未'}。升级会推送中控上的当前版本并重启 agent，不影响线路上的连接。</div>
        </div>
        <Button variant="secondary" loading={up.isPending} disabled={!!m.active_job}
          onClick={() => up.mutate(m.id, { onSuccess: () => toast.ok('已提交升级作业'), onError: toast.err })}>升级 agent</Button>
      </div>
    </Section>
  );
}

function DangerSection({ m, onClosed }: { m: MachineView; onClosed: () => void }) {
  const [dlg, setDlg] = React.useState<null | 'retire' | 'purge'>(null);
  const retire = useRetire(); const purge = usePurge();
  const [cleanup, setCleanup] = React.useState(true);
  const [reason, setReason] = React.useState('');
  const pv = useRetirePreview(m.id, dlg === 'retire');
  const busyJob = m.lifecycle === 'provisioning' || m.lifecycle === 'retiring';
  const retired = m.lifecycle === 'retired';

  return (
    <Section>
      <SectionHeader title={retired ? '已退役' : '下线'} />
      <div className="danger-zone">
        {retired ? (
          <div className="manage-row">
            <div>
              <div className="t">彻底删除</div>
              <div className="hint">已于 {F.dtFull(m.retired_at)} 退役。退役不支持恢复；彻底删除会清掉这台机器的记录与遥测，审计记录保留。</div>
            </div>
            <Button variant="outline-danger" onClick={() => setDlg('purge')}><Trash2 />彻底删除…</Button>
          </div>
        ) : (
          <div className="manage-row">
            <div>
              <div className="t">下线（退役）</div>
              <div className="hint">停用入口、吊销凭据，机器进入“已退役”，不再出现在首页和统计里。{m.kind !== 'relay' && '同团队后面的节点会自动前移补号。'}</div>
            </div>
            <Button variant="outline-danger" disabled={busyJob} onClick={() => setDlg('retire')}>下线…</Button>
          </div>
        )}
        {busyJob && m.active_job && <Note tone="info">作业进行中，暂不能操作。<Link to={`/jobs/${m.active_job.id}`} style={{ color: 'var(--accent)' }}>查看作业</Link></Note>}
      </div>

      <TypedConfirmDialog open={dlg === 'retire'} onOpenChange={(o) => !o && setDlg(null)} size="md" title={`下线 ${m.name}？`} expected={m.name} prompt="请输入显示名" confirmLabel="下线" danger
        description="下线后不可恢复。入口会立即停用，已有连接会断开。"
        onConfirm={async () => { const r = await retire.mutateAsync({ id: m.id, confirm: m.name, cleanup, reason: reason.trim() || undefined }); toast.ok('已提交下线作业'); onClosed(); return r; }}>
        {pv.data && pv.data.renames.length > 0 && (
          <Note tone="warn">
            <div style={{ fontWeight: 500, marginBottom: 4 }}>下线后以下节点会改名（编号保持连续）：</div>
            <ul className="reason-list" style={{ maxHeight: 200, overflow: 'auto' }}>
              {pv.data.renames.map((r) => <li key={r.machine_id}><span className="num">{r.old_name} <ArrowRight size={12} style={{ display: 'inline', verticalAlign: -1 }} /> <b>{r.new_name}</b></span></li>)}
            </ul>
          </Note>
        )}
        {m.kind !== 'relay' && <CheckRow checked={cleanup} onChange={setCleanup}>清理节点上的服务（卸载 agent 与代理服务）</CheckRow>}
        <Field label="原因（选填）"><Input value={reason} onChange={(e) => setReason(e.target.value)} placeholder="例如：供应商到期不续" /></Field>
      </TypedConfirmDialog>

      <TypedConfirmDialog open={dlg === 'purge'} onOpenChange={(o) => !o && setDlg(null)} title={`彻底删除 ${m.name}？`} expected={m.name} prompt="请输入显示名" confirmLabel="彻底删除" danger
        description="会删除机器记录与全部遥测数据，无法找回；审计记录保留。"
        onConfirm={async () => { await purge.mutateAsync({ id: m.id, confirm: m.name }); toast.ok('已提交删除作业'); onClosed(); }} />
    </Section>
  );
}
