// 节点菜单：右键菜单和卡片右上角“⋯”下拉共用同一份定义（useNodeMenu），只是渲染成不同的 Radix 组件。
// 菜单内容只在打开时挂载，所以这里的 hook（mutation、路由）不会给每张卡片增加开销。
// 菜单本身由 NodeMenus 按网格 / 表格挂一份，卡片和行里不放 Radix 根。
import * as React from 'react';
import { useNavigate } from 'react-router-dom';
import { ArrowUpCircle, BellOff, CalendarPlus, Copy, Link2, PanelRightOpen, PencilLine, Power, Repeat2, SquareTerminal, Undo2, Wrench } from 'lucide-react';
import type { MachineView } from '@/lib/types';
import { fetchShare, maintOwner, useEndMaintenance, useSilences, useUpgradeAgent } from '@/lib/queries';
import { useDrawer } from '@/lib/drawer';
import { openMaint, openMove, openRenew, openRestore, openRetire } from '@/lib/nodeActions';
import { collator } from '@/lib/sort';
import { openTerminal } from '@/lib/terminal';
import { copyShareLink, copyText } from '@/lib/clipboard';
import { useHeld } from '@/lib/useHeld';
import { toast } from '@/components/ui/toast';
import { ContextMenu, ContextMenuContent, ContextMenuItem, ContextMenuSeparator, ContextMenuSub, ContextMenuSubContent, ContextMenuSubTrigger, ContextMenuTrigger } from '@/components/ui/context-menu';
import { DropdownMenuItem, DropdownMenuSeparator, DropdownMenuSub, DropdownMenuSubContent, DropdownMenuSubTrigger } from '@/components/ui/overlays';
import { useAnchoredDropdown } from './AnchoredDropdown';

export type MenuEntry =
  | { key: string; sep: true }
  | { key: string; label: string; icon?: React.ReactNode; onSelect?: () => void; danger?: boolean; disabled?: boolean; children?: MenuEntry[]; filter?: string };
/** 子菜单超过这么多项时顶上给一个筛选框（线路机的分享链接一台几十条） */
const FILTER_MIN = 8;

function useNodeMenu(m: MachineView): MenuEntry[] {
  const { open } = useDrawer();
  const nav = useNavigate();
  const upgrade = useUpgradeAgent();
  const endMaint = useEndMaintenance();
  // 菜单内容只在打开时挂载，这里取静默列表不给每张卡片加开销；同机 IP 节点因宿主机维护而「维护中」时，结束的是宿主机的
  const silences = useSilences().data;
  const maintOnHost = m.maintenance && !!silences && maintOwner(silences, m).onHost;
  const retired = m.lifecycle === 'retired';
  const share = async (entryId: string) => {
    try { const r = await fetchShare(entryId); await copyShareLink(r.url); } catch (e) { toast.err(e); }
  };
  // 还在应用或正在移除的入口不给分享
  const shareable = m.entries.filter((e) => e.lifecycle !== 'pending' && e.lifecycle !== 'removing');
  // 线路机按节点名排（同名按端口），节点按线路机名
  const shareLabel = (e: (typeof shareable)[number]) => (m.kind === 'relay' ? `${e.node.name} · ${e.port}` : `${e.relay.name}:${e.port}`);
  const shareItem: MenuEntry = shareable.length > 1
    ? { key: 'share', label: '复制分享链接', icon: <Link2 />, filter: shareable.length > FILTER_MIN ? '筛选节点…' : undefined,
      children: shareable.map((e) => ({ key: `share-${e.id}`, label: shareLabel(e), onSelect: () => void share(e.id) })).sort((a, b) => collator.compare(a.label, b.label)) }
    : { key: 'share', label: '复制分享链接', icon: <Link2 />, disabled: shareable.length === 0 || retired, onSelect: () => void share(shareable[0].id) };
  const items: MenuEntry[] = [
    { key: 'open', label: '打开详情', icon: <PanelRightOpen />, onSelect: () => open(m.id) },
    { key: 'ip', label: '复制 IP', icon: <Copy />, onSelect: () => void copyText(m.ip, ' IP') },
    { key: 'name', label: '复制名称', icon: <Copy />, onSelect: () => void copyText(m.name, '名称') },
  ];
  if (retired) {
    items.push({ key: 'sep1', sep: true }, { key: 'restore', label: '恢复…', icon: <Undo2 />, onSelect: () => openRestore(m.id) });
    return items;
  }
  items.push(shareItem, { key: 'sep1', sep: true });
  items.push(
    // 家宽代理没有主机可登录；同机 IP 节点没有自己的 SSH，中控连到它的宿主机
    ...(m.kind !== 'proxy' && m.lifecycle === 'active'
      ? [{ key: 'term', label: m.host_machine ? '打开终端（宿主机）' : '打开终端', icon: <SquareTerminal />, onSelect: () => openTerminal({ kind: 'machine', id: m.id, name: m.name }) } as MenuEntry] : []),
    // 同机 IP 节点没有自己的 agent（用宿主机的），不给升级
    ...(m.host_machine ? [] : [{ key: 'upgrade', label: '升级 agent', icon: <ArrowUpCircle />, disabled: !m.agent || m.lifecycle !== 'active' || !!m.active_job || upgrade.isPending,
      // 菜单选中后内容会卸载，mutate 的回调届时不再触发；用 Promise 才能保证提示一定出现
      onSelect: () => { upgrade.mutateAsync(m.id).then(() => toast.ok('已提交升级作业'), (e) => toast.err(e)); } } as MenuEntry]),
    { key: 'rename', label: '改名 / 改编号', icon: <PencilLine />, onSelect: () => open(m.id, 'manage') },
    // 任何机器都能登记续费（提前续、按年续都行，不只是快到期的）；还没有订阅的先去「费用」页填
    m.subscription
      ? { key: 'renew', label: '登记续费…', icon: <CalendarPlus />, onSelect: () => openRenew(m.id) }
      : { key: 'renew', label: '设置费用…', icon: <CalendarPlus />, onSelect: () => open(m.id, 'billing') },
    ...(m.kind !== 'relay' && m.lifecycle === 'active' && m.entries.some((e) => e.lifecycle === 'active')
      ? [{ key: 'move', label: '换线路机…', icon: <Repeat2 />, disabled: !!m.active_job, onSelect: () => openMove(m.id) } as MenuEntry] : []),
    // 维护中的：结束这台自己的（同机 IP 节点可能是宿主机的）维护窗口；全部机器的那种要到告警页结束，mutation 会报错说明
    m.maintenance
      ? { key: 'maint', label: maintOnHost ? '结束宿主机维护' : '结束维护', icon: <Wrench />,
          onSelect: () => { endMaint.mutateAsync(m).then((o) => toast.ok(`${o.name} 已结束维护`), (e) => toast.err(e)); } }
      : { key: 'maint', label: '维护窗口…', icon: <Wrench />, onSelect: () => openMaint(m.id) },
    { key: 'silence', label: '新建静默', icon: <BellOff />, onSelect: () => nav(`/incidents?silence=${m.id}`) },
    { key: 'sep2', sep: true },
    { key: 'retire', label: '退役…', icon: <Power />, danger: true, disabled: m.lifecycle === 'retiring' || (m.lifecycle === 'provisioning' && !!m.active_job), onSelect: () => openRetire(m.id) },
  );
  return items;
}

interface Kit {
  Item: React.ElementType; Sep: React.ElementType; Sub: React.ElementType; SubTrigger: React.ElementType; SubContent: React.ElementType;
}
const CTX: Kit = { Item: ContextMenuItem, Sep: ContextMenuSeparator, Sub: ContextMenuSub, SubTrigger: ContextMenuSubTrigger, SubContent: ContextMenuSubContent };
const DD: Kit = { Item: DropdownMenuItem, Sep: DropdownMenuSeparator, Sub: DropdownMenuSub, SubTrigger: DropdownMenuSubTrigger, SubContent: DropdownMenuSubContent };

function Render({ items, kit }: { items: MenuEntry[]; kit: Kit }) {
  const { Item, Sep, Sub, SubTrigger, SubContent } = kit;
  return (
    <>
      {items.map((it) => {
        if ('sep' in it) return <Sep key={it.key} />;
        if (it.children) {
          return (
            <Sub key={it.key}>
              <SubTrigger disabled={it.disabled}>{it.icon}{it.label}</SubTrigger>
              <SubContent sideOffset={6} alignOffset={-4}><SubItems items={it.children} filter={it.filter} Item={Item} /></SubContent>
            </Sub>
          );
        }
        return <Item key={it.key} danger={it.danger} disabled={it.disabled} onSelect={it.onSelect}>{it.icon}{it.label}</Item>;
      })}
    </>
  );
}

/** 子菜单的项；filter 给了就在顶上放一个筛选框（按项的文字包含筛）。框里打字不触发菜单的按字母跳转，上下键照常移到菜单项 */
function SubItems({ items, filter, Item }: { items: MenuEntry[]; filter?: string; Item: React.ElementType }) {
  const [q, setQ] = React.useState('');
  const t = q.trim().toLowerCase();
  const shown = items.filter((c): c is Exclude<MenuEntry, { sep: true }> => !('sep' in c) && (!t || c.label.toLowerCase().includes(t)));
  return (
    <>
      {filter && <div className="menu-filter"><input value={q} onChange={(e) => setQ(e.target.value)} placeholder={filter} aria-label={filter}
        onKeyDown={(e) => { if (!['ArrowDown', 'ArrowUp', 'Escape', 'Tab'].includes(e.key)) e.stopPropagation(); }} /></div>}
      {shown.map((c) => <Item key={c.key} onSelect={c.onSelect} disabled={c.disabled}>{c.label}</Item>)}
      {filter && !shown.length && <div className="menu-label">没有匹配的</div>}
    </>
  );
}

function NodeContextItems({ m }: { m: MachineView }) {
  return <Render items={useNodeMenu(m)} kit={CTX} />;
}
function NodeDropdownItems({ m }: { m: MachineView }) {
  return <Render items={useNodeMenu(m)} kit={DD} />;
}

/** 网格 / 表格级的节点菜单：整个容器（children，须是 DOM 元素）只挂一个右键菜单和一个“⋯”下拉。
 *  以前每张卡、每行各一对 Radix 菜单根：每个根都给 document 加一个 keydown 捕获监听，卡片随推送重渲染时也跟着重画。
 *  右键：按最近的 [data-id] 找到机器，Radix ContextMenu 照常定位在指针处（触屏长按、Shift+F10 也照常）；
 *  “⋯”：卡片 / 行里带 data-node-menu 的按钮，锚定共享下拉见 useAnchoredDropdown。
 *  rows：容器里渲染着的机器，菜单内容随推送更新 */
export function NodeMenus({ rows, children }: { rows: MachineView[]; children: React.ReactElement }) {
  const find = (id: string | null | undefined) => (id ? rows.find((m) => m.id === id) : undefined);
  const picked = React.useRef<HTMLElement | null>(null);
  const [ctxId, setCtxId] = React.useState<string | null>(null);
  // 关闭动画期间继续用最近的那台渲染菜单
  const ctxM = useHeld(ctxId, find(ctxId));
  const dd = useAnchoredDropdown('[data-node-menu]', (el) => el.closest<HTMLElement>('[data-id]')?.dataset.id, find);

  // 右键（触屏是长按，Radix 从 pointerdown 起计时）先于 Radix 记下是哪张卡；落在卡片之间的空隙不弹（preventDefault 让 Radix 跳过）
  const pick = (e: React.SyntheticEvent) => {
    const el = (e.target as Element).closest<HTMLElement>('[data-id]');
    if (el && find(el.dataset.id)) picked.current = el; else e.preventDefault();
  };
  // 打开期间给那张卡 / 那一行写 data-menu（高亮，见 cards.css、table.css）
  const onCtx = (open: boolean) => {
    const el = picked.current;
    el?.toggleAttribute('data-menu', open);
    setCtxId(open && el ? el.dataset.id! : null);
  };
  return (
    <>
      <ContextMenu onOpenChange={onCtx}>
        <ContextMenuTrigger asChild onContextMenu={pick} onPointerDown={pick} onClick={dd.onClick}>{children}</ContextMenuTrigger>
        <ContextMenuContent>{ctxM && <NodeContextItems m={ctxM} />}</ContextMenuContent>
      </ContextMenu>
      {dd.menu('更多操作', dd.cur && <NodeDropdownItems m={dd.cur} />)}
    </>
  );
}
