import { useQuery, useQueryClient } from "@tanstack/react-query";
import { useVirtualizer } from "@tanstack/react-virtual";
import { useEffect, useMemo, useRef, useState, type KeyboardEvent, type ReactNode } from "react";
import { LazyTree, type FlatRow, type LazyNode } from "../lib/lazyTree";
import { IconChevron } from "./icons";

export interface TreeColumn<N> {
  id: string;
  header: ReactNode;
  width: number;
  align?: "left" | "right";
  render: (n: N) => ReactNode;
}

export interface LazyTreeTableProps<N extends LazyNode> {
  label: string;
  queryKey: unknown[];
  /** Fetches `node` (root when undefined) plus `levels` levels below it, pre-order. */
  fetch: (node: number | undefined, levels: number) => Promise<N[]>;
  renderName: (n: N) => ReactNode;
  columns: TreeColumn<N>[];
  weight: (n: N) => number;
  onSelect?: (n: N) => void;
  onActivate?: (n: N) => void;
  onContextMenu?: (n: N, e: React.MouseEvent) => void;
  /** Text matched against `text(n)` for "find next" (Enter in the search box). */
  search?: string;
  /** Changes on every "find next" request (Enter in the search box). */
  findToken?: string;
  text?: (n: N) => string;
  /** Node path (ids) to reveal and select, e.g. after a jump from the command palette. */
  reveal?: number[] | null;
  autoExpand?: boolean;
  empty?: ReactNode;
  /** Levels requested for the root and for each expansion (server-side depth). */
  initialLevels?: number;
  expandLevels?: number;
}

const ROW_H = 26;

export function LazyTreeTable<N extends LazyNode>(props: LazyTreeTableProps<N>) {
  const { queryKey, fetch, weight, autoExpand = true, initialLevels = 4, expandLevels = 2 } = props;
  const qc = useQueryClient();
  const root = useQuery({
    queryKey: [...queryKey, "root"],
    queryFn: () => fetch(undefined, initialLevels),
  });
  const [tree, setTree] = useState(() => new LazyTree<N>());
  const [version, setVersion] = useState(0);
  const [expanded, setExpanded] = useState<Set<number>>(new Set());
  const [selected, setSelected] = useState<number | null>(null);
  const loading = useRef(new Set<number>());
  /** Keeps following the hot path as lazily loaded levels arrive, until the user interacts. */
  const autoExpanding = useRef(false);
  const scrollRef = useRef<HTMLDivElement>(null);

  // Rebuild when the root query returns new data.
  useEffect(() => {
    if (!root.data) return;
    const t = new LazyTree<N>();
    t.merge(root.data, initialLevels);
    loading.current.clear();
    setTree(t);
    setVersion((v) => v + 1);
    autoExpanding.current = autoExpand;
    if (autoExpand) {
      setExpanded(new Set(t.hotPath(weight, 0.25)));
    } else setExpanded(new Set());
    setSelected(null);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [root.data]);

  // Load children of expanded-but-unloaded nodes.
  useEffect(() => {
    for (const id of expanded) {
      if (!tree.nodes.has(id) || tree.isLoaded(id) || loading.current.has(id)) continue;
      loading.current.add(id);
      qc.fetchQuery({ queryKey: [...queryKey, "node", id], queryFn: () => fetch(id, expandLevels) })
        .then((nodes) => {
          tree.merge(nodes, expandLevels);
          setVersion((v) => v + 1);
          if (autoExpanding.current) setExpanded((e) => new Set([...e, ...tree.hotPath(weight, 0.25)]));
        })
        .catch(() => undefined)
        .finally(() => loading.current.delete(id));
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [expanded, tree, version]);

  // Reveal a requested path.
  useEffect(() => {
    if (!props.reveal?.length) return;
    setExpanded((e) => {
      const n = new Set(e);
      props.reveal!.slice(0, -1).forEach((id) => n.add(id));
      return n;
    });
    setSelected(props.reveal[props.reveal.length - 1]);
  }, [props.reveal]);

  const rows = useMemo(
    () => tree.flatten(expanded),
    // eslint-disable-next-line react-hooks/exhaustive-deps
    [tree, expanded, version],
  );

  const virt = useVirtualizer({
    count: rows.length,
    getScrollElement: () => scrollRef.current,
    estimateSize: () => ROW_H,
    overscan: 16,
  });

  const selIndex = selected == null ? -1 : rows.findIndex((r) => r.node.id === selected);

  useEffect(() => {
    if (selIndex >= 0) virt.scrollToIndex(selIndex, { align: "auto" });
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [selIndex]);

  const toggle = (id: number, open?: boolean) => {
    autoExpanding.current = false;
    setExpanded((e) => {
      const n = new Set(e);
      const want = open ?? !n.has(id);
      if (want) n.add(id);
      else n.delete(id);
      return n;
    });
  };

  /** Expands the whole loaded subtree below a node (Shift+→ / "*"). */
  const expandAll = (id: number, depth = 6) => {
    const ids: number[] = [];
    const walk = (x: number, d: number) => {
      if (d > depth) return;
      ids.push(x);
      for (const c of tree.children.get(x) ?? []) walk(c, d + 1);
    };
    walk(id, 0);
    setExpanded((e) => new Set([...e, ...ids]));
  };

  const select = (row: FlatRow<N> | undefined) => {
    if (!row) return;
    setSelected(row.node.id);
    props.onSelect?.(row.node);
  };

  // "Find next": each new `findToken` selects the next loaded row matching `search`.
  useEffect(() => {
    const q = props.search?.trim().toLowerCase();
    if (!props.findToken || !q || !props.text) return;
    for (let k = 1; k <= rows.length; k++) {
      const r = rows[(Math.max(-1, selIndex) + k) % rows.length];
      if (props.text(r.node).toLowerCase().includes(q)) {
        select(r);
        break;
      }
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [props.findToken]);

  const onKey = (e: KeyboardEvent) => {
    if (!rows.length) return;
    const cur = rows[selIndex];
    switch (e.key) {
      case "ArrowDown":
        select(rows[Math.min(rows.length - 1, selIndex + 1)]);
        break;
      case "ArrowUp":
        select(rows[Math.max(0, selIndex - 1)]);
        break;
      case "PageDown":
        select(rows[Math.min(rows.length - 1, selIndex + 20)]);
        break;
      case "PageUp":
        select(rows[Math.max(0, selIndex - 20)]);
        break;
      case "Home":
        select(rows[0]);
        break;
      case "End":
        select(rows[rows.length - 1]);
        break;
      case "ArrowRight":
        if (!cur) break;
        if (e.shiftKey) expandAll(cur.node.id);
        else if (cur.node.child_count > 0 && !cur.expanded) toggle(cur.node.id, true);
        else if (cur.expanded) select(rows[selIndex + 1]);
        break;
      case "*":
        if (cur) expandAll(cur.node.id);
        break;
      case "ArrowLeft":
        if (!cur) break;
        if (cur.expanded) toggle(cur.node.id, false);
        else {
          const pi = rows.findIndex((r) => r.node.id === cur.node.parent);
          if (pi >= 0) select(rows[pi]);
        }
        break;
      case "Enter":
        if (cur) props.onActivate?.(cur.node);
        break;
      default:
        return;
    }
    e.preventDefault();
  };

  if (root.isError) return <div className="error">Failed to load: {String(root.error)}</div>;

  return (
    <div className="table" role="treegrid" aria-label={props.label}>
      <div className="table-head" role="row">
        <div className="cell flex" role="columnheader">
          Name
        </div>
        {props.columns.map((c) => (
          <div
            key={c.id}
            className="cell"
            role="columnheader"
            style={{ width: c.width, textAlign: c.align ?? "right" }}
          >
            {c.header}
          </div>
        ))}
      </div>
      <div className="table-scroll" ref={scrollRef} tabIndex={0} onKeyDown={onKey}>
        {root.isLoading ? (
          <div className="empty">
            <div className="spinner" />
          </div>
        ) : rows.length === 0 ? (
          (props.empty ?? <div className="empty">No data for the current selection.</div>)
        ) : (
          <div style={{ height: virt.getTotalSize(), position: "relative" }}>
            {virt.getVirtualItems().map((vi) => {
              const r = rows[vi.index];
              const n = r.node;
              return (
                <div
                  key={n.id}
                  role="row"
                  aria-level={r.level + 1}
                  aria-expanded={n.child_count > 0 ? r.expanded : undefined}
                  aria-selected={n.id === selected}
                  className="row"
                  style={{ top: vi.start, height: ROW_H }}
                  onClick={() => select(r)}
                  onDoubleClick={() => props.onActivate?.(n)}
                  onContextMenu={(e) => {
                    if (!props.onContextMenu) return;
                    select(r);
                    props.onContextMenu(n, e);
                  }}
                >
                  <div className="cell flex" role="gridcell" style={{ paddingLeft: 6 + r.level * 14 }}>
                    <span className="tree-label">
                      <span
                        className={`tree-toggle${r.expanded ? " open" : ""}`}
                        style={{ visibility: n.child_count > 0 ? "visible" : "hidden" }}
                        onClick={(e) => {
                          e.stopPropagation();
                          if (e.altKey || e.shiftKey) expandAll(n.id);
                          else toggle(n.id);
                        }}
                        aria-hidden="true"
                      >
                        <IconChevron />
                      </span>
                      {props.renderName(n)}
                    </span>
                  </div>
                  {props.columns.map((c) => (
                    <div
                      key={c.id}
                      role="gridcell"
                      className="cell num"
                      style={{ width: c.width, textAlign: c.align ?? "right" }}
                    >
                      {c.render(n)}
                    </div>
                  ))}
                </div>
              );
            })}
          </div>
        )}
      </div>
    </div>
  );
}
