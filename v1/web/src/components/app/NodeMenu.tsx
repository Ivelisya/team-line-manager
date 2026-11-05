// 节点菜单：右键菜单和卡片右上角“⋯”下拉共用同一份定义（useNodeMenu），只是渲染成不同的 Base UI 组件。
// 菜单内容只在打开时挂载，所以这里的 hook（mutation、路由）不会给每张卡片增加开销。
// 菜单本身由 NodeMenus 按网格 / 表格挂一份，卡片和行里不放菜单根。
import * as React from 'react';
import { useNavigate } from 'react-router-dom';
import { ArrowUpCircle, BellOff, CalendarPlus, Copy, Link2, PanelRightOpen, PencilLine, Power, Repeat2, Replace, SquareTerminal, Wrench } from 'lucide-react';
import type { MachineView } from '@/lib/types';
import { fetchShare, mainShare, maintOwner, useEndMaintenance, useSilences } from '@/lib/queries';
import { useDrawer } from '@/lib/drawer';
import { openMaint, openMove, openRenew, openReplace, openRetire, openUpgrade } from '@/lib/nodeActions';
import { collator } from '@/lib/sort';
import { openTerminal } from '@/lib/terminal';
import { copyShareLink, copyText } from '@/lib/clipboard';
import { useHeld } from '@/lib/useHeld';
import { toast } from '@/components/ui/toast';
import { ContextMenu, ContextMenuContent, ContextMenuItem, ContextMenuSeparator, ContextMenuSub, ContextMenuSubContent, ContextMenuSubTrigger, ContextMenuTrigger } from '@/components/ui/context-menu';
import { DropdownMenuItem, DropdownMenuSeparator, DropdownMenuSub, DropdownMenuSubContent, DropdownMenuSubTrigger } from '@/components/ui/dropdown-menu';
import { dropAfterMenuClosed, flushAfterMenuClosed, runAfterMenuClosed, useAnchoredDropdown } from './AnchoredDropdown';

export type MenuEntry =
  | { key: string; sep: true }
  | { key: string; label: string; icon?: React.ReactNode; onSelect?: () => void; danger?: boolean; disabled?: boolean; why?: string; children?: MenuEntry[]; filter?: string };
/** 子菜单超过这么多项时顶上给一个筛选框（线路机的分享链接一台几十条） */
const FILTER_MIN = 8;

type MenuItem = Exclude<MenuEntry, { sep: true }>;
/** 把几项收进一个子菜单（项数多了菜单太高）；只有一项（或没有）时不收，直接放出来（保留它自己的图标） */
const group = (key: string, label: string, icon: React.ReactNode, list: MenuItem[]): MenuEntry[] =>
  list.length < 2 ? list : [{ key, label, icon, children: list }];

function useNodeMenu(m: MachineView): MenuEntry[] {
  const { open } = useDrawer();
  const nav = useNavigate();
  const endMaint = useEndMaintenance();
  // 菜单内容只在打开时挂载，这里取静默列表不给每张卡片加开销；同机 IP 节点因宿主机维护而「维护中」时，结束的是宿主机的
  const silences = useSilences().data;
  const maintOnHost = m.maintenance && !!silences && maintOwner(silences, m).onHost;
  const retired = m.lifecycle === 'retired';
  const share = async (entryId: string) => {
    try { const r = await fetchShare(entryId); await copyShareLink(mainShare(r)); } catch (e) { toast.err(e); }
  };
  // 还在应用或正在移除的入口不给分享
  const shareable = m.entries.filter((e) => e.lifecycle !== 'pending' && e.lifecycle !== 'removing');
  // 线路机按节点名排（同名按端口），节点按线路机名
  const shareLabel = (e: (typeof shareable)[number]) => (m.kind === 'relay' ? `${e.node.name} · ${e.port}` : `${e.relay.name}:${e.port}`);
  const shareItem: MenuEntry = shareable.length > 1
    ? { key: 'share', label: '复制分享链接', icon: <Link2 />, filter: shareable.length > FILTER_MIN ? '筛选节点…' : undefined,
      children: shareable.map((e) => ({ key: `share-${e.id}`, label: shareLabel(e), onSelect: () => void share(e.id) })).sort((a, b) => collator.compare(a.label, b.label)) }
    : { key: 'share', label: '复制分享链接', icon: <Link2 />, disabled: shareable.length === 0 || retired, why: '没有可分享的入口', onSelect: () => void share(shareable[0].id) };
  const items: MenuEntry[] = [
    { key: 'open', label: '打开详情', icon: <PanelRightOpen />, onSelect: () => open(m.id) },
    ...group('copy', '复制', <Copy />, [
      { key: 'ip', label: 'IP', onSelect: () => void copyText(m.ip, ' IP', true) },
      { key: 'name', label: '名称', onSelect: () => void copyText(m.name, '名称', true) },
    ]),
  ];
  if (retired) return items; // 退役作业刚做完、还没移入回收站的一瞬间
  items.push(shareItem, { key: 'sep1', sep: true });
  items.push(
    // 家宽代理没有主机可登录；同机 IP 节点没有自己的 SSH，中控连到它的宿主机
    ...(m.kind !== 'proxy' && m.lifecycle === 'active'
      ? [{ key: 'term', label: m.host_machine ? '打开终端（宿主机）' : '打开终端', icon: <SquareTerminal />, onSelect: () => runAfterMenuClosed(() => openTerminal({ kind: 'machine', id: m.id, name: m.name })) } as MenuEntry] : []),
    // 同机 IP 节点没有自己的 agent（用宿主机的），不给升级
    // 升级会重启 agent、作业开始后不能取消：先弹确认（外壳里的 UpgradeDialog）。中控不报「当前 agent 版本」，判断不了是否已是最新
    ...(m.host_machine ? [] : [{ key: 'upgrade', label: '升级 agent…', icon: <ArrowUpCircle />, disabled: !m.agent || m.lifecycle !== 'active' || !!m.active_job,
      why: !m.agent ? (m.kind === 'proxy' ? '家宽代理没有 agent' : '这台还没有 agent') : m.lifecycle !== 'active' ? '只有在用的机器能升级' : '有作业正在进行，等它结束',
      onSelect: () => openUpgrade(m.id) } as MenuEntry]),
    { key: 'rename', label: '改名 / 改编号', icon: <PencilLine />, onSelect: () => open(m.id, 'manage') },
    // 任何机器都能登记续费（提前续、按年续都行，不只是快到期的）；还没有订阅的先去「费用」页填
    m.subscription
      ? { key: 'renew', label: '登记续费…', icon: <CalendarPlus />, onSelect: () => openRenew(m.id) }
      : { key: 'renew', label: '设置费用…', icon: <CalendarPlus />, onSelect: () => open(m.id, 'billing') },
    // 换线路机、替换：只有一项可用时直接放出来，两项都有时收进「更换」
    ...group('swap', '更换', <Repeat2 />, [
      ...(m.kind !== 'relay' && m.lifecycle === 'active' && m.entries.some((e) => e.lifecycle === 'active')
        ? [{ key: 'move', label: '换线路机…', icon: <Repeat2 />, disabled: !!m.active_job, why: '有作业正在进行，等它结束', onSelect: () => openMove(m.id) } as MenuItem] : []),
      // 替换：另一台在用节点接替它（软路由出口跟着换，编号交换，它进备用团队）
      ...(m.kind !== 'relay' && m.lifecycle === 'active'
        ? [{ key: 'replace', label: '替换为…', icon: <Replace />, disabled: !!m.active_job, why: '有作业正在进行，等它结束', onSelect: () => openReplace(m.id) } as MenuItem] : []),
    ]),
    // 维护中的：结束这台自己的（同机 IP 节点可能是宿主机的）维护窗口；全部机器的那种要到告警页结束，mutation 会报错说明
    ...group('upkeep', '维护与静默', <Wrench />, [
      m.maintenance
        ? { key: 'maint', label: maintOnHost ? '结束宿主机维护' : '结束维护', icon: <Wrench />,
            onSelect: () => { endMaint.mutateAsync(m).then((o) => toast.ok(`${o.name} 已结束维护`), (e) => toast.err(e)); } }
        : { key: 'maint', label: '维护窗口…', icon: <Wrench />, onSelect: () => openMaint(m.id) },
      { key: 'silence', label: '新建静默…', icon: <BellOff />, onSelect: () => nav(`/incidents?silence=${m.id}`) },
    ]),
    { key: 'sep2', sep: true },
    { key: 'retire', label: m.spare && m.lifecycle === 'active' ? '立即回收…' : '退役…', icon: <Power />, danger: true, disabled: m.lifecycle === 'retiring' || (m.lifecycle === 'provisioning' && !!m.active_job), why: m.lifecycle === 'retiring' ? '正在退役' : '上架作业还在进行，等它结束', onSelect: () => openRetire(m.id) },
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
              <SubTrigger disabled={it.disabled} data-tip={it.disabled ? it.why : undefined}>{it.icon}{it.label}</SubTrigger>
              <SubContent sideOffset={6} alignOffset={-4}><SubItems items={it.children} filter={it.filter} Item={Item} /></SubContent>
            </Sub>
          );
        }
        return <Item key={it.key} danger={it.danger} disabled={it.disabled} data-tip={it.disabled ? it.why : undefined} onClick={it.onSelect}>{it.icon}{it.label}</Item>;
      })}
    </>
  );
}

/** 子菜单里能选的项（筛选框之外的菜单项） */
const firstItem = (from: HTMLElement) => from.closest('[role="menu"]')?.querySelector<HTMLElement>('[role="menuitem"]:not([data-disabled])');

/** 子菜单的项；filter 给了就在顶上放一个筛选框（按项的文字包含筛）。子菜单一打开（键盘 → 也是）焦点就在框里；
 *  框里打字不触发菜单的按字母跳转，↓ 移到第一项，回车选中第一条匹配的。
 *  Base UI 只在焦点落在菜单项或菜单本身时响应方向键 / 按字母跳转（框里按的到不了它），键盘打开子菜单时还会把焦点放到第一项上：都在这里接过来 */
function SubItems({ items, filter, Item }: { items: MenuEntry[]; filter?: string; Item: React.ElementType }) {
  const [q, setQ] = React.useState('');
  const input = React.useRef<HTMLInputElement>(null);
  // 等 Base UI 打开时的自动聚焦（菜单容器 → 第一项）做完再接过焦点
  React.useEffect(() => { const h = setTimeout(() => input.current?.focus({ preventScroll: true })); return () => clearTimeout(h); }, []);
  const t = q.trim().toLowerCase();
  const shown = items.filter((c): c is Exclude<MenuEntry, { sep: true }> => !('sep' in c) && (!t || c.label.toLowerCase().includes(t)));
  const onKey = (e: React.KeyboardEvent<HTMLInputElement>) => {
    if (e.key === 'Escape' || e.key === 'Tab') return; // 交给菜单：Esc 关闭
    e.stopPropagation();
    if (e.key === 'ArrowDown' || e.key === 'Enter') {
      e.preventDefault();
      const el = firstItem(e.currentTarget);
      // 回车：点第一项，走菜单项自己的选中（执行并关掉整个菜单）
      if (e.key === 'Enter') el?.click(); else el?.focus();
    }
  };
  return (
    <>
      {filter && <div className="menu-filter"><input ref={input} value={q} onChange={(e) => setQ(e.target.value)} placeholder={filter} aria-label={filter} onKeyDown={onKey} /></div>}
      {shown.map((c) => <Item key={c.key} onClick={c.onSelect} disabled={c.disabled} data-tip={c.disabled ? c.why : undefined}>{c.icon}{c.label}</Item>)}
      {filter && !shown.length && <div className="px-2.5 py-1.5 text-label text-fg-2">没有匹配的</div>}
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
 *  不要给每张卡、每行各挂一对菜单根：每个根都给 document 加一个 keydown 捕获监听，卡片随推送重渲染时也跟着重画。
 *  右键：按最近的 [data-id] 找到机器，Base UI ContextMenu 照常定位在指针处（触屏长按、Shift+F10 也照常）；
 *  “⋯”：卡片 / 行里带 data-node-menu 的按钮，锚定共享下拉见 useAnchoredDropdown。
 *  rows：容器里渲染着的机器，菜单内容随推送更新 */
export function NodeMenus({ rows, children }: { rows: MachineView[]; children: React.ReactElement }) {
  const find = (id: string | null | undefined) => (id ? rows.find((m) => m.id === id) : undefined);
  const picked = React.useRef<HTMLElement | null>(null);
  const [ctxId, setCtxId] = React.useState<string | null>(null);
  // 关闭动画期间继续用最近的那台渲染菜单
  const ctxM = useHeld(ctxId, find(ctxId));
  const dd = useAnchoredDropdown('[data-node-menu]', (el) => el.closest<HTMLElement>('[data-id]')?.dataset.id, find);

  // 右键（触屏是长按，Base UI 从 touchstart 起计时）先于 Base UI 记下是哪张卡；落在卡片之间的空隙不弹（preventBaseUIHandler 让 Base UI 自己的处理跳过：
  // 触发区上的 props 先于 Base UI 的处理函数执行，调用它就拦住了打开；原生右键菜单照样被 Base UI 的文档级 contextmenu 监听压掉）
  const pick = (e: React.SyntheticEvent & { preventBaseUIHandler?: () => void }) => {
    const el = (e.target as Element).closest<HTMLElement>('[data-id]');
    if (el && find(el.dataset.id)) picked.current = el; else e.preventBaseUIHandler?.();
  };
  // 打开期间给那张卡 / 那一行写 data-menu（高亮，见 cards.css、table.css）
  const onCtx = (open: boolean) => {
    const el = picked.current;
    if (open) dropAfterMenuClosed();
    el?.toggleAttribute('data-menu', open);
    setCtxId(open && el ? el.dataset.id! : null);
  };
  return (
    <>
      <ContextMenu onOpenChange={onCtx}>
        <ContextMenuTrigger render={children} onContextMenu={pick} onTouchStart={pick} onClick={dd.onClick} />
        <ContextMenuContent finalFocus={() => !flushAfterMenuClosed()}>{ctxM && <NodeContextItems m={ctxM} />}</ContextMenuContent>
      </ContextMenu>
      {dd.menu('更多操作', dd.cur && <NodeDropdownItems m={dd.cur} />)}
    </>
  );
}
