// Bundles the CLI and the MCP server into self-contained files, so `npx claudecord` needs no workspace packages.
import { build } from "esbuild";
import { chmodSync, copyFileSync, rmSync } from "node:fs";

rmSync("dist", { recursive: true, force: true });

const common = {
  bundle: true,
  platform: "node",
  format: "esm",
  target: "node22",
  outdir: "dist",
  // CommonJS dependencies such as ws call require() on node builtins.
  banner: { js: "import { createRequire as __cr } from 'node:module'; const require = __cr(import.meta.url);" },
  // Optional native accelerators for ws. It works without them.
  external: ["bufferutil", "utf-8-validate"],
  logLevel: "info",
};

await build({ ...common, entryPoints: { cli: "src/cli.ts" } });
await build({ ...common, entryPoints: { mcp: "../agent-tools/src/mcp.ts" } });
chmodSync("dist/cli.js", 0o755);
copyFileSync("../../README.md", "README.md");
