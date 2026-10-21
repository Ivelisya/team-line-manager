// 退役确认：输入显示名确认，附“后面的节点会改名”预览。抽屉“管理”页和卡片/表格的右键菜单共用这一份。
import * as React from 'react';
import { ArrowRight } from 'lucide-react';
import type { MachineView } from '@/lib/types';
import { useRetire, useRetirePreview } from '@/lib/queries';
import { CheckRow, Field, Input } from '@/components/ui/form';
import { toast } from '@/components/ui/toast';
import { Note } from './primitives';
import { resetOnOpen } from '@/lib/useHeld';
import { TypedConfirmDialog } from './dialogs';

/** 每次打开重新挂载（resetOnOpen）：选项回到默认，关闭动画期间不变 */
export const RetireDialog = resetOnOpen(function RetireDialog({ m, open, onOpenChange, onDone }: { m: MachineView; open: boolean; onOpenChange: (o: boolean) => void; onDone?: () => void }) {
  const retire = useRetire();
  const [cleanup, setCleanup] = React.useState(true);
  const [reason, setReason] = React.useState('');
  const pv = useRetirePreview(m.id, open);
  // 重开时缓存里还是上一次的预检（关闭动画期间对话框还挂着、观察者没走，gcTime 0 也回收不了）：只认这次打开后取回的
  const done = pv.isFetchedAfterMount;
  return (
    <TypedConfirmDialog open={open} onOpenChange={onOpenChange} size="md" title={`退役 ${m.name}？`} expected={m.name} prompt="请输入显示名" confirmLabel="退役" danger
      description="入口会立即停用，已有连接会断开。之后可以在「管理」页恢复，但要重新上架。"
      confirmDisabled={!done}
      onConfirm={async () => { await retire.mutateAsync({ id: m.id, confirm: m.name, cleanup, reason: reason.trim() || undefined }); toast.ok('已提交退役作业'); onDone?.(); }}>
      {!done && <div className="hint">正在检查退役后会受影响的节点…</div>}
      {done && pv.isError && <Note tone="warn">无法预览改名影响：{(pv.error as Error).message}。仍可继续退役，但无法提前看到后面节点的新名字。</Note>}
      {done && pv.isSuccess && pv.data.renames.length > 0 && (
        <Note tone="warn">
          <div style={{ fontWeight: 500, marginBottom: 4 }}>退役后以下节点会改名（编号保持连续）：</div>
          <ul className="reason-list" style={{ maxHeight: 200, overflow: 'auto' }}>
            {pv.data.renames.map((r) => <li key={r.machine_id}><span className="num">{r.old_name} <ArrowRight size={12} style={{ display: 'inline', verticalAlign: -1 }} /> <b>{r.new_name}</b></span></li>)}
          </ul>
        </Note>
      )}
      {m.kind !== 'relay' && <CheckRow checked={cleanup} onChange={setCleanup}>清理节点上的服务（卸载 agent 与代理服务）</CheckRow>}
      <Field label="原因（选填）"><Input value={reason} onChange={(e) => setReason(e.target.value)} placeholder="例如：供应商到期不续" /></Field>
    </TypedConfirmDialog>
  );
});
