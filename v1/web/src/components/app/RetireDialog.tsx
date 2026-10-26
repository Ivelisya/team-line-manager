// 退役确认：移入备用可以撤销，直接确认；立即清理并回收不可撤销，要输入显示名确认。线路机还挂着节点入口时预览带阻断原因（blockers）：对话框只留原因条、「搬空到…」和「关闭」，不要原因框、确认输入和确认按钮。两种去向：移入备用（默认，机器照常运行，订阅到期满 1 天后自动回收）或立即清理并移入回收站；
// 附“后面的节点会改名”预览、还指向它的软路由出口（警告，可跳去「替换为…」）。抽屉“管理”页和卡片/表格的右键菜单共用这一份。
// 宿主机：预览列出会一并走的同机 IP 节点；同机 IP 节点：只动它自己，宿主机不受影响。已在备用里的机器只能立即回收。
import * as React from 'react';
import { ArrowRight } from 'lucide-react';
import type { MachineView } from '@/lib/types';
import { useMachine, useRetire, useRetirePreview } from '@/lib/queries';
import { openEvacuate, openReplace } from '@/lib/nodeActions';
import { CheckRow, Field, Input, InfoTip } from '@/components/ui/form';
import { toast } from '@/components/ui/toast';
import { Button } from '@/components/ui/button';
import { ErrorNote, HELD_TEXT, Note } from './primitives';
import { resetOnOpen } from '@/lib/useHeld';
import { TypedConfirmDialog } from './dialogs';
import { canReplace, RenameNote } from './ReplaceDialog';
import { movableEntries } from './LifecycleDialogs';

type Mode = 'spare' | 'now';

/** 会一并走的同机 IP 节点：名称（进备用时带上新名）+ IP（IP 取机器列表缓存里的那份） */
function IpNodeItem({ id, name, to }: { id: string; name: string; to?: string }) {
  const ip = useMachine(id).data?.ip;
  return <li><span className="num">{name}{to && <> <ArrowRight size={12} style={{ display: 'inline', verticalAlign: -1 }} /> <b>{to}</b></>}</span>{ip && <span className="t-sub">{ip}</span>}</li>;
}

function Choice({ checked, onSelect, children }: { checked: boolean; onSelect: () => void; children: React.ReactNode }) {
  return <label className="choice"><input type="radio" name="retire-mode" checked={checked} onChange={onSelect} /><span>{children}</span></label>;
}

/** 每次打开重新挂载（resetOnOpen）：选项回到默认，关闭动画期间不变 */
export const RetireDialog = resetOnOpen(function RetireDialog({ m, open, onOpenChange, onDone }: { m: MachineView; open: boolean; onOpenChange: (o: boolean) => void; onDone?: () => void }) {
  const retire = useRetire();
  const [pick, setPick] = React.useState<Mode>('spare');
  const [cleanup, setCleanup] = React.useState(true);
  const [reason, setReason] = React.useState('');
  const pv = useRetirePreview(m.id, open);
  // 重开时缓存里还是上一次的预检（关闭动画期间对话框还挂着、观察者没走，gcTime 0 也回收不了）：只认这次打开后取回的
  const done = pv.isFetchedAfterMount;
  const p = done && pv.isSuccess ? pv.data : undefined;
  const inSpare = m.spare && m.lifecycle === 'active';
  const canSpare = !!p?.spare.ok;
  const mode: Mode = canSpare ? pick : 'now';
  // 阻断原因是「立即回收」会被拒绝的原因（线路机还挂着节点入口、有作业正在执行一步…）：走「立即回收」时先说清、禁用确认
  const blocked = mode === 'now' && !!p?.blockers?.length;
  const h = m.host_machine;
  const xs: { id: string; name: string; to?: string }[] = (mode === 'spare' ? p?.spare.ip_nodes.map((x) => ({ id: x.id, name: x.name, to: x.to_name })) : p?.ip_nodes) ?? [];
  const due = m.subscription?.due_date.slice(5);
  const recycle = p?.spare.recycle_on?.slice(5);
  const go = mode === 'spare' ? '一起移入备用' : inSpare ? '一起回收' : '一起清理并回收';
  return (
    <TypedConfirmDialog open={open} onOpenChange={onOpenChange} size="md" title={`${inSpare ? '回收' : '退役'} · ${m.name}`} expected={m.name} prompt="请输入显示名"
      typed={mode === 'now'} danger={mode === 'now'}
      confirmLabel={mode === 'spare' ? '移入备用' : inSpare ? '立即回收' : '清理并回收'}
      description={p && !canSpare ? <>已有连接会断开 <InfoTip text="入口会立即停用，机器移入回收站，无法恢复。要再用，只能重新上架" /></> : undefined}
      confirmDisabled={!p} blocked={blocked}
      onConfirm={async () => {
        const r = await retire.mutateAsync({ id: m.id, confirm: m.name, now: mode === 'now', cleanup: mode === 'now' ? cleanup : undefined, reason: reason.trim() || undefined });
        if (r.mode === 'spare') {
          toast.ok(`已移入备用：${r.from} → ${r.name}${r.ip_nodes.length ? `，连同 ${r.ip_nodes.length} 个同机 IP 节点` : ''}`);
          for (const n of r.notes) toast.info(n);
        }
        else toast.ok(r.ip_node_jobs.length ? `已提交清理作业，连同 ${r.ip_node_jobs.length} 个同机 IP 节点` : '已提交清理作业');
        onDone?.();
      }}>
      {!done && <div className="hint">正在检查影响…</div>}
      {done && pv.isError && <ErrorNote error={pv.error} onRetry={() => void pv.refetch()}>无法检查影响：</ErrorNote>}
      {blocked && (
        <Note tone="bad">
          <div className="row row-between" style={{ gap: 8, flexWrap: 'nowrap' }}>
            <span style={{ minWidth: 0 }}>{p!.blockers!.map((b) => <div key={b}>{b}</div>)}</span>
            {m.kind === 'relay' && movableEntries(m).length > 0 && <Button size="sm" style={{ flex: 'none' }} onClick={() => { onOpenChange(false); openEvacuate(m.id); }}>搬空到…</Button>}
          </div>
        </Note>
      )}
      {p && canSpare && (
        <div className="choice-list" role="radiogroup" aria-label="去向">
          <Choice checked={mode === 'spare'} onSelect={() => setPick('spare')}>
            移入备用 → {p.spare.to_name}{p.routers.length > 0 ? `（${HELD_TEXT}）` : due && recycle ? `（到期 ${due}，${recycle} 起自动回收）` : recycle ? `（${recycle} 起自动回收）` : ''}
            <InfoTip text={`移到团队「${p.spare.team.name}」，改名为 ${p.spare.to_name}。机器照常运行，到期满 1 天后自动清理并移入回收站\n想留下它，把团队改回去就行`} />
          </Choice>
          <Choice checked={mode === 'now'} onSelect={() => setPick('now')}>
            立即清理并移入回收站 <span className="t-sub">已有连接会断开</span>
            <InfoTip text="移除入口、清理节点上的服务，机器移入回收站，不再显示在面板里" />
          </Choice>
        </div>
      )}

      {p && !canSpare && !inSpare && !blocked && <Note>{h ? '同机 IP 节点没有订阅，只能立即回收' : `不能移入备用：${p.spare.why}`}</Note>}
      {h && <Note>宿主机「{h.name}」不受影响 <InfoTip text={mode === 'spare' ? '只动这个 IP 的节点。' : `只回收这个 IP 的节点：移除它的入口，${cleanup ? '停掉它在宿主机上的 SS 实例' : '宿主机上的 SS 实例保留'}`} /></Note>}
      {xs.length > 0 && (
        <Note>
          <div className="note-title">同机 IP 节点会{go}：</div>
          <ul className="reason-list" style={{ maxHeight: 160, overflow: 'auto' }}>
            {xs.map((x) => <IpNodeItem key={x.id} id={x.id} name={x.name} to={x.to} />)}
          </ul>
        </Note>
      )}
      {p && p.routers.length > 0 && (
        <Note tone="warn">
          <div className="note-title">这些软路由的出口还指向它 <InfoTip text={mode === 'spare' ? '移入备用期间出口照常可用；回收之后就不通了。' : '回收之后这些出口就不通了。'} />
            {canReplace(m) && <> <button type="button" className="btn btn-link btn-sm" style={{ height: 'auto', padding: 0 }} onClick={() => { onOpenChange(false); openReplace(m.id); }}>替换为…</button></>}
          </div>
          <ul className="reason-list" style={{ maxHeight: 160, overflow: 'auto' }}>
            {p.routers.map((r) => <li key={r.id}><span className="num">{r.name}</span><span className="t-sub">{r.exits.map((e) => `${e.exit}（${e.node}）`).join('、')}</span></li>)}
          </ul>
        </Note>
      )}
      {p && mode === 'spare' && p.spare.warnings.length > 0 && (
        <Note tone="warn">
          <div className="note-title">移入备用后访问策略会变 <InfoTip text="软路由的客户走的是节点生效的访问策略，节点换了团队就按新团队的策略。" /></div>
          <ul className="reason-list" style={{ maxHeight: 160, overflow: 'auto' }}>
            {p.spare.warnings.map((w) => <li key={w}>{w}</li>)}
          </ul>
        </Note>
      )}
      {/* 进备用时的几条说明（自动续费、撤销后的编号）和补号合成一块带圆点的列表，不再一条一个同色提示条 */}
      {p && (() => {
        const notes = mode === 'spare' ? p.spare.notes : [];
        const ren = <RenameNote renames={p.renames} tip="同团队后面的节点前移补号，编号保持连续" bare={notes.length > 0} />;
        if (!notes.length) return ren;
        return (
          <Note>
            <ul className="reason-list note-list">
              {notes.map((n) => <li key={n}><span className="dot" data-tone="info" /><span>{n}</span></li>)}
              {p.renames.length > 0 && <li><span className="dot" data-tone="info" />{ren}</li>}
            </ul>
          </Note>
        );
      })()}
      {mode === 'now' && m.kind !== 'relay' && !blocked && (
        <div className="row" style={{ gap: 4 }}>
          <CheckRow checked={cleanup} onChange={setCleanup}>{h ? '清理这个 IP 的服务' : '清理节点上的服务'}</CheckRow>
          <InfoTip text={h ? '停掉它在宿主机上的 SS 实例、删掉它的防火墙规则' : xs.length ? '卸载 agent 和代理服务，同机 IP 节点的实例一起清理' : '卸载 agent 与代理服务'} />
        </div>
      )}
      {!blocked && <Field label="原因（选填）"><Input value={reason} onChange={(e) => setReason(e.target.value)} placeholder="例如：供应商到期不续" /></Field>}
    </TypedConfirmDialog>
  );
});
