// NonoClaw Desktop — Electron main process.
//
// Responsibilities:
//   1. Spawn `nonoclaw --serve-http 127.0.0.1:<port>` as a child process
//      (reused across window reloads; killed on app quit).
//   2. Wait for the HTTP server to accept connections, then open a
//      BrowserWindow pointed at it.
//
// The frontend derives its WebSocket URL from window.location.host, so
// loading the server URL directly means zero frontend changes.
//
// CLI options (Electron passes unknown flags through process.argv):
//   --port <N>        preferred server port (default 8799; auto-increments
//                     up to +20 if occupied)
//   --project <dir>   project directory to open (resolved to absolute;
//                     becomes the backend's cwd, so .nonoclaw/ config,
//                     sessions, and tools apply to that project)
//   <dir> positional  same as --project (e.g. `nonoclaw-desktop ~/my-wiki`)

const { app, BrowserWindow, shell } = require("electron");
const { spawn, execSync } = require("child_process");
const net = require("net");
const path = require("path");
const fs = require("fs");

const DEFAULT_PORT = 8799;
const STARTUP_TIMEOUT_MS = 60_000;

// ---------------------------------------------------------------------------
// Login-shell PATH enrichment
// ---------------------------------------------------------------------------
// When launched from the desktop GUI, Electron inherits a minimal PATH that
// omits user-installed tools (nvm's node/npx, cargo, ~/.local/bin). MCP
// servers spawned via bare `npx`/`node` then fail to start. Ask the user's
// login shell once for its full PATH and merge it in.
let loginPathCache = null;
function loginShellPath() {
  if (loginPathCache !== null) return loginPathCache;
  loginPathCache = "";
  const shells = [process.env.SHELL, "/bin/bash"].filter(Boolean);
  for (const sh of shells) {
    try {
      const out = execSync(`"${sh}" -ilc 'printf %s "$PATH"'`, {
        encoding: "utf8",
        timeout: 5000,
        stdio: ["ignore", "pipe", "ignore"],
      }).trim();
      if (out) {
        loginPathCache = out;
        break;
      }
    } catch {
      // shell unusable or slow — fall through to next candidate
    }
  }
  return loginPathCache;
}

// Merged env for the backend: existing PATH first, login-shell additions
// appended (so packaged binaries keep priority). Also ensure HOME is set —
// GUI-launched apps on some platforms lack it, breaking ~/.nonoclaw lookup.
function backendEnv() {
  const env = { ...process.env };
  const login = loginShellPath();
  if (login && login !== process.env.PATH) {
    env.PATH = `${process.env.PATH || ""}:${login}`.replace(/(^|:)+/, "$1");
  }
  if (!env.HOME) env.HOME = app.getPath("home");
  return env;
}

// ---------------------------------------------------------------------------
// CLI parsing (Electron's own flags are skipped: -- and everything before it)
// ---------------------------------------------------------------------------
function parseCliArgs(argv) {
  // process.argv: [electron, main.cjs, --ours..., --chromium-flags...]
  // Chromium flags start at the first one Electron doesn't recognize as ours;
  // safest split: only take args until we hit an unknown flag we don't own.
  // process.argv layout differs between dev and packaged:
  //   dev:      [electron, /path/main.cjs, --port, 8811, ...]
  //   packaged: [nonoclaw-frontend, --port, 8811, ...]
  // Drop everything up to and including the main script (any arg ending in
  // main.cjs), or just the exe if no script path is present.
  const opts = { port: DEFAULT_PORT, project: null };
  const args = argv.slice(1);
  const scriptIdx = args.findIndex((a) => a.endsWith("main.cjs"));
  const rest = scriptIdx >= 0 ? args.slice(scriptIdx + 1) : args;
  for (let i = 0; i < rest.length; i++) {
    const arg = rest[i];
    if (arg === "--port") {
      const value = rest[i + 1];
      const parsed = Number.parseInt(value, 10);
      if (Number.isInteger(parsed) && parsed > 0 && parsed < 65536) {
        opts.port = parsed;
        i++;
      } else {
        console.error(`[desktop] invalid --port value: ${value}, using default ${DEFAULT_PORT}`);
      }
    } else if (arg === "--project") {
      const value = rest[i + 1];
      if (value && !value.startsWith("-")) {
        opts.project = value;
        i++;
      }
    } else if (arg.startsWith("--port=")) {
      const parsed = Number.parseInt(arg.slice("--port=".length), 10);
      if (Number.isInteger(parsed) && parsed > 0 && parsed < 65536) opts.port = parsed;
    } else if (arg.startsWith("--project=")) {
      const value = arg.slice("--project=".length);
      if (value) opts.project = value;
    } else if (!arg.startsWith("-")) {
      // positional argument = project directory (e.g. `nonoclaw-desktop ~/wiki`)
      opts.project = arg;
    } else {
      // Chromium flag — ignore
    }
  }
  return opts;
}

const CLI = parseCliArgs(process.argv);

// ---------------------------------------------------------------------------
// nonoclaw binary resolution (first match wins)
// ---------------------------------------------------------------------------
function resolveNonoclawBin() {
  const candidates = [];

  if (process.env.NONOCLAW_BIN) {
    candidates.push(process.env.NONOCLAW_BIN);
  }

  // Packaged layout: resources/nonoclaw/bin/nonoclaw (extraResources).
  if (app.isPackaged) {
    candidates.push(
      path.join(process.resourcesPath, "nonoclaw", "bin", "nonoclaw"),
    );
  }

  // Dev layout: repo checkout — prefer release build, then debug build.
  const repoRoot = path.join(__dirname, "..", "..");
  candidates.push(
    path.join(repoRoot, "rust", "target", "release", "nonoclaw"),
    path.join(repoRoot, "rust", "target", "debug", "nonoclaw"),
  );

  // Windows binaries carry a .exe suffix — mirror every concrete candidate.
  if (process.platform === "win32") {
    const exeCandidates = [];
    for (const c of candidates) {
      if (c === "nonoclaw") continue;
      exeCandidates.push(c.endsWith(".exe") ? c : `${c}.exe`);
    }
    candidates.push(...exeCandidates);
  }

  // Last resort: whatever is on PATH.
  candidates.push("nonoclaw");

  for (const candidate of candidates) {
    try {
      fs.accessSync(candidate, fs.constants.X_OK);
      return candidate;
    } catch {
      // keep looking
    }
  }
  return "nonoclaw";
}

// ---------------------------------------------------------------------------
// Server lifecycle
// ---------------------------------------------------------------------------
let serverProc = null;
let shuttingDown = false;

function startServer(port) {
  const bin = resolveNonoclawBin();
  // static_service.rs resolves the SPA from cwd/frontend/dist first, so run
  // from the repo root in dev; when packaged we ship dist next to the binary
  // and set cwd to its parent. --project overrides the cwd so the backend
  // loads that project's .nonoclaw/ config, sessions, and git context.
  // Packaged default working directory (user's project context, NOT the
  // install dir — .nonoclaw/ sessions/config/git context live here):
  //   Linux:   ~/NonoClaw (created on first launch)
  //   Windows: user home directory
  // Explicit --project always overrides. The frontend SPA itself is resolved
  // by static_service.rs via exe-dir/data-dir candidates, not cwd.
  function defaultProjectDir() {
    const home = app.getPath("home");
    if (process.platform === "win32") return home;
    const dir = path.join(home, "NonoClaw");
    try {
      fs.mkdirSync(dir, { recursive: true });
    } catch {
      return home; // unwritable home — fall back rather than fail launch
    }
    return dir;
  }
  let cwd = app.isPackaged
    ? defaultProjectDir()
    : path.join(__dirname, "..", "..");
  if (CLI.project) {
    const resolved = path.resolve(CLI.project);
    try {
      const stat = fs.statSync(resolved);
      if (!stat.isDirectory()) throw new Error("not a directory");
      cwd = resolved;
    } catch {
      const { dialog } = require("electron");
      dialog.showErrorBox("NonoClaw", `--project 目录无效：${resolved}`);
      app.quit();
      throw new Error(`invalid --project: ${resolved}`);
    }
  }

  console.log(`[desktop] spawning: ${bin} --serve-http 127.0.0.1:${port} (cwd=${cwd})`);
  serverProc = spawn(bin, ["--serve-http", `127.0.0.1:${port}`], {
    cwd,
    env: backendEnv(),
    stdio: ["ignore", "pipe", "pipe"],
  });

  const forward = (buf) => process.stderr.write(`[nonoclaw] ${buf}`);
  serverProc.stdout.on("data", forward);
  serverProc.stderr.on("data", forward);
  serverProc.on("exit", (code, signal) => {
    console.log(`[desktop] nonoclaw exited code=${code} signal=${signal}`);
    serverProc = null;
    // If the backend dies unexpectedly, close windows so the user notices
    // instead of staring at a dead page. Skip during intentional shutdown.
    if (!shuttingDown && !app.isQuittingForServerExit) {
      for (const win of BrowserWindow.getAllWindows()) win.close();
    }
  });

  return serverProc;
}

function killServer() {
  if (!serverProc) return;
  shuttingDown = true;
  try {
    // SIGTERM lets tokio shut down gracefully (flush session writer actor).
    serverProc.kill("SIGTERM");
  } catch {
    // already gone
  }
  // Hard fallback if it ignores SIGTERM for >3s.
  const proc = serverProc;
  setTimeout(() => {
    try {
      if (proc && proc.exitCode === null) proc.kill("SIGKILL");
    } catch {
      // already gone
    }
  }, 3000).unref();
}

// Poll until the port accepts TCP connections.
function waitForServer(port, timeoutMs) {
  const deadline = Date.now() + timeoutMs;
  return new Promise((resolve, reject) => {
    const attempt = () => {
      const sock = net.connect({ host: "127.0.0.1", port, timeout: 1000 });
      sock.once("connect", () => {
        sock.destroy();
        resolve();
      });
      sock.once("error", () => {
        sock.destroy();
        if (Date.now() > deadline) {
          reject(new Error(`server did not come up within ${timeoutMs}ms`));
        } else {
          setTimeout(attempt, 250);
        }
      });
      sock.once("timeout", () => {
        sock.destroy();
        if (Date.now() > deadline) {
          reject(new Error(`server did not come up within ${timeoutMs}ms`));
        } else {
          setTimeout(attempt, 250);
        }
      });
    };
    attempt();
  });
}

async function pickPort(start) {
  for (let p = start; p < start + 20; p++) {
    const free = await new Promise((resolve) => {
      const probe = net.createServer();
      probe.once("error", () => resolve(false));
      probe.once("listening", () => probe.close(() => resolve(true)));
      probe.listen(p, "127.0.0.1");
    });
    if (free) return p;
  }
  throw new Error(`no free port found in [${start}, ${start + 20})`);
}

// ---------------------------------------------------------------------------
// Window
// ---------------------------------------------------------------------------
function createWindow(url) {
  const win = new BrowserWindow({
    width: 1440,
    height: 900,
    minWidth: 960,
    minHeight: 600,
    backgroundColor: "#101418",
    autoHideMenuBar: true,
    webPreferences: {
      preload: path.join(__dirname, "preload.cjs"),
      contextIsolation: true,
      nodeIntegration: false,
      sandbox: true,
    },
  });

  // External links (docs, QR help, etc.) open in the system browser,
  // never hijack the app window.
  win.webContents.setWindowOpenHandler(({ url: target }) => {
    shell.openExternal(target);
    return { action: "deny" };
  });

  win.loadURL(url);
  return win;
}

// ---------------------------------------------------------------------------
// App lifecycle
// ---------------------------------------------------------------------------
const gotLock = app.requestSingleInstanceLock();
if (!gotLock) {
  app.quit();
} else {
  app.on("second-instance", () => {
    const [win] = BrowserWindow.getAllWindows();
    if (win) {
      if (win.isMinimized()) win.restore();
      win.focus();
    }
  });

  app.whenReady().then(async () => {
    let port;
    try {
      port = await pickPort(CLI.port);
    } catch (err) {
      const { dialog } = require("electron");
      dialog.showErrorBox("NonoClaw", String(err));
      app.quit();
      return;
    }

    startServer(port);

    try {
      await waitForServer(port, STARTUP_TIMEOUT_MS);
    } catch (err) {
      const { dialog } = require("electron");
      killServer();
      dialog.showErrorBox(
        "NonoClaw",
        `后端服务启动失败：\n${err}\n\n可用环境变量 NONOCLAW_BIN 指定 nonoclaw 二进制路径。`,
      );
      app.quit();
      return;
    }

    createWindow(`http://127.0.0.1:${port}`);

    app.on("activate", () => {
      if (BrowserWindow.getAllWindows().length === 0) {
        createWindow(`http://127.0.0.1:${port}`);
      }
    });
  });

  app.on("window-all-closed", () => {
    // macOS convention: keep running until Cmd+Q.
    if (process.platform !== "darwin") app.quit();
  });

  app.on("before-quit", killServer);
}
