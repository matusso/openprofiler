import type { FrameKindName } from "../api/types";
import { splitLabel } from "../lib/format";

export const KIND_COLORS: Record<FrameKindName, string> = {
  interpreted: "var(--c5)",
  jit: "var(--c3)",
  inlined: "var(--c7)",
  native: "var(--c4)",
  kernel: "var(--c6)",
  cpp: "var(--c1)",
  unknown: "var(--c8)",
};

/** Highlights case-insensitive occurrences of `q` in `text`. */
export function Highlight({ text, q }: { text: string; q?: string }) {
  if (!q) return <>{text}</>;
  const lower = text.toLowerCase();
  const needle = q.toLowerCase();
  const parts: React.ReactNode[] = [];
  let i = 0;
  let n = 0;
  while (i < text.length) {
    const j = lower.indexOf(needle, i);
    if (j < 0) {
      parts.push(text.slice(i));
      break;
    }
    if (j > i) parts.push(text.slice(i, j));
    parts.push(
      <mark className="highlight" key={n++}>
        {text.slice(j, j + needle.length)}
      </mark>,
    );
    i = j + needle.length;
  }
  return <>{parts}</>;
}

/** Renders `pkg.Class.member` with muted package and accented member. */
export function MethodLabel({
  label,
  kind,
  q,
  plain,
}: {
  label: string;
  kind?: FrameKindName;
  q?: string;
  plain?: boolean;
}) {
  const { pkg, cls, member } = splitLabel(label);
  return (
    <span className="tree-label" title={label}>
      {kind && <span className="kind-chip" style={{ background: KIND_COLORS[kind] }} title={kind} />}
      <span className="text">
        {plain || !cls ? (
          <Highlight text={label} q={q} />
        ) : (
          <>
            {pkg && (
              <span className="label-pkg">
                <Highlight text={pkg + "."} q={q} />
              </span>
            )}
            <span className="label-cls">
              <Highlight text={cls} q={q} />
            </span>
            <span className="label-pkg">.</span>
            <span className="label-member">
              <Highlight text={member} q={q} />
            </span>
          </>
        )}
      </span>
    </span>
  );
}
