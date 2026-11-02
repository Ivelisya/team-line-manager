// 线路机的入口协议（PATCH /relays/{id}）：Reality + AnyTLS / 仅 Reality / 仅 AnyTLS。改动都先确认；中控拒绝（409：AnyTLS 没在跑、
// agent 太旧、有软路由出口指着）时原因写在对话框里
import * as React from 'react';
import type { MachineView, RelayProtocols } from '@/lib/types';
import { useRelayProtocols } from '@/lib/queries';
import { Select } from '@/components/ui/select';
import { toast } from '@/components/ui/toast';
import { ConfirmDialog } from './dialogs';
import { Note } from './primitives';

const OPTS: { value: RelayProtocols; label: string }[] = [
  { value: 'both', label: 'Reality + AnyTLS' },
  { value: 'reality', label: '仅 Reality' },
  { value: 'anytls', label: '仅 AnyTLS' },
];
const label = (p: RelayProtocols) => OPTS.find((o) => o.value === p)?.label ?? p;
const IMPACT: Record<RelayProtocols, string> = {
  both: 'Reality 与 AnyTLS 都对外开放',
  reality: '不再提供 AnyTLS：分享链接、订阅里的 AnyTLS 条目随之消失，正在用 AnyTLS 的客户端会断开',
  anytls: 'Reality 对公网关闭（穿透入口的只对它的节点开放），只留 AnyTLS：正在用 Reality 的客户端会断开，分享链接、订阅只给 AnyTLS',
};

export function RelayProtocolsField({ m }: { m: MachineView }) {
  const cur = m.relay?.protocols ?? 'both';
  const set = useRelayProtocols();
  const [want, setWant] = React.useState<RelayProtocols | null>(null);
  const [err, setErr] = React.useState<string | null>(null);
  const held = React.useRef<RelayProtocols>('both');
  if (want) held.current = want;
  return (
    <>
      <Select sm value={cur} options={OPTS} aria-label="入口协议" disabled={m.lifecycle !== 'active'} onChange={(v) => { if (v !== cur) { setErr(null); setWant(v as RelayProtocols); } }} />
      <ConfirmDialog open={!!want} onOpenChange={(o) => { if (!o) setWant(null); }} title={`入口协议改成 ${label(held.current)} · ${m.name}`}
        description={IMPACT[held.current]} confirmLabel="改协议" danger={held.current !== 'both'}
        onConfirm={async () => {
          try {
            await set.mutateAsync({ id: m.id, protocols: held.current });
          } catch (e) {
            setErr(e instanceof Error ? e.message : String(e));
            throw e;
          }
          toast.ok(`${m.name} 的入口协议已改成 ${label(held.current)}，线路机会自己应用`);
        }}>
        {err && <Note tone="bad">{err}</Note>}
      </ConfirmDialog>
    </>
  );
}
