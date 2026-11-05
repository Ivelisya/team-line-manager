import { cn } from "@/lib/utils"

/* shadcn base-mira 的 Field，只留用得上的几件：Field（竖排网格：标签 / 控件 / 提示三行）、FieldTitle、FieldDescription、FieldError。
   FieldSet / FieldLegend / FieldGroup / FieldContent / FieldLabel / FieldSeparator 与横排、响应式两种排法没有调用处，没留（要用时再从 registry 装）。 */

/** 竖排用网格（标签 / 控件 / 提示三行，列宽不被撑满——放在里面的分段控件靠左、只占自己的宽度）；表单页按它对齐同一行的字段。
 *  不带 shadcn 原版的 role="group"：每个表单项都会在无障碍树里多一个没有名字的分组（改前是普通 div）。
 *  不带原版的 w-full：在网格、竖排里本来就占满一格；在可换行的横排（数据保留页的两个天数并排）里按内容收缩，同改前的 .field */
function Field({ className, ...props }: React.ComponentProps<"div">) {
  return (
    <div
      data-slot="field"
      className={cn("group/field grid min-w-0 content-start gap-1.5 data-[invalid=true]:text-bad-text", className)}
      {...props}
    />
  )
}

/** 一组控件的小标题（不是 <label>，不指向某个控件） */
function FieldTitle({ className, ...props }: React.ComponentProps<"div">) {
  return (
    <div
      data-slot="field-label"
      className={cn(
        "flex w-fit items-center gap-2 text-label font-medium text-fg-2 group-data-[disabled=true]/field:opacity-(--o-disabled)",
        className
      )}
      {...props}
    />
  )
}

function FieldDescription({ className, ...props }: React.ComponentProps<"p">) {
  return (
    <p
      data-slot="field-description"
      className={cn(
        "text-left text-label leading-normal font-normal text-fg-2 text-pretty last:mt-0 nth-last-2:-mt-1",
        "[&>a]:underline [&>a]:underline-offset-4 [&>a:hover]:text-primary",
        className
      )}
      {...props}
    />
  )
}

/** 表单项下面的一行错误。不带 shadcn 原版的 role="alert"：这里的错误多是边输边算的（两次密码不一致、金额格式），每按一个键出现 / 消失一次，
 *  alert 会一次次打断读屏；同改前的 .hint.is-error 不播报，提交失败的错误另有 Note / toast */
function FieldError({ className, children, ...props }: React.ComponentProps<"div">) {
  if (!children) return null
  return (
    <div data-slot="field-error" className={cn("text-label font-normal text-bad-text", className)} {...props}>
      {children}
    </div>
  )
}

export { Field, FieldTitle, FieldDescription, FieldError }
