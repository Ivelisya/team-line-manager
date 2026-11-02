// 容器级的“⋯”下拉（节点网格 / 表格的 NodeMenus、入口区的 EntryMenus 共用）：一个容器只挂一个 Radix 下拉根，卡片和行里只放带 data 属性的“⋯”按钮。
// 按钮被点击（键盘回车 / 空格也是 click）时，往这个按钮里插一个盖满它的透明定位框当触发元素
// （插在按钮里而不是按坐标放：锁滚动让出滚动条槽位、推送改了卡片高度时，菜单仍跟着按钮走）。
// 下拉根随容器常驻，只有定位框跟着被点的按钮走：Radix 在根挂载之后才开始记“是不是键盘操作”，根若等点击时才挂载，
// 回车打开的菜单焦点会停在菜单容器上，落不到第一项。
import * as React from 'react';
import { createPortal } from 'react-dom';
import { useHeld } from '@/lib/useHeld';
import { DropdownMenu, DropdownMenuContent, DropdownMenuTrigger } from '@/components/ui/overlays';

// 菜单项要把焦点交给菜单外的面板（终端的密码框）时，得等菜单完全关掉再动手：菜单退场动画期间面板先拿到焦点，可能被菜单拉回菜单容器（指针离开菜单项时 Radix 会 focus 菜单容器；此时焦点圈已解除），
// 菜单卸载后焦点落在 body、再被下面的 onCloseAutoFocus 回给“⋯”按钮。登记一个回调，菜单关完（onCloseAutoFocus）时执行，且不再把焦点回给按钮
let afterClose: (() => void) | undefined;
export const runAfterMenuClosed = (fn: () => void) => { afterClose = fn; };
/** 菜单关完时调用（下拉和右键菜单各一处）：有登记的回调就执行并返回 true */
export const flushAfterMenuClosed = () => { const run = afterClose; afterClose = undefined; run?.(); return !!run; };

/** btn：“⋯”按钮的选择器；idOf：按钮 → 对象 id；find：按 id 取容器里当前的对象（找不到 = 已不在容器里）。
 *  返回 onClick（挂在容器上，点击从按钮冒泡上来）、cur（打开的那个对象，关闭动画期间是最近的那个）、menu（渲染下拉，菜单项按 cur 给） */
export function useAnchoredDropdown<T>(btn: string, idOf: (el: HTMLElement) => string | undefined, find: (id: string | undefined) => T | undefined) {
  const [at, setAt] = React.useState<{ id: string; el: HTMLElement } | null>(null);
  const [open, setOpen] = React.useState(false);
  const cur = useHeld(open ? at?.id : null, find(at?.id));
  // 打开期间那个对象没了（卡片被筛掉、入口被移除）：菜单跟着收起，不留在一个已经不在页面上的按钮上
  if (open && !cur) setOpen(false);

  const onClick = (e: React.MouseEvent) => {
    const el = (e.target as Element).closest<HTMLElement>(btn);
    const id = el ? idOf(el) : undefined;
    if (!el || !find(id)) return;
    el.setAttribute('aria-expanded', 'true');
    setAt({ id: id!, el }); setOpen(true);
  };
  const close = () => { at?.el.setAttribute('aria-expanded', 'false'); setOpen(false); };
  const openRef = React.useRef(open); openRef.current = open;
  const menu = (label: string, items: React.ReactNode) => (
    <DropdownMenu open={open} onOpenChange={(o) => { if (!o) close(); }}>
      {at && createPortal(<DropdownMenuTrigger asChild><span className="menu-anchor" aria-label={label} /></DropdownMenuTrigger>, at.el)}
      {/* 关闭后焦点回到被点的“⋯”（Radix 默认回到触发元素，这里的触发元素是看不见的定位框）。
          焦点已被别处接走（菜单项弹出的对话框）就不抢回来，只在焦点落在 body 上时才回按钮；菜单项登记了关闭后执行的回调（runAfterMenuClosed）则执行它、不回按钮。
          关闭动画播完再撤掉定位框：不一直挂在最后点过的按钮里（那张卡被筛掉、翻页后成了脱离文档的节点）；期间又点开了别的就不撤 */}
      <DropdownMenuContent align="end" onCloseAutoFocus={(e) => { e.preventDefault(); const f = document.activeElement; if (flushAfterMenuClosed()) { /* 焦点交给回调打开的面板 */ } else if (at?.el.isConnected && (!f || f === document.body)) at.el.focus(); setAt((a) => (openRef.current ? a : null)); }}>{items}</DropdownMenuContent>
    </DropdownMenu>
  );
  return { onClick, cur, menu, close };
}
