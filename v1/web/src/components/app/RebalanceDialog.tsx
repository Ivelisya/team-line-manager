// 搬迁建议：线路机按近 7 天的速度重置前会用满时，列出搬走后能撑到重置日的最少入口和目标线路机（只读建议，GET /relays/{id}/rebalance）。
// 「全部搬过去」打开批量换线路机对话框（一起换，每台软路由只重启一次），每行「搬过去」打开单个的换线路机对话框；
// 目标线路机都已选好，在那里预览软路由、确认后建作业；这里不执行任何事。
import * as React from 'react';
import { X } from 'lucide-react';
import type { MachineView } from '@/lib/types';
import { useMachines, useRebalance } from '@/lib/queries';
import { mmdd, quotaGb } from '@/lib/derive';
import * as F from '@/lib/format';
import { Button } from '@/components/ui/button';
import { Dialog, DialogContent, DialogDescription, DialogFooter, DialogHeader, DialogTitle } from '@/components/ui/dialog';
import { InfoTip } from '@/components/ui/form';
import { resetOnOpen } from '@/lib/useHeld';
import { ErrorNote, Note } from './primitives';
import { MoveDialog, MovesDialog } from './LifecycleDialogs';

const gb = (n: number) => F.str(quotaGb(n));

export const RebalanceDialog = resetOnOpen(function RebalanceDialog({ relay, open, onOpenChange }: { relay: MachineView; open: boolean; onOpenChange: (o: boolean) => void }) {
  // 不想动的节点（如分享给别人的入口）：点「不动」后重新算
  const [skip, setSkip] = React.useState<{ id: string; name: string }[]>([]);
  const q = useRebalance(relay.id, open, skip.map((x) => x.id));
  const p = q.data;
  const machines = useMachines().data ?? [];
  const [move, setMove] = React.useState<{ node: MachineView; from?: string } | null>(null);
  const [all, setAll] = React.useState(false);
  const start = (id: string, needsFrom: boolean) => {
    const node = machines.find((x) => x.id === id);
    if (node) setMove({ node, from: needsFrom ? relay.id : undefined });
  };
  return (
    <>
      <Dialog open={open} onOpenChange={onOpenChange}>
        <DialogContent size="md">
          <DialogHeader>
            <DialogTitle>搬迁建议 · {relay.name}</DialogTitle>
            <DialogDescription>
              {p ? <>本周期已用 {gb(p.used)} / {p.limit_gb} GB{p.daily_avg != null && <>，近 7 天日均 {gb(p.daily_avg)}</>}
                {p.forecast_full_at && <>，预计 {mmdd(p.forecast_full_at)} 用满（重置 {mmdd(p.period_end)}）</>}</> : '按近 7 天的速度估算'}
              {' '}<InfoTip text={'挑出搬走后能让这台撑到重置日的最少入口，搬去额度余量最多的在用线路机。\n每个入口的流量按近 7 天算，再按这台网卡的日均折算；这里只是建议，点「搬过去」才会走换线路机的预览与确认'} />
            </DialogDescription>
          </DialogHeader>
          {q.isError && <ErrorNote error={q.error} onRetry={() => void q.refetch()}>取建议失败：</ErrorNote>}
          {p && !p.moves.length && <Note tone={p.needed ? 'warn' : 'ok'}>{p.reason ?? '现在不用搬'}</Note>}
          {p && p.moves.length > 0 && p.target && (
            <div className="stack-sm" style={{ opacity: q.isFetching ? 0.6 : 1 }}>
              <div>
                搬到 <b>{p.target.name}</b>
                {p.after && <>，{relay.name} 日均降到 {gb(p.after.daily_avg)}，重置时约用到 <b className="num">{p.after.projected_pct}%</b>（现在按速度约 {p.projected_pct}%）</>}
                {p.target_after?.projected_pct != null && <>；{p.target.name} 接收后约 <b className="num">{p.target_after.projected_pct}%</b></>}
              </div>
              {p.reason && <Note tone="warn">{p.reason}</Note>}
              {p.moves.map((m) => (
                <div key={m.entry_id} className="row row-between">
                  <span><b>{m.node.name}</b> <span className="t-sub">· 端口 {m.port} · 本周期 {gb(m.period_bytes)} · 日均 {gb(m.daily_bytes)}</span></span>
                  <span className="row">
                    <Button size="sm" variant="quiet" onClick={() => setSkip((s) => [...s, m.node])} data-tip="这个节点不动，换别的入口凑"><X />不动</Button>
                    <Button size="sm" onClick={() => start(m.node.id, m.needs_from)} disabled={!machines.some((x) => x.id === m.node.id)}>搬过去</Button>
                  </span>
                </div>
              ))}
              <div className="row row-between">
                <span className="hint">搬走后入口地址会变：用订阅链接的自动更新，用分享链接的要换新链接</span>
                {p.moves.length > 1 && <Button size="sm" onClick={() => setAll(true)} data-tip="一起换，每台软路由只改一次、OpenClash 只重启一次">全部搬过去</Button>}
              </div>
            </div>
          )}
          {skip.length > 0 && (
            <div className="row">
              <span className="t-sub">不动：</span>
              {skip.map((x) => <Button key={x.id} size="sm" variant="quiet" onClick={() => setSkip((s) => s.filter((y) => y.id !== x.id))} data-tip="点一下恢复"><X />{x.name}</Button>)}
            </div>
          )}
          <DialogFooter><Button onClick={() => onOpenChange(false)}>关闭</Button></DialogFooter>
        </DialogContent>
      </Dialog>
      {all && p?.target && <MovesDialog nodes={p.moves.map((m) => ({ id: m.node.id, name: m.node.name, port: m.port }))} from={relay} open onOpenChange={(o) => { if (!o) setAll(false); }} onDone={() => { setAll(false); onOpenChange(false); }} initialTo={p.target.id} />}
      {move && <MoveDialog m={move.node} open onOpenChange={(o) => { if (!o) setMove(null); }} onDone={() => { setMove(null); onOpenChange(false); }} initialTo={q.data?.target?.id} initialFrom={move.from} />}
    </>
  );
});
