import ELK from "elkjs/lib/elk-api.js";
import type { ElkNode } from "elkjs/lib/elk-api";
import workerUrl from "elkjs/lib/elk-worker.min.js?url";

let elk: InstanceType<typeof ELK> | null = null;

/** Lays out a graph with ELK's layered algorithm; ELK runs in its own web worker. */
export function layoutGraph(graph: ElkNode): Promise<ElkNode> {
  if (!elk) elk = new ELK({ workerFactory: () => new Worker(workerUrl) });
  return elk.layout(graph);
}
