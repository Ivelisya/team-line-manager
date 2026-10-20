import * as React from 'react';
import { CheckCircle2, XCircle } from 'lucide-react';

// 极简 toast：toast.ok('已保存') / toast.err(error)。操作名与按钮名一致（“保存” → “已保存”）。
type T = { id: number; tone: 'ok' | 'bad'; text: string };
let push: ((t: Omit<T, 'id'>) => void) | null = null;
export const toast = {
  ok: (text: string) => push?.({ tone: 'ok', text }),
  err: (e: unknown) => push?.({ tone: 'bad', text: e instanceof Error ? e.message : String(e) }),
};

export function Toaster() {
  const [list, setList] = React.useState<T[]>([]);
  React.useEffect(() => {
    let n = 0;
    push = (t) => {
      const id = ++n; setList((l) => [...l.slice(-3), { ...t, id }]);
      setTimeout(() => setList((l) => l.filter((x) => x.id !== id)), t.tone === 'bad' ? 6000 : 3200);
    };
    return () => { push = null; };
  }, []);
  return (
    <div className="toasts" role="status" aria-live="polite">
      {list.map((t) => (
        <div key={t.id} className="toast" data-tone={t.tone}>
          {t.tone === 'ok' ? <CheckCircle2 size={16} /> : <XCircle size={16} />}
          <div className="toast-body">{t.text}</div>
        </div>
      ))}
    </div>
  );
}
