import * as React from 'react';
import { CheckCircle2, Info, X, XCircle } from 'lucide-react';
import { m } from 'motion/react';
import { cn } from '@/lib/utils';
import { SPRING, durMs } from '@/lib/motion';
import { watchSize } from '@/lib/sharedObserver';

// 堆叠式通知：toast.ok('已保存') / toast.err(error) / toast.show({ tone, title, text, action })。操作名与按钮名一致（“保存” → “已保存”）。
// 同样的消息在显示期间只叠一次（右上角计数）；鼠标停在上面时暂停计时；错误用 role="alert"，其余 role="status"。
// key：同一件事的通知（上架页的检查结果）只留最新一条，来了新的就顶掉旧的；toast.dismiss(key) 收起（内容改了，结果作废）。
// 页面底部有吸附条时，页面把它的高度写进根元素的 --toast-lift，通知整体抬高让开（不盖住吸附条上的按钮）
export type ToastTone = 'ok' | 'bad' | 'info';
export interface ToastInput { tone: ToastTone; text: string; title?: string; action?: { label: string; onClick: () => void }; duration?: number; key?: string }
interface T extends ToastInput { id: number; count: number; leaving?: boolean }

const MAX = 5;
let push: ((t: ToastInput) => void) | null = null;
let dismiss: ((key: string) => void) | null = null;
export const errMessage = (e: unknown) => (e instanceof Error ? e.message : String(e));
export const toast = {
  ok: (text: string) => push?.({ tone: 'ok', text }),
  info: (text: string) => push?.({ tone: 'info', text }),
  err: (e: unknown) => push?.({ tone: 'bad', text: errMessage(e) }),
  show: (t: ToastInput) => push?.(t),
  dismiss: (key: string) => dismiss?.(key),
};


/** 页面底部的吸附条（上架页的「检查 / 开始上架」）：挂着时按它的高度把通知抬高（--toast-lift），卸载时放回 */
export function useToastLift(ref: React.RefObject<HTMLElement | null>) {
  React.useEffect(() => {
    const el = ref.current; if (!el) return;
    const root = document.documentElement.style;
    const off = watchSize(el, () => root.setProperty('--toast-lift', `${el.offsetHeight + 8}px`));
    return () => { off(); root.removeProperty('--toast-lift'); };
  }, [ref]);
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
    <div className="toast" data-tone={t.tone} role={t.tone === 'bad' ? 'alert' : 'status'} data-leaving={t.leaving ? '1' : undefined}
      onMouseEnter={() => setPaused(true)} onMouseLeave={() => setPaused(false)} onFocus={() => setPaused(true)} onBlur={() => setPaused(false)}>
      <Ic size={16} />
      <div className="toast-body">
        {t.title && <div className="toast-title">{t.title}</div>}
        <div className={cn(t.title && 'toast-text')}>{t.text}</div>
        {t.action && <button type="button" className="toast-action" onClick={() => { t.action!.onClick(); onClose(t.id); }}>{t.action.label}</button>}
      </div>
      {t.count > 1 && <span className="toast-count" aria-label={`重复 ${t.count} 次`}>×{t.count}</span>}
      <button type="button" className="icon-btn icon-btn-xs toast-x" aria-label="关闭通知" onClick={() => onClose(t.id)}><X size={14} /></button>
      <span className="toast-timer" style={{ animationDuration: `${life}ms`, animationPlayState: paused ? 'paused' : 'running' }} key={t.count} />
    </div>
  );
}

export function Toaster() {
  const [list, setList] = React.useState<T[]>([]);
  const seq = React.useRef(0);
  const listRef = React.useRef(list); listRef.current = list;
  const close = React.useCallback((id: number) => {
    setList((l) => l.map((x) => (x.id === id ? { ...x, leaving: true } : x)));
    setTimeout(() => setList((l) => l.filter((x) => x.id !== id)), durMs('--dur-2')); // 等 .toast 的退场过渡（--dur-2，减弱动效时 0）播完再卸载
  }, []);
  React.useEffect(() => {
    // 带 key 的：先收起同 key 的旧通知（同样内容的也不叠计数，重新出现一条）
    dismiss = (key) => listRef.current.filter((x) => x.key === key && !x.leaving).forEach((x) => close(x.id));
    push = (t) => {
      if (t.key) dismiss!(t.key);
      setList((l) => {
        const dup = !t.key && l.find((x) => !x.leaving && !x.key && x.tone === t.tone && x.text === t.text && x.title === t.title && !x.action && !t.action);
        if (dup) return l.map((x) => (x === dup ? { ...x, count: x.count + 1 } : x));
        return [...l, { ...t, id: ++seq.current, count: 1 }];
      });
    };
    return () => { push = null; dismiss = null; };
  }, [close]);
  // 超过上限：最旧的一条走正常的退场（向右滑出再卸载），不再从列表里直接切掉、瞬间消失
  React.useEffect(() => {
    const live = list.filter((x) => !x.leaving);
    if (live.length > MAX) close(live[0].id);
  }, [list, close]);
  return (
    <div className="toasts">
      {/* 每条套一层布局容器：有通知进来或离开时，其余的滑到新位置而不是瞬间跳过去。
          layout 不放在 .toast 上：motion 的 transform 会盖掉它离场时的 translateX */}
      {list.map((t) => <m.div key={t.id} layout="position" transition={SPRING}><ToastItem t={t} onClose={close} /></m.div>)}
    </div>
  );
}
