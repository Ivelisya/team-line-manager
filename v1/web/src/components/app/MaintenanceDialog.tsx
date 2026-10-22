// 一台机器的维护窗口（卡片 / 表格的菜单、抽屉「管理」页）：从现在起选一个时长，提交时建一条只针对这台、标记为维护的静默
// （同告警页「新建静默」的接口；预约、全部机器、只静默某条规则这些仍在告警页做）。外壳里挂一份，按 nodeActions 的目标打开。
import * as React from 'react';
import { useAddSilence, useMachine } from '@/lib/queries';
import { closeMaint, useMaintTarget } from '@/lib/nodeActions';
import { resetOnOpen, useHeld } from '@/lib/useHeld';
import * as F from '@/lib/format';
import { Field, Input } from '@/components/ui/form';
import { toast } from '@/components/ui/toast';
import { FormDialog } from './dialogs';
import { DurationField, SILENCE_MAX_MIN, useDuration, type DurationItem } from './DurationField';

const MIN = 60_000;
const DURATIONS: DurationItem[] = [{ min: 30, label: '30 分钟' }, { min: 60, label: '1 小时' }, { min: 120, label: '2 小时' }, { min: 240, label: '4 小时' }];

const MaintenanceDialog = resetOnOpen(function MaintenanceDialog({ m, open, onOpenChange }: { m: { id: string; name: string }; open: boolean; onOpenChange: (o: boolean) => void }) {
  const add = useAddSilence();
  const d = useDuration(60, SILENCE_MAX_MIN);
  const [comment, setComment] = React.useState('');
  const { minutes } = d;
  const submit = () => {
    const s = Date.now();
    add.mutate({ subject_id: m.id, starts_at: new Date(s).toISOString(), ends_at: new Date(s + minutes * MIN).toISOString(), is_maintenance: true, comment: comment.trim() },
      { onSuccess: () => { toast.ok(`${m.name} 进入维护，${F.span(minutes * MIN)}后结束`); onOpenChange(false); }, onError: toast.err });
  };
  return (
    <FormDialog open={open} onOpenChange={onOpenChange} title={`维护窗口 · ${m.name}`} desc={d.valid ? `从现在起 ${F.span(minutes * MIN)}，期间不发告警` : '从现在起，期间不发告警'}
      submitLabel="开始维护" busy={add.isPending} valid={d.valid} dirty={d.dur !== 60 || !!comment.trim()} onSubmit={submit}>
      <DurationField d={d} items={DURATIONS} />
      <Field label="备注"><Input value={comment} onChange={(e) => setComment(e.target.value)} placeholder="例如：换内核" /></Field>
    </FormDialog>
  );
});

/** 外壳里的一份：目标机器从列表缓存取名字，关闭动画期间沿用 */
export function MaintenanceHost() {
  const id = useMaintTarget();
  const m = useHeld(id, useMachine(id).data);
  return m ? <MaintenanceDialog m={m} open={!!id} onOpenChange={(o) => !o && closeMaint()} /> : null;
}
