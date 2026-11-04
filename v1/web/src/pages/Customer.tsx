// 客户页 /c/:token：给客户看的只读页面，不登录、不在外壳里（customerEntry.tsx 单独的入口，不带管理端）。
// 读者是客户、不懂技术，只回答一件事：这个月哪些节点付了、哪些还没付。月份切换（?month=，默认本月）→ 一句话 → 逐个节点
// （节点 · 地区 · 这一期的起止 · 已支付 / 待支付；手机上不横向滚动）。口径同管理端：每个节点从开始收费日起每满一个月收一次，这个月列的是起点在这个月的那一期。
// 不出现 IP、供应商、价格。白底与中性标题由 theme-init.js 首屏就设好（html.is-cx）；一律浅色（theme-init.js 在 /c/ 下不读存储的主题，这里也不引 lib/theme）。
import * as React from 'react';
import { useParams, useSearchParams } from 'react-router-dom';
import { ArrowRight, Check, Eye, EyeOff, Link2Off, LockKeyhole, RotateCw } from 'lucide-react';
import { ApiError } from '@/lib/api';
import { useExpired, usePublicCustomer } from '@/lib/publicCustomer';
import { Button } from '@/components/ui/button';
import { TipHost } from '@/components/ui/tip';
import { compareBillingEffectiveDate } from '@/lib/sort';
import { Panel } from '@/components/app/primitives';
import { Empty } from '@/components/app/empty';
import { BillingNodeRow, MonthSummary, MonthSwitch, OutstandingHeading, PeriodLegend, cnMonth } from '@/features/billing';

export function CustomerPage() {
  const { token = '' } = useParams();
  return <CustomerView key={token} token={token} />;
}

function CustomerView({ token }: { token: string }) {
  const [input, setInput] = React.useState('');
  const [visible, setVisible] = React.useState(false);
  const [verified, setVerified] = React.useState(false);
  const [access, setAccess] = React.useState<{ id: number; password: string } | null>(null);
  const [sp, setSp] = useSearchParams();
  const want = /^\d{4}-\d{2}$/.test(sp.get('month') ?? '') ? sp.get('month')! : '';
  const q = usePublicCustomer(token, want, access);
  const expired = useExpired(q.data?.expires_at);
  React.useEffect(() => { if (q.data) setVerified(true); }, [q.data]);
  // 与管理台账统一按本期生效日排列，支付状态改变时保持节点位置。
  const d = React.useMemo(() => q.data && ({
    ...q.data,
    nodes: [...q.data.nodes].sort(compareBillingEffectiveDate),
  }), [q.data]);
  const title = d && !expired && !q.isError ? `${d.team} · 节点付费情况` : '节点付费情况';
  React.useEffect(() => { document.title = title; }, [title]);
  const onMonth = (m: string) => setSp(m === d?.today.slice(0, 7) ? {} : { month: m }, { replace: true });

  const locked = !access || (q.error instanceof ApiError && q.error.status === 403) || (q.isFetching && !verified && !q.error);
  let body: React.ReactNode;
  if (locked) {
    body = <form className="cx-access-card" onSubmit={(e) => {
      e.preventDefault();
      const password = input.trim();
      if (!password) return;
      setAccess(previous => ({ id: (previous?.id ?? 0) + 1, password }));
    }}>
      <div className="cx-access-icon"><LockKeyhole size={22} aria-hidden="true" /></div>
      <h1>查看团队台账</h1>
      <p className="cx-access-intro">输入对接人提供的密码，查看节点与付费情况。</p>
      <label className="cx-access-label" htmlFor="customer-password">查看密码</label>
      <div className="cx-access-input">
        <input id="customer-password" className="input" type={visible ? 'text' : 'password'} autoComplete="current-password"
          placeholder="输入查看密码" required value={input} onChange={e => setInput(e.target.value)}
          aria-invalid={q.error instanceof ApiError && q.error.status === 403 || undefined}
          aria-describedby="customer-password-error" disabled={q.isFetching} />
        <button type="button" className="cx-access-eye" aria-label={visible ? '隐藏密码' : '显示密码'}
          aria-pressed={visible} onClick={() => setVisible(v => !v)}>{visible ? <EyeOff size={18} /> : <Eye size={18} />}</button>
      </div>
      <div id="customer-password-error" className="cx-access-error" role={q.isError ? 'alert' : undefined}>{q.isError && q.error.message}</div>
      <Button variant="primary" type="submit" loading={q.isFetching} disabled={!input.trim()} className="cx-access-submit">
        {q.isFetching ? '正在验证…' : <>查看台账<ArrowRight size={16} /></>}
      </Button>
      <p className="cx-access-note">分享链接自生成起 1 小时内有效</p>
    </form>;
  } else if (expired || q.isError) {
    body = expired || (q.error instanceof ApiError && q.error.status === 404)
      ? <Empty icon={<Link2Off size={22} />} text="链接无效、已过期或已停用" hint="请联系对接人获取新链接" />
      : <Empty text="暂时打不开，请稍后再试"><Button loading={q.isFetching} onClick={() => void q.refetch()}><RotateCw />重试</Button></Empty>;
    body = <div className="cx-state">{body}</div>;
  } else if (!d) {
    body = <div className="cx-head" aria-busy><span className="skel" style={{ width: 260, height: 30 }} /><span className="skel" style={{ width: 360, height: 16 }} /><span className="skel" style={{ height: 240, marginTop: 24 }} /></div>;
  } else {
    const counts = new Map(d.outstanding.map(item => [item.month, item.count]));
    body = <div className="cx-body" data-stale={q.isPlaceholderData} inert={q.isPlaceholderData || undefined}>
      <header className="cx-ledger-head">
        <h1 className="cx-title">{d.team} · 台账</h1>
        <span className="bm-team-choice" aria-current="true">{d.team}</span>
      </header>
      <section className="panel bm-outstanding" aria-label="待支付">
        <OutstandingHeading />
        {counts.size > 0
          ? <MonthSummary counts={counts} selected={d.month} today={d.today} onSelect={onMonth} />
          : <Empty text="暂无待支付节点" />}
      </section>
      <Panel className="bm-month-panel" flush title={<MonthSwitch month={d.month} months={d.months} onChange={onMonth} />}>
        {d.nodes.length > 0 ? <>
          <PeriodLegend />
          <ul className="bm-list" aria-label={`${cnMonth(d.month, d.today)}节点清单`}>
            {d.nodes.map((n, i) => <BillingNodeRow key={`${n.name}|${i}`} n={n} today={d.today} side={
              <span className={n.paid ? 'bm-paid' : 'bm-due'}>{n.paid ? <><Check />已支付</> : '待支付'}</span>
            } />)}
          </ul>
        </> : <Empty text={`${cnMonth(d.month, d.today)}没有要支付的节点`} />}
      </Panel>
    </div>;
  }
  return (
    <main className={locked ? 'cx-page is-locked' : 'cx-page cx-ledger'}>
      {body}
      <TipHost />
    </main>
  );
}
