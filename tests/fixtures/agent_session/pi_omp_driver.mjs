// Drive an INSTALLED remux pi/omp extension the way pi and omp do: import it,
// hand it an `on` registrar, then fire events at the registered handlers with
// a fake context.
//
//   node pi_omp_driver.mjs <extension.ts> <spec.json>
//
// spec.json: {
//   "ctx":    { "mode": "tui", "hasUI": true, "file": "/abs/s.jsonl", "id": "abc" },
//   "env":    { "OMPCODE": "1" },          // set in this process before firing
//   "events": [ { "type": "session_start", "reason": "new" }, ... ]
// }
// A `file` or `id` of null makes the session manager return undefined for it.
//
// Node 24 strips the extension's TypeScript types on import. The extension's
// execFile children keep this process alive until they exit, so its last
// report has finished when the driver does.
import { readFileSync } from "node:fs";
import { pathToFileURL } from "node:url";

const [extension, specPath] = process.argv.slice(2);
const spec = JSON.parse(readFileSync(specPath, "utf8"));
for (const [key, value] of Object.entries(spec.env ?? {})) {
  process.env[key] = value;
}
const handlers = {};
const pi = {
  on(event, handler) {
    (handlers[event] ??= []).push(handler);
  },
};
const { default: register } = await import(pathToFileURL(extension).href);
register(pi);
const c = spec.ctx ?? {};
const ctx = {
  mode: c.mode,
  hasUI: c.hasUI,
  sessionManager: {
    getSessionFile: () => c.file ?? undefined,
    getSessionId: () => c.id ?? undefined,
  },
};
for (const event of spec.events ?? []) {
  for (const handler of handlers[event.type] ?? []) {
    await handler(event, ctx);
  }
}
