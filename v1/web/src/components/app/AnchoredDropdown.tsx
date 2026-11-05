// 容器级的“⋯”下拉（节点网格 / 表格的 NodeMenus、入口区的 EntryMenus 共用）：一个容器只挂一个 Base UI 下拉根，卡片和行里只放带 data 属性的“⋯”按钮。
// 按钮被点击（键盘回车 / 空格也是 click）时，把这个按钮当作菜单的定位锚（Positioner 的 anchor）：根上没有 Trigger，菜单按按钮的位置定位，
// 锁滚动让出滚动条槽位、推送改了卡片高度时仍跟着按钮走（floating-ui 的 autoUpdate）。
// 没有 Trigger 就没有“是键盘打开的”这条信息（Base UI 只在 Trigger 的点击里记）：这里自己看点击事件（detail === 0 是键盘 / 读屏触发的），
// 键盘打开时把焦点放到第一个可用的菜单项上（菜单随之高亮第一项）；鼠标 / 触屏打开时焦点留在菜单容器上，方向键再往下走。
import * as React from 'react';
import { useHeld } from '@/lib/useHeld';
import { DropdownMenu, DropdownMenuContent } from '@/components/ui/dropdown-menu';

// 菜单项要把焦点交给菜单外的面板（终端的密码框）时，得等菜单完全关掉再动手：菜单退场动画期间面板先拿到焦点，可能被菜单拉回菜单容器（指针离开菜单项时 Base UI 会 focus 菜单容器），
// 菜单卸载后焦点落在 body、再被下面的 finalFocus 回给“⋯”按钮。登记一个回调，菜单关完（finalFocus，Base UI 在弹层卸载时调）时执行，且不再把焦点回给按钮
let afterClose: (() => void) | undefined;
export const runAfterMenuClosed = (fn: () => void) => { afterClose = fn; };
/** 菜单关完时调用（下拉和右键菜单各一处）：有登记的回调就执行并返回 true */
export const flushAfterMenuClosed = () => { const run = afterClose; afterClose = undefined; run?.(); return !!run; };
/** 菜单打开时调用：丢掉上一次没执行掉的回调（菜单没关完整个容器就卸载了，finalFocus 不会触发），免得它在之后别的菜单关闭时打开旧对象的面板 */
export const dropAfterMenuClosed = () => { afterClose = undefined; };

/** btn：“⋯”按钮的选择器；idOf：按钮 → 对象 id；find：按 id 取容器里当前的对象（找不到 = 已不在容器里）。
 *  返回 onClick（挂在容器上，点击从按钮冒泡上来）、cur（打开的那个对象，关闭动画期间是最近的那个）、menu（渲染下拉，菜单项按 cur 给） */
export function useAnchoredDropdown<T>(btn: string, idOf: (el: HTMLElement) => string | undefined, find: (id: string | undefined) => T | undefined) {
  const [at, setAt] = React.useState<{ id: string; el: HTMLElement } | null>(null);
  const [open, setOpen] = React.useState(false);
  const cur = useHeld(open ? at?.id : null, find(at?.id));
  // 打开期间那个对象没了（卡片被筛掉、入口被移除）：菜单跟着收起，不留在一个已经不在页面上的按钮上
  if (open && !cur) setOpen(false);
  const popup = React.useRef<HTMLDivElement>(null);
  const keyboard = React.useRef(false);
  // 最近一次点的按钮：finalFocus 在弹层卸载时调，那时 at 可能已被 onOpenChangeComplete 清掉
  const last = React.useRef<HTMLElement | null>(null);

  const onClick = (e: React.MouseEvent) => {
    const el = (e.target as Element).closest<HTMLElement>(btn);
    const id = el ? idOf(el) : undefined;
    if (!el || !find(id)) return;
    el.setAttribute('aria-expanded', 'true');
    dropAfterMenuClosed();
    keyboard.current = e.detail === 0;
    last.current = el;
    setAt({ id: id!, el }); setOpen(true);
  };
  const close = () => { at?.el.setAttribute('aria-expanded', 'false'); setOpen(false); };
  const openRef = React.useRef(open); openRef.current = open;
  const menu = (label: string, items: React.ReactNode) => (
    // onOpenChangeComplete：关闭动画播完再撤掉定位锚：不一直挂在最后点过的按钮上（那张卡被筛掉、翻页后成了脱离文档的节点）；期间又点开了别的就不撤
    <DropdownMenu open={open} onOpenChange={(o) => { if (!o) close(); }} onOpenChangeComplete={(o) => { if (!o) setAt((a) => (openRef.current ? a : null)); }}>
      {/* 关闭后焦点回到被点的“⋯”（Base UI 默认回到 Trigger，这里没有 Trigger）。
          菜单项登记了关闭后执行的回调（runAfterMenuClosed）则执行它、不回按钮；焦点已被别处接走（菜单项弹出的对话框）也不抢回来，
          只在焦点还在菜单里（Base UI 在卸载前一刻调这个函数）或已落在 body 上时才回按钮 */}
      <DropdownMenuContent
        ref={(n) => {
          popup.current = n;
          // 键盘打开：菜单挂上之后（Base UI 自己的初始聚焦先做完）把焦点放到第一个可用的菜单项，菜单随之高亮第一项
          if (n && keyboard.current) { keyboard.current = false; requestAnimationFrame(() => n.querySelector<HTMLElement>('[role="menuitem"]:not([data-disabled])')?.focus()); }
        }}
        align="end"
        anchor={at?.el}
        aria-label={label}
        finalFocus={() => {
          if (flushAfterMenuClosed()) return false;
          const f = document.activeElement; const b = last.current;
          return b?.isConnected && (!f || f === document.body || !!popup.current?.contains(f)) ? b : false;
        }}
      >{items}</DropdownMenuContent>
    </DropdownMenu>
  );
  return { onClick, cur, menu, close };
}
