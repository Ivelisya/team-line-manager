"use client"

import * as React from "react"
import { Tabs as TabsPrimitive } from "@base-ui/react/tabs"
import { cn, scrollBehavior } from "@/lib/utils"
import { watchSize } from "@/lib/sharedObserver"

/* shadcn base-mira 的 Tabs（Base UI），做成全站的「下划线页签」：
   - 当前页签底下 2px 的强调色下划线是 Tabs.Indicator，按 Base UI 写的 --active-tab-left / --active-tab-width 定位，换页签时用 --dur-2 / --ease-out 顺滑滑过去（改前是 motion 的 layoutId）；
   - 方向键即激活（activateOnFocus，同改前 Radix），循环；
   - 页签放不下时（窄屏抽屉）横向滚动：被裁的那一侧渐隐提示还有页签（支持滚动驱动动画的浏览器靠 base.css 的 [data-slot=tabs-list] 规则，不支持的由 TabsList 的滚动监听写 data-fade 兜底），
     选中的页签自动滚进可见区；
   - TabsTrigger 的 noPanel：这组页签没有配对的 TabsContent（内容由页面自己按 value 渲染）时传，去掉指向不存在元素的 aria-controls。 */

const TabsCtx = React.createContext<string | undefined>(undefined)

function Tabs({
  className,
  orientation = "horizontal",
  value,
  defaultValue,
  onValueChange,
  ...props
}: TabsPrimitive.Root.Props) {
  // Base UI 不对外暴露当前值：自己跟一份（受控取 value，非受控取内部记录），给 TabsList 在选中页签变化时把它滚进可见区
  const [inner, setInner] = React.useState(defaultValue)
  const cur = value ?? inner
  return (
    <TabsCtx.Provider value={cur === undefined || cur === null ? undefined : String(cur)}>
      <TabsPrimitive.Root
        data-slot="tabs"
        data-orientation={orientation}
        value={value}
        defaultValue={defaultValue}
        onValueChange={(v, d) => { setInner(v); onValueChange?.(v, d) }}
        className={cn("group/tabs flex gap-2 data-horizontal:flex-col", className)}
        {...props}
      />
    </TabsCtx.Provider>
  )
}

function TabsList({ className, children, ref, activateOnFocus = true, ...props }: TabsPrimitive.List.Props) {
  const el = React.useRef<HTMLDivElement | null>(null)
  const cur = React.useContext(TabsCtx)
  const [fade, setFade] = React.useState("")
  const setRef = React.useCallback((n: HTMLDivElement | null) => {
    el.current = n
    if (typeof ref === "function") ref(n)
    else if (ref) (ref as React.MutableRefObject<HTMLDivElement | null>).current = n
  }, [ref])
  React.useLayoutEffect(() => {
    const n = el.current
    if (!n) return
    if (CSS.supports("animation-timeline: scroll()")) return // 渐隐由 base.css 的滚动驱动动画给；这里只给不支持的浏览器兜底
    const update = () => { const l = n.scrollLeft > 1; const r = n.scrollLeft + n.clientWidth < n.scrollWidth - 1; setFade(l && r ? "lr" : l ? "l" : r ? "r" : "") }
    update()
    n.addEventListener("scroll", update, { passive: true })
    const off = watchSize(n, update)
    return () => { n.removeEventListener("scroll", update); off() }
  }, [])
  React.useEffect(() => {
    const n = el.current
    const t = n?.querySelector<HTMLElement>("[data-active]")
    if (!n || !t) return
    const pad = 28 // 留出渐隐的宽度
    if (t.offsetLeft < n.scrollLeft + pad) n.scrollTo({ left: Math.max(0, t.offsetLeft - pad), behavior: scrollBehavior() })
    else if (t.offsetLeft + t.offsetWidth > n.scrollLeft + n.clientWidth - pad) n.scrollTo({ left: t.offsetLeft + t.offsetWidth - n.clientWidth + pad, behavior: scrollBehavior() })
  }, [cur])
  return (
    <TabsPrimitive.List
      ref={setRef}
      data-slot="tabs-list"
      data-fade={fade || undefined}
      activateOnFocus={activateOnFocus}
      className={cn(
        // 下划线页签：页签等高拉伸、间距 20px（手机上的抽屉 16px，见 NodeDrawer / RouterDrawer 的 className），底下一条细线，放不下横向滚动（滚动条藏起来）
        "relative flex items-stretch gap-5 overflow-x-auto overscroll-x-contain border-b border-line [scrollbar-width:none] [&::-webkit-scrollbar]:hidden",
        className
      )}
      {...props}
    >
      {children}
      <TabsPrimitive.Indicator
        data-slot="tabs-indicator"
        className="pointer-events-none absolute bottom-0 left-(--active-tab-left) h-0.5 w-(--active-tab-width) rounded-t-[1px] bg-brand [transition:left_var(--dur-2)_var(--ease-out),width_var(--dur-2)_var(--ease-out)]"
      />
    </TabsPrimitive.List>
  )
}

function TabsTrigger({ className, noPanel, ...props }: TabsPrimitive.Tab.Props & { noPanel?: boolean }) {
  return (
    <TabsPrimitive.Tab
      data-slot="tabs-trigger"
      className={cn(
        // 41 = 40 + 列表 1px 底线，底部 1px 内边距让文字位置不变；不随选中加粗（加粗会让页签变宽、把后面的页签顶开）。按下以底边中点为原点缩放（components.css「交互状态」）
        "relative inline-flex h-[41px] flex-none origin-[50%_100%] items-center gap-1.5 border-0 bg-transparent p-0 pb-px text-body font-medium whitespace-nowrap text-fg-2",
        "[transition:color_var(--dur-1)_var(--ease-std),scale_var(--dur-2)_var(--ease-out)] hover:text-fg data-active:text-fg",
        "focus-visible:-outline-offset-2 disabled:pointer-events-none disabled:opacity-(--o-disabled)", // 焦点环用全站的（base.css :focus-visible），收进元素里面免得被滚动容器裁掉
        "[&_svg]:pointer-events-none [&_svg]:shrink-0 [&_svg:not([class*='size-'])]:size-(--icon-sm)",
        className
      )}
      {...(noPanel ? { "aria-controls": undefined } : null)}
      {...props}
    />
  )
}

function TabsContent({ className, ...props }: TabsPrimitive.Panel.Props) {
  return (
    <TabsPrimitive.Panel
      data-slot="tabs-content"
      className={cn("flex-1 outline-none", className)}
      {...props}
    />
  )
}

export { Tabs, TabsList, TabsTrigger, TabsContent }
