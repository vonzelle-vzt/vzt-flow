// Local reader for meeting transcripts written by `flow meeting`. Reads the
// same ~/Documents/vzt-flow/meetings/ directory the CLI writes to (override
// with FLOW_MEETINGS_DIR), independent of the dictation daemon.

import fs from "node:fs";
import os from "node:os";
import path from "node:path";

/** Directory `flow meeting` writes transcripts to. */
export function meetingsDir(): string {
  if (process.env.FLOW_MEETINGS_DIR) return process.env.FLOW_MEETINGS_DIR;
  return path.join(os.homedir(), "Documents", "vzt-flow", "meetings");
}

/** Meeting transcript files (`*.md`), newest first by modified time. */
export function listMeetingFiles(): string[] {
  const dir = meetingsDir();
  if (!fs.existsSync(dir)) return [];
  return fs
    .readdirSync(dir)
    .filter((f) => f.toLowerCase().endsWith(".md"))
    .map((f) => path.join(dir, f))
    .map((p) => ({ p, mtime: safeMtime(p) }))
    .sort((a, b) => b.mtime - a.mtime)
    .map((e) => e.p);
}

function safeMtime(p: string): number {
  try {
    return fs.statSync(p).mtimeMs;
  } catch {
    return 0;
  }
}

const MAX_CHARS = 50_000;

/**
 * Resolves a meeting selector to its transcript text. `selector` is either a
 * numeric index (0 = latest, 1 = next-most-recent, ...) or a filename (with or
 * without the `.md` extension, or an absolute path). Text longer than 50k
 * chars is truncated to a head + tail with a marker in between.
 */
export function readMeetingTranscript(selector: number | string): string {
  const files = listMeetingFiles();

  let target: string | undefined;
  if (typeof selector === "number") {
    target = files[selector];
    if (!target) {
      return `No meeting at index ${selector} (found ${files.length} transcript${files.length === 1 ? "" : "s"} in ${meetingsDir()}).`;
    }
  } else {
    // Filename, filename without extension, or absolute path.
    if (path.isAbsolute(selector) && fs.existsSync(selector)) {
      target = selector;
    } else {
      const wanted = selector.toLowerCase().replace(/\.md$/, "");
      target = files.find((f) => path.basename(f).toLowerCase().replace(/\.md$/, "") === wanted);
    }
    if (!target) {
      return `No meeting matching "${selector}" in ${meetingsDir()}.`;
    }
  }

  let text: string;
  try {
    text = fs.readFileSync(target, "utf8");
  } catch (e) {
    return `Failed to read ${target}: ${(e as Error).message}`;
  }

  if (text.length > MAX_CHARS) {
    const half = Math.floor(MAX_CHARS / 2);
    const head = text.slice(0, half);
    const tail = text.slice(text.length - half);
    const omitted = text.length - MAX_CHARS;
    return `${head}\n\n... [truncated ${omitted} characters] ...\n\n${tail}`;
  }
  return text;
}
