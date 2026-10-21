// 节点名悬浮卡：悬停 350ms 出现迷你卡片——状态（不正常时列出原因）、IP（可复制）、团队/编号、所属线路机:端口、CPU/内存迷你条、到期。
// 内容只在打开时挂载，先用机器列表缓存里的那份，不会为每个名字单独发请求。
import * as React from 'react';
import { HoverCard, HoverCardContent, HoverCardTrigger } from '@/components/ui/hover-card';
import type { MachineView } from '@/lib/types';
import { useMachine } from '@/lib/queries';
import * as F from '@/lib/format';
import { entryText, isQuiet, resourceMeters } from '@/lib/derive';
import { KIND_LABEL } from '@/lib/sort';
import { CopyButton, Due, HealthPill, KindIcon, ReasonList } from './primitives';
import { MiniMeter } from './viz';

const REASONS_MAX = 3;

function MiniCard({ m }: { m: MachineView }) {
  const r = resourceMeters(m);
  const quiet = isQuiet(m);
  const team = m.team ? `${m.team.name}${m.number != null ? ` / ${F.pad2(m.number)}` : ''}` : KIND_LABEL[m.kind];
  // 有原因就直接列出（同状态胶囊悬停提示的那份），不用再去悬停胶囊：故障、降级的原因，以及不改状态的提醒（流量额度吃紧）；
  // info 级（如来源画像的学习期说明）不列。上架中、退役中这些生命周期状态没有健康原因
  const reasons = m.lifecycle === 'active' ? m.health.reasons.filter((r) => r.severity !== 'info') : [];
  return (
    <div className="hc">
      <div className="hc-head">
        <KindIcon kind={m.kind} />
        <b className="hc-name ellipsis">{m.name}</b>
        <HealthPill m={m} />
      </div>
      {reasons.length > 0 && <ReasonList reasons={reasons.slice(0, REASONS_MAX)} className="reason-list" />}
      {reasons.length > REASONS_MAX && <div className="hint">另有 {reasons.length - REASONS_MAX} 条，见详情</div>}
      <dl className="hc-rows">
        <div><dt>IP</dt><dd><span className="hc-ip">{m.ip}</span><CopyButton text={m.ip} label=" IP" size={13} /></dd></div>
        <div><dt>团队 / 编号</dt><dd>{team}</dd></div>
        {/* 同机 IP 节点：下面的 CPU / 内存是宿主机的 */}
        {m.host_machine && <div><dt>宿主机</dt><dd className="ellipsis">{m.host_machine.name}（读数共用）</dd></div>}
        <div><dt>{m.kind === 'relay' ? '入口' : '线路机:端口'}</dt><dd className="ellipsis">{entryText(m)}</dd></div>
        <div><dt>到期</dt><dd>{m.subscription ? <><Due days={m.subscription.due_days} /> <span className="t-sub">· {m.subscription.due_date}</span></> : <span className="t-muted">未设置</span>}</dd></div>
      </dl>
      {r ? (
        <div className="hc-bars">
          <div className="hc-bar"><span className="hc-bar-l">CPU</span><MiniMeter metric="cpu" pct={r.cpu.pct} empty={quiet} /></div>
          <div className="hc-bar"><span className="hc-bar-l">内存</span><MiniMeter metric="mem" pct={r.mem.pct} empty={quiet} /></div>
        </div>
      ) : (
        <div className="hint">{m.kind === 'proxy' ? '家宽代理没有主机指标' : '暂无主机指标'}</div>
      )}
    </div>
  );
}

function Lookup({ id, m }: { id: string; m?: MachineView }) {
  const q = useMachine(m ? null : id);
  const data = m ?? q.data;
  if (!data) return <div className="hc" style={{ minHeight: 64 }}><div className="hint">{q.isError ? '无法加载这台机器' : '加载中…'}</div></div>;
  return <MiniCard m={data} />;
}

/** 包住任意“节点名”元素：children 必须能接 ref（原生元素即可）。id 为空时不加悬浮卡 */
export function NodeHover({ id, m, children }: { id: string | null | undefined; m?: MachineView; children: React.ReactElement<{ tabIndex?: number }> }) {
  if (!id) return children;
  // 悬浮卡由 Radix 在触发元素获得焦点时打开；触发元素若是普通 span 就聚焦不到，键盘用户永远看不到卡片，所以补 tabIndex
  const plain = typeof children.type === 'string' && !['a', 'button', 'input', 'select', 'textarea'].includes(children.type) && children.props.tabIndex == null;
  return (
    <HoverCard>
      <HoverCardTrigger asChild>{plain ? React.cloneElement(children, { tabIndex: 0 }) : children}</HoverCardTrigger>
      <HoverCardContent><Lookup id={id} m={m} /></HoverCardContent>
    </HoverCard>
  );
}
