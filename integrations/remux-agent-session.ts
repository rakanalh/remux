// managed by remux; REMUX_INTEGRATION_ID=pi-omp-resume REMUX_INTEGRATION_VERSION=1
//
// Tells remux which session this agent is running in its pane, so that
// `remux restart` can resume it there. Installed by `remux integrate
// __REMUX_AGENT__-resume`; remove it with `remux integrate __REMUX_AGENT__-resume --uninstall`.
// remux rewrites this file on install, so edit nothing here.
//
// It only runs the remux binary below, never opens a socket, and ignores
// every error: outside a remux pane the command does nothing at all.

import { execFile } from "node:child_process";

const REMUX_BIN = "__REMUX_BIN__";
const AGENT = "__REMUX_AGENT__";

// Both report only with a UI: `hasUI` is false in print and RPC mode, which
// is how an agent runs when another agent's tool starts it. (pi's context has
// no `mode`.) omp also stops when OMPCODE is set: omp sets it in its own bash
// tool, so an omp started from inside omp would otherwise take over the
// pane's session.
function reporting(ctx: any): boolean {
  if (ctx?.hasUI !== true) {
    return false;
  }
  return AGENT !== "omp" || process.env.OMPCODE !== "1";
}

// The last reference sent. agent_start fires on every turn, and an unchanged
// session must not spawn the remux CLI each time.
let last = "";

function report(ctx: any, source?: string, onlyIfChanged = false): void {
  if (!reporting(ctx)) {
    return;
  }
  let file: unknown;
  let id: unknown;
  try {
    file = ctx.sessionManager.getSessionFile();
  } catch {}
  try {
    id = ctx.sessionManager.getSessionId();
  } catch {}
  const args = ["agent", "session", AGENT];
  if (typeof file === "string" && file.startsWith("/")) {
    args.push("--path", file);
  } else if (typeof id === "string" && id !== "") {
    args.push("--id", id);
  } else {
    return;
  }
  const ref = args.slice(3).join("\0");
  if (onlyIfChanged && ref === last) {
    return;
  }
  last = ref;
  if (source) {
    args.push("--source", source);
  }
  try {
    execFile(REMUX_BIN, args, { timeout: 3000 }, () => {});
  } catch {}
}

export default function (pi: any): void {
  pi.on("session_start", (event: any, ctx: any) => {
    report(ctx, event?.reason ?? (AGENT === "omp" ? "startup" : undefined));
  });
  pi.on("session_switch", (event: any, ctx: any) => {
    report(ctx, event?.reason ?? "resume");
  });
  // No source: a session that changed under the agent without a start or a
  // switch is refused by remux. An unchanged one is not sent at all.
  pi.on("agent_start", (_event: any, ctx: any) => {
    report(ctx, undefined, true);
  });
}
