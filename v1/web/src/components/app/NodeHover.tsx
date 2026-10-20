// 节点名悬浮卡：悬停 350ms 出现迷你卡片——状态、IP（可复制）、团队/编号、所属线路机:端口、CPU/内存迷你条、到期。
// 内容只在打开时挂载，先用机器列表缓存里的那份，不会为每个名字单独发请求。
import * as React from 'react';
import { HoverCard, HoverCardContent, HoverCardTrigger } from '@/components/ui/hover-card';
import type { MachineView } from '@/lib/types';
import { useMachinePreview } from '@/lib/queries';
import * as F from '@/lib/format';
import { entryText } from '@/lib/derive';
import { KIND_LABEL } from '@/lib/sort';
import { CopyButton, HealthPill, KindIcon, Num } from './primitives';

function MiniBar({ label, pct }: { label: string; pct: number | null }) {
  const p = pct == null ? 0 : Math.max(0, Math.min(100, pct));
  return (
    <div className="hc-bar" data-tone={p >= 90 ? 'bad' : p >= 75 ? 'warn' : undefined}>
      <span className="hc-bar-l">{label}</span>
      <span className="hc-bar-track"><span style={{ width: `${p}%` }} /></span>
      <span className="hc-bar-v num">{pct == null ? '—' : `${Math.round(p)}%`}</span>
    </div>
  );
}

function MiniCard({ m }: { m: MachineView }) {
  const h = m.host;
  const mem = h && h.mem_used != null && h.mem_total ? (h.mem_used / h.mem_total) * 100 : null;
  const due = F.dueFmt(m.subscription?.due_days);
  const team = m.team ? `${m.team.name}${m.number != null ? ` / ${F.pad2(m.number)}` : ''}` : KIND_LABEL[m.kind];
  return (
    <div className="hc">
      <div className="hc-head">
        <KindIcon kind={m.kind} />
        <b className="hc-name ellipsis">{m.name}</b>
        <HealthPill m={m} />
      </div>
      <dl className="hc-rows">
        <div><dt>IP</dt><dd><span className="hc-ip">{m.ip}</span><CopyButton text={m.ip} label=" IP" size={13} /></dd></div>
        <div><dt>团队 / 编号</dt><dd>{team}</dd></div>
        <div><dt>{m.kind === 'relay' ? '入口' : '线路机:端口'}</dt><dd className="ellipsis">{entryText(m)}</dd></div>
        <div><dt>到期</dt><dd>{m.subscription ? <span style={{ color: due.tone === 'bad' ? 'var(--bad-text)' : due.tone === 'warn' ? 'var(--warn-text)' : undefined }}><Num f={due.text} /> <span className="t-sub">· {m.subscription.due_date}</span></span> : <span className="t-muted">未设置</span>}</dd></div>
      </dl>
      {h ? (
        <div className="hc-bars"><MiniBar label="CPU" pct={h.cpu_pct} /><MiniBar label="内存" pct={mem} /></div>
      ) : (
        <div className="hint">{m.kind === 'proxy' ? '家宽代理没有主机指标' : '暂无主机指标'}</div>
      )}
    </div>
  );
}

function Lookup({ id, m }: { id: string; m?: MachineView }) {
  const q = useMachinePreview(m ? null : id);
  const data = m ?? q.data;
  if (!data) return <div className="hc" style={{ minHeight: 64 }}><div className="hint">{q.isError ? '无法加载这台机器' : '加载中…'}</div></div>;
  return <MiniCard m={data} />;
}

/** 包住任意“节点名”元素：children 必须能接 ref（原生元素即可）。id 为空时不加悬浮卡 */
export function NodeHover({ id, m, children }: { id: string | null | undefined; m?: MachineView; children: React.ReactElement }) {
  if (!id) return children;
  return (
    <HoverCard>
      <HoverCardTrigger asChild>{children}</HoverCardTrigger>
      <HoverCardContent><Lookup id={id} m={m} /></HoverCardContent>
    </HoverCard>
  );
}
