"use client"

import * as React from "react"

import { cn } from "@/lib/utils"

/* 标签：12px 中粗、次要色（fg-2）；禁用时随控件变淡 */
function Label({ className, ...props }: React.ComponentProps<"label">) {
  return (
    <label
      data-slot="label"
      className={cn(
        "flex items-center gap-1 text-label font-medium text-fg-2 group-data-[disabled=true]:pointer-events-none group-data-[disabled=true]:opacity-(--o-disabled) peer-disabled:cursor-default peer-disabled:opacity-(--o-disabled)",
        className
      )}
      {...props}
    />
  )
}

export { Label }
