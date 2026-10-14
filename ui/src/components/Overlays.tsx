import { useEffect } from "react";
import { store, useApp } from "../state";

export function Toasts() {
  const toasts = useApp((s) => s.toasts);
  return (
    <div className="toasts" role="status" aria-live="polite">
      {toasts.map((t) => (
        <div key={t.id} className={`toast ${t.tone}`}>
          {t.text}
        </div>
      ))}
    </div>
  );
}

export function ContextMenu() {
  const menu = useApp((s) => s.contextMenu);
  useEffect(() => {
    if (!menu) return;
    const close = () => store.set({ contextMenu: null });
    const key = (e: KeyboardEvent) => e.key === "Escape" && close();
    window.addEventListener("mousedown", close);
    window.addEventListener("scroll", close, true);
    window.addEventListener("keydown", key);
    window.addEventListener("blur", close);
    return () => {
      window.removeEventListener("mousedown", close);
      window.removeEventListener("scroll", close, true);
      window.removeEventListener("keydown", key);
      window.removeEventListener("blur", close);
    };
  }, [menu]);
  if (!menu) return null;
  const x = Math.min(menu.x, window.innerWidth - 240);
  const y = Math.min(menu.y, window.innerHeight - 40 * menu.items.length - 16);
  return (
    <div
      className="menu"
      role="menu"
      style={{ position: "fixed", left: x, top: y, minWidth: 220 }}
      onMouseDown={(e) => e.stopPropagation()}
    >
      {menu.items.map((it) => (
        <button
          key={it.label}
          role="menuitem"
          className="menu-item"
          disabled={it.disabled}
          onClick={() => {
            store.set({ contextMenu: null });
            it.run();
          }}
        >
          {it.label}
        </button>
      ))}
    </div>
  );
}
