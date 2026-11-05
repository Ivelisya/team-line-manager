// 软路由的对话框：接入（两种方式：填账号密码一键部署 / 在路由上执行命令）、重新部署、接入命令、删除后的清理说明。
// 接入命令、重新部署、删除说明从卡片、抽屉、页面都能弹（lib/routers 的小仓库），由外壳里那一份 RouterDialogsHost 渲染。
import { Field } from '@/components/ui/field';
import { Spinner } from '@/components/ui/spinner';
import * as React from 'react';
import { CircleCheck, RefreshCw, Terminal } from 'lucide-react';
import { useTeams } from '@/lib/queries';
import { closeBootstrap, closeDeploy, closeRemoved, showBootstrap, TUNNEL_CLEANUP, useAddRouter, useBootTarget, useDeployRouter, useDeployTarget, useRemoved, useRouters, type SshLogin } from '@/lib/routers';
import { resetOnOpen, useHeld } from '@/lib/useHeld';
import { useNow } from '@/lib/useNow';
import { Dialog, DialogContent, DialogDescription, DialogFooter, DialogHeader, DialogTitle } from '@/components/ui/dialog';
import { Button } from '@/components/ui/button';
import { FormField, LabelRow } from '@/components/ui/form';
import { InfoTip } from '@/components/ui/info-tip';
import { Input } from '@/components/ui/input';
import { Textarea } from '@/components/ui/textarea';
import { Select } from '@/components/ui/select';
import { Segmented } from '@/components/ui/overlays';
import { toast } from '@/components/ui/toast';
import { FormDialog } from '@/components/app/dialogs';
import { CopyButton, CopyIcon, Note, useCopied } from '@/components/app/primitives';
import { useShowBootstrap } from '@/components/app/RouterCard';
import { prefs } from '@/lib/prefs';
import { RouterOpsHost } from './RouterOps';
import '@/styles/settings.css';

type Method = 'ssh' | 'cmd';
const METHODS: { value: Method; label: string }[] = [{ value: 'ssh', label: '填账号密码一键部署' }, { value: 'cmd', label: '在路由上执行命令' }];
// 界面上只留短字，解释放进「(i)」（用户：说明性的小字太多）
const ONLY_ADDS = '只新增，不修改：添加中控的登录公钥，安装 fleet-tunnel 服务（与原来的 router-tunnel 并存）\n不动 root 密码、SSH 与 LuCI 的监听、OpenClash 配置和防火墙';
const METHOD_INFO = `一键部署：中控通过 SSH 登录路由并装好，要求中控能连到路由\n执行命令：中控连不到路由时用。生成一条命令，到路由的 SSH 里执行（30 分钟内有效，只能用一次）\n两种方式都${ONLY_ADDS}`;

/** 这个浏览器上次给这台路由填的 SSH 地址（中控不回传地址；密码从不保存）：「重新部署」预填 */
const sshKey = (id: string) => `fleet:router-ssh:${id}`;
const rememberSsh = (id: string, v: Omit<SshLogin, 'password'>) => prefs.set(sshKey(id), JSON.stringify(v));
const recallSsh = (id: string): Omit<SshLogin, 'password'> | null => { try { return JSON.parse(prefs.get(sshKey(id)) ?? 'null'); } catch { return null; } };

interface SshDraft { host: string; port: string; user: string; password: string }
const sshValid = (d: SshDraft) => !!d.host.trim() && /^\d+$/.test(d.port) && +d.port >= 1 && +d.port <= 65535 && !!d.user.trim();
const sshBody = (d: SshDraft): SshLogin => ({ host: d.host.trim(), port: +d.port, user: d.user.trim(), ...(d.password ? { password: d.password } : {}) });
/** SSH 地址 / 端口 / 用户名 / 密码。密码框不让浏览器和密码管理器记（路由的 root 密码不该存进浏览器） */
function SshFields({ d, set, autoFocus }: { d: SshDraft; set: (p: Partial<SshDraft>) => void; autoFocus?: boolean }) {
  const portErr = d.port !== '' && !(/^\d+$/.test(d.port) && +d.port >= 1 && +d.port <= 65535) ? '端口应在 1–65535 之间' : undefined;
  return (
    <div className="form-grid">
      <FormField label="SSH 地址" info="中控能连到的地址：公网 IP、域名，或 VPN 内网地址"><Input value={d.host} onChange={(e) => set({ host: e.target.value })} placeholder="中控能连到的 IP 或域名" autoFocus={autoFocus} autoComplete="off" data-1p-ignore data-lpignore="true" spellCheck={false} /></FormField>
      <FormField label="端口" error={portErr}><Input inputMode="numeric" value={d.port} onChange={(e) => set({ port: e.target.value.replace(/\D/g, '') })} aria-invalid={!!portErr} /></FormField>
      <FormField label="用户名"><Input value={d.user} onChange={(e) => set({ user: e.target.value })} autoComplete="off" data-1p-ignore data-lpignore="true" spellCheck={false} /></FormField>
      <FormField label="密码" info="只用于这一次，中控不会保存。留空则用中控已有的密钥登录"><Input type="password" value={d.password} onChange={(e) => set({ password: e.target.value })} placeholder="留空用中控密钥" autoComplete="off" data-1p-ignore data-lpignore="true" /></FormField>
    </div>
  );
}
const useSshDraft = (init?: Omit<SshLogin, 'password'> | null) => {
  const [d, setD] = React.useState<SshDraft>({ host: init?.host ?? '', port: String(init?.port ?? 22), user: init?.user ?? 'root', password: '' });
  return [d, React.useCallback((p: Partial<SshDraft>) => setD((x) => ({ ...x, ...p })), [])] as const;
};

/** 接入软路由：名称、团队、备注 + 两种方式（页签）。一键部署提交后进度在卡片上；执行命令的弹出接入命令。每次打开重新挂载，填过的不留 */
export const AddRouterDialog = resetOnOpen(function AddRouterDialog({ open, onOpenChange }: { open: boolean; onOpenChange: (o: boolean) => void }) {
  const add = useAddRouter();
  const teams = useTeams().data;
  const names = useRouters().data;
  const teamOpts = React.useMemo(() => (teams ?? []).map((t) => ({ value: t.id, label: t.name })), [teams]);
  const [name, setName] = React.useState(''); const [team, setTeam] = React.useState(''); const [note, setNote] = React.useState('');
  const [method, setMethod] = React.useState<Method>('ssh');
  const [ssh, setSsh] = useSshDraft();
  const dup = names?.some((r) => r.name === name.trim());
  const valid = !!name.trim() && !dup && (method === 'cmd' || sshValid(ssh));
  const submit = () => {
    const base = { name: name.trim(), ...(team ? { team_id: team } : {}), ...(note.trim() ? { note: note.trim() } : {}) };
    const body = method === 'ssh' ? { ...base, ssh: sshBody(ssh) } : base;
    setSsh({ password: '' }); // 密码只发这一次，不留在页面状态里
    add.mutate(body, {
      onSuccess: (r) => {
        onOpenChange(false);
        if (r.bootstrap) showBootstrap({ id: r.id, name: base.name, ...r.bootstrap, issued_at: new Date().toISOString(), replaced: false });
        else { rememberSsh(r.id, { host: ssh.host.trim(), port: +ssh.port, user: ssh.user.trim() }); toast.ok(`已开始部署「${base.name}」，进度在卡片上`); }
      },
    });
  };
  return (
    <FormDialog size="md" open={open} onOpenChange={onOpenChange} title="接入软路由" submitLabel={method === 'ssh' ? '开始部署' : '生成接入命令'} busy={add.isPending} valid={valid} error={add.error} onSubmit={submit}
      dirty={!!name.trim() || !!note.trim() || !!ssh.host.trim() || !!ssh.password}>
      <div className="form-grid">
        <FormField label="名称" error={dup ? '已有同名的软路由' : undefined}><Input value={name} onChange={(e) => setName(e.target.value)} placeholder="例如 团队2-路由A" autoFocus autoComplete="off" data-1p-ignore data-lpignore="true" aria-invalid={dup} /></FormField>
        <FormField label="团队"><Select value={team} onChange={setTeam} options={teamOpts} allowEmpty="未分团队" placeholder="未分团队" /></FormField>
        <FormField label="备注" className="span-2"><Textarea rows={2} value={note} onChange={(e) => setNote(e.target.value)} placeholder="例如：客户机房 2 楼弱电箱，旁路由" /></FormField>
      </div>
      <Field>
        <LabelRow info={METHOD_INFO}>接入方式</LabelRow>
        <Segmented label="接入方式" value={method} onChange={setMethod} items={METHODS} className="seg-fill" />
      </Field>
      {/* 换方式时账号密码这块平滑收起 / 展开（高度过渡），对话框不跳；收起时不可聚焦 */}
      <div className="rt-collapse" data-open={method === 'ssh' ? '' : undefined} inert={method !== 'ssh'}><div><SshFields d={ssh} set={setSsh} /></div></div>
    </FormDialog>
  );
});

/** 重新部署（一键部署失败后重试、换了地址或密码）：预填这个浏览器上次填的地址，密码留空。中控连不上时可改用执行命令 */
const DeployDialog = resetOnOpen(function DeployDialog({ open, id, name }: { open: boolean; id: string; name: string }) {
  const deploy = useDeployRouter();
  const boot = useShowBootstrap();
  const r = useRouters().data?.find((x) => x.id === id);
  const [ssh, setSsh] = useSshDraft(open ? recallSsh(id) : null);
  const submit = () => {
    const body = sshBody(ssh);
    setSsh({ password: '' });
    deploy.mutate({ id, ...body }, {
      onSuccess: () => { rememberSsh(id, { host: body.host, port: body.port, user: body.user }); closeDeploy(); toast.ok(`已开始重新部署「${name}」`); },
    }); // 已有一次部署在跑时 409：原因由对话框里的 error 显示
  };
  return (
    <FormDialog size="md" open={open} onOpenChange={(o) => !o && closeDeploy()} title={`重新部署 · ${name}`} submitLabel="开始部署" busy={deploy.isPending} valid={sshValid(ssh)} error={deploy.error} dirty={!!ssh.password} onSubmit={submit}>
      {r?.state === 'online' && <Note tone="warn">部署时短暂离线<InfoTip text="会更换隧道密钥并重启隧道服务，不影响客户端上网" /></Note>}
      <SshFields d={ssh} set={setSsh} autoFocus />
      <div className="hint">中控连不上？<Button variant="link" size="inline-sm" disabled={boot.pending} onClick={() => { closeDeploy(); void boot.run({ id, name, last_seen_at: r?.last_seen_at ?? null }); }}><Terminal size={12} />改用在路由上执行命令</Button></div>
    </FormDialog>
  );
});

const mmss = (ms: number) => { const s = Math.max(0, Math.ceil(ms / 1000)); return `${Math.floor(s / 60)}:${String(s % 60).padStart(2, '0')}`; };
/** 接入命令：等宽框 + 复制，倒计时；开着时跟着列表看这台有没有在拿到命令之后连上（routers 事件推来），连上了就说已接入。
 *  点遮罩不关（命令只显示这一次，误关只能重新生成） */
function BootstrapDialog() {
  const t = useBootTarget();
  const b = useHeld(t?.id, t);
  const r = useRouters().data?.find((x) => x.id === b?.id);
  const boot = useShowBootstrap();
  const now = useNow(!!t, 1000);
  const cp = useCopied();
  if (!b) return null;
  const left = Date.parse(b.expires_at) - now;
  const done = r?.state === 'online' && r.last_seen_at != null && Date.parse(r.last_seen_at) > Date.parse(b.issued_at);
  return (
    <Dialog open={!!t} onOpenChange={(o) => !o && closeBootstrap()} blockDismiss={(r) => r === 'outside-press'}>
      <DialogContent size="md">
        <DialogHeader>
          <DialogTitle>{done ? `已接入 · ${b.name}` : `接入软路由 · ${b.name}`}</DialogTitle>
          <DialogDescription>在路由的 SSH 里粘贴执行<InfoTip text={`30 分钟内有效、只能用一次。\n${ONLY_ADDS}`} /></DialogDescription>
        </DialogHeader>
        {b.replaced && !done && <Note tone="warn">旧隧道已断开</Note>}
        {/* 复制只留底部一个按钮 */}
        <div className="token-box rt-cmd"><code>{b.cmd}</code></div>
        {done ? <Note tone="ok" icon={<CircleCheck size={14} />}>路由已在线，可以关闭了</Note>
          : left > 0 ? <div className="row hint" style={{ gap: 6 }}><Spinner className="size-[13px]" />等路由连上 · <b className="num">{mmss(left)}</b> 后失效<InfoTip text="执行后约半分钟上线，这个窗口会自动刷新" /></div>
          : <Note tone="bad">命令已失效。<Button variant="link" size="inline-sm" disabled={boot.pending} onClick={() => void boot.run({ id: b.id, name: b.name, last_seen_at: r?.last_seen_at ?? null })}><RefreshCw size={12} />重新生成</Button></Note>}
        <DialogFooter>
          <Button variant="outline" onClick={() => void cp.copy(b.cmd, '接入命令')}><CopyIcon done={cp.done} />复制命令</Button>
          <Button onClick={closeBootstrap}>{done ? '完成' : '关闭'}</Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}

/** 删除之后：中控已撤销隧道公钥，路由上的 fleet-tunnel 会一直重连失败——给出在路由上停掉并删掉它的命令 */
function RemovedDialog() {
  const name = useRemoved();
  const held = useHeld(name, name);
  return (
    <Dialog open={!!name} onOpenChange={(o) => !o && closeRemoved()}>
      <DialogContent size="md">
        <DialogHeader><DialogTitle>已删除 · {held}</DialogTitle><DialogDescription>在路由上执行以清理<InfoTip text={"中控已撤销这台路由的授权，路由上的 fleet-tunnel 会一直重试连接。执行这条命令可以停掉并删除它\n原来的 router-tunnel 不受影响"} /></DialogDescription></DialogHeader>
        <div className="token-box rt-cmd"><code>{TUNNEL_CLEANUP}</code><CopyButton text={TUNNEL_CLEANUP} label="清理命令" size={15} /></div>
        <DialogFooter><Button onClick={closeRemoved}>知道了</Button></DialogFooter>
      </DialogContent>
    </Dialog>
  );
}

function DeployHost() {
  const t = useDeployTarget();
  const held = useHeld(t?.id, t);
  return held ? <DeployDialog open={!!t} id={held.id} name={held.name} /> : null;
}

export function RouterDialogsHost() {
  return <><BootstrapDialog /><DeployHost /><RemovedDialog /><RouterOpsHost /></>;
}
