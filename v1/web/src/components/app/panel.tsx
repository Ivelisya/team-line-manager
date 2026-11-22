import * as React from 'react';
import { cn } from '@/lib/utils';

// 单独成文件：客户页只要它，不必为此带上 primitives 里的一大堆（primitives 重新导出它，其余页面照旧从那里取）
/** ref 落在 section 上（分页器换页后滚回面板顶部用） */
export const Panel = React.forwardRef<HTMLElement, { title?: React.ReactNode; count?: React.ReactNode; sub?: React.ReactNode; tools?: React.ReactNode; children?: React.ReactNode; flush?: boolean; className?: string }>(function Panel({ title, count, sub, tools, children, flush, className }, ref) {
  return (
    <section ref={ref} className={cn('panel', className)}>
      {(title || tools) && (
        <header className="panel-head">
          {title && <h2 className="panel-title">{title}</h2>}
          {count != null && <span className="panel-count">{count}</span>}
          {sub && <span className={cn('panel-sub', typeof sub === 'string' && 'ellipsis')}>{sub}</span>}
          <div className="panel-tools">{tools}</div>
        </header>
      )}
      {children != null && <div className={cn('panel-body', flush && 'is-flush')}>{children}</div>}
    </section>
  );
});
