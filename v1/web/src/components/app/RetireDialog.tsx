// 退役确认：输入显示名确认，附“后面的节点会改名”预览。抽屉“管理”页和卡片/表格的右键菜单共用这一份。
// 宿主机：预览列出会一并退役的同机 IP 节点（改名预览覆盖所有受影响的团队）；同机 IP 节点：只摘它自己，宿主机不受影响。
import * as React from 'react';
import { ArrowRight } from 'lucide-react';
import type { MachineView } from '@/lib/types';
import { useMachine, useRetire, useRetirePreview } from '@/lib/queries';
import { CheckRow, Field, Input } from '@/components/ui/form';
import { toast } from '@/components/ui/toast';
import { Note } from './primitives';
import { resetOnOpen } from '@/lib/useHeld';
import { TypedConfirmDialog } from './dialogs';

/** 会一并退役的同机 IP 节点：名称 + IP（IP 取机器列表缓存里的那份） */
function IpNodeItem({ id, name }: { id: string; name: string }) {
  const ip = useMachine(id).data?.ip;
  return <li><span className="num">{name}</span>{ip && <span className="t-sub">{ip}</span>}</li>;
}

/** 每次打开重新挂载（resetOnOpen）：选项回到默认，关闭动画期间不变 */
export const RetireDialog = resetOnOpen(function RetireDialog({ m, open, onOpenChange, onDone }: { m: MachineView; open: boolean; onOpenChange: (o: boolean) => void; onDone?: () => void }) {
  const retire = useRetire();
  const [cleanup, setCleanup] = React.useState(true);
  const [reason, setReason] = React.useState('');
  const pv = useRetirePreview(m.id, open);
  // 重开时缓存里还是上一次的预检（关闭动画期间对话框还挂着、观察者没走，gcTime 0 也回收不了）：只认这次打开后取回的
  const done = pv.isFetchedAfterMount;
  const h = m.host_machine;
  const xs = done && pv.isSuccess ? pv.data.ip_nodes : [];
  return (
    <TypedConfirmDialog open={open} onOpenChange={onOpenChange} size="md" title={`退役 ${m.name}？`} expected={m.name} prompt="请输入显示名" confirmLabel="退役" danger
      description="入口会立即停用，已有连接会断开。之后可以在「管理」页恢复，但要重新上架。"
      confirmDisabled={!done}
      onConfirm={async () => {
        const r = await retire.mutateAsync({ id: m.id, confirm: m.name, cleanup, reason: reason.trim() || undefined });
        toast.ok(r.ip_node_jobs.length ? `已提交退役作业，连同 ${r.ip_node_jobs.length} 个同机 IP 节点` : '已提交退役作业');
        onDone?.();
      }}>
      {!done && <div className="hint">正在检查退役后会受影响的节点…</div>}
      {done && pv.isError && <Note tone="warn">无法预览改名影响：{(pv.error as Error).message}。仍可继续退役，但无法提前看到后面节点的新名字。</Note>}
      {h && <Note>只摘除这个 IP 的节点（它的入口与宿主机上它自己的 SS 实例），宿主机「{h.name}」不受影响。</Note>}
      {xs.length > 0 && (
        <Note tone="warn">
          <div style={{ fontWeight: 500, marginBottom: 4 }}>以下同机 IP 节点在这台机器上，会一起退役：</div>
          <ul className="reason-list" style={{ maxHeight: 160, overflow: 'auto' }}>
            {xs.map((x) => <IpNodeItem key={x.id} id={x.id} name={x.name} />)}
          </ul>
        </Note>
      )}
      {done && pv.isSuccess && pv.data.renames.length > 0 && (
        <Note tone="warn">
          <div style={{ fontWeight: 500, marginBottom: 4 }}>退役后以下节点会改名（编号保持连续）：</div>
          <ul className="reason-list" style={{ maxHeight: 200, overflow: 'auto' }}>
            {pv.data.renames.map((r) => <li key={r.machine_id}><span className="num">{r.old_name} <ArrowRight size={12} style={{ display: 'inline', verticalAlign: -1 }} /> <b>{r.new_name}</b></span></li>)}
          </ul>
        </Note>
      )}
      {m.kind !== 'relay' && (
        <CheckRow checked={cleanup} onChange={setCleanup}>
          {h ? '清理宿主机上这个 IP 的服务（停掉它的 SS 实例、删掉它的防火墙规则）' : xs.length ? '清理节点上的服务（卸载 agent 与代理服务，连同同机 IP 节点的实例）' : '清理节点上的服务（卸载 agent 与代理服务）'}
        </CheckRow>
      )}
      <Field label="原因（选填）"><Input value={reason} onChange={(e) => setReason(e.target.value)} placeholder="例如：供应商到期不续" /></Field>
    </TypedConfirmDialog>
  );
});
