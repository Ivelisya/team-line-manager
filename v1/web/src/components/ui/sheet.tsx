import * as React from "react"
import { Dialog as SheetPrimitive } from "@base-ui/react/dialog"
import { X } from "lucide-react"

import { cn } from "@/lib/utils"
import { IconButton } from "@/components/ui/button"
import { RememberFocus, dialogOpen, escapeIsTaken, mergeRefs, useRestoreFocus } from "@/components/ui/dialog"

/* 右侧抽屉（shadcn Sheet 的写法：Dialog + 右侧定位）。非模态（modal={false}）：模态时 Base UI 会锁 body 滚动、给页面加 inert / pointer-events 的遮挡，
   一开一关整页上万个元素重算样式（改前 Radix 同理，实测紧凑卡片开约 330 ms、关约 250 ms）；非模态时一帧都不卡。
   - 遮罩自己画（SheetOverlay：只压暗不模糊），点它关抽屉：用 disablePointerDismissal 关掉 Base UI 的「点外面就关」（非模态下任何外部点击都算，
     包括点通知、点还没换成 Base UI 的菜单 / 下拉里的项），改成只认点在遮罩自己身上的主键点击；同时它关掉「焦点移出就关」。右键、ctrl+点击不关（mac 上是右键）。
   - Esc、关闭按钮照旧；Esc 在提示开着、或抽屉上还有对话框 / 命令面板开着（不在抽屉的 React 子树里渲染的确认框）时不关抽屉，先收提示 / 先关上面那层。
   - 打开时把焦点放到抽屉面板上（不滚动），键盘 Tab 从抽屉里开始；关闭后焦点回到打开它的表格行 / 卡片。
   - 开着时 <html data-drawer> 计数：base.css 据此不让背后的页面滚（页面滚动条一起藏掉；scrollbar-gutter: stable 在，内容不横移，空出的槽位同步压暗）。
   代价：焦点不再困在抽屉里，滚轮能滚到后面的页面（被 data-drawer 挡住）。抽屉里再开的确认框等仍是模态 */
const SheetContext = React.createContext<{ panel: React.RefObject<HTMLDivElement | null>; open: boolean; close: () => void } | null>(null)
const useSheetContext = () => {
  const c = React.useContext(SheetContext)
  if (!c) throw new Error("SheetContent 要放在 Sheet 里面")
  return c
}

let drawers = 0

function Sheet({ open, onOpenChange, children }: { open: boolean; onOpenChange: (open: boolean) => void; children: React.ReactNode }) {
  const panel = React.useRef<HTMLDivElement>(null)
  const handle = (next: boolean, details: SheetPrimitive.Root.ChangeEventDetails) => {
    if (!next && details.reason === "escape-key" && (escapeIsTaken(details.event) || dialogOpen({ exiting: false }))) { details.cancel(); details.allowPropagation(); return }
    onOpenChange(next)
  }
  return (
    <SheetContext.Provider value={{ panel, open, close: () => onOpenChange(false) }}>
      <SheetPrimitive.Root open={open} onOpenChange={handle} modal={false} disablePointerDismissal>{children}</SheetPrimitive.Root>
    </SheetContext.Provider>
  )
}

function SheetOverlay({ className, onClick, ...props }: SheetPrimitive.Backdrop.Props) {
  const { close } = useSheetContext()
  return (
    <SheetPrimitive.Backdrop
      forceRender
      data-slot="sheet-overlay"
      className={className}
      onClick={(e) => {
        onClick?.(e)
        if (e.target === e.currentTarget && e.button === 0 && !e.ctrlKey) close()
      }}
      {...props}
    />
  )
}

function SheetContent({
  className,
  children,
  showCloseButton = true,
  initialFocus,
  finalFocus,
  ref,
  ...props
}: SheetPrimitive.Popup.Props & {
  /** 右上角关闭键（贴着右缘，宽度不受页面滚动条影响） */
  showCloseButton?: boolean
}) {
  const { open, panel } = useSheetContext()
  const focus = useRestoreFocus(panel)
  const merged = React.useMemo(() => mergeRefs(panel, ref), [panel, ref])
  React.useLayoutEffect(() => {
    if (!open) return
    if (drawers++ === 0) document.documentElement.dataset.drawer = ""
    return () => { if (--drawers === 0) delete document.documentElement.dataset.drawer }
  }, [open])
  return (
    <SheetPrimitive.Portal data-slot="sheet-portal">
      <SheetOverlay />
      <RememberFocus focus={focus} />
      <SheetPrimitive.Popup
        ref={merged}
        data-slot="sheet-content"
        initialFocus={initialFocus ?? (() => panel.current)}
        finalFocus={finalFocus ?? focus.finalFocus}
        className={cn(
          // 阴影放在 ::before 上、只动 opacity（components.css）：位移只动 transform，两者都能交给合成器，滑入这段正是抽屉挂载、下载懒块的时候。
          "fixed top-2 right-2 bottom-2 z-(--z-drawer) flex w-(--drawer-w) max-w-[calc(100vw-16px)] flex-col overflow-hidden rounded-section border border-(--line-float) bg-surface before:pointer-events-none before:absolute before:inset-0 before:-z-1 before:rounded-[inherit] before:shadow-(--shadow-modal) before:content-[''] max-[601px]:inset-0 max-[601px]:max-w-screen max-[601px]:rounded-none max-[601px]:border-0",
          className
        )}
        {...props}
      >
        {children}
        {showCloseButton && (
          <SheetPrimitive.Close data-slot="sheet-close" render={(p) => <IconButton {...(p as object)} label="关闭" className="absolute top-3 right-3" />}>
            <X size={16} />
          </SheetPrimitive.Close>
        )}
      </SheetPrimitive.Popup>
    </SheetPrimitive.Portal>
  )
}

function SheetTitle({ className, ...props }: SheetPrimitive.Title.Props) {
  return <SheetPrimitive.Title data-slot="sheet-title" className={cn("m-0 min-w-0 text-title font-semibold tracking-(--tracking-title)", className)} {...props} />
}

function SheetDescription({ className, ...props }: SheetPrimitive.Description.Props) {
  return <SheetPrimitive.Description data-slot="sheet-description" className={cn("m-0 text-body text-fg-2 text-pretty", className)} {...props} />
}

export { Sheet, SheetContent, SheetDescription, SheetTitle }
