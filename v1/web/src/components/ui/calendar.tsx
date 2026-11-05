import * as React from "react"
import { cn } from "@/lib/utils"
import {
  DayPicker,
  getDefaultClassNames,
  type DayButton,
  type Locale,
} from "react-day-picker"

import { Button } from "@/components/ui/button"
import { ChevronLeftIcon, ChevronRightIcon, ChevronDownIcon } from "lucide-react"

/* shadcn base-mira 的 Calendar（react-day-picker），颜色 / 圆角 / 尺寸走令牌：
   格子高 --cell-size（= --h-lg，触屏随令牌上调），选中日是强调色实底（bg-primary），今天是 1px 线描边，上 / 下月补位的日期三级字；
   上个月 / 下个月是 <Button>（带 data-slot，触屏热区外扩）。日期的显示格式、星期与月份文字、「今天」按钮由 date-field.tsx 给。 */

function Calendar({
  className,
  classNames,
  showOutsideDays = true,
  captionLayout = "label",
  buttonVariant = "ghost",
  locale,
  formatters,
  components,
  ...props
}: React.ComponentProps<typeof DayPicker> & {
  buttonVariant?: React.ComponentProps<typeof Button>["variant"]
}) {
  const defaultClassNames = getDefaultClassNames()

  return (
    <DayPicker
      showOutsideDays={showOutsideDays}
      className={cn(
        "group/calendar w-full bg-transparent [--cell-radius:var(--r-control)] [--cell-size:var(--h-lg)]",
        String.raw`rtl:**:[.rdp-button\_next>svg]:rotate-180`,
        String.raw`rtl:**:[.rdp-button\_previous>svg]:rotate-180`,
        className
      )}
      captionLayout={captionLayout}
      locale={locale}
      formatters={{
        formatMonthDropdown: (date) =>
          date.toLocaleString(locale?.code, { month: "short" }),
        ...formatters,
      }}
      classNames={{
        root: cn("w-full", defaultClassNames.root),
        months: cn(
          "relative flex flex-col gap-4 md:flex-row",
          defaultClassNames.months
        ),
        month: cn("flex w-full flex-col gap-1.5", defaultClassNames.month),
        nav: cn(
          "absolute inset-x-0 top-0 flex w-full items-center justify-between gap-1",
          defaultClassNames.nav
        ),
        month_caption: cn(
          "flex h-(--h-md) w-full items-center justify-center px-(--h-md)",
          defaultClassNames.month_caption
        ),
        dropdowns: cn(
          "flex h-(--h-md) w-full items-center justify-center gap-1.5 text-body font-medium",
          defaultClassNames.dropdowns
        ),
        dropdown_root: cn(
          "relative rounded-(--cell-radius)",
          defaultClassNames.dropdown_root
        ),
        dropdown: cn(
          "absolute inset-0 bg-popover opacity-0",
          defaultClassNames.dropdown
        ),
        caption_label: cn(
          "font-num text-body font-semibold select-none",
          captionLayout === "label"
            ? ""
            : "flex items-center gap-1 rounded-(--cell-radius) [&>svg]:size-(--icon-sm) [&>svg]:text-fg-2",
          defaultClassNames.caption_label
        ),
        month_grid: cn("w-full border-collapse", defaultClassNames.month_grid),
        weekdays: cn("flex gap-0.5", defaultClassNames.weekdays),
        weekday: cn(
          "grid h-6.5 flex-1 place-items-center text-micro font-normal text-fg-2 select-none",
          defaultClassNames.weekday
        ),
        week: cn("mt-0.5 flex w-full gap-0.5", defaultClassNames.week),
        week_number_header: cn(
          "w-(--cell-size) select-none",
          defaultClassNames.week_number_header
        ),
        week_number: cn(
          "text-micro text-fg-2 select-none",
          defaultClassNames.week_number
        ),
        day: cn(
          "group/day relative h-(--cell-size) min-w-0 flex-1 p-0 text-center select-none",
          defaultClassNames.day
        ),
        range_start: cn("rounded-l-(--cell-radius) bg-muted", defaultClassNames.range_start),
        range_middle: cn("rounded-none bg-muted", defaultClassNames.range_middle),
        range_end: cn("rounded-r-(--cell-radius) bg-muted", defaultClassNames.range_end),
        today: cn("", defaultClassNames.today),
        outside: cn("text-fg-3", defaultClassNames.outside),
        disabled: cn("", defaultClassNames.disabled),
        hidden: cn("invisible", defaultClassNames.hidden),
        ...classNames,
      }}
      components={{
        Root: ({ className, rootRef, ...props }) => {
          return (
            <div
              data-slot="calendar"
              ref={rootRef}
              className={cn(className)}
              {...props}
            />
          )
        },
        Chevron: ({ className, orientation, ...props }) => {
          if (orientation === "left") {
            return (
              <ChevronLeftIcon className={cn("size-(--icon-md)", className)} {...props} />
            )
          }

          if (orientation === "right") {
            return (
              <ChevronRightIcon className={cn("size-(--icon-md)", className)} {...props} />
            )
          }

          return (
            <ChevronDownIcon className={cn("size-(--icon-md)", className)} {...props} />
          )
        },
        PreviousMonthButton: ({ className, ...p }) => (
          <Button variant={buttonVariant} size="icon-sm" className={cn("select-none aria-disabled:opacity-(--o-disabled)", className)} {...(p as React.ComponentProps<typeof Button>)} />
        ),
        NextMonthButton: ({ className, ...p }) => (
          <Button variant={buttonVariant} size="icon-sm" className={cn("select-none aria-disabled:opacity-(--o-disabled)", className)} {...(p as React.ComponentProps<typeof Button>)} />
        ),
        DayButton: ({ ...props }) => (
          <CalendarDayButton locale={locale} {...props} />
        ),
        WeekNumber: ({ children, ...props }) => {
          return (
            <td {...props}>
              <div className="flex size-(--cell-size) items-center justify-center text-center">
                {children}
              </div>
            </td>
          )
        },
        ...components,
      }}
      {...props}
    />
  )
}

function CalendarDayButton({
  className,
  day,
  modifiers,
  locale,
  ...props
}: React.ComponentProps<typeof DayButton> & { locale?: Partial<Locale> }) {
  const defaultClassNames = getDefaultClassNames()

  const ref = React.useRef<HTMLButtonElement>(null)
  React.useEffect(() => {
    if (modifiers.focused) ref.current?.focus()
  }, [modifiers.focused])

  return (
    <Button
      ref={ref}
      variant="ghost"
      size="icon"
      data-day={day.date.toLocaleDateString(locale?.code)}
      data-selected-single={
        modifiers.selected &&
        !modifiers.range_start &&
        !modifiers.range_end &&
        !modifiers.range_middle
      }
      data-range-start={modifiers.range_start}
      data-range-end={modifiers.range_end}
      data-range-middle={modifiers.range_middle}
      data-today={modifiers.today || undefined}
      className={cn(
        // 数字用等宽数字字体；今天 = 1px 线描边；选中 = 强调色实底（描边让位）；焦点环贴在格子边上（格子之间只有 2px 缝）
        "relative isolate size-auto h-full w-full min-w-0 rounded-(--cell-radius) border-0 font-num font-medium text-fg [font-variant-numeric:var(--num-variant)] focus-visible:outline-offset-0 group-data-[outside=true]/day:text-fg-3 disabled:opacity-(--o-disabled) " +
          "data-[today]:shadow-[inset_0_0_0_1px_var(--line-strong)] " +
          "data-[range-end=true]:rounded-(--cell-radius) data-[range-end=true]:bg-primary data-[range-end=true]:text-primary-foreground data-[range-middle=true]:rounded-none data-[range-middle=true]:bg-muted data-[range-middle=true]:text-fg data-[range-start=true]:rounded-(--cell-radius) data-[range-start=true]:bg-primary data-[range-start=true]:text-primary-foreground " +
          "data-[selected-single=true]:bg-primary data-[selected-single=true]:text-primary-foreground data-[selected-single=true]:shadow-none data-[selected-single=true]:focus-visible:outline-fg data-[selected-single=true]:hover:not-disabled:bg-primary",
        defaultClassNames.day,
        className
      )}
      {...props}
    />
  )
}

export { Calendar, CalendarDayButton }
