#!/usr/bin/env node
// UO-4 acceptance: mechanical split of features/session/session.module.css.
//
// It proves four things, reading the old file from a git ref (default
// origin/main):
//
//  1. RULE EQUIVALENCE — every old rule survives as a REAL rule (not a
//     composes-only stub) in exactly one new module with the SAME
//       * at-rule/media context chain,
//       * selector (incl. multiplicity),
//       * declaration body.
//     Moving a rule across the media boundary (e.g. the .bubble 767px swap)
//     or changing a declaration therefore fails, not just a selector rename.
//
//  2. CASCADE ORDER — for two classes applied to the SAME element whose REAL
//     rules set a longhand to different values, the rule that was LATER in the
//     old file (the required winner) must still win after the split. Winner is
//     computed the way Vite emits CSS: each module's `composes` dependencies
//     are injected first, then the importer's direct CSS imports in statement
//     order (first injection wins); ties inside one module use source order.
//     A composes-only re-export that drags a modifier ahead of its base (the
//     ember/.chip bug) is rejected.
//
//  3. DEAD CLASSES — every class token of a deleted class is gone from every
//     destination selector (incl. pseudo-class/compound forms like
//     .sendIcon:disabled) and from .ts/.tsx bindings.
//
//  4. KEYFRAMES land in the expected modules.
//
// `node .../this-script.mjs [gitRef]` verifies.
// `node .../this-script.mjs --self-test` exercises the order/body logic with a
// deliberately swapped pair and a moved-media rule, both of which it must
// reject.
import { execSync } from "node:child_process";
import { readFileSync, existsSync } from "node:fs";
import { join, dirname, resolve as pathResolve, normalize as normPath } from "node:path";
import { fileURLToPath } from "node:url";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
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

const DEAD = new Set([
  "perm", "permMenu", "sendIcon", "deskOnly", "filesPane", "filesBack",
  "filesHint", "qOpts", "qOpt", "openOff",
]);

const EXPECTED_KEYFRAMES = {
  effortPendingPulse: ["src/features/session/composer.module.css"],
  emberDrift: ["src/features/session/effort.module.css"],
  emberDriftBack: ["src/features/session/effort.module.css"],
  emberTwinkleSlow: ["src/features/session/effort.module.css"],
  emberTwinkle: ["src/features/session/effort.module.css"],
  emberTwinkleFast: ["src/features/session/effort.module.css"],
  emberGlow: ["src/features/session/effort.module.css"],
  knobBreath: ["src/features/session/effort.module.css"],
  ember: ["src/features/session/composer.module.css"],
  emberChipGlow: ["src/features/session/composer.module.css"],
  spark: ["src/features/session/composer.module.css"],
  dim: ["src/features/session/toolCard.module.css", "src/features/session/transcript.module.css"],
};

const stripComments = (s) => s.replace(/\/\*[\s\S]*?\*\//g, "");
const normWs = (s) => stripComments(s).replace(/\s+/g, " ").trim();

// ---------- shorthand -> longhands (only what this codebase uses) ----------
function expand(prop, value) {
  const out = new Map();
  const v = value.trim().replace(/\s+/g, " ");
  const set = (p, x) => out.set(p, x);
  if (prop === "flex") {
    let g = "0", s = "0", b = "auto";
    if (v === "none") [g, s, b] = ["0", "0", "auto"];
    else if (v === "auto") [g, s, b] = ["1", "1", "auto"];
    else {
      const t = v.split(/\s+/);
      if (t[0]) g = t[0]; if (t[1]) s = t[1]; if (t[2]) b = t[2];
    }
    set("flex-grow", g); set("flex-shrink", s); set("flex-basis", b);
    return out;
  }
  if (prop === "background") {
    for (const p of ["background-color","background-image","background-repeat","background-position","background-size"]) set(p, v);
    return out;
  }
  if (prop === "border") {
    for (const side of ["top","right","bottom","left"])
      for (const part of ["width","style","color"]) set(`border-${side}-${part}`, v);
    return out;
  }
  set(prop, v);
  return out;
}

// Parse a stylesheet into ordered REAL rules:
//  { media:string[], selector:string, decls:Map(longhand,value), index }
// plus stubs: {cls, composes, from}, and keyframes:Set.
function parse(css) {
  const rules = [], stubs = [], keyframes = new Set();
  let order = 0;
  const walk = (text, media) => {
    let i = 0;
    while (i < text.length) {
      const open = text.indexOf("{", i);
      if (open === -1) break;
      const prelude = normWs(text.slice(i, open));
      let depth = 1, j = open + 1;
      for (; j < text.length && depth; j++) { if (text[j] === "{") depth++; else if (text[j] === "}") depth--; }
      const body = text.slice(open + 1, j - 1);
      if (/^@keyframes\b/.test(prelude)) {
        keyframes.add(prelude.split(/\s+/)[1]);
      } else if (/^@media\b/.test(prelude)) {
        walk(body, [...media, prelude]);
      } else if (prelude && !/^@/.test(prelude)) {
        const raw = stripComments(body).split(";").map((d) => d.trim()).filter(Boolean);
        const isStub = raw.length > 0 && raw.every((d) => /^composes:/.test(d));
        for (const sel of prelude.split(",").map((s) => s.trim()).filter(Boolean)) {
          if (isStub) {
            const cls = sel.match(/^\.([A-Za-z0-9_-]+)$/)?.[1] ?? null;
            const m = raw[0].match(/^composes:\s+([A-Za-z0-9_-]+)\s+from\s+"(.+)"$/);
            stubs.push({ selector: sel, cls, composes: m?.[1] ?? null, from: m?.[2] ?? null });
          } else {
            const decls = new Map();
            for (const d of raw) {
              const ci = d.indexOf(":");
              for (const [lp, lv] of expand(d.slice(0, ci).trim(), d.slice(ci + 1))) {
                if (!decls.has(lp)) decls.set(lp, lv);
              }
            }
            rules.push({ media, selector: sel, decls, index: order++ });
          }
        }
      }
      i = j;
    }
  };
  walk(css, []);
  // selector -> array of rules (multiplicity), keyed within media context
  const bySel = new Map();
  for (const r of rules) {
    const k = JSON.stringify([r.media.join(" | "), r.selector]);
    if (!bySel.has(k)) bySel.set(k, []);
    bySel.get(k).push(r);
  }
  return { rules, bySel, stubs, keyframes };
}

const declKey = (m) => JSON.stringify([...m.entries()].sort());

// Effective root-context declarations per single class: for each longhand the
// value from the LAST root rule (in source order) setting it, plus that rule's
// index. Grouped comma rules and repeated classes are handled per-longhand so
// an unrelated later rule (e.g. a shared width group) doesn't decide colour.
function effectiveByClass(parsed) {
  const eff = new Map(); // cls -> {decls:Map(lh->val), propIndex:Map(lh->idx)}
  for (const r of parsed.rules) {
    if (r.media.length) continue; // root context only
    const mm = r.selector.match(/^\.([A-Za-z0-9_-]+)$/);
    if (!mm) continue;
    const cls = mm[1];
    if (!eff.has(cls)) eff.set(cls, { decls: new Map(), propIndex: new Map() });
    const e = eff.get(cls);
    for (const [p, v] of r.decls) {
      e.decls.set(p, v); // later rule wins
      e.propIndex.set(p, r.index);
    }
  }
  return eff;
}

// ---------------- self-test harness (synthetic modules) ----------------
// Returns error strings for a tiny module/importer graph so --self-test can
// assert the validators reject bad inputs.
function validateRuleEquivalence(oldParsed, moduleParsed, baselines) {
  const errors = [];
  const allReal = new Map(); // key -> [{module, rule}]
  for (const [rel, p] of moduleParsed) {
    for (const r of p.rules) {
      const k = JSON.stringify([r.media.join(" | "), r.selector]);
      if (!allReal.has(k)) allReal.set(k, []);
      allReal.get(k).push({ rel, rule: r });
    }
  }
  for (const [k, oldRules] of oldParsed.bySel) {
    const deadSel = (() => {
      const classes = [...k.matchAll(/\.([A-Za-z0-9_-]+)/g)].map((m) => m[1]);
      return classes.length > 0 && classes.every((c) => DEAD.has(c));
    })();
    if (deadSel) continue;
    const got = allReal.get(k) ?? [];
    if (got.length !== oldRules.length) {
      errors.push(`rule ${JSON.parse(k)[1]} in [${JSON.parse(k)[0] || "root"}]: multiplicity ${oldRules.length} -> ${got.length}`);
      continue;
    }
    // bodies must match as a multiset
    const ob = oldRules.map((r) => declKey(r.decls)).sort();
    const gb = got.map((g) => declKey(g.rule.decls)).sort();
    for (let i = 0; i < ob.length; i++) if (ob[i] !== gb[i]) errors.push(`body changed for ${JSON.parse(k)[1]} in [${JSON.parse(k)[0]}]`);
  }
  // no genuinely new rule beyond baselines + dead drop
  for (const [rel, p] of moduleParsed) {
    for (const r of p.rules) {
      const k = JSON.stringify([r.media.join(" | "), r.selector]);
      const isBase = (baselines.get(rel)?.rules ?? []).some((b) => JSON.stringify([b.media.join(" | "), b.selector]) === k);
      if (isBase) continue;
      if (!oldParsed.bySel.has(k)) errors.push(`new rule ${r.selector} in ${rel} (not in old file, not a pre-existing baseline)`);
    }
  }
  return errors;
}

// emission order of css modules for one importer's direct imports + composes
function emissionOrder(directImports, moduleParsed) {
  const seen = new Set(), order = [];
  const emit = (rel) => {
    if (seen.has(rel)) return;
    seen.add(rel);
    const p = moduleParsed.get(rel);
    if (p) for (const st of p.stubs) {
      if (!st.from) continue;
      emit(normRel(join(dirname(rel), st.from)));
    }
    order.push(rel);
  };
  for (const rel of directImports) emit(rel);
  return order;
}
const normRel = (rel) => {
  const parts = [];
  for (const seg of rel.split("/")) {
    if (seg === "..") parts.pop();
    else if (seg !== "." && seg) parts.push(seg);
  }
  return parts.join("/");
};

// find real home (rel, cls) following composes stubs
function realHome(moduleParsed, rel, cls, guard = new Set()) {
  const p = moduleParsed.get(rel);
  if (!p) return null;
  if (p.rules.some((r) => r.selector === `.${cls}`)) return { rel, cls };
  const stub = p.stubs.find((s) => s.cls === cls);
  if (!stub?.from) return null;
  const target = normRel(join(dirname(rel), stub.from));
  const key = `${target}#${stub.composes}`;
  if (guard.has(key)) return null;
  guard.add(key);
  return realHome(moduleParsed, target, stub.composes, guard);
}

function differingLonghands(a, b) {
  const d = [];
  for (const [p, v] of a) if (b.has(p) && b.get(p) !== v) d.push(p);
  return d;
}

// Validate cascade for one importer. For every co-applied pair whose REAL
// homes differ or whose within-module order matters, for each longhand BOTH
// set to DIFFERENT values, the rule that was later in the OLD file for that
// longhand (the required winner) must still win in the emitted output.
function validateCascade({ importer, bindings, classGroups, moduleParsed, oldEff, oldClassSet }) {
  const errors = [];
  const order = emissionOrder(bindings.map((b) => b.module), moduleParsed);
  const emitRank = (rel) => order.indexOf(rel);
  // per-module new effective maps
  const effByMod = new Map();
  const effOf = (rel) => {
    if (!effByMod.has(rel)) effByMod.set(rel, effectiveByClass(moduleParsed.get(rel)));
    return effByMod.get(rel);
  };

  for (const group of classGroups) {
    const homes = new Map();
    for (const cls of group) {
      for (const b of bindings) {
        const h = realHome(moduleParsed, b.module, cls);
        if (h) { homes.set(cls, h); break; }
      }
    }
    const list = [...homes.keys()];
    for (let i = 0; i < list.length; i++) {
      for (let k = i + 1; k < list.length; k++) {
        const ca = list[i], cb = list[k];
        const ha = homes.get(ca), hb = homes.get(cb);
        if (!ha || !hb) continue;
        if (!(oldClassSet.has(ca) && oldClassSet.has(cb))) continue;
        const oa = oldEff.get(ca), ob = oldEff.get(cb);
        const na = effOf(ha.rel).get(ha.cls), nb = effOf(hb.rel).get(hb.cls);
        if (!oa || !ob || !na || !nb) continue;
        for (const p of oa.decls.keys()) {
          if (!ob.decls.has(p)) continue;
          if (oa.decls.get(p) === ob.decls.get(p)) continue; // equal value: order irrelevant
          // required winner for THIS longhand = later old rule setting it
          const winA = oa.propIndex.get(p) > ob.propIndex.get(p);
          const winner = winA ? { cls: ca, home: ha } : { cls: cb, home: hb };
          const loser = winA ? { cls: cb, home: hb } : { cls: ca, home: ha };
          let wins;
          if (winner.home.rel === loser.home.rel) {
            const we = effOf(winner.home.rel).get(winner.cls);
            const le = effOf(loser.home.rel).get(loser.cls);
            wins = (we?.propIndex.get(p) ?? -1) > (le?.propIndex.get(p) ?? -1);
          } else {
            wins = emitRank(winner.home.rel) > emitRank(loser.home.rel);
          }
          if (!wins) {
            errors.push(
              `cascade ${importer}: .${loser.cls} overrides required winner .${winner.cls} on ${p} ` +
              `(${loser.home.rel} emitted before ${winner.home.rel})`,
            );
          }
        }
      }
    }
  }
  return errors;
}

// ------------------------------- main run --------------------------------
function run(ref) {
  const errors = [];
  let oldCss;
  try {
    oldCss = execSync(`git show ${ref}:web/${oldRel}`, { cwd: join(root, "..") }).toString();
  } catch {
    try { oldCss = execSync(`git show ${ref}:${oldRel}`, { cwd: root }).toString(); }
    catch (e) { return [`cannot read old file from ${ref}`]; }
  }
  const oldParsed = parse(oldCss);
  const oldEff = effectiveByClass(oldParsed);
  const oldClassSet = new Set(oldEff.keys());

  const moduleParsed = new Map();
  const baselines = new Map();
  const gitAt = (rel) => execSync(`git show ${ref}:web/${rel}`, { cwd: join(root, "..") }).toString();
  const BASELINE_FILES = new Set([
    "src/features/session/runDetails.module.css",
    "src/features/session/transcript.module.css",
  ]);
  for (const rel of NEW_MODULES) {
    if (!existsSync(join(root, rel))) { errors.push(`missing ${rel}`); continue; }
    moduleParsed.set(rel, parse(readFileSync(join(root, rel), "utf8")));
    if (BASELINE_FILES.has(rel)) baselines.set(rel, parse(gitAt(rel)));
  }

  // 1. rule equivalence (bodies + media + multiplicity). Pre-existing rules
  // in appended-to baseline files (transcript/runDetails .fold etc.) are not
  // part of the migration, so subtract their occurrence counts.
  const baselineKeyCount = new Map();
  for (const bp of baselines.values()) {
    for (const r of bp.rules) {
      const k = JSON.stringify([r.media.join(" | "), r.selector]);
      baselineKeyCount.set(k, (baselineKeyCount.get(k) ?? 0) + 1);
    }
  }
  const allRealCount = new Map();
  for (const [, p] of moduleParsed)
    for (const r of p.rules) {
      const k = JSON.stringify([r.media.join(" | "), r.selector]);
      allRealCount.set(k, (allRealCount.get(k) ?? 0) + 1);
    }
  const eqErrors = [];
  for (const [k, oldRules] of oldParsed.bySel) {
    const classes = [...k.matchAll(/\.([A-Za-z0-9_-]+)/g)].map((m) => m[1]);
    if (classes.length && classes.every((c) => DEAD.has(c))) continue;
    const migrated = (allRealCount.get(k) ?? 0) - (baselineKeyCount.get(k) ?? 0);
    if (migrated !== oldRules.length) {
      eqErrors.push(`rule ${JSON.parse(k)[1]} in [${JSON.parse(k)[0] || "root"}]: multiplicity ${oldRules.length} -> ${migrated}`);
      continue;
    }
    // bodies match as a multiset across the modules that migrated this key
    const got = [];
    for (const [rel, p] of moduleParsed) {
      const baseN = [...(baselines.get(rel)?.rules ?? [])].filter(
        (b) => JSON.stringify([b.media.join(" | "), b.selector]) === k,
      ).length;
      let n = 0;
      for (const r of p.rules)
        if (JSON.stringify([r.media.join(" | "), r.selector]) === k) { if (n++ < baseN) continue; got.push(declKey(r.decls)); }
    }
    const ob = oldRules.map((r) => declKey(r.decls)).sort();
    got.sort();
    for (let i = 0; i < ob.length; i++) if (ob[i] !== got[i]) eqErrors.push(`body changed for ${JSON.parse(k)[1]} in [${JSON.parse(k)[0]}]`);
  }
  errors.push(...eqErrors);
  // no genuinely new rule beyond baselines + dead drop
  for (const [rel, p] of moduleParsed) {
    for (const r of p.rules) {
      const k = JSON.stringify([r.media.join(" | "), r.selector]);
      const isBase = (baselines.get(rel)?.rules ?? []).some((b) => JSON.stringify([b.media.join(" | "), b.selector]) === k);
      if (isBase) continue;
      if (!oldParsed.bySel.has(k)) errors.push(`new rule ${r.selector} in ${rel} (not in old file, not a pre-existing baseline)`);
    }
  }
  for (const [rel, p] of moduleParsed) {
    for (const st of p.stubs) {
      if (!st.cls || !st.composes || !st.from) { errors.push(`${rel}: malformed stub ${st.selector}`); continue; }
      const target = normRel(join(dirname(rel), st.from));
      if (!realHome(moduleParsed, target, st.composes))
        errors.push(`${rel}: stub .${st.cls} chain never reaches a real .${st.composes}`);
    }
  }

  // 3a. dead class tokens in ANY destination selector (pseudo/compound too)
  for (const [rel, p] of moduleParsed) {
    for (const r of p.rules) {
      const toks = [...r.selector.matchAll(/\.([A-Za-z0-9_-]+)/g)].map((m) => m[1]);
      if (toks.some((t) => DEAD.has(t))) errors.push(`${rel}: dead token survives in selector ${r.selector}`);
    }
  }
  // 3b. dead class bindings in TS/TSX
  const tsFiles = execSync("find src -type f \\( -name '*.ts' -o -name '*.tsx' \\)", { cwd: root }).toString().trim().split("\n");
  const deadRe = new RegExp(`\\.(?:${[...DEAD].join("|")})\\b`);
  for (const f of tsFiles) {
    const txt = readFileSync(join(root, f), "utf8");
    for (const m of txt.matchAll(/\b(?:styles|css|session|sessionCss|opt)\.([A-Za-z0-9_]+)/g))
      if (DEAD.has(m[1])) errors.push(`${f}: dead binding .${m[1]}`);
  }

  // 4. keyframes
  for (const [name, want] of Object.entries(EXPECTED_KEYFRAMES)) {
    const got = [...moduleParsed].filter(([, p]) => p.keyframes.has(name)).map(([rel]) => rel).sort();
    if (JSON.stringify(got) !== JSON.stringify([...want].sort()))
      errors.push(`@keyframes ${name}: expected [${want}] got [${got}]`);
  }
  if (existsSync(join(root, oldRel))) errors.push(`${oldRel} still exists`);

  // 5. cascade order over every TS/TSX importer of a css module
  for (const f of tsFiles) {
    const fileAbs = join(root, f);
    const txt = readFileSync(fileAbs, "utf8");
    if (!/\.module\.css/.test(txt)) continue;
    const bindingByName = new Map(); // local binding -> module rel
    for (const im of txt.matchAll(/import\s+(\w+)\s+from\s+"([^"]+\.module\.css)"/g)) {
      bindingByName.set(im[1], normPath(pathResolve(dirname(fileAbs), im[2])).replace(root + "/", ""));
    }
    if (!bindingByName.size) continue;
    // brace-aware className={...} groups, restricted to css-module bindings
    const groups = [];
    for (const mm of txt.matchAll(/className=\{/g)) {
      let depth = 1, i = mm.index + mm[0].length;
      for (; i < txt.length && depth; i++) { if (txt[i] === "{") depth++; else if (txt[i] === "}") depth--; }
      const expr = txt.slice(mm.index, i);
      const cls = [...new Set(
        [...expr.matchAll(/\b([A-Za-z][\w]*)\.([A-Za-z][\w]*)/g)]
          .filter((x) => bindingByName.has(x[1]))
          .map((x) => x[2]),
      )];
      if (cls.length > 1) groups.push(cls);
    }
    if (!groups.length) continue;
    errors.push(...validateCascade({
      importer: f,
      bindings: [...bindingByName].map(([name, module]) => ({ name, module })),
      classGroups: groups,
      moduleParsed,
      oldEff,
      oldClassSet,
    }));
  }

  return { errors, moduleParsed };
}

// ------------------------------- self-test --------------------------------
function selfTest() {
  const fails = [];
  const must = (cond, msg) => { if (!cond) fails.push(msg); };

  // (a) body+media: a rule moved across the media boundary must be rejected.
  const old = parse(`
    .bubble { max-width: 66%; }
    @media (max-width: 767px) { .bubble { max-width: 88%; } }
    .base { color: red; }
    .mod { color: blue; }
  `);
  const eff0 = effectiveByClass(old);
  const swappedMedia = new Map([["m.module.css", parse(`
    .bubble { max-width: 88%; }
    @media (max-width: 767px) { .bubble { max-width: 66%; } }
    .base { color: red; }
    .mod { color: blue; }
  `)]]);
  const e1 = validateRuleEquivalence(old, swappedMedia, new Map());
  must(e1.length > 0, "swapped .bubble media rule must be rejected");

  // (b) body change rejected even with identical selector+media
  const bodyChange = new Map([["m.module.css", parse(`
    .bubble { max-width: 66%; }
    @media (max-width: 767px) { .bubble { max-width: 99%; } }
    .base { color: red; }
    .mod { color: blue; }
  `)]]);
  const e2 = validateRuleEquivalence(old, bodyChange, new Map());
  must(e2.some((x) => x.includes("body changed")), "changed declaration body must be rejected");

  // (c) cascade: modifier .mod must win over .base; a composes graph that
  // emits modifier first must be rejected.
  const mp = new Map();
  mp.set("a/base.module.css", parse(`.base { color: red; }`));
  mp.set("b/mod.module.css", parse(`.mod { color: blue; }`));
  // consumer module stubs the modifier (the ember inversion): its composes
  // dependency emits first.
  mp.set("c/view.module.css", parse(`.mod { composes: mod from "../b/mod.module.css"; }\n.baseReal { }`));
  // rename baseReal->base real rule for clarity: rebuild simply
  mp.set("c/view.module.css", parse(`
    .base { color: red; }
    .mod { composes: mod from "../b/mod.module.css"; }
  `));
  const errs = validateCascade({
    importer: "View.tsx",
    bindings: [{ name: "css", module: "c/view.module.css" }],
    classGroups: [["base", "mod"]],
    moduleParsed: mp,
    oldEff: eff0,
    oldClassSet: new Set(["base", "mod"]),
  });
  must(errs.some((x) => x.includes("overrides required winner")), `inverted composes modifier must be rejected (got ${JSON.stringify(errs)})`);

  // (d) the correct layout (modifier same module, after base) passes
  const ok = new Map([["c/view.module.css", parse(`.base { color: red; }\n.mod { color: blue; }`)]]);
  const errsOk = validateCascade({
    importer: "View.tsx",
    bindings: [{ name: "css", module: "c/view.module.css" }],
    classGroups: [["base", "mod"]],
    moduleParsed: ok,
    oldEff: eff0,
    oldClassSet: new Set(["base", "mod"]),
  });
  must(errsOk.length === 0, `correct base->mod order must pass (got ${JSON.stringify(errsOk)})`);

  if (fails.length) {
    console.error("SELF-TEST FAILED:");
    for (const f of fails) console.error("  - " + f);
    process.exit(1);
  }
  console.log("self-test: all 4 checks passed (media swap, body change, composes inversion, correct order)");
}

// --------------------------------- entry ----------------------------------
if (process.argv[2] === "--self-test") {
  selfTest();
} else {
  const ref = process.argv[2] || "origin/main";
  const { errors } = run(ref);
  if (errors.length) {
    console.error(`UO-4 coverage FAILED (${errors.length}):`);
    for (const e of errors.slice(0, 60)) console.error("  - " + e);
    process.exit(1);
  }
  console.log("UO-4 coverage OK: rules match (media + selector + body), cascade order preserved, dead classes gone, keyframes accounted");
}
