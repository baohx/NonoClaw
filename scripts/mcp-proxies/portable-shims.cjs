// Entry shims for pre-installed MCP servers in the Windows portable package.
//
// Invoked by scripts/package-portable-electron.sh with MCPROOT pointing at
// <pkg>/runtime/node-mcp. Each shim uses __dirname-relative requires so the
// package works wherever it is unpacked (no absolute build-time paths).
//
// The playwright shim additionally forces loopback-only HTTP binding:
// playwright-mcp unconditionally listen()s a UI web server (port 5174) on the
// wildcard address at startup. On domain machines that triggers the Windows
// Firewall elevation prompt (node.exe wants to listen) — unanswerable without
// an admin password. Loopback binding is exempt from the prompt, so we patch
// http.Server.prototype.listen to rewrite any unspecified host to 127.0.0.1.
const fs = require("fs");
const path = require("path");

const root = process.env.MCPROOT;
const packages = {
  context7: ["@upstash/context7-mcp", "dist/index.js"],
  github: ["@modelcontextprotocol/server-github", "dist/index.js"],
  playwright: ["playwright-mcp", "dist/server.js"],
  "lark-cli": ["@larksuite/cli", "scripts/run.js"],
};

const loopbackPreamble = `\
// Portable shim: force loopback-only HTTP binding (see generator comment).
const http = require("http");
const originalListen = http.Server.prototype.listen;
http.Server.prototype.listen = function (...args) {
  if (typeof args[0] === "object" && args[0] !== null && !("host" in args[0])) {
    args[0] = { ...args[0], host: "127.0.0.1" };
  } else if (typeof args[0] === "number" && typeof args[1] === "function") {
    // listen(port, callback) — insert the host before the callback.
    args.splice(1, 0, "127.0.0.1");
  }
  return originalListen.apply(this, args);
};
`;

for (const [name, [pkg, entry]] of Object.entries(packages)) {
  const target = path.join(root, name, "node_modules", pkg, entry);
  if (!fs.existsSync(target)) throw new Error(`entry missing: ${target}`);
  const relative = "./" + path.relative(root, target).replaceAll(path.sep, "/");
  const body = name === "playwright"
    ? loopbackPreamble + `\nrequire(${JSON.stringify(relative)});\n`
    : `require(${JSON.stringify(relative)});\n`;
  fs.writeFileSync(path.join(root, name + ".js"), body);
  console.log(`  shim: ${name}.js -> ${relative}${name === "playwright" ? " (loopback listen patch)" : ""}`);
}
