//! D-060 decision 12: the machine half of the agent-parity ledger, and the
//! drift check the gate runs.
//!
//! The generator (`crates/remuda/examples/gen_parity.rs`) writes
//! `docs/design/agent-parity.generated.json`; this suite proves the committed
//! file is current, that the two halves do not drift, and — through fixtures —
//! that each drift class fails with a message naming the row or route and the
//! file to fix.

use remuda::parity::{
    GENERATED_FILE, HUMAN_FILE, ParityCommand, ParityModel, ParityRoute, build_model,
    check_generated, drift, machine_model, render,
};
use std::collections::BTreeSet;

fn route(method: &str, path: &str, middleware: &str) -> ParityRoute {
    ParityRoute {
        method: method.to_owned(),
        path: path.to_owned(),
        middleware: middleware.to_owned(),
        owned: None,
        handler: "handler-checked".to_owned(),
    }
}

fn model(routes: Vec<ParityRoute>, commands: Vec<ParityCommand>) -> ParityModel {
    ParityModel {
        about: String::new(),
        notes: Vec::new(),
        routes,
        commands,
    }
}

const HEADER: &str = "\
| Action | Hub route | Today (origin/main) | Planned |
|---|---|---|---|
";

// ── the live tree ─────────────────────────────────────────────────────────

#[test]
fn the_generated_file_is_current() {
    let model = machine_model().expect("the machine model builds");
    let committed = std::fs::read_to_string(remuda::parity::repo_root().join(GENERATED_FILE))
        .unwrap_or_else(|error| panic!("{GENERATED_FILE}: {error}"));
    check_generated(&model, &committed).unwrap_or_else(|error| panic!("{error}"));
}

#[test]
fn the_generator_is_deterministic() {
    let first = machine_model().expect("the machine model builds");
    let second = machine_model().expect("the machine model builds again");
    assert_eq!(first, second, "the model must not depend on walk order");
    assert_eq!(
        render(&first),
        render(&second),
        "two runs must render identically"
    );
}

#[test]
fn the_two_halves_do_not_drift() {
    let model = machine_model().expect("the machine model builds");
    let human = std::fs::read_to_string(remuda::parity::repo_root().join(HUMAN_FILE))
        .unwrap_or_else(|error| panic!("{HUMAN_FILE}: {error}"));
    let failures = drift(&model, &human);
    assert!(failures.is_empty(), "{failures:#?}");
}

// ── fixtures: each drift class fails with a named message ────────────────

#[test]
fn a_new_route_without_a_row_fails() {
    let model = model(
        vec![
            route("GET", "/v1/a", "admitted"),
            route("POST", "/v1/brand-new", "admitted"),
        ],
        Vec::new(),
    );
    let human = format!("{HEADER}| Do a | `GET /v1/a` | Agent route. | — |\n");
    let failures = drift(&model, &human);
    assert!(
        failures.iter().any(
            |failure| failure.contains("`POST /v1/brand-new` has no row")
                && failure.contains(HUMAN_FILE)
        ),
        "{failures:#?}"
    );
}

#[test]
fn a_row_naming_a_removed_route_fails() {
    let model = model(vec![route("GET", "/v1/a", "admitted")], Vec::new());
    let human = format!(
        "{HEADER}| Do a | `GET /v1/a` | Agent route. | — |\n\
         | Do gone | `GET /v1/gone` | Agent route. | — |\n"
    );
    let failures = drift(&model, &human);
    assert!(
        failures
            .iter()
            .any(|failure| failure.contains(":4 names `GET /v1/gone`")
                && failure.contains(HUMAN_FILE)),
        "{failures:#?}"
    );
}

#[test]
fn an_agent_route_row_over_a_refused_route_fails() {
    let model = model(vec![route("GET", "/v1/a", "refused")], Vec::new());
    let human = format!("{HEADER}| Do a | `GET /v1/a` | Agent route. Owned. | — |\n");
    let failures = drift(&model, &human);
    assert!(
        failures.iter().any(
            |failure| failure.contains("says `GET /v1/a` is an Agent route")
                && failure.contains("restrict_agent_routes refuses it")
        ),
        "{failures:#?}"
    );
}

#[test]
fn a_refusal_row_over_an_admitted_route_fails() {
    let model = model(vec![route("GET", "/v1/a", "admitted")], Vec::new());
    let human =
        format!("{HEADER}| Do a | `GET /v1/a` | Gap. Not admitted for Agent origin. | `ap-x` |\n");
    let failures = drift(&model, &human);
    assert!(
        failures.iter().any(
            |failure| failure.contains("says the middleware refuses `GET /v1/a`")
                && failure.contains("but it is admitted")
        ),
        "{failures:#?}"
    );
}

#[test]
fn an_owner_scoped_route_satisfies_an_agent_route_row() {
    let model = model(vec![route("GET", "/v1/a", "owner-scoped")], Vec::new());
    let human = format!("{HEADER}| Do a | `GET /v1/a` | Agent route. Owned targets. | — |\n");
    assert!(drift(&model, &human).is_empty());
}

#[test]
fn a_stale_generated_file_fails() {
    let model = model(vec![route("GET", "/v1/a", "admitted")], Vec::new());
    let error = check_generated(&model, "{\"routes\":[]}\n").expect_err("a stale file must fail");
    assert!(
        error.contains("is stale") && error.contains(GENERATED_FILE),
        "{error}"
    );
    check_generated(&model, &render(&model)).expect("the rendered bytes are current");
}

#[test]
fn a_clap_leaf_without_a_row_fails() {
    let model = model(
        Vec::new(),
        vec![ParityCommand {
            path: "instance frobnicate".to_owned(),
            group: false,
            hub_routes: Vec::new(),
        }],
    );
    let failures = drift(&model, "");
    assert!(
        failures
            .iter()
            .any(|failure| failure.contains("clap subcommand `remuda instance frobnicate`")),
        "{failures:#?}"
    );
    let human = format!("{HEADER}| `instance frobnicate` | — | Local | — |\n");
    assert!(drift(&model, &human).is_empty());
}

// ── the source scanner ────────────────────────────────────────────────────

#[test]
fn route_registrations_and_handler_gates_are_read_from_source() {
    // A synthetic Hub module: three routes whose handlers reach three
    // different gates, one of them through a same-module helper.
    let source = r#"
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/v1/things", get(list_things).post(create_thing))
        .route("/v1/things/{id}", delete(remove_thing))
}

async fn list_things(State(state): State<AppState>) -> Result<Json<Value>, HubError> {
    let (_, scope) = require_project_scope(&state, &headers).await?;
    Ok(Json(json!({})))
}

async fn create_thing(State(state): State<AppState>) -> Result<Json<Value>, HubError> {
    let device = require_dispatch(&state, &headers).await?;
    Ok(Json(json!({})))
}

async fn remove_thing(State(state): State<AppState>) -> Result<Json<Value>, HubError> {
    crate::agent_scope::require_operator(&state, &headers).await?;
    Ok(Json(json!({})))
}

async fn require_dispatch(state: &AppState, headers: &HeaderMap) -> Result<Device, HubError> {
    let device = caller(state, headers).await?;
    require_grant(state, &device, remuda_protocol::GrantVerb::Dispatch).await?;
    Ok(device)
}

async fn require_project_scope(state: &AppState, headers: &HeaderMap) -> Result<Scope, HubError> {
    let device = caller(state, headers).await?;
    let scope = caller_project_scope(state, &device).await?;
    Ok((device, scope))
}
"#;
    let sources = vec![("fake".to_owned(), source.to_owned())];
    let clap = clap::Command::new("remuda");
    let model = build_model(&sources, &BTreeSet::new(), &clap);
    let gate = |method: &str, path: &str| {
        model
            .routes
            .iter()
            .find(|route| route.method == method && route.path == path)
            .map(|route| route.handler.clone())
            .unwrap_or_else(|| panic!("{method} {path} missing from {:#?}", model.routes))
    };
    assert_eq!(gate("POST", "/v1/things"), "grant: dispatch");
    assert_eq!(gate("GET", "/v1/things"), "scope");
    assert_eq!(gate("DELETE", "/v1/things/{id}"), "operator-only");
    for route in &model.routes {
        assert_eq!(route.middleware, "refused", "{route:?}");
    }
}

#[test]
fn the_scan_covers_every_registered_route() {
    // The scan is recursive and must see the nested `bot` module, which the
    // OpenAPI document does not document today.
    let model = machine_model().expect("the machine model builds");
    let paths: BTreeSet<&str> = model
        .routes
        .iter()
        .map(|route| route.path.as_str())
        .collect();
    assert!(
        paths.contains("/v1/bot/card-tickets"),
        "nested modules are scanned"
    );
    assert!(
        paths.contains("/push/config"),
        "the nest-mounted carrier is documented"
    );
    assert!(
        model.routes.len() > 140,
        "{} routes is too few to be the Hub",
        model.routes.len()
    );
}
