//! The machine half of the agent-parity ledger (D-060 decision 12).
//!
//! `docs/design/agent-parity.md` is the human half: one row per PWA action and
//! per clap subcommand, with the class a person reads. This module generates
//! the machine half beside it and checks the two against each other:
//!
//! - **Routes** come from the Hub's own registrations: every `.route(...)`
//!   under `crates/remuda-hub/src` (recursively, so nested modules count),
//!   unioned with the documented OpenAPI paths (which alone cover the
//!   nest-mounted push carrier). Never a hand-kept route list.
//! - **Agent admission** is the verdict of
//!   [`remuda_hub::agent_route_admission`] — the same predicate
//!   `restrict_agent_routes` runs on every request — evaluated on a concrete
//!   path. It is never re-derived by reading the middleware's source.
//! - **Handler gates** are read from the handler's own source: the scanner
//!   follows same-module helper calls (`require_dispatch`,
//!   `require_project_scope`, …) two hops and reports `operator-only`, the
//!   named grant, or `scope`. Anything it cannot read is `handler-checked`,
//!   never a guess.
//! - **Commands** are the clap tree (`Command::get_subcommands`). The Hub
//!   route a subcommand calls is left empty: the clap registry carries an
//!   argv shape, not the REST calls the command makes, so inferring them from
//!   string literals in the module would be a guess. The human half's Hub
//!   route column is where that mapping is recorded.
//!
//! The committed file is JSON rather than a markdown table: it is the
//! machine's copy, the drift check compares bytes for staleness, and a JSON
//! document has one canonical serialization (no escaping or column-width
//! ambiguity) for a reader as well as for this module.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde::Serialize;

/// The committed machine half.
pub const GENERATED_FILE: &str = "docs/design/agent-parity.generated.json";
/// The human half.
pub const HUMAN_FILE: &str = "docs/design/agent-parity.md";
/// Sections whose bullets are an explicit, commented "no human row" list.
pub const ALLOWLIST_SECTIONS: [&str; 2] = [
    "## Hub routes no human action calls",
    "## Not agent actions",
];

/// Rows whose Today cell begins `Agent route` but cannot carry one verdict,
/// because the cell explains several paths and only some are admitted. Keyed
/// by the row's first cell, exactly as written. A row not listed here must
/// yield a claim for every route it names (see `drift`).
///
/// - `Follow the session live`: the follow socket is refused; the journal is
///   the admitted path.
/// - `Create a worktree for it`: the raw worktree routes are operator-only; a
///   task binding or a dispatch leases the worktree.
/// - `List pending items`: the list is admitted, the settlement frames ride
///   the refused follow socket.
/// - `` `instance create` ``: the create is admitted, the host inventory it
///   falls back to is not.
/// - `` `instance send` ``: the send is admitted, its follow-up command read is
///   refused.
/// - `` `gate <branch>` (verify) ``: enqueue and wait are admitted, the failure
///   log is not.
/// - `` `gate list` ``: with `--project` it is admitted, without it
///   `GET /v1/gate/jobs` is not.
/// - `` `watch` `` and `` `report` ``: the worker read is admitted, the
///   cross-project gate read is not.
const AGENT_ROUTE_EXEMPTIONS: [&str; 9] = [
    "Follow the session live",
    "Create a worktree for it",
    "List pending items",
    "`instance create`",
    "`instance send`",
    "`gate <branch>` (verify)",
    "`gate list`",
    "`watch`",
    "`report`",
];

const METHODS: [&str; 5] = ["get", "post", "put", "patch", "delete"];

/// One Hub route, as the machine half records it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ParityRoute {
    /// Upper-case HTTP method.
    pub method: String,
    /// The registered path, `{id}` templates included.
    pub path: String,
    /// What the middleware does with an Agent-origin request: `admitted`,
    /// `owner-scoped`, `refused` or `skipped`.
    pub middleware: String,
    /// The ownership a `owner-scoped` route is pinned to: `instance`,
    /// `lineage` or `worker`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub owned: Option<String>,
    /// The handler gate the source scan could read: `operator-only`,
    /// `grant: dispatch`, `scope`, their `; `-joined combination, or
    /// `handler-checked`.
    pub handler: String,
}

/// One clap command path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ParityCommand {
    /// Space-joined clap path, e.g. `instance read`.
    pub path: String,
    /// Whether the command has subcommands of its own.
    pub group: bool,
    /// The Hub routes the command calls; always empty here (see the module
    /// notes and `about`).
    pub hub_routes: Vec<String>,
}

/// The machine half as a whole; [`render`] is its canonical serialization.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ParityModel {
    /// One sentence naming the generator and its sources.
    pub about: String,
    /// How to read the file, and what each column can and cannot claim.
    pub notes: Vec<String>,
    /// Every Hub route, sorted by method, then path.
    pub routes: Vec<ParityRoute>,
    /// Every clap path, sorted.
    pub commands: Vec<ParityCommand>,
}

/// The repository root, from this crate's manifest directory.
#[must_use]
pub fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// The model built from the live tree: Hub route registrations, the OpenAPI
/// document and the clap tree.
pub fn machine_model() -> Result<ParityModel, String> {
    let root = repo_root();
    let sources = read_hub_sources(&root.join("crates/remuda-hub/src"))?;
    let openapi = std::fs::read_to_string(root.join("crates/remuda-hub/openapi/openapi.json"))
        .map_err(|error| format!("cannot read the Hub OpenAPI document: {error}"))?;
    let documented = openapi_routes(&openapi)?;
    Ok(build_model(&sources, &documented, &crate::cli_command()))
}

/// Build a model from its three inputs. Exposed so fixtures can drive the
/// scanner and the drift check without the live tree.
#[must_use]
pub fn build_model(
    sources: &[(String, String)],
    documented: &BTreeSet<(String, String)>,
    clap: &clap::Command,
) -> ParityModel {
    let scanned = scan_route_registrations(sources);
    let index = index_functions(sources);
    let mut keys: BTreeSet<(String, String)> = scanned.keys().cloned().collect();
    keys.extend(documented.iter().cloned());
    let routes = keys
        .into_iter()
        .map(|(method, path)| {
            let (middleware, owned) = middleware_verdict(&method, &path);
            let handler = scanned.get(&(method.clone(), path.clone())).map_or_else(
                || "handler-checked".to_owned(),
                |target| handler_gate(&index, target),
            );
            ParityRoute {
                method,
                path,
                middleware,
                owned,
                handler,
            }
        })
        .collect();
    ParityModel {
        about: "Generated from the Hub route registrations and agent_route_admission \
                (crates/remuda-hub), the documented OpenAPI, and the remuda clap tree. \
                Do not edit; run `cargo run -p remuda --example gen_parity`."
            .to_owned(),
        notes: vec![
            "routes: every `.route(..)` registration under crates/remuda-hub/src (recursively), \
             unioned with the documented OpenAPI paths; the latter alone carries the \
             nest-mounted /push carrier."
                .to_owned(),
            "middleware: the verdict of remuda_hub::agent_route_admission, the \
             predicate restrict_agent_routes calls for every Agent-origin request. `admitted` \
             means the middleware passes it to the handler; `owner-scoped` adds the named \
             ownership check; `refused` is the middleware's 403; `skipped` means the middleware \
             resolves no caller (a login, Node-link or health path, the Node object pull, or \
             the Node host-file staging endpoint) and the handler decides."
                .to_owned(),
            "handler: a gate the handler calls unconditionally at its top level, or one a \
             same-module helper it calls unconditionally does (at most two hops). A gate inside \
             a branch, or one the scan cannot follow, is `handler-checked`: the scan reports \
             only what the source states, and `handler-checked` is not a claim that the handler \
             admits the route."
                .to_owned(),
            "commands: the clap tree from Command::get_subcommands. hubRoutes is empty because \
             the clap registry records an argv shape, not the REST calls a command makes; the \
             human half's Hub route column is that record."
                .to_owned(),
        ],
        routes,
        commands: clap_commands(clap),
    }
}

/// The canonical bytes of the committed machine half.
#[must_use]
pub fn render(model: &ParityModel) -> String {
    let mut rendered = serde_json::to_string_pretty(model).expect("the model serializes");
    rendered.push('\n');
    rendered
}

/// `--check`: the committed file equals the model's canonical bytes.
pub fn check_generated(model: &ParityModel, committed: &str) -> Result<(), String> {
    if committed == render(model) {
        Ok(())
    } else {
        Err(format!(
            "{GENERATED_FILE} is stale; regenerate it with \
             `cargo run -p remuda --example gen_parity`"
        ))
    }
}

/// The drift between the machine model and the human half. Every message names
/// the row or route and the file to fix.
#[must_use]
pub fn drift(model: &ParityModel, human: &str) -> Vec<String> {
    let mut failures = Vec::new();
    let machine: BTreeSet<(String, String)> = model
        .routes
        .iter()
        .map(|route| (route.method.clone(), normalize_path(&route.path)))
        .collect();
    let admitted: BTreeSet<(String, String)> = model
        .routes
        .iter()
        .filter(|route| route.middleware == "admitted" || route.middleware == "owner-scoped")
        .map(|route| (route.method.clone(), normalize_path(&route.path)))
        .collect();
    let documented = parse_human(human);

    // 1. A row that names a route the router no longer registers.
    let mut missing: BTreeSet<(String, String)> = BTreeSet::new();
    for mention in &documented.mentions {
        let key = (mention.method.clone(), mention.path.clone());
        if !machine.contains(&key) && missing.insert(key) {
            failures.push(format!(
                "{HUMAN_FILE}:{} names `{} {}`, which no Hub route registers; \
                 fix that row (or remove the route)",
                mention.line, mention.method, mention.path
            ));
        }
    }

    // 2. A route (or a leaf command) no human row and no allowlist entry covers.
    let covered: BTreeSet<(String, String)> = documented
        .mentions
        .iter()
        .filter(|mention| mention.covered)
        .map(|mention| (mention.method.clone(), mention.path.clone()))
        .collect();
    for route in &model.routes {
        let key = (route.method.clone(), normalize_path(&route.path));
        if !covered.contains(&key) {
            failures.push(format!(
                "`{} {}` has no row in {HUMAN_FILE}; add one, or a commented entry \
                 under \"Hub routes no human action calls\" (or \"Not agent actions\")",
                route.method, route.path
            ));
        }
    }
    for command in model.commands.iter().filter(|command| !command.group) {
        if !documented.commands.contains(&command.path) {
            failures.push(format!(
                "clap subcommand `remuda {}` has no row in {HUMAN_FILE}; add one to a \
                 CLI table, or an entry under \"Processes and local commands\" saying why \
                 it is not a human action",
                command.path
            ));
        }
    }

    // 3. A row whose class contradicts the middleware.
    for row in &documented.rows {
        for mention in &row.routes {
            let Some(claim) = row.claim(mention) else {
                continue;
            };
            let key = (mention.method.clone(), mention.path.clone());
            let gets_through = admitted.contains(&key);
            match claim {
                Claim::Admitted if !gets_through => failures.push(format!(
                    "{HUMAN_FILE}:{} row \"{}\" says `{} {}` is an Agent route, but \
                     restrict_agent_routes refuses it for Agent origin; correct the row's class",
                    row.line, row.action, mention.method, mention.path
                )),
                Claim::Refused if gets_through => failures.push(format!(
                    "{HUMAN_FILE}:{} row \"{}\" says the middleware refuses `{} {}` for Agent \
                     origin, but it is admitted; correct the row's class",
                    row.line, row.action, mention.method, mention.path
                )),
                _ => {}
            }
        }
    }

    // 4. Every row that begins "Agent route" must carry a readable claim for
    //    each route it names, or sit on the exemption list: a prefix the check
    //    cannot read would let the middleware start refusing a promised verb
    //    with the ledger still green.
    for row in &documented.rows {
        if !row.today.starts_with("Agent route")
            || AGENT_ROUTE_EXEMPTIONS.contains(&row.action.as_str())
        {
            continue;
        }
        for mention in &row.routes {
            if row.claim(mention).is_none() {
                failures.push(format!(
                    "{HUMAN_FILE}:{} row \"{}\" begins \"Agent route\" but the check cannot read \
                     a claim for `{} {}`; start the Today cell with exactly \"Agent route.\" \
                     (or add the row to AGENT_ROUTE_EXEMPTIONS in crates/remuda/src/parity.rs, \
                     with a comment saying which paths it mixes)",
                    row.line, row.action, mention.method, mention.path
                ));
            }
        }
    }
    failures
}

// ── route registrations ────────────────────────────────────────────────────

/// A `.route(path, ..)` registration: one method and its handler path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouteTarget {
    /// Upper-case HTTP method.
    pub method: String,
    /// The registered path template.
    pub path: String,
    /// The module that registers the route: where a bare handler lives.
    pub module: String,
    /// The handler as written (`http::get_journal`, `claim`).
    pub handler: String,
}

/// Scan `.route(...)` registrations out of Rust sources (module, source). The
/// scan stops at the test module, and recurses through nested modules because
/// the walk that feeds it does.
#[must_use]
pub fn scan_route_registrations(
    sources: &[(String, String)],
) -> BTreeMap<(String, String), RouteTarget> {
    let mut routes = BTreeMap::new();
    for (module, source) in sources {
        let source = production_source(source);
        let mut cursor = 0;
        while let Some(relative) = source[cursor..].find(".route(") {
            let open = cursor + relative + ".route".len();
            let Some(end) = match_delimiter(source, open, '(', ')') else {
                break;
            };
            let inside = &source[open + 1..end - 1];
            if let Some((path, rest)) = split_path_argument(inside) {
                for (method, handler) in method_handlers(rest) {
                    routes.insert(
                        (method.to_ascii_uppercase(), path.to_owned()),
                        RouteTarget {
                            method: method.to_ascii_uppercase(),
                            path: path.to_owned(),
                            module: module.clone(),
                            handler,
                        },
                    );
                }
            }
            cursor = end;
        }
    }
    routes
}

/// The first argument of `.route(...)`, when it is a literal path.
fn split_path_argument(inside: &str) -> Option<(&str, &str)> {
    let inside = inside.trim_start();
    let rest = inside.strip_prefix('"')?;
    let close = rest.find('"')?;
    let path = &rest[..close];
    if !path.starts_with('/') {
        return None;
    }
    Some((path, &rest[close + 1..]))
}

/// `get(handler).post(other)` → `[("get", "handler"), ("post", "other")]`.
fn method_handlers(rest: &str) -> Vec<(String, String)> {
    let mut found = Vec::new();
    for method in METHODS {
        let needle = format!("{method}(");
        for (index, _) in rest.match_indices(&needle) {
            let boundary = index == 0
                || !rest.as_bytes()[index - 1].is_ascii_alphanumeric()
                    && rest.as_bytes()[index - 1] != b'_';
            if !boundary {
                continue;
            }
            let open = index + method.len();
            let Some(end) = match_delimiter(rest, open, '(', ')') else {
                continue;
            };
            let handler = rest[open + 1..end - 1].trim();
            if !handler.is_empty()
                && handler
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == ':')
            {
                found.push((method.to_owned(), handler.to_owned()));
            }
        }
    }
    found
}

/// Byte index just past the closing delimiter at `open`, respecting nested
/// delimiters, strings, char literals and both comment forms.
fn match_delimiter(source: &str, open: usize, opener: char, closer: char) -> Option<usize> {
    let bytes = source.as_bytes();
    if bytes.get(open).copied() != Some(opener as u8) {
        return None;
    }
    let mut depth = 0usize;
    let mut i = open;
    while i < bytes.len() {
        match bytes[i] {
            b if b == opener as u8 => depth += 1,
            b if b == closer as u8 => {
                depth -= 1;
                if depth == 0 {
                    return Some(i + 1);
                }
            }
            b'"' => {
                i += 1;
                while i < bytes.len() {
                    match bytes[i] {
                        b'\\' => i += 1,
                        b'"' => break,
                        _ => {}
                    }
                    i += 1;
                }
            }
            b'/' if bytes.get(i + 1) == Some(&b'/') => {
                i += 2;
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if bytes.get(i + 1) == Some(&b'*') => {
                i += 2;
                while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                    i += 1;
                }
                i += 1;
            }
            _ => {}
        }
        i += 1;
    }
    None
}

/// Every documented `(method, path)` in the Hub OpenAPI document.
pub fn openapi_routes(document: &str) -> Result<BTreeSet<(String, String)>, String> {
    let spec: serde_json::Value =
        serde_json::from_str(document).map_err(|error| format!("openapi.json: {error}"))?;
    let paths = spec["paths"]
        .as_object()
        .ok_or_else(|| "openapi.json has no `paths` object".to_owned())?;
    let mut routes = BTreeSet::new();
    for (path, item) in paths {
        for method in METHODS {
            if item.get(method).is_some() {
                routes.insert((method.to_ascii_uppercase(), path.clone()));
            }
        }
    }
    Ok(routes)
}

/// Read every `.rs` source under `root`, recursively, as (module, text).
fn read_hub_sources(root: &Path) -> Result<Vec<(String, String)>, String> {
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) -> std::io::Result<()> {
        for entry in std::fs::read_dir(dir)? {
            let path = entry?.path();
            if path.is_dir() {
                walk(&path, out)?;
            } else if path.extension().is_some_and(|ext| ext == "rs") {
                out.push(path);
            }
        }
        Ok(())
    }
    let mut files = Vec::new();
    walk(root, &mut files).map_err(|error| format!("cannot walk {}: {error}", root.display()))?;
    files.sort();
    let mut sources = Vec::new();
    for file in files {
        let text = std::fs::read_to_string(&file)
            .map_err(|error| format!("cannot read {}: {error}", file.display()))?;
        sources.push((module_of(root, &file), text));
    }
    Ok(sources)
}

/// `src/http.rs` → `http`, `src/bot/mod.rs` → `bot`.
fn module_of(root: &Path, file: &Path) -> String {
    let relative = file.strip_prefix(root).unwrap_or(file);
    let without_ext = relative.with_extension("");
    let mut parts: Vec<String> = without_ext
        .components()
        .map(|part| part.as_os_str().to_string_lossy().into_owned())
        .collect();
    if parts.last().is_some_and(|last| last == "mod") {
        parts.pop();
    }
    parts.join("::")
}

// ── the middleware verdict ────────────────────────────────────────────────

/// Turn a path template into a concrete path the middleware can be asked
/// about: every `{..}` segment becomes `x`.
fn concrete_path(template: &str) -> String {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        let Some(close) = rest[open..].find('}') else {
            out.push_str(&rest[open..]);
            return out;
        };
        out.push('x');
        rest = &rest[open + close + 1..];
    }
    out.push_str(rest);
    out
}

fn middleware_verdict(method: &str, path: &str) -> (String, Option<String>) {
    use remuda_hub::{AgentRouteAdmission, OwnedTarget};
    let parsed = axum::http::Method::from_bytes(method.as_bytes()).expect("a known method");
    let concrete = concrete_path(path);
    match remuda_hub::agent_route_admission(&parsed, &concrete) {
        AgentRouteAdmission::MiddlewareSkipped => ("skipped".to_owned(), None),
        AgentRouteAdmission::Admitted => ("admitted".to_owned(), None),
        AgentRouteAdmission::OwnerScoped(target) => (
            "owner-scoped".to_owned(),
            Some(
                match target {
                    OwnedTarget::Instance(_) => "instance",
                    OwnedTarget::Lineage(_) => "lineage",
                    OwnedTarget::Worker(_) => "worker",
                }
                .to_owned(),
            ),
        ),
        AgentRouteAdmission::Refused => ("refused".to_owned(), None),
    }
}

// ── handler gates ─────────────────────────────────────────────────────────

/// The readable gates reachable from a handler's body.
struct GateScan {
    operator: bool,
    grants: BTreeSet<String>,
    scope: bool,
}

/// A function body, filed under its module.
struct Function {
    module: String,
    body: String,
}

fn index_functions(sources: &[(String, String)]) -> BTreeMap<(String, String), Function> {
    let mut index = BTreeMap::new();
    for (module, source) in sources {
        let source = production_source(source);
        for (name, body) in function_bodies(source) {
            index.entry((module.clone(), name)).or_insert(Function {
                module: module.clone(),
                body,
            });
        }
    }
    index
}

/// `fn name(..) { .. }` bodies in one source; generic parameters and where
/// clauses are skipped up to the body's opening brace.
fn function_bodies(source: &str) -> Vec<(String, String)> {
    let mut bodies = Vec::new();
    for start in code_fn_positions(source) {
        let after = start + 3;
        let rest = &source[after..];
        let name_len = rest
            .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
            .unwrap_or(rest.len());
        if name_len == 0 {
            continue;
        }
        let name = rest[..name_len].to_owned();
        // Find the body's opening brace; `;` ends a declaration with no body.
        let mut i = after + name_len;
        let mut open = None;
        while i < source.len() {
            match source.as_bytes()[i] {
                b'{' => {
                    open = Some(i);
                    break;
                }
                b';' => break,
                b'"' => {
                    i += 1;
                    while i < source.len() && source.as_bytes()[i] != b'"' {
                        if source.as_bytes()[i] == b'\\' {
                            i += 1;
                        }
                        i += 1;
                    }
                }
                _ => {}
            }
            i += 1;
        }
        let Some(open) = open else {
            continue;
        };
        // An inherent or trait method is not a route handler.
        if source[after + name_len..open]
            .split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
            .any(|word| word == "self")
        {
            continue;
        }
        let Some(end) = match_delimiter(source, open, '{', '}') else {
            continue;
        };
        bodies.push((name, source[open + 1..end - 1].to_owned()));
    }
    bodies
}

/// Byte offsets of `fn` keywords on a code line (a line's text before its
/// `//` comment). Definitions start a signature line; this is steadier than a
/// character-state machine over the whole file when strings hold `/*` or
/// `//`.
fn code_fn_positions(source: &str) -> Vec<usize> {
    let mut positions = Vec::new();
    let mut offset = 0;
    for line in source.split_inclusive('\n') {
        let code = line.find("//").map_or(line, |at| &line[..at]);
        let bytes = code.as_bytes();
        let mut i = 0;
        while i + 3 <= bytes.len() {
            if bytes[i] == b'f'
                && bytes[i + 1] == b'n'
                && bytes[i + 2].is_ascii_whitespace()
                && (i == 0 || !ident_bytes(bytes, i - 1))
            {
                positions.push(offset + i);
            }
            i += 1;
        }
        offset += line.len();
    }
    positions
}

/// The source before its `#[cfg(test)] mod tests`, but not before an inline
/// `#[cfg(test)]` helper: cutting at the first attribute would hide every
/// handler defined after it.
fn production_source(source: &str) -> &str {
    let marker = "#[cfg(test)]";
    let mut from = 0;
    while let Some(at) = source[from..].find(marker) {
        let absolute = from + at;
        let rest = source[absolute + marker.len()..].trim_start();
        if rest.starts_with("mod ") {
            return &source[..absolute];
        }
        from = absolute + 1;
    }
    source
}

fn ident_bytes(bytes: &[u8], index: usize) -> bool {
    bytes[index].is_ascii_alphanumeric() || bytes[index] == b'_'
}

fn handler_gate(index: &BTreeMap<(String, String), Function>, target: &RouteTarget) -> String {
    // A bare handler lives in the module that registers the route; `http::f`
    // names module `http`. A handler name is not unique across the crate
    // (`list`, `claim`), so never fall back to every module.
    let (module, name) = match target.handler.split_once("::") {
        Some((module, name)) => (module.to_owned(), name.to_owned()),
        None => (target.module.clone(), target.handler.clone()),
    };
    let mut scan = GateScan {
        operator: false,
        grants: BTreeSet::new(),
        scope: false,
    };
    let mut visited = BTreeSet::new();
    let mut frontier: Vec<(String, String, usize)> = vec![(module, name, 0)];
    while let Some((module, name, depth)) = frontier.pop() {
        if !visited.insert((module.clone(), name.clone())) || visited.len() > 64 {
            continue;
        }
        let Some(function) = index.get(&(module.clone(), name.clone())) else {
            continue;
        };
        scan_body(&function.body, &mut scan);
        if depth == 2 {
            continue;
        }
        for callee in unconditional_calls(&function.body) {
            if index.contains_key(&(function.module.clone(), callee.clone())) {
                frontier.push((function.module.clone(), callee, depth + 1));
            }
        }
    }
    let mut parts = Vec::new();
    if scan.operator {
        parts.push("operator-only".to_owned());
    }
    if !scan.grants.is_empty() {
        parts.push(format!(
            "grant: {}",
            scan.grants.into_iter().collect::<Vec<_>>().join(", ")
        ));
    }
    if scan.scope {
        parts.push("scope".to_owned());
    }
    if parts.is_empty() {
        "handler-checked".to_owned()
    } else {
        parts.join("; ")
    }
}

fn scan_body(body: &str, scan: &mut GateScan) {
    // Only an unconditional call counts: a gate inside an `if` or a `match`
    // arm applies to one branch, which the scan cannot describe honestly.
    let depth = brace_depth(body);
    if top_level(body, &depth, "require_operator(") {
        scan.operator = true;
    }
    if top_level(body, &depth, "caller_project_scope(")
        || top_level(body, &depth, "require_project_scope(")
    {
        scan.scope = true;
    }
    let mut cursor = 0;
    while let Some(relative) = body[cursor..].find("require_grant(") {
        let start = cursor + relative;
        let open = start + "require_grant".len();
        let Some(end) = match_delimiter(body, open, '(', ')') else {
            break;
        };
        if depth.get(start).copied().unwrap_or(1) == 0 {
            // A verb chosen by a condition is still read reliably: report
            // every verb the call can pass rather than the first one.
            scan.grants.extend(grant_words(&body[open + 1..end - 1]));
        }
        cursor = end;
    }
}

/// Whether `needle` occurs at the body's top level.
fn top_level(body: &str, depth: &[u32], needle: &str) -> bool {
    body.match_indices(needle)
        .any(|(index, _)| depth.get(index).copied().unwrap_or(1) == 0)
}

/// Brace depth before each byte of `body`, ignoring strings and comments.
fn brace_depth(body: &str) -> Vec<u32> {
    let bytes = body.as_bytes();
    let mut depth = vec![0u32; bytes.len()];
    let mut level = 0u32;
    let mut i = 0;
    while i < bytes.len() {
        depth[i] = level;
        match bytes[i] {
            b'{' => level += 1,
            b'}' => level = level.saturating_sub(1),
            b'"' => {
                i += 1;
                while i < bytes.len() {
                    depth[i] = level;
                    if bytes[i] == b'\\' {
                        i += 1;
                        if i < bytes.len() {
                            depth[i] = level;
                        }
                    } else if bytes[i] == b'"' {
                        break;
                    }
                    i += 1;
                }
            }
            b'/' if bytes.get(i + 1) == Some(&b'/') => {
                i += 2;
                while i < bytes.len() && bytes[i] != b'\n' {
                    depth[i] = level;
                    i += 1;
                }
            }
            b'/' if bytes.get(i + 1) == Some(&b'*') => {
                i += 2;
                while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                    depth[i] = level;
                    i += 1;
                }
                i += 1;
            }
            _ => {}
        }
        i += 1;
    }
    depth
}

/// The grants named by `require_grant(..)`'s arguments: every
/// `GrantVerb::X` the call can pass, else a `"dispatch"` literal.
fn grant_words(arguments: &str) -> BTreeSet<String> {
    let mut verbs = BTreeSet::new();
    for rest in arguments.split("GrantVerb::").skip(1) {
        let verb: String = rest
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
            .collect();
        if !verb.is_empty() {
            verbs.insert(kebab(&verb));
        }
    }
    if verbs.is_empty() {
        for literal in arguments.split('"').skip(1).step_by(2) {
            if literal
                .chars()
                .all(|c| c.is_ascii_lowercase() || c == '-' || c == '_')
                && !literal.is_empty()
            {
                verbs.insert(literal.to_owned());
            }
        }
    }
    verbs
}

/// `Dispatch` → `dispatch`, `AddressOwner` → `address-owner`.
fn kebab(name: &str) -> String {
    let mut out = String::new();
    for (index, c) in name.char_indices() {
        if c.is_ascii_uppercase() && index > 0 {
            out.push('-');
        }
        out.push(c.to_ascii_lowercase());
    }
    out
}

/// Free-function call names a body makes unconditionally: an identifier
/// followed by `(`, excluding method calls (`x.f(`) and keywords.
fn unconditional_calls(body: &str) -> BTreeSet<String> {
    let depth = brace_depth(body);
    let mut calls = BTreeSet::new();
    let bytes = body.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'(' && depth.get(i).copied().unwrap_or(1) == 0 {
            let mut end = i;
            while end > 0 && (bytes[end - 1].is_ascii_alphanumeric() || bytes[end - 1] == b'_') {
                end -= 1;
            }
            let name = &body[end..i];
            let receiver = end > 0 && bytes[end - 1] == b'.';
            if !name.is_empty()
                && !receiver
                && !matches!(
                    name,
                    "if" | "while" | "match" | "for" | "return" | "fn" | "Some" | "Ok" | "Err"
                )
            {
                calls.insert(name.to_owned());
            }
            i += 1;
            continue;
        }
        i += 1;
    }
    calls
}

// ── the clap tree ─────────────────────────────────────────────────────────

/// Every path in the clap tree, sorted. The root (`remuda`) is the program
/// name, not a path: its children are the first level.
#[must_use]
pub fn clap_commands(root: &clap::Command) -> Vec<ParityCommand> {
    fn walk(command: &clap::Command, prefix: &str, out: &mut Vec<ParityCommand>) {
        for child in command.get_subcommands() {
            let path = if prefix.is_empty() {
                child.get_name().to_owned()
            } else {
                format!("{prefix} {}", child.get_name())
            };
            out.push(ParityCommand {
                path: path.clone(),
                group: child.get_subcommands().next().is_some(),
                hub_routes: Vec::new(),
            });
            walk(child, &path, out);
        }
    }
    let mut out = Vec::new();
    walk(root, "", &mut out);
    out.sort_by(|a, b| a.path.cmp(&b.path));
    out
}

// ── the human half ────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Claim {
    Admitted,
    Refused,
}

/// One `METHOD /path` mention in the human half.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Mention {
    method: String,
    path: String,
    line: usize,
    /// A table row or an allowlist bullet: the mention counts as coverage.
    covered: bool,
}

/// A table row that names Hub routes.
#[derive(Debug, Clone)]
struct Row {
    line: usize,
    action: String,
    today: String,
    routes: Vec<Mention>,
}

impl Row {
    /// What this row claims about `mention`, from the Today cell's segment
    /// after the route's mentions, else from the row's class.
    fn claim(&self, mention: &Mention) -> Option<Claim> {
        if let Some(claim) = segment_claim(&self.today, &mention.path) {
            return Some(claim);
        }
        row_claim(&self.today)
    }
}

struct Human {
    mentions: Vec<Mention>,
    rows: Vec<Row>,
    commands: BTreeSet<String>,
}

fn parse_human(human: &str) -> Human {
    let mut mentions = Vec::new();
    let mut rows = Vec::new();
    let mut commands = BTreeSet::new();
    let mut allowlist = false;
    for (index, line) in human.lines().enumerate() {
        let number = index + 1;
        let trimmed = line.trim_start();
        if trimmed.starts_with("## ") {
            allowlist = ALLOWLIST_SECTIONS
                .iter()
                .any(|heading| trimmed.starts_with(heading));
        }
        let is_table = trimmed.starts_with('|');
        let is_bullet = trimmed.starts_with("- ");
        let covered = is_table || (allowlist && is_bullet);
        for (method, path) in route_mentions(line) {
            mentions.push(Mention {
                method,
                path,
                line: number,
                covered,
            });
        }
        if let Some(cells) = table_cells(line)
            && cells.len() >= 4
        {
            let action = cells[0].clone();
            if let Some(name) = command_name(&action) {
                commands.insert(name);
            }
            let row_routes = route_mentions(&cells[1])
                .into_iter()
                .map(|(method, path)| Mention {
                    method,
                    path,
                    line: number,
                    covered: true,
                })
                .collect::<Vec<_>>();
            rows.push(Row {
                line: number,
                action,
                today: cells[2].clone(),
                routes: row_routes,
            });
        }
    }
    Human {
        mentions,
        rows,
        commands,
    }
}

/// The cells of a markdown table row, trimmed; `None` for a separator line.
fn table_cells(line: &str) -> Option<Vec<String>> {
    let trimmed = line.trim();
    if !trimmed.starts_with('|') {
        return None;
    }
    let cells: Vec<String> = trimmed
        .trim_matches('|')
        .split('|')
        .map(|cell| cell.trim().to_owned())
        .collect();
    if cells
        .iter()
        .all(|cell| cell.chars().all(|c| c == '-' || c == ':' || c == ' '))
    {
        return None;
    }
    Some(cells)
}

/// The clap path a row's first cell names, e.g. "`instance read`" →
/// "instance read"; "`gate <branch>` (verify)" → "gate".
fn command_name(cell: &str) -> Option<String> {
    let rest = cell.trim_start().strip_prefix('`')?;
    let span = rest.split('`').next()?;
    let span = span.strip_prefix("remuda ").unwrap_or(span);
    let words: Vec<&str> = span
        .split_whitespace()
        .take_while(|word| !word.starts_with('<') && !word.starts_with('-'))
        .collect();
    if words.is_empty() {
        None
    } else {
        Some(words.join(" "))
    }
}

/// Every `METHOD /path` token inside backticks.
fn route_mentions(text: &str) -> Vec<(String, String)> {
    let mut found = Vec::new();
    for span in text.split('`').skip(1).step_by(2) {
        let span = span.trim();
        let mut words = span.split_whitespace();
        let Some(method) = words.next() else {
            continue;
        };
        let method = method.to_ascii_uppercase();
        if !METHODS
            .iter()
            .any(|known| known.to_ascii_uppercase() == method)
        {
            continue;
        }
        let Some(path) = words.next() else {
            continue;
        };
        let path = path.split('?').next().unwrap_or(path);
        if path.starts_with('/') {
            found.push((method, normalize_path(path)));
        }
    }
    found
}

/// `{id}`, `{objectId}` and `{}` all compare equal.
fn normalize_path(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    let mut rest = path;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        let Some(close) = rest[open..].find('}') else {
            out.push_str(&rest[open..]);
            return out;
        };
        out.push_str("{}");
        rest = &rest[open + close + 1..];
    }
    out.push_str(rest);
    out
}

fn row_claim(today: &str) -> Option<Claim> {
    if today.starts_with("Agent route.") {
        return Some(Claim::Admitted);
    }
    if (today.starts_with("Gap.") || today.starts_with("Safety.")) && says_middleware_refuses(today)
    {
        return Some(Claim::Refused);
    }
    None
}

/// Phrases that claim a middleware refusal (not a handler one).
fn says_middleware_refuses(today: &str) -> bool {
    let today = today.to_ascii_lowercase();
    [
        "not admitted",
        "restrict_agent_routes",
        "the middleware",
        "read_target",
    ]
    .iter()
    .any(|phrase| today.contains(phrase))
}

/// The claim carried by the text after each mention of `path` in `today`,
/// up to the next route-like token. `None` when no segment says either way.
fn segment_claim(today: &str, path: &str) -> Option<Claim> {
    let mut admitted = false;
    let mut refused = false;
    let mut from = 0;
    while let Some((start, end)) = find_template(today, path, from) {
        let mut segment_end = today.len();
        for (index, _) in today[end..].match_indices('/') {
            let tail = &today[end + index..];
            if is_path_token(tail) {
                segment_end = end + index;
                break;
            }
        }
        // A sentence boundary ends the explanation too: one route's sentence
        // must not borrow the next sentence's verdict.
        for boundary in [". ", "; "] {
            if let Some(offset) = today[end..segment_end].find(boundary) {
                segment_end = end + offset;
            }
        }
        let segment = &today[end..segment_end];
        if says_middleware_refuses(segment) {
            refused = true;
        } else if segment.to_ascii_lowercase().contains("admit") {
            admitted = true;
        }
        from = start + 1;
    }
    match (admitted, refused) {
        (true, false) => Some(Claim::Admitted),
        (false, true) => Some(Claim::Refused),
        _ => None,
    }
}

/// Find `path` in `text` starting at `from`. A `{..}` in `path` matches any
/// bracketed template, and the match must start and end on a token boundary so
/// `/v1/instances` never matches inside `/v1/instances/{id}`.
fn find_template(text: &str, path: &str, from: usize) -> Option<(usize, usize)> {
    let parts = literal_parts(path);
    let first = parts.first()?;
    let mut cursor = from;
    while let Some(found) = text[cursor..].find(first) {
        let start = cursor + found;
        let mut end = start + first.len();
        let mut matched = true;
        for part in &parts[1..] {
            if part.is_empty() {
                continue;
            }
            match text[end..].find(part) {
                Some(offset) => end += offset + part.len(),
                None => {
                    matched = false;
                    break;
                }
            }
        }
        let before = text[..start].chars().next_back();
        let after = text[end..].chars().next();
        let starts_clean = before
            .is_none_or(|c| c == '`' || c.is_whitespace() || matches!(c, '(' | '"' | '\'' | ','));
        let ends_clean = after.is_none_or(|c| {
            c == '`'
                || c == '{'
                || c.is_whitespace()
                || matches!(c, '?' | '"' | '\'' | ',' | '.' | ')' | ';' | ':')
        });
        if matched && starts_clean && ends_clean {
            return Some((start, end));
        }
        cursor = start + 1;
    }
    None
}

/// Split a path on its `{..}` templates, keeping the literal chunks.
fn literal_parts(path: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut rest = path;
    while let Some(open) = rest.find('{') {
        parts.push(&rest[..open]);
        let Some(close) = rest[open..].find('}') else {
            parts.push(&rest[open..]);
            return parts;
        };
        rest = &rest[open + close + 1..];
    }
    parts.push(rest);
    parts
}

/// A `/v1/...` token starts a new segment.
fn is_path_token(text: &str) -> bool {
    let mut parts = text.split('/');
    parts.next();
    let segment = parts.next().unwrap_or_default();
    segment == "v1" || segment == "node" || segment == "healthz" || segment == "push"
}
