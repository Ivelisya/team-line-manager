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
import { FilterChip } from './primitives';

const MIN = 60_000;
const DURATIONS = [{ min: 30, label: '30 分钟' }, { min: 60, label: '1 小时' }, { min: 120, label: '2 小时' }, { min: 240, label: '4 小时' }];
const MAX_MIN = 720 * 60; // 同告警页：最长 30 天

const MaintenanceDialog = resetOnOpen(function MaintenanceDialog({ m, open, onOpenChange }: { m: { id: string; name: string }; open: boolean; onOpenChange: (o: boolean) => void }) {
  const add = useAddSilence();
  const [dur, setDur] = React.useState<number | 'custom'>(60);
  const [mins, setMins] = React.useState('');
  const [comment, setComment] = React.useState('');
  const n = Number(mins);
  const minutes = dur === 'custom' ? n : dur;
  const bad = dur === 'custom' && mins !== '' && !(n >= 1 && n <= MAX_MIN);
  const valid = dur !== 'custom' || (n >= 1 && n <= MAX_MIN);
  const submit = () => {
    const s = Date.now();
    add.mutate({ subject_id: m.id, starts_at: new Date(s).toISOString(), ends_at: new Date(s + minutes * MIN).toISOString(), is_maintenance: true, comment: comment.trim() },
      { onSuccess: () => { toast.ok(`${m.name} 进入维护，${F.span(minutes * MIN)}后结束`); onOpenChange(false); }, onError: toast.err });
  };
  return (
    <FormDialog open={open} onOpenChange={onOpenChange} title={`维护窗口 · ${m.name}`} desc={valid ? `从现在起 ${F.span(minutes * MIN)}，期间不发告警` : '从现在起，期间不发告警'}
      submitLabel="开始维护" busy={add.isPending} valid={valid} dirty={dur !== 60 || !!comment.trim()} onSubmit={submit}>
      <div className="field">
        <span className="label">时长</span>
        <div className="row" role="group" aria-label="时长">
          {DURATIONS.map((d) => <FilterChip key={d.min} pressed={dur === d.min} onClick={() => setDur(d.min)}>{d.label}</FilterChip>)}
          <FilterChip pressed={dur === 'custom'} onClick={() => { if (dur !== 'custom') setMins(String(dur)); setDur('custom'); }}>自定义</FilterChip>
        </div>
        {dur === 'custom' && <Input inputMode="numeric" aria-label="自定义时长（分钟）" placeholder="分钟数" autoFocus value={mins} onChange={(e) => setMins(e.target.value.replace(/\D/g, ''))} aria-invalid={bad} />}
        {bad && <div className="hint is-error">1–{MAX_MIN} 分钟（最长 30 天）</div>}
      </div>
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
