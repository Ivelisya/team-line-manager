import * as React from 'react';
import { Inbox } from 'lucide-react';

// 单独成文件：客户页只要它，不必为此带上 primitives 里的一大堆（primitives 重新导出它，其余页面照旧从那里取）
/** 空状态：图标放在柔和的圆底里，一句话说明现状；children 放下一步操作 */
export function Empty({ icon, text, hint, children }: { icon?: React.ReactNode; text: string; hint?: React.ReactNode; children?: React.ReactNode }) {
  return <div className="empty"><span className="empty-ic">{icon ?? <Inbox size={22} />}</span><div className="empty-text">{text}</div>{hint && <div className="hint">{hint}</div>}{children}</div>;
}
