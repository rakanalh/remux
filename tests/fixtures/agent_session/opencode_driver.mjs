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

if (mod.server) {
  const hooks = await mod.server({}, undefined);
  for (const event of spec.events ?? []) {
    await hooks.event({ event });
  }
} else {
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
