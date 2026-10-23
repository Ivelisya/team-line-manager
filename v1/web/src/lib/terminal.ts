// 网页终端：打开 / 关闭的状态与票据接口。窗口本身（features/Terminal.tsx，连同 xterm）只在第一次打开时才下载，
// 机器菜单、命令面板只引这个小文件。全站同时只开一个终端窗口；服务端另外限制每人、全中控同时几个（/meta terminal）。
import { createStore } from './store';
import { post } from './api';

/** kind：machine 机器、router 软路由（同一个窗口组件） */
export interface TermTarget { kind: 'machine' | 'router'; id: string; name: string }

const target = createStore<TermTarget | null>(null);
/** 会话在连着时要先问一句才换台或关窗：「close」= 关窗，TermTarget = 换到那一台 */
const asking = createStore<TermTarget | 'close' | null>(null);
export const useTerminalTarget = target.use;
export const useTerminalAsk = asking.use;

// 当前目标与会话是否连着：openTerminal / closeTerminal 在组件外调用，要同步读到，createStore 不给读值，这里另记一份
let cur: TermTarget | null = null;
let live = false;
// 打开终端的地方想在关窗后回到原处（软路由抽屉：为了不被盖住先收起，关掉终端再打开）：pending 是等换台确认的那次，back 是正在显示的这个终端的
/** 窗口在连上 / 断开时上报，决定关窗、换台前要不要确认 */
export const setTerminalLive = (v: boolean) => { live = v; };
let pendingBack: (() => void) | undefined;
let back: (() => void) | undefined;
const show = (t: TermTarget | null) => {
  const was = back;
  back = t ? pendingBack : undefined;
  pendingBack = undefined;
  cur = t; live = false; asking.set(null); target.set(t);
  if (!t) was?.();
};

/** onClosed：这个终端关掉时（只此一次）做的事；换到别的终端不算 */
export function openTerminal(t: TermTarget, onClosed?: () => void) {
  if (cur && cur.kind === t.kind && cur.id === t.id) { back = onClosed ?? back; return; }
  pendingBack = onClosed;
  if (cur && live) asking.set(t); else show(t);
}
/** 关窗：连着时先确认 */
export function closeTerminal() {
  if (live) asking.set('close'); else show(null);
}
/** 确认框里点了「断开」：照问的那样关窗或换台 */
export function confirmTerminalAsk(a: TermTarget | 'close') { show(a === 'close' ? null : a); }
/** 确认框里点了「不换」/ 取消：回到打开前的样子（换台被拒时，打开终端的地方想回到原处，如软路由抽屉；关窗确认没有等着的回调） */
export function cancelTerminalAsk() {
  asking.set(null);
  const b = pendingBack;
  pendingBack = undefined;
  b?.();
}
/** 窗口出错被边界收起时直接清掉，不再确认 */
export const resetTerminal = () => show(null);

/** 换票据：两步验证 / 密码只放在这一次请求里，不留存。票据在有效期内（/meta terminal.ticket_ttl_s）连上 WebSocket 后作为第一条消息发过去 */
export const requestTicket = (body: { kind: TermTarget['kind']; id: string; code?: string; password?: string }) =>
  post<{ ticket: string; expires_at: string }>('/terminal/tickets', body);

/** 票据不放在地址里（反代、中控的请求日志都记不到它），连上后第一条消息 {type: 'auth', ticket, cols, rows} 送过去 */
export const terminalUrl = () => `${location.protocol === 'https:' ? 'wss' : 'ws'}://${location.host}/api/v1/terminal/ws`;
