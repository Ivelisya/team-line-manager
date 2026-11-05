import * as React from 'react';
import { Toast } from '@base-ui/react/toast';
import { CheckCircle2, Info, X, XCircle } from 'lucide-react';
import { m } from 'motion/react';
import { Button } from '@/components/ui/button';
import { cn } from '@/lib/utils';
import { SPRING } from '@/lib/motion';
import { watchSize } from '@/lib/sharedObserver';

// 堆叠式通知（Base UI Toast）：toast.ok('已保存') / toast.err(error) / toast.show({ tone, title, text, action })。操作名与按钮名一致（“保存” → “已保存”）。
// shadcn base-mira 没有 toast（它用的 sonner 不是 Base UI），coss 的 toast 是折叠堆叠（绝对定位 + 缩放）、与这里「新的在最下面、旧的往上排」的平铺列表不是一回事，
// 所以直接用 Base UI 的 Toast 部件（Provider / Portal / Viewport / Root / Title / Description / Action / Close）拼，外观走令牌。
// 同样的消息在显示期间只叠一次（×N，Base UI 只按 id 去重：id 由 tone + title + text 生成，再来一条就更新这一条的计数并重新计时）；
// 鼠标停在任一条上，整组暂停计时（Base UI 的 Viewport 悬停 / 键盘聚焦暂停）；错误用 role="alert"，其余 role="status"。
// key：同一件事的通知（上架页的检查结果）只留最新一条，来了新的就在原位更新（内容、计时重来）；toast.dismiss(key) 收起（内容改了，结果作废）。
// 页面底部有吸附条时，页面把它的高度写进根元素的 --toast-lift，通知整体抬高让开（不盖住吸附条上的按钮）。位置、进出场见 components.css 的 [data-slot=toast-*]
export type ToastTone = 'ok' | 'bad' | 'info';
export interface ToastInput { tone: ToastTone; text: string; title?: string; action?: { label: string; onClick: () => void }; duration?: number; key?: string }
interface ToastData { count: number; life: number }

const MAX = 5;
const LIFE = { ok: 3600, info: 5000, bad: 7000 } as const;
const manager = Toast.createToastManager();
let live = false; // Toaster 挂着才收通知（同改前：还没挂载时的通知直接丢）
let seq = 0;
const counts = new Map<string, number>(); // 显示中的「相同消息」→ 已叠几次；通知开始收起时清掉

export const errMessage = (e: unknown) => (e instanceof Error ? e.message : String(e));
function show(t: ToastInput) {
  if (!live) return;
  // 带 key 的、带操作按钮的不叠计数（每条都是独立的一条 / 只留最新一条）
  const merge = !t.key && !t.action;
  const id = t.key ? `k:${t.key}` : t.action ? `a:${++seq}` : `m:${t.tone}|${t.title ?? ''}|${t.text}`;
  const count = merge ? (counts.get(id) ?? 0) + 1 : 1;
  if (merge) counts.set(id, count);
  const life = t.duration ?? (t.action ? 9000 : LIFE[t.tone]);
  manager.add({
    id, type: t.tone, title: t.title, description: t.text, timeout: life, data: { count, life } satisfies ToastData,
    onClose: () => { counts.delete(id); },
    // 点了执行并关闭
    actionProps: t.action ? { children: t.action.label, onClick: () => { t.action!.onClick(); manager.close(id); } } : undefined,
  });
}
export const toast = {
  ok: (text: string) => show({ tone: 'ok', text }),
  info: (text: string) => show({ tone: 'info', text }),
  err: (e: unknown) => show({ tone: 'bad', text: errMessage(e) }),
  show,
  dismiss: (key: string) => manager.close(`k:${key}`),
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
const TONE = { ok: 'text-ok', bad: 'text-bad', info: 'text-info' } as const;
const BAR = { ok: 'bg-ok', bad: 'bg-bad', info: 'bg-info' } as const;
// 每种语气给玻璃底一层很淡的同色晕（左上角起，到 60% 处散尽）
const WASH = { ok: 'from-ok/12', bad: 'from-bad/12', info: 'from-info/12' } as const;

function ToastItem({ t }: { t: ReturnType<typeof Toast.useToastManager<ToastData>>['toasts'][number] }) {
  const tone = (t.type ?? 'info') as ToastTone;
  const Ic = ICON[tone];
  const count = t.data?.count ?? 1;
  return (
    <Toast.Root toast={t} data-slot="toast" data-tone={tone} role={tone === 'bad' ? 'alert' : 'status'} tabIndex={-1}
      swipeDirection={[]} // 不做拖动关闭：错误信息常要框选复制，拖一下就把它移走了；关闭有 ×，也会自己消失
      className={cn('pointer-events-auto relative flex items-start gap-3 overflow-hidden rounded-card border border-(--line-float) bg-(--glass) bg-linear-135 to-transparent to-60% px-3.5 py-3 text-body shadow-(--shadow-pop) [backdrop-filter:var(--glass-blur)]', WASH[tone])}>
      <Ic size={18} className={cn('mt-px', TONE[tone])} />
      <div className="min-w-0 flex-1 wrap-anywhere">
        {t.title && <Toast.Title render={<div />} className="font-semibold" />}
        <Toast.Description render={<div />} className={cn(t.title && 'text-label text-fg-2')} />
        {t.actionProps && <Toast.Action render={<Button variant="link" size="inline-sm" className="mt-1" />} />}
      </div>
      {count > 1 && <span key={count} className="flex-none animate-[count-pop_var(--dur-2)_var(--ease-out)] self-center rounded-pill bg-sunken px-1.5 text-micro leading-(--h-chip-sm) text-fg-2" aria-label={`重复 ${count} 次`}>×{count}</span>}
      <Toast.Close render={<Button variant="ghost" size="icon-xs" className="-mt-0.5 -mr-1 flex-none text-fg-3" aria-label="关闭通知" />}><X size={14} /></Toast.Close>
      {/* 剩余时间细条：key 跟着 updateKey，相同消息又来一条时重新走；整组悬停 / 聚焦暂停时（Root 带 data-expanded）一起暂停 */}
      <span key={t.updateKey} aria-hidden style={{ animationDuration: `${t.data?.life ?? t.timeout}ms` }}
        className={cn('absolute inset-x-3.5 bottom-1.5 h-0.5 origin-left rounded-full opacity-40 animate-[toast-timer_linear_forwards] in-data-expanded:[animation-play-state:paused] motion-reduce:animate-none', BAR[tone])} />
    </Toast.Root>
  );
}

function ToastList() {
  const { toasts, close } = Toast.useToastManager<ToastData>();
  // 超过上限（Base UI 只给最旧的打上 limited、不移除）：最旧的一条走正常的退场（向右滑出再卸载），不瞬间消失
  React.useEffect(() => { for (const t of toasts) if (t.limited && t.transitionStatus !== 'ending') close(t.id); }, [toasts, close]);
  return (
    <Toast.Portal>
      <Toast.Viewport data-slot="toast-viewport" aria-label="通知">
        {/* 每条套一层布局容器：有通知进来或离开时，其余的滑到新位置而不是瞬间跳过去。
            layout 不放在 Root 上：motion 的 transform 会盖掉它进出场的 transform */}
        {/* Base UI 的列表是新的在前，倒过来：新的在最下面、旧的往上排 */}
        {[...toasts].reverse().map((t) => <m.div key={t.id} layout="position" transition={SPRING}><ToastItem t={t} /></m.div>)}
      </Toast.Viewport>
    </Toast.Portal>
  );
}

export function Toaster() {
  React.useEffect(() => { live = true; return () => { live = false; counts.clear(); }; }, []);
  return <Toast.Provider toastManager={manager} limit={MAX}><ToastList /></Toast.Provider>;
}
