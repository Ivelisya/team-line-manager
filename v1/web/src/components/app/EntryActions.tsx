// 入口操作：停用/启用、轮换凭据（输入 线路机名:端口 确认）、分享链接。节点抽屉的“管理”页与线路页共用。
import * as React from 'react';
import { KeyRound, Link2, MoreHorizontal, Power, PowerOff } from 'lucide-react';
import type { EntryView } from '@/lib/types';
import { fetchShare, useEntryAction } from '@/lib/queries';
import { DropdownMenu, DropdownMenuContent, DropdownMenuItem, DropdownMenuSeparator, DropdownMenuTrigger } from '@/components/ui/overlays';
import { IconButton, Button } from '@/components/ui/button';
import { Dialog, DialogContent, DialogDescription, DialogFooter, DialogTitle, DialogHeader } from '@/components/ui/dialog';
import { Textarea } from '@/components/ui/form';
import { toast } from '@/components/ui/toast';
import { copyText } from '@/lib/clipboard';
import { ConfirmDialog, TypedConfirmDialog } from './dialogs';

export const entryLabel = (e: EntryView) => `${e.relay.name}:${e.port}`;
export const entryDisabled = (e: EntryView) => e.lifecycle === 'disabled';

export function EntryActions({ entry }: { entry: EntryView }) {
  const act = useEntryAction();
  const [dlg, setDlg] = React.useState<null | 'rotate' | 'disable' | 'share'>(null);
  const [url, setUrl] = React.useState('');
  const off = entryDisabled(entry);
  const canAct = entry.lifecycle === 'active' || off;

  const share = async () => {
    try { const r = await fetchShare(entry.id); setUrl(r.url); setDlg('share'); } catch (e) { toast.err(e); }
  };
  return (
    <>
      <DropdownMenu>
        <DropdownMenuTrigger asChild><IconButton label={`入口 ${entryLabel(entry)} 的操作`} size="sm"><MoreHorizontal size={16} /></IconButton></DropdownMenuTrigger>
        <DropdownMenuContent align="end">
          <DropdownMenuItem onSelect={() => void share()} disabled={!canAct}><Link2 />分享链接</DropdownMenuItem>
          {off
            ? <DropdownMenuItem onSelect={() => act.mutate({ id: entry.id, action: 'enable' }, { onSuccess: () => toast.ok('入口已启用'), onError: toast.err })}><Power />启用入口</DropdownMenuItem>
            : <DropdownMenuItem onSelect={() => setDlg('disable')} disabled={!canAct}><PowerOff />停用入口</DropdownMenuItem>}
          <DropdownMenuSeparator />
          <DropdownMenuItem danger onSelect={() => setDlg('rotate')} disabled={!canAct}><KeyRound />轮换凭据…</DropdownMenuItem>
        </DropdownMenuContent>
      </DropdownMenu>

      <ConfirmDialog open={dlg === 'disable'} onOpenChange={(o) => !o && setDlg(null)} title={`停用入口 ${entryLabel(entry)}？`}
        description="停用后该入口立即不再转发流量，已有连接会断开。可随时重新启用。" confirmLabel="停用入口" danger
        onConfirm={async () => { await act.mutateAsync({ id: entry.id, action: 'disable' }); toast.ok('入口已停用'); }} />

      <TypedConfirmDialog open={dlg === 'rotate'} onOpenChange={(o) => !o && setDlg(null)} title="轮换凭据" expected={entryLabel(entry)} confirmLabel="轮换凭据" danger
        description={<>会为入口 <b>{entryLabel(entry)}</b> 生成新的凭据，旧分享链接立即失效，需要重新发给使用者。</>}
        onConfirm={async () => { await act.mutateAsync({ id: entry.id, action: 'rotate', confirm: entryLabel(entry) }); toast.ok('凭据已轮换'); }} />

      <Dialog open={dlg === 'share'} onOpenChange={(o) => !o && setDlg(null)}>
        <DialogContent size="md">
          <DialogHeader><DialogTitle>分享链接 · {entryLabel(entry)}</DialogTitle><DialogDescription>查看分享链接会记入审计。链接等同于凭据，请只发给需要的人。</DialogDescription></DialogHeader>
          <Textarea readOnly value={url} rows={4} className="mono" onFocus={(e) => e.currentTarget.select()} />
          <DialogFooter>
            <Button onClick={() => setDlg(null)}>关闭</Button>
            <Button variant="primary" onClick={() => void copyText(url, '链接')}>复制链接</Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>
    </>
  );
}
