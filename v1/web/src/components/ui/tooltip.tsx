"use client"

import * as React from "react"
import { Tooltip as TooltipPrimitive } from "@base-ui/react/tooltip"

import { cn } from "@/lib/utils"
import { watchSize } from "@/lib/sharedObserver"

/* shadcn base-mira 的 Tooltip（Base UI）。单独成文件：客户页只要提示，不必带上菜单、弹层、页签、motion。
   提示框外观：反色实底（--tip-*，暗色见 tokens.css）、--shadow-pop + 内描边；进场 --dur-2 / --ease-out（淡入 + 从 .96 放大，朝远离触发元素的方向走 4px），退场 --dur-1 / --ease-std；
   pre-line 让 data-tip 里的换行生效（心跳格说明分行） */

/** 全站提示延迟 250 ms（Mira 默认 0）；刚收起一个又悬停到下一个，Base UI 的 timeout 内直接出现 */
function TooltipProvider({ delay = 250, ...props }: TooltipPrimitive.Provider.Props) {
  return <TooltipPrimitive.Provider data-slot="tooltip-provider" delay={delay} {...props} />
}

function Tooltip({ ...props }: TooltipPrimitive.Root.Props) {
  return <TooltipPrimitive.Root data-slot="tooltip" {...props} />
}

function TooltipTrigger({ ...props }: TooltipPrimitive.Trigger.Props) {
  return <TooltipPrimitive.Trigger data-slot="tooltip-trigger" {...props} />
}

function TooltipContent({
  className,
  side = "top",
  sideOffset = 6,
  align = "center",
  alignOffset = 0,
  collisionPadding = 8,
  anchor,
  children,
  ...props
}: TooltipPrimitive.Popup.Props &
  Pick<
    TooltipPrimitive.Positioner.Props,
    "align" | "alignOffset" | "side" | "sideOffset" | "collisionPadding" | "anchor"
  >) {
  return (
    <TooltipPrimitive.Portal>
      <TooltipPrimitive.Positioner
        align={align}
        alignOffset={alignOffset}
        side={side}
        sideOffset={sideOffset}
        collisionPadding={collisionPadding}
        anchor={anchor}
        className="isolate z-(--z-tooltip)"
      >
        <TooltipPrimitive.Popup
          data-slot="tooltip-content"
          className={cn(
            "w-fit max-w-(--pop-w-md) origin-(--transform-origin) rounded-control bg-(--tip-bg) px-2 py-1 text-label whitespace-pre-line text-(--tip-fg) [box-shadow:var(--shadow-pop),inset_0_0_0_0.5px_var(--tip-ring)]",
            "[--pop-x:0px] [--pop-y:0px] data-[side=top]:[--pop-y:4px] data-[side=bottom]:[--pop-y:-4px] data-[side=left]:[--pop-x:4px] data-[side=right]:[--pop-x:-4px]",
            "[transition:opacity_var(--dur-1)_var(--ease-std),transform_var(--dur-1)_var(--ease-std)] data-open:[transition:opacity_var(--dur-2)_var(--ease-out),transform_var(--dur-2)_var(--ease-out)]",
            "data-starting-style:opacity-0 data-starting-style:[transform:translate(var(--pop-x),var(--pop-y))_scale(.96)] data-ending-style:opacity-0 data-ending-style:[transform:translate(var(--pop-x),var(--pop-y))_scale(.96)]",
            // TipHost 在相邻元素间换项：直接出现、不再播一遍进场（退场照旧）。不叫 data-instant：那是 Base UI 自带的属性
            "data-open:data-tip-instant:transition-none",
            className
          )}
          {...props}
        >
          {children}
        </TooltipPrimitive.Popup>
      </TooltipPrimitive.Positioner>
    </TooltipPrimitive.Portal>
  )
}

/* ---------- TipHost：全站 data-tip 委托的共享提示 ---------- */

type Side = "top" | "bottom" | "left" | "right"

/** 共享悬停提示：卡片、表格行里成百上千的只读说明不各挂一个 Tooltip 根（每个根带一套 Positioner 上下文，随实时推送跟着重渲染），
 *  元素上写 data-tip="文字"，外壳里这一份按悬停 / 键盘聚焦到的元素定位显示（Base UI Positioner 的虚拟锚点）。外观、延迟同 TooltipContent：停 250ms 出现，刚有提示开着时直接换过去。
 *  空的 data-tip 当作没有（拼出来的说明可能为空，调用方不用再判）；data-tip-side 指定弹出方向（默认在上方） */
const TIP_SEL = '[data-tip]:not([data-tip=""])'
export function TipHost() {
  const [tip, setTip] = React.useState<{ id: number; text: string; r: DOMRect; side: Side; instant: boolean } | null>(null)
  const [open, setOpen] = React.useState(false)
  const hide = React.useRef(() => {})
  React.useEffect(() => {
    let cur: Element | null = null; let timer = 0; let shown = false; let closedAt = 0; let seq = 0
    let tapClosed: Element | null = null // 按下时收起的是哪个（再点同一个「(i)」= 收起，不再打开）
    let touch = false // 最近一次按下是手指
    // 对话框标题下面那行说明里的「(i)」往下弹：往上会盖住标题
    const sideOf = (el: HTMLElement): Side => (el.dataset.tipSide as Side | undefined) ?? (el.closest(".dialog-desc, [data-slot=dialog-description]") ? "bottom" : "top")
    // instant：刚收起一个又悬停到下一个（扫过一排心跳格），新提示直接出现、不再播一遍缩放淡入（macOS 也是第一次等、之后换项不播）
    const show = (el: HTMLElement, instant = false) => {
      shown = true; watch.disconnect(); unwatchSize?.(); watch.observe(el, { attributes: true, attributeFilter: ["data-tip"] }); unwatchSize = watchSize(el, refresh)
      setTip({ id: ++seq, text: el.dataset.tip ?? "", r: el.getBoundingClientRect(), side: sideOf(el), instant }); setOpen(true)
    }
    // 悬停着的元素 data-tip 变了（推送改了读数、说明跟着变）、尺寸变了就按新的文字和位置更新（同一个 id，不重播动画）；元素被移出文档（卡片被换掉、页签内容被换掉）就收起。
    // 只盯当前这一个元素：属性观察 + 共用的 ResizeObserver（元素被摘出文档时它也会回调，尺寸变成 0）
    let unwatchSize: (() => void) | null = null
    const watch = new MutationObserver(() => refresh())
    const refresh = () => {
      if (!shown || !cur) return
      const el = cur as HTMLElement
      if (!el.isConnected || !el.matches(TIP_SEL)) { close(); return }
      const text = el.dataset.tip ?? "", r = el.getBoundingClientRect()
      setTip((t) => !t || (t.text === text && t.r.left === r.left && t.r.top === r.top && t.r.width === r.width && t.r.height === r.height) ? t : { ...t, text, r }) // ResizeObserver 挂上时会立刻回调一次，没变就原样返回
    }
    const close = () => {
      clearTimeout(timer); cur = null; watch.disconnect(); unwatchSize?.(); unwatchSize = null
      if (shown) { shown = false; closedAt = performance.now(); setOpen(false) }
    }
    hide.current = close
    const over = (e: PointerEvent) => {
      if (e.pointerType === "touch") return
      const el = (e.target as Element).closest<HTMLElement>(TIP_SEL)
      if (el === cur) return
      close()
      if (!el) return
      cur = el
      if (performance.now() - closedAt < 300) show(el, true); else timer = window.setTimeout(() => show(el), 250)
    }
    // 键盘聚焦才弹；鼠标按下得到的焦点不算
    const focus = (e: FocusEvent) => { const el = (e.target as Element).closest<HTMLElement>(TIP_SEL); if (el?.matches(":focus-visible")) { close(); cur = el; show(el) } }
    const leave = (e: PointerEvent) => { if (!e.relatedTarget) close() } // 指针离开窗口
    // 轻触同一个「(i)」第二下 = 收起；鼠标点开着的「(i)」不收（悬停已经打开了，点它是想看）
    const down = (e: PointerEvent) => { touch = e.pointerType === "touch"; tapClosed = shown && touch ? cur : null; close() }
    // 「(i)」点一下 / 轻触也能打开：触屏没有悬停，鼠标点下去得到的焦点也不弹（只认键盘聚焦）。
    // 触屏上轻触别的带说明的字（省略了的名称、心跳格、小标签）也弹；本身可点的（按钮、链接、整行可点的表格行）点了是去做事，不弹
    const click = (e: MouseEvent) => {
      const t = e.target as Element
      const el = t.closest<HTMLElement>(".info-tip") ?? (touch ? t.closest<HTMLElement>(TIP_SEL) : null)
      if (el && !el.classList.contains("info-tip") && el.closest('a, button, [role="button"], .is-link')) return
      if (!el?.dataset.tip || tapClosed === el) { tapClosed = null; return }
      cur = el; show(el)
    }
    const on: [EventTarget, string, EventListener, boolean][] = [
      [document, "pointerover", over as EventListener, false], [document, "pointerout", leave as EventListener, false], [document, "pointerdown", down as EventListener, true], [document, "click", click as EventListener, false],
      [document, "focusin", focus as EventListener, false], [document, "focusout", close, false], [window, "scroll", close, true], [window, "wheel", close, true],
    ]
    on.forEach(([t, n, f, c]) => t.addEventListener(n, f, c))
    return () => { clearTimeout(timer); watch.disconnect(); unwatchSize?.(); on.forEach(([t, n, f, c]) => t.removeEventListener(n, f, c)) }
  }, [])
  // 虚拟锚点：有 getBoundingClientRect 的对象；矩形变了才换一个（refresh 在文字、位置都没变时原样返回同一个 tip，r 的引用不变）
  const r = tip?.r
  const anchor = React.useMemo(() => (r ? { getBoundingClientRect: () => r } : undefined), [r])
  if (!tip) return null
  // key：换到另一个元素时重新挂载（旧的收起、新的立即出现），不在原地改字挪位置
  return (
    <TooltipPrimitive.Root key={tip.id} open={open} onOpenChange={(o) => { if (!o) hide.current() }}>
      <TooltipContent side={tip.side} anchor={anchor} data-tip-instant={tip.instant || undefined}>{tip.text}</TooltipContent>
    </TooltipPrimitive.Root>
  )
}

/* ---------- Trunc：单行省略文本 ---------- */

/** 单行省略文本：只有真的被截断时悬停才出现完整内容的提示。不再每个单元格一个 Tooltip 根（表格里几百个）：被截断时给自己写上 data-tip，提示交给 TipHost
 *  （触屏轻触也由它弹）；被截断、又不在按钮 / 链接里时才加入 Tab 顺序（在按钮里的由按钮自己承担焦点）。直接改 DOM 属性，不走 React 状态，也就不会因为量一次尺寸重渲染一个单元格。共用 lib/sharedObserver 的 ResizeObserver。
 *  指针移上来时再量一次（盒子没变、内容变宽时 ResizeObserver 不回调）：监听在元素自己身上，先于 TipHost 挂在 document 上的 pointerover 执行，data-tip 来得及写上 */
export function Trunc({ children, text, className }: { children: React.ReactNode; text?: string; className?: string }) {
  const ref = React.useRef<HTMLSpanElement>(null)
  const full = text ?? (typeof children === "string" ? children : "")
  React.useEffect(() => {
    const el = ref.current
    if (!el) return
    const measure = () => {
      const cut = !!full && el.scrollWidth > el.clientWidth + 1
      if (cut) el.dataset.tip = full; else delete el.dataset.tip
      if (cut && !el.parentElement?.closest("a, button, [tabindex]")) el.tabIndex = 0; else el.removeAttribute("tabindex")
    }
    measure()
    const stop = watchSize(el, measure)
    el.addEventListener("pointerover", measure)
    // 清理时不摘 data-tip：文字变了（推送）时下一轮 measure 直接改写，先摘掉会让 TipHost 以为元素不再有提示、把正开着的提示收起
    return () => { stop(); el.removeEventListener("pointerover", measure) }
  }, [full])
  return <span ref={ref} className={cn("ellipsis block", className)}>{children}</span>
}

export { Tooltip, TooltipTrigger, TooltipContent, TooltipProvider }
