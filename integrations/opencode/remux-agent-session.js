// managed by remux; REMUX_INTEGRATION_ID=opencode-resume-server REMUX_INTEGRATION_VERSION=1
//
// Tells remux which OpenCode 1.x session `opencode run` or `opencode --mini`
// is running in its pane, so that `remux restart` can resume it there. Installed
// by `remux integrate opencode-resume`; remove it with `remux integrate
// opencode-resume --uninstall`. remux rewrites this file on install, so edit
// nothing here.
//
// It only runs the remux binary below, never opens a socket, and ignores
// every error: outside a remux pane the command does nothing at all.

import { execFile } from "node:child_process";

const REMUX_BIN = "__REMUX_BIN__";

// The full TUI reports through remux-tui-session/tui.js, which knows the
// session on screen. serve, web and attach run sessions for other clients, not for
// this pane, so only `run` and `--mini` report from here.
function reporting() {
  const main = typeof Bun === "undefined" ? "" : String(Bun.main ?? "");
  if (main.endsWith("/cli/tui/worker.js")) {
    return false;
  }
  const argv = process.argv;
  return argv[2] === "run" || argv.includes("--mini");
}

// A child session (a subagent's) is reported as its root, the session the
// user resumes.
const parents = new Map();
let reported;

function rootOf(id) {
  for (let depth = 0; depth < 16 && parents.has(id); depth++) {
    id = parents.get(id);
  }
  return id;
}

function report(id) {
  try {
    execFile(REMUX_BIN, ["agent", "session", "opencode", "--id", id], { timeout: 3000 }, () => {});
  } catch {}
}

export default {
  id: "remux-agent-session",
  // OpenCode 2.x calls `setup` and never `server`. It runs server plugins in
  // one background service that every TUI on the machine shares and that
  // outlives the pane that started it, so a session seen there cannot be
  // traced to a pane: under 2.x only the TUI plugin reports. 1.x's TUI worker
  // calls `setup` too, beside `server`, so it must stay a no-op.
  setup: () => {},
  server: async () => ({
    event: async ({ event }) => {
      if (event?.type !== "session.created" && event?.type !== "session.updated") {
        return;
      }
      const info = event.properties?.info;
      if (typeof info?.id !== "string" || !reporting()) {
        return;
      }
      if (typeof info.parentID === "string") {
        parents.set(info.id, info.parentID);
      }
      const root = rootOf(info.id);
      if (root !== reported) {
        reported = root;
        report(root);
      }
    },
  }),
};
