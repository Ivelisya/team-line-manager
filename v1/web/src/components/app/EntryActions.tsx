// 入口操作：停用/启用、开关穿透（确认对话框列出预览）、轮换凭据（输入 线路机名:端口 确认）、分享链接。节点抽屉的“管理”页与线路页共用。
// 菜单、对话框与 mutation 按容器挂一份（EntryMenus：一张线路机卡、抽屉管理页的入口区），行里只放 EntryMenuButton——
// 不要每行挂一个 Radix DropdownMenu 根（各给 document 加一个 keydown 捕获监听）和一个 mutation 观察者：入口几十条的卡随推送重渲染时会一起重画。
import * as React from 'react';
import { Slot } from '@radix-ui/react-slot';
import { ArrowLeftRight, KeyRound, Link2, Loader2, MoreHorizontal, Power, PowerOff } from 'lucide-react';
import type { EntryView } from '@/lib/types';
import { fetchShare, mainShare, useEntryAction, useEntryRouterExits, useReverse, useReversePreview } from '@/lib/queries';
import { isReverse } from '@/lib/derive';
import { DropdownMenuItem, DropdownMenuSeparator } from '@/components/ui/overlays';
import { IconButton, Button } from '@/components/ui/button';
import { Dialog, DialogContent, DialogDescription, DialogFooter, DialogTitle, DialogHeader } from '@/components/ui/dialog';
import { Field, InfoTip, Textarea } from '@/components/ui/form';
import { Note } from './primitives';
import { toast } from '@/components/ui/toast';
import { copyText } from '@/lib/clipboard';
import { ConfirmDialog, TypedConfirmDialog } from './dialogs';
import { useAnchoredDropdown } from './AnchoredDropdown';

export const entryLabel = (e: EntryView) => `${e.relay.name}:${e.port}`;

/** 行里的“⋯”：data-entry-menu 记着入口 id，点击由外面的 EntryMenus 接住。不带悬停提示（菜单打开时不和它叠在一起） */
export function EntryMenuButton({ entry }: { entry: EntryView }) {
  return <IconButton label={`入口「${entryLabel(entry)}」的操作`} tip={false} size="sm" data-entry-menu={entry.id} aria-haspopup="menu"><MoreHorizontal size={16} /></IconButton>;
}

/** 轮换 / 停用之后，指向这个入口的软路由出口就连不上了（不会自动改路由，替换节点、换线路机才会）：对话框里列出来 */
function RouterExitsNote({ entryId, open, verb }: { entryId: string; open: boolean; verb: string }) {
  const q = useEntryRouterExits(entryId, open);
  if (!q.data?.length) return null;
  return (
    <Note tone="warn">{verb}后，这些软路由出口会连不上，需要到路由上手动改：
      <ul className="reason-list">{q.data.map((r) => <li key={r.id}><span><b>{r.name}</b>：{r.exits.join('、')}</span></li>)}</ul>
    </Note>
  );
}

/** 穿透开关的确认对话框：打开时取预览，逐条列出会发生什么、注意、不能切的原因；确认后建作业（推 bridge → 切换 → 经入口验收出口，
 *  验收不过中控自动切回原来的模式），进度在作业页 */
function ReverseDialog({ e, open, onOpenChange }: { e: EntryView; open: boolean; onOpenChange: (o: boolean) => void }) {
  const on = !isReverse(e);
  const pv = useReversePreview(e.id, on, open);
  const go = useReverse();
  const d = pv.data;
  return (
    <ConfirmDialog open={open} onOpenChange={onOpenChange} size="md" title={`${on ? '开启' : '关闭'}穿透 · ${entryLabel(e)}`}
      description={on ? <>由节点主动连这个入口，客户流量经隧道从节点自己的 IP 出去 <InfoTip text="给供应商丢入站连接（SYN）的节点用：线路机不再向节点新建连接。分享链接、订阅、软路由出口都不变。" /></>
        : <>改回线路机连节点的 ssserver</>}
      confirmLabel={on ? '开启穿透' : '关闭穿透'} confirmDisabled={!d?.ok}
      onConfirm={async () => {
        const r = await go.mutateAsync({ id: e.id, on });
        toast.ok(`已提交${on ? '开启' : '关闭'}穿透：${r.entry.name}（作业页看进度）`);
      }}>
      {pv.isLoading && <div className="hint"><Loader2 size={14} className="spin" /> 正在检查…</div>}
      {pv.isError && <Note tone="bad">{(pv.error as Error).message}</Note>}
      {d && <>
        {d.blockers.length > 0 && <Note tone="bad">现在不能切换：<ul className="reason-list">{d.blockers.map((b) => <li key={b}><span>{b}</span></li>)}</ul></Note>}
        <ul className="reason-list">{d.impacts.map((x) => <li key={x}><span>{x}</span></li>)}</ul>
        {d.warnings.map((w) => <Note key={w} tone="warn">{w}</Note>)}
      </>}
    </ConfirmDialog>
  );
}

type Dlg = { k: 'rotate' | 'disable' | 'share' | 'reverse'; e: EntryView; url: string; at?: string | null; open: boolean };

/** 容器级的入口菜单：children（须是 DOM 元素）里的 EntryMenuButton 被点时弹出共享下拉（useAnchoredDropdown，同 NodeMenus 的“⋯”）。
 *  entries：容器里的入口，菜单内容随推送更新 */
export function EntryMenus({ entries, children }: { entries: EntryView[]; children: React.ReactElement }) {
  const act = useEntryAction();
  const find = (id: string | undefined) => (id ? entries.find((e) => e.id === id) : undefined);
  const dd = useAnchoredDropdown('[data-entry-menu]', (el) => el.dataset.entryMenu, find);
  const cur = dd.cur;
  // 对话框第一次打开后才挂载，关闭后保留（关闭动画不被截断）；dlg.e 是打开时的那条入口
  const [dlg, setDlg] = React.useState<Dlg | null>(null);
  const show = (k: Dlg['k'], e: EntryView, url = '', at: string | null = null) => setDlg({ k, e, url, at, open: true });
  const hide = (o: boolean) => { if (!o) setDlg((d) => d && { ...d, open: false }); };
  const share = async (e: EntryView) => {
    try { const r = await fetchShare(e.id); show('share', e, r.url ?? '', r.anytls_url ?? null); } catch (err) { toast.err(err); }
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
        {/* 穿透：只有节点入口（直出没有节点），停用的先启用 */}
        {!cur.direct && <DropdownMenuItem onSelect={() => show('reverse', cur)} disabled={cur.lifecycle !== 'active'}><ArrowLeftRight />{isReverse(cur) ? '关闭穿透…' : '开启穿透…'}</DropdownMenuItem>}
        <DropdownMenuSeparator />
        <DropdownMenuItem danger onSelect={() => show('rotate', cur)} disabled={!canAct}><KeyRound />轮换凭据…</DropdownMenuItem>
      </>)}

      {dlg?.k === 'reverse' && <ReverseDialog e={dlg.e} open={dlg.open} onOpenChange={hide} />}

      {dlg?.k === 'disable' && <ConfirmDialog open={dlg.open} onOpenChange={hide} title={`停用入口 · ${entryLabel(dlg.e)}`}
        description={<>已有连接会断开 <InfoTip text="停用后入口立即停止转发，随时可以重新启用" /></>} confirmLabel="停用入口" danger
        onConfirm={async () => { await act.mutateAsync({ id: dlg.e.id, action: 'disable' }); toast.ok(`入口 ${entryLabel(dlg.e)} 已停用`); }}>
        <RouterExitsNote entryId={dlg.e.id} open={dlg.open} verb="停用" />
      </ConfirmDialog>}

      {dlg?.k === 'rotate' && <TypedConfirmDialog open={dlg.open} onOpenChange={hide} title={`轮换入口凭据 · ${entryLabel(dlg.e)}`} expected={entryLabel(dlg.e)} confirmLabel="轮换凭据" danger
        description={<>旧分享链接会立即失效 <InfoTip text="会生成新的凭据，要把新的分享链接重新发给客户" /></>}
        onConfirm={async () => {
          const e = dlg.e;
          await act.mutateAsync({ id: e.id, action: 'rotate', confirm: entryLabel(e) });
          // 旧链接已失效：通知里直接给新链接，不用再去菜单里找
          toast.show({ tone: 'ok', text: `入口「${entryLabel(e)}」的凭据已轮换，旧链接已失效`, duration: 15000,
            action: { label: '复制新链接', onClick: () => void fetchShare(e.id).then((r) => copyText(mainShare(r), '新分享链接'), toast.err) } });
        }}>
        <RouterExitsNote entryId={dlg.e.id} open={dlg.open} verb="轮换" />
      </TypedConfirmDialog>}

      {dlg?.k === 'share' && <Dialog open={dlg.open} onOpenChange={hide}>
        <DialogContent size="md">
          <DialogHeader><DialogTitle>分享链接 · {entryLabel(dlg.e)}</DialogTitle><DialogDescription>只发给需要的人 <InfoTip text="链接等同于凭据；查看分享链接会记入审计。" /></DialogDescription></DialogHeader>
          {/* 线路机上 AnyTLS 可用时两条：Reality 与 AnyTLS（同一入口、同一份凭据），各自复制；线路机只留 AnyTLS 时只有 AnyTLS 一条 */}
          {dlg.at && dlg.url ? <>
            <Field label="Reality"><Textarea readOnly value={dlg.url} rows={3} className="mono" onFocus={(ev) => ev.currentTarget.select()} /></Field>
            <Field label="AnyTLS" info="证书是线路机自签的，链接里已带跳过校验"><Textarea readOnly value={dlg.at} rows={2} className="mono" onFocus={(ev) => ev.currentTarget.select()} /></Field>
          </> : <Textarea readOnly value={dlg.url || dlg.at || ''} rows={4} className="mono" onFocus={(ev) => ev.currentTarget.select()} />}
          <DialogFooter>
            <Button onClick={() => hide(false)}>关闭</Button>
            {dlg.at && dlg.url && <Button onClick={() => void copyText(dlg.at ?? '', 'AnyTLS 链接')}>复制 AnyTLS</Button>}
            <Button variant="primary" onClick={() => void copyText(dlg.url || dlg.at || '', '链接')}>{dlg.at && dlg.url ? '复制 Reality' : '复制链接'}</Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>}
    </>
  );
}
