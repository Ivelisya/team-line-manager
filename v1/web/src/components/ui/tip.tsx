import * as React from 'react';
import * as TooltipPrimitive from '@radix-ui/react-tooltip';

// 单独成文件：客户页只要提示，不必为此带上 overlays 里的下拉、弹层、页签、motion（overlays 重新导出它们，其余页面照旧从那里取）。不要反向 import overlays
/* ---- Tooltip ---- */
export const TooltipProvider = ({ children }: { children: React.ReactNode }) => <TooltipPrimitive.Provider delayDuration={250}>{children}</TooltipPrimitive.Provider>;
type Side = 'top' | 'bottom' | 'left' | 'right';
export function Tip({ text, children, side = 'top' }: { text: React.ReactNode; children: React.ReactElement; side?: Side }) {
  if (!text) return children;
  return (
    <TooltipPrimitive.Root>
      <TooltipPrimitive.Trigger asChild>{children}</TooltipPrimitive.Trigger>
      <TooltipPrimitive.Portal><TooltipPrimitive.Content side={side} sideOffset={6} collisionPadding={8} className="tooltip">{text}</TooltipPrimitive.Content></TooltipPrimitive.Portal>
    </TooltipPrimitive.Root>
  );
}

/** 共享悬停提示：卡片、表格行里成百上千的只读说明不各挂一个 Radix Tooltip 根（每个根带一套 Popper 上下文，随实时推送跟着重渲染），
 *  元素上写 data-tip="文字"，外壳里这一份按悬停 / 键盘聚焦到的元素定位显示。外观、延迟同 Tip：停 250ms 出现，刚有提示开着时直接换过去。
 *  空的 data-tip 当作没有（拼出来的说明可能为空，调用方不用再判）；data-tip-side 指定弹出方向（默认在上方） */
const TIP_SEL = '[data-tip]:not([data-tip=""])';
export function TipHost() {
  const [tip, setTip] = React.useState<{ id: number; text: string; r: DOMRect; side: Side; instant: boolean } | null>(null);
  const [open, setOpen] = React.useState(false);
  const hide = React.useRef(() => {});
  React.useEffect(() => {
    let cur: Element | null = null; let timer = 0; let shown = false; let closedAt = 0; let seq = 0;
    let tapClosed: Element | null = null; // 按下时收起的是哪个（再点同一个「(i)」= 收起，不再打开）
    let touch = false; // 最近一次按下是手指
    // 对话框标题下面那行说明里的「(i)」往下弹：往上会盖住标题
    const sideOf = (el: HTMLElement): Side => (el.dataset.tipSide as Side | undefined) ?? (el.closest('.dialog-desc') ? 'bottom' : 'top');
    // instant：刚收起一个又悬停到下一个（扫过一排心跳格），新提示直接出现、不再播一遍缩放淡入（macOS 也是第一次等、之后换项不播）
    const show = (el: HTMLElement, instant = false) => {
      shown = true; watch.disconnect(); size.disconnect(); watch.observe(el, { attributes: true, attributeFilter: ['data-tip'] }); size.observe(el);
      setTip({ id: ++seq, text: el.dataset.tip ?? '', r: el.getBoundingClientRect(), side: sideOf(el), instant }); setOpen(true);
    };
    // 悬停着的元素 data-tip 变了（推送改了读数、说明跟着变）、尺寸变了就按新的文字和位置更新（同一个 id，不重播动画）；元素被移出文档（卡片被换掉、页签内容被换掉）就收起。
    // 只盯当前这一个元素：属性观察 + ResizeObserver（元素被摘出文档时它也会回调，尺寸变成 0）
    const watch = new MutationObserver(() => refresh());
    const size = new ResizeObserver(() => refresh());
    const refresh = () => {
      if (!shown || !cur) return;
      const el = cur as HTMLElement;
      if (!el.isConnected || !el.matches(TIP_SEL)) { close(); return; }
      const text = el.dataset.tip ?? '', r = el.getBoundingClientRect();
      setTip((t) => !t || (t.text === text && t.r.left === r.left && t.r.top === r.top && t.r.width === r.width && t.r.height === r.height) ? t : { ...t, text, r }); // ResizeObserver 挂上时会立刻回调一次，没变就原样返回
    };
    const close = () => {
      clearTimeout(timer); cur = null; watch.disconnect(); size.disconnect();
      if (shown) { shown = false; closedAt = performance.now(); setOpen(false); }
    };
    hide.current = close;
    const over = (e: PointerEvent) => {
      if (e.pointerType === 'touch') return;
      const el = (e.target as Element).closest<HTMLElement>(TIP_SEL);
      if (el === cur) return;
      close();
      if (!el) return;
      cur = el;
      if (performance.now() - closedAt < 300) show(el, true); else timer = window.setTimeout(() => show(el), 250);
    };
    // 键盘聚焦才弹；鼠标按下得到的焦点不算（同 Radix）
    const focus = (e: FocusEvent) => { const el = (e.target as Element).closest<HTMLElement>(TIP_SEL); if (el?.matches(':focus-visible')) { close(); cur = el; show(el); } };
    const leave = (e: PointerEvent) => { if (!e.relatedTarget) close(); }; // 指针离开窗口
    // 轻触同一个「(i)」第二下 = 收起；鼠标点开着的「(i)」不收（悬停已经打开了，点它是想看）
    const down = (e: PointerEvent) => { touch = e.pointerType === 'touch'; tapClosed = shown && touch ? cur : null; close(); };
    // 「(i)」点一下 / 轻触也能打开：触屏没有悬停，鼠标点下去得到的焦点也不弹（只认键盘聚焦）。
    // 触屏上轻触别的带说明的字（省略了的名称、心跳格、小标签）也弹；本身可点的（按钮、链接、整行可点的表格行）点了是去做事，不弹
    const click = (e: MouseEvent) => {
      const t = e.target as Element;
      const el = t.closest<HTMLElement>('.info-tip') ?? (touch ? t.closest<HTMLElement>(TIP_SEL) : null);
      if (el && !el.classList.contains('info-tip') && el.closest('a, button, [role="button"], .is-link')) return;
      if (!el?.dataset.tip || tapClosed === el) { tapClosed = null; return; }
      cur = el; show(el);
    };
    const on: [EventTarget, string, EventListener, boolean][] = [
      [document, 'pointerover', over as EventListener, false], [document, 'pointerout', leave as EventListener, false], [document, 'pointerdown', down as EventListener, true], [document, 'click', click as EventListener, false],
      [document, 'focusin', focus as EventListener, false], [document, 'focusout', close, false], [window, 'scroll', close, true], [window, 'wheel', close, true],
    ];
    on.forEach(([t, n, f, c]) => t.addEventListener(n, f, c));
    return () => { clearTimeout(timer); watch.disconnect(); size.disconnect(); on.forEach(([t, n, f, c]) => t.removeEventListener(n, f, c)); };
  }, []);
  if (!tip) return null;
  // key：换到另一个元素时重新挂载（同 Tip 之间切换：旧的收起、新的立即出现），不在原地改字挪位置
  return (
    <TooltipPrimitive.Root key={tip.id} open={open} onOpenChange={(o) => { if (!o) hide.current(); }}>
      <TooltipPrimitive.Trigger asChild><span className="float-anchor" style={{ left: tip.r.left, top: tip.r.top, width: tip.r.width, height: tip.r.height }} /></TooltipPrimitive.Trigger>
      <TooltipPrimitive.Portal><TooltipPrimitive.Content side={tip.side} sideOffset={6} collisionPadding={8} className="tooltip" data-instant={tip.instant || undefined}>{tip.text}</TooltipPrimitive.Content></TooltipPrimitive.Portal>
    </TooltipPrimitive.Root>
  );
}
