// 通用对话框：ConfirmDialog（一次确认）、TypedConfirmDialog（输入指定文字才可确认，用于退役、彻底删除、轮换凭据）、
// FormDialog（表单：标题 + 字段 + 取消 / 提交，Enter 提交）。
// TypedConfirmDialog 的失败原因（如「还不能彻底删除」）留在对话框里显示，不只是一闪而过的通知。
import * as React from 'react';
import { Dialog, DialogContent, DialogDescription, DialogFooter, DialogTitle, DialogHeader } from '@/components/ui/dialog';
import { Button } from '@/components/ui/button';
import { Input } from '@/components/ui/form';
import { toast } from '@/components/ui/toast';
import { resetOnOpen } from '@/lib/useHeld';
import { Note } from './primitives';

interface Base {
  open: boolean; onOpenChange: (o: boolean) => void; title: string; description?: React.ReactNode;
  confirmLabel: string; /** 取消按钮的字（确认的动作本身叫「取消…」时换成「返回」之类，不撞名） */ cancelLabel?: string; danger?: boolean; /** 额外的禁用条件（例如预览还没加载完） */ confirmDisabled?: boolean; onConfirm: () => Promise<unknown> | unknown; children?: React.ReactNode; size?: 'sm' | 'md';
  /** 有未提交的改动：点遮罩不关闭（Esc 与取消仍可关） */ dirty?: boolean;
}

/** 内容长的对话框（scroll）：只滚中间，标题与按钮留在原位。左右各让出对话框的内边距，聚焦环不被裁掉 */
const SCROLL_BODY: React.CSSProperties = {
  display: 'grid', gap: 16, overflowY: 'auto', maxHeight: 'calc(100dvh - min(10vh, 96px) - 220px)',
  margin: '0 calc(-1 * var(--sp-20))', padding: '2px var(--sp-20)',
};

/** asForm：内容包在 form 里，可确认时在输入框里按 Enter 即执行 */
export function ConfirmDialog({ open, onOpenChange, title, description, confirmLabel, danger, confirmDisabled, onConfirm, children, size, dirty, asForm, scroll, cancelLabel }: Base & { asForm?: boolean; scroll?: boolean }) {
  const [busy, setBusy] = React.useState(false);
  const run = async () => {
    setBusy(true);
    try { await onConfirm(); onOpenChange(false); } catch (e) { toast.err(e); } finally { setBusy(false); }
  };
  const body = (
    <>
      <DialogHeader><DialogTitle>{title}</DialogTitle>{description && <DialogDescription>{description}</DialogDescription>}</DialogHeader>
      {scroll ? <div style={SCROLL_BODY}>{children}</div> : children}
      <DialogFooter>
        <Button onClick={() => onOpenChange(false)} disabled={busy}>{cancelLabel ?? '取消'}</Button>
        <Button type={asForm ? 'submit' : 'button'} variant={danger ? 'danger' : 'primary'} loading={busy} disabled={confirmDisabled} onClick={asForm ? undefined : run}>{confirmLabel}</Button>
      </DialogFooter>
    </>
  );
  return (
    <Dialog open={open} onOpenChange={(o) => !busy && onOpenChange(o)}>
      <DialogContent size={size} onInteractOutside={(e) => { if (dirty) e.preventDefault(); }}>
        {asForm
          ? <form style={{ display: 'grid', gap: 16 }} onSubmit={(e) => { e.preventDefault(); e.stopPropagation(); if (!confirmDisabled && !busy) void run(); }}>{body}</form>
          : body}
      </DialogContent>
    </Dialog>
  );
}

/** 表单对话框：可提交（valid）时在输入框里按 Enter 即提交。提交中（busy）不能关；dirty（有未保存的改动）时点遮罩不关，Esc 与取消仍可关 */
export function FormDialog({ open, onOpenChange, title, desc, children, onSubmit, submitLabel, busy, valid = true, size, dirty }: {
  open: boolean; onOpenChange: (o: boolean) => void; title: string; desc?: React.ReactNode; children: React.ReactNode; onSubmit: () => void; submitLabel: string; busy?: boolean; valid?: boolean; size?: 'sm' | 'md'; dirty?: boolean;
}) {
  return (
    <Dialog open={open} onOpenChange={(o) => !busy && onOpenChange(o)}>
      <DialogContent size={size} onInteractOutside={(e) => { if (dirty) e.preventDefault(); }}>
        <form style={{ display: 'grid', gap: 16 }} onSubmit={(e) => { e.preventDefault(); if (valid && !busy) onSubmit(); }}>
          <DialogHeader><DialogTitle>{title}</DialogTitle>{desc && <DialogDescription>{desc}</DialogDescription>}</DialogHeader>
          {children}
          <DialogFooter><Button type="button" onClick={() => onOpenChange(false)} disabled={busy}>取消</Button><Button type="submit" variant="primary" loading={busy} disabled={!valid}>{submitLabel}</Button></DialogFooter>
        </form>
      </DialogContent>
    </Dialog>
  );
}

/** 需要输入 expected 才能确认。expected 区分大小写，两端空格忽略。blocked：现在做不了（内容里已经说明原因）：不要输入框和确认按钮，取消换成「关闭」。
 *  每次打开重新挂载（resetOnOpen）：输入与上次的失败原因清空，关闭动画期间内容保持原样 */
export const TypedConfirmDialog = resetOnOpen(function TypedConfirmDialog({ expected, prompt, blocked, ...base }: Base & { expected: string; prompt?: string; blocked?: boolean }) {
  const [text, setText] = React.useState('');
  const [busy, setBusy] = React.useState(false);
  const [err, setErr] = React.useState<string | null>(null);
  const input = React.useRef<HTMLInputElement>(null);
  const ok = text.trim() === expected && !base.confirmDisabled;
  const run = async () => {
    setBusy(true); setErr(null);
    try { await base.onConfirm(); base.onOpenChange(false); } catch (e) { setErr(e instanceof Error ? e.message : String(e)); } finally { setBusy(false); }
  };
  return (
    <Dialog open={base.open} onOpenChange={(o) => !busy && base.onOpenChange(o)}>
      {/* 焦点直接落在确认输入框（默认会落到前面的复选框等控件上） */}
      <DialogContent size={base.size} onOpenAutoFocus={(e) => { if (input.current) { e.preventDefault(); input.current.focus(); } }}>
        <DialogHeader><DialogTitle>{base.title}</DialogTitle>{base.description && <DialogDescription>{base.description}</DialogDescription>}</DialogHeader>
        {base.children}
        {!blocked && (
          <div className="field">
            <label className="label" htmlFor="typed-confirm">{prompt ?? '请输入'} <b className="mono" style={{ color: 'var(--fg)' }}>{expected}</b> 以确认</label>
            <Input ref={input} id="typed-confirm" value={text} onChange={(e) => setText(e.target.value)} autoComplete="off" spellCheck={false} placeholder={expected}
              onKeyDown={(e) => { if (e.key === 'Enter' && ok && !busy) void run(); }} />
          </div>
        )}
        {err && <Note tone="bad">{err}</Note>}
        <DialogFooter>
          <Button onClick={() => base.onOpenChange(false)} disabled={busy}>{base.cancelLabel ?? (blocked ? '关闭' : '取消')}</Button>
          {!blocked && <Button variant={base.danger ? 'danger' : 'primary'} disabled={!ok} loading={busy} onClick={run}>{base.confirmLabel}</Button>}
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
});
