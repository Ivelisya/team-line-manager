"use client"

import { Progress as ProgressPrimitive } from "@base-ui/react/progress"

import { cn } from "@/lib/utils"

/* 进度条：0–100（Base UI Progress：role=progressbar 与 aria-valuenow / min / max 由它给）。tone 对应状态色（ok / warn / bad，默认强调色）；
   live 时填充条上有缓慢流动的细纹，表示还在推进（细纹与 keyframes 在 components.css）。沿用额度条的轨道语言：高 --quota-h、药丸形、轨道色 --line。
   填充条整宽、按进度往左平移（只动 transform 不触发布局），左端被轨道裁掉；不用 Base UI 默认的 width（要过渡 width 会触发布局），也不用 scaleX（右端圆角和细纹会被压扁） */
function Progress({
  className,
  children,
  value,
  tone,
  live,
  ...props
}: ProgressPrimitive.Root.Props & { tone?: "ok" | "warn" | "bad"; live?: boolean }) {
  const v = Math.max(0, Math.min(100, Number.isFinite(value) ? (value as number) : 0))
  return (
    <ProgressPrimitive.Root
      value={v}
      data-slot="progress"
      data-tone={tone}
      data-live={live ? "" : undefined}
      className={cn("group/progress flex flex-wrap gap-3", className)}
      {...props}
    >
      {children}
      <ProgressTrack>
        <ProgressIndicator style={{ width: "100%", transform: `translateX(${v - 100}%)` }} />
      </ProgressTrack>
    </ProgressPrimitive.Root>
  )
}

function ProgressTrack({ className, ...props }: ProgressPrimitive.Track.Props) {
  return (
    <ProgressPrimitive.Track
      className={cn("relative h-(--quota-h) w-full overflow-hidden rounded-pill bg-line", className)}
      data-slot="progress-track"
      {...props}
    />
  )
}

function ProgressIndicator({
  className,
  ...props
}: ProgressPrimitive.Indicator.Props) {
  return (
    <ProgressPrimitive.Indicator
      data-slot="progress-indicator"
      className={cn(
        "absolute inset-y-0 left-0 overflow-hidden rounded-pill bg-brand transition-[transform,background-color] duration-(--dur-data) ease-(--ease-out)",
        // 只认本进度条根节点上的 data-tone（group/progress），不被外层带 data-tone 的提示条、卡片染色
        "group-data-[tone=ok]/progress:bg-ok group-data-[tone=warn]/progress:bg-warn group-data-[tone=bad]/progress:bg-bad",
        className
      )}
      {...props}
    />
  )
}

function ProgressLabel({ className, ...props }: ProgressPrimitive.Label.Props) {
  return (
    <ProgressPrimitive.Label
      className={cn("text-label font-medium", className)}
      data-slot="progress-label"
      {...props}
    />
  )
}

function ProgressValue({ className, ...props }: ProgressPrimitive.Value.Props) {
  return (
    <ProgressPrimitive.Value
      className={cn("ml-auto text-label text-fg-2 tabular-nums", className)}
      data-slot="progress-value"
      {...props}
    />
  )
}

export {
  Progress,
  ProgressTrack,
  ProgressIndicator,
  ProgressLabel,
  ProgressValue,
}
