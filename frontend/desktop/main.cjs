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
const { spawn, execFileSync } = require("child_process");
const net = require("net");
const path = require("path");
const crypto = require("crypto");

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
  // Windows has no login-shell concept; `/bin/bash` cannot exist and the
  // execFileSync probe below would just burn ~5s in the timeout path.
  if (process.platform === "win32") return "";
  if (loginPathCache !== null) return loginPathCache;
  loginPathCache = "";
  const shells = [process.env.SHELL, "/bin/bash"].filter(Boolean);
  for (const sh of shells) {
    try {
      const out = execFileSync(sh, ["-ilc", 'printf %s "$PATH"'], {
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
// Portable (green) mode — Windows zero-install deployments.
//
// Layout produced by scripts/package-portable-electron.sh:
//   NonoClaw.exe                  (electron-builder win-unpacked root)
//   resources/app.asar            (SPA + this file)
//   resources/nonoclaw/bin/...    (backend via extraResources)
//   runtime/node|python|bin|git   (node, embeddable python, rg+markitdown
//                                  shim, MinGit, poppler)
//   templates/settings.json       (first-run seed)
//
// Data home resolution: portable builds are deliberately fail-closed. Every
// writable file lives under `.nonoclaw/` next to NonoClaw.exe; the app refuses
// to start rather than leaking configuration or credentials into the profile.
// ---------------------------------------------------------------------------
const PORTABLE_ROOT = (() => {
  if (process.platform !== "win32" || app.isPackaged === false) return null;
  const root = path.dirname(app.getPath("exe"));
  return fs.existsSync(path.join(root, "runtime", "node", "node.exe"))
    ? root
    : null;
})();

function resolvePortableHome() {
  if (!PORTABLE_ROOT) return null;
  const bundled = path.join(PORTABLE_ROOT, ".nonoclaw");
  try {
    fs.mkdirSync(bundled, { recursive: true });
    fs.accessSync(bundled, fs.constants.W_OK);
    return bundled;
  } catch (err) {
    throw new Error(`便携目录不可写：${bundled} (${err})`);
  }
}

// Env for the backend when running portable: NONOCLAW_HOME pinned into the
// package + bundled runtimes first on PATH (node/npm/npx, python, rg,
// markitdown shim, MinGit, poppler). Order matters — bundled wins over any
// domain-machine system installs.
function portableBackendEnv(home) {
  const rt = path.join(PORTABLE_ROOT, "runtime");
  const additions = [
    path.join(rt, "node"),
    path.join(rt, "python"),
    path.join(rt, "python", "Scripts"),
    path.join(rt, "bin"),
    path.join(rt, "git", "cmd"),
    path.join(rt, "git", "usr", "bin"),
    path.join(rt, "poppler", "Library", "bin"),
  ].filter((p) => fs.existsSync(p));
  return {
    ...process.env,
    NONOCLAW_HOME: home,
    // static_service.rs checks NONOCLAW_DATA_DIR/frontend/dist. The backend
    // executable lives one directory deeper under resources/nonoclaw/bin, so
    // relying on executable-relative discovery misses the packaged SPA.
    NONOCLAW_DATA_DIR: path.join(process.resourcesPath, "nonoclaw"),
    // Exposed for templates/diagnostics; NOT read by mcp.rs's expander —
    // the settings template's ${NONOCLAW_PORTABLE_ROOT} placeholders are
    // pre-expanded by ensurePortableFirstRun() at seed time.
    NONOCLAW_PORTABLE_ROOT: PORTABLE_ROOT,
    PATH: `${additions.join(";")};${process.env.PATH || ""}`,
  };
}

// First run: seed NONOCLAW_HOME with the bundled settings template when the
// machine has no existing config. Never overwrite user edits. The template's
// ${NONOCLAW_HOME} placeholders stay verbatim — the backend expands them at
// request time (mcp.rs + toolchain.rs), so the seeded settings.json keeps
// working when the whole package moves between drives (C:, D:, USB) or is
// renamed. Re-seeding only happens when the template itself changes.
function ensurePortableFirstRun(home) {
  if (!PORTABLE_ROOT) return;
  const target = path.join(home, "settings.json");
  const template = path.join(PORTABLE_ROOT, "templates", "settings.json");
  if (!fs.existsSync(template)) return;
  try {
    fs.mkdirSync(home, { recursive: true });
    const raw = fs.readFileSync(template, "utf8");
    const templateHash = crypto.createHash("sha256").update(raw).digest("hex");
    const marker = path.join(home, ".settings-seed");
    const knownHash = fs.existsSync(marker)
      ? fs.readFileSync(marker, "utf8").trim()
      : null;
    if (fs.existsSync(target) && knownHash === templateHash) return;
    if (fs.existsSync(target)) {
      fs.copyFileSync(target, `${target}.previous`);
      console.log("[desktop] settings template changed — re-seeding (old copy at settings.json.previous)");
    }
    fs.writeFileSync(target, raw);
    fs.writeFileSync(marker, templateHash);
    console.log(`[desktop] portable first-run: seeded ${target}`);
  } catch (err) {
    console.warn(`[desktop] portable first-run seed failed: ${err}`);
  }
}

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
let lastBackendExit = null;
let shuttingDown = false;

function startServer(port) {
  lastBackendExit = null;
  const backendErrChunks = [];
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
  let cwd = PORTABLE_ROOT
    ? path.join(PORTABLE_ROOT, "workspace")
    : app.isPackaged
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
  // Portable mode: NONOCLAW_HOME and the default project both live inside
  // the package; bundled runtimes take precedence over machine installations.
  let env = backendEnv();
  if (PORTABLE_ROOT) {
    const home = resolvePortableHome();
    fs.mkdirSync(cwd, { recursive: true });
    // Seed only on first run, or when a NEW package (different template
    // hash) is unpacked over the old folder. User edits between releases
    // are preserved; a stale seed from an older broken build is replaced
    // exactly once, with the previous file kept for manual recovery.
    ensurePortableFirstRun(home);
    env = portableBackendEnv(home);
    console.log(`[desktop] portable mode: NONOCLAW_HOME=${home}`);
    const seeded = fs.existsSync(path.join(home, "settings.json"));
    console.log(`[desktop] settings.json seeded=${seeded}`);
  }
  serverProc = spawn(bin, ["--serve-http", `127.0.0.1:${port}`], {
    cwd,
    env,
    stdio: ["ignore", "pipe", "pipe"],
  });

  const forward = (buf) => process.stderr.write(`[nonoclaw] ${buf}`);
  serverProc.stdout.on("data", forward);
  serverProc.stderr.on("data", (buf) => {
    forward(buf);
    backendErrChunks.push(buf);
  });
  serverProc.on("exit", (code, signal) => {
    console.log(`[desktop] nonoclaw exited code=${code} signal=${signal}`);
    lastBackendExit = { code, signal, stderr: backendErrChunks.join("").slice(-4000) };
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
  shuttingDown = true;
  if (!serverProc) return;
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
          const detail = lastBackendExit
            ? `\n后端退出码：${lastBackendExit.code}${lastBackendExit.signal ? ` (${lastBackendExit.signal})` : ""}\n${lastBackendExit.stderr.trim().slice(-1500)}`
            : "\n后端进程仍在运行；请检查端口或安全软件拦截。";
          reject(new Error(`server did not come up within ${timeoutMs}ms${detail}`));
        } else {
          setTimeout(attempt, 250);
        }
      });
      sock.once("timeout", () => {
        sock.destroy();
        if (Date.now() > deadline) {
          const detail = lastBackendExit
            ? `\n后端退出码：${lastBackendExit.code}${lastBackendExit.signal ? ` (${lastBackendExit.signal})` : ""}\n${lastBackendExit.stderr.trim().slice(-1500)}`
            : "\n后端进程仍在运行；请检查端口或安全软件拦截。";
          reject(new Error(`server did not come up within ${timeoutMs}ms${detail}`));
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
function allowedExternalUrl(target) {
  try {
    const protocol = new URL(target).protocol;
    return protocol === "https:" || protocol === "mailto:";
  } catch {
    return false;
  }
}

function openAllowedExternal(target) {
  if (!allowedExternalUrl(target)) {
    console.warn("[desktop] blocked external URL with an untrusted scheme");
    return;
  }
  void shell.openExternal(target).catch(() => {
    console.warn("[desktop] failed to open external URL");
  });
}

function createWindow(url) {
  const win = new BrowserWindow({
    width: 1440,
    height: 900,
    minWidth: 960,
    minHeight: 600,
    backgroundColor: "#f5f5f7",
    autoHideMenuBar: true,
    show: false,
    webPreferences: {
      preload: path.join(__dirname, "preload.cjs"),
      contextIsolation: true,
      nodeIntegration: false,
      sandbox: true,
    },
  });

  // External links (docs, QR help, etc.) open in the system browser,
  // never hijack the app window. Only explicitly safe schemes are allowed.
  win.webContents.setWindowOpenHandler(({ url: target }) => {
    openAllowedExternal(target);
    return { action: "deny" };
  });

  const appOrigin = new URL(url).origin;
  win.webContents.on("will-navigate", (event, target) => {
    try {
      if (new URL(target).origin === appOrigin) return;
    } catch {
      // Malformed targets are blocked below.
    }
    event.preventDefault();
    openAllowedExternal(target);
  });

  let loadFinished = false;
  const showWindow = () => {
    if (!win.isDestroyed() && !win.isVisible()) win.show();
  };
  win.once("ready-to-show", showWindow);
  // Never leave an invisible or black window if Chromium does not emit
  // ready-to-show on an unusual graphics stack.
  setTimeout(showWindow, 5000).unref();

  win.webContents.on("did-finish-load", () => {
    loadFinished = true;
    showWindow();
  });
  win.webContents.on("did-fail-load", (_event, code, description, failedUrl, isMainFrame) => {
    if (!isMainFrame) return;
    const message = `页面加载失败 (${code})：${description}\n${failedUrl}`;
    console.error(`[desktop] ${message}`);
    const { dialog } = require("electron");
    dialog.showErrorBox("NonoClaw 启动失败", message);
    showWindow();
  });
  win.webContents.on("render-process-gone", (_event, details) => {
    const message = `页面渲染进程异常退出：${details.reason} (${details.exitCode})`;
    console.error(`[desktop] ${message}`);
    const { dialog } = require("electron");
    dialog.showErrorBox("NonoClaw 渲染失败", message);
  });

  void win.loadURL(url).catch((err) => {
    console.error(`[desktop] loadURL failed: ${err}`);
    showWindow();
  });
  setTimeout(() => {
    if (!loadFinished && !win.isDestroyed()) {
      console.error("[desktop] page did not finish loading within 20 seconds");
      win.webContents.reloadIgnoringCache();
    }
  }, 20_000).unref();
  return win;
}

// ---------------------------------------------------------------------------
// App lifecycle
// ---------------------------------------------------------------------------
// Chromium GPU compositing is a frequent black-window failure mode on
// domain-managed Windows 11 machines, Remote Desktop, VMs, and older drivers.
// Portable mode favors reliable software rendering over GPU acceleration.
if (PORTABLE_ROOT) {
  app.disableHardwareAcceleration();
  app.commandLine.appendSwitch("disable-gpu-compositing");
}

const gotLock = app.requestSingleInstanceLock();
if (!gotLock) {
  app.quit();
} else {
  // Portable: keep Chromium profile data inside the package too. Failure is
  // fatal so the package never writes to %APPDATA%. Must run before
  // `app.whenReady()` — userData is consumed during readiness.
  if (PORTABLE_ROOT) {
    try {
      const userDataDir = path.join(PORTABLE_ROOT, ".electron-data");
      fs.mkdirSync(userDataDir, { recursive: true });
      fs.accessSync(userDataDir, fs.constants.W_OK);
      app.setPath("userData", userDataDir);
    } catch (err) {
      console.error(`[desktop] portable data directory is not writable: ${err}`);
      app.quit();
    }
  }
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
