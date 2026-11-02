// 客户页 /c/:token：给客户看的只读页面，不登录、不在外壳里（customerEntry.tsx 单独的入口，不带管理端）。
// 读者是客户、不懂技术，只回答一件事：这个月哪些节点付了、哪些还没付。月份切换（?month=，默认本月）→ 一句话 → 逐个节点
// （节点 · 地区 · 这一期的起止 · 已付 / 待付；手机上不横向滚动）。口径同管理端：每个节点从开始收费日起每满一个月收一次，这个月列的是起点在这个月的那一期。
// 不出现 IP、供应商、价格。白底与中性标题由 theme-init.js 首屏就设好（html.is-cx）；一律浅色（theme-init.js 在 /c/ 下不读存储的主题，这里也不引 lib/theme）。
import * as React from 'react';
import { useParams, useSearchParams } from 'react-router-dom';
import { Check, Link2Off, RotateCw } from 'lucide-react';
import { ApiError } from '@/lib/api';
import { usePublicCustomer } from '@/lib/publicCustomer';
import type { PublicCustomer } from '@/lib/types';
import { Button } from '@/components/ui/button';
import { TipHost } from '@/components/ui/tip';
import { Empty } from '@/components/app/empty';
import { MonthSwitch, cnMonth, cnSpan, regionLabel } from '@/features/billing';

/** 一句话：「11 月：你们有 10 个节点，已付 9 个，待付 1 个：团队1-10。」/「11 月的 10 个节点都已付清 ✓」 */
function Lead({ d }: { d: PublicCustomer }) {
  const m = cnMonth(d.month, d.today);
  const due = d.nodes.filter((n) => !n.paid);
  const n = d.nodes.length;
  if (n === 0) return <p className="cx-lead">{m}没有要付费的节点。</p>;
  if (due.length === 0) return <p className="cx-lead">{m}的 <b>{n} 个节点</b><span className="cx-done">都已付清 ✓</span></p>;
  return (
    <p className="cx-lead">
      {m}：你们有 <b>{n} 个节点</b>，已付 <b>{n - due.length} 个</b>，待付 <b>{due.length} 个</b>
      {due.length <= 5 ? <>：{due.map((x, i) => <React.Fragment key={i}>{i > 0 && '、'}<span>{x.name}</span></React.Fragment>)}。</> : '。'}
    </p>
  );
}

export function CustomerPage() {
  const { token = '' } = useParams();
  const [sp, setSp] = useSearchParams();
  const want = /^\d{4}-\d{2}$/.test(sp.get('month') ?? '') ? sp.get('month')! : '';
  const q = usePublicCustomer(token, want);
  const d = q.data;
  const title = d ? `${d.team} · 节点付费情况` : '节点付费情况';
  React.useEffect(() => { document.title = title; }, [title]);
  const onMonth = (m: string) => setSp(m === d?.today.slice(0, 7) ? {} : { month: m }, { replace: true });

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
      <div className="cx-body" data-stale={q.isPlaceholderData}>
        <header className="cx-head">
          <h1 className="cx-title">{d.team} · 节点付费情况</h1>
          <MonthSwitch month={d.month} months={d.months} onChange={onMonth} />
          <Lead d={d} />
        </header>
        {d.nodes.length > 0 && (
          <ul className="bm-list" aria-label={`${cnMonth(d.month, d.today)}每个节点的付费情况`}>
            {d.nodes.map((n, i) => (
              <li key={`${n.name}|${i}`} className="bm-row">
                <div className="bm-main">
                  <span className="bm-name">{n.name}</span>
                  <span className="bm-sub">{regionLabel(n.region)} · <span className="num">{cnSpan(n.period_start, n.period_end, d.today)}</span></span>
                </div>
                <span className="bm-side">{n.paid ? <span className="bm-paid"><Check />已付</span> : <span className="bm-due">待付</span>}</span>
              </li>
            ))}
          </ul>
        )}
      </div>
    );
  }
  return (
    <main className="cx-page">
      {body}
      <TipHost />
    </main>
  );
}
