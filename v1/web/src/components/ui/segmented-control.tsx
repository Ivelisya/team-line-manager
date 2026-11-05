"use client";

import { mergeProps } from "@base-ui/react/merge-props";
import { useRender } from "@base-ui/react/use-render";
import * as React from "react";
import { cn } from "@/lib/utils";

export type SegmentedControlSize = "default" | "lg" | "sm";

/* coss ui 的 segmented-control（Base UI 生态，shadcn 没有带滑块的分段控件），尺寸 / 颜色 / 圆角走令牌：
   轨道 --h-lg（sm 是 --h-sm）、内边距 --seg-pad、底色 --track、药丸形；项高 --seg-item（sm 是 --seg-item-sm），触屏随令牌上调；
   选中块是 --thumb + --shadow-thumb 的药丸，换项时 width / translate 用 --dur-2 + --ease-out 滑过去；按下缩放在 components.css「交互状态」。
   选中判定沿用 coss 的三个属性：data-checked（Radio）、data-pressed（Toggle）、aria-current=page。 */
export const segmentedControlItemSizeClassNames: Record<
  SegmentedControlSize,
  string
> = {
  default: "h-(--seg-item) px-3.5 text-body",
  lg: "h-(--seg-item) px-3.5 text-body",
  sm: "h-(--seg-item-sm) px-2.5 text-label",
};

export const segmentedControlItemLayoutClassName =
  "gap-1.5 [&_svg]:pointer-events-none [&_svg]:shrink-0 [&_svg:not([class*='size-'])]:size-(--icon-sm)";

export const segmentedControlIndicatorSurfaceClassName =
  "rounded-pill bg-(--thumb) [box-shadow:var(--shadow-thumb),inset_0_1px_0_color-mix(in_srgb,white_35%,transparent)]";

const segmentedControlRootSize: Record<SegmentedControlSize, string> = {
  default: "h-(--h-lg)",
  lg: "h-(--h-lg)",
  sm: "h-[calc(var(--seg-item-sm)+2*var(--seg-pad))]", // = --h-sm（触屏 --seg-item-sm 上调到 28，轨道随项变高）
};

const SegmentedControlContext: React.Context<SegmentedControlSize> =
  React.createContext<SegmentedControlSize>("default");

export function SegmentedControl({
  className,
  children,
  render,
  size = "default",
  ...props
}: useRender.ComponentProps<"div"> & {
  size?: SegmentedControlSize;
}): React.ReactElement {
  const defaultProps = {
    children: (
      <>
        {children}
        <SegmentedControlIndicator />
      </>
    ),
    className: cn(
      // 放在表单网格里时不被拉满整行（justify-self-start）；放不下时横向滚动、滚动条藏起来（边缘渐隐见 base.css）
      "relative z-0 box-border flex w-fit max-w-full flex-none items-center gap-0.5 overflow-x-auto overscroll-x-contain rounded-pill bg-track p-(--seg-pad) [scrollbar-width:none] justify-self-start [&::-webkit-scrollbar]:hidden",
      segmentedControlRootSize[size],
      className,
    ),
    "data-size": size,
    "data-slot": "segmented-control",
  };

  const element = useRender({
    defaultTagName: "div",
    props: mergeProps<"div">(defaultProps, props),
    render,
  });

  return (
    <SegmentedControlContext.Provider value={size}>
      {element}
    </SegmentedControlContext.Provider>
  );
}

export function SegmentedControlItem({
  className,
  render,
  size,
  ...props
}: useRender.ComponentProps<"button"> & {
  size?: SegmentedControlSize;
}): React.ReactElement {
  const contextSize = React.useContext(SegmentedControlContext);
  const resolvedSize = size ?? contextSize;

  const defaultProps = {
    className: cn(
      // 项：药丸、二级字，悬停浅底 + 一级字（选中的不叠），选中一级字；焦点环收进轨道里面（轨道会裁掉外圈）；禁用整组置灰仍占位
      "relative isolate inline-flex shrink-0 cursor-pointer select-none items-center justify-center rounded-pill border-0 bg-transparent font-medium whitespace-nowrap text-fg-2 [font-variant-numeric:tabular-nums] focus-visible:-outline-offset-2",
      "[transition:color_var(--dur-1)_var(--ease-std),background-color_var(--dur-1)_var(--ease-std),scale_var(--dur-2)_var(--ease-out)]",
      "hover:not-data-checked:not-data-pressed:not-disabled:not-data-disabled:bg-hover hover:not-data-checked:not-data-pressed:not-disabled:not-data-disabled:text-fg",
      "disabled:pointer-events-none disabled:opacity-(--o-disabled) disabled:saturate-40 data-disabled:pointer-events-none data-disabled:opacity-(--o-disabled) data-disabled:saturate-40",
      "data-checked:text-fg aria-[current=page]:text-fg data-pressed:text-fg",
      // 滑块还没量出来的那一刻（首帧）：选中项自己垫一块底，量出来以后由滑块接手
      "data-checked:bg-(--thumb) data-checked:shadow-(--shadow-thumb) aria-[current=page]:bg-(--thumb) aria-[current=page]:shadow-(--shadow-thumb) data-pressed:bg-(--thumb) data-pressed:shadow-(--shadow-thumb)",
      "[:has(>[data-slot=segmented-control-indicator]:not([hidden]))>&]:bg-transparent! [:has(>[data-slot=segmented-control-indicator]:not([hidden]))>&]:shadow-none!",
      segmentedControlItemLayoutClassName,
      segmentedControlItemSizeClassNames[resolvedSize],
      className,
    ),
    "data-size": resolvedSize,
    "data-slot": "segmented-control-item",
    ...(render ? {} : { type: "button" as const }),
  };

  return useRender({
    defaultTagName: "button",
    props: mergeProps<"button">(defaultProps, props),
    render,
  });
}

const activeItemSelector =
  ':scope > [data-checked], :scope > [aria-current="page"], :scope > [data-pressed]';

const observedAttributes: string[] = [
  "data-checked",
  "aria-current",
  "data-pressed",
];

const MAX_LAYOUT_ROUNDING_ERROR = 2;

interface ActiveItemRect {
  height: number;
  left: number;
  top: number;
  width: number;
}

function isSameRect(
  a: ActiveItemRect | null,
  b: ActiveItemRect | null,
): boolean {
  return (
    a?.left === b?.left &&
    a?.top === b?.top &&
    a?.width === b?.width &&
    a?.height === b?.height
  );
}

function getCssSize(element: HTMLElement): { height: number; width: number } {
  const style = getComputedStyle(element);
  const width = Number.parseFloat(style.width) || 0;
  const height = Number.parseFloat(style.height) || 0;
  if (
    Math.round(width) !== element.offsetWidth ||
    Math.round(height) !== element.offsetHeight
  ) {
    return { height: element.offsetHeight, width: element.offsetWidth };
  }
  return { height, width };
}

function measureItem(item: HTMLElement, root: HTMLElement): ActiveItemRect {
  const { width, height } = getCssSize(item);
  const rootSize = getCssSize(root);
  const itemRect = item.getBoundingClientRect();
  const rootRect = root.getBoundingClientRect();
  const scaleX = rootSize.width > 0 ? rootRect.width / rootSize.width : 1;
  const scaleY = rootSize.height > 0 ? rootRect.height / rootSize.height : 1;

  const rectLeft =
    (itemRect.left - rootRect.left) / scaleX -
    root.clientLeft +
    root.scrollLeft;
  const rectTop =
    (itemRect.top - rootRect.top) / scaleY - root.clientTop + root.scrollTop;

  const isUndistorted =
    Math.abs(rectLeft - item.offsetLeft) <= MAX_LAYOUT_ROUNDING_ERROR &&
    Math.abs(rectTop - item.offsetTop) <= MAX_LAYOUT_ROUNDING_ERROR;

  return {
    height,
    left: isUndistorted ? rectLeft : item.offsetLeft,
    top: isUndistorted ? rectTop : item.offsetTop,
    width,
  };
}

function SegmentedControlIndicator(): React.ReactElement {
  const ref = React.useRef<HTMLSpanElement>(null);
  const [rect, setRect] = React.useState<ActiveItemRect | null>(null);

  React.useLayoutEffect(() => {
    const root = ref.current?.parentElement;
    if (!root) {
      return;
    }

    const update = (): void => {
      const item = root.querySelector<HTMLElement>(activeItemSelector);
      const next = item ? measureItem(item, root) : null;
      setRect((previous) => (isSameRect(previous, next) ? previous : next));
    };

    update();

    const resizeObserver = new ResizeObserver(update);
    const mutationObserver = new MutationObserver((records) => {
      if (records.some((record) => record.type === "childList")) {
        observeItems();
      }
      update();
    });
    const observeItems = (): void => {
      resizeObserver.disconnect();
      resizeObserver.observe(root);
      for (const child of root.children) {
        resizeObserver.observe(child);
        mutationObserver.observe(child, {
          attributeFilter: observedAttributes,
          attributes: true,
        });
      }
    };
    mutationObserver.observe(root, { childList: true });
    observeItems();

    return () => {
      resizeObserver.disconnect();
      mutationObserver.disconnect();
    };
  }, []);

  return (
    <span
      aria-hidden="true"
      className={cn(
        "pointer-events-none absolute top-0 left-0 -z-1 h-(--active-item-height) w-(--active-item-width) translate-x-(--active-item-left) translate-y-(--active-item-top) [--ease-spring:cubic-bezier(.34,1.32,.5,1)] [transition:width_var(--dur-3)_var(--ease-spring),height_var(--dur-3)_var(--ease-spring),translate_var(--dur-3)_var(--ease-spring)]",
        segmentedControlIndicatorSurfaceClassName,
      )}
      data-slot="segmented-control-indicator"
      hidden={!rect}
      ref={ref}
      style={
        {
          "--active-item-height": `${rect?.height ?? 0}px`,
          "--active-item-left": `${rect?.left ?? 0}px`,
          "--active-item-top": `${rect?.top ?? 0}px`,
          "--active-item-width": `${rect?.width ?? 0}px`,
        } as React.CSSProperties
      }
    />
  );
}
