// 客户到期页 /c/:token：给客户看的只读页面，不登录、不在外壳里（customerEntry.tsx 单独的入口，不带管理端）。
// 只放有效的批次：标题、各地区有效个数、时间轴、节点月总计。不出现 IP、供应商、价格、节点名，也没有管理端导航。
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
import { BatchTimeline, TimelineLegend, mmdd, type TimelineItem } from '@/features/BatchTimeline';

/** 副标题的年份：今天这一年；时间轴跨年时写成「2025–2026年」 */
function yearsOf(d: PublicCustomer): string {
  const ys = [d.today, ...d.batches.flatMap((b) => [b.start_date, b.end_date])].map((s) => s.slice(0, 4));
  const lo = ys.reduce((a, b) => (b < a ? b : a)); const hi = ys.reduce((a, b) => (b > a ? b : a));
  return lo === hi ? `${lo}年` : `${lo}–${hi}年`;
}

function Regions({ d }: { d: PublicCustomer }) {
  return (
    <section className="cx-sum" aria-label="当前有效节点">
      <div className="cx-sum-head"><span className="num">{mmdd(d.today)}</span> 当前有效</div>
      <div className="cx-regions">
        {d.regions.map((r) => (
          <div key={r.region} className="cx-region">
            <span className="cx-region-name">{r.region === '未知' ? (d.regions.length > 1 ? '其他' : '当前') : r.region}有效</span>
            <span className="cx-region-val">{r.total}<small>个</small></span>
            {(r.counts.length > 1 || r.today_count > 0) && (
              <span className="cx-region-calc">
                {r.counts.length > 1 && <span className="num">{r.counts.join('+')}={r.total} 个</span>}
                {r.counts.length > 1 && r.today_count > 0 && ' · '}
                {r.today_count > 0 && <span className="is-today">含今日到期 <span className="num">{r.today_count}</span> 个</span>}
              </span>
            )}
          </div>
        ))}
      </div>
    </section>
  );
}

export function CustomerPage() {
  const { token = '' } = useParams();
  const q = usePublicCustomer(token);
  const d = q.data;
  const title = d ? `${d.team} · 节点到期时间轴` : '节点到期时间轴';
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
          <h1 className="cx-title">{d.team} · 节点到期时间轴</h1>
          <p className="cx-sub">{yearsOf(d)} · 共 {d.batches.length} 批 · 每小格 1 天 · 起点为本期开始日期，终点为到期日期</p>
        </header>
        {d.batches.length === 0
          ? <Empty text="当前没有有效的节点" />
          : (
            <>
              <Regions d={d} />
              <section className="cx-board" aria-label="批次时间轴">
                <TimelineLegend />
                <BatchTimeline items={items} today={d.today} />
              </section>
              <footer className="cx-foot">
                <div className="cx-total">总计：<span className="num">{d.node_months}</span> 个节点月</div>
                <div className="cx-note">按完整服务周期计算，包含已经续期的未来部分。</div>
              </footer>
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
