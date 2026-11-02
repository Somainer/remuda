#!/usr/bin/env node
// Web UI coverage self-report (T17 / cb-ui-coverage).
//
//   pnpm --dir web run coverage:ui -- --base origin/main
//   pnpm --dir web run coverage:ui -- --base origin/main --json
//
// Given a git ref, list the web/src files a branch changed and say which
// routes/pages import each one (transitively), which hub specs navigate to
// those routes, and which evidence notes name them. Files no hub spec can
// reach are the UNCOVERED list.
//
// No build: the module graph comes from the TypeScript parser that is
// already a devDependency. Every entry (route table, import graph, specs,
// evidence) is read from the working tree; only the changed-file list comes
// from git.
import { execFileSync } from "node:child_process";
import { existsSync, readFileSync, readdirSync, statSync } from "node:fs";
import { dirname, extname, join, relative, resolve, sep } from "node:path";
import { fileURLToPath } from "node:url";
import ts from "typescript";

const MODULE_EXTS = [".ts", ".tsx", ".mts", ".cts", ".js", ".jsx", ".mjs", ".cjs"];
const NODE_EXTS = [
  ...MODULE_EXTS,
  ".json",
  ".css",
  ".scss",
  ".sass",
  ".less",
  ".svg",
  ".png",
  ".webp",
  ".avif",
  ".woff",
  ".woff2",
  ".wasm",
];
const SCRIPT_KINDS = new Map([
  [".ts", ts.ScriptKind.TS],
  [".tsx", ts.ScriptKind.TSX],
  [".mts", ts.ScriptKind.TS],
  [".cts", ts.ScriptKind.TS],
  [".js", ts.ScriptKind.JS],
  [".jsx", ts.ScriptKind.JSX],
  [".mjs", ts.ScriptKind.JS],
  [".cjs", ts.ScriptKind.JS],
]);
// Unit tests and test support are not UI: a reviewer asking "is this view
// exercised" does not mean "does a component test import it". They are
// reported as ignored instead of polluting UNCOVERED.
const IGNORED_PATTERNS = [
  /\.test\.(?:ts|tsx|mts|cts|js|jsx|mjs|cjs)$/,
  /(?:^|\/)src\/test\//,
  /(?:^|\/)src\/fixtures\//,
  /(?:^|\/)__fixtures__\//,
  /\.d\.ts$/,
];
const CHROME = "(chrome)";

const toPosix = (p) => p.split(sep).join("/");

// ---------------------------------------------------------------------------
// Import scan
// ---------------------------------------------------------------------------

/** Every module specifier in a file: static import/export-from and dynamic import(). */
export function parseModuleSpecifiers(source, fileName = "module.tsx") {
  return parseImportEdges(source, fileName).map((edge) => edge.specifier);
}

/** Specifiers with whether they are a dynamic `import()` edge (route chunks). */
export function parseImportEdges(source, fileName = "module.tsx") {
  const kind = SCRIPT_KINDS.get(extname(fileName)) ?? ts.ScriptKind.TSX;
  const file = ts.createSourceFile(fileName, source, ts.ScriptTarget.Latest, true, kind);
  const edges = [];
  const push = (node, dynamic) => {
    if (node && ts.isStringLiteralLike(node)) {
      edges.push({ specifier: node.text, dynamic });
    }
  };
  const visit = (node) => {
    if (ts.isImportDeclaration(node) || ts.isExportDeclaration(node)) {
      push(node.moduleSpecifier, false);
    } else if (
      ts.isCallExpression(node) &&
      node.expression.kind === ts.SyntaxKind.ImportKeyword
    ) {
      push(node.arguments[0], true);
    }
    ts.forEachChild(node, visit);
  };
  visit(file);
  return edges;
}

/** Resolve a relative/absolute specifier against `root`; null for bare packages. */
export function resolveModule(fromFile, specifier, root) {
  if (!specifier.startsWith(".") && !specifier.startsWith("/")) return null;
  const clean = specifier.replace(/[?#].*$/, "");
  const base = resolve(dirname(fromFile), clean);
  const candidates = [base];
  for (const ext of NODE_EXTS) candidates.push(base + ext);
  for (const ext of NODE_EXTS) candidates.push(join(base, "index" + ext));
  for (const candidate of candidates) {
    try {
      if (existsSync(candidate) && statSync(candidate).isFile()) {
        const rel = toPosix(relative(root, candidate));
        if (rel.startsWith("..")) return null;
        return candidate;
      }
    } catch {
      // unreadable path: treat as unresolved
    }
  }
  return null;
}

/** Recursively list files under `dir` (absolute paths), sorted. */
function walkFiles(dir) {
  const out = [];
  const skip = new Set([
    "node_modules",
    ".git",
    "dist",
    "dist-e2e",
    "test-results",
    "playwright-report",
    "coverage",
    "scratch",
  ]);
  const visit = (current) => {
    for (const entry of readdirSync(current, { withFileTypes: true })) {
      if (skip.has(entry.name)) continue;
      const abs = join(current, entry.name);
      if (entry.isDirectory()) visit(abs);
      else if (entry.isFile()) out.push(abs);
    }
  };
  if (existsSync(dir)) visit(dir);
  return out.sort();
}

/** CSS `@import`, `composes: … from "…"` and relative `url(…)` references. */
export function parseCssSpecifiers(source) {
  const specifiers = [];
  for (const match of source.matchAll(/@import\s+(?:url\(\s*)?["']([^"']+)["']/g)) {
    specifiers.push(match[1]);
  }
  for (const match of source.matchAll(/composes\s*:[^;]*?\bfrom\s+["']([^"']+)["']/g)) {
    specifiers.push(match[1]);
  }
  for (const match of source.matchAll(/url\(\s*["']?([^"')]+)["']?\s*\)/g)) {
    specifiers.push(match[1]);
  }
  return specifiers;
}

/**
 * Import graph over every file under `root`, as
 * Map<absolute, Array<{ target, dynamic }>>. Only local modules
 * (relative/absolute specifiers) become edges, so bare package imports never
 * appear. `dynamic` marks a route-chunk `import()` edge: it matters to a
 * page's reachability but not to what the mounted chrome loads eagerly.
 * Stylesheets contribute their `composes`/`@import` edges too, so a class
 * module reached through another module's `composes` counts as imported.
 */
export function buildImportGraph(root, files = walkFiles(root)) {
  const graph = new Map();
  for (const file of files) graph.set(file, []);
  for (const file of files) {
    const ext = extname(file);
    const isScript = MODULE_EXTS.includes(ext);
    const isStyle = [".css", ".scss", ".sass", ".less"].includes(ext);
    if (!isScript && !isStyle) continue;
    let source;
    try {
      source = readFileSync(file, "utf8");
    } catch {
      continue;
    }
    const edges = graph.get(file);
    const found = isStyle
      ? parseCssSpecifiers(source).map((specifier) => ({ specifier, dynamic: false }))
      : parseImportEdges(source, file);
    for (const edge of found) {
      const target = resolveModule(file, edge.specifier, root);
      if (target) edges.push({ target, dynamic: edge.dynamic });
    }
  }
  return graph;
}

/**
 * Every file reachable from `entry` (including itself). `followDynamic:
 * false` walks only static imports; `stopAt` prunes a subtree at the named
 * modules without including them (route entries are handled by their route).
 */
export function reachableFrom(
  entry,
  graph,
  { followDynamic = true, stopAt = new Set() } = {},
) {
  const seen = new Set();
  const stack = [entry];
  while (stack.length > 0) {
    const file = stack.pop();
    if (seen.has(file)) continue;
    if (stopAt.has(file)) continue;
    seen.add(file);
    for (const edge of graph.get(file) ?? []) {
      if (!followDynamic && edge.dynamic) continue;
      if (!seen.has(edge.target)) stack.push(edge.target);
    }
  }
  return seen;
}

// ---------------------------------------------------------------------------
// Router / lazy-route table
// ---------------------------------------------------------------------------

/** `export const X = createLazyRoute(() => import("..."), "X")` → X ⇒ specifier. */
export function parseLazyRoutes(source) {
  const file = ts.createSourceFile(
    "lazyRoutes.ts",
    source,
    ts.ScriptTarget.Latest,
    true,
    ts.ScriptKind.TS,
  );
  const entries = new Map();
  const visit = (node) => {
    if (
      ts.isVariableDeclaration(node) &&
      ts.isIdentifier(node.name) &&
      node.initializer &&
      ts.isCallExpression(node.initializer) &&
      node.initializer.expression.getText() === "createLazyRoute"
    ) {
      let specifier = null;
      const findImport = (child) => {
        if (
          specifier === null &&
          ts.isCallExpression(child) &&
          child.expression.kind === ts.SyntaxKind.ImportKeyword &&
          child.arguments[0] &&
          ts.isStringLiteralLike(child.arguments[0])
        ) {
          specifier = child.arguments[0].text;
          return;
        }
        ts.forEachChild(child, findImport);
      };
      findImport(node.initializer);
      if (specifier) entries.set(node.name.text, specifier);
    }
    ts.forEachChild(node, visit);
  };
  visit(file);
  return entries;
}

function jsxOpening(node) {
  return ts.isJsxSelfClosingElement(node) ? node : node.openingElement;
}

function jsxTagName(node) {
  const opening = jsxOpening(node);
  return opening?.tagName?.getText() ?? null;
}

function jsxAttribute(node, name) {
  const opening = jsxOpening(node);
  return (
    opening?.attributes.properties.find(
      (prop) => ts.isJsxAttribute(prop) && prop.name.getText() === name,
    ) ?? null
  );
}

function jsxAttributeString(node, name) {
  const attr = jsxAttribute(node, name);
  if (!attr?.initializer) return null;
  let value = attr.initializer;
  if (ts.isJsxExpression(value)) value = value.expression;
  return value && ts.isStringLiteralLike(value) ? value.text : null;
}

/** Component rendered by `element={<Foo .../>}`. */
function jsxElementComponent(node) {
  const attr = jsxAttribute(node, "element");
  if (!attr?.initializer) return null;
  let expression = attr.initializer;
  if (ts.isJsxExpression(expression)) expression = expression.expression;
  if (!expression) return null;
  if (ts.isJsxSelfClosingElement(expression) || ts.isJsxElement(expression)) {
    return jsxTagName(expression);
  }
  return null;
}

function jsxRouteChildren(node) {
  if (!ts.isJsxElement(node)) return [];
  return node.children.filter(
    (child) => ts.isJsxElement(child) || ts.isJsxSelfClosingElement(child),
  );
}

/** Imports declared in one module: local name ⇒ resolved absolute path. */
function moduleImports(absFile, source, root) {
  const kind = SCRIPT_KINDS.get(extname(absFile)) ?? ts.ScriptKind.TSX;
  const file = ts.createSourceFile(
    absFile,
    source,
    ts.ScriptTarget.Latest,
    true,
    kind,
  );
  const imports = new Map();
  for (const statement of file.statements) {
    if (!ts.isImportDeclaration(statement) || !statement.moduleSpecifier) continue;
    if (!ts.isStringLiteralLike(statement.moduleSpecifier)) continue;
    const target = resolveModule(absFile, statement.moduleSpecifier.text, root);
    if (!target) continue;
    const bindings = statement.importClause?.namedBindings;
    if (bindings && ts.isNamedImports(bindings)) {
      for (const element of bindings.elements) imports.set(element.name.text, target);
    } else if (statement.importClause?.name) {
      imports.set(statement.importClause.name.text, target);
    }
  }
  return imports;
}

function joinRoutePath(parent, path) {
  if (path === null || path === undefined) return parent;
  let full;
  if (path.startsWith("/")) full = path;
  else if (parent === "/" || parent === "") full = "/" + path;
  else full = parent.replace(/\/$/, "") + "/" + path;
  full = full.replace(/\/{2,}/g, "/");
  if (full.length > 1) full = full.replace(/\/$/, "");
  return full || "/";
}

/**
 * Route table from router.tsx + lazyRoutes.ts.
 *
 * A Route with children is a layout: its element belongs to the always
 * mounted chrome (Shell, PhoneShell, AuthGate, …), not to one page. A
 * terminal Route's element is the page entry. Redirect routes (`<Navigate>`)
 * and routes whose element is defined in router.tsx itself carry no entry.
 *
 * Returns { routes: [{ pattern, entry }], chrome: [absolute paths] }.
 */
export function parseRouteTable({ routerSource, lazySource, routerFile, lazyFile, root }) {
  const routerImports = moduleImports(routerFile, routerSource, root);
  const lazySpecifiers = lazySource === null ? new Map() : parseLazyRoutes(lazySource);
  const lazyEntries = new Map();
  if (lazySource !== null) {
    for (const [name, specifier] of lazySpecifiers) {
      const target = resolveModule(lazyFile, specifier, root);
      if (target) lazyEntries.set(name, target);
    }
  }

  const file = ts.createSourceFile(
    routerFile,
    routerSource,
    ts.ScriptTarget.Latest,
    true,
    ts.ScriptKind.TSX,
  );

  let routesElement = null;
  const findRoutes = (node) => {
    if (
      routesElement === null &&
      (ts.isJsxElement(node) || ts.isJsxSelfClosingElement(node)) &&
      jsxTagName(node) === "Routes"
    ) {
      routesElement = node;
      return;
    }
    ts.forEachChild(node, findRoutes);
  };
  findRoutes(file);

  const routes = [];
  const chrome = new Set([routerFile]);

  const resolveElement = (name) => {
    if (!name) return null;
    const target = routerImports.get(name);
    if (!target) return null;
    if (lazyEntries.has(name)) return lazyEntries.get(name);
    return target;
  };

  const walk = (node, parentPattern) => {
    if (jsxTagName(node) !== "Route") return;
    const index = jsxAttribute(node, "index") !== null;
    const path = jsxAttributeString(node, "path");
    const children = jsxRouteChildren(node);
    const pattern = index ? parentPattern : joinRoutePath(parentPattern, path);
    const component = jsxElementComponent(node);
    const entry = resolveElement(component);

    if (component === "Navigate") {
      // redirect only
    } else if (children.length > 0) {
      // layout route: its mounted tree is chrome
      if (entry) chrome.add(entry);
    } else if (entry && pattern !== "*") {
      routes.push({ pattern, entry });
    }

    for (const child of children) walk(child, pattern);
  };

  const top = routesElement
    ? ts.isJsxElement(routesElement)
      ? routesElement.children.filter(
          (child) => ts.isJsxElement(child) || ts.isJsxSelfClosingElement(child),
        )
      : [routesElement]
    : [];
  for (const node of top) walk(node, "");

  return { routes, chrome: [...chrome] };
}

// ---------------------------------------------------------------------------
// Route matching
// ---------------------------------------------------------------------------

const META_SEGMENT = /[[\](){}*+?\\^$|]|\.\*/;

/** Decode a regex literal body (`/\/s\//`) into a path-ish string. */
export function decodeRegexLiteral(text) {
  if (!text.startsWith("/")) return null;
  let end = text.length;
  while (end > 1) {
    const slash = text.lastIndexOf("/", end - 1);
    if (slash <= 0) break;
    // an escaped slash does not close the literal
    let backslashes = 0;
    for (let i = slash - 1; i >= 0 && text[i] === "\\"; i -= 1) backslashes += 1;
    if (backslashes % 2 === 0) {
      end = slash;
      break;
    }
    end = slash;
  }
  if (end <= 0 || end === text.length) return null;
  return text.slice(1, end).replace(/\\(.)/g, "$1");
}

/**
 * Turn a path-shaped string into a comparable path: template holes and
 * regex tokens collapse to `:param`, anchors/query/flags are dropped.
 * Returns null for anything that is not a path.
 */
export function normalizeRouteCandidate(raw) {
  if (typeof raw !== "string") return null;
  let text = raw.trim().replace(/^[`"']+|[`"']+$/g, "");
  text = text.replace(/\$\{[^}]*\}/g, ":param");
  text = text.replace(/\\(.)/g, "$1");
  text = text.replace(/^\^+/, "");
  text = text.split(/[?#]/)[0];
  if (!text.startsWith("/")) return null;
  text = text.replace(/\/{2,}/g, "/");
  if (text.length > 1) text = text.replace(/\/+$/, "");
  // A regex anchor after the path (the literal itself was already decoded).
  text = text.replace(/\$+$/, "");
  return text;
}

function segments(path) {
  return path.split("/").filter(Boolean).map((segment) => {
    if (segment === "*") return "*";
    if (segment.startsWith(":") || META_SEGMENT.test(segment)) return ":param";
    return segment;
  });
}

/** Does a concrete path match a router pattern (`/s/:id/tty`)? */
export function routeMatches(pattern, candidate) {
  const want = segments(normalizeRouteCandidate(pattern));
  const got = segments(normalizeRouteCandidate(candidate));
  if (want === null || got === null) return false;
  for (let i = 0; i < want.length; i += 1) {
    const part = want[i];
    if (part === "*") return true;
    if (i >= got.length) return false;
    if (part === ":param") continue;
    if (part !== got[i]) return false;
  }
  return want.length === got.length;
}

function specificity(pattern) {
  const parts = segments(pattern);
  const params = parts.filter((part) => part === ":param").length;
  return [parts.filter((part) => part !== ":param" && part !== "*").length, -params];
}

/** Best (most specific) matching route pattern, or null. */
export function matchRoute(candidate, patterns) {
  const matches = patterns.filter((pattern) => routeMatches(pattern, candidate));
  if (matches.length === 0) return null;
  matches.sort((a, b) => {
    const [aStatic, aParams] = specificity(a);
    const [bStatic, bParams] = specificity(b);
    return bStatic - aStatic || bParams - aParams || a.localeCompare(b);
  });
  return matches[0];
}

/** Route patterns named by prose/source text (string, template and regex literals). */
export function routesNamedInText(text, patterns) {
  const candidates = [];
  const file = ts.createSourceFile(
    "probe.ts",
    text,
    ts.ScriptTarget.Latest,
    true,
    ts.ScriptKind.TSX,
  );
  const visit = (node) => {
    if (ts.isStringLiteral(node) || ts.isNoSubstitutionTemplateLiteral(node)) {
      candidates.push(node.text);
    } else if (ts.isTemplateExpression(node)) {
      let value = node.head.text;
      for (const span of node.templateSpans) value += ":param" + span.literal.text;
      candidates.push(value);
    } else if (ts.isRegularExpressionLiteral(node)) {
      const decoded = decodeRegexLiteral(node.getText());
      if (decoded !== null) candidates.push(decoded);
    } else if (
      ts.isBinaryExpression(node) &&
      node.operatorToken.kind === ts.SyntaxKind.PlusToken
    ) {
      // "/s/" + id + "/tty" → /s/:param/tty
      let value = "";
      const flatten = (child) => {
        if (ts.isBinaryExpression(child) && child.operatorToken.kind === ts.SyntaxKind.PlusToken) {
          flatten(child.left);
          flatten(child.right);
        } else if (ts.isStringLiteral(child) || ts.isNoSubstitutionTemplateLiteral(child)) {
          value += child.text;
        } else if (ts.isNumericLiteral(child)) {
          value += child.text;
        } else {
          value += ":param";
        }
      };
      flatten(node);
      candidates.push(value);
    }
    ts.forEachChild(node, visit);
  };
  visit(file);
  return routesForCandidates(candidates, patterns);
}

/** Markdown evidence notes: inline code spans and link targets. */
export function routesNamedInMarkdown(text, patterns) {
  const candidates = [];
  for (const match of text.matchAll(/`([^`\n]+)`/g)) candidates.push(match[1]);
  for (const match of text.matchAll(/\]\((\/[^)\s]+)\)/g)) candidates.push(match[1]);
  return routesForCandidates(candidates, patterns);
}

/** Does a path pattern have `candidate` as a segment prefix (`/s/:id` ⊂ `/s/:id/files`)? */
function isPrefixOfPattern(candidate, pattern) {
  const got = segments(normalizeRouteCandidate(candidate));
  const want = segments(normalizeRouteCandidate(pattern));
  if (got === null || want === null || got.length > want.length) return false;
  for (let i = 0; i < got.length; i += 1) {
    if (want[i] === "*") return true;
    if (want[i] === ":param") continue;
    if (want[i] !== got[i]) return false;
  }
  return true;
}

function routesForCandidates(candidates, patterns) {
  const found = new Set();
  for (const raw of candidates) {
    let normalized = normalizeRouteCandidate(raw);
    if (normalized === null) continue;
    // Evidence notes often write `/s/:id*` for "the session view and below".
    const wildcard = normalized.length > 1 && normalized.endsWith("*");
    if (wildcard) normalized = normalized.slice(0, -1).replace(/\/+$/, "") || "/";
    for (const pattern of patterns) {
      if (routeMatches(pattern, normalized)) found.add(pattern);
      else if (wildcard && isPrefixOfPattern(normalized, pattern)) found.add(pattern);
    }
  }
  return found;
}

// ---------------------------------------------------------------------------
// Report assembly
// ---------------------------------------------------------------------------

function git(root, args) {
  return execFileSync("git", ["-C", root, ...args], { encoding: "utf8" }).trim();
}

/** Changed web/src-relative files between `<base>...HEAD`. */
export function changedFiles(root, base, changedFrom = null) {
  if (changedFrom) {
    return readFileSync(changedFrom, "utf8")
      .split("\n")
      .map((line) => line.trim())
      .filter(Boolean)
      .sort();
  }
  const top = git(root, ["rev-parse", "--show-toplevel"]);
  const prefix = git(root, ["rev-parse", "--show-prefix"]);
  const merged = (() => {
    try {
      return git(root, ["merge-base", base, "HEAD"]);
    } catch {
      return null;
    }
  })();
  const range = merged ? `${base}...HEAD` : base;
  const output = git(top, ["diff", "--name-only", "--diff-filter=ACMR", range, "--", `${prefix}src`]);
  return output
    .split("\n")
    .filter(Boolean)
    .map((path) => path.slice(prefix.length))
    .sort();
}

export function loadSpecRoutes(root, patterns) {
  const specsDir = join(root, "tests", "e2e");
  const specs = walkFiles(specsDir).filter((file) => file.endsWith(".hub.spec.ts"));
  const bySpec = new Map();
  for (const file of specs) {
    const routes = routesNamedInText(readFileSync(file, "utf8"), patterns);
    bySpec.set(relative(root, file), routes);
  }
  return bySpec;
}

export function loadEvidenceRoutes(root, patterns) {
  // `root` is web/; evidence notes live at the repository's docs/.
  const repoRoot = resolve(root, "..");
  const evidenceDir = join(repoRoot, "docs", "design", "evidence");
  const notes = walkFiles(evidenceDir).filter((file) => file.endsWith(".md"));
  const byNote = new Map();
  for (const file of notes) {
    const routes = routesNamedInMarkdown(readFileSync(file, "utf8"), patterns);
    byNote.set(relative(repoRoot, file), routes);
  }
  return byNote;
}

/**
 * Build the whole report. `root` is the web/ directory.
 * Returns { base, files, uncovered, ignored, routes }.
 */
export function buildReport({
  root = process.cwd(),
  base = "origin/main",
  changedFrom = null,
  changed = null,
} = {}) {
  const srcDir = join(root, "src");
  const routerFile = join(srcDir, "app", "router.tsx");
  const lazyFile = join(srcDir, "app", "lazyRoutes.ts");
  const routerSource = readFileSync(routerFile, "utf8");
  const lazySource = existsSync(lazyFile) ? readFileSync(lazyFile, "utf8") : null;
  const { routes, chrome } = parseRouteTable({
    routerSource,
    lazySource,
    routerFile,
    lazyFile,
    root,
  });

  const patterns = [...new Set(routes.map((route) => route.pattern))];
  const graph = buildImportGraph(root);

  // module → route patterns that reach it, plus whether chrome reaches it
  const reach = new Map();
  const addReach = (file, pattern) => {
    if (!reach.has(file)) reach.set(file, new Set());
    reach.get(file).add(pattern);
  };
  for (const route of routes) {
    for (const file of reachableFrom(route.entry, graph)) addReach(file, route.pattern);
  }
  // App bootstrap (main.tsx → Root.tsx) is mounted on every page too. The
  // chrome walk is static-only and stops at route entries: a page chunk that
  // a shell lazily imports is not mounted on the other routes.
  const chromeRoots = new Set(chrome);
  for (const extra of [join(srcDir, "main.tsx"), join(srcDir, "app", "Root.tsx")]) {
    if (existsSync(extra)) chromeRoots.add(extra);
  }
  const routeEntries = new Set(routes.map((route) => route.entry));
  for (const entry of chromeRoots) {
    for (const file of reachableFrom(entry, graph, {
      followDynamic: false,
      stopAt: routeEntries,
    })) {
      addReach(file, CHROME);
    }
  }

  const specRoutes = loadSpecRoutes(root, patterns);
  const evidenceRoutes = loadEvidenceRoutes(root, patterns);

  const changedFilesList = changed ?? changedFiles(root, base, changedFrom);
  const files = [];
  const ignored = [];
  for (const file of changedFilesList) {
    if (IGNORED_PATTERNS.some((pattern) => pattern.test(file))) {
      ignored.push(file);
      continue;
    }
    const abs = join(root, file);
    const fileRoutes = reach.get(abs) ?? new Set();
    const routeList = [...fileRoutes].sort((a, b) => {
      if (a === CHROME) return 1;
      if (b === CHROME) return -1;
      return a.localeCompare(b);
    });
    const covers = (namedRoutes) =>
      [...namedRoutes].some((route) => fileRoutes.has(route)) ||
      (fileRoutes.has(CHROME) && [...namedRoutes].length > 0);
    const specs = [...specRoutes]
      .filter(([, named]) => covers(named))
      .map(([name]) => name)
      .sort();
    const evidence = [...evidenceRoutes]
      .filter(([, named]) => covers(named))
      .map(([name]) => name)
      .sort();
    files.push({
      file,
      routes: routeList,
      specs,
      evidence,
      covered: specs.length > 0,
    });
  }

  return {
    base,
    files,
    uncovered: files.filter((entry) => !entry.covered).map((entry) => entry.file),
    ignored,
    routes: patterns,
  };
}

function markdownCell(values, limit = 8) {
  if (values.length === 0) return "—";
  const shown = values.slice(0, limit);
  const rest = values.length - shown.length;
  return shown.join("<br>") + (rest > 0 ? `<br>… +${rest} more` : "");
}

export function renderMarkdown(report) {
  const lines = [];
  lines.push(`# Web UI coverage vs ${report.base}`);
  lines.push("");
  lines.push(
    `${report.files.length} changed UI file(s); ${report.files.filter((f) => f.covered).length} reached by a hub spec, ` +
      `${report.uncovered.length} uncovered.`,
  );
  if (report.ignored.length > 0) {
    lines.push(
      `${report.ignored.length} unit-test/support file(s) ignored (not UI targets).`,
    );
  }
  lines.push("");
  lines.push("| File | Routes | Hub specs | Evidence |");
  lines.push("| --- | --- | --- | --- |");
  for (const entry of report.files) {
    lines.push(
      `| ${entry.file} | ${markdownCell(entry.routes)} | ${markdownCell(entry.specs)} | ${markdownCell(entry.evidence)} |`,
    );
  }
  lines.push("");
  lines.push("## UNCOVERED");
  lines.push("");
  lines.push(
    "No hub spec navigates a route whose module graph reaches these files:",
  );
  lines.push("");
  if (report.uncovered.length === 0) lines.push("none");
  else for (const file of report.uncovered) lines.push(`- ${file}`);
  lines.push("");
  return lines.join("\n");
}

function parseArgs(argv) {
  const args = {
    base: "origin/main",
    json: false,
    root: process.cwd(),
    changedFrom: null,
  };
  for (let i = 0; i < argv.length; i += 1) {
    const arg = argv[i];
    // pnpm/npm forward the `--` separator when script args follow it.
    if (arg === "--") continue;
    if (arg === "--base") args.base = argv[++i] ?? args.base;
    else if (arg.startsWith("--base=")) args.base = arg.slice("--base=".length);
    else if (arg === "--json") args.json = true;
    else if (arg === "--root") args.root = resolve(argv[++i]);
    else if (arg === "--changed-from") args.changedFrom = resolve(argv[++i]);
    else if (arg === "--help" || arg === "-h") args.help = true;
    else throw new Error(`unknown argument: ${arg}`);
  }
  return args;
}

const HELP = `usage: pnpm --dir web run coverage:ui -- [--base <ref>] [--json]

  --base <ref>        committed ref to compare against (default: origin/main)
  --json              machine-readable report instead of a markdown table
  --root <dir>        web/ directory to analyse (default: cwd)
  --changed-from <f>  read changed paths from a file instead of git (testing)
`;

if (
  process.argv[1] &&
  resolve(process.argv[1]) === resolve(fileURLToPath(import.meta.url))
) {
  const args = parseArgs(process.argv.slice(2));
  if (args.help) {
    process.stdout.write(HELP);
  } else {
    const report = buildReport({
      root: args.root,
      base: args.base,
      changedFrom: args.changedFrom,
    });
    process.stdout.write(
      args.json ? JSON.stringify(report, null, 2) + "\n" : renderMarkdown(report),
    );
  }
}
