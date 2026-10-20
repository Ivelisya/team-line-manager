// 抽屉“管理”页：改名/改编号（带实时预览）、入口操作、下线（退役）、彻底删除（仅已退役）。
import * as React from 'react';
import { Link } from 'react-router-dom';
import { ArrowRight, PackageOpen, Plus, Repeat2, Trash2, Undo2 } from 'lucide-react';
import type { MachineView } from '@/lib/types';
import { useAddEntry, usePatchMachine, usePurge, useRelayChoices, useTeams, useUpgradeAgent } from '@/lib/queries';
import * as F from '@/lib/format';
import { Button } from '@/components/ui/button';
import { Field, Input } from '@/components/ui/form';
import { Select } from '@/components/ui/select';
import { toast } from '@/components/ui/toast';
import { entryState, Note, Section, SectionHeader } from '@/components/app/primitives';
import { TypedConfirmDialog } from '@/components/app/dialogs';
import { RetireDialog } from '@/components/app/RetireDialog';
import { EntryActions, entryLabel } from '@/components/app/EntryActions';
import { EvacuateDialog, MoveDialog, RestoreDialog, movableEntries } from '@/components/app/LifecycleDialogs';

export function ManageTab({ m, onClosed }: { m: MachineView; onClosed: () => void }) {
  const retired = m.lifecycle === 'retired';
  return (
    <>
      {!retired && <RenameSection m={m} />}
      {/* 节点（落地机/家宽代理）没有入口时也要能“添加入口”；线路机的入口来自节点，没有入口就不显示 */}
      {!retired && (m.kind === 'relay' ? m.entries.length > 0 : m.lifecycle === 'active' || m.entries.length > 0) && <EntrySection m={m} />}
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
  const numOk = Number.isInteger(n) && n >= 1 && n <= 999; // 与服务端一致：编号范围 1–999
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
            <Field label="编号" hint="编号在团队内连续；已被占用时会提示" error={num && !numOk ? '编号应为 1–999 的整数' : undefined}>
              <Input inputMode="numeric" value={num} onChange={(e) => setNum(e.target.value.replace(/\D/g, ''))} aria-invalid={!!num && !numOk} />
            </Field>
          </>
        )}
        <div className="span-2 row row-between">
          <span className="row" style={{ gap: 8 }}>
            <span className="t-sub">新显示名</span>
            <span className="preview-name">{m.name}<ArrowRight size={14} style={{ color: 'var(--fg-3)' }} />{preview || '—'}</span>
          </span>
          <Button variant="primary" disabled={!changed || !valid || patch.isPending} loading={patch.isPending} onClick={save}>保存</Button>
        </div>
        <div className="hint span-2">改名只改显示名与线路机上的备注，不换令牌，不会断开现有连接。</div>
      </div>
    </Section>
  );
}

function EntrySection({ m }: { m: MachineView }) {
  const relays = useRelayChoices().data ?? [];
  const add = useAddEntry();
  const [relay, setRelay] = React.useState('');
  const [dlg, setDlg] = React.useState<null | 'move' | 'evacuate'>(null);
  const used = new Set(m.entries.map((e) => e.relay.id));
  const opts = relays.filter((r) => !used.has(r.id)).map((r) => ({ value: r.id, label: r.name }));
  return (
    <Section>
      <SectionHeader title="入口" meta="停用 / 启用 / 轮换凭据 / 分享链接" />
      <div className="stack-sm">
        {m.entries.length === 0 && <div className="hint">这台机器还没有入口，选一台线路机把它挂上去。</div>}
        {m.entries.map((e) => (
          <div key={e.id} className="manage-row" style={{ border: '1px solid var(--line)', borderRadius: 'var(--r-float)', padding: '8px 12px' }}>
            <div><div className="t">{m.kind === 'relay' ? `${e.port} · ${e.node.name}` : entryLabel(e)}</div><div className="hint">{e.apply_error ?? (e.applied ? '已下发到线路机' : '等待下发')}</div></div>
            <span className="row" style={{ gap: 8 }}>{entryState(e)}<EntryActions entry={e} /></span>
          </div>
        ))}
        {m.kind !== 'relay' && m.lifecycle === 'active' && movableEntries(m).length > 0 && (
          <div className="manage-row">
            <div><div className="t">换线路机</div><div className="hint">把入口换到另一台线路机：新入口验收通过后才摘旧入口，不断网；完成后分享链接会变。</div></div>
            <Button variant="secondary" disabled={!!m.active_job} onClick={() => setDlg('move')}><Repeat2 />换线路机…</Button>
          </div>
        )}
        {m.kind === 'relay' && m.lifecycle === 'active' && movableEntries(m).length > 0 && (
          <div className="manage-row">
            <div><div className="t">搬空线路机</div><div className="hint">把上面所有节点换到另一台线路机，之后就能安全下线这台。节点的分享链接会变。</div></div>
            <Button variant="secondary" onClick={() => setDlg('evacuate')}><PackageOpen />搬空到…</Button>
          </div>
        )}
        {m.kind !== 'relay' && opts.length > 0 && (
          <div className="row">
            <div style={{ width: 200 }}><Select value={relay} onChange={setRelay} options={opts} placeholder="再挂到另一台线路机" /></div>
            <Button disabled={!relay || add.isPending} loading={add.isPending} onClick={() => add.mutate({ id: m.id, relay_id: relay }, { onSuccess: () => { toast.ok('已提交，新入口正在下发'); setRelay(''); }, onError: toast.err })}><Plus />添加入口</Button>
          </div>
        )}
      </div>
      <MoveDialog m={m} open={dlg === 'move'} onOpenChange={(o) => !o && setDlg(null)} onDone={() => setDlg(null)} />
      <EvacuateDialog relay={m} open={dlg === 'evacuate'} onOpenChange={(o) => !o && setDlg(null)} onDone={() => setDlg(null)} />
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
          <div className="t">版本 {a.version ?? '未知'}</div>
          <div className="hint">最近上报 {a.last_report_at ? F.ago(a.last_report_at) : '从未'}。升级会推送中控上的当前版本并重启 agent，不影响线路上的连接。</div>
        </div>
        <Button variant="secondary" loading={up.isPending} disabled={!!m.active_job}
          onClick={() => up.mutate(m.id, { onSuccess: () => toast.ok('已提交升级作业'), onError: toast.err })}>升级 agent</Button>
      </div>
    </Section>
  );
}

function DangerSection({ m, onClosed }: { m: MachineView; onClosed: () => void }) {
  const [dlg, setDlg] = React.useState<null | 'retire' | 'purge' | 'restore'>(null);
  const purge = usePurge();
  const busyJob = m.lifecycle === 'provisioning' || m.lifecycle === 'retiring';
  const retired = m.lifecycle === 'retired';

  return (
    <Section>
      <SectionHeader title={retired ? '已退役' : '下线'} />
      <div className="danger-zone">
        {retired ? (
          <>
            <div className="manage-row">
              <div>
                <div className="t">恢复</div>
                <div className="hint">已于 {F.dtFull(m.retired_at)} 退役。恢复会保持原来的机器 ID（历史、账单、审计接上），重新走一遍上架，编号接在团队末尾。</div>
              </div>
              <Button variant="primary" onClick={() => setDlg('restore')}><Undo2 />恢复…</Button>
            </div>
            <div className="manage-row">
              <div>
                <div className="t">彻底删除</div>
                <div className="hint">会清掉这台机器的记录与遥测，无法找回；审计记录保留。</div>
              </div>
              <Button variant="outline-danger" onClick={() => setDlg('purge')}><Trash2 />彻底删除…</Button>
            </div>
          </>
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

      <RestoreDialog m={m} open={dlg === 'restore'} onOpenChange={(o) => !o && setDlg(null)} onDone={onClosed} />
      <RetireDialog m={m} open={dlg === 'retire'} onOpenChange={(o) => !o && setDlg(null)} onDone={onClosed} />

      <TypedConfirmDialog open={dlg === 'purge'} onOpenChange={(o) => !o && setDlg(null)} title={`彻底删除 ${m.name}？`} expected={m.name} prompt="请输入显示名" confirmLabel="彻底删除" danger
        description="会删除机器记录与全部遥测数据，无法找回；审计记录保留。"
        onConfirm={async () => { await purge.mutateAsync({ id: m.id, confirm: m.name }); toast.ok('已提交删除作业'); onClosed(); }} />
    </Section>
  );
}
