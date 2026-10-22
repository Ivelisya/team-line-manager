// 抽屉“费用”页：订阅信息编辑 + 登记续费 + 续费记录。
import * as React from 'react';
import { CalendarPlus } from 'lucide-react';
import type { MachineView, SubField, Subscription } from '@/lib/types';
import { useRenew, useRenewals, useSaveSubscription, type SubscriptionBody } from '@/lib/queries';
import * as F from '@/lib/format';
import { Button } from '@/components/ui/button';
import { Field, Input, Switch } from '@/components/ui/form';
import { DateField } from '@/components/ui/date-field';
import { inheritedValue, inheritSource, inheritText, isOverridden, limitGb, quota, quotaGb, remainingValue, trafficRuleLabel, cycleLabel, currencyOptions, cycleOptions, meta } from '@/lib/derive';
import { Select } from '@/components/ui/select';
import { toast } from '@/components/ui/toast';
import { resetOnOpen } from '@/lib/useHeld';
import { Due, ErrorNote, LabelValue, LabelValueGrid, Note, Section, SectionHeader } from '@/components/app/primitives';
import { FormDialog } from '@/components/app/dialogs';
import { TzInput } from '@/components/app/TzInput';

export function BillingTab({ m }: { m: MachineView }) {
  const s = m.subscription;
  const renewals = useRenewals(m.id);
  const [renew, setRenew] = React.useState(false);
  const rv = remainingValue(m.subscription); const q = quota(m);
  const h = m.host_machine;
  return (
    <>
      {/* 同机 IP 节点：费用默认记在宿主机、不单独记；流量额度和宿主机共用（宿主机的已用含它的流量） */}
      {h && (
        <Section>
          <Note>同机 IP 节点默认不单独记费用，费用记在宿主机「{h.name}」；供应商对附加 IP 另收费时可以在这里单独填。流量额度与宿主机共用：宿主机的「本周期已用」含这台节点的流量。</Note>
        </Section>
      )}
      {s && (
        <Section>
          <SectionHeader title="当前订阅" />
          <LabelValueGrid>
            <LabelValue label="费用"><span className="num">{F.money(s.price_cents, s.currency)}</span> <span className="unit">/ {cycleLabel(s.cycle)}</span>
              {m.sub_template && <div className="hint">{(['price_cents', 'currency', 'cycle'] as const).some((k) => isOverridden(s, k)) ? '单独设置' : inheritSource(m, 'price_cents')}</div>}</LabelValue>
            <LabelValue label="到期日">{s.due_date}（<Due days={s.due_days} state={s.due_state} />）</LabelValue>
            <LabelValue label="剩余价值" wrap>{rv ? <><span className="num">{F.money(rv.cents, rv.currency)}</span> <span className="unit">{rv.frac > 1 ? `已预付 ${rv.frac.toFixed(1)} 期 · 每期 ${rv.days} 天` : `本期还剩 ${Math.round(rv.frac * 100)}%（剩 ${Math.max(0, s.due_days)} 天 / 共 ${rv.days} 天）`}</span></> : <span className="t-muted">—</span>}</LabelValue>
            <LabelValue label={m.ip_nodes.length ? '本周期已用（含同机 IP 节点）' : '本周期已用'} wrap>
              <span className="num">{F.str(quotaGb(q.used))}</span> <span className="unit">/ {q.limit > 0 ? F.str(limitGb(q.limit)) : q.shared ? '与宿主机共用' : '不限'}</span>
              <div className="hint">{q.rule ? trafficRuleLabel(q.rule) : '业务流量（没有网卡读数）'}{q.baseAt && ` · 已校准 · ${F.dt(q.baseAt)}`}</div>
            </LabelValue>
            <LabelValue label="流量周期"><span className="num">{q.start.slice(5)}</span> 起</LabelValue>
            <LabelValue label="自动续费">{s.auto_renew ? '是' : '否'}</LabelValue>
          </LabelValueGrid>
          <div style={{ marginTop: 12 }}><Button variant="primary" onClick={() => setRenew(true)}><CalendarPlus />登记续费</Button></div>
        </Section>
      )}
      <Section>
        <SectionHeader title={s ? '修改订阅' : '设置订阅'} />
        {!s && !h && <div style={{ marginBottom: 12 }}><Note>这台机器还没有订阅信息。填写后会参与到期提醒与费用统计。{m.plan ? '有套餐时只需填到期日。' : ''}</Note></div>}
        {m.sub_template && <div className="hint" style={{ marginBottom: 12 }}>没点「单独设置」的项跟随{m.plan ? `套餐「${m.plan.name}」` : '默认值'}{m.provider ? `与供应商「${m.provider.name}」` : ''}：在「团队与套餐」里改了，这里立即跟着变。</div>}
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
                  <tr key={r.id}><td>{F.dt(r.created_at)}</td><td>{r.old_due ?? '—'} → {r.new_due}</td><td className="r">{F.money(r.amount_cents, r.currency ?? s.currency)}</td><td>{r.note ?? ''}</td></tr>
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

/** 可以单独设置的几项（表单里的顺序） */
const OV_FIELDS: SubField[] = ['price_cents', 'currency', 'cycle', 'billing_tz', 'remind_days', 'traffic_gb', 'traffic_reset_day'];

/** 可跟随的一项：跟随时显示套餐 / 供应商给的值（不可编辑），点「单独设置」才能改；单独设置时注明跟随的值，可改回跟随 */
function OvField({ m, k, label, on, setOn, due, error, locked, children }: {
  m: MachineView; k: SubField; label: string; on: boolean; setOn: (on: boolean) => void; due: string; error?: string; locked?: boolean; children: React.ReactNode;
}) {
  // 同机 IP 节点没有套餐：流量上限跟随的是宿主机的额度
  const follow = k === 'traffic_gb' && m.host_machine && !m.plan ? '与宿主机共用额度' : inheritText(m, k, due);
  const toggle = !locked && follow != null && (
    <Button variant="link" size="sm" onClick={() => setOn(!on)}>{on ? `改回${inheritSource(m, k) === '默认' ? '默认' : inheritSource(m, k)}` : '单独设置'}</Button>
  );
  return (
    <Field label={label} error={on ? error : undefined} hint={<span className="row" style={{ gap: 8 }}>{on && follow != null && <span>单独设置 · {follow}</span>}{toggle}</span>}>
      {on ? children : <Input disabled value={follow ?? ''} aria-label={label} />}
    </Field>
  );
}

function SubForm({ m }: { m: MachineView }) {
  const s = m.subscription;
  const save = useSaveSubscription();
  // 旧接口没有模板：各项都按单独设置填（同以前）。没有套餐时价格没有可跟随的，必须单独填
  const legacy = !m.sub_template;
  const priceLocked = legacy || m.sub_template?.price_cents == null;
  const [ov, setOv] = React.useState<Record<SubField, boolean>>(() =>
    Object.fromEntries(OV_FIELDS.map((k) => [k, legacy || isOverridden(s, k) || (k === 'price_cents' && priceLocked)])) as Record<SubField, boolean>);
  const setOn = (k: SubField) => (on: boolean) => setOv((o) => ({ ...o, [k]: on }));
  const [dueDate, setDueDate] = React.useState(s?.due_date ?? '');
  // 各项的输入：单独设置过的取当前值，否则取跟随的值（点「单独设置」时从它改起）
  const init = (k: SubField) => {
    const v = s && (legacy || isOverridden(s, k)) ? s[k] : inheritedValue(m, k, s?.due_date);
    return v == null ? '' : k === 'price_cents' ? String(Number(v) / 100) : String(v);
  };
  const [price, setPrice] = React.useState(() => init('price_cents'));
  // 都没有时（旧接口没有模板）用 /meta 的默认值；账单时区默认组织时区
  const [currency, setCurrency] = React.useState(() => init('currency') || meta().defaults.currency);
  const [cycle, setCycle] = React.useState(() => init('cycle') || meta().defaults.cycle);
  const [tz, setTz] = React.useState(() => init('billing_tz') || meta().org_timezone);
  const [remind, setRemind] = React.useState(() => init('remind_days'));
  const [traffic, setTraffic] = React.useState(() => init('traffic_gb'));
  const [resetDay, setResetDay] = React.useState(() => init('traffic_reset_day'));
  const [auto, setAuto] = React.useState(s?.auto_renew ?? false);
  // 按面板校准本周期已用：留空 = 不动原有校准
  const [used, setUsed] = React.useState('');
  const priceNum = Number(price);
  // 保存按钮不可用时要能看出原因：空着提示必填，填错了标红（只看单独设置的项）
  const priceBad = ov.price_cents && (price.trim() === '' || !(isFinite(priceNum) && priceNum >= 0));
  const remindBad = ov.remind_days && !(remind !== '' && Number(remind) <= 60);
  const resetBad = ov.traffic_reset_day && !(Number(resetDay) >= 1 && Number(resetDay) <= 31);
  const trafficBad = ov.traffic_gb && traffic === '';
  const tzBad = ov.billing_tz && tz === '';
  const dueBad = dueDate !== '' && !/^\d{4}-\d{2}-\d{2}$/.test(dueDate);
  const usedBad = used.trim() !== '' && !(isFinite(Number(used)) && Number(used) >= 0);
  const valid = dueDate !== '' && !dueBad && !priceBad && !remindBad && !resetBad && !trafficBad && !tzBad && !usedBad;
  const baseAt = m.quota.base_at;
  const submit = () => {
    const val: Record<SubField, string | number> = {
      price_cents: Math.round(priceNum * 100), currency, cycle, billing_tz: tz, remind_days: Number(remind), traffic_gb: Number(traffic), traffic_reset_day: Number(resetDay),
    };
    const body: SubscriptionBody = {
      due_date: dueDate, auto_renew: auto, ...Object.fromEntries(OV_FIELDS.map((k) => [k, ov[k] ? val[k] : null])),
      ...(used.trim() !== '' && { traffic_used_gb: Number(used) }),
    };
    save.mutate({ id: m.id, ...body }, { onSuccess: () => { setUsed(''); toast.ok('订阅已保存'); }, onError: toast.err });
  };
  const o = (k: SubField, label: string, children: React.ReactNode, error?: string) => (
    <OvField m={m} k={k} label={label} on={ov[k]} setOn={setOn(k)} due={dueDate} error={error} locked={legacy || (k === 'price_cents' && priceLocked)}>{children}</OvField>
  );
  return (
    <div className="form-grid">
      <Field label="到期日" hint={dueDate === '' ? '必填' : undefined} error={dueBad ? '格式应为 YYYY-MM-DD' : undefined}><DateField value={dueDate} onChange={setDueDate} /></Field>
      {o('price_cents', '费用', <Input inputMode="decimal" value={price} onChange={(e) => setPrice(e.target.value)} placeholder="68" aria-invalid={priceBad || undefined} />, priceBad ? (price.trim() === '' ? '必填，免费填 0' : '应为 0 或正数') : undefined)}
      {o('currency', '币种', <Select value={currency} onChange={setCurrency} options={F.withCurrent(currencyOptions(), currency)} />)}
      {o('cycle', '计费周期', <Select value={cycle} onChange={setCycle} options={cycleOptions()} />)}
      {o('billing_tz', '账单时区（到期按它的日期算）', <TzInput value={tz} onChange={setTz} aria-label="账单时区" />, tzBad ? '填 IANA 时区名，如 Asia/Shanghai' : undefined)}
      {o('remind_days', '提前提醒（天）', <Input inputMode="numeric" value={remind} onChange={(e) => setRemind(e.target.value.replace(/\D/g, ''))} />, remindBad ? '填 0–60' : undefined)}
      {o('traffic_gb', '流量上限（GB，0 = 不限）', <Input inputMode="numeric" value={traffic} onChange={(e) => setTraffic(e.target.value.replace(/\D/g, ''))} />, trafficBad ? '填 GB 数，0 = 不限' : undefined)}
      {o('traffic_reset_day', '流量重置日（每月几号，按账单时区）', <Input inputMode="numeric" maxLength={2} value={resetDay} onChange={(e) => setResetDay(e.target.value.replace(/\D/g, ''))} />, resetBad ? '填 1–31（月份没有这一天按月末）' : undefined)}
      <Field label="按面板校准本周期已用（GB）" className="span-2" error={usedBad ? '应为 0 或正数' : undefined}
        hint={`填供应商面板上本周期已用的数字；之后的用量按${m.quota.rule ? '网卡流量' : '业务流量'}累加${baseAt ? `。已校准 · ${F.dt(baseAt)}` : ''}`}>
        <Input inputMode="decimal" value={used} onChange={(e) => setUsed(e.target.value)} placeholder="留空 = 不校准" aria-invalid={usedBad || undefined} />
      </Field>
      <label className="check-row span-2" style={{ gap: 10 }}><Switch checked={auto} onCheckedChange={setAuto} /><span>到期自动续费（只用于提醒口径，不会真的扣款）</span></label>
      <div className="span-2 row" style={{ justifyContent: 'flex-end' }}><Button variant="primary" disabled={!valid || save.isPending} loading={save.isPending} onClick={submit}>保存订阅</Button></div>
    </div>
  );
}

/** 告警页「即将到期 / 已过期」的行操作也用这个。每次打开重新挂载（resetOnOpen）：表单按当前订阅预填，关闭动画期间不变 */
export const RenewDialog = resetOnOpen(function RenewDialog({ m, s, open, onOpenChange }: { m: Pick<MachineView, 'id' | 'name'>; s: Subscription; open: boolean; onOpenChange: (o: boolean) => void }) {
  const renew = useRenew();
  // 默认续一个周期：服务端算好的 next_due
  const [newDue, setNewDue] = React.useState(s.next_due);
  const [amount, setAmount] = React.useState(() => String(s.price_cents / 100));
  const [note, setNote] = React.useState('');
  const amountBad = amount.trim() !== '' && !(isFinite(Number(amount)) && Number(amount) >= 0);
  const ok = /^\d{4}-\d{2}-\d{2}$/.test(newDue) && newDue > s.due_date && amount.trim() !== '' && !amountBad;
  const submit = () => renew.mutate({ id: m.id, new_due: newDue, amount_cents: Math.round(Number(amount) * 100), note: note.trim() || undefined }, { onSuccess: () => { toast.ok(`已登记续费，到期日 ${newDue}`); onOpenChange(false); }, onError: toast.err });
  return (
    <FormDialog open={open} onOpenChange={onOpenChange} dirty={!!note.trim() || amount !== String(s.price_cents / 100) || newDue !== s.next_due} title={`登记续费 · ${m.name}`} desc={`只记录「已续费」，到期日顺延；不会向供应商付款。当前到期日 ${s.due_date}。`}
      submitLabel="登记续费" busy={renew.isPending} valid={ok} onSubmit={submit}>
      <div className="form-grid">
        <Field label="新到期日" error={newDue && newDue <= s.due_date ? '应晚于当前到期日' : undefined}><DateField value={newDue} onChange={setNewDue} min={F.addDays(s.due_date, 1)} /></Field>
        <Field label={`实付金额（${s.currency}）`} error={amountBad ? '应为 0 或正数' : amount.trim() === '' ? '请填写金额，免费填 0' : undefined}><Input inputMode="decimal" value={amount} onChange={(e) => setAmount(e.target.value)} aria-invalid={amountBad || undefined} /></Field>
        <Field label="备注（选填）" className="span-2"><Input value={note} onChange={(e) => setNote(e.target.value)} placeholder="例如：账单 #2041" /></Field>
      </div>
    </FormDialog>
  );
});
