import * as React from "react"
import { Button as ButtonPrimitive } from "@base-ui/react/button"
import { cva, type VariantProps } from "class-variance-authority"

import { cn } from "@/lib/utils"
import { Spinner } from "@/components/ui/spinner"
import { Tooltip, TooltipContent, TooltipTrigger } from "@/components/ui/tooltip"

/* shadcn base-mira 的 Button（Base UI），变体 / 尺寸用 shadcn 原名；颜色、圆角、字号、时长一律走令牌（theme.css 的映射），
   尺寸高度取 --h-xs / --h-sm / --h-md / --h-lg（触屏由 overrides.css 上调令牌）。
   焦点环是全站统一的 2px 轮廓（base.css :focus-visible），所以这里不写 outline-none / focus ring。 */
const buttonVariants = cva(
  [
    "group/button relative inline-flex shrink-0 cursor-pointer items-center justify-center rounded-control border border-transparent bg-clip-padding font-medium whitespace-nowrap no-underline select-none",
    // 悬停 / 颜色用 --dur-1 + 标准曲线；按下缩放（scale 独立属性，不碰 transform）用 --dur-2 + 出场曲线弹回，按下时换成 --dur-press 跟手
    "[transition:background-color_var(--dur-1)_var(--ease-std),color_var(--dur-1)_var(--ease-std),border-color_var(--dur-1)_var(--ease-std),scale_var(--dur-2)_var(--ease-out)]",
    "active:not-disabled:scale-(--press-md) active:not-disabled:[transition-duration:var(--dur-press)]",
    // 禁用：半透明 + 去饱和；加载中（aria-busy）不显示成禁用的样子，转圈替换原图标（按钮宽度不变）
    "disabled:cursor-default disabled:opacity-(--o-disabled) disabled:saturate-40 aria-disabled:opacity-(--o-disabled) aria-disabled:saturate-40",
    "aria-busy:disabled:cursor-progress aria-busy:disabled:opacity-100 aria-busy:disabled:saturate-100 aria-busy:[&>svg:not([data-slot=spinner])]:hidden",
    "[&_svg]:pointer-events-none [&_svg]:shrink-0",
  ],
  {
    variants: {
      variant: {
        // 主按钮；禁用时不留实色（暗底上像被按住的暗紫色按钮），改轨道底 + 三级字
        default:
          "bg-primary text-primary-foreground shadow-(--shadow-btn) hover:not-disabled:bg-(--accent-strong) disabled:bg-track disabled:text-fg-3 disabled:shadow-none disabled:opacity-100 disabled:saturate-100 aria-disabled:bg-track aria-disabled:text-fg-3 aria-disabled:shadow-none aria-disabled:opacity-100 aria-disabled:saturate-100 aria-busy:disabled:bg-primary aria-busy:disabled:text-primary-foreground aria-busy:disabled:shadow-(--shadow-btn)",
        // 次按钮（描边）；展开着的下拉触发按钮同悬停底
        outline:
          "border-input bg-surface text-foreground hover:not-disabled:not-aria-disabled:bg-(image:--state-press) aria-expanded:bg-(image:--state-press)",
        secondary:
          "bg-secondary text-secondary-foreground hover:not-disabled:bg-[color-mix(in_srgb,var(--secondary),var(--foreground)_5%)] aria-expanded:bg-secondary",
        ghost:
          "text-fg-2 hover:not-disabled:bg-accent hover:not-disabled:text-foreground aria-expanded:bg-accent aria-expanded:text-foreground aria-pressed:bg-accent aria-pressed:text-foreground",
        // 危险操作两档（名字同 coss ui）：destructive = 实心红（对话框里的危险确认：清理并回收、删除…），禁用时同主按钮改轨道底 + 三级字；
        // destructive-outline = 红字红描边（入口按钮：「退役…」「删除…」，点开还有确认）
        destructive:
          "bg-destructive text-(--on-solid) hover:not-disabled:bg-(--bad-solid-strong) disabled:bg-track disabled:text-fg-3 disabled:opacity-100 disabled:saturate-100 aria-disabled:bg-track aria-disabled:text-fg-3 aria-disabled:opacity-100 aria-disabled:saturate-100 aria-busy:disabled:bg-destructive aria-busy:disabled:text-(--on-solid)",
        "destructive-outline":
          "border-[color-mix(in_srgb,var(--bad)_35%,var(--line))] bg-surface text-bad-text hover:not-disabled:border-[color-mix(in_srgb,var(--bad)_50%,var(--line))] hover:not-disabled:bg-bad-bg",
        link: "text-brand underline-offset-3 hover:underline",
      },
      size: {
        default: "h-(--h-md) gap-1.5 px-2.5 text-body [&_svg:not([class*='size-'])]:size-(--icon-sm)",
        xs: "h-(--h-xs) gap-1 rounded-tag px-1.5 text-label [&_svg:not([class*='size-'])]:size-(--icon-2xs)",
        sm: "h-(--h-sm) gap-1 px-2 text-label [&_svg:not([class*='size-'])]:size-(--icon-sm)",
        lg: "h-(--h-lg) gap-1.5 px-3 text-body [&_svg:not([class*='size-'])]:size-(--icon-sm)",
        // 行内文字链接（正文里的「查看作业」「展开」）：不占固定高度、没有内边距
        inline: "h-auto gap-1 rounded-xs p-0 text-body [&_svg:not([class*='size-'])]:size-(--icon-sm)",
        "inline-sm": "h-auto gap-1 rounded-xs p-0 text-label [&_svg:not([class*='size-'])]:size-(--icon-xs)",
        icon: "size-(--h-md) active:not-disabled:scale-(--press-sm)",
        "icon-xs": "size-(--h-xs) rounded-tag active:not-disabled:scale-(--press-sm)",
        "icon-sm": "size-(--h-sm) active:not-disabled:scale-(--press-sm)",
        "icon-lg": "size-(--h-lg) active:not-disabled:scale-(--press-sm)",
      },
    },
    // 页面工具条与对话框底部：默认尺寸的按钮和同行的输入、下拉、分段控件一样高（--h-lg）；手机上的对话框底部面板按钮条 44px 等分。
    // 对话框底部的类名随 B2a 换成 shadcn 的 data-slot 后，两个选择器都认
    compoundVariants: [
      {
        size: "default",
        className:
          "in-[.page-tools,.dialog-foot,[data-slot=dialog-footer]]:h-(--h-lg) in-[.page-tools,.dialog-foot,[data-slot=dialog-footer]]:px-3.5 max-[600px]:in-[.dialog-foot,[data-slot=dialog-footer]]:h-11 max-[600px]:in-[.dialog-foot,[data-slot=dialog-footer]]:flex-1",
      },
    ],
    defaultVariants: {
      variant: "default",
      size: "default",
    },
  }
)

type ButtonProps = ButtonPrimitive.Props &
  VariantProps<typeof buttonVariants> & {
    /** 进行中：转圈替换原图标、按钮宽度不变、不可点（aria-busy），但不显示成禁用的样子 */
    loading?: boolean
  }

/** render（Link、<a>）：自动按非原生按钮处理（nativeButton=false），role 保持 link */
function Button({
  className,
  variant = "default",
  size = "default",
  loading,
  disabled,
  children,
  render,
  nativeButton,
  ...props
}: ButtonProps) {
  return (
    <ButtonPrimitive
      data-slot="button"
      data-variant={variant}
      data-size={size}
      className={cn(buttonVariants({ variant, size, className }))}
      disabled={disabled || loading}
      aria-busy={loading || undefined}
      render={render}
      nativeButton={nativeButton ?? (render ? false : undefined)}
      // Base UI 给非原生按钮补 role="button"；render 的都是链接（<a> / Link），保持链接的语义
      role={render ? "link" : undefined}
      {...props}
    >
      {loading && <Spinner />}
      {children}
    </ButtonPrimitive>
  )
}

/** 图标按钮：label 同时是无障碍名称和悬停提示（统一的 Tooltip，不用浏览器自带的 title）；tip=false 关提示，side 指定方向 */
function IconButton({
  label,
  tip = true,
  side,
  variant = "ghost",
  size = "icon",
  ...props
}: Omit<ButtonProps, "size" | "aria-label"> & {
  label: string
  size?: "icon-xs" | "icon-sm" | "icon" | "icon-lg"
  tip?: boolean
  side?: React.ComponentProps<typeof TooltipContent>["side"]
}) {
  const button = <Button variant={variant} size={size} aria-label={label} {...props} />
  if (!tip) return button
  return (
    <Tooltip>
      <TooltipTrigger render={button} />
      <TooltipContent side={side}>{label}</TooltipContent>
    </Tooltip>
  )
}

export { Button, IconButton, buttonVariants }
export type { ButtonProps }
