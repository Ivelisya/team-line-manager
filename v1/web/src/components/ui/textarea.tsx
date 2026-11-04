import * as React from "react"

import { cn } from "@/lib/utils"

/* 不用 Mira 的 field-sizing-content（会随内容自动撑高）：行数由调用处的 rows / 高度管，仍可纵向拖拽 */
function Textarea({ className, ...props }: React.ComponentProps<"textarea">) {
  return (
    <textarea
      data-slot="textarea"
      className={cn(
        "flex min-h-18 w-full resize-y rounded-control border border-input bg-surface px-2.5 py-2 text-body text-foreground [transition:border-color_var(--dur-1)_var(--ease-std),box-shadow_var(--dur-1)_var(--ease-std)] outline-none placeholder:text-fg-3 focus-visible:border-brand focus-visible:ring-[3px] focus-visible:ring-(--focus-halo) disabled:cursor-default disabled:bg-sunken disabled:text-fg-3 aria-invalid:border-bad",
        className
      )}
      {...props}
    />
  )
}

export { Textarea }
