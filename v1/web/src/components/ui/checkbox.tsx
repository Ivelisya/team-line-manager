"use client"

import { Checkbox as CheckboxPrimitive } from "@base-ui/react/checkbox"
import { CheckIcon, MinusIcon } from "lucide-react"

import { cn } from "@/lib/utils"

/* 复选框 16px（--r-xs 圆角、勾 12px 粗线）。半选用 indeterminate（减号）；onCheckedChange 拿到布尔值。
   根节点是 span（不是原生控件）：禁用态认 data-disabled，不是 :disabled；焦点环用全站的 :focus-visible（base.css），不写 outline-none（工具类会压掉它）。
   触屏上命中范围比外观大（overrides.css 按 data-slot 用 ::after 外扩） */
function Checkbox({ className, ...props }: CheckboxPrimitive.Root.Props) {
  return (
    <CheckboxPrimitive.Root
      data-slot="checkbox"
      className={cn(
        "peer group/checkbox relative flex size-4 shrink-0 cursor-pointer items-center justify-center rounded-xs border border-input bg-surface text-on-accent [transition:background-color_var(--dur-1)_var(--ease-std),border-color_var(--dur-1)_var(--ease-std)] data-disabled:cursor-default data-disabled:opacity-(--o-disabled) data-disabled:saturate-40 aria-invalid:border-bad data-checked:border-primary data-checked:bg-primary data-indeterminate:border-primary data-indeterminate:bg-primary",
        className
      )}
      {...props}
    >
      <CheckboxPrimitive.Indicator
        data-slot="checkbox-indicator"
        className="grid place-content-center text-current transition-none [&>svg]:size-(--icon-2xs) [&>svg]:stroke-[var(--icon-stroke-strong)]"
      >
        <CheckIcon className="group-data-indeterminate/checkbox:hidden" />
        <MinusIcon className="hidden group-data-indeterminate/checkbox:block" />
      </CheckboxPrimitive.Indicator>
    </CheckboxPrimitive.Root>
  )
}

export { Checkbox }
