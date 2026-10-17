import type { SVGProps } from "react";

type P = SVGProps<SVGSVGElement>;

const base = (d: React.ReactNode) =>
  function Icon(props: P) {
    return (
      <svg
        viewBox="0 0 24 24"
        fill="none"
        stroke="currentColor"
        strokeWidth={1.8}
        strokeLinecap="round"
        strokeLinejoin="round"
        aria-hidden="true"
        {...props}
      >
        {d}
      </svg>
    );
  };

export const IconOverview = base(
  <>
    <rect x="3" y="3" width="7" height="9" rx="1.5" />
    <rect x="14" y="3" width="7" height="5" rx="1.5" />
    <rect x="14" y="12" width="7" height="9" rx="1.5" />
    <rect x="3" y="16" width="7" height="5" rx="1.5" />
  </>,
);
export const IconCpu = base(
  <>
    <rect x="6" y="6" width="12" height="12" rx="2" />
    <path d="M9 2v4M15 2v4M9 18v4M15 18v4M2 9h4M2 15h4M18 9h4M18 15h4" />
  </>,
);
export const IconMemory = base(
  <>
    <rect x="2" y="7" width="20" height="10" rx="2" />
    <path d="M6 7v10M10 7v10M14 7v10M18 7v10" />
  </>,
);
export const IconHeap = base(
  <>
    <circle cx="12" cy="12" r="9" />
    <circle cx="12" cy="12" r="4.5" />
    <path d="M12 3v4.5M21 12h-4.5" />
  </>,
);
export const IconThreads = base(<path d="M3 6h12M3 12h18M3 18h8M18 4l3 2-3 2M14 16l3 2-3 2" />);
export const IconLock = base(
  <>
    <rect x="4" y="11" width="16" height="10" rx="2" />
    <path d="M8 11V7a4 4 0 0 1 8 0v4" />
  </>,
);
export const IconProbe = base(
  <>
    <path d="M10 2v6L4 20a1 1 0 0 0 1 1.5h14a1 1 0 0 0 1-1.5L14 8V2" />
    <path d="M8.5 2h7M7 14h10" />
  </>,
);
export const IconMethods = base(
  <>
    <path d="M4 5h6M7 5v14M4 19h6" />
    <path d="M13 9l3 3-3 3M17 15h3" />
  </>,
);
export const IconEvents = base(
  <>
    <path d="M4 6h16M4 12h16M4 18h10" />
    <circle cx="19" cy="18" r="2" />
  </>,
);
export const IconBean = base(
  <>
    <path d="M12 3c5 0 8 3 8 7s-3 11-8 11-8-7-8-11 3-7 8-7z" />
    <path d="M9 10c1.5 2 4.5 2 6 0" />
  </>,
);
export const IconCode = base(<path d="M8 7l-5 5 5 5M16 7l5 5-5 5M13.5 4l-3 16" />);
export const IconCompare = base(<path d="M8 3v18M16 3v18M3 8h5M16 16h5M3 16h5M16 8h5" />);
export const IconChevron = base(<path d="M9 5l7 7-7 7" />);
export const IconSearch = base(
  <>
    <circle cx="11" cy="11" r="7" />
    <path d="M20 20l-3.5-3.5" />
  </>,
);
export const IconOpen = base(
  <path d="M3 7a2 2 0 0 1 2-2h4l2 2h8a2 2 0 0 1 2 2v8a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2z" />,
);
export const IconPlug = base(<path d="M9 2v6M15 2v6M6 8h12v3a6 6 0 0 1-12 0zM12 17v5" />);
export const IconClose = base(<path d="M6 6l12 12M18 6L6 18" />);
export const IconSun = base(
  <>
    <circle cx="12" cy="12" r="4" />
    <path d="M12 2v2M12 20v2M2 12h2M20 12h2M4.9 4.9l1.4 1.4M17.7 17.7l1.4 1.4M4.9 19.1l1.4-1.4M17.7 6.3l1.4-1.4" />
  </>,
);
export const IconMoon = base(<path d="M20 14.5A8.5 8.5 0 0 1 9.5 4a8.5 8.5 0 1 0 10.5 10.5z" />);
export const IconContrast = base(
  <>
    <circle cx="12" cy="12" r="9" />
    <path d="M12 3v18a9 9 0 0 0 0-18z" fill="currentColor" />
  </>,
);
export const IconPanel = base(
  <>
    <rect x="3" y="4" width="18" height="16" rx="2" />
    <path d="M15 4v16" />
  </>,
);
export const IconRecord = base(<circle cx="12" cy="12" r="6" fill="currentColor" />);
export const IconCamera = base(
  <>
    <path d="M4 8h3l2-3h6l2 3h3a1 1 0 0 1 1 1v10a1 1 0 0 1-1 1H4a1 1 0 0 1-1-1V9a1 1 0 0 1 1-1z" />
    <circle cx="12" cy="13" r="3.5" />
  </>,
);
export const IconBookmark = base(<path d="M6 3h12v18l-6-4-6 4z" />);
export const IconTrash = base(<path d="M4 7h16M10 11v6M14 11v6M5 7l1 13h12l1-13M9 7V4h6v3" />);
export const IconDownload = base(<path d="M12 3v12M7 10l5 5 5-5M4 21h16" />);
export const IconFlame = base(
  <path d="M12 2c1 4 6 6 6 12a6 6 0 0 1-12 0c0-3 2-5 3-6 0 2 1 3 2 3 0-3-1-6 1-9z" />,
);
export const IconZoomOut = base(
  <>
    <circle cx="11" cy="11" r="7" />
    <path d="M8 11h6M20 20l-3.5-3.5" />
  </>,
);

export { OpenProfilerLogo } from "./Brand";
export const IconShare = base(
  <>
    <circle cx="18" cy="5" r="2.5" />
    <circle cx="6" cy="12" r="2.5" />
    <circle cx="18" cy="19" r="2.5" />
    <path d="M8.2 10.8l7.6-4.4M8.2 13.2l7.6 4.4" />
  </>,
);
