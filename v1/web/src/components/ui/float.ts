import * as React from "react"
import { hintOpen } from "@/components/ui/tooltip"

/* 锚定弹层（下拉 / 右键 / 选择 / 弹层 / 日历 / 悬浮卡）共用的外观类名，各 ui 文件按需取用。
   不是组件：registry 里的 Tailwind 类名改成令牌写法后，五种浮层都要同一套材质和进出场，写五份会漂移。

   材质：毛玻璃（--glass + --glass-blur）、--shadow-pop、细边（--line-float）、--r-float 圆角；下拉选择是实底（常开在对话框里、伸出对话框下沿，玻璃会透出上下两截颜色）。
   进出场（Base UI 的 data-starting-style / data-ending-style + CSS 过渡，不是 keyframes）：进场 --dur-2 / --ease-out，淡入 + 从 .96 放大，同时朝远离触发元素的方向走 4px；
   退场 --dur-1 / --ease-std 原路收回。缩放原点是触发元素那一侧（--transform-origin）。过渡可被打断（退场中又打开，从当前位置折返）；
   减弱动效时 --dur-* 为 0，过渡即刻完成、立即卸载。 */

/** Positioner（定位层）：层级只写在这里 */
export const floatPositioner = "isolate z-(--z-popover) outline-none"

const motion =
  "origin-(--transform-origin) [--pop-x:0px] [--pop-y:0px] data-[side=top]:[--pop-y:4px] data-[side=bottom]:[--pop-y:-4px] data-[side=left]:[--pop-x:4px] data-[side=right]:[--pop-x:-4px] " +
  "[transition:opacity_var(--dur-1)_var(--ease-std),transform_var(--dur-1)_var(--ease-std)] data-open:[transition:opacity_var(--dur-2)_var(--ease-out),transform_var(--dur-2)_var(--ease-out)] " +
  "data-starting-style:opacity-0 data-starting-style:[transform:translate(var(--pop-x),var(--pop-y))_scale(.96)] data-ending-style:opacity-0 data-ending-style:[transform:translate(var(--pop-x),var(--pop-y))_scale(.96)]"

/** Popup（可见的面）：毛玻璃 */
export const floatGlass =
  "rounded-float border border-(--line-float) bg-(--glass) text-fg shadow-(--shadow-pop) outline-none [-webkit-backdrop-filter:var(--glass-blur)] [backdrop-filter:var(--glass-blur)] " + motion

/** Popup：实底（下拉选择） */
export const floatSolid = "rounded-float border border-(--line-float) bg-raised text-fg shadow-(--shadow-pop) outline-none " + motion

/** 菜单 / 下拉选择里的一行：同心圆角（外圈 --r-float 减菜单内边距 4px）、键盘 / 指针高亮 --row-highlight、按下叠 --state-press（行不缩放）、禁用三级字。
 *  写了 data-tip 的禁用项：不能点，但悬停能看到为什么（pointer-events 放开，点击由 Base UI 的 disabled 挡住）。
 *  图标统一 14px、二级字色（--item-ic），危险项 / 选中项改写这个变量 */
export const floatItem =
  "relative flex min-h-(--h-md) w-full cursor-default items-center gap-2 rounded-[calc(var(--r-float)-var(--sp-4))] px-2.5 text-body text-fg outline-hidden select-none [--item-ic:var(--fg-2)] [--hl:color-mix(in_srgb,var(--accent)_9%,transparent)] " +
  "transition-[background-color] duration-(--dur-1) ease-(--ease-std) data-highlighted:bg-(--hl) data-popup-open:bg-(--hl) active:[background-image:var(--state-press)] " +
  "data-disabled:pointer-events-none data-disabled:text-fg-3 data-disabled:data-tip:pointer-events-auto " +
  "[&_svg]:pointer-events-none [&_svg]:shrink-0 [&_svg:not([class*='size-'])]:size-(--icon-sm) [&_svg:not([class*='text-'])]:text-(--item-ic)"

/** 危险项（删除、退役、轮换凭据）：文字和图标红色 */
export const floatItemDanger = "text-bad-text [--item-ic:var(--bad-text)] [--hl:var(--bad-bg)]"

/** 菜单标签 / 分组名 */
export const floatLabel = "px-2.5 pt-1.5 pb-0.5 text-micro font-medium text-fg-2"

/** 分隔线 */
export const floatSep = "my-1 h-px bg-line"

/** 选中项右侧的勾：强调色 */
export const floatCheck = "ml-auto inline-flex text-brand"

/** 提示开着时按 Esc：这一下只用来收起提示，菜单 / 下拉 / 弹层留着（Base UI 的层不看 defaultPrevented，只能在 onOpenChange 里按 reason 拦；allowPropagation 让 Esc 继续传给提示，由它自己收起） */
export function guardEscape<D extends { reason: string; event: Event; cancel(): void; allowPropagation(): void }>(onOpenChange?: (open: boolean, details: D) => void, tooltipOnly = false) {
  return (open: boolean, details: D) => {
    if (!open && details.reason === "escape-key" && hintOpen(tooltipOnly)) { details.cancel(); details.allowPropagation(); return }
    onOpenChange?.(open, details)
  }
}

/* ---- 浮层开着时点对话框 / 抽屉的遮罩：这一下只用来收起浮层 ----
   同改前 Radix 层栈（只有最上层响应外部点击）。Base UI 里下拉选择 / 菜单 / 弹层的外部点击和对话框、抽屉的外部点击是各自独立判断的，点遮罩会两个一起关。
   在 pointerdown 的捕获阶段（各层的关闭还没执行）记下当时有没有这样的层开着；对话框 / 抽屉在 outside-press / 遮罩点击时看 floatOpenAtPress()，有就这一下不关。悬浮卡不算（非模态、不抢点击）。
   监听只在有对话框 / 抽屉打开着时才注册（useFloatPressGuard(open)，计数；关着的对话框 / 抽屉仍挂在树上，不能按挂载算） */
const FLOAT_OPEN = ["select", "dropdown-menu", "dropdown-menu-sub", "context-menu", "context-menu-sub", "popover"].map((n) => '[data-slot="' + n + '-content"][data-open]').join(", ")
let floatAtPress = false
let guards = 0
const onPress = () => { floatAtPress = !!document.querySelector(FLOAT_OPEN) }
export function useFloatPressGuard(open: boolean) {
  React.useEffect(() => {
    if (!open) return
    if (guards++ === 0) window.addEventListener("pointerdown", onPress, true)
    return () => { if (--guards === 0) { window.removeEventListener("pointerdown", onPress, true); floatAtPress = false } }
  }, [open])
}
/** 最近一次按下时，有没有下拉选择 / 菜单 / 弹层开着 */
export const floatOpenAtPress = () => floatAtPress
