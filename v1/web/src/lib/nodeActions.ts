// 卡片/表格菜单、命令面板触发的“要弹对话框”的动作：状态放在这里，由外壳里唯一一份 LifecycleDialogsHost 渲染对话框，
// 这样几十张卡片不用各带一份对话框和 mutation。
import { createStore } from './store';

/** 一个“目标机器 id”：open 设置、close 清空，use 订阅 */
function target() {
  const s = createStore<string | null>(null);
  return { open: (id: string) => s.set(id), close: () => s.set(null), use: s.use };
}

const retire = target();
export const openRetire = retire.open;
export const closeRetire = retire.close;
export const useRetireTarget = retire.use;

/** 换线路机（目标是节点） */
const move = target();
export const openMove = move.open;
export const closeMove = move.close;
export const useMoveTarget = move.use;

/** 搬空线路机（目标是线路机） */
const evacuate = target();
export const openEvacuate = evacuate.open;
export const closeEvacuate = evacuate.close;
export const useEvacuateTarget = evacuate.use;

/** 恢复已退役的机器 */
const restore = target();
export const openRestore = restore.open;
export const closeRestore = restore.close;
export const useRestoreTarget = restore.use;

/** 登记续费（任何有订阅的机器，不只是快到期的） */
const renew = target();
export const openRenew = renew.open;
export const closeRenew = renew.close;
export const useRenewTarget = renew.use;
