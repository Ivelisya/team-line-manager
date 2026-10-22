// 入口操作：停用/启用、轮换凭据（输入 线路机名:端口 确认）、分享链接。节点抽屉的“管理”页与线路页共用。
// 菜单、对话框与 mutation 按容器挂一份（EntryMenus：一张线路机卡、抽屉管理页的入口区），行里只放 EntryMenuButton——
// 以前每行一个 Radix DropdownMenu 根（各给 document 加一个 keydown 捕获监听）和一个 mutation 观察者，入口几十条的卡随推送重渲染时一起重画。
import * as React from 'react';
import { Slot } from '@radix-ui/react-slot';
import { KeyRound, Link2, Loader2, MoreHorizontal, Power, PowerOff } from 'lucide-react';
import type { EntryView } from '@/lib/types';
import { fetchShare, useEntryAction } from '@/lib/queries';
import { DropdownMenuItem, DropdownMenuSeparator } from '@/components/ui/overlays';
import { IconButton, Button } from '@/components/ui/button';
import { Dialog, DialogContent, DialogDescription, DialogFooter, DialogTitle, DialogHeader } from '@/components/ui/dialog';
import { InfoTip, Textarea } from '@/components/ui/form';
import { toast } from '@/components/ui/toast';
import { copyText } from '@/lib/clipboard';
import { ConfirmDialog, TypedConfirmDialog } from './dialogs';
import { useAnchoredDropdown } from './AnchoredDropdown';

export const entryLabel = (e: EntryView) => `${e.relay.name}:${e.port}`;

/** 行里的“⋯”：data-entry-menu 记着入口 id，点击由外面的 EntryMenus 接住。不带悬停提示（菜单打开时不和它叠在一起） */
export function EntryMenuButton({ entry }: { entry: EntryView }) {
  return <IconButton label={`入口 ${entryLabel(entry)} 的操作`} tip={false} size="sm" data-entry-menu={entry.id} aria-haspopup="menu"><MoreHorizontal size={16} /></IconButton>;
}

type Dlg = { k: 'rotate' | 'disable' | 'share'; e: EntryView; url: string; open: boolean };

/** 容器级的入口菜单：children（须是 DOM 元素）里的 EntryMenuButton 被点时弹出共享下拉（useAnchoredDropdown，同 NodeMenus 的“⋯”）。
 *  entries：容器里的入口，菜单内容随推送更新 */
export function EntryMenus({ entries, children }: { entries: EntryView[]; children: React.ReactElement }) {
  const act = useEntryAction();
  const find = (id: string | undefined) => (id ? entries.find((e) => e.id === id) : undefined);
  const dd = useAnchoredDropdown('[data-entry-menu]', (el) => el.dataset.entryMenu, find);
  const cur = dd.cur;
  // 对话框第一次打开后才挂载，关闭后保留（关闭动画不被截断）；dlg.e 是打开时的那条入口
  const [dlg, setDlg] = React.useState<Dlg | null>(null);
  const show = (k: Dlg['k'], e: EntryView, url = '') => setDlg({ k, e, url, open: true });
  const hide = (o: boolean) => { if (!o) setDlg((d) => d && { ...d, open: false }); };
  const share = async (e: EntryView) => {
    try { const r = await fetchShare(e.id); show('share', e, r.url); } catch (err) { toast.err(err); }
  };
  const off = cur?.lifecycle === 'disabled';
  const canAct = cur?.lifecycle === 'active' || off;
  const enabling = act.isPending && act.variables?.action === 'enable' && act.variables.id === cur?.id;

  return (
    <>
      <Slot onClick={dd.onClick}>{children}</Slot>
      {/* 打开对话框的项带「…」；启用不用确认，直接执行：菜单留着、这一项转圈，成功后收起菜单并提示 */}
      {dd.menu('入口操作', cur && <>
        <DropdownMenuItem onSelect={() => void share(cur)} disabled={!canAct}><Link2 />分享链接…</DropdownMenuItem>
        {off
          ? <DropdownMenuItem disabled={enabling} onSelect={(ev) => { ev.preventDefault(); act.mutate({ id: cur.id, action: 'enable' }, { onSuccess: () => { dd.close(); toast.ok(`入口 ${entryLabel(cur)} 已启用`); }, onError: toast.err }); }}>
            {enabling ? <Loader2 className="spin" /> : <Power />}{enabling ? '正在启用…' : '启用入口'}</DropdownMenuItem>
          : <DropdownMenuItem onSelect={() => show('disable', cur)} disabled={!canAct}><PowerOff />停用入口…</DropdownMenuItem>}
        <DropdownMenuSeparator />
        <DropdownMenuItem danger onSelect={() => show('rotate', cur)} disabled={!canAct}><KeyRound />轮换凭据…</DropdownMenuItem>
      </>)}

      {dlg?.k === 'disable' && <ConfirmDialog open={dlg.open} onOpenChange={hide} title={`停用入口 ${entryLabel(dlg.e)}？`}
        description={<>已有连接会断开 <InfoTip text="停用后入口立即不再转发流量；可随时重新启用。" /></>} confirmLabel="停用入口" danger
        onConfirm={async () => { await act.mutateAsync({ id: dlg.e.id, action: 'disable' }); toast.ok(`入口 ${entryLabel(dlg.e)} 已停用`); }} />}

      {dlg?.k === 'rotate' && <TypedConfirmDialog open={dlg.open} onOpenChange={hide} title={`轮换入口 ${entryLabel(dlg.e)} 的凭据？`} expected={entryLabel(dlg.e)} confirmLabel="轮换凭据" danger
        description={<>旧分享链接会立即失效 <InfoTip text="会生成新的凭据，需要把新的分享链接重新发给使用者。" /></>}
        onConfirm={async () => {
          const e = dlg.e;
          await act.mutateAsync({ id: e.id, action: 'rotate', confirm: entryLabel(e) });
          // 旧链接已失效：通知里直接给新链接，不用再去菜单里找
          toast.show({ tone: 'ok', text: `入口 ${entryLabel(e)} 的凭据已轮换，旧链接已失效`, duration: 15000,
            action: { label: '复制新链接', onClick: () => void fetchShare(e.id).then((r) => copyText(r.url, '新分享链接'), toast.err) } });
        }} />}

      {dlg?.k === 'share' && <Dialog open={dlg.open} onOpenChange={hide}>
        <DialogContent size="md">
          <DialogHeader><DialogTitle>分享链接 · {entryLabel(dlg.e)}</DialogTitle><DialogDescription>只发给需要的人 <InfoTip text="链接等同于凭据；查看分享链接会记入审计。" /></DialogDescription></DialogHeader>
          <Textarea readOnly value={dlg.url} rows={4} className="mono" onFocus={(ev) => ev.currentTarget.select()} />
          <DialogFooter>
            <Button onClick={() => hide(false)}>关闭</Button>
            <Button variant="primary" onClick={() => void copyText(dlg.url, '链接')}>复制链接</Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>}
    </>
  );
}
