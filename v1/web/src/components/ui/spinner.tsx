import { cn } from "@/lib/utils"
import { Loader2Icon } from "lucide-react"

/** 转圈。一圈 --dur-spin（减弱动效时 overrides.css 放慢到 2s、不停）；全站的转圈都是挨着文字的装饰，所以默认 aria-hidden（shadcn 原版带 role=status，会在按钮名称里多出一句 Loading） */
function Spinner({ className, ...props }: React.ComponentProps<"svg">) {
  return (
    <Loader2Icon
      data-slot="spinner"
      aria-hidden
      className={cn("size-(--icon-sm) shrink-0 [animation:spin_var(--dur-spin)_linear_infinite]", className)}
      {...props}
    />
  )
}

export { Spinner }
