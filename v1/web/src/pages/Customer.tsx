// 客户页 /c/:token：给客户看的只读页面，不登录、不在外壳里（customerEntry.tsx 单独的入口，不带管理端）。
// 读者是客户、不懂技术：一句话总结（几个节点在用、哪天续几个）→ 逐个节点的清单（节点 · 地区 · 本期 · 下次续费 · 还有几天，按下次续费日排；
// 手机上每个节点两行，不横向滚动）→ 按续费日分组的时间轴（次要，看「这一期过了多少」）。
// 不写「批次 / 节点月」；服务端给的 node_months（各组从开始到本期的累计）对不上任何一笔账，不显示。不出现 IP、供应商、价格。
// 白底与中性标题由 theme-init.js 首屏就设好（html.is-cx）；一律浅色（theme-init.js 在 /c/ 下不读存储的主题，这里也不引 lib/theme）。
import * as React from 'react';
import { useParams } from 'react-router-dom';
import { Link2Off, RotateCw } from 'lucide-react';
import { ApiError } from '@/lib/api';
import { usePublicCustomer } from '@/lib/publicCustomer';
import type { PublicCustomer } from '@/lib/types';
import { Button } from '@/components/ui/button';
import { TipHost } from '@/components/ui/tip';
import { Empty } from '@/components/app/empty';
import { BatchTimeline, TimelineLegend, cnDate, cnSpan, daysBetween, regionLabel, type TimelineItem } from '@/features/BatchTimeline';

/** 一句话总结：「你们现在有 10 个节点在用（美区 9 个，墨西哥 1 个）。下次续费：11 月 30 日（1 个），12 月 1 日（9 个）。」 */
function Lead({ d }: { d: PublicCustomer }) {
  const regions = new Map<string, number>();
  const due = new Map<string, number>();
  for (const n of d.nodes) {
    regions.set(n.region, (regions.get(n.region) ?? 0) + 1);
    if (n.renews) due.set(n.period_end, (due.get(n.period_end) ?? 0) + 1);
  }
  const next = [...due.entries()].sort(([a], [b]) => (a < b ? -1 : 1)).slice(0, 2);
  return (
    <p className="cx-lead">
      你们现在有 <b>{d.nodes.length} 个节点</b>在用{regions.size > 1 && <>（{[...regions].map(([r, n], i) => <React.Fragment key={r}>{i > 0 && '，'}<span>{regionLabel(r)} {n} 个</span></React.Fragment>)}）</>}。
      {next.length > 0 && <>下次续费：{next.map(([s, n], i) => <React.Fragment key={s}>{i > 0 && '，'}<span><b>{cnDate(s, d.today)}</b>（{n} 个）</span></React.Fragment>)}。</>}
    </p>
  );
}

/** 逐个节点：宽屏是一张表，窄屏每个节点两行（名字 · 地区 · 还有几天 / 本期 · 下次续费） */
function Nodes({ d }: { d: PublicCustomer }) {
  return (
    <section className="cx-nodes" aria-label="每个节点的续费时间">
      <div className="cx-nrow cx-nhead" aria-hidden><span>节点</span><span>地区</span><span>本期</span><span>下次续费</span><span className="cx-nleft">还有几天</span></div>
      {d.nodes.map((n) => {
        const left = daysBetween(d.today, n.period_end);
        return (
          <div key={`${n.name}|${n.period_start}`} className="cx-nrow">
            <span className="cx-nname">{n.name}</span>
            <span className="cx-nregion">{regionLabel(n.region)}</span>
            <span className="cx-nspan"><span className="cx-nlabel">本期 </span><span className="num">{cnSpan(n.period_start, n.period_end, d.today)}</span></span>
            <span className="cx-nnext num">{n.renews ? <>{cnDate(n.period_end, d.today)}<span className="cx-nlabel">续费</span></> : <span className="t-sub">到期后不再续费</span>}</span>
            <span className="cx-nleft num" data-soon={left <= 3 || undefined}><span className="cx-nlabel">还有 </span>{left} 天</span>
          </div>
        );
      })}
    </section>
  );
}

export function CustomerPage() {
  const { token = '' } = useParams();
  const q = usePublicCustomer(token);
  const d = q.data;
  const title = d ? `${d.team} · 节点续费情况` : '节点续费情况';
  React.useEffect(() => { document.title = title; }, [title]);
  const items = React.useMemo<TimelineItem[]>(() => (d?.batches ?? []).map((b) => ({ ...b, key: String(b.seq) })), [d]);

  let body: React.ReactNode;
  if (q.isError && !d) {
    body = q.error instanceof ApiError && q.error.status === 404
      ? <Empty icon={<Link2Off size={22} />} text="链接无效或已停用" hint="请联系对接人获取新链接" />
      : <Empty text="暂时打不开，请稍后再试"><Button loading={q.isFetching} onClick={() => void q.refetch()}><RotateCw />重试</Button></Empty>;
    body = <div className="cx-state">{body}</div>;
  } else if (!d) {
    body = <div className="cx-head" aria-busy><span className="skel" style={{ width: 260, height: 30 }} /><span className="skel" style={{ width: 360, height: 16 }} /><span className="skel" style={{ height: 240, marginTop: 24 }} /></div>;
  } else {
    body = (
      <>
        <header className="cx-head">
          <h1 className="cx-title">{d.team} · 节点续费情况</h1>
          {d.nodes.length > 0 && <Lead d={d} />}
        </header>
        {d.nodes.length === 0
          ? <Empty text="现在没有在用的节点" />
          : (
            <>
              <Nodes d={d} />
              {items.length > 0 && (
                <section className="cx-board" aria-label="按续费日分组">
                  <div className="cx-board-head">
                    <h2 className="cx-h2">按续费日分组</h2>
                    <p className="cx-sub">每一小格是一天；每条线从这一期开始的日子，画到下次续费的日子</p>
                  </div>
                  <TimelineLegend />
                  <BatchTimeline items={items} today={d.today} />
                </section>
              )}
            </>
          )}
      </>
    );
  }
  return (
    <main className="cx-page">
      {body}
      <TipHost />
    </main>
  );
}
