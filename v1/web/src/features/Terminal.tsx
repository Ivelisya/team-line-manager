// 网页终端：经中控 SSH 到受管机器（同机 IP 节点由中控连到它的宿主机）。整个文件连同 xterm 按需加载（Shell 里第一次打开时才下载）。
// 流程：换票据（可能先要两步验证码或登录密码）→ 连 WebSocket，第一条消息送票据和行列 → 二进制帧双向传终端字节，文本帧是 JSON 控制消息
// （客户端发 resize，服务端断开前发 exit 和原因）。上限都取 /meta 的 terminal。窗口是右侧不带遮罩的面板：开着终端也能照常翻页面、看卡片。
import * as React from 'react';
import { Terminal as XTerm, type ITheme } from '@xterm/xterm';
import { FitAddon } from '@xterm/addon-fit';
import '@xterm/xterm/css/xterm.css';
import '@/styles/terminal.css';
import { RotateCw, SquareTerminal, X } from 'lucide-react';
import { ApiError } from '@/lib/api';
import { useMachine } from '@/lib/queries';
import { meta } from '@/lib/derive';
import { useHeld } from '@/lib/useHeld';
import { cancelTerminalAsk, closeTerminal, confirmTerminalAsk, requestTicket, setTerminalLive, terminalUrl, useTerminalAsk, useTerminalTarget, type TermTarget } from '@/lib/terminal';
import { Button, IconButton } from '@/components/ui/button';
import { Field, InfoTip, Input } from '@/components/ui/form';
import { ConfirmDialog } from '@/components/app/dialogs';
import { Pill, type Tone } from '@/components/app/primitives';

type Need = 'totp' | 'password';
type Phase =
  | { s: 'auth'; need?: Need } // 正在换票据；need：是带着验证码 / 密码换的，表单留着显示「正在验证」
  | { s: 'cred'; need: Need; err?: string }
  | { s: 'connecting' }
  | { s: 'open' }
  | { s: 'closed'; reason: string; failed: boolean }; // failed：一次都没连上

/** 终端配色取站点 token（都是十六进制色值，xterm 直接认），深浅色、配色一换就跟着换 */
function readTheme(): ITheme {
  const s = getComputedStyle(document.documentElement);
  const v = (n: string) => s.getPropertyValue(n).trim();
  const dark = document.documentElement.dataset.theme === 'dark';
  const accent = v('--accent');
  return {
    background: v('--sunken'), foreground: v('--fg'), cursor: accent, cursorAccent: v('--sunken'),
    selectionBackground: `${accent}40`, // 强调色 25%：选中的字仍看得清
    black: dark ? '#3f3f46' : '#27272a', brightBlack: v('--fg-3'),
    red: v('--bad'), brightRed: v('--bad-text'), green: v('--ok'), brightGreen: v('--ok-text'),
    yellow: v('--warn'), brightYellow: v('--warn-text'), blue: v('--info'), brightBlue: v('--info-text'),
    magenta: v('--tag-violet-fg'), brightMagenta: v('--tag-pink-fg'), cyan: v('--tag-cyan-fg'), brightCyan: v('--tag-sky-fg'),
    // 亮色底上 ANSI 白要压暗，否则 ls 之类着白色的字看不见
    white: dark ? '#d4d4d8' : v('--fg-2'), brightWhite: dark ? '#fafafa' : v('--fg-2'),
  };
}

/** 服务端没先发 exit 就关掉时，按关闭码说原因（服务端一般会先发 exit） */
const CLOSE_REASON: Record<number, string> = { 1009: '一次发送的内容太大', 1011: '中控内部错误', 4401: '登录已失效，请重新登录' };

const PILL: Record<Exclude<Phase['s'], 'closed'>, [Tone, string]> = { auth: ['info', '正在验证'], cred: ['warn', '正在验证'], connecting: ['info', '正在连接'], open: ['ok', '已连接'] };

/** 一个终端窗口。kind 现在只有 machine，router 以后同样走它 */
export function Terminal({ kind, id, name }: TermTarget) {
  const host = useMachine(kind === 'machine' ? id : null).data?.host_machine;
  const box = React.useRef<HTMLDivElement>(null);
  const term = React.useRef<XTerm | null>(null);
  const fit = React.useRef<FitAddon | null>(null);
  const sock = React.useRef<WebSocket | null>(null);
  const gen = React.useRef(0); // 每次（重新）连接加一：上一轮迟到的票据、关闭事件不再改状态
  const [phase, setPhase] = React.useState<Phase>({ s: 'auth' });
  const titleId = React.useId();

  React.useEffect(() => {
    const el = box.current!;
    const t = new XTerm({ fontFamily: getComputedStyle(document.documentElement).getPropertyValue('--font-mono').trim(), fontSize: 13, lineHeight: 1.2, cursorBlink: true, scrollback: 5000, theme: readTheme() });
    const f = new FitAddon();
    t.loadAddon(f);
    t.open(el);
    f.fit();
    const enc = new TextEncoder();
    const send = (d: Uint8Array | string) => { const ws = sock.current; if (ws?.readyState === WebSocket.OPEN) ws.send(d); };
    // 一条消息有上限（超了服务端以 1009 断开）：大段粘贴切成几条发，终端按字节拼回去
    const max = meta().terminal.max_message;
    t.onData((d) => { const b = enc.encode(d); for (let i = 0; i < b.length; i += max) send(b.subarray(i, i + max)); });
    t.onBinary((d) => send(Uint8Array.from(d, (c) => c.charCodeAt(0)))); // 部分鼠标上报是 latin-1 字节串，不能按 UTF-8 编码
    t.onResize(({ cols, rows }) => send(JSON.stringify({ type: 'resize', cols, rows })));
    // 有选中内容时 Ctrl+C 交给浏览器复制（xterm 在 copy 事件里放进剪贴板），不发 ^C；没选中照常中断
    t.attachCustomKeyEventHandler((e) => !(e.type === 'keydown' && e.ctrlKey && !e.shiftKey && !e.altKey && e.key === 'c' && t.hasSelection()));
    // 终端里的按键不再冒泡到 window：全站快捷键（Ctrl+K 打开命令面板、Esc 收起侧栏）不能抢 shell 的 Ctrl+K、vim 的 Esc
    const stop = (e: KeyboardEvent) => e.stopPropagation();
    el.addEventListener('keydown', stop);
    const ro = new ResizeObserver(() => { if (el.clientWidth > 0 && el.clientHeight > 0) f.fit(); });
    ro.observe(el);
    const mo = new MutationObserver(() => { t.options.theme = readTheme(); });
    mo.observe(document.documentElement, { attributes: true, attributeFilter: ['data-theme', 'data-palette'] });
    term.current = t; fit.current = f;
    return () => { ro.disconnect(); mo.disconnect(); el.removeEventListener('keydown', stop); t.dispose(); term.current = null; fit.current = null; };
  }, []);

  /** 写一行暗色的状态说明进终端（断开原因留在输出里，往回翻也看得到） */
  const note = (s: string) => term.current?.write(`\r\n\x1b[2m[${s}]\x1b[0m\r\n`);

  const connect = React.useCallback(async (cred?: { code?: string; password?: string }) => {
    const my = ++gen.current;
    setPhase({ s: 'auth', need: cred ? (cred.code != null ? 'totp' : 'password') : undefined });
    let ticket: string;
    try {
      ticket = (await requestTicket({ kind, id, ...cred })).ticket;
    } catch (e) {
      if (my !== gen.current) return;
      const code = e instanceof ApiError ? e.code : '';
      const msg = e instanceof Error ? e.message : String(e);
      if (code === 'totp_required' || code === 'password_required') setPhase({ s: 'cred', need: code === 'totp_required' ? 'totp' : 'password' });
      else if (code === 'totp_invalid' || code === 'password_invalid') setPhase({ s: 'cred', need: code === 'totp_invalid' ? 'totp' : 'password', err: msg });
      else { note(`连接失败：${msg}`); setPhase({ s: 'closed', reason: msg, failed: true }); }
      return;
    }
    if (my !== gen.current) return;
    setPhase({ s: 'connecting' });
    const ws = new WebSocket(terminalUrl());
    ws.binaryType = 'arraybuffer';
    sock.current = ws;
    let opened = false;
    let exit: string | null = null;
    ws.onopen = () => {
      opened = true;
      setPhase({ s: 'open' });
      const t = term.current!;
      fit.current!.fit();
      ws.send(JSON.stringify({ type: 'auth', ticket, cols: t.cols, rows: t.rows })); // 10 秒内要送到
      t.focus();
    };
    ws.onmessage = (e) => {
      if (typeof e.data === 'string') { const m = JSON.parse(e.data) as { type: string; reason?: string }; if (m.type === 'exit') exit = m.reason ?? null; }
      else term.current?.write(new Uint8Array(e.data as ArrayBuffer));
    };
    ws.onclose = (e) => {
      if (sock.current !== ws) return; // 自己关的（重连、关窗、卸载）
      sock.current = null;
      const reason = exit ?? (e.reason || CLOSE_REASON[e.code] || (opened ? '连接中断' : '没能连上中控（网络问题，或票据已过期）'));
      note(opened ? `已断开：${reason}` : `连接失败：${reason}`);
      setPhase({ s: 'closed', reason, failed: !opened });
    };
  }, [kind, id]);

  React.useEffect(() => {
    void connect();
    return () => { gen.current++; const ws = sock.current; sock.current = null; ws?.close(); };
  }, [connect]);

  // 连着时：关窗、换台要先确认（lib/terminal），关页面 / 刷新浏览器也先问
  const live = phase.s === 'open';
  React.useEffect(() => {
    setTerminalLive(live);
    if (!live) return;
    const f = (e: BeforeUnloadEvent) => e.preventDefault();
    window.addEventListener('beforeunload', f);
    return () => window.removeEventListener('beforeunload', f);
  }, [live]);

  const [tone, label] = phase.s === 'closed' ? [phase.failed ? 'bad' : 'neutral', `${phase.failed ? '连接失败' : '已断开'}：${phase.reason}`] as const : PILL[phase.s];
  const need = phase.s === 'cred' || phase.s === 'auth' ? phase.need : undefined;
  return (
    <section className="term" role="dialog" aria-modal="false" aria-labelledby={titleId}>
      <header className="term-head">
        <SquareTerminal size={16} className="term-ic" aria-hidden />
        <h2 id={titleId} className="term-title ellipsis">终端 · {name}</h2>
        <Pill tone={tone} dot={phase.s === 'auth' || phase.s === 'connecting' ? 'pulse' : undefined}
          tip={phase.s === 'closed' ? label : undefined} role="status"><span className="ellipsis">{label}</span></Pill>
        <div className="term-tools">
          {phase.s === 'closed' && <Button size="sm" onClick={() => void connect()}><RotateCw />重新连接</Button>}
          <IconButton label="关闭" onClick={closeTerminal}><X size={16} /></IconButton>
        </div>
      </header>
      {host && <div className="term-sub hint">连的是宿主机「{host.name}」<InfoTip text="同机 IP 节点没有自己的 SSH，连接的是它的宿主机" /></div>}
      <div className="term-screen">
        <div ref={box} className="term-xterm" />
        {need && <CredForm key={need} need={need} err={phase.s === 'cred' ? phase.err : undefined} busy={phase.s === 'auth'} onSubmit={(c) => void connect(c)} />}
      </div>
      <footer className="term-foot hint">输入的命令不进审计<InfoTip text="终端里输入的命令不进审计，只记谁、哪台、多久、多少字节" /></footer>
    </section>
  );
}

const limitInfo = () => `验证后 ${Math.round(meta().terminal.verify_ttl_s / 60)} 分钟内再开终端不用重输。输错会计入登录的失败次数，错太多次会被暂时限制登录。`;

/** 开终端前再验证一次身份：开了两步验证要验证码（或恢复码），没开的要当前登录密码。输入只用于这一次请求，提交即清空 */
function CredForm({ need, err, busy, onSubmit }: { need: Need; err?: string; busy: boolean; onSubmit: (c: { code: string } | { password: string }) => void }) {
  const [v, setV] = React.useState('');
  const totp = need === 'totp';
  return (
    <div className="term-cover">
      <form className="term-card" onSubmit={(e) => { e.preventDefault(); if (!v.trim() || busy) return; onSubmit(totp ? { code: v.trim() } : { password: v }); setV(''); }}>
        <div className="term-card-title">{totp ? '输入两步验证码' : '输入当前登录密码'}</div>
        <Field label={totp ? '验证码或恢复码' : '登录密码'} error={err} hint="输错计入登录失败次数"
          info={`${totp ? '打开终端前要再验证一次：验证器里的 6 位验证码，或一个恢复码（用过即作废）。' : '打开终端前要再输入一次当前登录密码，确认是本人在操作。'}\n${limitInfo()}`}>
          {/* 恢复码带字母，不能限成纯数字键盘 */}
          {totp
            ? <Input value={v} onChange={(e) => setV(e.target.value)} autoFocus autoComplete="one-time-code" autoCapitalize="off" spellCheck={false} placeholder="6 位验证码，或一个恢复码" />
            : <Input type="password" value={v} onChange={(e) => setV(e.target.value)} autoFocus autoComplete="current-password" />}
        </Field>
        <div className="dialog-foot">
          <Button onClick={closeTerminal}>取消</Button>
          <Button type="submit" variant="primary" loading={busy} disabled={!v.trim()}>验证并连接</Button>
        </div>
      </form>
    </div>
  );
}

/** 外壳里唯一一份：当前的终端窗口，和「连着时关窗 / 换台」的确认框 */
export function TerminalHost() {
  const t = useTerminalTarget();
  const ask = useTerminalAsk();
  // 确认框关闭动画期间文字保持不变
  const a = useHeld(ask ? 'ask' : null, ask);
  const cur = useHeld(t ? `${t.kind}:${t.id}` : null, t);
  return (
    <>
      {t && <Terminal key={`${t.kind}:${t.id}`} {...t} />}
      <ConfirmDialog open={!!ask} onOpenChange={(o) => { if (!o) cancelTerminalAsk(); }} danger
        title={a === 'close' || !a ? `断开 ${cur?.name ?? ''} 的终端？` : `断开 ${cur?.name ?? ''}，打开 ${a.name} 的终端？`}
        description={<>还在跑的命令会随之结束<InfoTip text="会话会立即断开；放在 tmux、screen 或 nohup 里的命令除外" /></>}
        confirmLabel={a === 'close' || !a ? '断开并关闭' : '断开并切换'} onConfirm={() => { if (ask) confirmTerminalAsk(ask); }} />
    </>
  );
}
