// 确认类对话框：ConfirmDialog（一次确认）与 TypedConfirmDialog（输入指定文字才可确认，用于下线、彻底删除、轮换凭据）。
import * as React from 'react';
import { Dialog, DialogContent, DialogDescription, DialogFooter, DialogTitle } from '@/components/ui/dialog';
import { Button } from '@/components/ui/button';
import { Input } from '@/components/ui/form';
import { toast } from '@/components/ui/toast';

interface Base {
  open: boolean; onOpenChange: (o: boolean) => void; title: string; description?: React.ReactNode;
  confirmLabel: string; danger?: boolean; /** 额外的禁用条件（例如预览还没加载完） */ confirmDisabled?: boolean; onConfirm: () => Promise<unknown> | unknown; children?: React.ReactNode; size?: 'sm' | 'md';
}

export function ConfirmDialog({ open, onOpenChange, title, description, confirmLabel, danger, confirmDisabled, onConfirm, children, size }: Base) {
  const [busy, setBusy] = React.useState(false);
  const run = async () => {
    setBusy(true);
    try { await onConfirm(); onOpenChange(false); } catch (e) { toast.err(e); } finally { setBusy(false); }
  };
  return (
    <Dialog open={open} onOpenChange={(o) => !busy && onOpenChange(o)}>
      <DialogContent size={size}>
        <div style={{ display: 'grid', gap: 6 }}><DialogTitle>{title}</DialogTitle>{description && <DialogDescription>{description}</DialogDescription>}</div>
        {children}
        <DialogFooter>
          <Button onClick={() => onOpenChange(false)} disabled={busy}>取消</Button>
          <Button variant={danger ? 'danger' : 'primary'} loading={busy} disabled={confirmDisabled} onClick={run}>{confirmLabel}</Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}

/** 需要输入 expected 才能确认。expected 区分大小写，两端空格忽略 */
export function TypedConfirmDialog({ expected, prompt, ...base }: Base & { expected: string; prompt?: string }) {
  const [text, setText] = React.useState('');
  const [busy, setBusy] = React.useState(false);
  React.useEffect(() => { if (!base.open) setText(''); }, [base.open]);
  const ok = text.trim() === expected && !base.confirmDisabled;
  const run = async () => {
    setBusy(true);
    try { await base.onConfirm(); base.onOpenChange(false); } catch (e) { toast.err(e); } finally { setBusy(false); }
  };
  return (
    <Dialog open={base.open} onOpenChange={(o) => !busy && base.onOpenChange(o)}>
      <DialogContent size={base.size}>
        <div style={{ display: 'grid', gap: 6 }}><DialogTitle>{base.title}</DialogTitle>{base.description && <DialogDescription>{base.description}</DialogDescription>}</div>
        {base.children}
        <div className="field">
          <label className="label" htmlFor="typed-confirm">{prompt ?? '请输入'} <b className="mono" style={{ color: 'var(--fg)' }}>{expected}</b> 以确认</label>
          <Input id="typed-confirm" value={text} onChange={(e) => setText(e.target.value)} autoComplete="off" spellCheck={false} placeholder={expected}
            onKeyDown={(e) => { if (e.key === 'Enter' && ok && !busy) void run(); }} />
        </div>
        <DialogFooter>
          <Button onClick={() => base.onOpenChange(false)} disabled={busy}>取消</Button>
          <Button variant={base.danger ? 'danger' : 'primary'} disabled={!ok} loading={busy} onClick={run}>{base.confirmLabel}</Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}
