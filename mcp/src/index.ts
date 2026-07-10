#!/usr/bin/env node
// VZT Flow MCP server: gives Claude Code a `listen` voice-input tool (plus
// file transcription and history lookup) backed by the local VZT Flow
// dictation daemon, with a standalone CLI fallback when the desktop app
// isn't running.

import { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js";
import { StdioServerTransport } from "@modelcontextprotocol/sdk/server/stdio.js";
import { z } from "zod";

import { callDaemon, isDaemonAlive } from "./daemon.js";
import { extractTranscript, runFlowCli } from "./flow-cli.js";
import { formatHistory, readHistoryFile } from "./history.js";
import { listMeetingFiles, meetingsDir, readMeetingTranscript } from "./meeting.js";

const server = new McpServer({ name: "vzt-flow", version: "0.1.0" });

const NO_DAEMON_NO_CLI = (detail: string) =>
  `vzt-flow is not reachable: no daemon is running (start the VZT Flow desktop app) and the standalone ` +
  `\`flow\` CLI could not be run either (${detail}). Set VZT_FLOW_BIN to the \`flow\` binary path if it isn't ` +
  `on PATH.`;

server.registerTool(
  "listen",
  {
    title: "Listen (voice dictation)",
    description:
      "Record the user's voice from the microphone and return the transcribed, cleaned text. Use when the " +
      "user wants to dictate a prompt, answer, or any text by voice.",
    inputSchema: {
      mode: z
        .enum(["raw", "clean", "polish", "code"])
        .default("clean")
        .describe("Pipeline mode: raw (verbatim), clean (filler-word removal), polish (rewritten), code (identifier/symbol transform)."),
      max_seconds: z
        .number()
        .int()
        .positive()
        .max(600)
        .default(120)
        .describe("Hard cap on recording duration, in seconds (up to 600 = 10min, matching the app's max_hold_secs/max_handsfree_secs)."),
    },
  },
  async ({ mode, max_seconds }) => {
    if (await isDaemonAlive()) {
      try {
        const resp = await callDaemon({ cmd: "listen", mode, max_secs: max_seconds }, (max_seconds + 60) * 1000);
        if (!resp.ok) {
          return { content: [{ type: "text", text: `vzt-flow error: ${resp.error ?? "unknown"}` }], isError: true };
        }
        return { content: [{ type: "text", text: resp.text ?? "" }] };
      } catch (e) {
        return { content: [{ type: "text", text: `vzt-flow daemon error: ${(e as Error).message}` }], isError: true };
      }
    }

    try {
      const stdout = await runFlowCli(["listen", "--mode", mode, "--max-secs", String(max_seconds)], (max_seconds + 60) * 1000);
      return { content: [{ type: "text", text: stdout.trim() }] };
    } catch (e) {
      return { content: [{ type: "text", text: NO_DAEMON_NO_CLI((e as Error).message) }], isError: true };
    }
  },
);

server.registerTool(
  "transcribe_file",
  {
    title: "Transcribe audio file",
    description:
      "Transcribe an existing audio file (wav, or anything ffmpeg can read) through VZT Flow's dictionary " +
      "correction pass and return the text.",
    inputSchema: {
      path: z.string().describe("Absolute path to the audio file."),
    },
  },
  async ({ path: filePath }) => {
    if (await isDaemonAlive()) {
      try {
        const resp = await callDaemon({ cmd: "transcribe", path: filePath }, 60_000);
        if (!resp.ok) {
          return { content: [{ type: "text", text: `vzt-flow error: ${resp.error ?? "unknown"}` }], isError: true };
        }
        return { content: [{ type: "text", text: resp.text ?? "" }] };
      } catch (e) {
        return { content: [{ type: "text", text: `vzt-flow daemon error: ${(e as Error).message}` }], isError: true };
      }
    }

    try {
      const stdout = await runFlowCli(["transcribe", filePath], 120_000);
      return { content: [{ type: "text", text: extractTranscript(stdout) }] };
    } catch (e) {
      return { content: [{ type: "text", text: NO_DAEMON_NO_CLI((e as Error).message) }], isError: true };
    }
  },
);

server.registerTool(
  "dictation_history",
  {
    title: "Dictation history",
    description: "Show recent VZT Flow dictation history entries (timestamp, app, mode, and the pasted text).",
    inputSchema: {
      n: z.number().int().positive().default(10).describe("Number of most-recent entries to return."),
    },
  },
  async ({ n }) => {
    if (await isDaemonAlive()) {
      try {
        const resp = await callDaemon({ cmd: "history", n }, 5_000);
        if (resp.ok) {
          return { content: [{ type: "text", text: formatHistory(resp.history ?? []) }] };
        }
      } catch {
        // Fall through to the local file read below.
      }
    }
    return { content: [{ type: "text", text: formatHistory(readHistoryFile(n)) }] };
  },
);

server.registerTool(
  "meeting_transcript",
  {
    title: "Meeting transcript",
    description:
      "Return the text of a locally-recorded meeting transcript written by `flow meeting`. " +
      "Select by index (0 = latest, 1 = next most recent, ...) or by filename. Long transcripts " +
      "are truncated to a head + tail. Use to read, summarize, or extract action items from a call.",
    inputSchema: {
      meeting: z
        .union([z.number().int().nonnegative(), z.string()])
        .default(0)
        .describe("Meeting selector: a numeric index (0 = latest) or a transcript filename."),
    },
  },
  async ({ meeting }) => {
    const files = listMeetingFiles();
    if (files.length === 0) {
      return {
        content: [{ type: "text", text: `No meeting transcripts found in ${meetingsDir()}. Record one with \`flow meeting\`.` }],
      };
    }
    return { content: [{ type: "text", text: readMeetingTranscript(meeting) }] };
  },
);

async function main() {
  const transport = new StdioServerTransport();
  await server.connect(transport);
}

main().catch((e) => {
  console.error("[vzt-flow-mcp] fatal error:", e);
  process.exit(1);
});
