use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

use super::{path_specificity, ServiceSpec, SpecVerification};

fn openapi_path_matches(template: &str, actual: &str) -> bool {
    let t_segs: Vec<&str> = template.trim_matches('/').split('/').collect();
    let a_segs: Vec<&str> = actual.trim_matches('/').split('/').collect();
    if t_segs.len() != a_segs.len() {
        return false;
    }
    t_segs
        .iter()
        .zip(a_segs.iter())
        .all(|(t, a)| (t.starts_with('{') && t.ends_with('}')) || t.starts_with(':') || t == a)
}

/// Parsed OpenAPI 3.x contract supplied alongside `SPEC.md`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct OpenApiContract {
    /// Canonicalized JSON representation used for deterministic caching and prompting.
    pub raw_json: String,
    /// Parsed OpenAPI 3.x document.
    pub document: serde_json::Value,
}

impl OpenApiContract {
    /// Parse and validate an OpenAPI 3.x JSON document.
    pub fn parse(raw: &str) -> Result<Self, String> {
        let document: serde_json::Value = serde_json::from_str(raw).map_err(|error| {
            format!("OpenAPI contract must be valid JSON (YAML is not supported yet): {error}")
        })?;

        let obj = document
            .as_object()
            .ok_or_else(|| "OpenAPI contract root must be a JSON object".to_string())?;

        let version = obj.get("openapi").and_then(|v| v.as_str()).ok_or_else(|| {
            "OpenAPI contract must declare an 'openapi' version string".to_string()
        })?;

        if !version.starts_with("3.") {
            return Err(format!(
                "unsupported OpenAPI version '{version}'; expected 3.x"
            ));
        }

        if !obj.get("paths").is_some_and(|p| p.is_object()) {
            return Err("OpenAPI contract must declare a 'paths' object".to_string());
        }

        let raw_json = serde_json::to_string_pretty(&document)
            .map_err(|error| format!("failed to serialize OpenAPI contract: {error}"))?;

        Ok(Self { raw_json, document })
    }

    /// Generate a baseline OpenAPI 3.0.3 JSON contract from a parsed `SPEC.md`.
    pub fn from_service_spec(spec: &ServiceSpec) -> Self {
        let mut paths = serde_json::Map::new();

        for endpoint in &spec.endpoints {
            let openapi_path = to_openapi_path(&endpoint.path);
            let path_entry = paths
                .entry(openapi_path)
                .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
            let Some(path_obj) = path_entry.as_object_mut() else {
                continue;
            };

            let mut op = serde_json::Map::new();
            if let Some(desc) = &endpoint.description {
                op.insert(
                    "summary".to_string(),
                    serde_json::Value::String(desc.clone()),
                );
            }

            if let Some(body_schema) = &endpoint.body_schema {
                let schema_val = serde_json::from_str::<serde_json::Value>(body_schema)
                    .unwrap_or_else(
                        |_| serde_json::json!({ "type": "object", "description": body_schema }),
                    );
                op.insert(
                    "requestBody".to_string(),
                    serde_json::json!({
                        "required": true,
                        "content": {
                            "application/json": {
                                "schema": schema_val
                            }
                        }
                    }),
                );
            }

            let mut responses = serde_json::Map::new();
            if endpoint.responses.is_empty() {
                responses.insert(
                    "200".to_string(),
                    serde_json::json!({ "description": "OK" }),
                );
            } else {
                for resp in &endpoint.responses {
                    let mut resp_obj = serde_json::Map::new();
                    resp_obj.insert(
                        "description".to_string(),
                        serde_json::Value::String(
                            resp.description
                                .clone()
                                .unwrap_or_else(|| format!("HTTP {}", resp.status)),
                        ),
                    );
                    if let Some(schema) = &resp.schema {
                        let schema_val = serde_json::from_str::<serde_json::Value>(schema)
                            .unwrap_or_else(
                                |_| serde_json::json!({ "type": "object", "description": schema }),
                            );
                        resp_obj.insert(
                            "content".to_string(),
                            serde_json::json!({
                                "application/json": {
                                    "schema": schema_val
                                }
                            }),
                        );
                    }
                    responses.insert(resp.status.to_string(), serde_json::Value::Object(resp_obj));
                }
            }
            op.insert(
                "responses".to_string(),
                serde_json::Value::Object(responses),
            );

            path_obj.insert(
                endpoint.method.to_ascii_lowercase(),
                serde_json::Value::Object(op),
            );
        }

        let mut info = serde_json::Map::new();
        info.insert(
            "title".to_string(),
            serde_json::Value::String(spec.name.clone()),
        );
        info.insert(
            "version".to_string(),
            serde_json::Value::String("1.0.0".to_string()),
        );
        if let Some(desc) = &spec.description {
            info.insert(
                "description".to_string(),
                serde_json::Value::String(desc.clone()),
            );
        }

        let document = serde_json::json!({
            "openapi": "3.0.3",
            "info": serde_json::Value::Object(info),
            "paths": serde_json::Value::Object(paths),
        });
        let raw_json = serde_json::to_string_pretty(&document)
            .expect("generated OpenAPI document is always valid JSON");

        Self { raw_json, document }
    }

    /// Look up the JSON response schema for `(method, path, status)` if declared.
    ///
    /// If the schema uses `#/components/...` `$ref`s, `components` from the OpenAPI root
    /// is attached to the returned schema object so `jsonschema` can resolve them.
    pub fn response_schema(
        &self,
        method: &str,
        path: &str,
        status: u16,
    ) -> Option<serde_json::Value> {
        let paths = self.document.get("paths")?.as_object()?;
        let normalized_path = path.split('?').next().unwrap_or(path);

        let (_, path_item) = paths
            .iter()
            .filter(|(template, _)| openapi_path_matches(template, normalized_path))
            .max_by_key(|(template, _)| path_specificity(template))?;

        let method_lower = method.to_ascii_lowercase();
        let operation = path_item.get(&method_lower)?.as_object()?;
        let responses = operation.get("responses")?.as_object()?;

        let status_key = status.to_string();
        let class_key = format!("{}XX", status / 100);
        let response_obj = responses
            .get(&status_key)
            .or_else(|| responses.get(&class_key))
            .or_else(|| responses.get("default"))?
            .as_object()?;

        let content = response_obj.get("content")?.as_object()?;
        let media = content
            .get("application/json")
            .or_else(|| {
                content
                    .iter()
                    .find(|(k, _)| k.starts_with("application/") && k.ends_with("+json"))
                    .map(|(_, v)| v)
            })?
            .as_object()?;

        let mut schema = media.get("schema")?.clone();
        if let Some(components) = self.document.get("components") {
            if let Some(schema_obj) = schema.as_object_mut() {
                schema_obj
                    .entry("components".to_string())
                    .or_insert_with(|| components.clone());
            }
        }
        Some(schema)
    }

    /// Synthesize baseline negative verification cases (405 on unsupported methods,
    /// 400-422 on missing required fields and type mismatches) from the OpenAPI contract.
    pub fn synthesize_negative_cases(
        &self,
        existing_cases: &[SpecVerification],
    ) -> Vec<SpecVerification> {
        let mut cases = Vec::new();
        let Some(paths) = self.document.get("paths").and_then(|p| p.as_object()) else {
            return cases;
        };

        let existing_pairs: HashSet<(String, String)> = existing_cases
            .iter()
            .map(|c| (c.method.to_ascii_uppercase(), c.path.clone()))
            .collect();

        let candidate_methods = ["GET", "POST", "PUT", "PATCH", "DELETE"];

        for (template, path_item) in paths {
            let Some(path_obj) = path_item.as_object() else {
                continue;
            };
            let concrete_path = materialize_path(template);

            // 1. 405 Method Not Allowed for one undeclared method per path
            let declared_methods: HashSet<String> = candidate_methods
                .iter()
                .filter(|m| path_obj.contains_key(&m.to_ascii_lowercase()))
                .map(|m| (*m).to_string())
                .collect();

            if !declared_methods.is_empty() {
                if let Some(unsupported) = candidate_methods
                    .iter()
                    .find(|m| !declared_methods.contains(**m))
                {
                    if !existing_pairs.contains(&(unsupported.to_string(), concrete_path.clone())) {
                        cases.push(SpecVerification {
                            method: unsupported.to_string(),
                            path: concrete_path.clone(),
                            query: HashMap::new(),
                            headers: HashMap::new(),
                            body: None,
                            expected_status: Some(405),
                            expected_status_max: None,
                            body_contains: None,
                            response_headers: HashMap::new(),
                            response_json_schema: None,
                            json_path_assertions: Vec::new(),
                        });
                    }
                }
            }

            // 2. 400-422 for missing required fields & type mismatches on JSON request bodies
            for method in &declared_methods {
                let Some(op) = path_obj
                    .get(&method.to_ascii_lowercase())
                    .and_then(|v| v.as_object())
                else {
                    continue;
                };
                let Some(schema) = op
                    .get("requestBody")
                    .and_then(|rb| rb.get("content"))
                    .and_then(|c| c.get("application/json"))
                    .and_then(|m| m.get("schema"))
                else {
                    continue;
                };
                let resolved = self.resolve_local_schema_ref(schema);
                let Some(schema_obj) = resolved.as_object() else {
                    continue;
                };

                let mut json_headers = HashMap::new();
                json_headers.insert("content-type".to_string(), "application/json".to_string());

                if schema_obj
                    .get("required")
                    .and_then(|r| r.as_array())
                    .is_some_and(|arr| !arr.is_empty())
                {
                    cases.push(SpecVerification {
                        method: method.clone(),
                        path: concrete_path.clone(),
                        query: HashMap::new(),
                        headers: json_headers.clone(),
                        body: Some("{}".to_string()),
                        expected_status: Some(400),
                        expected_status_max: Some(422),
                        body_contains: None,
                        response_headers: HashMap::new(),
                        response_json_schema: None,
                        json_path_assertions: Vec::new(),
                    });
                }

                if let Some(props) = schema_obj.get("properties").and_then(|p| p.as_object()) {
                    if let Some((prop_name, wrong_val)) = props.iter().find_map(|(name, prop)| {
                        let ty = prop.get("type").and_then(|t| t.as_str())?;
                        let wrong = match ty {
                            "integer" | "number" => serde_json::json!("not-a-number"),
                            "boolean" => serde_json::json!("not-a-boolean"),
                            "string" => serde_json::json!(12345),
                            "array" => serde_json::json!("not-an-array"),
                            "object" => serde_json::json!("not-an-object"),
                            _ => return None,
                        };
                        Some((name.clone(), wrong))
                    }) {
                        let mut bad_obj = serde_json::Map::new();
                        bad_obj.insert(prop_name, wrong_val);
                        cases.push(SpecVerification {
                            method: method.clone(),
                            path: concrete_path.clone(),
                            query: HashMap::new(),
                            headers: json_headers,
                            body: Some(serde_json::Value::Object(bad_obj).to_string()),
                            expected_status: Some(400),
                            expected_status_max: Some(422),
                            body_contains: None,
                            response_headers: HashMap::new(),
                            response_json_schema: None,
                            json_path_assertions: Vec::new(),
                        });
                    }
                }
            }
        }

        cases
    }

    fn resolve_local_schema_ref<'a>(
        &'a self,
        schema: &'a serde_json::Value,
    ) -> &'a serde_json::Value {
        if let Some(ref_str) = schema.get("$ref").and_then(|r| r.as_str()) {
            if let Some(name) = ref_str.strip_prefix("#/components/schemas/") {
                if let Some(target) = self
                    .document
                    .get("components")
                    .and_then(|c| c.get("schemas"))
                    .and_then(|s| s.get(name))
                {
                    return target;
                }
            }
        }
        schema
    }
}

fn to_openapi_path(path: &str) -> String {
    path.split('/')
        .map(|seg| {
            if let Some(name) = seg.strip_prefix(':') {
                format!("{{{name}}}")
            } else {
                seg.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("/")
}

fn materialize_path(template: &str) -> String {
    template
        .split('/')
        .map(|seg| {
            if (seg.starts_with('{') && seg.ends_with('}')) || seg.starts_with(':') {
                "1"
            } else {
                seg
            }
        })
        .collect::<Vec<_>>()
        .join("/")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_valid_openapi_3_json() {
        let raw = r##"{
            "openapi": "3.0.3",
            "info": { "title": "Items", "version": "1.0.0" },
            "paths": {
                "/items/{id}": {
                    "get": {
                        "responses": {
                            "200": {
                                "description": "OK",
                                "content": {
                                    "application/json": {
                                        "schema": { "$ref": "#/components/schemas/Item" }
                                    }
                                }
                            }
                        }
                    }
                }
            },
            "components": {
                "schemas": {
                    "Item": {
                        "type": "object",
                        "required": ["id"],
                        "properties": { "id": { "type": "string" } }
                    }
                }
            }
        }"##;

        let contract = OpenApiContract::parse(raw).expect("valid OpenAPI 3.0.3");
        let schema = contract
            .response_schema("GET", "/items/42?verbose=true", 200)
            .expect("schema for GET /items/42 200");
        let validator = jsonschema::validator_for(&schema).expect("valid JSON schema");
        assert!(validator.is_valid(&serde_json::json!({ "id": "42" })));
        assert!(!validator.is_valid(&serde_json::json!({ "name": "missing id" })));
    }

    #[test]
    fn rejects_non_3x_openapi() {
        let raw = r#"{ "openapi": "2.0", "paths": {} }"#;
        assert!(OpenApiContract::parse(raw).is_err());
    }

    #[test]
    fn synthesizes_negative_cases_for_405_missing_required_and_type_mismatch() {
        let raw = r##"{
            "openapi": "3.0.3",
            "info": { "title": "Items", "version": "1.0.0" },
            "paths": {
                "/items": {
                    "post": {
                        "requestBody": {
                            "required": true,
                            "content": {
                                "application/json": {
                                    "schema": { "$ref": "#/components/schemas/CreateItem" }
                                }
                            }
                        },
                        "responses": {
                            "201": { "description": "Created" }
                        }
                    }
                }
            },
            "components": {
                "schemas": {
                    "CreateItem": {
                        "type": "object",
                        "required": ["name"],
                        "properties": {
                            "name": { "type": "string" },
                            "count": { "type": "integer" }
                        }
                    }
                }
            }
        }"##;

        let contract = OpenApiContract::parse(raw).expect("valid OpenAPI");
        let cases = contract.synthesize_negative_cases(&[]);

        assert!(cases
            .iter()
            .any(|c| c.method == "GET" && c.path == "/items" && c.expected_status == Some(405)));
        assert!(cases.iter().any(|c| {
            c.method == "POST"
                && c.path == "/items"
                && c.body.as_deref() == Some("{}")
                && c.expected_status == Some(400)
                && c.expected_status_max == Some(422)
        }));
        assert!(cases.iter().any(|c| {
            c.method == "POST"
                && c.path == "/items"
                && c.body.as_deref().is_some_and(|b| b != "{}")
                && c.expected_status == Some(400)
                && c.expected_status_max == Some(422)
        }));
    }

    #[test]
    fn generates_openapi_from_service_spec() {
        let spec =
            ServiceSpec::parse(include_str!("../../examples/minimal/SPEC.md")).expect("valid spec");

        let generated = OpenApiContract::from_service_spec(&spec);
        assert_eq!(generated.document["openapi"], "3.0.3");
        assert_eq!(generated.document["info"]["title"], spec.name);
        assert!(generated.document["paths"]
            .as_object()
            .is_some_and(|p| !p.is_empty()));
    }
}
