import * as React from "react"
import { Toast } from "@base-ui/react/toast"
import { CircleAlertIcon, CircleCheckIcon, InfoIcon, XIcon } from "lucide-react"

import { Button } from "@/components/ui/button"
import { cn } from "@/lib/utils"
import { watchSize } from "@/lib/sharedObserver"

// 堆叠式通知（coss ui 的 toast 为底，Base UI Toast）：toast.ok('已保存') / toast.err(error) / toast.show({ tone, title, text, action })。操作名与按钮名一致（“保存” → “已保存”）。
// 折叠时最新的一条在最前面完整可读，旧的依次缩小、往后叠、只露一道边；鼠标悬停 / 键盘聚焦时整组展开（每条完整可读）并暂停计时（Base UI 的 Viewport）。
// 和 coss 原版的差别：对外是 toast.* 一组函数（不暴露 manager）；外观走令牌；时长走动效令牌（减弱动效时即刻完成）；不做拖动关闭（错误信息常要框选复制）；
// 相同消息在显示期间只叠一次（×N，Base UI 只按 id 去重：id 由 tone + title + text 生成，再来一条就更新这一条的计数并重新计时）；
// key：同一件事的通知（上架页的检查结果）只留最新一条，来了新的就在原位更新；toast.dismiss(key) 收起（内容改了，结果作废）。
// 页面底部有吸附条时，页面把它的高度写进根元素的 --toast-lift，通知整体抬高让开（不盖住吸附条上的按钮）。视口位置、抽屉让位见 components.css 的 [data-slot=toast-viewport]
export type ToastTone = "ok" | "bad" | "info"
export interface ToastInput { tone: ToastTone; text: string; title?: string; action?: { label: string; onClick: () => void }; duration?: number; key?: string }
interface ToastData { count: number; life: number }

const MAX = 5
const LIFE = { ok: 3600, info: 5000, bad: 7000 } as const
const manager = Toast.createToastManager()
let live = false // Toaster 挂着才收通知（同改前：还没挂载时的通知直接丢）
let seq = 0
const counts = new Map<string, number>() // 显示中的「相同消息」→ 已叠几次；通知开始收起时清掉

export const errMessage = (e: unknown) => (e instanceof Error ? e.message : String(e))
function show(t: ToastInput) {
  if (!live) return
  // 带 key 的、带操作按钮的不叠计数（每条都是独立的一条 / 只留最新一条）
  const merge = !t.key && !t.action
  const id = t.key ? `k:${t.key}` : t.action ? `a:${++seq}` : `m:${t.tone}|${t.title ?? ""}|${t.text}`
  const count = merge ? (counts.get(id) ?? 0) + 1 : 1
  if (merge) counts.set(id, count)
  const life = t.duration ?? (t.action ? 9000 : LIFE[t.tone])
  manager.add({
    id, type: t.tone, title: t.title, description: t.text, timeout: life, data: { count, life } satisfies ToastData,
    onClose: () => { counts.delete(id) },
    // 点了执行并关闭
    actionProps: t.action ? { children: t.action.label, onClick: () => { t.action!.onClick(); manager.close(id) } } : undefined,
  })
}
export const toast = {
  ok: (text: string) => show({ tone: "ok", text }),
  info: (text: string) => show({ tone: "info", text }),
  err: (e: unknown) => show({ tone: "bad", text: errMessage(e) }),
  show,
  dismiss: (key: string) => manager.close(`k:${key}`),
}

/** 页面底部的吸附条（上架页的「检查 / 开始上架」）：挂着时按它的高度把通知抬高（--toast-lift），卸载时放回 */
export function useToastLift(ref: React.RefObject<HTMLElement | null>) {
  React.useEffect(() => {
    const el = ref.current; if (!el) return
    const root = document.documentElement.style
    const off = watchSize(el, () => root.setProperty("--toast-lift", `${el.offsetHeight + 8}px`))
    return () => { off(); root.removeProperty("--toast-lift") }
  }, [ref])
}

const ICON = { ok: CircleCheckIcon, bad: CircleAlertIcon, info: InfoIcon } as const
const TONE = { ok: "text-ok", bad: "text-bad", info: "text-info" } as const
const BAR = { ok: "bg-ok", bad: "bg-bad", info: "bg-info" } as const
// 每种语气给玻璃底一层很淡的同色晕（左上角起，到 60% 处散尽）
const WASH = { ok: "from-ok/12", bad: "from-bad/12", info: "from-info/12" } as const

/** 手机（≤600px）通知从顶栏下方落下，宽屏在右下角 */
function usePhone() {
  return React.useSyncExternalStore(
    (cb) => { const m = matchMedia("(max-width: 600px)"); m.addEventListener("change", cb); return () => m.removeEventListener("change", cb) },
    () => matchMedia("(max-width: 600px)").matches,
    () => false,
  )
}

type Item = ReturnType<typeof Toast.useToastManager<ToastData>>["toasts"][number]

function ToastItem({ t, top }: { t: Item; top: boolean }) {
  const tone = (t.type ?? "info") as ToastTone
  const Icon = ICON[tone]
  const count = t.data?.count ?? 1
  const dir = top ? "top" : "bottom"
  return (
    <Toast.Root
      toast={t}
      data-slot="toast"
      data-tone={tone}
      data-position={dir}
      role={tone === "bad" ? "alert" : "status"}
      tabIndex={-1}
      swipeDirection={[]}
      className={cn(
        // 堆叠：--toast-index 是第几条（0 = 最前），折叠时按序号缩小、往后叠（--toast-peek 露出的边），展开（悬停 / 聚焦，data-expanded）时按各自高度排开
        "absolute z-[calc(9999-var(--toast-index))] h-(--toast-calc-height) w-full rounded-card border border-(--line-float) bg-(--glass) bg-linear-135 to-transparent to-60% text-body text-popover-foreground shadow-(--shadow-pop) select-none",
        "[-webkit-backdrop-filter:var(--glass-blur)] [backdrop-filter:var(--glass-blur)]",
        "[--toast-calc-height:var(--toast-frontmost-height,var(--toast-height))] [--toast-gap:var(--sp-8)] [--toast-peek:var(--sp-10)] [--toast-scale:calc(max(0,1-(var(--toast-index)*.06)))] [--toast-shrink:calc(1-var(--toast-scale))]",
        "[transition:transform_var(--dur-3)_var(--ease-out),opacity_var(--dur-2)_var(--ease-out),height_var(--dur-1)_var(--ease-std)]",
        // 间隙填上：鼠标在两条之间移动时不会掉出「悬停」
        "after:absolute after:left-0 after:h-[calc(var(--toast-gap)+1px)] after:w-full",
        top
          ? "top-0 bottom-auto origin-[50%_calc(50%-50%*min(var(--toast-index,0),1))] [--toast-calc-offset-y:calc(var(--toast-offset-y)+var(--toast-index)*var(--toast-gap)+var(--toast-swipe-movement-y))] [transform:translateY(calc((var(--toast-index)*var(--toast-peek))+(var(--toast-shrink)*var(--toast-calc-height))))_scale(var(--toast-scale))] after:top-full"
          : "top-auto bottom-0 origin-[50%_calc(50%+50%*min(var(--toast-index,0),1))] [--toast-calc-offset-y:calc(var(--toast-offset-y)*-1+var(--toast-index)*var(--toast-gap)*-1+var(--toast-swipe-movement-y))] [transform:translateY(calc(-1*(var(--toast-index)*var(--toast-peek))-(var(--toast-shrink)*var(--toast-calc-height))))_scale(var(--toast-scale))] after:bottom-full",
        "data-limited:opacity-0 data-expanded:h-(--toast-height) data-expanded:[transform:translateY(var(--toast-calc-offset-y))]",
        // 进场：从屏幕边缘外升起 / 落下；退场：淡出并往边缘收
        top ? "data-starting-style:[transform:translateY(calc(-100%-var(--sp-16)))]" : "data-starting-style:[transform:translateY(calc(100%+var(--sp-16)))]",
        top ? "data-ending-style:[transform:translateY(calc(-100%-var(--sp-16)))]" : "data-ending-style:[transform:translateY(calc(100%+var(--sp-16)))]",
        "data-ending-style:opacity-0",
        WASH[tone],
      )}
    >
      <Toast.Content
        // 折叠时后面的几条只露边、内容藏起来（最前面那条完整可读）；展开后每条完整可读
        className="pointer-events-auto relative flex h-full items-start gap-3 overflow-hidden rounded-[inherit] px-3.5 py-3 transition-opacity duration-(--dur-2) data-behind:not-data-expanded:pointer-events-none data-behind:opacity-0 data-expanded:opacity-100"
      >
        <Icon size={18} className={cn("mt-px shrink-0", TONE[tone])} />
        <div className="min-w-0 flex-1 wrap-anywhere">
          {t.title && <Toast.Title render={<div />} className="font-semibold" data-slot="toast-title" />}
          <Toast.Description render={<div />} className={cn(t.title && "text-label text-fg-2")} data-slot="toast-description" />
          {t.actionProps && <Toast.Action render={<Button variant="link" size="inline-sm" className="mt-1" />} data-slot="toast-action" />}
        </div>
        {count > 1 && <span key={count} className="flex-none animate-[count-pop_var(--dur-2)_var(--ease-out)] self-center rounded-pill bg-sunken px-1.5 text-micro leading-(--h-chip-sm) text-fg-2" aria-label={`重复 ${count} 次`}>×{count}</span>}
        <Toast.Close render={<Button variant="ghost" size="icon-xs" className="-mt-0.5 -mr-1 flex-none text-fg-3" aria-label="关闭通知" />}><XIcon size={14} /></Toast.Close>
        {/* 剩余时间细条：key 跟着 updateKey，相同消息又来一条时重新走；整组悬停 / 聚焦暂停时（Root 带 data-expanded）一起暂停 */}
        <span key={t.updateKey} aria-hidden style={{ animationDuration: `${t.data?.life ?? t.timeout}ms` }}
          className={cn("absolute inset-x-3.5 bottom-1.5 h-0.5 origin-left rounded-full opacity-40 animate-[toast-timer_linear_forwards] in-data-expanded:[animation-play-state:paused] motion-reduce:animate-none", BAR[tone])} />
      </Toast.Content>
    </Toast.Root>
  )
}

function ToastList() {
  const { toasts, close } = Toast.useToastManager<ToastData>()
  const phone = usePhone()
  // 超过上限（Base UI 只给最旧的打上 limited、不移除）：最旧的一条走正常的退场，不瞬间消失
  React.useEffect(() => { for (const t of toasts) if (t.limited && t.transitionStatus !== "ending") close(t.id) }, [toasts, close])
  return (
    <Toast.Portal data-slot="toast-portal">
      <Toast.Viewport data-slot="toast-viewport" data-position={phone ? "top" : "bottom"} aria-label="通知">
        {toasts.map((t) => <ToastItem key={t.id} t={t} top={phone} />)}
      </Toast.Viewport>
    </Toast.Portal>
  )
}

export function Toaster() {
  React.useEffect(() => { live = true; return () => { live = false; counts.clear() } }, [])
  return <Toast.Provider toastManager={manager} limit={MAX}><ToastList /></Toast.Provider>
}
