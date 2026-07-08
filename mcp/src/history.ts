// Local fallback for `dictation_history` when no daemon is reachable —
// reads ~/.config/vzt-flow/history.jsonl directly, same file the daemon
// and CLI both read.

import fs from "node:fs";
import os from "node:os";
import path from "node:path";

export interface HistoryEntry {
  ts: number;
  app: string | null;
  raw_text: string;
  duration_s: number;
  rtf: number;
  clean_text: string;
  mode: string;
}

function historyPath(): string {
  return path.join(os.homedir(), ".config", "vzt-flow", "history.jsonl");
}

export function readHistoryFile(n: number): HistoryEntry[] {
  const p = historyPath();
  if (!fs.existsSync(p)) return [];
  const lines = fs.readFileSync(p, "utf8").split("\n").filter((l) => l.trim().length > 0);
  const entries: HistoryEntry[] = [];
  for (const line of lines) {
    try {
      entries.push(JSON.parse(line) as HistoryEntry);
    } catch {
      // Skip malformed lines, same tolerance as the Rust reader.
    }
  }
  entries.reverse();
  return entries.slice(0, n);
}

export function formatHistory(entries: HistoryEntry[]): string {
  if (entries.length === 0) return "(no dictation history yet)";
  return entries
    .map((e) => {
      const app = e.app ?? "unknown";
      const when = new Date(e.ts * 1000).toISOString();
      return `[${when}] ${e.duration_s.toFixed(1)}s | mode=${e.mode} | app=${app}\n  ${e.clean_text.trim()}`;
    })
    .join("\n\n");
}
