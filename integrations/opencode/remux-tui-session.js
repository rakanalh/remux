// managed by remux; REMUX_INTEGRATION_ID=opencode-resume-tui REMUX_INTEGRATION_VERSION=1
//
// Tells remux which OpenCode session is open in the TUI in its pane, so that
// `remux restart` can resume it there. Installed by `remux integrate
// opencode-resume`, which lists this file in tui.json for OpenCode 1.x
// (2.x finds its directory by itself); remove it with `remux integrate
// opencode-resume --uninstall`. remux rewrites this file on install, so edit
// nothing here.
//
// It only runs the remux binary below, never opens a socket, and ignores
// every error: outside a remux pane the command does nothing at all.

import { execFile } from "node:child_process";

const REMUX_BIN = "__REMUX_BIN__";

// Neither OpenCode 1.x nor 2.x has an event for a session being selected, so
// the open route is polled. A poll that finds the same root does nothing, and
// the CLI runs only when the ROOT of the open session changes.
const POLL_MS = 500;
const MAX_DEPTH = 16;

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
  for (let depth = 0; depth < MAX_DEPTH; depth++) {
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

// 2.x's session store fills in after the route changes. Until the session on
// screen is in it, its parent is unknown, and a report then could name the
// child instead of the root.
function rootFromStore(ctx, id) {
  if (!ctx.data?.session?.get?.(id)) {
    return undefined;
  }
  for (let depth = 0; depth < MAX_DEPTH; depth++) {
    const parent = ctx.data.session.get(id)?.parentID;
    if (typeof parent !== "string" || parent === "") {
      return id;
    }
    id = parent;
  }
  return id;
}

// A 2.x TUI started with --server shows a session of another machine's
// server, which `opencode --session=<id>` here would not open.
function attachedElsewhere() {
  return process.argv.some((arg) => arg === "--server" || arg.startsWith("--server="));
}

export default {
  id: "remux-tui-session",
  // OpenCode 1.x.
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
  // OpenCode 2.x, which never calls `tui`; 1.x never calls `setup`.
  setup: (ctx) => {
    if (attachedElsewhere()) {
      return;
    }
    let reported;
    const poll = () => {
      const route = ctx.ui?.router?.current?.();
      const id = route?.type === "session" ? route.sessionID : undefined;
      if (typeof id !== "string") {
        return;
      }
      const root = rootFromStore(ctx, id);
      if (root !== undefined && root !== reported) {
        reported = root;
        report(root);
      }
    };
    const timer = setInterval(() => {
      try {
        poll();
      } catch {}
    }, POLL_MS);
    return () => clearInterval(timer);
  },
};
