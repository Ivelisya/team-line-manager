// 时长选择：常用时长胶囊 + 「自定义」分钟数（只收整数分钟、去掉前导 0）。告警页「新建静默」与卡片菜单的「维护窗口…」共用
import { Field } from '@/components/ui/field';
import * as React from 'react';
import * as F from '@/lib/format';
import { LabelRow, UnitInput } from '@/components/ui/form';

export type DurationItem = { min: number; label: string };
/** 共用的常用时长（分钟）：静默与维护窗口两个对话框一份，别的时长用「自定义」填分钟数 */
export const DURATIONS: DurationItem[] = [
  { min: 30, label: '30 分钟' }, { min: 60, label: '1 小时' }, { min: 120, label: '2 小时' }, { min: 240, label: '4 小时' }, { min: 1440, label: '24 小时' },
];
/** 默认时长（分钟）：1 小时 */
export const DEFAULT_DURATION_MIN = 60;
/** 静默 / 维护窗口最长多久（分钟）：中控最长收 30 天 */
export const SILENCE_MAX_MIN = 720 * 60;

/** 时长的状态：dur 是选中的胶囊（分钟）或 'custom'；minutes 是最终的分钟数，valid 为假时不能提交 */
export function useDuration(init: number, max: number) {
  const [dur, setDur] = React.useState<number | 'custom'>(init);
  const [mins, setMins] = React.useState('');
  const n = Number(mins);
  return { dur, setDur, mins, setMins, max, minutes: dur === 'custom' ? n : dur, valid: dur !== 'custom' || (n >= 1 && n <= max) };
}

export function DurationField({ d, items = DURATIONS }: { d: ReturnType<typeof useDuration>; items?: DurationItem[] }) {
  const bad = d.dur === 'custom' && d.mins !== '' && !d.valid;
  // 单选组：只有选中的一颗在 Tab 顺序里，方向键在组内切换（焦点跟着选中走）
  const opts: (number | 'custom')[] = [...items.map((x) => x.min), 'custom'];
  const pick = (v: number | 'custom') => {
    // 改成自定义时带上刚才选的时长
    if (v === 'custom' && d.dur !== 'custom') d.setMins(String(d.dur));
    d.setDur(v);
  };
  const onKey = (e: React.KeyboardEvent<HTMLDivElement>) => {
    const step = e.key === 'ArrowRight' || e.key === 'ArrowDown' ? 1 : e.key === 'ArrowLeft' || e.key === 'ArrowUp' ? -1 : 0;
    if (!step) return;
    e.preventDefault();
    const i = (opts.indexOf(d.dur) + step + opts.length) % opts.length;
    pick(opts[i]);
    (e.currentTarget.children[i] as HTMLElement).focus();
  };
  const chip = (v: number | 'custom', label: string) => (
    <button key={v} type="button" role="radio" aria-checked={d.dur === v} tabIndex={d.dur === v ? 0 : -1} className="fchip" onClick={() => pick(v)}>{label}</button>
  );
  return (
    <Field>
      <LabelRow>时长</LabelRow>
      <div className="row" role="radiogroup" aria-label="时长" onKeyDown={onKey}>
        {items.map((x) => chip(x.min, x.label))}
        {chip('custom', '自定义')}
      </div>
      {d.dur === 'custom' && (
        <UnitInput unit="分钟" width={160} inputMode="numeric" aria-label="自定义时长（分钟）" placeholder={`1–${d.max}`} autoFocus value={d.mins} aria-invalid={bad}
          onChange={(e) => d.setMins(e.target.value.replace(/\D/g, '').replace(/^0+/, ''))} />
      )}
      {bad && <div className="hint is-error">填 1–{d.max} 分钟（最长 {F.span(d.max * 60_000)}）</div>}
    </Field>
  );
}
