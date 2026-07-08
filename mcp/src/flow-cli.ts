// Standalone (no-daemon) fallback: shells out to the `flow` CLI binary.

import { execFile } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";

/**
 * Resolves the `flow` binary: an explicit `FLOW_BIN` env var wins, then the
 * known local build output for this repo (this MCP server ships alongside
 * vzt-flow, not as a standalone package), then bare `flow` on PATH.
 */
function resolveFlowBin(): string {
  if (process.env.FLOW_BIN) return process.env.FLOW_BIN;
  const knownBuild = path.join(os.homedir(), "vzt-flow", "target", "release", "flow");
  if (fs.existsSync(knownBuild)) return knownBuild;
  return "flow";
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
