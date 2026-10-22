// 时长选择：常用时长胶囊 + 「自定义」分钟数（只收整数分钟、去掉前导 0）。告警页「新建静默」用，交互同卡片菜单的「维护窗口…」
import * as React from 'react';
import { UnitInput } from '@/components/ui/form';
import { FilterChip } from './primitives';

export type DurationItem = { min: number; label: string };

/** 时长的状态：dur 是选中的胶囊（分钟）或 'custom'；minutes 是最终的分钟数，valid 为假时不能提交 */
export function useDuration(init: number, max: number) {
  const [dur, setDur] = React.useState<number | 'custom'>(init);
  const [mins, setMins] = React.useState('');
  const n = Number(mins);
  return { dur, setDur, mins, setMins, max, minutes: dur === 'custom' ? n : dur, valid: dur !== 'custom' || (n >= 1 && n <= max) };
}

export function DurationField({ d, items }: { d: ReturnType<typeof useDuration>; items: DurationItem[] }) {
  const bad = d.dur === 'custom' && d.mins !== '' && !d.valid;
  return (
    <div className="field">
      <span className="label">时长</span>
      <div className="row" role="group" aria-label="时长">
        {items.map((x) => <FilterChip key={x.min} pressed={d.dur === x.min} onClick={() => d.setDur(x.min)}>{x.label}</FilterChip>)}
        {/* 改成自定义时带上刚才选的时长 */}
        <FilterChip pressed={d.dur === 'custom'} onClick={() => { if (d.dur !== 'custom') d.setMins(String(d.dur)); d.setDur('custom'); }}>自定义</FilterChip>
      </div>
      {d.dur === 'custom' && (
        <UnitInput unit="分钟" width={160} inputMode="numeric" aria-label="自定义时长（分钟）" placeholder={`1–${d.max}`} autoFocus value={d.mins} aria-invalid={bad}
          onChange={(e) => d.setMins(e.target.value.replace(/\D/g, '').replace(/^0+/, ''))} />
      )}
      {bad && <div className="hint is-error">填 1–{d.max} 分钟（最长 30 天）</div>}
    </div>
  );
}
