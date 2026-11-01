// Drive an INSTALLED remux OpenCode plugin the way OpenCode 1.18 does, with
// the surface the T18 spike recorded:
//
//   node opencode_driver.mjs <plugin.js> <spec.json>
//
// spec.json, for the server plugin (`export default { id, server }`):
//   { "argv": ["run"], "bun_main": "/$bunfs/root/src/index.js",
//     "events": [ { "type": "session.created", "properties": { "info": { "id": "r" } } } ] }
// `argv` replaces process.argv[2..]; `bun_main`, when given, becomes Bun.main.
//
// for the TUI plugin (`export default { id, tui }`):
//   { "parents": { "child": "root" },
//     "routes": [ { "after_ms": 300, "route": { "name": "session", "params": { "sessionID": "child" } } } ],
//     "run_ms": 1500 }
// `api.route.current` follows `routes`, `api.client.session.get` answers
// `{ data: { id, parentID } }` from `parents`, and dispose runs at `run_ms`.
//
// "api": "v2" drives the same files the way OpenCode 2.x does (see
// opencode-v2-spike.md): the server plugin's `setup(ctx)` is called and its
// `server` is not, and the TUI plugin's `setup(ctx)` is called with
//   { "sessions": { "child": { "parentID": "root", "loaded_after_ms": 600 }, "root": {} },
//     "routes": [ { "after_ms": 300, "route": { "type": "session", "sessionID": "child" } } ],
//     "run_ms": 1500 }
// `ctx.ui.router.current()` follows `routes`, `ctx.data.session.get(id)` answers
// `{ id, parentID }` for a listed session once its `loaded_after_ms` (default 0)
// has passed and undefined before, and the cleanup `setup` returns runs at
// `run_ms`. `argv` replaces process.argv[2..] in every mode. The driver prints
// one JSON line naming the hook it called, or the one the module lacks.
//
// Both take "record": every execFile the plugin makes is appended there as a
// JSON line with its args, exit code, stdout and stderr (see pi_omp_driver.mjs).
import childProcess from "node:child_process";
import { appendFileSync, readFileSync } from "node:fs";
import { syncBuiltinESMExports } from "node:module";
import { pathToFileURL } from "node:url";

const [plugin, specPath] = process.argv.slice(2);
const spec = JSON.parse(readFileSync(specPath, "utf8"));
if (spec.record) {
  const real = childProcess.execFile;
  childProcess.execFile = (file, args, options, callback) =>
    real(file, args, options, (error, stdout, stderr) => {
      appendFileSync(
        spec.record,
        JSON.stringify({ args, code: error ? (error.code ?? -1) : 0, stdout, stderr }) + "\n",
      );
      callback?.(error, stdout, stderr);
    });
  syncBuiltinESMExports();
}
process.argv = [process.argv[0], "/$bunfs/root/src/index.js", ...(spec.argv ?? [])];
if (spec.bun_main) {
  globalThis.Bun = { main: spec.bun_main };
}
const { default: mod } = await import(pathToFileURL(plugin).href);
const start = Date.now();
const said = (hook) => console.log(JSON.stringify({ called: hook }));

if (spec.api === "v2" && "server" in mod) {
  if (typeof mod.setup !== "function") {
    said("no setup");
  } else {
    said("setup");
    const cleanup = await mod.setup({ options: {} });
    if (typeof cleanup === "function") await cleanup();
  }
} else if (spec.api === "v2") {
  if (typeof mod.setup !== "function") {
    said("no setup");
  } else {
    said("setup");
    let route = { type: "home" };
    const sessions = spec.sessions ?? {};
    const ctx = {
      ui: { router: { current: () => route } },
      data: {
        session: {
          get: (id) => {
            const known = sessions[id];
            if (!known || Date.now() - start < (known.loaded_after_ms ?? 0)) return undefined;
            return { id, ...(known.parentID ? { parentID: known.parentID } : {}) };
          },
        },
      },
    };
    const cleanup = await mod.setup(ctx);
    for (const step of spec.routes ?? []) {
      setTimeout(() => {
        route = step.route;
      }, step.after_ms);
    }
    setTimeout(() => {
      if (typeof cleanup === "function") cleanup();
    }, spec.run_ms ?? 1500);
  }
} else if (mod.server) {
  said("server");
  const hooks = await mod.server({}, undefined);
  for (const event of spec.events ?? []) {
    await hooks.event({ event });
  }
} else {
  said("tui");
  let route = { name: "home" };
  const disposers = [];
  const api = {
    route: {
      get current() {
        return route;
      },
    },
    client: {
      session: {
        get: async ({ sessionID }) => ({ data: { id: sessionID, parentID: spec.parents?.[sessionID] } }),
      },
    },
    lifecycle: { onDispose: (fn) => disposers.push(fn) },
  };
  await mod.tui(api, undefined, {});
  for (const step of spec.routes ?? []) {
    setTimeout(() => {
      route = step.route;
    }, step.after_ms);
  }
  setTimeout(() => disposers.forEach((fn) => fn()), spec.run_ms ?? 1500);
}
