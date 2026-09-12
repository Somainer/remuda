#!/usr/bin/env node
/** Generate `src/lib/api.generated.ts` from the Hub OpenAPI spec with stable key order. */

import { spawnSync } from "node:child_process";
import { mkdtempSync, readFileSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

function sortValue(value) {
  if (Array.isArray(value)) return value.map(sortValue);
  if (value && typeof value === "object") {
    const out = {};
    for (const key of Object.keys(value).sort()) out[key] = sortValue(value[key]);
    return out;
  }
  return value;
}

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const specPath = join(root, "../crates/remuda-hub/openapi/openapi.json");
const outPath = join(root, "src/lib/api.generated.ts");
const spec = sortValue(JSON.parse(readFileSync(specPath, "utf8")));
const sorted = join(mkdtempSync(join(tmpdir(), "remuda-openapi-")), "openapi.json");
writeFileSync(sorted, `${JSON.stringify(spec, null, 2)}\n`);

const bin = join(root, "node_modules/openapi-typescript/bin/cli.js");
const result = spawnSync(process.execPath, [bin, sorted, "-o", outPath], { stdio: "inherit" });
process.exit(result.status ?? 1);
