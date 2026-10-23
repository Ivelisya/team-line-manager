// 设置「数据保留」：访问记录的保留期——原始记录（每条连接）与小时汇总（统计、曲线）各保留多少天。
// 调低后中控马上删掉超出新保留期的数据，删了不能恢复：调低时先确认，说清楚删到哪天。
import * as React from 'react';
import { useAccessRetention, useSaveRetention, type AccessRetention } from '@/lib/access';
import { Button } from '@/components/ui/button';
import { Field, InfoTip, UnitInput } from '@/components/ui/form';
import { toast } from '@/components/ui/toast';
import { ConfirmDialog } from '@/components/app/dialogs';
import { ErrorNote, Panel } from '@/components/app/primitives';
import { SkeletonRows } from '@/components/ui/skeleton';

export function RetentionTab() {
  const q = useAccessRetention();
  return (
    <Panel title={<>访问记录保留期<InfoTip text="访问数据保存多久。超过的会每天自动清理" /></>}>
      {q.isError && <ErrorNote error={q.error} onRetry={() => void q.refetch()} />}
      {q.isLoading ? <SkeletonRows rows={2} /> : q.data && <RetentionForm key={`${q.data.raw_days}-${q.data.hourly_days}`} r={q.data} />}
    </Panel>
  );
}

function RetentionForm({ r }: { r: AccessRetention }) {
  const save = useSaveRetention();
  const [raw, setRaw] = React.useState(String(r.raw_days));
  const [hourly, setHourly] = React.useState(String(r.hourly_days));
  const [ask, setAsk] = React.useState(false);
  const rawN = Number(raw), hourlyN = Number(hourly);
  const range = (n: number) => Number.isInteger(n) && n >= r.min && n <= r.max;
  const rawErr = raw === '' || !range(rawN) ? `填 ${r.min}–${r.max} 的整数` : undefined;
  const hourlyErr = hourly === '' || !range(hourlyN) ? `填 ${r.min}–${r.max} 的整数` : !rawErr && hourlyN < rawN ? '不能比原始记录短' : undefined;
  const dirty = rawN !== r.raw_days || hourlyN !== r.hourly_days;
  const lowering = rawN < r.raw_days || hourlyN < r.hourly_days;
  const submit = () => save.mutate({ raw_days: rawN, hourly_days: hourlyN }, { onSuccess: () => toast.ok('保留期已保存'), onError: toast.err });
  // 确认框写出从几天调到几天：「原始记录 8 → 7 天」
  const lowered = [rawN < r.raw_days && `原始记录 ${r.raw_days} → ${rawN} 天`, hourlyN < r.hourly_days && `小时汇总 ${r.hourly_days} → ${hourlyN} 天`].filter(Boolean).join('，');
  return (
    <>
      {/* 保存在右下：卡片里的表单统一同对话框的「取消 / 确认」，提交放右边、不配图标 */}
      <div className="row" style={{ gap: 'var(--sp-24)', alignItems: 'flex-start' }}>
        <Field label="原始记录" info="每一条连接的记录，「原始记录」页签查的就是它" error={raw !== '' ? rawErr : undefined}>
          <UnitInput unit="天" width={120} inputMode="numeric" maxLength={3} value={raw} onChange={(e) => setRaw(e.target.value.replace(/\D/g, ''))} />
        </Field>
        <Field label="小时汇总" info={"按小时汇总的统计，用于域名、来源、节点概览和曲线\n要看「30 天」范围，至少要保留 30 天；不能比原始记录短"} hint={r.profile_days > r.hourly_days ? `来源画像按学习期留 ${r.profile_days} 天` : undefined} error={hourly !== '' ? hourlyErr : undefined}>
          <UnitInput unit="天" width={120} inputMode="numeric" maxLength={3} value={hourly} onChange={(e) => setHourly(e.target.value.replace(/\D/g, ''))} />
        </Field>
      </div>
      <div className="row" style={{ marginTop: 'var(--sp-12)', justifyContent: 'flex-end' }}>
        <Button variant="primary" disabled={!dirty || !!rawErr || !!hourlyErr} loading={save.isPending} onClick={() => (lowering ? setAsk(true) : submit())}>保存</Button>
      </div>
      <ConfirmDialog open={ask} onOpenChange={setAsk} danger confirmLabel="调低并删除" title="调低保留期？"
        description={<>{lowered}；超出的数据马上删除，不能恢复 <InfoTip text={[
          '保存后马上删除：',
          rawN < r.raw_days && `原始记录：${rawN} 天以前的`,
          hourlyN < r.hourly_days && `小时汇总：${hourlyN} 天以前的（更长时间范围的统计随之不可用）`,
        ].filter(Boolean).join('\n')} /></>}
        onConfirm={async () => { await save.mutateAsync({ raw_days: rawN, hourly_days: hourlyN }); toast.ok('保留期已保存'); }} />
    </>
  );
}
