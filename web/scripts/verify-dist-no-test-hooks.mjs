#!/usr/bin/env node
/**
 * c-unboundresume r4 item 1 — production-isolation gate.
 *
 * The NORMAL production artifact (web/dist) must never contain the e2e test
 * handle (`window.__remudaHub`): installation is compile-time gated and dead
 * code eliminated, so grepping the built ASSETS (not the source) proves the
 * live bundle cannot expose it. The dedicated e2e artifact (web/dist-e2e) is
 * expected to contain it and is verified separately by the security e2e.
 */
import { readdirSync, readFileSync, statSync } from "node:fs";
import { join } from "node:path";

const rootArg = process.argv[2] ?? "dist";
const root = join(process.cwd(), rootArg);
const token = "__remudaHub";
const scanExts = new Set([".js", ".mjs", ".css", ".html"]);
let occurrences = 0;
const hits = [];

const walk = (dir) => {
  for (const name of readdirSync(dir)) {
    const p = join(dir, name);
    const st = statSync(p);
    if (st.isDirectory()) {
      walk(p);
    } else if (scanExts.has(name.slice(name.lastIndexOf(".")))) {
      const body = readFileSync(p, "utf8");
      let from = 0;
      for (;;) {
        const at = body.indexOf(token, from);
        if (at < 0) break;
        occurrences += 1;
        from = at + token.length;
      }
      if (body.includes(token)) hits.push(p);
    }
  }
};

try {
  walk(root);
} catch (err) {
  console.error(`cannot scan ${root}: ${err?.message ?? err}`);
  process.exit(2);
}

console.log(`${root}: ${occurrences} ${token} occurrence(s)`);
for (const h of hits) console.log(`  hit: ${h}`);
process.exit(occurrences === 0 ? 0 : 1);
