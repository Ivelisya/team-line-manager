"use client"

import { Switch as SwitchPrimitive } from "@base-ui/react/switch"

import { cn } from "@/lib/utils"

/* iOS 式开关 34×20：圆钮 16px 移动到位用 --dur-2 / --ease-out；按住时圆钮拉长到 20px（选中态同时少移 4px，右缘不动）。
   焦点环用全站的 :focus-visible（base.css），不写 outline-none（工具类会压掉它） */
function Switch({ className, ...props }: SwitchPrimitive.Root.Props) {
  return (
    <SwitchPrimitive.Root
      data-slot="switch"
      className={cn(
        "peer group/switch relative inline-flex h-5 w-[34px] shrink-0 cursor-pointer items-center rounded-pill border-0 bg-line-strong p-0.5 [transition:background-color_var(--dur-1)_var(--ease-std)] data-checked:bg-primary data-disabled:cursor-default data-disabled:opacity-(--o-disabled) data-disabled:saturate-40",
        className
      )}
      {...props}
    >
      <SwitchPrimitive.Thumb
        data-slot="switch-thumb"
        // 滑动过渡的是 translate（Tailwind 4 的 translate-x-* 写独立的 translate 属性，不是 transform）；按住才拉长，禁用的不响应（根节点是 span，:disabled 不匹配，认 data-disabled）
        className="pointer-events-none block size-4 rounded-pill bg-(--on-solid) shadow-(--shadow-btn) ring-0 [transition:translate_var(--dur-2)_var(--ease-out),width_var(--dur-2)_var(--ease-out)] data-checked:translate-x-3.5 group-[:active:not([data-disabled])]/switch:w-5 group-[:active:not([data-disabled])]/switch:[transition-duration:var(--dur-press)] group-[:active:not([data-disabled])]/switch:data-checked:translate-x-2.5"
      />
    </SwitchPrimitive.Root>
  )
}

export { Switch }
