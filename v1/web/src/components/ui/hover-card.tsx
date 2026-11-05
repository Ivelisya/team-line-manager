"use client"

import * as React from "react"
import { PreviewCard as PreviewCardPrimitive } from "@base-ui/react/preview-card"
import { cn } from "@/lib/utils"
import { floatGlass, floatPositioner, guardEscape } from "@/components/ui/float"

/* shadcn base-mira 的 HoverCard（Base UI PreviewCard），外观同 Popover（float.ts）。
   延迟在 Base UI 里是触发元素的属性，这里保持改前的写法——写在 HoverCard 根上（openDelay 默认 350 ms 出现，closeDelay 默认 120 ms 收起），经 context 交给触发元素；
   弹层默认 sideOffset 8、align start、离视口边 12px，宽 --pop-w-md。 */

const DelayContext = React.createContext<{ openDelay: number; closeDelay: number }>({ openDelay: 350, closeDelay: 120 })

function HoverCard({ openDelay = 350, closeDelay = 120, onOpenChange, ...props }: PreviewCardPrimitive.Root.Props & { openDelay?: number; closeDelay?: number }) {
  const delays = React.useMemo(() => ({ openDelay, closeDelay }), [openDelay, closeDelay])
  return (
    <DelayContext.Provider value={delays}>
      <PreviewCardPrimitive.Root data-slot="hover-card" onOpenChange={guardEscape(onOpenChange, true)} {...props} />
    </DelayContext.Provider>
  )
}

function HoverCardTrigger({ delay, closeDelay, ...props }: PreviewCardPrimitive.Trigger.Props) {
  const d = React.useContext(DelayContext)
  return (
    <PreviewCardPrimitive.Trigger data-slot="hover-card-trigger" delay={delay ?? d.openDelay} closeDelay={closeDelay ?? d.closeDelay} {...props} />
  )
}

function HoverCardContent({
  className,
  side = "bottom",
  sideOffset = 8,
  align = "start",
  alignOffset = 0,
  collisionPadding = 12,
  ...props
}: PreviewCardPrimitive.Popup.Props &
  Pick<
    PreviewCardPrimitive.Positioner.Props,
    "align" | "alignOffset" | "side" | "sideOffset" | "collisionPadding"
  >) {
  return (
    <PreviewCardPrimitive.Portal data-slot="hover-card-portal">
      <PreviewCardPrimitive.Positioner
        align={align}
        alignOffset={alignOffset}
        side={side}
        sideOffset={sideOffset}
        collisionPadding={collisionPadding}
        className={floatPositioner}
      >
        <PreviewCardPrimitive.Popup
          data-slot="hover-card-content"
          className={cn("w-(--pop-w-md) max-w-[calc(100vw-24px)] p-3", floatGlass, className)}
          {...props}
        />
      </PreviewCardPrimitive.Positioner>
    </PreviewCardPrimitive.Portal>
  )
}

export { HoverCard, HoverCardTrigger, HoverCardContent }
