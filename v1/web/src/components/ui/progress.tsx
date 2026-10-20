import { cn } from '@/lib/utils';

/** 进度条：0–100。tone 对应状态色；live 时填充条上有缓慢流动的细纹，表示还在推进。沿用 .quota 的轨道语言。 */
export function Progress({ value, tone, live, className, label }: { value: number; tone?: 'ok' | 'bad' | 'warn'; live?: boolean; className?: string; label?: string }) {
  const v = Math.max(0, Math.min(100, Number.isFinite(value) ? value : 0));
  return (
    <div className={cn('progress', className)} data-tone={tone} data-live={live ? '1' : undefined} role="progressbar" aria-valuemin={0} aria-valuemax={100} aria-valuenow={Math.round(v)} aria-label={label}>
      <div className="progress-fill" style={{ width: `${v}%` }} />
    </div>
  );
}
