#!/usr/bin/env node
// UO-4 acceptance: mechanical split of features/session/session.module.css.
//
// 1. Every selector in the old file (read from a git ref, default origin/main)
//    must be *really* declared (a rule with declarations other than
//    `composes`) in exactly one new module, with the same multiplicity.
//    `composes`-only re-export stubs are not declarations; they must point at
//    a class that is really declared somewhere.
// 2. The deleted dead classes are neither declared in a new module nor
//    referenced from any .ts/.tsx as `styles.x` / `css.x` / `session.x` /
//    `sessionCss.x` / `opt.x`.
// 3. @keyframes land in the expected modules (`dim` is intentionally copied
//    into both toolCard and transcript).
//
// Usage: node scripts/uo4-session-split-coverage.mjs [gitRef]
import { execSync } from "node:child_process";
import { readFileSync, existsSync } from "node:fs";
import { join, dirname } from "node:path";
import { fileURLToPath } from "node:url";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const ref = process.argv[2] || "origin/main";
const oldRel = "src/features/session/session.module.css";

const NEW_MODULES = [
  "src/chrome/sessionHeader.module.css",
  "src/chrome/sessionPage.module.css",
  "src/features/session/ViewSwitch.module.css",
  "src/features/session/runDetails.module.css",
  "src/features/session/transcript.module.css",
  "src/features/session/toolCard.module.css",
  "src/features/session/taskTrack.module.css",
  "src/components/diff.module.css",
  "src/features/session/screen.module.css",
  "src/features/session/composer.module.css",
  "src/features/session/popover.module.css",
  "src/features/session/contextUsage.module.css",
  "src/features/session/optionRows.module.css",
  "src/features/session/effort.module.css",
  "src/features/approvals/decision.module.css",
];

const DEAD = [
  "perm", "permMenu", "sendIcon", "deskOnly", "filesPane", "filesBack",
  "filesHint", "qOpts", "qOpt", "openOff",
];

// keyframes name -> modules that must contain the (real) keyframes definition
const EXPECTED_KEYFRAMES = {
  effortPendingPulse: ["src/features/session/composer.module.css"],
  emberDrift: ["src/features/session/effort.module.css"],
  emberDriftBack: ["src/features/session/effort.module.css"],
  emberTwinkleSlow: ["src/features/session/effort.module.css"],
  emberTwinkle: ["src/features/session/effort.module.css"],
  emberTwinkleFast: ["src/features/session/effort.module.css"],
  emberGlow: ["src/features/session/effort.module.css"],
  knobBreath: ["src/features/session/effort.module.css"],
  ember: ["src/features/session/effort.module.css"],
  emberChipGlow: ["src/features/session/effort.module.css"],
  spark: ["src/features/session/effort.module.css"],
  dim: [
    "src/features/session/toolCard.module.css",
    "src/features/session/transcript.module.css",
  ],
};

const stripComments = (s) => s.replace(/\/\*[\s\S]*?\*\//g, "");
const norm = (s) => stripComments(s).replace(/\s+/g, " ").trim();

// Parse one stylesheet:
//  selectors: Map<normalizedSelector, count of REAL (non-stub) rules>
//  stubs: Array<{ cls, from, selector }>
//  keyframes: Set<name>
function parse(css) {
  const selectors = new Map();
  const stubs = [];
  const keyframes = new Set();
  const walk = (text) => {
    let i = 0;
    while (i < text.length) {
      const open = text.indexOf("{", i);
      if (open === -1) break;
      const prelude = norm(text.slice(i, open));
      let depth = 1;
      let j = open + 1;
      for (; j < text.length && depth; j++) {
        if (text[j] === "{") depth++;
        else if (text[j] === "}") depth--;
      }
      const body = text.slice(open + 1, j - 1);
      if (/^@keyframes\b/.test(prelude)) {
        const name = prelude.split(/\s+/)[1];
        keyframes.add(name);
      } else if (/^@media\b/.test(prelude)) {
        walk(body);
      } else if (prelude) {
        const decls = stripComments(body)
          .split(";")
          .map((d) => d.trim())
          .filter(Boolean);
        const isStub =
          decls.length > 0 && decls.every((d) => /^composes:/.test(d));
        const parts = prelude.split(",").map((s) => s.trim()).filter(Boolean);
        if (isStub) {
          for (const sel of parts) {
            const cls = sel.match(/^\.([A-Za-z0-9_-]+)$/)?.[1];
            const m = decls[0].match(/^composes:\s+([A-Za-z0-9_-]+)\s+from\s+"(.+)"$/);
            stubs.push({ selector: sel, cls: cls ?? null, composes: m?.[1] ?? null, from: m?.[2] ?? null });
          }
        } else {
          for (const sel of parts) {
            selectors.set(sel, (selectors.get(sel) ?? 0) + 1);
          }
        }
      }
      i = j;
    }
  };
  walk(css);
  return { selectors, stubs, keyframes };
}

let oldCss;
try {
  oldCss = execSync(`git show ${ref}:web/${oldRel}`, {
    cwd: join(root, ".."),
    encoding: "utf8",
    stdio: ["ignore", "pipe", "pipe"],
  });
} catch {
  oldCss = execSync(`git show ${ref}:${oldRel}`, { cwd: root, encoding: "utf8" });
}
const old = parse(oldCss);

const errors = [];
const perModule = new Map();
const gitAtRef = (rel) =>
  execSync(`git show ${ref}:web/${rel}`, {
    cwd: join(root, ".."),
    encoding: "utf8",
    stdio: ["ignore", "pipe", "ignore"],
  });
// These two modules pre-existed and merely had rules appended; their pre-UO-4
// selectors are not part of the migration accounting.
const BASELINE = new Set([
  "src/features/session/runDetails.module.css",
  "src/features/session/transcript.module.css",
]);
const baselines = new Map();
for (const rel of NEW_MODULES) {
  const abs = join(root, rel);
  if (!existsSync(abs)) {
    errors.push(`missing new module: ${rel}`);
    continue;
  }
  const parsed = parse(readFileSync(abs, "utf8"));
  perModule.set(rel, parsed);
  if (BASELINE.has(rel)) {
    const base = parse(gitAtRef(rel));
    baselines.set(rel, base);
    for (const [sel, n] of base.selectors) {
      const rest = (parsed.selectors.get(sel) ?? 0) - n;
      if (rest <= 0) parsed.selectors.delete(sel);
      else parsed.selectors.set(sel, rest);
    }
  }
}

// old selectors whose every class token is a deleted class are allowed to vanish
const deadSelectors = new Set();
for (const sel of old.selectors.keys()) {
  const classes = [...sel.matchAll(/\.([A-Za-z0-9_-]+)/g)].map((m) => m[1]);
  if (classes.length > 0 && classes.every((c) => DEAD.includes(c))) {
    deadSelectors.add(sel);
  }
}

// 1a. every old non-dead selector: same total real multiplicity, in exactly one module
for (const [sel, oldCount] of old.selectors) {
  if (deadSelectors.has(sel)) continue;
  let total = 0;
  const homes = [];
  for (const [rel, p] of perModule) {
    const c = p.selectors.get(sel);
    if (c) {
      total += c;
      homes.push(`${rel}×${c}`);
    }
  }
  if (total !== oldCount) {
    errors.push(
      `selector ${sel}: old count ${oldCount}, new count ${total} [${homes.join(", ") || "nowhere"}]`,
    );
  } else if (homes.length !== 1) {
    errors.push(`selector ${sel}: spread across ${homes.join(", ")} (must be one module)`);
  }
}
// 1b. no new real selector that did not exist in the old file
for (const [rel, p] of perModule) {
  for (const sel of p.selectors.keys()) {
    if (!old.selectors.has(sel)) errors.push(`new selector ${sel} in ${rel} (not in old file)`);
  }
}
// 1c. every stub's composes chain ends at a class with a real rule
const normRel = (rel) => {
  const parts = [];
  for (const seg of rel.split("/")) {
    if (seg === "..") parts.pop();
    else if (seg !== ".") parts.push(seg);
  }
  return parts.join("/");
};
const resolvesToReal = (rel, cls, seen = new Set()) => {
  const key = `${rel}#${cls}`;
  if (seen.has(key)) return false;
  seen.add(key);
  const p = perModule.get(rel);
  if (!p) return false;
  if (p.selectors.has(`.${cls}`)) return true;
  const stub = p.stubs.find((s) => s.selector === `.${cls}`);
  if (!stub?.from) return false;
  return resolvesToReal(normRel(join(dirname(rel), stub.from)), stub.composes, seen);
};
for (const [rel, p] of perModule) {
  for (const stub of p.stubs) {
    if (!stub.cls || !stub.composes || !stub.from) {
      errors.push(`${rel}: malformed stub selector "${stub.selector}"`);
      continue;
    }
    const targetRel = normRel(join(dirname(rel), stub.from));
    if (!perModule.has(targetRel)) {
      errors.push(`${rel}: stub .${stub.cls} -> ${stub.from} (not a split module)`);
      continue;
    }
    if (!resolvesToReal(targetRel, stub.composes)) {
      errors.push(`${rel}: stub .${stub.cls} chain never reaches a real .${stub.composes} rule`);
    }
  }
}

// 1d. cascade-order guard: .popover/.popoverCard are base rules that
// .usageSheet (z-index:60) overrides by source order. They must NOT be
// re-exported as composes stubs from composer/contextUsage — a stub pulls
// popover.module.css into that module's dependency position AFTER the
// overrides, flipping z-index so the mobile context sheet falls behind the
// options sheet. Those two importers must bind popover.module.css directly
// and import it first.
for (const rel of [
  "src/features/session/composer.module.css",
  "src/features/session/contextUsage.module.css",
]) {
  const bad = perModule.get(rel)?.stubs.filter((s) => s.cls === "popover" || s.cls === "popoverCard");
  for (const s of bad ?? []) {
    errors.push(`${rel}: .${s.cls} must bind popover.module.css directly, not via composes (cascade order)`);
  }
}

// 2. dead classes: not really declared, not referenced from ts/tsx
for (const cls of DEAD) {
  for (const [rel, p] of perModule) {
    if (p.selectors.has(`.${cls}`)) errors.push(`dead class .${cls} still declared in ${rel}`);
  }
}
const tsFiles = execSync(
  `find src -type f \\( -name "*.ts" -o -name "*.tsx" \\) -print`,
  { cwd: root, encoding: "utf8" },
).split("\n").filter(Boolean);
for (const cls of DEAD) {
  const re = new RegExp(`\\b(?:styles|css|session|sessionCss|opt)\\.${cls}\\b`);
  for (const f of tsFiles) {
    const text = readFileSync(join(root, f), "utf8");
    if (re.test(text)) errors.push(`dead class .${cls} referenced in ${f}`);
  }
}

// 3. keyframes
for (const [name, mods] of Object.entries(EXPECTED_KEYFRAMES)) {
  const got = [...perModule].filter(([, p]) => p.keyframes.has(name)).map(([rel]) => rel);
  const want = [...mods].sort();
  const gotSorted = [...got].sort();
  if (gotSorted.join("|") !== want.join("|")) {
    errors.push(`@keyframes ${name}: expected [${want}], found [${gotSorted}]`);
  }
}

// 4. old file gone
if (existsSync(join(root, oldRel))) errors.push(`${oldRel} still exists`);

// report
for (const [rel, p] of perModule) {
  console.log(
    `${rel}: ${p.selectors.size} real selectors, ${p.stubs.length} stubs, ${p.keyframes.size} keyframes`,
  );
}
if (errors.length) {
  console.error(`\nUO-4 coverage FAILED (${errors.length}):`);
  for (const e of errors) console.error("  - " + e);
  process.exit(1);
}
console.log(
  `\nUO-4 coverage OK: ${old.selectors.size} selectors mapped 1:1, ${DEAD.length} dead classes unreferenced, ${Object.keys(EXPECTED_KEYFRAMES).length} keyframes accounted`,
);
