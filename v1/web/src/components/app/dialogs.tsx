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
  confirmLabel: string; danger?: boolean; /** 额外的禁用条件（例如预览还没加载完） */ confirmDisabled?: boolean; onConfirm: () => Promise<unknown> | unknown; children?: React.ReactNode; size?: 'sm' | 'md';
  /** 有未提交的改动：点遮罩不关闭（Esc 与取消仍可关） */ dirty?: boolean;
}

/** asForm：内容包在 form 里，可确认时在输入框里按 Enter 即执行 */
export function ConfirmDialog({ open, onOpenChange, title, description, confirmLabel, danger, confirmDisabled, onConfirm, children, size, dirty, asForm }: Base & { asForm?: boolean }) {
  const [busy, setBusy] = React.useState(false);
  const run = async () => {
    setBusy(true);
    try { await onConfirm(); onOpenChange(false); } catch (e) { toast.err(e); } finally { setBusy(false); }
  };
  const body = (
    <>
      <DialogHeader><DialogTitle>{title}</DialogTitle>{description && <DialogDescription>{description}</DialogDescription>}</DialogHeader>
      {children}
      <DialogFooter>
        <Button onClick={() => onOpenChange(false)} disabled={busy}>取消</Button>
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

/** 需要输入 expected 才能确认。expected 区分大小写，两端空格忽略。
 *  每次打开重新挂载（resetOnOpen）：输入与上次的失败原因清空，关闭动画期间内容保持原样 */
export const TypedConfirmDialog = resetOnOpen(function TypedConfirmDialog({ expected, prompt, ...base }: Base & { expected: string; prompt?: string }) {
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
      <DialogContent size={base.size} onOpenAutoFocus={(e) => { e.preventDefault(); input.current?.focus(); }}>
        <DialogHeader><DialogTitle>{base.title}</DialogTitle>{base.description && <DialogDescription>{base.description}</DialogDescription>}</DialogHeader>
        {base.children}
        <div className="field">
          <label className="label" htmlFor="typed-confirm">{prompt ?? '请输入'} <b className="mono" style={{ color: 'var(--fg)' }}>{expected}</b> 以确认</label>
          <Input ref={input} id="typed-confirm" value={text} onChange={(e) => setText(e.target.value)} autoComplete="off" spellCheck={false} placeholder={expected}
            onKeyDown={(e) => { if (e.key === 'Enter' && ok && !busy) void run(); }} />
        </div>
        {err && <Note tone="bad">{err}</Note>}
        <DialogFooter>
          <Button onClick={() => base.onOpenChange(false)} disabled={busy}>取消</Button>
          <Button variant={base.danger ? 'danger' : 'primary'} disabled={!ok} loading={busy} onClick={run}>{base.confirmLabel}</Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
});
