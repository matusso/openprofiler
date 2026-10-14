import type { SVGProps } from "react";

/** The public vector master also supplies the favicon and desktop icons. */
export function OpenProfilerLogo({ className = "", ...props }: SVGProps<SVGSVGElement>) {
  return (
    <svg
      viewBox="0 0 64 64"
      width={32}
      height={32}
      className={`brand-mark ${className}`.trim()}
      role="img"
      aria-label="OpenProfiler"
      focusable="false"
      {...props}
    >
      <use href={`${import.meta.env.BASE_URL}brand/openprofiler-mark.svg#mark`} />
    </svg>
  );
}

export function OpenProfilerWordmark() {
  return (
    <span className="brand-wordmark">
      <span>Open</span>
      <span>Profiler</span>
    </span>
  );
}
