// Client for VZT Flow's daemon control socket — a Unix domain socket at
// ~/.config/vzt-flow/daemon.sock speaking newline-delimited JSON request/
// response frames (see crates/flow-core/src/ipc.rs for the Rust side of
// this protocol; this is a from-scratch Node implementation of the same
// wire format, not a shared library).

import net from "node:net";
import os from "node:os";
import path from "node:path";

export function socketPath(): string {
  return path.join(os.homedir(), ".config", "vzt-flow", "daemon.sock");
}

export interface DaemonResponse {
  ok: boolean;
  error?: string;
  state?: string;
  model_loaded?: boolean;
  cleanup_loaded?: boolean;
  version?: string;
  raw?: string;
  text?: string;
  mode?: string;
  duration_s?: number;
  history?: Array<{
    ts: number;
    app: string | null;
    raw_text: string;
    duration_s: number;
    rtf: number;
    clean_text: string;
    mode: string;
  }>;
}

/** Quick connect test — resolves true if a daemon is currently listening. */
export function isDaemonAlive(timeoutMs = 1000): Promise<boolean> {
  return new Promise((resolve) => {
    let settled = false;
    const finish = (v: boolean) => {
      if (settled) return;
      settled = true;
      clearTimeout(timer);
      sock.destroy();
      resolve(v);
    };
    const sock = net.createConnection({ path: socketPath() });
    const timer = setTimeout(() => finish(false), timeoutMs);
    sock.once("connect", () => finish(true));
    sock.once("error", () => finish(false));
  });
}

/**
 * Sends one request and waits for one newline-terminated JSON response.
 * `timeoutMs` should be generous for `listen` (recording + pipeline can
 * take up to the requested max duration plus processing time).
 */
export function callDaemon(req: Record<string, unknown>, timeoutMs: number): Promise<DaemonResponse> {
  return new Promise((resolve, reject) => {
    const sock = net.createConnection({ path: socketPath() });
    let buf = "";
    let settled = false;

    const finish = (fn: () => void) => {
      if (settled) return;
      settled = true;
      clearTimeout(timer);
      fn();
    };

    const timer = setTimeout(() => {
      finish(() => {
        sock.destroy();
        reject(new Error(`daemon request timed out after ${timeoutMs}ms`));
      });
    }, timeoutMs);

    sock.once("connect", () => {
      sock.write(JSON.stringify(req) + "\n");
    });

    sock.on("data", (chunk) => {
      buf += chunk.toString("utf8");
      const idx = buf.indexOf("\n");
      if (idx === -1) return;
      const line = buf.slice(0, idx);
      finish(() => {
        sock.end();
        try {
          resolve(JSON.parse(line) as DaemonResponse);
        } catch (e) {
          reject(new Error(`failed to parse daemon response: ${(e as Error).message}`));
        }
      });
    });

    sock.on("error", (e) => {
      finish(() => reject(e));
    });

    sock.on("close", () => {
      finish(() => reject(new Error("daemon closed the connection without a response")));
    });
  });
}
