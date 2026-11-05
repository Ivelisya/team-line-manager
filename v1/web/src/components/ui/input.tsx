import * as React from "react"
import { Input as InputPrimitive } from "@base-ui/react/input"

import { cn } from "@/lib/utils"

/** 登录用的输入：密码框，或 autoComplete 写明是用户名 / 密码 / 验证码的。其余文本框（团队名、套餐名、搜索…）不让密码管理器（1Password / LastPass）挂图标 */
const CREDENTIAL = new Set(["username", "current-password", "new-password", "one-time-code"])

/* 高度取 --h-lg（和同行的下拉、按钮、分段一样高），sm 取 --h-sm；字号 13px（sm 12px）；占位符 --fg-3；聚焦 = 强调色描边 + 3px 光环 */
function Input({ className, type, sm, ...props }: React.ComponentProps<"input"> & { sm?: boolean }) {
  const secret = type === "password" || CREDENTIAL.has(props.autoComplete ?? "")
  return (
    <InputPrimitive
      type={type}
      data-slot="input"
      className={cn(
        "w-full min-w-0 rounded-control border border-input bg-surface text-foreground [transition:border-color_var(--dur-1)_var(--ease-std),box-shadow_var(--dur-1)_var(--ease-std)] outline-none file:inline-flex file:border-0 file:bg-transparent file:text-label file:font-medium file:text-foreground placeholder:text-fg-3 focus-visible:border-brand focus-visible:ring-[3px] focus-visible:ring-(--focus-halo) disabled:cursor-default disabled:bg-sunken disabled:text-fg-3 aria-invalid:border-bad",
        sm ? "h-(--h-sm) px-2 text-label" : "h-(--h-lg) px-2.5 text-body",
        className
      )}
      {...(secret ? null : { "data-1p-ignore": "", "data-lpignore": "true" })}
      {...props}
    />
  )
}

export { Input }
