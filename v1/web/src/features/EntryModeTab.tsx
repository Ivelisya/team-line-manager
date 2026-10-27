// 设置「入口」：新入口默认模式（普通 / 穿透）。上架、加入口不指定模式时用它；没设过是穿透（新上架的多是入站质量差的 AT&T 系机器）。
// 只影响之后新建的入口：存量入口在入口菜单（或 fleetctl entry reverse）里逐个切。家宽代理、同机 IP 节点不支持穿透，按普通建。
import * as React from 'react';
import { useEntryModeSetting, useSaveEntryMode } from '@/lib/queries';
import type { EntryMode } from '@/lib/types';
import { Button } from '@/components/ui/button';
import { Field, InfoTip } from '@/components/ui/form';
import { Select } from '@/components/ui/select';
import { toast } from '@/components/ui/toast';
import { ConfirmDialog } from '@/components/app/dialogs';
import { ErrorNote, Note, Panel } from '@/components/app/primitives';
import { SkeletonRows } from '@/components/ui/skeleton';

export const ENTRY_MODE_OPTIONS = [
  { value: 'reverse', label: '穿透：节点主动连入口' },
  { value: 'normal', label: '普通：线路机连节点' },
];

export function EntryModeTab() {
  const q = useEntryModeSetting();
  const save = useSaveEntryMode();
  const [mode, setMode] = React.useState<EntryMode | ''>('');
  const [ask, setAsk] = React.useState(false);
  const cur = q.data?.default;
  const pick = (mode || cur || '') as EntryMode | '';
  return (
    <Panel title={<>新入口默认模式<InfoTip text={'上架节点、给节点加入口时没指定模式就用它；只影响之后新建的入口\n存量入口在入口菜单「开启 / 关闭穿透」逐个切'} /></>}>
      {q.isError && <ErrorNote error={q.error} onRetry={() => void q.refetch()} />}
      {q.isLoading ? <SkeletonRows rows={2} /> : q.data && <>
        <Field label="默认模式" hint={q.data.set ? undefined : '没设过：按内置默认（穿透）'}>
          <Select value={pick} onChange={(v) => setMode(v as EntryMode)} options={ENTRY_MODE_OPTIONS} aria-label="新入口默认模式" />
        </Field>
        <Note>穿透：节点上的 bridge 主动连线路机入口，客户流量经隧道从节点自己的 IP 出去，线路机不再向节点新建连接——给供应商丢入站连接（SYN）的节点用。
          家宽代理、同机 IP 节点不支持，自动按普通建；线路机或节点的 agent 太旧时也按普通建（作业日志写明）。</Note>
        <div className="row" style={{ marginTop: 'var(--sp-12)', justifyContent: 'flex-end' }}>
          <Button variant="primary" disabled={!pick || (pick === cur && q.data.set)} loading={save.isPending} onClick={() => setAsk(true)}>保存</Button>
        </div>
        <ConfirmDialog open={ask} onOpenChange={setAsk} confirmLabel="保存" title={`新入口默认改成「${pick === 'reverse' ? '穿透' : '普通'}」？`}
          description="只影响之后上架、加入口新建的入口，存量入口不动"
          onConfirm={async () => { if (pick) { await save.mutateAsync(pick); toast.ok('新入口默认模式已保存'); setMode(''); } }} />
      </>}
    </Panel>
  );
}
