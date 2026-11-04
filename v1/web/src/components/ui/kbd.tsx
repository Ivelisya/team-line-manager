import { cn } from "@/lib/utils"

/* 键帽：18px 高的小方块（--h-chip-sm），浅底细边框，等宽数字字体 */
function Kbd({ className, ...props }: React.ComponentProps<"kbd">) {
  return (
    <kbd
      data-slot="kbd"
      className={cn(
        "pointer-events-none inline-grid h-(--h-chip-sm) min-w-(--h-chip-sm) place-items-center rounded-xs border border-line bg-sunken px-1 font-num text-micro text-fg-2 select-none",
        className
      )}
      {...props}
    />
  )
}

function KbdGroup({ className, ...props }: React.ComponentProps<"div">) {
  return <kbd data-slot="kbd-group" className={cn("inline-flex items-center gap-1", className)} {...props} />
}

export { Kbd, KbdGroup }
