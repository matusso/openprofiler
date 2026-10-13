import { useQuery } from "@tanstack/react-query";
import { useState } from "react";
import { sk, useCurrentSession } from "../api/hooks";
import { client } from "../state";
import { ProfileTrees } from "./profile/ProfileTrees";

/** Chip input for the allocated-class filter, with suggestions from JFR allocation events. */
function ClassFilter({ value, onChange }: { value: string[]; onChange: (v: string[]) => void }) {
  const s = useCurrentSession();
  const [text, setText] = useState("");
  const suggestions = useQuery({
    queryKey: sk(s?.id, "alloc-classes"),
    queryFn: async () => {
      const r = await client.events(s!.id, { type: "jdk.ObjectAllocationSample", limit: 5000 });
      const weight = new Map<string, number>();
      for (const row of r.rows) {
        const c = row.fields.objectClass;
        if (typeof c === "string") weight.set(c, (weight.get(c) ?? 0) + Number(row.fields.weight ?? 1));
      }
      return [...weight.entries()].sort((a, b) => b[1] - a[1]).map(([c]) => c);
    },
    enabled: s?.kind === "jfr",
    retry: false,
    staleTime: Infinity,
  });
  const add = (c: string) => {
    const v = c.trim();
    if (v && !value.includes(v)) onChange([...value, v]);
    setText("");
  };
  return (
    <span style={{ display: "inline-flex", alignItems: "center", gap: 4, flexWrap: "wrap" }}>
      <span className="label">Allocated class</span>
      {value.map((c) => (
        <span key={c} className="badge accent mono" style={{ height: 22 }}>
          {c}
          <button
            className="link"
            style={{ color: "inherit", marginLeft: 4 }}
            aria-label={`Remove ${c}`}
            onClick={() => onChange(value.filter((x) => x !== c))}
          >
            ✕
          </button>
        </span>
      ))}
      <input
        className="input mono"
        style={{ width: 170 }}
        list="alloc-class-suggestions"
        placeholder="e.g. byte[]"
        value={text}
        aria-label="Filter by allocated class"
        onChange={(e) => {
          const v = e.target.value;
          // Picking a datalist option fires a change with the full value.
          if (suggestions.data?.includes(v)) add(v);
          else setText(v);
        }}
        onKeyDown={(e) => {
          if (e.key === "Enter") add(text);
          else if (e.key === "Backspace" && !text && value.length) onChange(value.slice(0, -1));
        }}
      />
      <datalist id="alloc-class-suggestions">
        {suggestions.data?.slice(0, 200).map((c) => (
          <option key={c} value={c} />
        ))}
      </datalist>
    </span>
  );
}

export function MemoryView() {
  const [classes, setClasses] = useState<string[]>([]);
  return (
    <ProfileTrees
      view="memory"
      title="Memory · allocations"
      kinds={["alloc"]}
      withStates={false}
      classes={classes}
      toolbarExtra={<ClassFilter value={classes} onChange={setClasses} />}
    />
  );
}
