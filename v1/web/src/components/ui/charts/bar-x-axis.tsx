"use client";

import { m } from "motion/react";
import { memo, useMemo } from "react";
import { createPortal } from "react-dom";
import { cn } from "@/lib/utils";
import {
  useChart,
  useChartPortalContainer,
  useChartStable,
} from "./chart-context";
import {
  type ChartLabelCount,
  resolveResponsiveChartLabelCount,
} from "./responsive-chart-label-count";

export interface BarXAxisProps {
  /** Width of the date ticker box for fade calculation. Default: 50 */
  tickerHalfWidth?: number;
  /** Whether to show all labels or skip some for dense data. Default: false */
  showAllLabels?: boolean;
  /** Maximum number of labels to show. Supports responsive breakpoints. */
  maxLabels?: ChartLabelCount;
  /** Text for a category label (default: the category itself). */
  formatLabel?: (label: string) => string;
  /** Thin the labels so neighbours are at least this many px apart (replaces `maxLabels`). */
  minLabelGap?: number;
  /** Fade labels near the hovered bar (for the date ticker). Default: true */
  fadeOnHover?: boolean;
}

interface BarXAxisLabelProps {
  label: string;
  x: number;
  crosshairX: number | null;
  isHovering: boolean;
  tickerHalfWidth: number;
}

function BarXAxisLabel({
  label,
  x,
  crosshairX,
  isHovering,
  tickerHalfWidth,
}: BarXAxisLabelProps) {
  const fadeBuffer = 20;
  const fadeRadius = tickerHalfWidth + fadeBuffer;

  let opacity = 1;
  if (isHovering && crosshairX !== null) {
    const distance = Math.abs(x - crosshairX);
    if (distance < tickerHalfWidth) {
      opacity = 0;
    } else if (distance < fadeRadius) {
      opacity = (distance - tickerHalfWidth) / fadeBuffer;
    }
  }

  // Zero-width container approach for perfect centering
  return (
    <div
      className="absolute"
      style={{
        left: x,
        bottom: 0, // 绘图区下沿的「0」刻度是半个字高，横轴标签贴底、不和它重叠
        width: 0,
        display: "flex",
        justifyContent: "center",
      }}
    >
      <m.span
        animate={{ opacity }}
        className={cn("whitespace-nowrap font-num text-chart-label text-micro")}
        initial={{ opacity: 1 }}
        transition={{ duration: 0.4, ease: "easeInOut" }}
      >
        {label}
      </m.span>
    </div>
  );
}

export function BarXAxis(props: BarXAxisProps) {
  const { containerRef, barScale } = useChartStable();
  const container = useChartPortalContainer(containerRef);
  if (!container) {
    return null;
  }

  if (!barScale) {
    return null;
  }

  return <BarXAxisInner {...props} container={container} />;
}

/** Every step-th item counted back from the LAST one, so the newest label (today / the current hour) is always kept. */
function everyNthFromEnd<T>(items: T[], step: number): T[] {
  return items.filter((_, i) => (items.length - 1 - i) % step === 0);
}

const BarXAxisInner = memo(function BarXAxisInner({
  tickerHalfWidth = 50,
  showAllLabels = false,
  maxLabels = 12,
  formatLabel,
  minLabelGap,
  fadeOnHover = true,
  container,
}: BarXAxisProps & { container: HTMLDivElement }) {
  const {
    margin,
    tooltipData,
    barScale,
    bandWidth,
    barXAccessor,
    data,
    width,
  } = useChart();

  const resolvedMaxLabels = useMemo(
    () => resolveResponsiveChartLabelCount(maxLabels, width, 12),
    [maxLabels, width]
  );

  // Generate labels for each bar
  const labelsToShow = useMemo(() => {
    if (!(barScale && bandWidth && barXAccessor)) {
      return [];
    }

    const allLabels = data.map((d) => {
      const label = barXAccessor(d);
      const bandX = barScale(label) ?? 0;
      // Center the label under the bar group
      const x = bandX + bandWidth / 2 + margin.left;
      return { label: formatLabel ? formatLabel(label) : label, x };
    });

    // Neighbours at least `minLabelGap` px apart: keep every `step`-th band.
    if (minLabelGap) {
      const pitch =
        (barScale as unknown as { step?: () => number }).step?.() ?? bandWidth;
      const gapStep = Math.max(1, Math.ceil(minLabelGap / Math.max(1, pitch)));
      return everyNthFromEnd(allLabels, gapStep);
    }

    // If showAllLabels is true or we have fewer than maxLabels, show all
    if (showAllLabels || allLabels.length <= resolvedMaxLabels) {
      return allLabels;
    }

    // Otherwise, skip some labels to avoid crowding
    const step = Math.ceil(allLabels.length / Math.max(1, resolvedMaxLabels));
    return everyNthFromEnd(allLabels, step);
  }, [
    barScale,
    bandWidth,
    barXAccessor,
    data,
    margin.left,
    showAllLabels,
    resolvedMaxLabels,
    formatLabel,
    minLabelGap,
  ]);

  const isHovering = fadeOnHover && tooltipData !== null;
  const crosshairX = tooltipData ? tooltipData.x + margin.left : null;

  return createPortal(
    <div className="pointer-events-none absolute inset-0">
      {labelsToShow.map((item) => (
        <BarXAxisLabel
          crosshairX={crosshairX}
          isHovering={isHovering}
          key={`${item.label}-${item.x}`}
          label={item.label}
          tickerHalfWidth={tickerHalfWidth}
          x={item.x}
        />
      ))}
    </div>,
    container
  );
});

BarXAxis.displayName = "BarXAxis";

export default BarXAxis;
