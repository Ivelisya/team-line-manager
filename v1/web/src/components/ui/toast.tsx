import * as React from 'react';
import { CheckCircle2, Info, X, XCircle } from 'lucide-react';
import { cn } from '@/lib/utils';

// 堆叠式通知：toast.ok('已保存') / toast.err(error) / toast.show({ tone, title, text, action })。操作名与按钮名一致（“保存” → “已保存”）。
// 同样的消息在显示期间只叠一次（右上角计数）；鼠标停在上面时暂停计时；错误用 role="alert"。
export type ToastTone = 'ok' | 'bad' | 'info';
export interface ToastInput { tone: ToastTone; text: string; title?: string; action?: { label: string; onClick: () => void }; duration?: number }
interface T extends ToastInput { id: number; count: number; leaving?: boolean }

const MAX = 5;
let push: ((t: ToastInput) => void) | null = null;
export const toast = {
  ok: (text: string) => push?.({ tone: 'ok', text }),
  info: (text: string) => push?.({ tone: 'info', text }),
  err: (e: unknown) => push?.({ tone: 'bad', text: e instanceof Error ? e.message : String(e) }),
  show: (t: ToastInput) => push?.(t),
};

/** 离场过渡时长：直接读 .toast 的 transition 所用的 --dur-base 令牌（减弱动效时是 0ms），不再写死一个会和 CSS 漂移的数字 */
function leaveMs(): number {
  try {
    const v = parseFloat(getComputedStyle(document.documentElement).getPropertyValue('--dur-base'));
    return Number.isFinite(v) ? v : 180;
  } catch { return 180; }
}

const ICON = { ok: CheckCircle2, bad: XCircle, info: Info } as const;
const LIFE = { ok: 3600, info: 5000, bad: 7000 } as const;

function ToastItem({ t, onClose }: { t: T; onClose: (id: number) => void }) {
  const life = t.duration ?? (t.action ? 9000 : LIFE[t.tone]);
  const [paused, setPaused] = React.useState(false);
  const left = React.useRef(life);
  const started = React.useRef(0);
  // t.count 变化 = 又来了一条相同消息：重新计时
  React.useEffect(() => { left.current = life; }, [t.count, life]);
  React.useEffect(() => {
    if (paused || t.leaving) return;
    started.current = Date.now();
    const h = setTimeout(() => onClose(t.id), left.current);
    return () => { clearTimeout(h); left.current = Math.max(0, left.current - (Date.now() - started.current)); };
  }, [paused, t.id, t.count, t.leaving, onClose]);
  const Ic = ICON[t.tone];
  return (
    <div className="toast" data-tone={t.tone} data-leaving={t.leaving ? '1' : undefined} role={t.tone === 'bad' ? 'alert' : 'status'}
      onMouseEnter={() => setPaused(true)} onMouseLeave={() => setPaused(false)} onFocus={() => setPaused(true)} onBlur={() => setPaused(false)}>
      <Ic size={16} />
      <div className="toast-body">
        {t.title && <div className="toast-title">{t.title}</div>}
        <div className={cn(t.title && 'toast-text')}>{t.text}</div>
        {t.action && <button type="button" className="toast-action" onClick={() => { t.action!.onClick(); onClose(t.id); }}>{t.action.label}</button>}
      </div>
      {t.count > 1 && <span className="toast-count" aria-label={`重复 ${t.count} 次`}>×{t.count}</span>}
      <button type="button" className="toast-x" aria-label="关闭通知" onClick={() => onClose(t.id)}><X size={14} /></button>
      <span className="toast-timer" style={{ animationDuration: `${life}ms`, animationPlayState: paused ? 'paused' : 'running' }} key={t.count} />
    </div>
  );
}

export function Toaster() {
  const [list, setList] = React.useState<T[]>([]);
  const seq = React.useRef(0);
  const close = React.useCallback((id: number) => {
    setList((l) => l.map((x) => (x.id === id ? { ...x, leaving: true } : x)));
    setTimeout(() => setList((l) => l.filter((x) => x.id !== id)), leaveMs());
  }, []);
  React.useEffect(() => {
    push = (t) => setList((l) => {
      const dup = l.find((x) => !x.leaving && x.tone === t.tone && x.text === t.text && x.title === t.title && !x.action && !t.action);
      if (dup) return l.map((x) => (x === dup ? { ...x, count: x.count + 1 } : x));
      return [...l.slice(-(MAX - 1)), { ...t, id: ++seq.current, count: 1 }];
    });
    return () => { push = null; };
  }, []);
  return (
    <div className="toasts" aria-live="polite">
      {list.map((t) => <ToastItem key={t.id} t={t} onClose={close} />)}
    </div>
  );
}
