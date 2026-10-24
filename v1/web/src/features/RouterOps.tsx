// 软路由出口的两个操作，抽屉的出口页签与告警页都能弹（lib/routers 的小仓库 showSync / showSwitch），由外壳里的 RouterDialogsHost 渲染：
// - 同步出口参数：参数和对应入口不一致的出口改成入口现在的凭据（作业 router_sync，会重启 OpenClash）；
// - 临时切换：select 组经 mihomo API 切到组里另一个出口（不重启、不断已有连接），可到时自动切回。
// 抽屉里的「策略组」一段（GroupsSection）也在这里：组 → 当前出口，select 组每组一个「临时切换…」，切换中的可以切回。
import * as React from 'react';
import { ConfirmDialog, FormDialog } from '@/components/app/dialogs';
import { ErrorNote, Note, Section, SectionHeader } from '@/components/app/primitives';
import { Button } from '@/components/ui/button';
import { Checkbox, Field, InfoTip } from '@/components/ui/form';
import { Select } from '@/components/ui/select';
import { Skeleton } from '@/components/ui/skeleton';
import { toast } from '@/components/ui/toast';
import { useHeld, resetOnOpen } from '@/lib/useHeld';
import { showSwitch, showSync, useRouter, useSwitchBack, useSwitchGroup, useSwitchTarget, useSyncPreview, useSyncRouter, useSyncTarget, type RouterGroup, type RouterSwitch, type RouterView, type SwitchTarget, type SyncTarget } from '@/lib/routers';
import * as F from '@/lib/format';

const KIND: Record<string, string> = { URLTest: 'url-test', Fallback: 'fallback', LoadBalance: 'load-balance', Relay: 'relay', Smart: 'smart' };

/** 抽屉出口页签的「策略组」：select 组一行一个（当前出口，切换中的写原来是谁、几点切回），自动选择的组收成一句 */
export function GroupsSection({ r, groups }: { r: RouterView; groups: RouterGroup[] }) {
  const back = useSwitchBack();
  const sel = groups.filter((g) => g.type === 'Selector');
  const auto = groups.filter((g) => g.type !== 'Selector');
  if (!groups.length) return null;
  const online = r.state === 'online';
  const sw = (g: RouterGroup): RouterSwitch | undefined => r.switches?.find((s) => s.group === g.name);
  return (
    <Section>
      <SectionHeader title={<>策略组<InfoTip text={'临时切换只改 mihomo 当前的选择，不改配置、不重启 OpenClash，已有连接不断\nOpenClash 重启后一般保留当前选择，到时仍由中控切回'} /></>} meta={`${groups.length} 个`} />
      <div className="mgroup">
        {sel.map((g) => {
          const s = sw(g);
          return (
            <div key={g.name} className="manage-row">
              <div style={{ minWidth: 0 }}>
                <div className="t ellipsis">{g.name}</div>
                <div className="hint ellipsis">当前 {g.now ?? '—'}{s && <> · <span className="rt-warn">临时切换中</span>，原来是 {s.from}{s.back_at ? `，${F.dt(s.back_at)} 自动切回` : ''}</>}</div>
              </div>
              <div className="row" style={{ gap: 6, flexWrap: 'nowrap' }}>
                {s && <Button variant="secondary" disabled={!online} loading={back.isPending && back.variables?.group === g.name}
                  onClick={() => back.mutate({ id: r.id, group: g.name }, { onSuccess: (d) => toast.ok(`${g.name} 已切回 ${d.to}`), onError: toast.err })}>切回</Button>}
                <Button variant="secondary" disabled={!online || g.all.length < 2} onClick={() => showSwitch({ id: r.id, name: r.name, group: g.name })}>临时切换…</Button>
              </div>
            </div>
          );
        })}
        {auto.length > 0 && (
          <div className="hint" data-tip={auto.map((g) => `${g.name}（${KIND[g.type] ?? g.type}）→ ${g.now ?? '—'}`).join('\n')}>
            另有 {auto.length} 个自动选择的组，由 mihomo 按延迟自己选，不能手动切换
          </div>
        )}
      </div>
    </Section>
  );
}

/** 同步出口参数：先取预览（要改哪些出口、同步不了的），确认后建作业 */
const SyncDialog = resetOnOpen(function SyncDialog({ open, t }: { open: boolean; t: SyncTarget }) {
  const [force, setForce] = React.useState(false);
  const pv = useSyncPreview(open ? t.id : null, t.exits, force);
  const run = useSyncRouter();
  const p = pv.data;
  const exits = p?.routers[0]?.exits ?? [];
  const needForce = p?.need_force ?? [];
  // 需要勾选确认的那几条不重复写成错误
  const blockers = (p?.blockers ?? []).filter((b) => !needForce.some((u) => b.includes(`「${u.exit}」从没对上过`)));
  return (
    <ConfirmDialog open={open} onOpenChange={(o) => !o && showSync(null)} title={`同步出口参数 · ${t.name}`} confirmLabel="同步" confirmDisabled={!p?.ok}
      description={<>改成对应入口现在的凭据，地址和端口不变<InfoTip text={'先备份配置，改完重启 OpenClash 并测延迟，测不通自动改回\n重启时经这台路由的连接会全部断开、马上重连'} /></>}
      onConfirm={async () => { await run.mutateAsync({ id: t.id, exits: t.exits, force }); toast.ok(`已提交「${t.name}」的出口同步作业`); }}>
      {pv.isLoading ? <Skeleton h={48} /> : pv.isError ? <ErrorNote error={pv.error} onRetry={() => void pv.refetch()} /> : p && (
        <>
          {exits.length > 0 && <p style={{ margin: 0 }}>{exits.map((e) => e.exit).join('、')} <span className="t-sub mono">（{[...new Set(exits.map((e) => e.from))].join('、')}）</span></p>}
          {p.skipped.length > 0 && <Note tone="warn">入口已不在用，同步不了：{p.skipped.join('、')}<InfoTip text="要换成别的入口，用「改出口指向」" /></Note>}
          {p.untrusted.length > 0 && <Note tone="warn">从没对上过对应的入口，可能是路由上有人加的，没有同步：{p.untrusted.map((u) => u.exit).join('、')}<InfoTip text="确认是自己的，在出口页签里单独选它同步" /></Note>}
          {needForce.length > 0 && (
            <label className="check-row"><Checkbox checked={force} onCheckedChange={(v) => setForce(v === true)} />
              <span>{needForce.map((u) => `${u.exit}（${u.entry}，${u.why}）`).join('、')} 从没对上过这个入口，可能是路由上有人加的；确认是自己的，仍要同步</span></label>
          )}
          {!p.ok && blockers.length > 0 && <Note tone="bad">{blockers.join('；')}</Note>}
        </>
      )}
    </ConfirmDialog>
  );
});

function SyncHost() {
  const t = useSyncTarget();
  const held = useHeld(t?.id, t);
  return held ? <SyncDialog open={!!t} t={held} /> : null;
}

const BACK = [
  { value: '3600', label: '1 小时后' }, { value: '14400', label: '4 小时后' }, { value: '43200', label: '12 小时后' }, { value: '86400', label: '1 天后' }, { value: 'never', label: '不自动切回' },
];

/** mihomo 内置的策略（不是出口） */
const BUILTIN = new Map([['DIRECT', '直连，不走代理'], ['REJECT', '拒绝连接'], ['REJECT-DROP', '丢弃连接'], ['PASS', '跳过这条规则'], ['COMPATIBLE', '兼容占位']]);

/** 临时切换：选组（select 组）与要切到的出口（组里的、不是当前的），默认 4 小时后自动切回 */
const SwitchDialog = resetOnOpen(function SwitchDialog({ open, t }: { open: boolean; t: SwitchTarget }) {
  const q = useRouter(t.id);
  const groups = (q.data?.detail?.groups ?? []).filter((g) => g.type === 'Selector');
  const exits = q.data?.detail?.exits ?? [];
  const [group, setGroup] = React.useState(t.group ?? '');
  const [to, setTo] = React.useState(t.to ?? '');
  const [back, setBack] = React.useState('14400');
  const run = useSwitchGroup();
  const g = groups.find((x) => x.name === (group || groups[0]?.name));
  // 出口后面带上延迟，不通的写出来：选的时候一眼看出哪个能用
  const label = (n: string) => {
    const e = exits.find((x) => x.name === n);
    const st = !e ? '' : e.alive === false && e.fail_streak >= 2 ? ' · 不通' : e.delay_ms != null ? ` · ${e.delay_ms} ms` : '';
    return `${n}${st}`;
  };
  // mihomo 内置的 REJECT（拒绝）、DIRECT（直连）等不是出口：排到最后、写明是什么，免得排在第一个被顺手选中
  const opts = (g?.all ?? []).filter((x) => x !== g?.now).sort((a, b) => Number(BUILTIN.has(a)) - Number(BUILTIN.has(b)))
    .map((x) => ({ value: x, label: BUILTIN.has(x) ? `${x}（${BUILTIN.get(x)}）` : label(x) }));
  const valid = !!g && !!to && to !== g.now && g.all.includes(to);
  const submit = () => run.mutate({ id: t.id, group: g!.name, to, back_after_s: back === 'never' ? undefined : Number(back) }, {
    onSuccess: (d) => { toast.ok(`${g!.name}：${d.from} → ${d.to}${d.back_at ? `，${F.dt(d.back_at)} 自动切回` : ''}`); showSwitch(null); },
    onError: toast.err,
  });
  return (
    <FormDialog open={open} onOpenChange={(o) => !o && showSwitch(null)} title={`临时切换出口 · ${t.name}`} submitLabel="切换" busy={run.isPending} valid={valid} onSubmit={submit}
      desc={<>不重启 OpenClash，已有连接不断，新连接走新出口<InfoTip text="OpenClash 重启后一般保留当前选择；组的选择被别人改掉时，待切回随之取消" /></>}>
      {q.isLoading ? <Skeleton h={96} /> : !groups.length ? <Note tone="warn">这台路由没有可以手动切换的 select 组</Note> : (
        <div className="form-grid">
          <Field label="策略组" className="span-2"><Select value={g?.name ?? ''} onChange={(v) => { setGroup(v); setTo(''); }} options={groups.map((x) => ({ value: x.name, label: x.name }))} /></Field>
          <Field label="切到" hint={g?.now ? `现在是 ${g.now}` : undefined}><Select value={to} onChange={setTo} options={opts} placeholder="选组里的出口" /></Field>
          <Field label="自动切回"><Select value={back} onChange={setBack} options={BACK} /></Field>
        </div>
      )}
    </FormDialog>
  );
});

function SwitchHost() {
  const t = useSwitchTarget();
  const held = useHeld(t?.id, t);
  return held ? <SwitchDialog open={!!t} t={held} /> : null;
}

export function RouterOpsHost() {
  return <><SyncHost /><SwitchHost /></>;
}
