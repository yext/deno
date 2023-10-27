// Copyright 2018-2023 the Deno authors. All rights reserved. MIT license.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use crate::args::CliOptions;
use crate::colors;
use crate::lsp::ReplLanguageServer;
use crate::npm::CliNpmResolver;
use crate::resolver::CliGraphResolver;
use crate::tools::test::report_tests;
use crate::tools::test::reporters::PrettyTestReporter;
use crate::tools::test::reporters::TestReporter;
use crate::tools::test::run_tests_for_worker;
use crate::tools::test::worker_has_tests;
use crate::tools::test::TestEvent;
use crate::tools::test::TestEventSender;

use deno_ast::swc::ast as swc_ast;
use deno_ast::swc::visit::noop_visit_type;
use deno_ast::swc::visit::Visit;
use deno_ast::swc::visit::VisitWith;
use deno_ast::DiagnosticsError;
use deno_ast::ImportsNotUsedAsValues;
use deno_ast::ModuleSpecifier;
use deno_core::error::AnyError;
use deno_core::futures::channel::mpsc::UnboundedReceiver;
use deno_core::futures::FutureExt;
use deno_core::futures::StreamExt;
use deno_core::serde_json;
use deno_core::serde_json::Value;
use deno_core::unsync::spawn;
use deno_core::LocalInspectorSession;
use deno_graph::source::ResolutionMode;
use deno_graph::source::Resolver;
use deno_runtime::worker::MainWorker;
use deno_semver::npm::NpmPackageReqReference;
use once_cell::sync::Lazy;

use super::cdp;

/// We store functions used in the repl on this object because
/// the user might modify the `Deno` global or delete it outright.
pub static REPL_INTERNALS_NAME: Lazy<String> = Lazy::new(|| {
  let now = std::time::SystemTime::now();
  let seconds = now
    .duration_since(std::time::SystemTime::UNIX_EPOCH)
    .unwrap()
    .as_secs();
  // use a changing variable name to make it hard to depend on this
  format!("__DENO_REPL_INTERNALS_{seconds}__")
});

fn get_prelude() -> String {
  format!(
    r#"
Object.defineProperty(globalThis, "{0}", {{
  enumerable: false,
  writable: false,
  value: {{
    lastEvalResult: undefined,
    lastThrownError: undefined,
    inspectArgs: Deno[Deno.internal].inspectArgs,
    noColor: Deno.noColor,
  }},
}});
Object.defineProperty(globalThis, "_", {{
  configurable: true,
  get: () => {0}.lastEvalResult,
  set: (value) => {{
   Object.defineProperty(globalThis, "_", {{
     value: value,
     writable: true,
     enumerable: true,
     configurable: true,
   }});
   console.log("Last evaluation result is no longer saved to _.");
  }},
}});

Object.defineProperty(globalThis, "_error", {{
  configurable: true,
  get: () => {0}.lastThrownError,
  set: (value) => {{
   Object.defineProperty(globalThis, "_error", {{
     value: value,
     writable: true,
     enumerable: true,
     configurable: true,
   }});

   console.log("Last thrown error is no longer saved to _error.");
  }},
}});

globalThis.clear = console.clear.bind(console);
"#,
    *REPL_INTERNALS_NAME
  )
}

pub enum EvaluationOutput {
  Value(String),
  Error(String),
}

impl std::fmt::Display for EvaluationOutput {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match self {
      EvaluationOutput::Value(value) => f.write_str(value),
      EvaluationOutput::Error(value) => f.write_str(value),
    }
  }
}

pub fn result_to_evaluation_output(
  r: Result<EvaluationOutput, AnyError>,
) -> EvaluationOutput {
  match r {
    Ok(value) => value,
    Err(err) => {
      EvaluationOutput::Error(format!("{} {:#}", colors::red("error:"), err))
    }
  }
}

#[derive(Debug)]
pub struct TsEvaluateResponse {
  pub ts_code: String,
  pub value: cdp::EvaluateResponse,
}

pub struct ReplSession {
  npm_resolver: Arc<dyn CliNpmResolver>,
  resolver: Arc<CliGraphResolver>,
  pub worker: MainWorker,
  session: LocalInspectorSession,
  pub context_id: u64,
  pub language_server: ReplLanguageServer,
  pub notifications: Rc<RefCell<UnboundedReceiver<Value>>>,
  referrer: ModuleSpecifier,
  main_module: ModuleSpecifier,
  test_reporter_factory: Box<dyn Fn() -> Box<dyn TestReporter>>,
  test_event_sender: TestEventSender,
  /// This is only optional because it's temporarily taken when evaluating.
  test_event_receiver: Option<tokio::sync::mpsc::UnboundedReceiver<TestEvent>>,
}

impl ReplSession {
  pub async fn initialize(
    cli_options: &CliOptions,
    npm_resolver: Arc<dyn CliNpmResolver>,
    resolver: Arc<CliGraphResolver>,
    mut worker: MainWorker,
    main_module: ModuleSpecifier,
    test_event_sender: TestEventSender,
    test_event_receiver: tokio::sync::mpsc::UnboundedReceiver<TestEvent>,
  ) -> Result<Self, AnyError> {
    let language_server = ReplLanguageServer::new_initialized().await?;
    let mut session = worker.create_inspector_session().await;

    worker
      .with_event_loop(
        session
          .post_message::<()>("Runtime.enable", None)
          .boxed_local(),
      )
      .await?;

    // Enabling the runtime domain will always send trigger one executionContextCreated for each
    // context the inspector knows about so we grab the execution context from that since
    // our inspector does not support a default context (0 is an invalid context id).
    let context_id: u64;
    let mut notification_rx = session.take_notification_rx();

    loop {
      let notification = notification_rx.next().await.unwrap();
      let method = notification.get("method").unwrap().as_str().unwrap();
      let params = notification.get("params").unwrap();
      if method == "Runtime.executionContextCreated" {
        let context = params.get("context").unwrap();
        assert!(context
          .get("auxData")
          .unwrap()
          .get("isDefault")
          .unwrap()
          .as_bool()
          .unwrap());
        context_id = context.get("id").unwrap().as_u64().unwrap();
        break;
      }
    }
    assert_ne!(context_id, 0);

    let referrer =
      deno_core::resolve_path("./$deno$repl.ts", cli_options.initial_cwd())
        .unwrap();

    let mut repl_session = ReplSession {
      npm_resolver,
      resolver,
      worker,
      session,
      context_id,
      language_server,
      referrer,
      notifications: Rc::new(RefCell::new(notification_rx)),
      test_reporter_factory: Box::new(|| {
        Box::new(PrettyTestReporter::new(false, true, false, true))
      }),
      main_module,
      test_event_sender,
      test_event_receiver: Some(test_event_receiver),
    };

    // inject prelude
    repl_session.evaluate_expression(&get_prelude()).await?;

    Ok(repl_session)
  }

  pub fn set_test_reporter_factory(
    &mut self,
    f: Box<dyn Fn() -> Box<dyn TestReporter>>,
  ) {
    self.test_reporter_factory = f;
  }

  pub async fn closing(&mut self) -> Result<bool, AnyError> {
    let closed = self
      .evaluate_expression("(this.closed)")
      .await?
      .result
      .value
      .unwrap()
      .as_bool()
      .unwrap();

    Ok(closed)
  }

  pub async fn post_message_with_event_loop<T: serde::Serialize>(
    &mut self,
    method: &str,
    params: Option<T>,
  ) -> Result<Value, AnyError> {
    self
      .worker
      .with_event_loop(self.session.post_message(method, params).boxed_local())
      .await
  }

  pub async fn run_event_loop(&mut self) -> Result<(), AnyError> {
    self.worker.run_event_loop(true).await
  }

  pub async fn evaluate_line_and_get_output(
    &mut self,
    line: &str,
  ) -> EvaluationOutput {
    fn format_diagnostic(diagnostic: &deno_ast::Diagnostic) -> String {
      let display_position = diagnostic.display_position();
      format!(
        "{}: {} at {}:{}",
        colors::red("parse error"),
        diagnostic.message(),
        display_position.line_number,
        display_position.column_number,
      )
    }

    async fn inner(
      session: &mut ReplSession,
      line: &str,
    ) -> Result<EvaluationOutput, AnyError> {
      match session.evaluate_line_with_object_wrapping(line).await {
        Ok(evaluate_response) => {
          let cdp::EvaluateResponse {
            result,
            exception_details,
          } = evaluate_response.value;

          Ok(if let Some(exception_details) = exception_details {
            session.set_last_thrown_error(&result).await?;
            let description = match exception_details.exception {
              Some(exception) => {
                if let Some(description) = exception.description {
                  description
                } else if let Some(value) = exception.value {
                  value.to_string()
                } else {
                  "undefined".to_string()
                }
              }
              None => "Unknown exception".to_string(),
            };
            EvaluationOutput::Error(format!(
              "{} {}",
              exception_details.text, description
            ))
          } else {
            session
              .language_server
              .commit_text(&evaluate_response.ts_code)
              .await;

            session.set_last_eval_result(&result).await?;
            let value = session.get_eval_value(&result).await?;
            EvaluationOutput::Value(value)
          })
        }
        Err(err) => {
          // handle a parsing diagnostic
          match err.downcast_ref::<deno_ast::Diagnostic>() {
            Some(diagnostic) => {
              Ok(EvaluationOutput::Error(format_diagnostic(diagnostic)))
            }
            None => match err.downcast_ref::<DiagnosticsError>() {
              Some(diagnostics) => Ok(EvaluationOutput::Error(
                diagnostics
                  .0
                  .iter()
                  .map(format_diagnostic)
                  .collect::<Vec<_>>()
                  .join("\n\n"),
              )),
              None => Err(err),
            },
          }
        }
      }
    }

    let result = inner(self, line).await;
    result_to_evaluation_output(result)
  }

  pub async fn evaluate_line_with_object_wrapping(
    &mut self,
    line: &str,
  ) -> Result<TsEvaluateResponse, AnyError> {
    // Expressions like { "foo": "bar" } are interpreted as block expressions at the
    // statement level rather than an object literal so we interpret it as an expression statement
    // to match the behavior found in a typical prompt including browser developer tools.
    let wrapped_line = if line.trim_start().starts_with('{')
      && !line.trim_end().ends_with(';')
    {
      format!("({})", &line)
    } else {
      line.to_string()
    };

    let evaluate_response = self.evaluate_ts_expression(&wrapped_line).await;

    // If that fails, we retry it without wrapping in parens letting the error bubble up to the
    // user if it is still an error.
    let result = if wrapped_line != line
      && (evaluate_response.is_err()
        || evaluate_response
          .as_ref()
          .unwrap()
          .value
          .exception_details
          .is_some())
    {
      self.evaluate_ts_expression(line).await
    } else {
      evaluate_response
    };

    if worker_has_tests(&mut self.worker) {
      let report_tests_handle = spawn(report_tests(
        self.test_event_receiver.take().unwrap(),
        (self.test_reporter_factory)(),
      ));
      run_tests_for_worker(
        &mut self.worker,
        &self.main_module,
        &Default::default(),
        &Default::default(),
      )
      .await
      .unwrap();
      self
        .test_event_sender
        .send(TestEvent::ForceEndReport)
        .unwrap();
      self.test_event_receiver = Some(report_tests_handle.await.unwrap().1);
    }

    result
  }

  async fn set_last_thrown_error(
    &mut self,
    error: &cdp::RemoteObject,
  ) -> Result<(), AnyError> {
    self
      .post_message_with_event_loop(
        "Runtime.callFunctionOn",
        Some(cdp::CallFunctionOnArgs {
          function_declaration: format!(
            r#"function (object) {{ {}.lastThrownError = object; }}"#,
            *REPL_INTERNALS_NAME
          ),
          object_id: None,
          arguments: Some(vec![error.into()]),
          silent: None,
          return_by_value: None,
          generate_preview: None,
          user_gesture: None,
          await_promise: None,
          execution_context_id: Some(self.context_id),
          object_group: None,
          throw_on_side_effect: None,
        }),
      )
      .await?;
    Ok(())
  }

  async fn set_last_eval_result(
    &mut self,
    evaluate_result: &cdp::RemoteObject,
  ) -> Result<(), AnyError> {
    self
      .post_message_with_event_loop(
        "Runtime.callFunctionOn",
        Some(cdp::CallFunctionOnArgs {
          function_declaration: format!(
            r#"function (object) {{ {}.lastEvalResult = object; }}"#,
            *REPL_INTERNALS_NAME
          ),
          object_id: None,
          arguments: Some(vec![evaluate_result.into()]),
          silent: None,
          return_by_value: None,
          generate_preview: None,
          user_gesture: None,
          await_promise: None,
          execution_context_id: Some(self.context_id),
          object_group: None,
          throw_on_side_effect: None,
        }),
      )
      .await?;
    Ok(())
  }

  pub async fn call_function_on_args(
    &mut self,
    function_declaration: String,
    args: &[cdp::RemoteObject],
  ) -> Result<cdp::CallFunctionOnResponse, AnyError> {
    let arguments: Option<Vec<cdp::CallArgument>> = if args.is_empty() {
      None
    } else {
      Some(args.iter().map(|a| a.into()).collect())
    };

    let inspect_response = self
      .post_message_with_event_loop(
        "Runtime.callFunctionOn",
        Some(cdp::CallFunctionOnArgs {
          function_declaration,
          object_id: None,
          arguments,
          silent: None,
          return_by_value: None,
          generate_preview: None,
          user_gesture: None,
          await_promise: None,
          execution_context_id: Some(self.context_id),
          object_group: None,
          throw_on_side_effect: None,
        }),
      )
      .await?;

    let response: cdp::CallFunctionOnResponse =
      serde_json::from_value(inspect_response)?;
    Ok(response)
  }

  pub async fn get_eval_value(
    &mut self,
    evaluate_result: &cdp::RemoteObject,
  ) -> Result<String, AnyError> {
    // TODO(caspervonb) we should investigate using previews here but to keep things
    // consistent with the previous implementation we just get the preview result from
    // Deno.inspectArgs.
    let response = self
      .call_function_on_args(
        format!(
          r#"function (object) {{
          try {{
            return {0}.inspectArgs(["%o", object], {{ colors: !{0}.noColor }});
          }} catch (err) {{
            return {0}.inspectArgs(["%o", err]);
          }}
        }}"#,
          *REPL_INTERNALS_NAME
        ),
        &[evaluate_result.clone()],
      )
      .await?;
    let value = response.result.value.unwrap();
    let s = value.as_str().unwrap();

    Ok(s.to_string())
  }

  async fn evaluate_ts_expression(
    &mut self,
    expression: &str,
  ) -> Result<TsEvaluateResponse, AnyError> {
    let parsed_module = deno_ast::parse_module(deno_ast::ParseParams {
      specifier: "repl.ts".to_string(),
      text_info: deno_ast::SourceTextInfo::from_string(expression.to_string()),
      media_type: deno_ast::MediaType::TypeScript,
      capture_tokens: false,
      maybe_syntax: None,
      scope_analysis: false,
    })?;

    self
      .check_for_npm_or_node_imports(&parsed_module.program())
      .await?;

    let transpiled_src = parsed_module
      .transpile(&deno_ast::EmitOptions {
        emit_metadata: false,
        source_map: false,
        inline_source_map: false,
        inline_sources: false,
        imports_not_used_as_values: ImportsNotUsedAsValues::Preserve,
        // JSX is not supported in the REPL
        transform_jsx: false,
        jsx_automatic: false,
        jsx_development: false,
        jsx_factory: "React.createElement".into(),
        jsx_fragment_factory: "React.Fragment".into(),
        jsx_import_source: None,
        precompile_jsx: false,
        var_decl_imports: true,
      })?
      .text;

    let value = self
      .evaluate_expression(&format!("'use strict'; void 0;\n{transpiled_src}"))
      .await?;

    Ok(TsEvaluateResponse {
      ts_code: expression.to_string(),
      value,
    })
  }

  async fn check_for_npm_or_node_imports(
    &mut self,
    program: &swc_ast::Program,
  ) -> Result<(), AnyError> {
    let Some(npm_resolver) = self.npm_resolver.as_managed() else {
      return Ok(()); // don't auto-install for byonm
    };

    let mut collector = ImportCollector::new();
    program.visit_with(&mut collector);

    let resolved_imports = collector
      .imports
      .iter()
      .flat_map(|i| {
        self
          .resolver
          .resolve(i, &self.referrer, ResolutionMode::Execution)
          .ok()
          .or_else(|| ModuleSpecifier::parse(i).ok())
      })
      .collect::<Vec<_>>();

    let npm_imports = resolved_imports
      .iter()
      .flat_map(|url| NpmPackageReqReference::from_specifier(url).ok())
      .map(|r| r.into_inner().req)
      .collect::<Vec<_>>();
    let has_node_specifier =
      resolved_imports.iter().any(|url| url.scheme() == "node");
    if !npm_imports.is_empty() || has_node_specifier {
      npm_resolver.add_package_reqs(&npm_imports).await?;

      // prevent messages in the repl about @types/node not being cached
      if has_node_specifier {
        npm_resolver.inject_synthetic_types_node_package().await?;
      }
    }
    Ok(())
  }

  async fn evaluate_expression(
    &mut self,
    expression: &str,
  ) -> Result<cdp::EvaluateResponse, AnyError> {
    self
      .post_message_with_event_loop(
        "Runtime.evaluate",
        Some(cdp::EvaluateArgs {
          expression: expression.to_string(),
          object_group: None,
          include_command_line_api: None,
          silent: None,
          context_id: Some(self.context_id),
          return_by_value: None,
          generate_preview: None,
          user_gesture: None,
          await_promise: None,
          throw_on_side_effect: None,
          timeout: None,
          disable_breaks: None,
          repl_mode: Some(true),
          allow_unsafe_eval_blocked_by_csp: None,
          unique_context_id: None,
        }),
      )
      .await
      .and_then(|res| serde_json::from_value(res).map_err(|e| e.into()))
  }
}

/// Walk an AST and get all import specifiers for analysis if any of them is
/// an npm specifier.
struct ImportCollector {
  pub imports: Vec<String>,
}

impl ImportCollector {
  pub fn new() -> Self {
    Self { imports: vec![] }
  }
}

impl Visit for ImportCollector {
  noop_visit_type!();

  fn visit_call_expr(&mut self, call_expr: &swc_ast::CallExpr) {
    if !matches!(call_expr.callee, swc_ast::Callee::Import(_)) {
      return;
    }

    if !call_expr.args.is_empty() {
      let arg = &call_expr.args[0];
      if let swc_ast::Expr::Lit(swc_ast::Lit::Str(str_lit)) = &*arg.expr {
        self.imports.push(str_lit.value.to_string());
      }
    }
  }

  fn visit_module_decl(&mut self, module_decl: &swc_ast::ModuleDecl) {
    use deno_ast::swc::ast::*;

    match module_decl {
      ModuleDecl::Import(import_decl) => {
        if import_decl.type_only {
          return;
        }

        self.imports.push(import_decl.src.value.to_string());
      }
      ModuleDecl::ExportAll(export_all) => {
        self.imports.push(export_all.src.value.to_string());
      }
      ModuleDecl::ExportNamed(export_named) => {
        if let Some(src) = &export_named.src {
          self.imports.push(src.value.to_string());
        }
      }
      _ => {}
    }
  }
}
