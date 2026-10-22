// 设置「数据保留」：访问记录的保留期——原始记录（每条连接）与小时汇总（统计、曲线）各保留多少天。
// 调低会让中控删掉超出新保留期的数据，删了不能恢复：调低时先确认，说清楚删到哪天。
import * as React from 'react';
import { Archive } from 'lucide-react';
import { useAccessRetention, useSaveRetention } from '@/lib/access';
import { Button } from '@/components/ui/button';
import { Field, InfoTip, Input } from '@/components/ui/form';
import { toast } from '@/components/ui/toast';
import { ConfirmDialog } from '@/components/app/dialogs';
import { ErrorNote, Panel } from '@/components/app/primitives';
import { SkeletonRows } from '@/components/ui/skeleton';

export function RetentionTab() {
  const q = useAccessRetention();
  return (
    <Panel title={<>访问记录保留期<InfoTip className="ml-1 align-[-3px]" text="访问监控的数据存多久；超过保留期的每天自动清理" /></>}>
      {q.isError && <ErrorNote error={q.error} onRetry={() => void q.refetch()} />}
      {q.isLoading ? <SkeletonRows rows={2} /> : q.data && <RetentionForm key={`${q.data.raw_days}-${q.data.hourly_days}`} r={q.data} />}
    </Panel>
  );
}

function RetentionForm({ r }: { r: { raw_days: number; hourly_days: number; min: number; max: number } }) {
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
  return (
    <>
      <div className="form-grid">
        <Field label="原始记录（天）" info="每一条连接：访问监控的「原始记录」页签按它查" error={raw !== '' ? rawErr : undefined}>
          <Input inputMode="numeric" value={raw} onChange={(e) => setRaw(e.target.value.replace(/\D/g, ''))} />
        </Field>
        <Field label="小时汇总（天）" info="按小时汇总的统计：域名、来源、节点概览和曲线。「30 天」时间范围要它至少 30 天；不能比原始记录短" error={hourly !== '' ? hourlyErr : undefined}>
          <Input inputMode="numeric" value={hourly} onChange={(e) => setHourly(e.target.value.replace(/\D/g, ''))} />
        </Field>
      </div>
      <div className="row" style={{ justifyContent: 'flex-end', marginTop: 12 }}>
        <Button variant="primary" disabled={!dirty || !!rawErr || !!hourlyErr} loading={save.isPending} onClick={() => (lowering ? setAsk(true) : submit())}><Archive />保存</Button>
      </div>
      <ConfirmDialog open={ask} onOpenChange={setAsk} danger confirmLabel="调低并删除" title="调低保留期？"
        description={<>{[
          rawN < r.raw_days && `原始记录 ${rawN} 天以前的会被删掉。`,
          hourlyN < r.hourly_days && `小时汇总 ${hourlyN} 天以前的会被删掉。`,
          '不能恢复。',
        ].filter(Boolean).join('')}<InfoTip className="ml-1 align-[-3px]" text={`下一次清理时删除。${hourlyN < r.hourly_days ? '\n小时汇总删掉后，更长时间范围的统计随之不可用。' : ''}`} /></>}
        onConfirm={async () => { await save.mutateAsync({ raw_days: rawN, hourly_days: hourlyN }); toast.ok('保留期已保存'); }} />
    </>
  );
}
