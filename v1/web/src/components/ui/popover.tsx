import * as React from "react"
import { Popover as PopoverPrimitive } from "@base-ui/react/popover"
import { cn } from "@/lib/utils"
import { floatGlass, floatPositioner, floatSolid, guardEscape } from "@/components/ui/float"

/* shadcn base-mira 的 Popover（Base UI），外观走令牌（毛玻璃 + --shadow-pop，类名在 float.ts）。
   和 shadcn 原版的差别：默认宽 --pop-w-md（最宽不超过视口减 24px）、内边距 12px，sideOffset 6，离视口边 8px；Esc 先收起开着的提示（guardEscape）。
   页面里的弹层变体（时区 / 静默对象选择 cs-pop、作业 / 告警列表 jobs-pop / alerts-pop）靠 className 的钩子类，样式在 palette.css / base.css。 */

function Popover({ onOpenChange, ...props }: PopoverPrimitive.Root.Props) {
  return <PopoverPrimitive.Root data-slot="popover" onOpenChange={guardEscape(onOpenChange)} {...props} />
}

function PopoverTrigger({ ...props }: PopoverPrimitive.Trigger.Props) {
  return <PopoverPrimitive.Trigger data-slot="popover-trigger" {...props} />
}

function PopoverContent({
  className,
  align = "center",
  alignOffset = 0,
  side = "bottom",
  sideOffset = 6,
  collisionPadding = 8,
  solid,
  ...props
}: PopoverPrimitive.Popup.Props &
  Pick<
    PopoverPrimitive.Positioner.Props,
    "align" | "alignOffset" | "side" | "sideOffset" | "collisionPadding"
  > & {
    /** 实底（不用毛玻璃）：常开在对话框里的可搜索下拉（国家、时区…），玻璃会透出底下的表单字 */
    solid?: boolean
  }) {
  return (
    <PopoverPrimitive.Portal>
      <PopoverPrimitive.Positioner
        align={align}
        alignOffset={alignOffset}
        side={side}
        sideOffset={sideOffset}
        collisionPadding={collisionPadding}
        className={floatPositioner}
      >
        <PopoverPrimitive.Popup
          data-slot="popover-content"
          className={cn("w-(--pop-w-md) max-w-[calc(100vw-24px)] p-3", solid ? floatSolid : floatGlass, className)}
          {...props}
        />
      </PopoverPrimitive.Positioner>
    </PopoverPrimitive.Portal>
  )
}

function PopoverHeader({ className, ...props }: React.ComponentProps<"div">) {
  return (
    <div
      data-slot="popover-header"
      className={cn("flex flex-col gap-1 text-label", className)}
      {...props}
    />
  )
}

function PopoverTitle({ className, ...props }: PopoverPrimitive.Title.Props) {
  return (
    <PopoverPrimitive.Title
      data-slot="popover-title"
      className={cn("text-body font-medium", className)}
      {...props}
    />
  )
}

function PopoverDescription({
  className,
  ...props
}: PopoverPrimitive.Description.Props) {
  return (
    <PopoverPrimitive.Description
      data-slot="popover-description"
      className={cn("text-fg-2", className)}
      {...props}
    />
  )
}

export {
  Popover,
  PopoverContent,
  PopoverDescription,
  PopoverHeader,
  PopoverTitle,
  PopoverTrigger,
}
