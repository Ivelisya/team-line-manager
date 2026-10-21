// 抽屉“费用”页：订阅信息编辑 + 登记续费 + 续费记录。
import * as React from 'react';
import { CalendarPlus } from 'lucide-react';
import type { MachineView, Subscription } from '@/lib/types';
import { useRenew, useRenewals, useSaveSubscription } from '@/lib/queries';
import * as F from '@/lib/format';
import { Button } from '@/components/ui/button';
import { Field, Input, Switch } from '@/components/ui/form';
import { DateField } from '@/components/ui/date-field';
import { remainingValue } from '@/lib/derive';
import { Select } from '@/components/ui/select';
import { Dialog, DialogContent, DialogDescription, DialogFooter, DialogTitle, DialogHeader } from '@/components/ui/dialog';
import { toast } from '@/components/ui/toast';
import { Due, ErrorNote, LabelValue, LabelValueGrid, Note, Section, SectionHeader } from '@/components/app/primitives';

const CURRENCIES = ['CNY', 'USD', 'EUR', 'HKD', 'JPY'].map((c) => ({ value: c, label: `${c} ${F.currencySym(c).trim()}` }));
const CYCLES = Object.entries(F.CYCLE).filter(([k]) => k !== 'once').map(([value, label]) => ({ value, label: `每${label}` }));
const TZS = ['Asia/Shanghai', 'America/Los_Angeles', 'America/New_York', 'Europe/London', 'Asia/Tokyo', 'UTC'].map((z) => ({ value: z, label: z }));

export function BillingTab({ m }: { m: MachineView }) {
  const s = m.subscription;
  const renewals = useRenewals(m.id);
  const [renew, setRenew] = React.useState(false);
  const rv = remainingValue(m.subscription);
  return (
    <>
      {s && (
        <Section>
          <SectionHeader title="当前订阅" />
          <LabelValueGrid>
            <LabelValue label="费用"><span className="num">{F.money(s.price_cents, s.currency)}</span> <span className="unit">/ {F.cycleLabel(s.cycle)}</span></LabelValue>
            <LabelValue label="到期日">{s.due_date}（<Due days={s.due_days} />）</LabelValue>
            <LabelValue label="剩余价值">{rv ? <><span className="num">{F.money(rv.cents, rv.currency)}</span> <span className="unit">{rv.frac > 1 ? `已预付 ${rv.frac.toFixed(1)} 期 · 每期 ${rv.days} 天` : `本期 ${Math.round(rv.frac * 100)}% · 共 ${rv.days} 天`}</span></> : <span className="t-muted">一次性付费，不折算</span>}</LabelValue>
            <LabelValue label="自动续费">{s.auto_renew ? '是' : '否'}</LabelValue>
          </LabelValueGrid>
          <div style={{ marginTop: 12 }}><Button variant="primary" onClick={() => setRenew(true)}><CalendarPlus />登记续费</Button></div>
        </Section>
      )}
      <Section>
        <SectionHeader title={s ? '修改订阅' : '设置订阅'} />
        {!s && <div style={{ marginBottom: 12 }}><Note>这台机器还没有订阅信息。填写后会参与到期提醒与费用统计。</Note></div>}
        <SubForm key={m.id + (s?.due_date ?? '')} m={m} />
      </Section>
      {s && (
        <Section>
          <SectionHeader title="续费记录" />
          {renewals.data?.length ? (
            <div className="table-wrap" style={{ border: '1px solid var(--line)', borderRadius: 'var(--r-float)' }}>
              <table className="dt is-one" style={{ minWidth: 520 }}>
                <colgroup><col style={{ width: 136 }} /><col style={{ width: 200 }} /><col style={{ width: 90 }} /><col /></colgroup>
                <thead><tr><th>登记时间</th><th>到期日</th><th className="r">金额</th><th>备注</th></tr></thead>
                <tbody>{renewals.data.map((r) => (
                  <tr key={r.id}><td>{F.dt(r.created_at)}</td><td>{r.old_due ?? '—'} → {r.new_due}</td><td className="r">{F.money(r.amount_cents, s.currency)}</td><td>{r.note ?? ''}</td></tr>
                ))}</tbody>
              </table>
            </div>
          ) : renewals.isError ? <ErrorNote error={renewals.error} onRetry={() => void renewals.refetch()} /> : <div className="hint">{renewals.isLoading ? '加载中…' : '暂无续费记录'}</div>}
        </Section>
      )}
      {s && <RenewDialog m={m} s={s} open={renew} onOpenChange={setRenew} />}
    </>
  );
}

function SubForm({ m }: { m: MachineView }) {
  const s = m.subscription;
  const save = useSaveSubscription();
  const [price, setPrice] = React.useState(s ? String(s.price_cents / 100) : '');
  const [currency, setCurrency] = React.useState(s?.currency ?? 'USD');
  const [cycle, setCycle] = React.useState(s?.cycle ?? 'monthly');
  const [dueDate, setDueDate] = React.useState(s?.due_date ?? '');
  const [tz, setTz] = React.useState(s?.billing_tz ?? 'Asia/Shanghai');
  const [auto, setAuto] = React.useState(s?.auto_renew ?? false);
  const [remind, setRemind] = React.useState(String(s?.remind_days ?? 7));
  const [traffic, setTraffic] = React.useState(s?.traffic_gb ? String(s.traffic_gb) : '');
  const [resetDay, setResetDay] = React.useState(s?.traffic_reset_day ? String(s.traffic_reset_day) : '1');
  const priceNum = Number(price);
  const valid = price !== '' && isFinite(priceNum) && priceNum >= 0 && /^\d{4}-\d{2}-\d{2}$/.test(dueDate);
  const submit = () => {
    const body: Omit<Subscription, 'due_days'> = {
      price_cents: Math.round(priceNum * 100), currency, cycle, due_date: dueDate, billing_tz: tz, auto_renew: auto,
      remind_days: Number(remind) || 7, traffic_gb: traffic ? Number(traffic) : null, traffic_reset_day: resetDay ? Number(resetDay) : null,
    };
    save.mutate({ id: m.id, ...body }, { onSuccess: () => toast.ok('订阅已保存'), onError: toast.err });
  };
  return (
    <div className="form-grid">
      <Field label="费用"><Input inputMode="decimal" value={price} onChange={(e) => setPrice(e.target.value)} placeholder="68" /></Field>
      <Field label="币种"><Select value={currency} onChange={setCurrency} options={CURRENCIES} /></Field>
      <Field label="计费周期"><Select value={cycle} onChange={setCycle} options={CYCLES} /></Field>
      <Field label="到期日"><DateField value={dueDate} onChange={setDueDate} /></Field>
      <Field label="账单时区" hint="到期按这个时区的日期算"><Select value={tz} onChange={setTz} options={TZS} /></Field>
      <Field label="提前提醒（天）"><Input inputMode="numeric" value={remind} onChange={(e) => setRemind(e.target.value.replace(/\D/g, ''))} /></Field>
      <Field label="流量上限（GiB）" hint="留空 = 不限"><Input inputMode="numeric" value={traffic} onChange={(e) => setTraffic(e.target.value.replace(/\D/g, ''))} /></Field>
      <Field label="流量重置日（每月几号）"><Input inputMode="numeric" value={resetDay} onChange={(e) => setResetDay(e.target.value.replace(/\D/g, ''))} /></Field>
      <label className="check-row span-2" style={{ gap: 10 }}><Switch checked={auto} onCheckedChange={setAuto} /><span>到期自动续费（只用于提醒口径，不会真的扣款）</span></label>
      <div className="span-2 row" style={{ justifyContent: 'flex-end' }}><Button variant="primary" disabled={!valid || save.isPending} loading={save.isPending} onClick={submit}>保存订阅</Button></div>
    </div>
  );
}

/** 告警页「即将到期 / 已过期」的行操作也用这个 */
export function RenewDialog({ m, s, open, onOpenChange }: { m: MachineView; s: Subscription; open: boolean; onOpenChange: (o: boolean) => void }) {
  const renew = useRenew();
  const [newDue, setNewDue] = React.useState('');
  const [amount, setAmount] = React.useState('');
  const [note, setNote] = React.useState('');
  React.useEffect(() => {
    if (open) { setNewDue(F.addMonths(s.due_date, F.CYCLE_MONTHS[s.cycle] ?? 1)); setAmount(String(s.price_cents / 100)); setNote(''); }
  }, [open, s.due_date, s.cycle, s.price_cents]);
  const ok = /^\d{4}-\d{2}-\d{2}$/.test(newDue) && newDue > s.due_date && isFinite(Number(amount)) && amount !== '';
  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent>
        <DialogHeader><DialogTitle>登记续费 · {m.name}</DialogTitle><DialogDescription>只记录“已续费”，到期日顺延；不会向供应商付款。当前到期日 {s.due_date}。</DialogDescription></DialogHeader>
        <div className="form-grid">
          <Field label="新到期日" error={newDue && newDue <= s.due_date ? '应晚于当前到期日' : undefined}><DateField value={newDue} onChange={setNewDue} min={F.addDays(s.due_date, 1)} /></Field>
          <Field label={`实付金额（${s.currency}）`}><Input inputMode="decimal" value={amount} onChange={(e) => setAmount(e.target.value)} /></Field>
          <Field label="备注（选填）" className="span-2"><Input value={note} onChange={(e) => setNote(e.target.value)} placeholder="例如：账单 #2041" /></Field>
        </div>
        <DialogFooter>
          <Button onClick={() => onOpenChange(false)}>取消</Button>
          <Button variant="primary" disabled={!ok || renew.isPending} loading={renew.isPending}
            onClick={() => renew.mutate({ id: m.id, new_due: newDue, amount_cents: Math.round(Number(amount) * 100), note: note.trim() || undefined }, { onSuccess: () => { toast.ok(`已登记续费，到期日 ${newDue}`); onOpenChange(false); }, onError: toast.err })}>登记续费</Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}
