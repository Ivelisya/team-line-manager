import * as React from "react"
import { cva, type VariantProps } from "class-variance-authority"

import { cn } from "@/lib/utils"
import { Input } from "@/components/ui/input"

/* 输入组：外框管边线 / 底 / 聚焦光环（和 Input 同一套），里面的 input 无边框；左右的 Addon 放图标、单位。高度 --h-lg。
   只留用得上的（UnitInput 的单位后缀、SearchInput 的放大镜）：shadcn 原版的 InputGroupButton / InputGroupTextarea、上下排的 block-* 对齐没有调用处，没留。
   不带原版的 role="group"：每个带单位 / 搜索的输入框都会在无障碍树里多出没有名字的分组 */
function InputGroup({ className, ...props }: React.ComponentProps<"div">) {
  return (
    <div
      data-slot="input-group"
      className={cn(
        "group/input-group relative flex h-(--h-lg) w-full min-w-0 items-center rounded-control border border-input bg-surface [transition:border-color_var(--dur-1)_var(--ease-std),box-shadow_var(--dur-1)_var(--ease-std)] outline-none has-[[data-slot=input-group-control]:focus-visible]:border-brand has-[[data-slot=input-group-control]:focus-visible]:ring-[3px] has-[[data-slot=input-group-control]:focus-visible]:ring-(--focus-halo) has-[[data-slot][aria-invalid=true]]:border-bad has-disabled:bg-sunken has-disabled:text-fg-3 has-[>[data-align=inline-end]]:[&>input]:pr-1.5 has-[>[data-align=inline-start]]:[&>input]:pl-1.5",
        className
      )}
      {...props}
    />
  )
}

const inputGroupAddonVariants = cva(
  "flex h-auto cursor-text items-center justify-center gap-1 py-2 text-body font-normal text-fg-3 select-none group-data-[disabled=true]/input-group:opacity-(--o-disabled) [&>svg:not([class*='size-'])]:size-(--icon-sm)",
  {
    variants: {
      align: {
        "inline-start": "order-first pl-2.5",
        "inline-end": "order-last pr-2.5",
      },
    },
    defaultVariants: {
      align: "inline-start",
    },
  }
)

/** 点在图标 / 单位上也聚焦到输入框 */
function InputGroupAddon({
  className,
  align = "inline-start",
  ...props
}: React.ComponentProps<"div"> & VariantProps<typeof inputGroupAddonVariants>) {
  return (
    <div
      data-slot="input-group-addon"
      data-align={align}
      className={cn(inputGroupAddonVariants({ align }), className)}
      onClick={(e) => {
        if ((e.target as HTMLElement).closest("button")) {
          return
        }
        e.currentTarget.parentElement?.querySelector("input")?.focus()
      }}
      {...props}
    />
  )
}

function InputGroupText({ className, ...props }: React.ComponentProps<"span">) {
  return (
    <span
      className={cn(
        "flex items-center gap-2 text-body text-fg-2 [&_svg]:pointer-events-none [&_svg:not([class*='size-'])]:size-(--icon-sm)",
        className
      )}
      {...props}
    />
  )
}

function InputGroupInput({
  className,
  ...props
}: React.ComponentProps<"input">) {
  return (
    <Input
      data-slot="input-group-control"
      className={cn(
        "h-full flex-1 rounded-none border-0 bg-transparent shadow-none ring-0 focus-visible:ring-0 aria-invalid:ring-0 disabled:bg-transparent",
        className
      )}
      {...props}
    />
  )
}

export {
  InputGroup,
  InputGroupAddon,
  InputGroupText,
  InputGroupInput,
}
