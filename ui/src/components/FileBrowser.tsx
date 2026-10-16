import { keepPreviousData, useQuery } from "@tanstack/react-query";
import { useState } from "react";
import type { FsEntry } from "../api/types";
import { formatBytes } from "../lib/format";
import { client } from "../state";
import { IconOpen } from "./icons";

const KIND_LABEL: Record<string, string> = { profile: "profile", heap: "heap dump", code: "code" };

/**
 * Browses directories on the machine running the daemon. A click selects a file or enters a
 * directory; a double click (or Enter) opens a file. A directory of `.class` files can be opened
 * as a whole.
 */
export function FileBrowser({
  start,
  selected,
  onSelect,
  onOpen,
}: {
  start?: string;
  selected: string;
  onSelect: (path: string) => void;
  onOpen: (path: string) => void;
}) {
  const [dir, setDir] = useState<string | undefined>(start);
  const [hidden, setHidden] = useState(false);
  const [onlyKnown, setOnlyKnown] = useState(false);
  const q = useQuery({
    queryKey: ["fs", dir ?? "", hidden],
    queryFn: () => client.fsList(dir, hidden),
    placeholderData: keepPreviousData,
    retry: false,
  });
  const l = q.data;
  const entries = (l?.entries ?? []).filter((e) => !onlyKnown || e.dir || e.kind);
  const enter = (e: FsEntry) => (e.dir ? setDir(e.path) : onOpen(e.path));
  return (
    <div className="fs-browser">
      <div className="fs-bar">
        <button
          className="btn"
          disabled={!l?.parent}
          onClick={() => l?.parent && setDir(l.parent)}
          title="Parent directory"
        >
          ↑ Up
        </button>
        <button
          className="btn"
          disabled={!l?.home}
          onClick={() => l?.home && setDir(l.home)}
          title="Home directory"
        >
          ~ Home
        </button>
        <span className="fs-path mono" title={l?.path}>
          {l?.path ?? "…"}
        </span>
        {q.isFetching && <div className="spinner" />}
      </div>
      <div className="fs-options">
        <label>
          <input type="checkbox" checked={onlyKnown} onChange={(e) => setOnlyKnown(e.target.checked)} /> Only
          openable files
        </label>
        <label>
          <input type="checkbox" checked={hidden} onChange={(e) => setHidden(e.target.checked)} /> Hidden
          files
        </label>
        <span className="grow" />
        <button
          className="btn"
          disabled={!l}
          onClick={() => l && onOpen(l.path)}
          title="Open this directory: its .class files are decompiled (e.g. target/classes)"
        >
          Open folder as classes
        </button>
      </div>
      {q.isError && <div className="error">{String(q.error.message)}</div>}
      <div className="list fs-list" role="listbox" aria-label="Files">
        {entries.map((e) => (
          <button
            key={e.path}
            role="option"
            aria-selected={e.path === selected}
            className={`list-item mono${e.kind || e.dir ? "" : " faint"}`}
            onClick={() => (e.dir ? setDir(e.path) : onSelect(e.path))}
            onDoubleClick={() => !e.dir && onOpen(e.path)}
            onKeyDown={(k) => k.key === "Enter" && (k.preventDefault(), enter(e))}
            title={e.path}
          >
            {e.dir ? <IconOpen /> : <span className="fs-icon" />}
            <span className="grow fs-name">{e.name}</span>
            {e.kind && <span className="badge">{KIND_LABEL[e.kind] ?? e.kind}</span>}
            {!e.dir && <span className="faint fs-size">{formatBytes(e.size)}</span>}
          </button>
        ))}
        {l && entries.length === 0 && <div className="empty">Nothing here</div>}
        {l?.truncated && <div className="muted">Only the first entries are shown.</div>}
      </div>
    </div>
  );
}
