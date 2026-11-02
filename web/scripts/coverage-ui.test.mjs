import { readFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { describe, expect, it } from "vitest";
import {
  buildImportGraph,
  buildReport,
  decodeRegexLiteral,
  matchRoute,
  normalizeRouteCandidate,
  parseCssSpecifiers,
  parseLazyRoutes,
  parseModuleSpecifiers,
  parseRouteTable,
  reachableFrom,
  renderMarkdown,
  resolveModule,
  routeMatches,
  routesNamedInMarkdown,
  routesNamedInText,
} from "./coverage-ui.mjs";

const here = dirname(fileURLToPath(import.meta.url));
const fixture = join(here, "__fixtures__", "ui-coverage");
const web = join(fixture, "web");
const src = join(web, "src");

function read(rel) {
  return readFileSync(join(src, rel), "utf8");
}

function table() {
  const routerFile = join(src, "app", "router.tsx");
  const lazyFile = join(src, "app", "lazyRoutes.ts");
  return parseRouteTable({
    routerSource: readFileSync(routerFile, "utf8"),
    lazySource: readFileSync(lazyFile, "utf8"),
    routerFile,
    lazyFile,
    root: web,
  });
}

const patterns = () => [...new Set(table().routes.map((route) => route.pattern))];

describe("import scan", () => {
  it("finds static imports, re-exports and dynamic imports", () => {
    const source = `
      import a from "./a";
      import { b } from "./b";
      export { c } from "./c";
      export * from "./d";
      const e = () => import("./e");
      const f = "not an import";
    `;
    expect(parseModuleSpecifiers(source, "probe.ts")).toEqual([
      "./a",
      "./b",
      "./c",
      "./d",
      "./e",
    ]);
  });

  it("resolves extensions, directory indexes and strips query suffixes", () => {
    const from = join(src, "pages", "FooPage.tsx");
    expect(resolveModule(from, "../features/foo/Foo", web)).toBe(
      join(src, "features", "foo", "Foo.tsx"),
    );
    expect(resolveModule(from, "../features/foo/helper.ts", web)).toBe(
      join(src, "features", "foo", "helper.ts"),
    );
    expect(resolveModule(from, "../features/foo/foo.module.css?raw", web)).toBe(
      join(src, "features", "foo", "foo.module.css"),
    );
    expect(resolveModule(from, "react", web)).toBeNull();
    expect(resolveModule(from, "../../../outside", web)).toBeNull();
  });

  it("reads composes, @import and relative url() from stylesheets", () => {
    const specs = parseCssSpecifiers(`
      @import "./theme.css";
      .a { composes: base from "./base.module.css"; }
      .b { background: url("../img/x.png"); }
    `);
    expect(specs).toEqual(["./theme.css", "./base.module.css", "../img/x.png"]);
  });

  it("builds a transitive graph, including dynamic and composes edges", () => {
    const graph = buildImportGraph(web);
    const fooPage = join(src, "pages", "FooPage.tsx");
    const reached = reachableFrom(fooPage, graph);
    expect(reached.has(join(src, "features", "foo", "Foo.tsx"))).toBe(true);
    expect(reached.has(join(src, "features", "foo", "helper.ts"))).toBe(true);
    // Foo.tsx imports foo.module.css, which composes base.module.css.
    expect(reached.has(join(src, "features", "foo", "base.module.css"))).toBe(true);

    // BarPage → Bar → dynamic import("./Lazy")
    expect(reached.has(join(src, "features", "bar", "Lazy.tsx"))).toBe(false);
    const barPage = join(src, "pages", "BarPage.tsx");
    expect(reachableFrom(barPage, graph).has(join(src, "features", "bar", "Lazy.tsx"))).toBe(
      true,
    );
  });

  it("can ignore dynamic edges and prune route entries", () => {
    const graph = buildImportGraph(web);
    const shell = join(src, "app", "Shell.tsx");
    const staticOnly = reachableFrom(shell, graph, { followDynamic: false });
    expect(staticOnly.has(join(src, "app", "lazyRoutes.ts"))).toBe(true);
    expect(staticOnly.has(join(src, "pages", "FooPage.tsx"))).toBe(false);

    const pruned = reachableFrom(shell, graph, {
      followDynamic: false,
      stopAt: new Set([join(src, "app", "lazyRoutes.ts")]),
    });
    expect(pruned.has(join(src, "app", "lazyRoutes.ts"))).toBe(false);
  });

  it("maps lazy route exports to their page modules", () => {
    const entries = parseLazyRoutes(read("app/lazyRoutes.ts"));
    expect(entries.get("FooRoute")).toBe("../pages/FooPage");
    expect(entries.get("BarRoute")).toBe("../pages/BarPage");
    expect(entries.get("HomeRoute")).toBe("../pages/HomePage");
  });
});

describe("route table", () => {
  it("joins nested paths and resolves lazy and direct page entries", () => {
    const routes = table().routes;
    const byPattern = new Map(routes.map((route) => [route.pattern, route.entry]));
    expect([...byPattern.keys()].sort()).toEqual([
      "/bar",
      "/detail/:id",
      "/foo",
      "/gated",
      "/login",
      "/m",
      "/s/:id",
      "/s/:id/files",
    ]);
    expect(byPattern.get("/foo")).toBe(join(src, "pages", "FooPage.tsx"));
    expect(byPattern.get("/login")).toBe(join(src, "pages", "LoginPage.tsx"));
    expect(byPattern.get("/m")).toBe(join(src, "pages", "HomePage.tsx"));
    // A route rendered by a component defined in router.tsx has no page entry.
    expect(byPattern.has("/")).toBe(false);
    // <Navigate> is a redirect, not a view.
    expect(byPattern.has("/m/inbox")).toBe(false);
  });

  it("treats layout-route elements as chrome, not pages", () => {
    const { chrome } = table();
    expect(chrome).toContain(join(src, "app", "AuthGate.tsx"));
    expect(chrome).toContain(join(src, "app", "Shell.tsx"));
    expect(chrome).toContain(join(src, "app", "PhoneShell.tsx"));
    expect(chrome).not.toContain(join(src, "pages", "FooPage.tsx"));
  });
});

describe("route matching", () => {
  it("normalizes template holes, regex escapes, anchors and queries", () => {
    expect(normalizeRouteCandidate("/s/${id}/tty")).toBe("/s/:param/tty");
    expect(normalizeRouteCandidate("/m$/")).toBe("/m");
    expect(normalizeRouteCandidate("/sessions?scope=all")).toBe("/sessions");
    expect(decodeRegexLiteral("/\\/s\\/[^/]+\\/files$/")).toBe("/s/[^/]+/files$");
    expect(normalizeRouteCandidate("not-a-path")).toBeNull();
  });

  it("matches concrete paths against parameter and wildcard patterns", () => {
    expect(routeMatches("/s/:instanceId/tty", "/s/abc/tty")).toBe(true);
    expect(routeMatches("/s/:instanceId/tty", "/s/abc/files")).toBe(false);
    expect(routeMatches("/s/:instanceId", "/s/abc/files")).toBe(false);
    expect(routeMatches("/files/*", "/files/a/b")).toBe(true);
    expect(routeMatches("/hosts/:hostId", "/hosts/h-1")).toBe(true);
    expect(routeMatches("/hosts", "/hosts/h-1")).toBe(false);
  });

  it("picks the most specific route", () => {
    const all = patterns();
    expect(matchRoute("/s/abc/files", all)).toBe("/s/:id/files");
    expect(matchRoute("/s/abc", all)).toBe("/s/:id");
    expect(matchRoute("/m", all)).toBe("/m");
    expect(matchRoute("/m/inbox", all)).toBeNull();
    expect(matchRoute("/v1/hosts", all)).toBeNull();
  });

  it("extracts routes from spec navigation strings and regexes", () => {
    const source = readFileSync(join(web, "tests", "e2e", "foo.hub.spec.ts"), "utf8");
    expect([...routesNamedInText(source, patterns())].sort()).toEqual([
      "/detail/:id",
      "/foo",
      "/m",
    ]);
  });

  it("extracts routes from evidence-note code spans and link targets", () => {
    const source = readFileSync(
      join(fixture, "docs", "design", "evidence", "foo-1.md"),
      "utf8",
    );
    expect([...routesNamedInMarkdown(source, patterns())].sort()).toEqual([
      "/detail/:id",
      "/foo",
      "/m",
    ]);
  });

  it("treats a trailing star in an evidence route as 'this view and below'", () => {
    expect([...routesNamedInMarkdown("`/s/:id*`", patterns())].sort()).toEqual([
      "/s/:id",
      "/s/:id/files",
    ]);
  });
});

describe("report", () => {
  const changed = [
    "src/features/foo/Foo.tsx",
    "src/features/foo/base.module.css",
    "src/features/shell/chromeHelper.ts",
    "src/features/foo/Dead.tsx",
    "src/features/foo/Foo.test.ts",
  ];
  const report = () => buildReport({ root: web, base: "HEAD", changed });

  it("maps files to routes, hub specs and evidence notes", () => {
    const byFile = new Map(report().files.map((entry) => [entry.file, entry]));
    expect(byFile.get("src/features/foo/Foo.tsx").routes).toEqual(["/foo"]);
    expect(byFile.get("src/features/foo/Foo.tsx").specs).toEqual([
      "tests/e2e/foo.hub.spec.ts",
    ]);
    expect(byFile.get("src/features/foo/Foo.tsx").evidence).toEqual([
      "docs/design/evidence/foo-1.md",
    ]);
    // Reached through another module's `composes`.
    expect(byFile.get("src/features/foo/base.module.css").routes).toEqual(["/foo"]);
  });

  it("marks the always-mounted chrome and lets any navigating spec cover it", () => {
    const chrome = report().files.find(
      (entry) => entry.file === "src/features/shell/chromeHelper.ts",
    );
    expect(chrome.routes).toEqual(["(chrome)"]);
    expect(chrome.specs).toEqual(["tests/e2e/foo.hub.spec.ts"]);
    expect(chrome.covered).toBe(true);
  });

  it("lists only files no hub spec reaches as uncovered", () => {
    const result = report();
    expect(result.uncovered).toEqual(["src/features/foo/Dead.tsx"]);
    expect(result.ignored).toEqual(["src/features/foo/Foo.test.ts"]);
  });

  it("renders a deterministic markdown table with the uncovered list", () => {
    const markdown = renderMarkdown(report());
    expect(markdown).toContain("| File | Routes | Hub specs | Evidence |");
    expect(markdown).toContain("## UNCOVERED");
    expect(markdown).toContain("- src/features/foo/Dead.tsx");
    expect(markdown).not.toContain("- src/features/foo/Foo.tsx");
    expect(renderMarkdown(report())).toBe(markdown);
  });
});
