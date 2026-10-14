// OpenProfiler VS Code extension: a thin client of the daemon's IDE bridge (docs/spec/query-api.md, /api/v1/ide).
import * as vscode from "vscode";
import * as fs from "fs";
import * as os from "os";
import * as path from "path";

interface LineHeat {
  class: string;
  method: string;
  line: number;
  self_percent: number;
  total_percent: number;
}

interface Discovery {
  url: string;
  token: string;
}

interface AgentLaunch {
  path: string;
  options: string;
}

/** Debug configuration attribute that marks a Java launch for profiling, and the one we add to track it. */
const PROFILE_ATTR = "openprofiler";
const INFOFILE_ATTR = "openprofilerInfofile";

let socket: WebSocket | undefined;
let nextId = 1;
const pending = new Map<number, (v: unknown, err?: string) => void>();
// Four heat levels; VS Code decorations can't vary background per instance.
const HEAT_LEVELS = [0.08, 0.16, 0.28, 0.42];
let heat: vscode.TextEditorDecorationType[] = [];
let status: vscode.StatusBarItem;
// Debug sessions running with the agent; while any is alive the gutter heat refreshes periodically.
const profiled = new Set<string>();
let liveRefresh: ReturnType<typeof setInterval> | undefined;

function discovery(): Discovery | undefined {
  const cfg = vscode.workspace.getConfiguration("openprofiler");
  const url = cfg.get<string>("daemonUrl");
  const token = cfg.get<string>("token");
  if (url && token) return { url, token };
  try {
    const file = path.join(os.homedir(), ".openprofiler", "daemon.json");
    return JSON.parse(fs.readFileSync(file, "utf8")) as Discovery;
  } catch {
    return undefined;
  }
}

function call<T>(method: string, params: object): Promise<T> {
  return new Promise((resolve, reject) => {
    if (!socket || socket.readyState !== WebSocket.OPEN) {
      reject(new Error("not connected to the OpenProfiler daemon"));
      return;
    }
    const id = nextId++;
    pending.set(id, (v, err) => (err ? reject(new Error(err)) : resolve(v as T)));
    socket.send(JSON.stringify({ jsonrpc: "2.0", id, method, params }));
  });
}

async function connect(): Promise<void> {
  const d = discovery();
  if (!d) {
    void vscode.window.showWarningMessage("OpenProfiler daemon not found. Start `openprofiler daemon` or set openprofiler.daemonUrl/openprofiler.token.");
    return;
  }
  socket?.close();
  const wsUrl = d.url.replace(/^http/, "ws") + `/api/v1/ide?token=${encodeURIComponent(d.token)}`;
  const ws = new WebSocket(wsUrl);
  socket = ws;
  ws.onopen = () => {
    status.text = "$(flame) OpenProfiler";
    status.tooltip = `Connected to ${d.url}`;
    void call("hello", { ide: "vscode", version: vscode.version }).then(() => refreshAll());
  };
  ws.onclose = () => {
    status.text = "$(flame) OpenProfiler (offline)";
  };
  ws.onmessage = (ev: MessageEvent) => {
    const msg = JSON.parse(String(ev.data));
    if (msg.id !== undefined && pending.has(msg.id)) {
      pending.get(msg.id)!(msg.result, msg.error?.message);
      pending.delete(msg.id);
    } else if (msg.method === "navigate") {
      void navigate(msg.params);
    }
  };
}

/** Top-level class names declared in a Java/Kotlin file, from its package and file name. */
function classesOf(doc: vscode.TextDocument): string[] {
  const pkg = /^\s*package\s+([\w.]+)/m.exec(doc.getText())?.[1];
  const base = path.basename(doc.fileName).replace(/\.(java|kt|scala)$/, "");
  const names = [pkg ? `${pkg}.${base}` : base];
  if (doc.fileName.endsWith(".kt")) names.push(`${names[0]}Kt`);
  return names;
}

async function refresh(editor: vscode.TextEditor): Promise<void> {
  if (!/\.(java|kt|scala)$/.test(editor.document.fileName)) return;
  const min = vscode.workspace.getConfiguration("openprofiler").get<number>("minPercent") ?? 0.5;
  let res: { lines: LineHeat[] };
  try {
    res = await call("gutterData", { classes: classesOf(editor.document) });
  } catch {
    return;
  }
  if (heat.length === 0) {
    heat = HEAT_LEVELS.map((a) =>
      vscode.window.createTextEditorDecorationType({ isWholeLine: true, backgroundColor: `rgba(232, 89, 12, ${a})` }),
    );
  }
  const buckets: vscode.DecorationOptions[][] = HEAT_LEVELS.map(() => []);
  for (const l of res.lines) {
    if (l.total_percent < min || l.line < 1 || l.line > editor.document.lineCount) continue;
    const level = l.total_percent >= 25 ? 3 : l.total_percent >= 10 ? 2 : l.total_percent >= 3 ? 1 : 0;
    buckets[level].push({
      range: editor.document.lineAt(l.line - 1).range,
      hoverMessage: new vscode.MarkdownString(
        `**OpenProfiler CPU** \`${l.method}\` line ${l.line}: total ${l.total_percent}% · self ${l.self_percent}%`,
      ),
      renderOptions: {
        after: { contentText: `  ${l.total_percent.toFixed(1)}%`, color: new vscode.ThemeColor("editorCodeLens.foreground") },
      },
    });
  }
  heat.forEach((t, i) => editor.setDecorations(t, buckets[i]));
}

function refreshAll(): void {
  for (const e of vscode.window.visibleTextEditors) void refresh(e);
}

/** The agent library and options: the `openprofiler.agentPath` setting, else whatever the daemon uses. */
async function agentLaunch(): Promise<AgentLaunch | undefined> {
  const configured = vscode.workspace.getConfiguration("openprofiler").get<string>("agentPath");
  if (configured) return { path: configured, options: "tls" };
  try {
    return await call<AgentLaunch>("agent", {});
  } catch {
    return undefined;
  }
}

function agentOption(agent: AgentLaunch, infofile?: string): string {
  return `-agentpath:${agent.path}=${agent.options}${infofile ? `,infofile=${infofile}` : ""}`;
}

/** Adds the agent to Java launch configurations that set `"openprofiler": true`. */
const javaLaunchProvider: vscode.DebugConfigurationProvider = {
  async resolveDebugConfiguration(_folder, config) {
    if (!config[PROFILE_ATTR] || config.request !== "launch") return config;
    const agent = await agentLaunch();
    if (!agent) {
      void vscode.window.showErrorMessage(
        "OpenProfiler: cannot find the agent. Start `openprofiler daemon` or set openprofiler.agentPath.",
      );
      return undefined;
    }
    const infofile = path.join(os.tmpdir(), `openprofiler-vscode-${process.pid}-${Date.now()}.json`);
    const arg = agentOption(agent, infofile);
    if (Array.isArray(config.vmArgs)) config.vmArgs = [...config.vmArgs, arg];
    else config.vmArgs = [config.vmArgs, /\s/.test(arg) ? `"${arg}"` : arg].filter(Boolean).join(" ");
    config[INFOFILE_ATTR] = infofile;
    return config;
  },
};

async function onSessionStarted(session: vscode.DebugSession): Promise<void> {
  const infofile = session.configuration[INFOFILE_ATTR] as string | undefined;
  if (!infofile) return;
  profiled.add(session.id);
  liveRefresh ??= setInterval(refreshAll, 5000);
  try {
    const s = await call<{ name: string }>("attachLaunched", { infofile });
    void vscode.window.showInformationMessage(`OpenProfiler: profiling ${s.name}`);
  } catch (e) {
    onSessionEnded(session);
    void vscode.window.showWarningMessage(`OpenProfiler: could not connect to the launched JVM: ${(e as Error).message}`);
  }
}

function onSessionEnded(session: vscode.DebugSession): void {
  if (!profiled.delete(session.id) || profiled.size > 0) return;
  clearInterval(liveRefresh);
  liveRefresh = undefined;
  refreshAll();
}

/** Picks a Java launch configuration from launch.json (or the current file) and runs it with the agent. */
async function launchWithAgent(): Promise<void> {
  type Item = vscode.QuickPickItem & { folder?: vscode.WorkspaceFolder; config: vscode.DebugConfiguration };
  const items: Item[] = [];
  for (const folder of vscode.workspace.workspaceFolders ?? []) {
    const configs = vscode.workspace.getConfiguration("launch", folder).get<vscode.DebugConfiguration[]>("configurations") ?? [];
    for (const config of configs) {
      if (config.type === "java" && config.request === "launch") {
        items.push({ label: config.name, description: folder.name, folder, config });
      }
    }
  }
  const file = vscode.window.activeTextEditor?.document;
  if (file && /\.java$/.test(file.fileName)) {
    items.push({
      label: `Current file: ${path.basename(file.fileName)}`,
      folder: vscode.workspace.getWorkspaceFolder(file.uri),
      config: { type: "java", request: "launch", name: `OpenProfiler: ${path.basename(file.fileName)}`, mainClass: "${file}" },
    });
  }
  if (items.length === 0) {
    void vscode.window.showInformationMessage(
      "OpenProfiler: no Java launch configuration found. Add one to launch.json (needs the Debugger for Java extension).",
    );
    return;
  }
  const pick = items.length === 1 ? items[0] : await vscode.window.showQuickPick(items, { placeHolder: "Launch with the OpenProfiler agent" });
  if (!pick) return;
  await vscode.debug.startDebugging(pick.folder, { ...pick.config, [PROFILE_ATTR]: true }, { noDebug: true });
}

async function navigate(p: { class: string; method?: string; line?: number }): Promise<void> {
  const top = p.class.split("$")[0];
  const rel = top.replace(/\./g, "/");
  const files = await vscode.workspace.findFiles(`**/${rel}.{java,kt,scala}`, "**/node_modules/**", 1);
  if (files.length === 0) {
    void vscode.window.showInformationMessage(`OpenProfiler: source for ${p.class} not found in workspace`);
    return;
  }
  const doc = await vscode.workspace.openTextDocument(files[0]);
  let line = (p.line ?? 1) - 1;
  if ((!p.line || p.line < 1) && p.method) {
    const idx = doc.getText().search(new RegExp(`\\b${p.method}\\s*\\(`));
    if (idx >= 0) line = doc.positionAt(idx).line;
  }
  const pos = new vscode.Position(Math.max(0, line), 0);
  await vscode.window.showTextDocument(doc, { selection: new vscode.Range(pos, pos) });
}

export function activate(ctx: vscode.ExtensionContext): void {
  status = vscode.window.createStatusBarItem(vscode.StatusBarAlignment.Left);
  status.text = "$(flame) OpenProfiler (offline)";
  status.command = "openprofiler.connect";
  status.show();
  ctx.subscriptions.push(
    status,
    vscode.commands.registerCommand("openprofiler.connect", connect),
    vscode.commands.registerCommand("openprofiler.refreshGutter", refreshAll),
    vscode.commands.registerCommand("openprofiler.launch", launchWithAgent),
    vscode.commands.registerCommand("openprofiler.copyAgentOption", async () => {
      const agent = (await agentLaunch()) ?? { path: "/path/to/libopenprofiler_agent.so", options: "tls" };
      await vscode.env.clipboard.writeText(agentOption(agent));
      void vscode.window.showInformationMessage(
        "Copied the OpenProfiler -agentpath option. Add it to vmArgs, or set \"openprofiler\": true in a Java launch configuration.",
      );
    }),
    vscode.commands.registerCommand("openprofiler.openFile", async () => {
      const pick = await vscode.window.showOpenDialog({ filters: { Profiles: ["jfr", "hprof", "opsnap", "oplink", "gz"] } });
      if (pick?.[0]) await call("openFile", { path: pick[0].fsPath });
    }),
    vscode.window.onDidChangeVisibleTextEditors(refreshAll),
    vscode.debug.registerDebugConfigurationProvider("java", javaLaunchProvider),
    vscode.debug.onDidStartDebugSession(onSessionStarted),
    vscode.debug.onDidTerminateDebugSession(onSessionEnded),
  );
  void connect();
}

export function deactivate(): void {
  clearInterval(liveRefresh);
  socket?.close();
  heat.forEach((t) => t.dispose());
}
