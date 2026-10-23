// 网页终端的 mock：换票据第一次要登录密码（「wrong」算错），之后 5 分钟内直接给；
// /api/v1/terminal/ws 换成一个假 shell（第一条消息送票据）：回显输入、回车后回一行说明。输入 idle 演示服务端因空闲断开，exit 演示正常退出。
import type { MachineView } from '@/lib/types';

type Ctx = { params: string[]; query: URLSearchParams; body: any };
type Reg = (method: string, path: string, h: (c: Ctx) => unknown) => void;
interface Env { machines: () => MachineView[]; routers: () => { id: string; name: string }[]; bad: (msg: string, status?: number, code?: string) => Error }

const GRACE = 5 * 60_000;
let verifiedAt = 0;
const tickets = new Map<string, { name: string; exp: number }>();
const open = new Set<MockTerm>();
const enc = new TextEncoder();

class MockTerm {
  readyState = 0; binaryType = 'arraybuffer';
  onopen: (() => void) | null = null; onmessage: ((e: MessageEvent) => void) | null = null; onclose: ((e: CloseEvent) => void) | null = null; onerror: (() => void) | null = null;
  private line = '';
  private prompt = '';
  private authed = false;
  constructor() {
    // 同服务端：连上就算打开，10 秒内要收到 {type: 'auth', ticket}
    setTimeout(() => { if (this.readyState === 3) return; this.readyState = 1; this.onopen?.(); }, 300);
    setTimeout(() => { if (!this.authed) this.exit('10 秒内没有收到票据'); }, 10_000);
  }
  private auth(ticket: string) {
    const t = tickets.get(ticket);
    if (!t || t.exp < Date.now()) { this.exit('票据无效或已过期'); return; }
    tickets.delete(ticket); this.authed = true; open.add(this);
    this.prompt = `\x1b[32mroot@${t.name}\x1b[0m:\x1b[34m~\x1b[0m# `;
    this.out(`这是 mock 终端，命令不会真的执行。\r\n输入 idle 演示空闲断开，exit 退出。\r\n\r\n${this.prompt}`);
  }
  private out(s: string) { this.onmessage?.(new MessageEvent('message', { data: enc.encode(s).buffer })); }
  private end(code: number, reason = '') { if (this.readyState === 3) return; this.readyState = 3; open.delete(this); this.onclose?.(new CloseEvent('close', { code, reason })); }
  private exit(reason: string) { this.onmessage?.(new MessageEvent('message', { data: JSON.stringify({ type: 'exit', reason }) })); this.end(1000); }
  send(d: string | ArrayBufferLike | ArrayBufferView) {
    if (typeof d === 'string') { const m = JSON.parse(d) as { type: string; ticket?: string }; if (m.type === 'auth' && !this.authed) this.auth(m.ticket ?? ''); return; } // resize：假 shell 不管行列
    if (!this.authed) return;
    for (const c of new TextDecoder().decode(d as ArrayBuffer)) {
      if (c === '\r') {
        const cmd = this.line.trim(); this.line = '';
        if (cmd === 'exit') { this.out('\r\nlogout\r\n'); this.exit('对端已退出（exit）'); return; }
        if (cmd === 'idle') { this.exit('空闲超过 15 分钟'); return; }
        this.out(`\r\n${cmd ? `mock：不会执行「${cmd}」\r\n` : ''}${this.prompt}`);
      } else if (c === '\x7f') { if (this.line) { this.line = this.line.slice(0, -1); this.out('\b \b'); } }
      else if (c === '\x03') { this.line = ''; this.out(`^C\r\n${this.prompt}`); }
      else if (c >= ' ') { this.line += c; this.out(c); }
    }
  }
  close() { this.readyState = 3; open.delete(this); }
}

export function installTerminalMock(R: Reg, env: Env) {
  R('POST', '/terminal/tickets', ({ body }) => {
    const router = body.kind === 'router' ? env.routers().find((x) => x.id === body.id) : undefined;
    const m = env.machines().find((x) => x.id === body.id);
    if (!router && (body.kind !== 'machine' || !m)) throw env.bad(body.kind === 'router' ? '软路由不存在' : '机器不存在', 404, 'not_found');
    if (m && (m.kind === 'proxy' || m.lifecycle !== 'active')) throw env.bad('这台机器不能打开终端', 409, 'conflict');
    if (Date.now() - verifiedAt > GRACE) {
      if (!body.password) throw env.bad('打开终端前请再输入一次登录密码', 401, 'password_required');
      if (body.password === 'wrong') throw env.bad('密码不对', 401, 'password_invalid');
      verifiedAt = Date.now();
    }
    if (open.size >= 3) throw env.bad('同时最多开 3 个终端，先关掉一个', 429, 'terminal_limit');
    const ticket = Math.random().toString(36).slice(2);
    tickets.set(ticket, { name: router?.name ?? m!.host_machine?.name ?? m!.name, exp: Date.now() + 60_000 });
    return { ticket, expires_at: new Date(Date.now() + 60_000).toISOString() };
  });
  // 只顶替终端的 WebSocket；installMock 之后再包实时流那一层，其余照常走真的
  window.WebSocket = new Proxy(window.WebSocket, {
    construct: (Real, args: ConstructorParameters<typeof WebSocket>) => (String(args[0]).includes('/api/v1/terminal/ws') ? new MockTerm() : new Real(...args)),
  });
}
