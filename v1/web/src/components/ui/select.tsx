import * as React from "react"
import { Select as SelectPrimitive } from "@base-ui/react/select"
import { cn } from "@/lib/utils"
import { ChevronDownIcon, CheckIcon, ChevronUpIcon } from "lucide-react"
import { floatCheck, floatItem, floatLabel, floatPositioner, floatSep, floatSolid, guardEscape } from "@/components/ui/float"

/* shadcn base-mira 的 Select（Base UI），复合导出保持原名（reui 的 data-grid 会用）；页面里的「options 数组」写法见 option-select.tsx。
   和 shadcn 原版的差别（都是为了守住改前的行为）：
   - 触发器长得像输入框（高 --h-lg、1px 线边框、聚焦 / 展开时强调色边 + 3px 光环），size="sm" 是 --h-sm；类名导出为 selectTrigger，日期选择器的触发器共用；
   - 弹层不盖住触发器（alignItemWithTrigger 默认 false），宽度不小于触发器（--anchor-width），最高 300px（不超过可用高度）、里面滚动；
   - 弹层是实底（常开在对话框里、伸出对话框下沿，毛玻璃会透出上下两截颜色）；
   - 不画滚动箭头（原生滚动 + 触控板 / 滚轮已够）。 */

/** 根：Esc 先收起开着的提示（guardEscape，同菜单 / 弹层） */
function Select<Value, Multiple extends boolean | undefined = false>({ onOpenChange, ...props }: SelectPrimitive.Root.Props<Value, Multiple>) {
  return <SelectPrimitive.Root onOpenChange={guardEscape(onOpenChange)} {...props} />
}

function SelectGroup({ className, ...props }: SelectPrimitive.Group.Props) {
  return (
    <SelectPrimitive.Group
      data-slot="select-group"
      className={cn("scroll-my-1", className)}
      {...props}
    />
  )
}

function SelectValue({ className, ...props }: SelectPrimitive.Value.Props) {
  return (
    <SelectPrimitive.Value
      data-slot="select-value"
      className={cn("min-w-0 flex-1 truncate text-left", className)}
      {...props}
    />
  )
}

/** 触发器的外观（Select 的 SelectTrigger 与日期选择器的 Popover 触发器共用）。
 *  按下缩放（--press-md）、触屏热区在 components.css「交互状态」里按 [data-slot=select-trigger] 写 */
export const selectTrigger =
  "flex h-(--h-lg) w-full min-w-0 cursor-pointer items-center justify-between gap-2 rounded-control border border-line-strong bg-surface px-2.5 text-left text-body whitespace-nowrap text-fg outline-none " +
  "[transition:border-color_var(--dur-1)_var(--ease-std),box-shadow_var(--dur-1)_var(--ease-std),scale_var(--dur-2)_var(--ease-out)] " +
  "hover:border-[color-mix(in_srgb,var(--line-strong),var(--fg)_12%)] focus-visible:border-brand focus-visible:shadow-[0_0_0_3px_var(--focus-halo)] data-popup-open:border-brand data-popup-open:shadow-[0_0_0_3px_var(--focus-halo)] " +
  "disabled:cursor-default disabled:bg-sunken disabled:text-fg-3 data-placeholder:text-fg-3 " +
  "data-[size=sm]:h-(--h-sm) data-[size=sm]:px-2 data-[size=sm]:text-label " +
  "[&_svg]:pointer-events-none [&_svg]:shrink-0 [&_svg:not([class*='size-'])]:size-(--icon-sm) [&_svg]:text-fg-2"

function SelectTrigger({
  className,
  size = "default",
  children,
  ...props
}: SelectPrimitive.Trigger.Props & {
  size?: "sm" | "default"
}) {
  return (
    <SelectPrimitive.Trigger
      data-slot="select-trigger"
      data-size={size}
      className={cn(selectTrigger, className)}
      {...props}
    >
      {children}
      <SelectPrimitive.Icon
        render={
          <ChevronDownIcon className="pointer-events-none" />
        }
      />
    </SelectPrimitive.Trigger>
  )
}

function SelectContent({
  className,
  children,
  side = "bottom",
  sideOffset = 4,
  align = "start",
  alignOffset = 0,
  collisionPadding = 8,
  alignItemWithTrigger = false,
  ...props
}: SelectPrimitive.Popup.Props &
  Pick<
    SelectPrimitive.Positioner.Props,
    "align" | "alignOffset" | "side" | "sideOffset" | "collisionPadding" | "alignItemWithTrigger"
  >) {
  return (
    <SelectPrimitive.Portal>
      <SelectPrimitive.Positioner
        side={side}
        sideOffset={sideOffset}
        align={align}
        alignOffset={alignOffset}
        collisionPadding={collisionPadding}
        alignItemWithTrigger={alignItemWithTrigger}
        className={floatPositioner}
      >
        <SelectPrimitive.Popup
          data-slot="select-content"
          className={cn("max-h-[min(300px,var(--available-height))] min-w-(--anchor-width) overflow-x-hidden overflow-y-auto overscroll-contain p-1", floatSolid, className)}
          {...props}
        >
          <SelectPrimitive.List>{children}</SelectPrimitive.List>
        </SelectPrimitive.Popup>
      </SelectPrimitive.Positioner>
    </SelectPrimitive.Portal>
  )
}

function SelectLabel({
  className,
  ...props
}: SelectPrimitive.GroupLabel.Props) {
  return (
    <SelectPrimitive.GroupLabel
      data-slot="select-label"
      className={cn(floatLabel, className)}
      {...props}
    />
  )
}

function SelectItem({
  className,
  children,
  ...props
}: SelectPrimitive.Item.Props) {
  return (
    <SelectPrimitive.Item
      data-slot="select-item"
      className={cn(floatItem, "justify-between", className)}
      {...props}
    >
      <SelectPrimitive.ItemText className="min-w-0 flex-1 truncate">
        {children}
      </SelectPrimitive.ItemText>
      <SelectPrimitive.ItemIndicator
        render={<span className={floatCheck} />}
      >
        <CheckIcon className="pointer-events-none [stroke-width:var(--icon-stroke-strong)]" />
      </SelectPrimitive.ItemIndicator>
    </SelectPrimitive.Item>
  )
}

function SelectSeparator({
  className,
  ...props
}: SelectPrimitive.Separator.Props) {
  return (
    <SelectPrimitive.Separator
      data-slot="select-separator"
      className={cn("pointer-events-none", floatSep, className)}
      {...props}
    />
  )
}

function SelectScrollUpButton({
  className,
  ...props
}: React.ComponentProps<typeof SelectPrimitive.ScrollUpArrow>) {
  return (
    <SelectPrimitive.ScrollUpArrow
      data-slot="select-scroll-up-button"
      className={cn(
        "top-0 z-10 flex w-full cursor-default items-center justify-center bg-raised py-1 [&_svg:not([class*='size-'])]:size-(--icon-sm)",
        className
      )}
      {...props}
    >
      <ChevronUpIcon
      />
    </SelectPrimitive.ScrollUpArrow>
  )
}

function SelectScrollDownButton({
  className,
  ...props
}: React.ComponentProps<typeof SelectPrimitive.ScrollDownArrow>) {
  return (
    <SelectPrimitive.ScrollDownArrow
      data-slot="select-scroll-down-button"
      className={cn(
        "bottom-0 z-10 flex w-full cursor-default items-center justify-center bg-raised py-1 [&_svg:not([class*='size-'])]:size-(--icon-sm)",
        className
      )}
      {...props}
    >
      <ChevronDownIcon
      />
    </SelectPrimitive.ScrollDownArrow>
  )
}

export {
  Select,
  SelectContent,
  SelectGroup,
  SelectItem,
  SelectLabel,
  SelectScrollDownButton,
  SelectScrollUpButton,
  SelectSeparator,
  SelectTrigger,
  SelectValue,
}
