import { cn } from "@/lib/utils"

/* 骨架占位：挂载后 150 ms 才显示（占位尺寸一直在，布局不跳）：局域网里数据几十毫秒就回来，快的直接出现，不先闪一下灰块；
   底色同格条未点亮色（--cell-off）：暗色下 --sunken 比卡片还暗，骨架几乎看不见；减弱动效下不播脉动，照旧直接显示。
   keyframes skel-show / skel-pulse 在 base.css。宽高用 className 写（w-16 h-5），占一整块图表 / 面板位置的加 rounded-float。
   纯装饰，对读屏隐藏（「加载中」由外层的 role=status 说） */
function Skeleton({ className, ...props }: React.ComponentProps<"div">) {
  return (
    <div
      aria-hidden
      data-slot="skeleton"
      className={cn(
        "block rounded-xs bg-(--cell-off) [animation:skel-show_0s_linear_150ms_backwards,skel-pulse_var(--dur-loop)_var(--ease-std)_150ms_infinite] motion-reduce:animate-none",
        className
      )}
      {...props}
    />
  )
}

export { Skeleton }
