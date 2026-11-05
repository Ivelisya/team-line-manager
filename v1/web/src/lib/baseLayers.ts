// 还没换成 Base UI 的 Radix 层（菜单、下拉、日期，B2b 换完后这个文件里与 Radix 相关的两处可删）与 Base UI 对话框 / 命令面板同时存在时要互相让一让。

/** 此刻有没有打开着的 Base UI 对话框 / 命令面板（退场动画中的不算） */
export const baseDialogOpen = () => !!document.querySelector('[data-slot="dialog-content"]:not([data-ending-style]), [data-slot="command-dialog"]:not([data-ending-style])');

/** 此刻有没有打开着的 Radix 模态层（下拉选择 / 菜单）：Radix 的模态层会给 body 加 pointer-events: none，点击落到 html 上，Base UI 的对话框会当成「点了外面」 */
export const radixModalOpen = () => document.body.style.pointerEvents === 'none' || !!document.querySelector('[data-radix-popper-content-wrapper], [data-radix-menu-content]');

/** Radix 菜单的 onCloseAutoFocus：菜单项打开了 Base UI 对话框 / 命令面板，菜单关闭时别把焦点还给「⋯」按钮（会落到模态框外面）。调用方自己的先跑（它可能要执行「菜单关完再打开」的回调） */
export const keepFocusInDialog = (own?: (e: Event) => void) => (e: Event) => { own?.(e); if (!e.defaultPrevented && baseDialogOpen()) e.preventDefault(); };
