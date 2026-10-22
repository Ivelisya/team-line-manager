// 软路由的对话框：接入（两种方式：填账号密码一键部署 / 在路由上执行命令）、重新部署、接入命令、删除后的清理说明。
// 接入命令、重新部署、删除说明从卡片、抽屉、页面都能弹（lib/routers 的小仓库），由外壳里那一份 RouterDialogsHost 渲染。
import * as React from 'react';
import { CircleCheck, Loader2, RefreshCw, Terminal } from 'lucide-react';
import { useTeams } from '@/lib/queries';
import { closeBootstrap, closeDeploy, closeRemoved, showBootstrap, TUNNEL_CLEANUP, useAddRouter, useBootTarget, useDeployRouter, useDeployTarget, useRemoved, useRouters, type SshLogin } from '@/lib/routers';
import { copyText } from '@/lib/clipboard';
import { resetOnOpen, useHeld } from '@/lib/useHeld';
import { useNow } from '@/lib/useNow';
import { Dialog, DialogContent, DialogDescription, DialogFooter, DialogHeader, DialogTitle } from '@/components/ui/dialog';
import { Button } from '@/components/ui/button';
import { Field, Input, Textarea } from '@/components/ui/form';
import { Select } from '@/components/ui/select';
import { Tabs, TabsList, TabsTrigger } from '@/components/ui/overlays';
import { toast } from '@/components/ui/toast';
import { FormDialog } from '@/components/app/dialogs';
import { CopyButton, Note } from '@/components/app/primitives';
import { useShowBootstrap } from '@/components/app/RouterCard';

type Method = 'ssh' | 'cmd';
const ONLY_ADDS = '两种方式都只新增：给路由加中控的登录公钥、装 fleet-tunnel 服务（和原来的 router-tunnel 并存）；不改 root 密码、dropbear / LuCI 监听、OpenClash 配置与防火墙。';
const SSH_NOTE = '中控能 SSH 到路由时最省事：填路由的 SSH 地址，中控登录上去替你装好。密码只随这一次请求发出，中控不保存、之后也不显示；不填就用中控现有的密钥登录。';
const CMD_NOTE = '路由在 NAT 后面、中控连不上它时用：生成一条命令，在路由的 SSH 里粘贴执行（30 分钟内有效、只能用一次）。';

/** 这个浏览器上次给这台路由填的 SSH 地址（中控不回传地址；密码从不保存）：「重新部署」预填 */
const sshKey = (id: string) => `fleet:router-ssh:${id}`;
const rememberSsh = (id: string, v: Omit<SshLogin, 'password'>) => { try { localStorage.setItem(sshKey(id), JSON.stringify(v)); } catch { /* 隐私模式 */ } };
const recallSsh = (id: string): Omit<SshLogin, 'password'> | null => { try { return JSON.parse(localStorage.getItem(sshKey(id)) ?? 'null'); } catch { return null; } };

interface SshDraft { host: string; port: string; user: string; password: string }
const sshValid = (d: SshDraft) => !!d.host.trim() && /^\d+$/.test(d.port) && +d.port >= 1 && +d.port <= 65535 && !!d.user.trim();
const sshBody = (d: SshDraft): SshLogin => ({ host: d.host.trim(), port: +d.port, user: d.user.trim(), ...(d.password ? { password: d.password } : {}) });
/** SSH 地址 / 端口 / 用户 / 密码。密码框不让浏览器和密码管理器记（路由的 root 密码不该存进浏览器） */
function SshFields({ d, set, autoFocus }: { d: SshDraft; set: (p: Partial<SshDraft>) => void; autoFocus?: boolean }) {
  const portErr = d.port !== '' && !(/^\d+$/.test(d.port) && +d.port >= 1 && +d.port <= 65535) ? '端口应在 1–65535 之间' : undefined;
  return (
    <div className="form-grid">
      <Field label="SSH 地址" hint="中控连得到的地址：公网 IP、域名，或经 VPN 能到的内网地址"><Input value={d.host} onChange={(e) => set({ host: e.target.value })} placeholder="例如 203.0.113.20" autoFocus={autoFocus} autoComplete="off" spellCheck={false} /></Field>
      <Field label="端口" error={portErr}><Input inputMode="numeric" value={d.port} onChange={(e) => set({ port: e.target.value.replace(/\D/g, '') })} aria-invalid={!!portErr} /></Field>
      <Field label="用户"><Input value={d.user} onChange={(e) => set({ user: e.target.value })} autoComplete="off" spellCheck={false} /></Field>
      <Field label="密码" hint="可不填：不填就用中控现有的密钥登录"><Input type="password" value={d.password} onChange={(e) => set({ password: e.target.value })} autoComplete="off" data-1p-ignore data-lpignore="true" /></Field>
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
      onError: toast.err,
    });
  };
  return (
    <FormDialog size="md" open={open} onOpenChange={onOpenChange} title="接入软路由" desc={ONLY_ADDS} submitLabel={method === 'ssh' ? '开始部署' : '生成接入命令'} busy={add.isPending} valid={valid} onSubmit={submit}
      dirty={!!name.trim() || !!note.trim() || !!ssh.host.trim() || !!ssh.password}>
      <div className="form-grid">
        <Field label="名称" hint="例如：团队2-路由A" error={dup ? '已有同名的软路由' : undefined}><Input value={name} onChange={(e) => setName(e.target.value)} autoFocus autoComplete="off" aria-invalid={dup} /></Field>
        <Field label="团队"><Select value={team} onChange={setTeam} options={teamOpts} allowEmpty="不分团队" placeholder="不分团队" /></Field>
        <Field label="备注" className="span-2"><Textarea rows={2} value={note} onChange={(e) => setNote(e.target.value)} placeholder="例如：客户机房 2 楼弱电箱，旁路由" /></Field>
      </div>
      <div className="field">
        <span className="label">接入方式</span>
        <Tabs value={method} onValueChange={(v) => setMethod(v as Method)}>
          <TabsList><TabsTrigger value="ssh" noPanel>填账号密码一键部署</TabsTrigger><TabsTrigger value="cmd" noPanel>在路由上执行命令</TabsTrigger></TabsList>
        </Tabs>
        <p className="rt-method-note">{method === 'ssh' ? SSH_NOTE : CMD_NOTE}</p>
      </div>
      {method === 'ssh' && <SshFields d={ssh} set={setSsh} />}
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
      onError: toast.err, // 已有一次部署在跑时 409，原样给出原因
    });
  };
  return (
    <FormDialog size="md" open={open} onOpenChange={(o) => !o && closeDeploy()} title={`重新部署「${name}」`} desc={SSH_NOTE} submitLabel="开始部署" busy={deploy.isPending} valid={sshValid(ssh)} dirty={!!ssh.password} onSubmit={submit}>
      {r?.state === 'online' && <Note tone="warn">这台现在在线：重新部署会换一把隧道密钥并重启 fleet-tunnel，期间约半分钟连不上（不影响 OpenClash 和客户端上网）。</Note>}
      <SshFields d={ssh} set={setSsh} autoFocus />
      <div className="hint">中控连不上这台路由（在 NAT 后面）？<button type="button" className="btn btn-link btn-sm" style={{ height: 'auto', padding: 0 }} disabled={boot.pending}
        onClick={() => { closeDeploy(); void boot.run({ id, name, last_seen_at: r?.last_seen_at ?? null }); }}><Terminal size={12} />改用在路由上执行命令</button></div>
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
  if (!b) return null;
  const left = Date.parse(b.expires_at) - now;
  const done = r?.state === 'online' && r.last_seen_at != null && Date.parse(r.last_seen_at) > Date.parse(b.issued_at);
  return (
    <Dialog open={!!t} onOpenChange={(o) => !o && closeBootstrap()}>
      <DialogContent size="md" onInteractOutside={(e) => e.preventDefault()}>
        <DialogHeader>
          <DialogTitle>{done ? `「${b.name}」已接入` : `接入「${b.name}」`}</DialogTitle>
          <DialogDescription>在路由的 SSH 里粘贴执行；30 分钟内有效、只能用一次。</DialogDescription>
        </DialogHeader>
        {b.replaced && !done && <Note tone="warn">旧的隧道密钥已作废：路由上原来的 fleet-tunnel 现在连不上中控，执行这条新命令后恢复。</Note>}
        <div className="token-box rt-cmd"><code>{b.cmd}</code><CopyButton text={b.cmd} label="接入命令" size={15} /></div>
        {done ? <Note tone="ok" icon={<CircleCheck size={14} />}>路由已连上中控，状态在线。可以关掉这个窗口了。</Note>
          : left > 0 ? <div className="row hint" style={{ gap: 6 }}><Loader2 size={13} className="spin" />等路由连上：执行后约 30 秒内显示在线（这个窗口开着会自动更新）· 还有 <b className="num">{mmss(left)}</b> 过期</div>
          : <Note tone="bad">命令已过期（30 分钟）。<button type="button" className="btn btn-link btn-sm" style={{ height: 'auto', padding: 0 }} disabled={boot.pending} onClick={() => void boot.run({ id: b.id, name: b.name, last_seen_at: r?.last_seen_at ?? null })}><RefreshCw size={12} />重新生成</button></Note>}
        <div className="hint">{ONLY_ADDS}</div>
        <DialogFooter>
          <Button onClick={() => void copyText(b.cmd, '接入命令')}>复制命令</Button>
          <Button variant="primary" onClick={closeBootstrap}>{done ? '完成' : '关闭'}</Button>
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
        <DialogHeader><DialogTitle>已删除「{held}」</DialogTitle><DialogDescription>中控已撤销它的隧道公钥，路由上的 fleet-tunnel 之后连不上中控，会一直重试。</DialogDescription></DialogHeader>
        <div className="hint">在路由的 SSH 里执行下面这条，停掉并删掉这个服务（原来连 ivekasy 的 router-tunnel 不受影响）：</div>
        <div className="token-box rt-cmd"><code>{TUNNEL_CLEANUP}</code><CopyButton text={TUNNEL_CLEANUP} label="清理命令" size={15} /></div>
        <DialogFooter><Button variant="primary" onClick={closeRemoved}>知道了</Button></DialogFooter>
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
  return <><BootstrapDialog /><DeployHost /><RemovedDialog /></>;
}
