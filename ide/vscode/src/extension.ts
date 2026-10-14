// Ember VS Code extension: a thin client of the daemon's IDE bridge (docs/spec/query-api.md, /api/v1/ide).
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

let socket: WebSocket | undefined;
let nextId = 1;
const pending = new Map<number, (v: unknown, err?: string) => void>();
// Four heat levels; VS Code decorations can't vary background per instance.
const HEAT_LEVELS = [0.08, 0.16, 0.28, 0.42];
let heat: vscode.TextEditorDecorationType[] = [];
let status: vscode.StatusBarItem;

function discovery(): Discovery | undefined {
  const cfg = vscode.workspace.getConfiguration("ember");
  const url = cfg.get<string>("daemonUrl");
  const token = cfg.get<string>("token");
  if (url && token) return { url, token };
  try {
    const file = path.join(os.homedir(), ".ember", "daemon.json");
    return JSON.parse(fs.readFileSync(file, "utf8")) as Discovery;
  } catch {
    return undefined;
  }
}

function call<T>(method: string, params: object): Promise<T> {
  return new Promise((resolve, reject) => {
    if (!socket || socket.readyState !== WebSocket.OPEN) {
      reject(new Error("not connected to the Ember daemon"));
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
    void vscode.window.showWarningMessage("Ember daemon not found. Start `ember daemon` or set ember.daemonUrl/ember.token.");
    return;
  }
  socket?.close();
  const wsUrl = d.url.replace(/^http/, "ws") + `/api/v1/ide?token=${encodeURIComponent(d.token)}`;
  const ws = new WebSocket(wsUrl);
  socket = ws;
  ws.onopen = () => {
    status.text = "$(flame) Ember";
    status.tooltip = `Connected to ${d.url}`;
    void call("hello", { ide: "vscode", version: vscode.version }).then(() => refreshAll());
  };
  ws.onclose = () => {
    status.text = "$(flame) Ember (offline)";
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
  const min = vscode.workspace.getConfiguration("ember").get<number>("minPercent") ?? 0.5;
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
        `**Ember CPU** \`${l.method}\` line ${l.line}: total ${l.total_percent}% · self ${l.self_percent}%`,
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

async function navigate(p: { class: string; method?: string; line?: number }): Promise<void> {
  const top = p.class.split("$")[0];
  const rel = top.replace(/\./g, "/");
  const files = await vscode.workspace.findFiles(`**/${rel}.{java,kt,scala}`, "**/node_modules/**", 1);
  if (files.length === 0) {
    void vscode.window.showInformationMessage(`Ember: source for ${p.class} not found in workspace`);
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
  status.text = "$(flame) Ember (offline)";
  status.command = "ember.connect";
  status.show();
  ctx.subscriptions.push(
    status,
    vscode.commands.registerCommand("ember.connect", connect),
    vscode.commands.registerCommand("ember.refreshGutter", refreshAll),
    vscode.commands.registerCommand("ember.copyAgentOption", async () => {
      const agent = vscode.workspace.getConfiguration("ember").get<string>("agentPath") || "/path/to/libember_agent.so";
      await vscode.env.clipboard.writeText(`-agentpath:${agent}=port=0`);
      void vscode.window.showInformationMessage("Copied the Ember -agentpath option. Add it to your launch configuration's vmArgs.");
    }),
    vscode.commands.registerCommand("ember.openFile", async () => {
      const pick = await vscode.window.showOpenDialog({ filters: { Profiles: ["jfr", "hprof", "ember", "gz"] } });
      if (pick?.[0]) await call("openFile", { path: pick[0].fsPath });
    }),
    vscode.window.onDidChangeVisibleTextEditors(refreshAll),
  );
  void connect();
}

export function deactivate(): void {
  socket?.close();
  heat.forEach((t) => t.dispose());
}
