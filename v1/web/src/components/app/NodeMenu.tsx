// 节点菜单：右键菜单和卡片右上角“⋯”下拉共用同一份定义（useNodeMenu），只是渲染成不同的 Radix 组件。
// 菜单内容只在打开时挂载，所以这里的 hook（mutation、路由）不会给每张卡片增加开销。
import * as React from 'react';
import { useNavigate } from 'react-router-dom';
import { ArrowUpCircle, BellOff, Copy, Link2, PanelRightOpen, PencilLine, Power, Repeat2, Undo2 } from 'lucide-react';
import type { MachineView } from '@/lib/types';
import { fetchShare, useUpgradeAgent } from '@/lib/queries';
import { useDrawer } from '@/lib/drawer';
import { openMove, openRestore, openRetire } from '@/lib/nodeActions';
import { copyText } from '@/lib/clipboard';
import { toast } from '@/components/ui/toast';
import { ContextMenuItem, ContextMenuSeparator, ContextMenuSub, ContextMenuSubContent, ContextMenuSubTrigger } from '@/components/ui/context-menu';
import { DropdownMenuItem, DropdownMenuSeparator, DropdownMenuSub, DropdownMenuSubContent, DropdownMenuSubTrigger } from '@/components/ui/overlays';

export type MenuEntry =
  | { key: string; sep: true }
  | { key: string; label: string; icon?: React.ReactNode; onSelect?: () => void; danger?: boolean; disabled?: boolean; children?: MenuEntry[] };

export function useNodeMenu(m: MachineView): MenuEntry[] {
  const { open } = useDrawer();
  const nav = useNavigate();
  const upgrade = useUpgradeAgent();
  const retired = m.lifecycle === 'retired';
  const share = async (entryId: string) => {
    try { const r = await fetchShare(entryId); await copyText(r.url, '分享链接'); } catch (e) { toast.err(e); }
  };
  // 还在应用或正在移除的入口不给分享
  const shareable = m.entries.filter((e) => e.lifecycle !== 'pending' && e.lifecycle !== 'removing');
  const shareItem: MenuEntry = shareable.length > 1
    ? { key: 'share', label: '复制分享链接', icon: <Link2 />, children: shareable.map((e) => ({ key: `share-${e.id}`, label: m.kind === 'relay' ? `${e.port} · ${e.node.name}` : `${e.relay.name}:${e.port}`, onSelect: () => void share(e.id) })) }
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
    { key: 'upgrade', label: '升级 agent', icon: <ArrowUpCircle />, disabled: !m.agent || m.lifecycle !== 'active' || !!m.active_job || upgrade.isPending,
      // 菜单选中后内容会卸载，mutate 的回调届时不再触发；用 Promise 才能保证提示一定出现
      onSelect: () => { upgrade.mutateAsync(m.id).then(() => toast.ok('已提交升级作业'), (e) => toast.err(e)); } },
    { key: 'rename', label: '改名 / 改编号', icon: <PencilLine />, onSelect: () => open(m.id, 'manage') },
    ...(m.kind !== 'relay' && m.lifecycle === 'active' && m.entries.some((e) => e.lifecycle === 'active')
      ? [{ key: 'move', label: '换线路机…', icon: <Repeat2 />, disabled: !!m.active_job, onSelect: () => openMove(m.id) } as MenuEntry] : []),
    { key: 'silence', label: '新建静默', icon: <BellOff />, onSelect: () => nav(`/incidents?silence=${m.id}`) },
    { key: 'sep2', sep: true },
    { key: 'retire', label: '退役…', icon: <Power />, danger: true, disabled: m.lifecycle === 'provisioning' || m.lifecycle === 'retiring', onSelect: () => openRetire(m.id) },
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
              <SubContent sideOffset={6} alignOffset={-4}>
                {it.children.map((c) => ('sep' in c ? null : <Item key={c.key} onSelect={c.onSelect} disabled={c.disabled}>{c.label}</Item>))}
              </SubContent>
            </Sub>
          );
        }
        return <Item key={it.key} danger={it.danger} disabled={it.disabled} onSelect={it.onSelect}>{it.icon}{it.label}</Item>;
      })}
    </>
  );
}

export function NodeContextItems({ m }: { m: MachineView }) {
  return <Render items={useNodeMenu(m)} kit={CTX} />;
}
export function NodeDropdownItems({ m }: { m: MachineView }) {
  return <Render items={useNodeMenu(m)} kit={DD} />;
}
