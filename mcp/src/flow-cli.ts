// Standalone (no-daemon) fallback: shells out to the `flow` CLI binary.

import { execFile } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";

/**
 * Resolves the `flow` binary. Order:
 *   1. `VZT_FLOW_BIN` env var (the documented name — every install script,
 *      README snippet, and release note tells users to set this one).
 *   2. `FLOW_BIN` — deprecated alias, kept so anyone already relying on the
 *      old var name doesn't silently break.
 *   3. The real locations `scripts/install.sh` installs `flow` to: prefers
 *      `/usr/local/bin` (already on PATH for most users), falling back to
 *      `~/.local/bin` when `/usr/local/bin` isn't writable. On Windows, the
 *      installer places `flow.exe` under
 *      `%LOCALAPPDATA%\Programs\vzt-flow\bin`.
 *   4. The dev-tree build output (`~/vzt-flow/target/release/flow`) — last
 *      resort before bare `flow`, kept only because this MCP server is
 *      developed alongside vzt-flow in-repo and a contributor may be running
 *      it against a local build that was never `install.sh`'d.
 *   5. Bare `flow`/`flow.exe` on PATH.
 */
export function resolveFlowBin(): string {
  if (process.env.VZT_FLOW_BIN) return process.env.VZT_FLOW_BIN;
  if (process.env.FLOW_BIN) return process.env.FLOW_BIN;

  const isWindows = process.platform === "win32";
  const exeName = isWindows ? "flow.exe" : "flow";

  const installedCandidates = isWindows
    ? [path.join(process.env.LOCALAPPDATA ?? path.join(os.homedir(), "AppData", "Local"), "Programs", "vzt-flow", "bin", "flow.exe")]
    : ["/usr/local/bin/flow", path.join(os.homedir(), ".local", "bin", "flow")];

  for (const candidate of installedCandidates) {
    if (fs.existsSync(candidate)) return candidate;
  }

  const knownBuild = path.join(os.homedir(), "vzt-flow", "target", "release", exeName);
  if (fs.existsSync(knownBuild)) return knownBuild;

  return exeName;
}

export function runFlowCli(args: string[], timeoutMs: number): Promise<string> {
  return new Promise((resolve, reject) => {
    execFile(
      resolveFlowBin(),
      args,
      { timeout: timeoutMs, maxBuffer: 16 * 1024 * 1024 },
      (err, stdout, stderr) => {
        if (err) {
          const msg = stderr?.toString().trim() || err.message;
          reject(new Error(msg));
          return;
        }
        resolve(stdout.toString());
      },
    );
  });
}

/**
 * Extracts the transcript body from `flow transcribe`'s human-readable
 * output (it prints diagnostics to stdout too, bracketed by
 * "--- Transcript ---" / "------------------" markers) — this is a fallback
 * path only; the daemon-first path gets clean JSON instead.
 */
export function extractTranscript(cliOutput: string): string {
  const match = cliOutput.match(/--- Transcript ---\n([\s\S]*?)\n-+\n/);
  return match ? match[1].trim() : cliOutput.trim();
}
