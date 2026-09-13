//! Each registration couples one schema to one callable handler.

use std::{future::Future, pin::Pin};

use anyhow::Result;
use serde_json::{Value, json};

use super::{HubClient, tool_content};

type Handler =
    for<'a> fn(&'a HubClient, Value) -> Pin<Box<dyn Future<Output = Result<Value>> + Send + 'a>>;

pub(super) struct Tool {
    pub name: &'static str,
    description: &'static str,
    schema: Value,
    handler: Handler,
    structured_report: bool,
}

impl Tool {
    pub fn new(
        name: &'static str,
        description: &'static str,
        mut schema: Value,
        handler: Handler,
    ) -> Self {
        super::scope::tool_schema(name, &mut schema);
        Self {
            name,
            description,
            schema,
            handler,
            structured_report: false,
        }
    }

    /// Reports expose structured content and map their exit code to MCP isError.
    pub fn report(mut self) -> Self {
        self.structured_report = true;
        self
    }

    pub fn catalog(&self) -> Value {
        json!({"name": self.name, "description": self.description, "inputSchema": self.schema})
    }

    pub async fn call(&self, client: &HubClient, args: Value) -> Value {
        let result = (self.handler)(client, args).await;
        let report = self
            .structured_report
            .then(|| result.as_ref().ok().cloned())
            .flatten();
        let mut content = tool_content(result);
        if let Some(report) = report {
            content["isError"] = json!(report["exitCode"] != 0);
            content["structuredContent"] = report;
        }
        content
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmd::hub_client::connect_for_test;

    #[test]
    fn registry_has_unique_names_and_object_schemas() {
        let mut names = std::collections::BTreeSet::new();
        for tool in super::super::tools() {
            assert!(names.insert(tool.name), "duplicate tool: {}", tool.name);
            assert_eq!(tool.catalog()["inputSchema"]["type"], "object");
            assert!(!tool.description.is_empty());
        }
        assert!(!names.is_empty());
    }

    #[tokio::test]
    async fn registration_owns_dispatch_and_report_semantics() {
        let client = connect_for_test("http://127.0.0.1:1".into(), "fixture".into()).unwrap();
        let tool = Tool::new(
            "test_report",
            "fixture",
            json!({"type":"object"}),
            |_client, args| Box::pin(async move { Ok(args) }),
        )
        .report();
        for code in [0, 1, 2, 3] {
            let result = tool.call(&client, json!({"exitCode":code})).await;
            assert_eq!(result["isError"], code != 0);
            assert_eq!(result["structuredContent"]["exitCode"], code);
        }
        let broken = Tool::new(
            "test_error",
            "fixture",
            json!({"type":"object"}),
            |_client, _args| Box::pin(async move { anyhow::bail!("fixture failure") }),
        )
        .report();
        let result = broken.call(&client, json!({})).await;
        assert_eq!(result["isError"], true);
        assert!(result.get("structuredContent").is_none());
    }
}
