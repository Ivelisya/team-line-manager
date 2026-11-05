import * as React from "react"
import { Dialog as DialogPrimitive } from "@base-ui/react/dialog"
import { X } from "lucide-react"

import { cn } from "@/lib/utils"
import { durMs } from "@/lib/motion"
import { IconButton } from "@/components/ui/button"
import { radixModalOpen } from "@/lib/baseLayers"

/* shadcn base-mira 的 Dialog（Base UI），外观走令牌；进出场动画在 components.css（[data-slot=dialog-*] 的 [data-open] / [data-closed]）。
   和 shadcn 原版的差别（都是为了守住改前的行为）：
   - 对话框顶部锚定（内容异步变长只往下长），宽度三档 sm 440 / md 660 / lg 760；手机（≤600px）是底部面板，右上角补关闭键；
   - 遮罩：毛玻璃、嵌套时第二层只轻压（forceRender）；挂着（含退场动画期间）时 <html data-overlay> 计数；
   - 「拦下关闭」：Dialog 的 blockDismiss（有未保存改动、新密钥还没复制）拦下点遮罩 / Esc 时，面板轻轻回弹一下；
   - 初始焦点跳过说明里的「(i)」，关闭后焦点回到打开前的元素（已被别处接走的不抢）。 */

/* ---------- 毛玻璃遮罩开着时：<html data-overlay> ---------- */
// 遮罩挂着（含退场动画期间）时在 <html> 上写 data-overlay，overrides.css 据此停掉下面一层（页面、抽屉）的循环动画和读数过渡，KPI 数字也直接到位（useCountUp）；
// base.css 也据此把页面右侧滚动条槽（锁滚动后留着的空位）同步压暗。
// 不要写成 body:has(.overlay[data-open]) …：:has 的参数里有 [data-open]，悬停提示、菜单开合一改属性，浏览器就要把整页几千个元素重新匹配一遍。
// 计数而不是开关：对话框叠对话框、关一个开一个交替时不会被提前清掉。节点抽屉的遮罩只压暗不模糊，不挂这个。
let shown = 0
function useOverlayFlag() {
  React.useLayoutEffect(() => {
    if (shown++ === 0) document.documentElement.dataset.overlay = ""
    return () => { if (--shown === 0) delete document.documentElement.dataset.overlay }
  }, [])
}

/** 把调用方传的 ref 与内部的面板 ref 合在一起（React 19 里 ref 是普通 prop，直接 {...props} 会盖掉内部的） */
export function mergeRefs<T>(...refs: Array<React.Ref<T> | undefined>): React.RefCallback<T> {
  return (node) => {
    for (const r of refs) {
      if (typeof r === "function") r(node)
      else if (r) (r as React.MutableRefObject<T | null>).current = node
    }
  }
}

/* ---------- 打开前的焦点 ---------- */
/** 打开时记下焦点在谁身上，关闭后还给它：调用方没自己指定 finalFocus；那个元素还在文档里；焦点此刻在 body 或刚关掉的面板里
 *  （已被别处接走的，如 MachineGrid 还给表格行、AnchoredDropdown 交给回调的，不抢）。从菜单项打开的，记下的菜单项已卸载，什么都不做。
 *  不能放在 initialFocus 里记：内容里有 autoFocus 输入框（命令面板、各种表单对话框）时，挂载那一刻焦点就已在面板里，所以统一由 RememberFocus 在内容挂载之前调用 */
export function useRestoreFocus(panel: React.RefObject<HTMLElement | null>) {
  const back = React.useRef<HTMLElement | null>(null)
  return {
    remember: () => {
      const a = document.activeElement as HTMLElement | null
      back.current = a && a !== document.body && !panel.current?.contains(a) ? a : null
    },
    /** 给 Popup 的 finalFocus：返回要还焦点的元素，或 false（不还）。没有副作用（不清 back）：Base UI 在 effect 清理时调它，同一次提交里 effect 若又重跑
     *  （严格模式、开着时依赖变了），清掉了就真正关闭时还不了；下次打开 RememberFocus 会重新记 */
    finalFocus: () => {
      const el = back.current
      const a = document.activeElement
      return el?.isConnected && (!a || a === document.body || !!panel.current?.contains(a)) ? el : false
    },
  }
}

/** 放在面板 Popup 之前（同一个 Portal 里）：兄弟节点按顺序跑 layout effect，先于 Popup 子树里输入框的 autoFocus（React 在提交的 layout 阶段才 focus()），此时焦点还在打开它的元素上 */
export function RememberFocus({ focus }: { focus: { remember: () => void } }) {
  // 只记一次：开发模式的严格模式会把 effect 跑两遍，第二遍时输入框已被 autoFocus 抢到焦点，会把「打开前的元素」盖成面板自己的输入框
  const done = React.useRef(false)
  React.useLayoutEffect(() => { if (!done.current) { done.current = true; focus.remember() } }, [])
  return null
}

/** 触屏（pointer: coarse）：内容里有 autoFocus 的输入框时，React 在挂载那一刻就聚焦了它，软键盘会弹出来盖住底部面板的按钮。放在面板内容的最后（它的 layout effect 在前面的 autoFocus 之后跑，面板的 ref 此时还没挂上，所以晚一个微任务），
 *  发现焦点落在输入框上就挪到面板自己身上；调用方自己指定了 initialFocus 的不动 */
function TouchBlur({ panel, enabled }: { panel: React.RefObject<HTMLElement | null>; enabled: boolean }) {
  React.useLayoutEffect(() => {
    if (!enabled || !matchMedia("(pointer: coarse)").matches) return
    // 面板的 ref 在子级的 layout effect 之后才挂上，晚一个微任务再看
    queueMicrotask(() => {
      const a = document.activeElement as HTMLElement | null
      if (a && panel.current?.contains(a) && a.matches("input, textarea")) panel.current.focus({ preventScroll: true })
    })
  }, [])
  return null
}

/** 打开时的初始焦点跳过说明里的「(i)」：不然键盘打开对话框时提示气泡立刻盖在内容上、按 Enter 也没反应。落到第一个输入框或按钮上
 *  （单选组里不在 Tab 顺序的按钮不算，焦点落在已选的那颗）。Base UI 的复选框 / 开关旁边各有一个隐藏的原生 input（tabindex=-1、aria-hidden）：
 *  不算，焦点要落在看得见的那个控件（role=checkbox / switch 的 span）上。找不到时返回 true，走 Base UI 的默认（第一个可聚焦的，没有就是面板自己） */
const FIRST_FOCUSABLE = 'input:not([disabled]):not([type="hidden"]):not([tabindex="-1"]):not([aria-hidden="true"]), textarea:not([disabled]):not([tabindex="-1"]), select:not([disabled]):not([tabindex="-1"]), button:not([disabled]):not([tabindex="-1"]), [href], [tabindex]:not([tabindex="-1"]):not(.info-tip)'

/* ---------- Dialog（根）：拦下关闭 + 回弹 ---------- */
type DismissReason = "outside-press" | "escape-key"
const DialogContext = React.createContext<{ panel: React.RefObject<HTMLDivElement | null> } | null>(null)
const useDialogContext = () => {
  const c = React.useContext(DialogContext)
  if (!c) throw new Error("DialogContent 要放在 Dialog 里面")
  return c
}

/** 点遮罩 / Esc 想关、却被拦下：面板轻轻回弹一下（1 → 1.02 → 1，--dur-2 + --ease-out），告诉用户“点到了，但不会关”，不再毫无反应。
 *  减弱动效时 --dur-2 为 0，不弹。缩放用独立的 scale 属性，与居中的 translate、进出场的动画互不干扰 */
export function bounce(panel: HTMLElement | null) {
  const ms = durMs("--dur-2")
  if (ms > 0) panel?.animate([{ scale: "1" }, { scale: "1.02" }, { scale: "1" }], { duration: ms, easing: getComputedStyle(document.documentElement).getPropertyValue("--ease-out").trim() })
}

/** 提示（悬停卡 / (i) / data-tip）开着时，这下 Esc 只用来收起提示，不关对话框 / 抽屉（同改前 Radix 层栈的先后）：
 *  Base UI 的层不看 defaultPrevented，所以在 onOpenChange 里按 reason 拦。tooltip.tsx 的 TooltipProvider 在 window 捕获阶段会把这下 Esc 标成 defaultPrevented
 *  （提示自己的收起与对话框的关闭在同一次按键里先后发生，到这里时提示可能已经收起了，所以两个条件都看）；Radix 的层（菜单、下拉）收起时也会 preventDefault，一并认 */
export const escapeIsTaken = (e: Event) => e.defaultPrevented || !!document.querySelector('[data-slot="tooltip-content"][data-open]')

// Radix 的模态层（下拉选择、菜单；B2b 换成 Base UI 后可删）开着时，它给 body 加 pointer-events: none，点对话框里任何地方都落到 html 上，Base UI 当成「点了外面」而关掉对话框。
// 在 pointerdown 的捕获阶段（Radix 还没来得及卸载）记下当时有没有 Radix 层，outside-press 时若有就取消：这一下只用来收起 Radix 的层
let radixAtPress = false
if (typeof window !== "undefined") window.addEventListener("pointerdown", () => { radixAtPress = radixModalOpen() }, true)

function Dialog({
  open,
  onOpenChange,
  blockDismiss,
  ...props
}: Omit<DialogPrimitive.Root.Props, "onOpenChange"> & {
  onOpenChange?: (open: boolean, details: DialogPrimitive.Root.ChangeEventDetails) => void
  /** 拦下点遮罩 / Esc 关闭（true = 两个都拦；函数按原因决定）：拦下时面板回弹一下，不调用 onOpenChange */
  blockDismiss?: boolean | ((reason: DismissReason) => boolean)
}) {
  const panel = React.useRef<HTMLDivElement>(null)
  const handle = (next: boolean, details: DialogPrimitive.Root.ChangeEventDetails) => {
    if (!next && (details.reason === "escape-key" || details.reason === "outside-press")) {
      const reason: DismissReason = details.reason
      if (reason === "outside-press") {
        if (radixAtPress || radixModalOpen()) { details.cancel(); return }
        // Ctrl+左键点遮罩不关（mac 上是右键；改前 Radix 同样当右键处理），不回弹
        const e = details.event as MouseEvent
        if (e.button === 0 && e.ctrlKey) { details.cancel(); return }
      }
      if (reason === "escape-key" && escapeIsTaken(details.event)) { details.cancel(); details.allowPropagation(); return } // allowPropagation：Esc 继续传给提示，由它自己收起
      if (typeof blockDismiss === "function" ? blockDismiss(reason) : blockDismiss) { details.cancel(); bounce(panel.current); return }
    }
    onOpenChange?.(next, details)
  }
  return (
    <DialogContext.Provider value={{ panel }}>
      <DialogPrimitive.Root open={open} onOpenChange={handle} {...props} />
    </DialogContext.Provider>
  )
}

function DialogTrigger({ ...props }: DialogPrimitive.Trigger.Props) {
  return <DialogPrimitive.Trigger data-slot="dialog-trigger" {...props} />
}

function DialogPortal({ ...props }: DialogPrimitive.Portal.Props) {
  return <DialogPrimitive.Portal data-slot="dialog-portal" {...props} />
}

function DialogClose({ ...props }: DialogPrimitive.Close.Props) {
  return <DialogPrimitive.Close data-slot="dialog-close" {...props} />
}

/** 毛玻璃遮罩（对话框、命令面板共用）。forceRender：Base UI 对嵌套的对话框默认不渲染子级遮罩，我们要它（抽屉上再开对话框、对话框上再开确认框，
 *  第二层遮罩只轻压 --scrim-stacked，见 components.css），计数照样加上。外观与进出场在 components.css 的 [data-slot=dialog-overlay] */
function DialogOverlay({ className, ...props }: DialogPrimitive.Backdrop.Props) {
  useOverlayFlag()
  return <DialogPrimitive.Backdrop forceRender data-slot="dialog-overlay" className={className} {...props} />
}

const SIZE = { sm: "w-[min(var(--dialog-sm),calc(100vw-32px))]", md: "w-[min(var(--dialog-md),calc(100vw-32px))]", lg: "w-[min(var(--dialog-lg),calc(100vw-32px))]" } as const

function DialogContent({
  className,
  children,
  size = "sm",
  showCloseButton = true,
  initialFocus,
  finalFocus,
  ref,
  ...props
}: DialogPrimitive.Popup.Props & {
  /** sm 440 / md 660 / lg 760 */
  size?: keyof typeof SIZE
  /** 手机底部面板右上角的关闭键（宽屏不显示）；自带关闭键的（快捷键帮助）传 false */
  showCloseButton?: boolean
}) {
  const { panel } = useDialogContext()
  const focus = useRestoreFocus(panel)
  const merged = React.useMemo(() => mergeRefs(panel, ref), [panel, ref])
  return (
    <DialogPortal>
      <DialogOverlay />
      <RememberFocus focus={focus} />
      <DialogPrimitive.Popup
        ref={merged}
        data-slot="dialog-content"
        // 触屏打开时焦点放在面板上（聚焦输入框会弹出软键盘，盖住底部面板的按钮）：受控打开没有触发事件，Base UI 给的 openType 常是空，所以再认「主要指针是粗的（pointer: coarse）」；其余跳过 (i)
        initialFocus={initialFocus ?? ((openType) => (openType === "touch" || matchMedia("(pointer: coarse)").matches ? panel.current : panel.current?.querySelector<HTMLElement>(FIRST_FOCUSABLE) ?? true))}
        finalFocus={finalFocus ?? focus.finalFocus}
        className={cn(
          // 顶部锚定（同命令面板）：内容异步变多时只往下长；矮屏时整块滚动，底部按钮条吸底（DialogFooter），不会被截掉。
          // 水平居中用独立的 translate 属性：进出场动画只写自己的 transform，点遮罩被拦下时的回弹（独立 scale）也不用管居中
          "fixed top-[min(10vh,96px)] left-1/2 z-(--z-dialog) flex max-h-[calc(100dvh-min(10vh,96px)-16px)] origin-top flex-col gap-5 overflow-auto rounded-section border border-(--line-float) bg-raised px-6 pt-6 pb-(--dlg-pb) shadow-(--shadow-modal) [--dlg-pb:var(--sp-24)] [translate:-50%_0] *:flex-none",
          SIZE[size],
          // 手机（≤600px）：从底部升起的面板（同 iOS 的 sheet）——通栏、上圆角、底部让出安全区。max-[601px] = 宽度 ≤ 600
          "max-[601px]:top-auto max-[601px]:bottom-0 max-[601px]:left-0 max-[601px]:max-h-[calc(100dvh-var(--topbar-h))] max-[601px]:w-screen max-[601px]:origin-bottom max-[601px]:rounded-b-none max-[601px]:border-x-0 max-[601px]:border-b-0 max-[601px]:[--dlg-pb:calc(var(--sp-24)+env(safe-area-inset-bottom,0px))] max-[601px]:[translate:none]",
          className
        )}
        {...props}
      >
        {children}
        <TouchBlur panel={panel} enabled={initialFocus === undefined} />
        {showCloseButton && (
          <DialogPrimitive.Close
            data-slot="dialog-close"
            render={(p) => <IconButton {...(p as object)} label="关闭" className="absolute top-3 right-3 hidden max-[601px]:inline-flex" />}
          >
            <X size={16} />
          </DialogPrimitive.Close>
        )}
      </DialogPrimitive.Popup>
    </DialogPortal>
  )
}

/** 标题 + 说明的头部块（标题与说明之间固定的小间距）；手机上右边让出关闭键的位置 */
function DialogHeader({ className, ...props }: React.ComponentProps<"div">) {
  return <div data-slot="dialog-header" className={cn("grid gap-2 max-[601px]:pr-[calc(var(--h-md)+var(--sp-4))]", className)} {...props} />
}

/** 底部按钮条：吸底（sticky 的 bottom 按滚动容器内边距内缩，取负的下内边距才贴到弹层底边），毛玻璃底，通栏（抵消弹层的内边距）；
 *  内容滚到下面时顶上出一条细线（components.css 的 ::before）；不滚时与原来的位置、间距一致。
 *  按钮高度、手机上 44px 等分在 button.tsx 里按 [data-slot=dialog-footer] > [data-slot=button] 写 */
function DialogFooter({ className, ...props }: React.ComponentProps<"div">) {
  return (
    <div
      data-slot="dialog-footer"
      className={cn(
        "sticky bottom-[calc(-1*var(--dlg-pb))] z-1 -mx-6 -mt-2 -mb-(--dlg-pb) flex flex-wrap justify-end gap-(--control-gap) bg-(--glass) px-6 pt-3 pb-(--dlg-pb) [-webkit-backdrop-filter:var(--glass-blur)] [backdrop-filter:var(--glass-blur)]",
        className
      )}
      {...props}
    />
  )
}

function DialogTitle({ className, ...props }: DialogPrimitive.Title.Props) {
  return <DialogPrimitive.Title data-slot="dialog-title" className={cn("m-0 text-section font-semibold tracking-(--tracking-title) text-balance", className)} {...props} />
}

function DialogDescription({ className, ...props }: DialogPrimitive.Description.Props) {
  return <DialogPrimitive.Description data-slot="dialog-description" className={cn("m-0 text-body text-fg-2 text-pretty", className)} {...props} />
}

export {
  Dialog,
  DialogClose,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogOverlay,
  DialogPortal,
  DialogTitle,
  DialogTrigger,
}
