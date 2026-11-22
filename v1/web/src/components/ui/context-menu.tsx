"use client"

import * as React from "react"
import { ContextMenu as ContextMenuPrimitive } from "@base-ui/react/context-menu"
import { cn } from "@/lib/utils"
import { ChevronRightIcon } from "lucide-react"
import { floatGlass, floatItem, floatItemDanger, floatPositioner, floatSep, guardEscape } from "@/components/ui/float"

/* shadcn base-mira 的 ContextMenu（Base UI）。外观与下拉菜单（dropdown-menu.tsx）同一套（float.ts），同一份菜单定义可以同时渲染成两种。
   和 shadcn 原版的差别：触发区不加 select-none（NodeMenus 的触发区是整张卡片网格 / 表格，不能禁掉里面的文字选择）；离视口边留 8px；
   危险项 danger；Esc 先收起开着的提示（guardEscape）。右键位置、触屏长按（500 ms）、Shift+F10 由 Base UI 处理。 */

function ContextMenu({ onOpenChange, ...props }: ContextMenuPrimitive.Root.Props) {
  return <ContextMenuPrimitive.Root data-slot="context-menu" onOpenChange={guardEscape(onOpenChange)} {...props} />
}


function ContextMenuTrigger({
  ...props
}: ContextMenuPrimitive.Trigger.Props) {
  return (
    <ContextMenuPrimitive.Trigger
      data-slot="context-menu-trigger"
      {...props}
    />
  )
}

function ContextMenuContent({
  className,
  align = "start",
  alignOffset = 4,
  side = "right",
  sideOffset = 0,
  collisionPadding = 8,
  ...props
}: ContextMenuPrimitive.Popup.Props &
  Pick<
    ContextMenuPrimitive.Positioner.Props,
    "align" | "alignOffset" | "side" | "sideOffset" | "collisionPadding"
  >) {
  return (
    <ContextMenuPrimitive.Portal>
      <ContextMenuPrimitive.Positioner
        className={floatPositioner}
        align={align}
        alignOffset={alignOffset}
        side={side}
        sideOffset={sideOffset}
        collisionPadding={collisionPadding}
      >
        <ContextMenuPrimitive.Popup
          data-slot="context-menu-content"
          className={cn("max-h-(--available-height) min-w-(--pop-w-sm) overflow-x-hidden overflow-y-auto overscroll-contain p-1", floatGlass, className)}
          {...props}
        />
      </ContextMenuPrimitive.Positioner>
    </ContextMenuPrimitive.Portal>
  )
}



function ContextMenuItem({
  className,
  danger,
  ...props
}: ContextMenuPrimitive.Item.Props & {
  /** 危险项：文字和图标红色 */
  danger?: boolean
}) {
  return (
    <ContextMenuPrimitive.Item
      data-slot="context-menu-item"
      data-variant={danger ? "destructive" : undefined}
      className={cn(floatItem, danger && floatItemDanger, className)}
      {...props}
    />
  )
}

function ContextMenuSub({ ...props }: ContextMenuPrimitive.SubmenuRoot.Props) {
  return (
    <ContextMenuPrimitive.SubmenuRoot data-slot="context-menu-sub" {...props} />
  )
}

function ContextMenuSubTrigger({
  className,
  children,
  ...props
}: ContextMenuPrimitive.SubmenuTrigger.Props) {
  return (
    <ContextMenuPrimitive.SubmenuTrigger
      data-slot="context-menu-sub-trigger"
      className={cn(floatItem, className)}
      {...props}
    >
      {children}
      <ChevronRightIcon className="ml-auto text-fg-3" />
    </ContextMenuPrimitive.SubmenuTrigger>
  )
}

function ContextMenuSubContent({
  className,
  alignOffset = -4,
  sideOffset = 0,
  ...props
}: React.ComponentProps<typeof ContextMenuContent>) {
  return (
    <ContextMenuContent
      data-slot="context-menu-sub-content"
      className={cn("w-auto", className)}
      side="right"
      alignOffset={alignOffset}
      sideOffset={sideOffset}
      {...props}
    />
  )
}




function ContextMenuSeparator({
  className,
  ...props
}: ContextMenuPrimitive.Separator.Props) {
  return (
    <ContextMenuPrimitive.Separator
      data-slot="context-menu-separator"
      className={cn(floatSep, className)}
      {...props}
    />
  )
}


export {
  ContextMenu,
  ContextMenuTrigger,
  ContextMenuContent,
  ContextMenuItem,
  ContextMenuSeparator,
  ContextMenuSub,
  ContextMenuSubContent,
  ContextMenuSubTrigger,
}
