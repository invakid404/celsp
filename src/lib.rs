//! CEL Language Server implementation.
//!
//! On native targets, this crate provides a full tower-lsp language server
//! with proto file support, settings discovery, etc.
//!
//! On `wasm32`, the tower-lsp / tokio / proto dependencies are stripped and
//! the crate exposes a `CelAnalyzer` struct via wasm-bindgen with a
//! synchronous JSON-RPC `handle_request` entry point.

mod document;
mod lsp;
mod type_parser;
pub(crate) mod types;

pub use document::{DocumentState, LineIndex};

// ─── Native-only: tower-lsp server, proto support, settings ────────────────

#[cfg(not(target_arch = "wasm32"))]
pub(crate) mod protovalidate;
#[cfg(not(target_arch = "wasm32"))]
pub(crate) mod settings;

#[cfg(not(target_arch = "wasm32"))]
pub use document::{DocumentKind, ProtoDocumentState};
#[cfg(not(target_arch = "wasm32"))]
pub use lsp::{completion_at_position_proto, proto_to_diagnostics, to_diagnostics};
#[cfg(not(target_arch = "wasm32"))]
pub use settings::{build_env_with_protos, discover_settings, load_proto_registry, load_settings};

#[cfg(not(target_arch = "wasm32"))]
mod native {
    use std::path::PathBuf;
    use std::sync::{Arc, OnceLock};

    use cel_core::Env;
    use cel_core_proto::ProstProtoRegistry;
    use tower_lsp::jsonrpc::Result;
    use tower_lsp::lsp_types::*;
    use tower_lsp::{Client, LanguageServer, LspService};

    use crate::document::{DocumentKind, DocumentStore};
    use crate::lsp;
    use crate::settings;

    pub struct Backend {
        client: Client,
        documents: DocumentStore,
        workspace_root: OnceLock<PathBuf>,
        proto_registry: OnceLock<Option<Arc<ProstProtoRegistry>>>,
        env: OnceLock<Arc<Env>>,
    }

    impl Backend {
        pub(crate) fn new(client: Client) -> Self {
            Self {
                client,
                documents: DocumentStore::new(),
                workspace_root: OnceLock::new(),
                proto_registry: OnceLock::new(),
                env: OnceLock::new(),
            }
        }

        async fn on_document_change(&self, uri: Url, text: String, version: i32) {
            let registry = self.proto_registry.get().and_then(|r| r.clone());
            let env = self.env.get();
            let state = self
                .documents
                .open(uri.clone(), text, version, registry.as_ref(), env);
            self.publish_diagnostics_for(&uri, &state).await;
        }

        async fn publish_diagnostics_for(&self, uri: &Url, state: &DocumentKind) {
            let (diagnostics, version) = match state {
                DocumentKind::Cel(cel_state) => {
                    let diags = lsp::to_diagnostics(
                        &cel_state.errors,
                        cel_state.check_errors(),
                        &cel_state.line_index,
                    );
                    (diags, cel_state.version)
                }
                DocumentKind::Proto(proto_state) => {
                    let diags = lsp::proto_to_diagnostics(proto_state);
                    (diags, proto_state.version)
                }
            };

            self.client
                .publish_diagnostics(uri.clone(), diagnostics, Some(version))
                .await;
        }
    }

    #[tower_lsp::async_trait]
    impl LanguageServer for Backend {
        async fn initialize(&self, params: InitializeParams) -> Result<InitializeResult> {
            let workspace_root = params
                .workspace_folders
                .as_ref()
                .and_then(|folders| folders.first())
                .and_then(|f| f.uri.to_file_path().ok())
                .or_else(|| {
                    #[allow(deprecated)]
                    params.root_uri.as_ref()?.to_file_path().ok()
                });

            if let Some(root) = workspace_root {
                let _ = self.workspace_root.set(root.clone());
                let (settings, settings_dir) = settings::discover_settings(&root);
                let registry = settings::load_proto_registry(&settings, &settings_dir);
                let env =
                    Arc::new(settings::build_env_with_protos(&settings, &settings_dir));
                let _ = self.proto_registry.set(registry);
                let _ = self.env.set(env);
            } else {
                let _ = self.proto_registry.set(None);
            }

            Ok(InitializeResult {
                capabilities: ServerCapabilities {
                    text_document_sync: Some(TextDocumentSyncCapability::Kind(
                        TextDocumentSyncKind::FULL,
                    )),
                    hover_provider: Some(HoverProviderCapability::Simple(true)),
                    completion_provider: Some(CompletionOptions {
                        trigger_characters: Some(vec![".".to_string()]),
                        resolve_provider: Some(false),
                        ..Default::default()
                    }),
                    semantic_tokens_provider: Some(
                        SemanticTokensServerCapabilities::SemanticTokensOptions(
                            SemanticTokensOptions {
                                legend: lsp::legend(),
                                full: Some(SemanticTokensFullOptions::Bool(true)),
                                range: None,
                                work_done_progress_options:
                                    WorkDoneProgressOptions::default(),
                            },
                        ),
                    ),
                    ..Default::default()
                },
                ..Default::default()
            })
        }

        async fn initialized(&self, _: InitializedParams) {
            self.client
                .log_message(MessageType::INFO, "CEL language server initialized")
                .await;
        }

        async fn shutdown(&self) -> Result<()> {
            Ok(())
        }

        async fn did_open(&self, params: DidOpenTextDocumentParams) {
            self.on_document_change(
                params.text_document.uri,
                params.text_document.text,
                params.text_document.version,
            )
            .await;
        }

        async fn did_change(&self, params: DidChangeTextDocumentParams) {
            if let Some(change) = params.content_changes.into_iter().next() {
                self.on_document_change(
                    params.text_document.uri,
                    change.text,
                    params.text_document.version,
                )
                .await;
            }
        }

        async fn did_close(&self, params: DidCloseTextDocumentParams) {
            self.documents.close(&params.text_document.uri);
            self.client
                .publish_diagnostics(params.text_document.uri, vec![], None)
                .await;
        }

        async fn hover(&self, params: HoverParams) -> Result<Option<Hover>> {
            let uri = &params.text_document_position_params.text_document.uri;
            let position = params.text_document_position_params.position;

            let Some(doc) = self.documents.get(uri) else {
                return Ok(None);
            };

            match doc.as_ref() {
                DocumentKind::Cel(state) => {
                    let Some(ast) = state.ast() else {
                        return Ok(None);
                    };
                    Ok(lsp::hover_at_position(
                        &state.line_index,
                        ast,
                        state.check_result.as_ref(),
                        position,
                    ))
                }
                DocumentKind::Proto(state) => {
                    Ok(lsp::hover_at_position_proto(state, position))
                }
            }
        }

        async fn completion(
            &self,
            params: CompletionParams,
        ) -> Result<Option<CompletionResponse>> {
            let uri = &params.text_document_position.text_document.uri;
            let position = params.text_document_position.position;

            let Some(doc) = self.documents.get(uri) else {
                eprintln!("[completion] no document found for {}", uri);
                return Ok(None);
            };

            match doc.as_ref() {
                DocumentKind::Cel(state) => Ok(lsp::completion_at_position(
                    &state.line_index,
                    &state.source,
                    &state.env,
                    position,
                )),
                DocumentKind::Proto(state) => {
                    let host_offset = state.line_index.position_to_offset(position);
                    eprintln!(
                        "[completion] proto position={:?} host_offset={:?} regions={}",
                        position,
                        host_offset,
                        state.regions.len()
                    );
                    if let Some(offset) = host_offset {
                        for (i, r) in state.regions.iter().enumerate() {
                            let start = r.mapper.host_offset();
                            let end =
                                start + r.mapper.host_length(r.region.source.len());
                            eprintln!(
                                "[completion]   region[{}]: host=[{}..{}] source={:?} contains={}",
                                i,
                                start,
                                end,
                                r.region.source,
                                r.contains_host_offset(offset)
                            );
                        }
                    }
                    let result = lsp::completion_at_position_proto(state, position);
                    eprintln!(
                        "[completion] result items={}",
                        result
                            .as_ref()
                            .map(|r| match r {
                                CompletionResponse::Array(items) => items.len(),
                                _ => 0,
                            })
                            .unwrap_or(0)
                    );
                    Ok(result)
                }
            }
        }

        async fn semantic_tokens_full(
            &self,
            params: SemanticTokensParams,
        ) -> Result<Option<SemanticTokensResult>> {
            let uri = &params.text_document.uri;

            let Some(doc) = self.documents.get(uri) else {
                return Ok(None);
            };

            let tokens = match doc.as_ref() {
                DocumentKind::Cel(state) => {
                    let Some(ast) = state.ast() else {
                        return Ok(None);
                    };
                    lsp::tokens_for_ast(&state.line_index, ast)
                }
                DocumentKind::Proto(state) => lsp::tokens_for_proto(state),
            };

            Ok(Some(SemanticTokensResult::Tokens(SemanticTokens {
                result_id: None,
                data: tokens,
            })))
        }
    }

    pub fn create_service() -> (LspService<Backend>, tower_lsp::ClientSocket) {
        LspService::new(Backend::new)
    }
}

#[cfg(not(target_arch = "wasm32"))]
pub use native::{create_service, Backend};

// ─── WASM: synchronous JSON-RPC entry point ────────────────────────────────

#[cfg(target_arch = "wasm32")]
mod wasm {
    use std::collections::HashMap;
    use std::sync::Arc;

    use cel_core::types::FunctionDecl;
    use cel_core::types::OverloadDecl;
    use cel_core::{CelType, CheckResult, Env};
    use lsp_types::*;
    use serde_json::Value;
    use wasm_bindgen::prelude::*;

    use crate::document::DocumentState;
    use crate::lsp;
    use crate::type_parser::parse_type_string;

    /// CEL expression analyzer exposed to JavaScript via wasm-bindgen.
    ///
    /// Holds the CEL environment and per-document state. The JS side creates
    /// one instance and calls `handle_request` for each LSP JSON-RPC message.
    #[wasm_bindgen]
    pub struct CelAnalyzer {
        env: Arc<Env>,
        documents: HashMap<String, DocumentState>,
        hover_show_errors: bool,
    }

    // ── JSON → CelType / FunctionDecl conversion ───────────────────────────

    /// Parse a JSON type definition into a CelType.
    ///
    /// Accepts the celsp type-string syntax and the legacy/object wasm-cel format:
    ///   - strings: "int", "optional(string)", "map(string, optional(int))", ...
    ///   - objects: { "kind": "list", "elementType": ... }
    ///              { "kind": "map", "keyType": ..., "valueType": ... }
    ///              { "kind": "optional", "innerType": ... }
    fn parse_cel_type(v: &Value) -> Option<CelType> {
        match v {
            Value::String(s) => parse_type_string(s).ok(),
            Value::Object(obj) => {
                let kind = obj.get("kind")?.as_str()?;
                match kind {
                    "list" => {
                        let elem = parse_cel_type(obj.get("elementType")?)?;
                        Some(CelType::List(Arc::new(elem)))
                    }
                    "map" => {
                        let key = parse_cel_type(obj.get("keyType")?)?;
                        let val = parse_cel_type(obj.get("valueType")?)?;
                        Some(CelType::Map(Arc::new(key), Arc::new(val)))
                    }
                    "optional" => {
                        let inner = parse_cel_type(obj.get("innerType")?)?;
                        Some(CelType::optional(inner))
                    }
                    _ => None,
                }
            }
            _ => None,
        }
    }

    /// Parse a JSON function declaration into a cel-core FunctionDecl.
    ///
    /// Expected shape (matching wasm-cel's CELFunctionDefinition):
    /// ```json
    /// {
    ///   "name": "myFunc",
    ///   "params": [{ "name": "a", "type": "int" }, ...],
    ///   "returnType": "string",
    ///   "overloads": [ ... ]
    /// }
    /// ```
    fn parse_function_decl(v: &Value) -> Option<FunctionDecl> {
        let obj = v.as_object()?;
        let name = obj.get("name")?.as_str()?;

        let mut decl = FunctionDecl::new(name);

        // Parse the primary overload from params + returnType.
        if let Some(overload) = parse_overload(name, obj) {
            decl = decl.with_overload(overload);
        }

        // Parse additional overloads.
        if let Some(Value::Array(overloads)) = obj.get("overloads") {
            for ov in overloads {
                if let Some(ov_obj) = ov.as_object() {
                    if let Some(overload) = parse_overload(name, ov_obj) {
                        decl = decl.with_overload(overload);
                    }
                }
            }
        }

        Some(decl)
    }

    /// Parse an overload from a JSON object with `params` and `returnType`.
    fn parse_overload(
        func_name: &str,
        obj: &serde_json::Map<String, Value>,
    ) -> Option<OverloadDecl> {
        let params_arr = obj.get("params")?.as_array()?;
        let ret = parse_cel_type(obj.get("returnType")?)?;

        let mut param_types = Vec::with_capacity(params_arr.len());
        let mut param_names = Vec::with_capacity(params_arr.len());
        for p in params_arr {
            let p_obj = p.as_object()?;
            let p_type = parse_cel_type(p_obj.get("type")?)?;
            let p_name = p_obj
                .get("name")
                .and_then(|n| n.as_str())
                .unwrap_or("_");
            param_types.push(p_type);
            param_names.push(p_name);
        }

        // Build an overload ID from function name + param type names.
        let type_names: Vec<String> = param_types.iter().map(|t| t.display_name()).collect();
        let id = format!("{}_{}", func_name, type_names.join("_"));

        Some(OverloadDecl::function(id, param_types, ret))
    }

    #[wasm_bindgen]
    impl CelAnalyzer {
        /// Create a new CelAnalyzer.
        ///
        /// `options_json` is a JSON string with optional fields:
        ///   - `variables`: array of `{ name, type }` declarations
        ///   - `functions`: array of function declarations (wasm-cel format)
        ///   - `hoverShowErrors`: boolean (default true)
        #[wasm_bindgen(constructor)]
        pub fn new(options_json: &str) -> Self {
            let options: Value =
                serde_json::from_str(options_json).unwrap_or(Value::Object(Default::default()));

            let mut env = Env::with_standard_library().with_all_extensions();

            // Register variables.
            if let Some(Value::Array(vars)) = options.get("variables") {
                for v in vars {
                    if let (Some(name), Some(ty)) =
                        (v.get("name").and_then(|n| n.as_str()), v.get("type"))
                    {
                        if let Some(cel_type) = parse_cel_type(ty) {
                            env = env.with_variable(name, cel_type);
                        }
                    }
                }
            }

            // Register functions.
            if let Some(Value::Array(funcs)) = options.get("functions") {
                for f in funcs {
                    if let Some(decl) = parse_function_decl(f) {
                        env = env.with_function(decl);
                    }
                }
            }

            let hover_show_errors = options
                .get("hoverShowErrors")
                .and_then(|v| v.as_bool())
                .unwrap_or(true);

            Self {
                env: Arc::new(env),
                documents: HashMap::new(),
                hover_show_errors,
            }
        }

        /// Handle an LSP JSON-RPC request. Takes a JSON string, returns a JSON string.
        pub fn handle_request(&mut self, json: &str) -> String {
            let request: Value = match serde_json::from_str(json) {
                Ok(v) => v,
                Err(e) => {
                    return self.json_rpc_error(None, -32700, &format!("Parse error: {}", e));
                }
            };

            let id = request.get("id").cloned();
            let method = request
                .get("method")
                .and_then(|m| m.as_str())
                .unwrap_or("");
            let params = request.get("params").cloned().unwrap_or(Value::Null);

            match method {
                "initialize" => self.handle_initialize(id),
                "initialized" => String::new(),
                "textDocument/didOpen" => self.handle_did_open(&params),
                "textDocument/didChange" => self.handle_did_change(&params),
                "textDocument/completion" => self.handle_completion(id, &params),
                "textDocument/hover" => self.handle_hover(id, &params),
                "textDocument/semanticTokens/full" => {
                    self.handle_semantic_tokens(id, &params)
                }
                "shutdown" | "exit" => self.json_rpc_response(id, Value::Null),
                _ => self.json_rpc_error(
                    id,
                    -32601,
                    &format!("Method not found: {}", method),
                ),
            }
        }

        /// Get semantic tokens for a document (direct call, not JSON-RPC).
        pub fn semantic_tokens(&self, uri: &str) -> String {
            let Some(state) = self.documents.get(uri) else {
                return "null".to_string();
            };

            let Some(ast) = state.ast() else {
                return "null".to_string();
            };

            let tokens = lsp::tokens_for_ast(&state.line_index, ast);
            let result = SemanticTokensResult::Tokens(SemanticTokens {
                result_id: None,
                data: tokens,
            });

            serde_json::to_string(&result).unwrap_or_else(|_| "null".to_string())
        }
    }

    // Private JSON-RPC dispatch helpers.
    impl CelAnalyzer {
        fn handle_initialize(&self, id: Option<Value>) -> String {
            let capabilities = InitializeResult {
                capabilities: ServerCapabilities {
                    text_document_sync: Some(TextDocumentSyncCapability::Kind(
                        TextDocumentSyncKind::FULL,
                    )),
                    hover_provider: Some(HoverProviderCapability::Simple(true)),
                    completion_provider: Some(CompletionOptions {
                        trigger_characters: Some(vec![".".to_string()]),
                        resolve_provider: Some(false),
                        ..Default::default()
                    }),
                    semantic_tokens_provider: Some(
                        SemanticTokensServerCapabilities::SemanticTokensOptions(
                            SemanticTokensOptions {
                                legend: lsp::legend(),
                                full: Some(SemanticTokensFullOptions::Bool(true)),
                                range: None,
                                work_done_progress_options:
                                    WorkDoneProgressOptions::default(),
                            },
                        ),
                    ),
                    ..Default::default()
                },
                ..Default::default()
            };

            let result = serde_json::to_value(&capabilities).unwrap_or(Value::Null);
            self.json_rpc_response(id, result)
        }

        fn handle_did_open(&mut self, params: &Value) -> String {
            let uri = params
                .pointer("/textDocument/uri")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let text = params
                .pointer("/textDocument/text")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let version = params
                .pointer("/textDocument/version")
                .and_then(|v| v.as_i64())
                .unwrap_or(0) as i32;

            let state =
                DocumentState::with_env(text.to_string(), version, Arc::clone(&self.env));
            let diagnostics = lsp::to_diagnostics(
                &state.errors,
                state.check_errors(),
                &state.line_index,
            );
            self.documents.insert(uri.to_string(), state);

            self.diagnostics_notification(uri, version, &diagnostics)
        }

        fn handle_did_change(&mut self, params: &Value) -> String {
            let uri = params
                .pointer("/textDocument/uri")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let version = params
                .pointer("/textDocument/version")
                .and_then(|v| v.as_i64())
                .unwrap_or(0) as i32;
            let text = params
                .pointer("/contentChanges/0/text")
                .and_then(|v| v.as_str())
                .unwrap_or("");

            let state =
                DocumentState::with_env(text.to_string(), version, Arc::clone(&self.env));
            let diagnostics = lsp::to_diagnostics(
                &state.errors,
                state.check_errors(),
                &state.line_index,
            );
            self.documents.insert(uri.to_string(), state);

            self.diagnostics_notification(uri, version, &diagnostics)
        }

        fn handle_completion(&self, id: Option<Value>, params: &Value) -> String {
            let uri = params
                .pointer("/textDocument/uri")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let line = params
                .pointer("/position/line")
                .and_then(|v| v.as_u64())
                .unwrap_or(0) as u32;
            let character = params
                .pointer("/position/character")
                .and_then(|v| v.as_u64())
                .unwrap_or(0) as u32;

            let Some(state) = self.documents.get(uri) else {
                return self.json_rpc_response(id, Value::Null);
            };

            let position = Position::new(line, character);
            let result = lsp::completion_at_position(
                &state.line_index,
                &state.source,
                &state.env,
                position,
            );

            let value = serde_json::to_value(&result).unwrap_or(Value::Null);
            self.json_rpc_response(id, value)
        }

        fn handle_hover(&self, id: Option<Value>, params: &Value) -> String {
            let uri = params
                .pointer("/textDocument/uri")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let line = params
                .pointer("/position/line")
                .and_then(|v| v.as_u64())
                .unwrap_or(0) as u32;
            let character = params
                .pointer("/position/character")
                .and_then(|v| v.as_u64())
                .unwrap_or(0) as u32;

            let Some(state) = self.documents.get(uri) else {
                return self.json_rpc_response(id, Value::Null);
            };

            let position = Position::new(line, character);

            // When hover_show_errors is false, pass a CheckResult with empty
            // errors so hover skips error display but still has the type_map
            // for variable type info.
            let check_ref;
            let filtered;
            if self.hover_show_errors {
                check_ref = state.check_result.as_ref();
            } else if let Some(cr) = &state.check_result {
                filtered = CheckResult {
                    errors: Vec::new(),
                    type_map: cr.type_map.clone(),
                    reference_map: cr.reference_map.clone(),
                };
                check_ref = Some(&filtered);
            } else {
                check_ref = None;
            }

            let result = match state.ast() {
                Some(ast) => lsp::hover_at_position(
                    &state.line_index,
                    ast,
                    check_ref,
                    position,
                ),
                None => None,
            };

            let value = serde_json::to_value(&result).unwrap_or(Value::Null);
            self.json_rpc_response(id, value)
        }

        fn handle_semantic_tokens(&self, id: Option<Value>, params: &Value) -> String {
            let uri = params
                .pointer("/textDocument/uri")
                .and_then(|v| v.as_str())
                .unwrap_or("");

            let Some(state) = self.documents.get(uri) else {
                return self.json_rpc_response(id, Value::Null);
            };

            let result = match state.ast() {
                Some(ast) => {
                    let tokens = lsp::tokens_for_ast(&state.line_index, ast);
                    Some(SemanticTokensResult::Tokens(SemanticTokens {
                        result_id: None,
                        data: tokens,
                    }))
                }
                None => None,
            };

            let value = serde_json::to_value(&result).unwrap_or(Value::Null);
            self.json_rpc_response(id, value)
        }

        fn json_rpc_response(&self, id: Option<Value>, result: Value) -> String {
            serde_json::to_string(&serde_json::json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": result,
            }))
            .unwrap_or_else(|_| "{}".to_string())
        }

        fn json_rpc_error(&self, id: Option<Value>, code: i32, message: &str) -> String {
            serde_json::to_string(&serde_json::json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": { "code": code, "message": message },
            }))
            .unwrap_or_else(|_| "{}".to_string())
        }

        fn diagnostics_notification(
            &self,
            uri: &str,
            version: i32,
            diagnostics: &[Diagnostic],
        ) -> String {
            serde_json::to_string(&serde_json::json!({
                "jsonrpc": "2.0",
                "method": "textDocument/publishDiagnostics",
                "params": {
                    "uri": uri,
                    "version": version,
                    "diagnostics": diagnostics,
                },
            }))
            .unwrap_or_else(|_| "{}".to_string())
        }
    }
}

#[cfg(target_arch = "wasm32")]
pub use wasm::CelAnalyzer;

#[cfg(test)]
#[cfg(not(target_arch = "wasm32"))]
mod tests {
    use super::*;

    #[test]
    fn service_can_be_created() {
        let (_service, _socket) = create_service();
    }
}
