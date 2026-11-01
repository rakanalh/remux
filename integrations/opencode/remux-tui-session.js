// managed by remux; REMUX_INTEGRATION_ID=opencode-resume-tui REMUX_INTEGRATION_VERSION=1
//
// Tells remux which OpenCode session is open in the TUI in its pane, so that
// `remux restart` can resume it there. Installed by `remux integrate
// opencode-resume`, which lists this file in tui.json; remove both with
// `remux integrate opencode-resume --uninstall`. remux rewrites this file on
// install, so edit nothing here.
//
// It only runs the remux binary below, never opens a socket, and ignores
// every error: outside a remux pane the command does nothing at all.

import { execFile } from "node:child_process";

const REMUX_BIN = "__REMUX_BIN__";

// OpenCode 1.18 has no event for a session being selected, so the open route
// is polled. A poll that finds the same session does nothing.
const POLL_MS = 250;

function report(id) {
  try {
    execFile(
      REMUX_BIN,
      ["agent", "session", "opencode", "--id", id, "--source", "select"],
      { timeout: 3000 },
      () => {},
    );
  } catch {}
}

// The session on screen can be a child (a subagent's); the one to resume is
// its root.
async function rootOf(api, id) {
  for (let depth = 0; depth < 16; depth++) {
    let parent;
    try {
      parent = (await api.client.session.get({ sessionID: id }))?.data?.parentID;
    } catch {
      return id;
    }
    if (typeof parent !== "string" || parent === "") {
      return id;
    }
    id = parent;
  }
  return id;
}

export default {
  id: "remux-tui-session",
  tui: async (api) => {
    let seen;
    let reported;
    let busy = false;
    const poll = async () => {
      const route = api.route?.current;
      const id = route?.name === "session" ? route.params?.sessionID : undefined;
      if (busy || typeof id !== "string" || id === seen) {
        return;
      }
      busy = true;
      try {
        seen = id;
        const root = await rootOf(api, id);
        if (root !== reported) {
          reported = root;
          report(root);
        }
      } finally {
        busy = false;
      }
    };
    const timer = setInterval(() => {
      poll().catch(() => {});
    }, POLL_MS);
    api.lifecycle?.onDispose?.(() => clearInterval(timer));
  },
};
