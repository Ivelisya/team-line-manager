// 抽屉“费用”页：订阅信息编辑 + 登记续费 + 续费记录。
import * as React from 'react';
import { CalendarPlus, Pencil, Scale } from 'lucide-react';
import type { MachineView, SubField, Subscription } from '@/lib/types';
import { useRenew, useRenewals, useSaveSubscription, type SubscriptionBody } from '@/lib/queries';
import * as F from '@/lib/format';
import { Button } from '@/components/ui/button';
import { Field, InfoTip, Input, Label, Switch, UnitInput } from '@/components/ui/form';
import { Segmented } from '@/components/ui/overlays';
import { DateField } from '@/components/ui/date-field';
import { useUsdPreview } from '@/lib/fx';
import { inheritedValue, inheritSource, inheritText, inheritValueText, isOverridden, limitGb, monthFlow, monthFlowTip, quota, quotaCycleText, quotaForecast, quotaGb, quotaNotes, quotaRuleText, quotaUsedLabel, recycleOn, remainingValue, cycleLabel, feeView, priceHint, currencyOptions, cycleOptions, meta } from '@/lib/derive';
import { Select } from '@/components/ui/select';
import { toast } from '@/components/ui/toast';
import { resetOnOpen } from '@/lib/useHeld';
import { Due, DueDate, ErrorNote, FromTag, LabelValue, LabelValueGrid, Note, Num, Section, SectionHeader, SpareDue } from '@/components/app/primitives';
import { FormDialog } from '@/components/app/dialogs';
import { RebalanceDialog } from '@/components/app/RebalanceDialog';
import { TzInput } from '@/components/app/TzInput';

export function BillingTab({ m }: { m: MachineView }) {
  const s = m.subscription;
  const renewals = useRenewals(m.id);
  const [renew, setRenew] = React.useState(false);
  const [rebalance, setRebalance] = React.useState(false);
  // 已有订阅时修改表单默认收起：表单里的值和上面「当前订阅」一样，一屏里不重复列两遍；点「修改订阅」才展开
  const [editing, setEditing] = React.useState(false);
  const formRef = React.useRef<HTMLElement>(null);
  React.useEffect(() => { if (editing) formRef.current?.scrollIntoView({ block: 'nearest', behavior: 'smooth' }); }, [editing]);
  const rv = remainingValue(m.subscription); const q = quota(m); const fl = monthFlow(m);
  const h = m.host_machine;
  return (
    <>
      {/* 同机 IP 节点：费用默认记在宿主机、不单独记；流量额度和宿主机共用（宿主机的已用含它的流量） */}
      {h && (
        <Section>
          <Note>费用默认记在宿主机「{h.name}」 <InfoTip text={'同机 IP 节点默认不单独记费用；供应商对附加 IP 另收费时可以在这里单独填。\n流量额度与宿主机共用：宿主机的「本周期已用」含这台节点的流量。'} /></Note>
        </Section>
      )}
      {s && (
        <Section>
          <SectionHeader title="当前订阅" />
          <LabelValueGrid>
            {/* 计费统一按 USD（feeView）：主数字是服务端折好的月费；原价（供应商收的币种与周期）与汇率在下面一行，没有汇率的主数字写原币种月费、下面一行写未折算 */}
            <LabelValue label="费用"><span className="num">{feeView(s).main}</span> <span className="unit">{feeView(s).unit}</span>
              <div className="hint">{[priceHint(s), (['price_cents', 'currency', 'cycle'] as const).some((k) => isOverridden(s, k)) ? '单独设置' : inheritSource(m, 'price_cents')].filter(Boolean).join(' · ')}</div></LabelValue>
            <LabelValue label="到期日" wrap><DueDate sub={s} />（<Due days={s.due_days} state={s.due_state} />）{recycleOn(m) && <div className="hint"><SpareDue on={recycleOn(m)!} /></div>}</LabelValue>
            {/* 剩余比例只写一个百分数，天数在悬停里；预付了多期只写金额，期数在 (i) 里 */}
            <LabelValue label="剩余价值">{rv ? <><span className="num">{F.money(rv.cents, rv.currency)}</span> {rv.frac > 1
              ? <InfoTip text={`已预付 ${rv.frac.toFixed(1)} 期 · 每期 ${rv.days} 天`} />
              : <span className="unit" data-tip={`剩 ${Math.max(0, s.due_days)} 天 / 共 ${rv.days} 天`}>本期剩 {Math.round(rv.frac * 100)}%</span>}</> : <span className="t-muted">—</span>}</LabelValue>
            <LabelValue label="自动续费">{s.auto_renew ? '是' : '否'}</LabelValue>
            {/* 有额度：本周期已用 + 规则 · 网卡，流量周期；不限流量不评估周期，只写本月；同机 IP 节点指向宿主机 */}
            {q.limit > 0 ? <>
              {/* 流量相关的两项各占一行的两列：数字、单位与「X 起」不折行 */}
              <LabelValue label={m.ip_nodes.length ? `${quotaUsedLabel(q)}（含同机 IP 节点）` : quotaUsedLabel(q)} wrap className="span-2">
                <span className="nowrap" data-over={q.pct != null && q.pct >= 100 ? '' : undefined} data-tip={q.pct != null && q.pct >= 100 ? `已超出额度（${Math.floor(q.pct)}%）` : undefined}><span className="num">{F.str(quotaGb(q.used))}</span> <span className="unit">/ {F.str(limitGb(q.limit))}</span><FromTag from={q.from} start={q.start} /></span> <InfoTip text={[quotaRuleText(m), ...quotaNotes(m)].join('\n')} />
              </LabelValue>
              <LabelValue label="流量周期" wrap className="span-2"><span className="nowrap">{quotaCycleText(m)}</span></LabelValue>
              {quotaForecast(m) && <LabelValue label="用满预测" wrap className="span-2"><span className="nowrap due" data-tone="warn">{quotaForecast(m)}</span> <InfoTip text="按近 7 天的日均用量估算，早于重置日才显示" /></LabelValue>}
            </> : q.shared ? <LabelValue label="流量额度">与宿主机共用</LabelValue> : (
              <LabelValue label="本月流量" wrap className="span-2">
                <span className="nowrap">{fl.kind === 'nic' ? <>↓ 下行 <Num f={F.bytes(fl.down)} /> · ↑ 上行 <Num f={F.bytes(fl.up)} /></> : <>下载 <Num f={F.bytes(fl.down)} /> · 上传 <Num f={F.bytes(fl.up)} /></>}<FromTag from={fl.from} /></span>
                <div className="hint">{q.unset ? '未设置流量上限' : '流量不限'} <InfoTip text={monthFlowTip(m)} /></div>
              </LabelValue>
            )}
          </LabelValueGrid>
          <div className="row" style={{ marginTop: 12, gap: 8 }}>
            <Button onClick={() => setRenew(true)}><CalendarPlus />登记续费</Button>
            {!editing && <Button onClick={() => setEditing(true)}><Pencil />修改订阅</Button>}
            {m.kind === 'relay' && q.limit > 0 && !q.rolling && <Button onClick={() => setRebalance(true)}><Scale />搬迁建议</Button>}
          </div>
          {m.kind === 'relay' && <RebalanceDialog relay={m} open={rebalance} onOpenChange={setRebalance} />}
        </Section>
      )}
      {(!s || editing) && (
        <section className="sec" ref={formRef}>
          <SectionHeader title={<>{s ? '修改订阅' : '设置订阅'} <InfoTip text={`选「跟随」的项跟随${m.plan ? `套餐「${m.plan.name}」` : '默认值'}${m.provider ? `与供应商「${m.provider.name}」` : ''}：在「团队与套餐」里改了，这里立即跟着变。`} /></>}>
            {s && <Button variant="quiet" size="sm" onClick={() => setEditing(false)}>收起</Button>}
          </SectionHeader>
          {!s && !h && <div style={{ marginBottom: 12 }}><Note>还没有订阅信息 <InfoTip text={`填写后会参与到期提醒与费用统计。${m.plan ? '有套餐时只需填到期日。' : ''}`} /></Note></div>}
          <SubForm key={m.id + (s?.due_date ?? '')} m={m} />
        </section>
      )}
      {s && (
        <Section>
          <SectionHeader title="续费记录" />
          {renewals.data?.length ? (
            <div className="table-wrap is-boxed">
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

const OV_ITEMS = [{ value: 'follow', label: '跟随' }, { value: 'own', label: '单独' }];
/** 可跟随的一项：标签右边一个紧凑的「跟随 / 单独」切换。跟随时显示套餐 / 供应商给的值（灰色文字，不是输入框），切到「单独」才能改；
 *  跟随的来源与值在切换的悬停里（提交的字段不变：单独的带值，跟随的为 null） */
function OvField({ m, k, label, info, on, setOn, due, error, hint, locked, onTouch, children }: {
  m: MachineView; k: SubField; label: string; info?: string; on: boolean; setOn: (on: boolean) => void; due: string; error?: string; hint?: string | null; locked?: boolean; onTouch: () => void; children: React.ReactElement;
}) {
  const id = React.useId();
  // 同机 IP 节点没有套餐：流量上限跟随的是宿主机的额度
  const shared = k === 'traffic_gb' && m.host_machine && !m.plan;
  const follow = shared ? '与宿主机共用额度' : inheritText(m, k, due);
  const toggle = !locked && (
    <span data-tip={follow}><Segmented sm label={`${label}：跟随或单独设置`} value={on ? 'own' : 'follow'} onChange={(v) => setOn(v === 'own')} items={OV_ITEMS} /></span>
  );
  return (
    <div className="field" onBlur={onTouch}>
      <div className="ov-head">
        <span className="field-label"><Label htmlFor={id}>{label}</Label>{info && <InfoTip text={info} />}</span>
        {toggle}
      </div>
      {on ? React.cloneElement(children as React.ReactElement<{ id?: string }>, { id }) : <div id={id} className="ov-follow">{shared ? follow : inheritValueText(m, k, due)}</div>}
      {on && error && <div className="hint is-error">{error}</div>}
      {on && !error && hint && <div className="hint">{hint}</div>}
    </div>
  );
}

function SubForm({ m }: { m: MachineView }) {
  const s = m.subscription;
  const save = useSaveSubscription();
  // 没有套餐时价格没有可跟随的，必须单独填
  const priceLocked = m.sub_template.price_cents == null;
  const [ov, setOv] = React.useState<Record<SubField, boolean>>(() =>
    Object.fromEntries(OV_FIELDS.map((k) => [k, isOverridden(s, k) || (k === 'price_cents' && priceLocked)])) as Record<SubField, boolean>);
  const setOn = (k: SubField) => (on: boolean) => setOv((o) => ({ ...o, [k]: on }));
  const [dueDate, setDueDate] = React.useState(s?.due_date ?? '');
  // 必填 / 填错的提示等用户离开那一项再出现（一打开就满屏红字像是已经出错了）；到期日在离开表单里别的项时才算离开
  const [touched, setTouched] = React.useState<Record<string, boolean>>({});
  const touch = (k: string) => () => setTouched((t) => (t[k] ? t : { ...t, [k]: true }));
  // 各项的输入：单独设置过的取当前值，否则取跟随的值（点「单独设置」时从它改起）
  const init = (k: SubField) => {
    const v = s && isOverridden(s, k) ? s[k] : inheritedValue(m, k, s?.due_date);
    return v == null ? '' : k === 'price_cents' ? String(Number(v) / 100) : String(v);
  };
  const [price, setPrice] = React.useState(() => init('price_cents'));
  // 都没有时用 /meta 的默认值；账单时区默认组织时区
  const [currency, setCurrency] = React.useState(() => init('currency') || meta().defaults.currency);
  const [cycle, setCycle] = React.useState(() => init('cycle') || meta().defaults.cycle);
  const [tz, setTz] = React.useState(() => init('billing_tz') || meta().org_timezone);
  const [remind, setRemind] = React.useState(() => init('remind_days'));
  const [traffic, setTraffic] = React.useState(() => init('traffic_gb'));
  const [resetDay, setResetDay] = React.useState(() => init('traffic_reset_day'));
  const [auto, setAuto] = React.useState(s?.auto_renew ?? false);
  const priceNum = Number(price);
  // 保存按钮不可用时要能看出原因：空着提示必填，填错了标红（只看单独设置的项）
  const priceBad = ov.price_cents && (price.trim() === '' || !(isFinite(priceNum) && priceNum >= 0));
  const remindBad = ov.remind_days && !(remind !== '' && Number(remind) <= 60);
  const resetBad = ov.traffic_reset_day && !(Number(resetDay) >= 1 && Number(resetDay) <= 31);
  const trafficBad = ov.traffic_gb && traffic === '';
  const tzBad = ov.billing_tz && tz === '';
  const dueBad = dueDate !== '' && !/^\d{4}-\d{2}-\d{2}$/.test(dueDate);
  const valid = dueDate !== '' && !dueBad && !priceBad && !remindBad && !resetBad && !trafficBad && !tzBad;
  const val: Record<SubField, string | number> = {
    price_cents: Math.round(priceNum * 100), currency, cycle, billing_tz: tz, remind_days: Number(remind), traffic_gb: Number(traffic), traffic_reset_day: Number(resetDay),
  };
  // 没改动时不能保存：和上一次保存（或打开时）提交的内容比
  const sig = JSON.stringify([dueDate, auto, OV_FIELDS.map((k) => (ov[k] ? val[k] : null))]);
  const saved = React.useRef(sig);
  const submit = () => {
    const body: SubscriptionBody = {
      due_date: dueDate, auto_renew: auto, ...Object.fromEntries(OV_FIELDS.map((k) => [k, ov[k] ? val[k] : null])),
    };
    save.mutate({ id: m.id, ...body }, { onSuccess: () => { saved.current = sig; toast.ok('订阅已保存'); }, onError: toast.err });
  };
  // 费用框里的单位：生效的币种 / 周期（单独设置的取输入，跟随的取套餐 / 供应商的）
  const priceCur = ov.currency ? currency : m.sub_template.currency;
  const priceUnit = `${priceCur} / ${cycleLabel(ov.cycle ? cycle : m.sub_template.cycle)}`;
  // 价格按供应商收的币种录入，旁边提示折合多少美元（按设置里的汇率）
  const usdHint = useUsdPreview(price.trim() === '' || priceBad ? null : Math.round(priceNum * 100), priceCur);
  const o = (k: SubField, label: string, children: React.ReactElement, error?: string, info?: string) => (
    <OvField m={m} k={k} label={label} info={info} on={ov[k]} setOn={setOn(k)} due={dueDate} error={touched[k] ? error : undefined} hint={k === 'price_cents' ? usdHint : undefined} onTouch={touch(k)} locked={k === 'price_cents' && priceLocked}>{children}</OvField>
  );
  return (
    <div className="form-grid sub-form" onBlur={(e) => { if (!(e.target as HTMLElement).closest('[data-due]')) touch('due')(); }}>
      <Field label="到期日" hint={touched.due && dueDate === '' ? '必填' : undefined} error={dueBad ? '格式应为 YYYY-MM-DD' : undefined}><div data-due><DateField value={dueDate} onChange={setDueDate} /></div></Field>
      {o('price_cents', '费用', <UnitInput width="full" unit={priceUnit} style={{ paddingRight: `calc(var(--sp-10) + ${priceUnit.length * 0.62 + 0.4}em)` }} inputMode="decimal" value={price} onChange={(e) => setPrice(e.target.value)} placeholder="68" aria-invalid={(touched.price_cents && priceBad) || undefined} />, priceBad ? (price.trim() === '' ? '必填，免费填 0' : '应为 0 或正数') : undefined)}
      {o('currency', '币种', <Select value={currency} onChange={setCurrency} options={F.withCurrent(currencyOptions(), currency)} />)}
      {o('cycle', '计费周期', <Select value={cycle} onChange={setCycle} options={cycleOptions()} />)}
      {o('billing_tz', '账单时区', <TzInput value={tz} onChange={setTz} aria-label="账单时区" />, tzBad ? '填 IANA 时区名，如 Asia/Shanghai' : undefined, '到期日按这个时区的日期算')}
      {o('remind_days', '提前提醒（天）', <Input inputMode="numeric" value={remind} onChange={(e) => setRemind(e.target.value.replace(/\D/g, ''))} />, remindBad ? '填 0–60' : undefined)}
      {o('traffic_gb', '流量上限（GB）', <Input inputMode="numeric" value={traffic} onChange={(e) => setTraffic(e.target.value.replace(/\D/g, ''))} placeholder="0 = 不限" />, trafficBad ? '填 GB 数，0 = 不限' : undefined)}
      {o('traffic_reset_day', '流量重置日', <Input inputMode="numeric" maxLength={2} value={resetDay} onChange={(e) => setResetDay(e.target.value.replace(/\D/g, ''))} placeholder="1–31" />, resetBad ? '填 1–31' : undefined, '每月几号重置，按账单时区；月份没有这一天按月末')}
      <div className="row span-2" style={{ gap: 4 }}><label className="check-row" style={{ gap: 10 }}><Switch checked={auto} onCheckedChange={setAuto} /><span>到期自动续费</span></label><InfoTip text="只用于提醒口径，不会真的扣款" /></div>
      <div className="span-2 row" style={{ justifyContent: 'flex-end' }}><Button variant="primary" disabled={!valid || sig === saved.current || save.isPending} loading={save.isPending} onClick={submit}>保存订阅</Button></div>
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
  const usdHint = useUsdPreview(amount.trim() === '' || amountBad ? null : Math.round(Number(amount) * 100), s.currency);
  const submit = () => renew.mutate({ id: m.id, new_due: newDue, amount_cents: Math.round(Number(amount) * 100), note: note.trim() || undefined }, { onSuccess: () => { toast.ok(`已登记续费，到期日 ${newDue}`); onOpenChange(false); }, onError: toast.err });
  return (
    <FormDialog open={open} onOpenChange={onOpenChange} dirty={!!note.trim() || amount !== String(s.price_cents / 100) || newDue !== s.next_due} title={`登记续费 · ${m.name}`} desc={<>当前到期日 {s.due_date} <InfoTip text="只记录「已续费」，到期日顺延；不会向供应商付款。" /></>}
      submitLabel="登记续费" busy={renew.isPending} valid={ok} onSubmit={submit}>
      <div className="form-grid">
        <Field label="新到期日" error={newDue && newDue <= s.due_date ? '应晚于当前到期日' : undefined}><DateField value={newDue} onChange={setNewDue} min={F.addDays(s.due_date, 1)} /></Field>
        <Field label={`实付金额（${s.currency}）`} error={amountBad ? '应为 0 或正数' : amount.trim() === '' ? '请填写金额，免费填 0' : undefined}><Input inputMode="decimal" value={amount} onChange={(e) => setAmount(e.target.value)} aria-invalid={amountBad || undefined} />{usdHint && <div className="hint">{usdHint}</div>}</Field>
        <Field label="备注（选填）" className="span-2"><Input value={note} onChange={(e) => setNote(e.target.value)} placeholder="例如：账单 #2041" /></Field>
      </div>
    </FormDialog>
  );
});
