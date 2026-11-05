import * as React from 'react';
import { ToggleGroup } from '@base-ui/react/toggle-group';
import { Toggle } from '@base-ui/react/toggle';
import { ChevronDown } from 'lucide-react';
import { cn } from '@/lib/utils';
import { watchSize } from '@/lib/sharedObserver';
import { SegmentedControl, SegmentedControlItem } from '@/components/ui/segmented-control';
import { DropdownMenu, DropdownMenuContent, DropdownMenuItem, DropdownMenuTrigger } from '@/components/ui/dropdown-menu';

/* 分段控件（单选）：大卡片 / 小卡片 / 紧凑、团队筛选等。底座是 coss ui 的 segmented-control（带滑块的轨道 + 项，见 segmented-control.tsx），
   单选用 Base UI 的 ToggleGroup（单值，role=group + aria-label，项 aria-pressed）：Tab 进组、方向键只移动焦点、Enter / 空格才选中——手动激活，同改前的 Radix ToggleGroup，也是 macOS 的习惯；
   不用 RadioGroup：它方向键一按就选中，首页的视图 / 密度、时间范围会随方向键反复重排 / 重取数，访问页的视图切换按一下 → 就跳走了，键盘过不去。再按一下已选中的项不取消（始终选着一项）。
   选中块是轨道里的一个滑块，换项时 width / translate 过渡滑过去（--dur-2 + --ease-out）。 */
export interface SegItem<T extends string> { value: T; label: React.ReactNode; count?: number }
/** memo：父页面随实时推送重渲染时，items / onChange 稳定（模块常量、useMemo、useCallback）的分段控件不跟着重画 */
/** disabled：整组置灰（如首页的卡片密度在表格 / 环形视图下），仍占位、不让工具栏跳动 */
type SegProps<T extends string> = { value: T; onChange: (v: T) => void; items: readonly SegItem<T>[]; label: string; sm?: boolean; disabled?: boolean; className?: string };

const Count = ({ n }: { n: number }) => <span className="font-normal text-fg-2 [font-variant-numeric:tabular-nums]">{n}</span>;

/** 单值：再按一下已选中的项会得到空数组，忽略（始终选着一项） */
const pick = <T extends string>(onChange: (v: T) => void) => (v: string[]) => { if (v.length) onChange(v[0] as T); };

function Item<T extends string>({ it }: { it: SegItem<T> }) {
  return (
    <Toggle value={it.value} render={<SegmentedControlItem />}>
      {it.label}{it.count != null && <Count n={it.count} />}
    </Toggle>
  );
}

export const Segmented = React.memo(function Segmented<T extends string>({ value, onChange, items, label, sm, disabled, className }: SegProps<T>) {
  return (
    <ToggleGroup value={[value]} onValueChange={pick(onChange)} aria-label={label} disabled={disabled} render={<SegmentedControl size={sm ? 'sm' : 'default'} className={className} />}>
      {items.map((it) => <Item key={it.value} it={it} />)}
    </ToggleGroup>
  );
}) as <T extends string>(p: SegProps<T>) => React.ReactElement;

/** 放不下就收进「更多」的分段控件（首页的团队分组）：轨道撑满可用宽度，从左往右能放几项放几项，其余进最右的「更多 ▾」下拉。
 *  选中项一定留在轨道上：它本该在「更多」里时，顶替轨道上最后一个可见项（被顶下来的进下拉）。
 *  每项的自然宽度在一份隐藏的同款副本里量（轨道里的项会被拉伸，量不准）。重量的时机：轨道宽度变了、选项文字 / 计数变了，
 *  以及副本自己的尺寸变了（字体、样式晚到，项变宽变窄）和页面字体加载完——只量一次会量在布局稳定之前（机器忙时首页的分段控件被误收进「更多」，之后宽度不变就一直不重量） */
export const SegmentedOverflow = React.memo(function SegmentedOverflow<T extends string>({ value, onChange, items, label, className }: Omit<SegProps<T>, 'sm' | 'disabled'>) {
  const root = React.useRef<HTMLDivElement>(null);
  const meas = React.useRef<HTMLDivElement>(null);
  const [dim, setDim] = React.useState<{ avail: number; widths: number[]; more: number; gap: number } | null>(null);
  // 文字也算进去（团队改名时 id、台数都没变）；label 不是字符串的（图标等）按值认
  const sig = items.map((it) => `${it.value}:${it.count ?? ''}:${typeof it.label === 'string' || typeof it.label === 'number' ? it.label : ''}`).join('|');
  React.useLayoutEffect(() => {
    const r = root.current; const ms = meas.current;
    if (!r || !ms) return;
    const measure = () => {
      const cs = getComputedStyle(r);
      const avail = r.clientWidth - parseFloat(cs.paddingLeft) - parseFloat(cs.paddingRight);
      const els = [...ms.children] as HTMLElement[];
      const widths = els.slice(0, -1).map((e) => e.getBoundingClientRect().width);
      const more = els[els.length - 1].getBoundingClientRect().width;
      const gap = parseFloat(getComputedStyle(ms).columnGap) || 0;
      setDim((o) => (o && o.avail === avail && o.more === more && o.widths.join() === widths.join() ? o : { avail, widths, more, gap }));
    };
    measure();
    // 只盯轨道和副本：副本是 w-max，任何一项变宽变窄它整体就跟着变（字体 / 样式晚到时尤其）
    const offs = [watchSize(r, measure), watchSize(ms, measure)];
    document.fonts?.ready.then(measure);
    document.fonts?.addEventListener('loadingdone', measure);
    return () => { offs.forEach((f) => f()); document.fonts?.removeEventListener('loadingdone', measure); };
  }, [sig]);
  // 算出轨道上放哪些项（按原顺序），其余进「更多」
  const sel = items.findIndex((it) => it.value === value);
  let shown = items.map((_, i) => i);
  if (dim && dim.widths.length === items.length) {
    const sum = (ix: number[]) => ix.reduce((a, i) => a + dim.widths[i], 0) + dim.gap * Math.max(0, ix.length - 1);
    if (sum(shown) > dim.avail + 0.5) {
      let k = items.length - 1;
      const pickShown = (k: number) => { const ix = shown.slice(0, k); return sel >= k && sel >= 0 ? [...ix.slice(0, Math.max(0, k - 1)), sel] : ix; };
      while (k > 1 && sum(pickShown(k)) + dim.gap + dim.more > dim.avail) k--;
      shown = pickShown(k);
    }
  }
  const hidden = items.filter((_, i) => !shown.includes(i));
  return (
    <div className={cn('seg-ovf', className)}>
      <ToggleGroup ref={root} value={[value]} onValueChange={pick(onChange)} aria-label={label} render={<SegmentedControl className="min-w-0 flex-1 basis-0 max-w-none [&>[data-slot=segmented-control-item]]:grow" />}>
        {shown.map((i) => <Item key={items[i].value} it={items[i]} />)}
        {hidden.length > 0 && (
          <DropdownMenu>
            <DropdownMenuTrigger aria-label={`更多${label}`} render={<SegmentedControlItem className="flex-none! gap-0.5 data-popup-open:bg-hover data-popup-open:text-fg" />}>更多<ChevronDown size={14} className="text-fg-3" /></DropdownMenuTrigger>
            <DropdownMenuContent align="end">
              {hidden.map((it) => (
                <DropdownMenuItem key={it.value} onClick={() => onChange(it.value)}>
                  <span className="grow">{it.label}</span>{it.count != null && <Count n={it.count} />}
                </DropdownMenuItem>
              ))}
            </DropdownMenuContent>
          </DropdownMenu>
        )}
      </ToggleGroup>
      {/* 量宽用的隐藏副本：叠在原位、不占位、不可见、不接指针，样式同轨道里的项（SegmentedControlItem），所以量出来的就是项的自然宽度 */}
      <div ref={meas} aria-hidden className="pointer-events-none invisible absolute top-0 left-0 flex w-max max-w-none gap-0.5 p-(--seg-pad)">
        {items.map((it) => <SegmentedControlItem key={it.value} render={<span />}>{it.label}{it.count != null && <Count n={it.count} />}</SegmentedControlItem>)}
        <SegmentedControlItem render={<span />} className="gap-0.5">更多<ChevronDown size={14} /></SegmentedControlItem>
      </div>
    </div>
  );
}) as <T extends string>(p: Omit<SegProps<T>, 'sm' | 'disabled'>) => React.ReactElement;
