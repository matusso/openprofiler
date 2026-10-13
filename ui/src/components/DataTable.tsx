import { useVirtualizer } from "@tanstack/react-virtual";
import { useEffect, useRef, type KeyboardEvent, type ReactNode } from "react";

export interface Column<T> {
  id: string;
  header: ReactNode;
  width?: number;
  flex?: boolean;
  align?: "left" | "right";
  render: (row: T, index: number) => ReactNode;
  /** When set, clicking the header requests sorting by this key. */
  sortKey?: string;
  title?: (row: T) => string;
}

export interface DataTableProps<T> {
  rows: T[];
  columns: Column<T>[];
  rowKey: (row: T, index: number) => string | number;
  label: string;
  selected?: string | number | null;
  onSelect?: (row: T, index: number) => void;
  onActivate?: (row: T, index: number) => void;
  onContextMenu?: (row: T, index: number, e: React.MouseEvent) => void;
  sort?: string;
  onSort?: (key: string) => void;
  /** Total row count when paging; triggers `onLoadMore` near the end. */
  totalRows?: number;
  onLoadMore?: () => void;
  rowHeight?: number;
  empty?: ReactNode;
  rowClass?: (row: T) => string | undefined;
}

export function DataTable<T>({
  rows,
  columns,
  rowKey,
  label,
  selected,
  onSelect,
  onActivate,
  onContextMenu,
  sort,
  onSort,
  totalRows,
  onLoadMore,
  rowHeight = 26,
  empty,
  rowClass,
}: DataTableProps<T>) {
  const scrollRef = useRef<HTMLDivElement>(null);
  const virt = useVirtualizer({
    count: rows.length,
    getScrollElement: () => scrollRef.current,
    estimateSize: () => rowHeight,
    overscan: 12,
  });
  const items = virt.getVirtualItems();
  const last = items[items.length - 1];

  useEffect(() => {
    if (!onLoadMore || !last) return;
    if (last.index >= rows.length - 20 && (totalRows ?? 0) > rows.length) onLoadMore();
  }, [last, rows.length, totalRows, onLoadMore]);

  const selIndex = selected == null ? -1 : rows.findIndex((r, i) => rowKey(r, i) === selected);

  const onKey = (e: KeyboardEvent) => {
    if (!rows.length) return;
    let next: number;
    if (e.key === "ArrowDown") next = Math.min(rows.length - 1, selIndex + 1);
    else if (e.key === "ArrowUp") next = Math.max(0, selIndex - 1);
    else if (e.key === "PageDown") next = Math.min(rows.length - 1, selIndex + 20);
    else if (e.key === "PageUp") next = Math.max(0, selIndex - 20);
    else if (e.key === "Home") next = 0;
    else if (e.key === "End") next = rows.length - 1;
    else if (e.key === "Enter" && selIndex >= 0) {
      onActivate?.(rows[selIndex], selIndex);
      e.preventDefault();
      return;
    } else return;
    e.preventDefault();
    if (next >= 0) {
      onSelect?.(rows[next], next);
      virt.scrollToIndex(next, { align: "auto" });
    }
  };

  const cellStyle = (c: Column<T>) =>
    c.flex
      ? { flex: 1, minWidth: c.width ?? 160, textAlign: c.align }
      : { width: c.width ?? 100, textAlign: c.align };

  // Wide tables (e.g. JFR events with many fields) scroll horizontally as a whole.
  const minWidth = columns.reduce((a, c) => a + (c.flex ? (c.width ?? 160) : (c.width ?? 100)), 0);

  return (
    <div className="table table-x" role="grid" aria-label={label} aria-rowcount={totalRows ?? rows.length}>
      <div className="table-inner" style={{ minWidth }}>
        <div className="table-head" role="row">
          {columns.map((c) => (
            <div
              key={c.id}
              role="columnheader"
              className={`cell${c.sortKey ? " sortable" : ""}`}
              style={cellStyle(c)}
              aria-sort={c.sortKey && sort === c.sortKey ? "descending" : undefined}
              onClick={() => c.sortKey && onSort?.(c.sortKey)}
            >
              {c.header}
              {c.sortKey && sort === c.sortKey ? " ▾" : ""}
            </div>
          ))}
        </div>
        <div className="table-scroll" ref={scrollRef} tabIndex={0} onKeyDown={onKey}>
          {rows.length === 0 && empty ? (
            empty
          ) : (
            <div style={{ height: virt.getTotalSize(), position: "relative" }}>
              {items.map((vi) => {
                const row = rows[vi.index];
                const key = rowKey(row, vi.index);
                return (
                  <div
                    key={key}
                    role="row"
                    aria-rowindex={vi.index + 1}
                    aria-selected={key === selected}
                    className={`row ${rowClass?.(row) ?? ""}`}
                    style={{ top: vi.start, height: rowHeight }}
                    onClick={() => onSelect?.(row, vi.index)}
                    onDoubleClick={() => onActivate?.(row, vi.index)}
                    onContextMenu={(e) => {
                      if (!onContextMenu) return;
                      onSelect?.(row, vi.index);
                      onContextMenu(row, vi.index, e);
                    }}
                  >
                    {columns.map((c) => (
                      <div
                        key={c.id}
                        role="gridcell"
                        className={`cell${c.align === "right" ? " num" : ""}`}
                        style={cellStyle(c)}
                        title={c.title?.(row)}
                      >
                        {c.render(row, vi.index)}
                      </div>
                    ))}
                  </div>
                );
              })}
            </div>
          )}
        </div>
      </div>
    </div>
  );
}

/** A proportional bar with a percentage, used in total/self columns. */
export function PctBar({ value, total, self }: { value: number; total: number; self?: boolean }) {
  const pct = total ? Math.max(0, Math.min(1, value / total)) : 0;
  return (
    <span className="bar-cell">
      <span className="bar" aria-hidden="true">
        <span className={self ? "self" : undefined} style={{ width: `${pct * 100}%` }} />
      </span>
      <span className="num" style={{ width: 46 }}>
        {(pct * 100).toFixed(1)}%
      </span>
    </span>
  );
}
