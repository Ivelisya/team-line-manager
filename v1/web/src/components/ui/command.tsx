import * as React from "react"
import { Command as CommandPrimitive } from "cmdk"
import { Dialog as DialogPrimitive } from "@base-ui/react/dialog"
import { SearchIcon } from "lucide-react"

import { cn } from "@/lib/utils"
import { Button } from "@/components/ui/button"
import { Dialog, DialogOverlay, DialogPortal, DialogTitle, RememberFocus, useRestoreFocus } from "@/components/ui/dialog"

/* shadcn 的 Command 写法：cmdk 提供过滤 / 键盘导航，外观走 palette.css 的 .cmd-* 类（令牌来自 tokens.css；
   国家 / 时区 / 宿主机等可搜索下拉也用同一套列表样式，所以没有换成 shadcn 原版的工具类）。CommandDialog 是 Base UI 的 Dialog（shadcn 的写法），
   遮罩与回焦同 dialog.tsx。 */
function Command({ className, ...props }: React.ComponentProps<typeof CommandPrimitive>) {
  return <CommandPrimitive data-slot="command" className={cn("cmd", className)} {...props} />
}

/** 命令面板对话框：遮罩 + 顶部居中的面板（宽 --palette-w），层级 --z-palette */
function CommandDialog({ open, onOpenChange, title, children }: { open: boolean; onOpenChange: (o: boolean) => void; title: string; children: React.ReactNode }) {
  const panel = React.useRef<HTMLDivElement>(null)
  const focus = useRestoreFocus(panel)
  return (
    <Dialog open={open} onOpenChange={(o) => onOpenChange(o)}>
      <DialogPortal>
        <DialogOverlay data-palette className="z-(--z-palette-scrim)" />
        <RememberFocus focus={focus} />
        <DialogPrimitive.Popup
          ref={panel}
          data-slot="command-dialog"
          finalFocus={focus.finalFocus}
          className="fixed top-[min(14vh,120px)] left-1/2 z-(--z-palette) flex w-[min(var(--palette-w),calc(100vw-24px))] origin-top flex-col overflow-hidden rounded-modal border border-(--line-float) bg-raised shadow-(--shadow-modal) [translate:-50%_0]"
        >
          <DialogTitle className="sr-only">{title}</DialogTitle>
          {children}
          {/* 触屏没有 Esc：输入框右侧给一个「取消」（只在 pointer: coarse 时显示） */}
          <DialogPrimitive.Close
            render={<Button variant="ghost" size="sm" className="hidden text-brand pointer-coarse:absolute pointer-coarse:top-[calc((52px-var(--h-sm))/2)] pointer-coarse:right-2.5 pointer-coarse:inline-flex" />}
          >
            取消
          </DialogPrimitive.Close>
        </DialogPrimitive.Popup>
      </DialogPortal>
    </Dialog>
  )
}

function CommandInput({ className, ...props }: React.ComponentProps<typeof CommandPrimitive.Input>) {
  return (
    <div data-slot="command-input-wrapper" className="cmd-input-wrap">
      <SearchIcon size={16} />
      <CommandPrimitive.Input data-slot="command-input" className={cn("cmd-input", className)} {...props} />
    </div>
  )
}

function CommandList({ className, ...props }: React.ComponentProps<typeof CommandPrimitive.List>) {
  return <CommandPrimitive.List data-slot="command-list" className={cn("cmd-list", className)} {...props} />
}

function CommandEmpty({ className, ...props }: React.ComponentProps<typeof CommandPrimitive.Empty>) {
  return <CommandPrimitive.Empty data-slot="command-empty" className={cn("cmd-empty", className)} {...props} />
}

function CommandGroup({ className, ...props }: React.ComponentProps<typeof CommandPrimitive.Group>) {
  return <CommandPrimitive.Group data-slot="command-group" className={cn("cmd-group", className)} {...props} />
}

function CommandItem({ className, ...props }: React.ComponentProps<typeof CommandPrimitive.Item>) {
  return <CommandPrimitive.Item data-slot="command-item" className={cn("cmd-item", className)} {...props} />
}

export { Command, CommandDialog, CommandEmpty, CommandGroup, CommandInput, CommandItem, CommandList }
