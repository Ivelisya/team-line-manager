// 团队与套餐 → 待付：未来 30 天内到期、要付的机器，按 供应商 × 到期日 分组（服务端折好 USD）。每组一个「登记已付…」：
// 打开批量续费确认（金额默认订阅价），一个事务里登记整组、一条审计。不含备用团队（等到期丢弃）；开了自动续费的单列、不算待付。
import * as React from 'react';
import { useQuery } from '@tanstack/react-query';
import { ArrowRight, ExternalLink } from 'lucide-react';
import { renewBatchPreview, useDue, useRenewBatch } from '@/lib/queries';
import type { DueGroup, DueProvider } from '@/lib/types';
import { meta, usdTotalTip } from '@/lib/derive';
import { tzCity, tzLabel } from '@/lib/tz';
import { useUsdPreview } from '@/lib/fx';
import * as F from '@/lib/format';
import { Button } from '@/components/ui/button';
import { Field, InfoTip, Input, UnitInput } from '@/components/ui/form';
import { SkeletonRows } from '@/components/ui/skeleton';
import { toast } from '@/components/ui/toast';
import { resetOnOpen, useHeld } from '@/lib/useHeld';
import { Due, Empty, ErrorNote, Note, Panel, TableWrap } from '@/components/app/primitives';
import { FormDialog } from '@/components/app/dialogs';

/** 看未来多少天 */
const DAYS = 30;

const NO_PROVIDER = <span className="t-muted">（无供应商）</span>;
const dueState = (days: number) => (days < 0 ? 'overdue' : 'ok');
/** 一组的原币种小计：「¥80.00 + $10.00」，没有汇率的币种照写 */
const subtotal = (g: DueGroup) => [...g.total.by_currency, ...g.total.unconverted].map((p) => F.money(p.cents, p.currency)).join(' + ');
const usdText = (t: DueGroup['total']) => `${F.money(t.cents, 'USD')}${t.unconverted.length ? '*' : ''}`;

/** 到期日 + 剩余天数：天数按这组的账单时区的今天算，悬停写明；账单时区不是组织时区的，日期后标城市名（同一天到期的两组差一天时一眼看得出） */
function DueWhen({ g, short }: { g: DueGroup; short?: boolean }) {
  const tz = g.billing_tz;
  const other = tz && tz !== meta().org_timezone;
  return (
    <span data-tip={tz ? `剩余天数按${tzLabel(tz)}时间算` : undefined}>
      <span className="num">{short ? g.due_date.slice(5) : g.due_date}</span> <Due days={g.machines[0].due_days} state={dueState(g.machines[0].due_days)} />
      {other && <span className="t-muted"> · {tzCity(tz)}</span>}
    </span>
  );
}

/** 供应商名 + 控制台链接（http(s) 才做成链接） */
function ProviderCell({ p }: { p: DueProvider }) {
  const link = p.panel_url && /^https?:\/\//i.test(p.panel_url) ? p.panel_url : null;
  return (
    <span className="row" style={{ gap: 6, flexWrap: 'nowrap' }}>
      <span className="strong">{p.name ?? NO_PROVIDER}</span>
      {link && <a href={link} target="_blank" rel="noreferrer noopener" className="btn btn-link" style={{ gap: 4 }} data-tip={`打开 ${p.name ?? ''} 控制台`}><ExternalLink size={12} /><span className="sr-only">供应商控制台</span></a>}
    </span>
  );
}

export function DueTab() {
  const q = useDue(DAYS);
  const [pay, setPay] = React.useState<DueGroup | null>(null);
  const held = useHeld(pay ? `${pay.provider.id}|${pay.due_date}|${pay.billing_tz}` : null, pay);
  const d = q.data;
  const n = d?.groups.reduce((s, g) => s + g.machines.length, 0) ?? 0;
  return (
    <>
      {q.isError && <ErrorNote error={q.error} onRetry={() => void q.refetch()} />}
      <Panel title={`未来 ${DAYS} 天待付`} count={d ? `${n} 台` : undefined}
        sub={<><InfoTip text={"按供应商和到期日分组，金额是订阅的每期价格；折成美元按「设置 → 汇率」\n已过期还没续费的也在这里\n备用团队的机器等到期丢弃，不在这里\n登记已付只记账、顺延到期日，不会向供应商付款"} />{d && n > 0 && <span className="t-sub" style={{ marginLeft: 'var(--sp-8)' }} data-tip={usdTotalTip(d.total)}>合计 <span className="num">{usdText(d.total)}</span></span>}</>} flush>
        <TableWrap>
          {d && d.groups.length > 0 && (
            <table className="dt is-one due-table" style={{ minWidth: 820 }}>
              <colgroup><col style={{ width: 170 }} /><col style={{ width: 210 }} /><col /><col style={{ width: 150 }} /><col style={{ width: 110 }} /><col style={{ width: 120 }} /></colgroup>
              <thead><tr><th>供应商</th><th>到期日</th><th>机器</th><th className="r">小计</th><th className="r">折合 USD</th><th /></tr></thead>
              <tbody>
                {d.groups.map((g) => {
                  const names = g.machines.map((m) => m.name);
                  return (
                    <tr key={`${g.provider.id}|${g.due_date}|${g.billing_tz}`}>
                      <td><ProviderCell p={g.provider} /></td>
                      <td><DueWhen g={g} /></td>
                      <td data-tip={names.length > 4 ? names.join('、') : undefined}>{names.slice(0, 4).join('、')}{names.length > 4 && <span className="t-muted"> 等 {names.length} 台</span>}</td>
                      <td className="r num" data-tip={usdTotalTip(g.total)}>{subtotal(g)}</td>
                      <td className="r num">{usdText(g.total)}</td>
                      <td className="c"><Button size="sm" onClick={() => setPay(g)}>登记已付…</Button></td>
                    </tr>
                  );
                })}
              </tbody>
            </table>
          )}
          {/* 窄屏不用表（横滑才看得全机器、小计、USD）：一组一张小卡，按钮在卡尾 */}
          {d && d.groups.length > 0 && (
            <ul className="due-list">
              {d.groups.map((g) => {
                const names = g.machines.map((m) => m.name);
                return (
                  <li key={`${g.provider.id}|${g.due_date}|${g.billing_tz}`} className="due-item">
                    <div className="due-item-top"><ProviderCell p={g.provider} /><DueWhen g={g} short /></div>
                    <div className="due-item-machines">{names.slice(0, 4).join('、')}{names.length > 4 && <span className="t-muted"> 等 {names.length} 台</span>}</div>
                    <div className="due-item-foot"><span className="num" data-tip={usdTotalTip(g.total)}>{subtotal(g)}<span className="t-muted"> ≈ {usdText(g.total)}</span></span><Button size="sm" onClick={() => setPay(g)}>登记已付…</Button></div>
                  </li>
                );
              })}
            </ul>
          )}
          {q.isLoading && <SkeletonRows rows={3} one />}
          {d && d.groups.length === 0 && <Empty text={`未来 ${DAYS} 天没有要付的`} />}
        </TableWrap>
      </Panel>
      {d && d.auto.length > 0 && (
        <Note tone="info">自动续费（到期后自动顺延，不算待付）<InfoTip text="开了自动续费的机器到期后由中控按周期顺延并记一条续费，只记账；供应商那边要自己设好自动扣费" />：
          {' '}{d.auto.map((a) => `${a.machine.name} ${a.machine.due_date.slice(5)}`).join('、')}</Note>
      )}
      {held && <PayForm g={held} open={!!pay} onClose={() => setPay(null)} />}
    </>
  );
}

/** 一台的实付金额：默认订阅价，旁边提示折合多少美元 */
function AmountCell({ currency, value, onChange }: { currency: string; value: string; onChange: (v: string) => void }) {
  const bad = value.trim() === '' || !(isFinite(Number(value)) && Number(value) >= 0);
  const hint = useUsdPreview(bad ? null : Math.round(Number(value) * 100), currency);
  return (
    <div>
      <UnitInput unit={currency} width={130} inputMode="decimal" value={value} onChange={(e) => onChange(e.target.value)} aria-invalid={bad || undefined} aria-label={`实付金额（${currency}）`} />
      {hint && currency !== 'USD' && <div className="hint">{hint}</div>}
    </div>
  );
}

/** 登记已付：这一组的机器一起续费一个周期（预览里是中控算的新到期日），金额逐台可改 */
const PayForm = resetOnOpen(function PayForm({ g, open, onClose }: { g: DueGroup; open: boolean; onClose: () => void }) {
  const run = useRenewBatch();
  const items = React.useMemo(() => g.machines.map((m) => ({ machine_id: m.id })), [g]);
  const pv = useQuery({ queryKey: ['renew-batch-preview', items], queryFn: () => renewBatchPreview(items), staleTime: 0, gcTime: 0 });
  const [amounts, setAmounts] = React.useState<Record<string, string>>(() => Object.fromEntries(g.machines.map((m) => [m.id, String(m.price_cents / 100)])));
  const [ref, setRef] = React.useState('');
  const bad = g.machines.some((m) => { const a = amounts[m.id]; return a.trim() === '' || !(isFinite(Number(a)) && Number(a) >= 0); });
  const submit = () => run.mutate(
    { items: g.machines.map((m) => ({ machine_id: m.id, amount_cents: Math.round(Number(amounts[m.id]) * 100) })), ref: ref.trim() || undefined },
    { onSuccess: (r) => { toast.ok(`已登记 ${r.items.length} 台续费`); onClose(); }, onError: toast.err },
  );
  const newDue = (id: string) => pv.data?.items.find((i) => i.machine_id === id)?.new_due;
  return (
    <FormDialog open={open} onOpenChange={(o) => !o && onClose()} size="md" dirty={!!ref.trim() || g.machines.some((m) => amounts[m.id] !== String(m.price_cents / 100))}
      title={`登记已付 · ${g.provider.name ?? '无供应商'} · ${g.due_date.slice(5)} 到期`}
      desc={<>{g.machines.length} 台，折合 {usdText(g.total)} <InfoTip text="只记录「已续费」，到期日按订阅周期顺延；不会向供应商付款。金额默认订阅价，实付不同可以改" /></>}
      submitLabel={`登记 ${g.machines.length} 台`} busy={run.isPending} valid={!bad && !pv.isError} onSubmit={submit}>
      {pv.isError && <Note tone="bad">{(pv.error as Error).message}</Note>}
      <div className="table-wrap is-boxed is-capped">
        <table className="dt is-one"><colgroup><col /><col style={{ width: 180 }} /><col style={{ width: 150 }} /></colgroup>
          <thead><tr><th>机器</th><th>到期日</th><th>实付</th></tr></thead>
          <tbody>{g.machines.map((m) => (
            <tr key={m.id}><td className="strong num">{m.name}</td>
              <td className="num">{m.due_date} <ArrowRight size={12} style={{ color: 'var(--fg-3)' }} /> {newDue(m.id) ?? <span className="t-muted">…</span>}</td>
              <td><AmountCell currency={m.currency} value={amounts[m.id]} onChange={(v) => setAmounts((a) => ({ ...a, [m.id]: v }))} /></td></tr>
          ))}</tbody></table>
      </div>
      <Field label="账单号（选填）"><Input value={ref} onChange={(e) => setRef(e.target.value)} placeholder="例如：#2041" maxLength={64} /></Field>
    </FormDialog>
  );
});
