/** Client-side cache for server trees that are fetched lazily, a few levels at a time. */
export interface LazyNode {
  id: number;
  parent: number | null;
  child_count: number;
}

export interface FlatRow<N> {
  node: N;
  level: number;
  expanded: boolean;
  loaded: boolean;
}

export class LazyTree<N extends LazyNode> {
  readonly nodes = new Map<number, N>();
  readonly children = new Map<number, number[]>();
  readonly depth = new Map<number, number>();
  rootId: number | null = null;

  /**
   * Merges a pre-order response whose first node is the requested node and which contains
   * `levels` complete levels below it. Nodes above the cut-off get their child lists marked loaded.
   */
  merge(resp: N[], levels: number): void {
    if (!resp.length) return;
    const first = resp[0];
    if (this.rootId === null) this.rootId = first.id;
    const baseDepth = this.depth.get(first.id) ?? 0;
    this.depth.set(first.id, baseDepth);
    const grouped = new Map<number, number[]>();
    for (const n of resp) {
      this.nodes.set(n.id, n);
      if (n !== first && n.parent !== null) {
        const d = (this.depth.get(n.parent) ?? baseDepth) + 1;
        this.depth.set(n.id, d);
        let g = grouped.get(n.parent);
        if (!g) grouped.set(n.parent, (g = []));
        g.push(n.id);
      }
    }
    for (const n of resp) {
      const d = this.depth.get(n.id) ?? baseDepth;
      if (d - baseDepth < levels) this.children.set(n.id, grouped.get(n.id) ?? []);
    }
  }

  isLoaded(id: number): boolean {
    return this.children.has(id) || this.nodes.get(id)?.child_count === 0;
  }

  /** Visible rows below the root (root itself excluded unless `includeRoot`). */
  flatten(expanded: ReadonlySet<number>, includeRoot = false): FlatRow<N>[] {
    const out: FlatRow<N>[] = [];
    if (this.rootId === null) return out;
    const visit = (id: number, level: number) => {
      const node = this.nodes.get(id);
      if (!node) return;
      const isExp = expanded.has(id);
      out.push({ node, level, expanded: isExp, loaded: this.isLoaded(id) });
      if (isExp) for (const c of this.children.get(id) ?? []) visit(c, level + 1);
    };
    if (includeRoot) visit(this.rootId, 0);
    else for (const c of this.children.get(this.rootId) ?? []) visit(c, 0);
    return out;
  }

  /** Ids along the "hot path": repeatedly the first child while it holds ≥ `share` of its parent. */
  hotPath(weight: (n: N) => number, share = 0.3, maxDepth = 30): number[] {
    const path: number[] = [];
    if (this.rootId === null) return path;
    let cur = this.rootId;
    const rootW = weight(this.nodes.get(cur)!) || 1;
    for (let i = 0; i < maxDepth; i++) {
      const kids = this.children.get(cur);
      if (!kids?.length) break;
      const top = this.nodes.get(kids[0])!;
      if (weight(top) / rootW < share) break;
      path.push(top.id);
      cur = top.id;
    }
    return path;
  }

  ancestors(id: number): number[] {
    const out: number[] = [];
    let cur = this.nodes.get(id)?.parent ?? null;
    while (cur !== null && cur !== undefined) {
      out.push(cur);
      cur = this.nodes.get(cur)?.parent ?? null;
    }
    return out;
  }
}
