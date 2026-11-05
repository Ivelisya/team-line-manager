// 容器级的“⋯”下拉（节点网格 / 表格的 NodeMenus、入口区的 EntryMenus 共用）：一个容器只挂一个 Base UI 下拉根，卡片和行里只放带 data 属性的“⋯”按钮。
// 按钮被点击（键盘回车 / 空格也是 click）时，把这个按钮当作菜单的定位锚（Positioner 的 anchor）：根上没有 Trigger，菜单按按钮的位置定位，
// 锁滚动让出滚动条槽位、推送改了卡片高度时仍跟着按钮走（floating-ui 的 autoUpdate）。
// 没有 Trigger 就没有“是键盘打开的”这条信息（Base UI 只在 Trigger 的点击里记）：这里自己看点击事件（detail === 0 是键盘 / 读屏触发的），
// 键盘打开时把焦点放到第一个可用的菜单项上（菜单随之高亮第一项）；鼠标 / 触屏打开时焦点留在菜单容器上，方向键再往下走。
import * as React from 'react';
import { useHeld } from '@/lib/useHeld';
import { DropdownMenu, DropdownMenuContent } from '@/components/ui/dropdown-menu';

// 菜单项要把焦点交给菜单外的面板（终端的密码框）时，得等菜单完全关掉再动手：菜单退场动画期间面板先拿到焦点，可能被菜单拉回菜单容器（指针离开菜单项时 Base UI 会 focus 菜单容器），
// 菜单卸载后焦点落在 body、再被下面的 finalFocus 回给“⋯”按钮。登记一个回调，菜单关完（onOpenChangeComplete(false)，见 menuClosed）时执行，且不再把焦点回给按钮
let afterClose: (() => void) | undefined;
// 回调已经执行过（本次菜单）：回调接手了焦点（打开对话框 / 面板），之后弹层卸载时的 finalFocus 不能再把焦点抢回“⋯”
let handedOff = false;
export const runAfterMenuClosed = (fn: () => void) => { afterClose = fn; };
/** 菜单关完时调用（下拉和右键菜单各一处）：有登记的回调就执行并返回 true */
export const flushAfterMenuClosed = () => { const run = afterClose; afterClose = undefined; run?.(); if (run) handedOff = true; return !!run; };
/** 菜单的 finalFocus（Base UI 在弹层卸载时调，也会在 effect 清理时调——严格模式、依赖变了都会，所以必须是纯函数，不在这里执行回调）：
 *  登记了关闭后执行的回调（还没执行）就不还焦点；焦点已被别处接走（菜单项弹出的对话框 / 面板）也不还——只在焦点还在菜单里或已落在 body 上时才还：
 *  函数形式的 finalFocus 在 Base UI 眼里是「明确指定的返回目标」，会跳过它自己的「焦点已移走就不抢」检查，得在这里自己做。
 *  target：要还给谁（“⋯”按钮）；不给就走 Base UI 默认（返回 true） */
export function menuReturnFocus(popup: React.RefObject<HTMLElement | null>, target?: () => HTMLElement | null | undefined): boolean | HTMLElement {
  if (afterClose || handedOff) return false;
  const f = document.activeElement;
  if (f && f !== document.body && !popup.current?.contains(f)) return false;
  if (!target) return true;
  const t = target();
  return t?.isConnected ? t : false;
}
/** 菜单的 onOpenChangeComplete：关闭动画播完（弹层卸载前）执行登记的回调，再由对话框 / 面板接走焦点 */
export const menuClosed = (open: boolean) => { if (!open) flushAfterMenuClosed(); };
/** 菜单打开时调用：丢掉上一次没执行掉的回调（菜单没关完整个容器就卸载了，onOpenChangeComplete 不会触发），免得它在之后别的菜单关闭时打开旧对象的面板 */
export const dropAfterMenuClosed = () => { afterClose = undefined; handedOff = false; };

/** 菜单「关完再交接」的全部接线收在这一处，新菜单照着用，不会漏掉其中一件：
 *  - rootProps：onOpenChange（打开时丢掉上次没执行的回调）、onOpenChangeComplete（关完执行登记的回调，menuClosed）→ 展开到 Menu / ContextMenu 根上；
 *  - popupProps：ref（弹层元素，判断焦点还在不在菜单里）、finalFocus（menuReturnFocus，纯函数）→ 展开到 *Content 上。
 *  opts.target：关闭后焦点还给谁（不给 = Base UI 默认的触发元素）；opts.onOpenChange / onOpenChangeComplete：调用方自己的那一份，在接线之后调用。
 *  opts 每次渲染都换新也没关系（存在 ref 里读最新的），返回的函数引用稳定 */
export function useMenuHandoff(opts: { target?: () => HTMLElement | null | undefined; onOpenChange?: (open: boolean) => void; onOpenChangeComplete?: (open: boolean) => void } = {}) {
  const popup = React.useRef<HTMLDivElement | null>(null);
  const o = React.useRef(opts); o.current = opts;
  const finalFocus = React.useCallback(() => menuReturnFocus(popup, o.current.target ? () => o.current.target?.() : undefined), []);
  const onOpenChange = React.useCallback((open: boolean, ..._rest: unknown[]) => { if (open) dropAfterMenuClosed(); o.current.onOpenChange?.(open); }, []);
  const onOpenChangeComplete = React.useCallback((open: boolean) => { menuClosed(open); o.current.onOpenChangeComplete?.(open); }, []);
  return { popup, rootProps: { onOpenChange, onOpenChangeComplete }, popupProps: { ref: popup, finalFocus } };
}

/** btn：“⋯”按钮的选择器；idOf：按钮 → 对象 id；find：按 id 取容器里当前的对象（找不到 = 已不在容器里）。
 *  返回 onClick（挂在容器上，点击从按钮冒泡上来）、cur（打开的那个对象，关闭动画期间是最近的那个）、menu（渲染下拉，菜单项按 cur 给） */
export function useAnchoredDropdown<T>(btn: string, idOf: (el: HTMLElement) => string | undefined, find: (id: string | undefined) => T | undefined) {
  const [at, setAt] = React.useState<{ id: string; el: HTMLElement } | null>(null);
  const [open, setOpen] = React.useState(false);
  const cur = useHeld(open ? at?.id : null, find(at?.id));
  // 打开期间那个对象没了（卡片被筛掉、入口被移除）：菜单跟着收起，不留在一个已经不在页面上的按钮上
  if (open && !cur) setOpen(false);
  // 最近一次点的按钮：finalFocus 在弹层卸载时调，那时 at 可能已被 onOpenChangeComplete 清掉
  const last = React.useRef<HTMLElement | null>(null);
  const openRef = React.useRef(open); openRef.current = open;
  const close = () => { at?.el.setAttribute('aria-expanded', 'false'); setOpen(false); };
  // onOpenChangeComplete：关闭动画播完再撤掉定位锚：不一直挂在最后点过的按钮上（那张卡被筛掉、翻页后成了脱离文档的节点）；期间又点开了别的就不撤
  // 键盘打开（点击的 detail === 0）：把焦点放到第一个可用的菜单项，菜单随之高亮第一项。已经在菜单项上就不动
  const kbOpen = React.useRef(false);
  const focusFirst = () => { if (kbOpen.current && !document.activeElement?.matches('[role="menuitem"]')) h.popup.current?.querySelector<HTMLElement>('[role="menuitem"]:not([data-disabled])')?.focus(); };
  const h = useMenuHandoff({ target: () => last.current, onOpenChange: (o) => { if (!o) close(); }, onOpenChangeComplete: (o) => { if (o) focusFirst(); else { kbOpen.current = false; setAt((x) => (openRef.current ? x : null)); } } });
  // 打开后晚一帧做一次（Base UI 自己的初始聚焦先做完）；上一次的退场动画还没播完时再用键盘打开，弹层不会重新挂载、Base UI 的聚焦时机也不同，
  // 所以动画播完（onOpenChangeComplete(true)）再补一次。看 open 而不是弹层的挂载
  React.useEffect(() => {
    if (open) requestAnimationFrame(focusFirst);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [open]);

  const onClick = (e: React.MouseEvent) => {
    const el = (e.target as Element).closest<HTMLElement>(btn);
    const id = el ? idOf(el) : undefined;
    if (!el || !find(id)) return;
    el.setAttribute('aria-expanded', 'true');
    dropAfterMenuClosed();
    kbOpen.current = e.detail === 0;
    last.current = el;
    setAt({ id: id!, el }); setOpen(true);
  };
  const menu = (label: string, items: React.ReactNode) => (
    <DropdownMenu open={open} {...h.rootProps}>
      {/* 关闭后焦点回到被点的“⋯”（Base UI 默认回到 Trigger，这里没有 Trigger）。规则见 menuReturnFocus（纯函数）；登记的回调在 onOpenChangeComplete 里执行 */}
      <DropdownMenuContent {...h.popupProps} align="end" anchor={at?.el} aria-label={label}>{items}</DropdownMenuContent>
    </DropdownMenu>
  );
  return { onClick, cur, menu, close };
}
