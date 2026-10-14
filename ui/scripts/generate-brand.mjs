import { execFileSync } from "node:child_process";
import { copyFileSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const ui = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const publicDir = join(ui, "public");
const source = readFileSync(join(publicDir, "brand/openprofiler-mark.svg"), "utf8");
const geometry = source.match(/<g id="mark">([\s\S]*?)<\/g>/)?.[1];
if (!geometry) throw new Error("The logo master must contain a group with id=mark.");

const icon = `<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 64 64" fill="none">
  <title>OpenProfiler</title>
  <rect width="64" height="64" rx="16" fill="#151820"/>
  <g color="#ff7a3d">${geometry}  </g>
</svg>
`;
writeFileSync(join(publicDir, "favicon.svg"), icon);
writeFileSync(
  join(publicDir, "brand/openprofiler-logo.svg"),
  `<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 350 64" fill="none" color="#171a21">
  <title>OpenProfiler</title>
  <g color="#ff7a3d">${geometry}  </g>
  <text x="80" y="44" fill="currentColor" font-family="Inter, -apple-system, BlinkMacSystemFont, Segoe UI, sans-serif" font-size="38" letter-spacing="-1.5"><tspan font-weight="400">Open</tspan><tspan font-weight="700">Profiler</tspan></text>
</svg>
`,
);

const temp = mkdtempSync(join(tmpdir(), "openprofiler-brand-"));
try {
  const desktop = join(temp, "desktop");
  const touch = join(temp, "touch");
  const run = (args) =>
    execFileSync("npm", ["exec", "tauri", "icon", "--", ...args], { cwd: ui, stdio: "inherit" });
  run(["public/favicon.svg", "--output", desktop]);
  run(["public/favicon.svg", "--output", touch, "--png", "180"]);
  mkdirSync(join(ui, "src-tauri/icons"), { recursive: true });
  for (const file of ["32x32.png", "128x128.png", "128x128@2x.png", "icon.png", "icon.ico", "icon.icns"]) {
    copyFileSync(join(desktop, file), join(ui, "src-tauri/icons", file));
  }
  copyFileSync(join(desktop, "32x32.png"), join(publicDir, "favicon-32x32.png"));
  copyFileSync(join(desktop, "icon.ico"), join(publicDir, "favicon.ico"));
  copyFileSync(join(touch, "180x180.png"), join(publicDir, "apple-touch-icon.png"));
} finally {
  rmSync(temp, { recursive: true, force: true });
}
