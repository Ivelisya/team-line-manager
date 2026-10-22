// 顶栏告警胶囊：进行中的告警数，点击去告警页；悬停（或键盘聚焦）预览最前面的几条——级别点、对象、规则、摘要、已持续多久，
// 点一行打开那台机器的抽屉。用的就是顶栏计数那份查询（告警页第一页的缓存，严重度优先），不另取、不轮询
import * as React from 'react';
import { Link, NavLink } from 'react-router-dom';
import { useIncidents } from '@/lib/queries';
import { useDrawer } from '@/lib/drawer';
import * as F from '@/lib/format';
import { HoverCard, HoverCardContent, HoverCardTrigger } from '@/components/ui/hover-card';
import { Pill, SeverityMark } from '@/components/app/primitives';

const SHOW = 5;

export function AlertsIndicator() {
  const q = useIncidents('firing');
  const { open: openDrawer } = useDrawer();
  // 受控：点了一行要先收起预览——抽屉在弹层下面一层，开着的预览会盖在抽屉上
  const [open, setOpen] = React.useState(false);
  const n = q.data?.total ?? 0;
  const list = q.data?.items.slice(0, SHOW) ?? [];
  // 告警全部恢复时预览随胶囊一起卸掉，开关也复位（渲染中直接跟上）：不然下一条告警一来，预览会自己弹出来
  if (!n) { if (open) setOpen(false); return null; }
  const now = Date.now();
  return (
    <HoverCard open={open} onOpenChange={setOpen}>
      <HoverCardTrigger asChild>
        <NavLink to="/incidents" className="top-alert" aria-label={`${n} 条告警`} onClick={() => setOpen(false)}><Pill tone="bad">{n}<span className="hide-sm"> 条告警</span></Pill></NavLink>
      </HoverCardTrigger>
      <HoverCardContent align="end" className="alerts-pop">
        <div className="pop-head">进行中的告警<span className="t-sub">{n}</span></div>
        <ul className="pop-list">
          {list.map((i) => {
            const sid = i.subject?.id ?? i.subject_id;
            const name = i.subject?.name ?? i.subject_name ?? '—';
            // 摘要以「对象名：」开头（告警通知要能单独看懂），这里名字已在上一行，去掉不重复
            const sum = i.summary.startsWith(`${name}：`) ? i.summary.slice(name.length + 1) : i.summary;
            return (
              <li key={i.id}>
                <button type="button" className="pop-item" onClick={() => { setOpen(false); if (sid) openDrawer(sid); }}>
                  <span className="alerts-pop-row">
                    <SeverityMark severity={i.severity} />
                    <b className="ellipsis">{name}</b>
                    <span className="alerts-pop-rule ellipsis">{i.rule_name}</span>
                    <span className="alerts-pop-age">已持续 {F.str(F.since(i.opened_at, now))}</span>
                  </span>
                  <span className="alerts-pop-sum hint ellipsis" data-tip={sum}>{sum}</span>
                </button>
              </li>
            );
          })}
        </ul>
        <Link to="/incidents" onClick={() => setOpen(false)} className="pop-all">查看全部 {n} 条 →</Link>
      </HoverCardContent>
    </HoverCard>
  );
}
