use crate::function_tool::FunctionCallError;
use crate::tools::context::FunctionToolOutput;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolPayload;
use crate::tools::context::boxed_tool_output;
use crate::tools::handlers::parse_arguments;
use crate::tools::handlers::shell_spec::CommandToolOptions;
use crate::tools::handlers::shell_spec::create_monitor_stop_tool;
use crate::tools::handlers::shell_spec::create_monitor_tool;
use crate::tools::registry::CoreToolRuntime;
use crate::tools::registry::PostToolUsePayload;
use crate::tools::registry::PreToolUsePayload;
use crate::tools::registry::ToolExecutor;
use codex_tools::ToolName;
use codex_tools::ToolSpec;
use serde::Deserialize;

use super::ExecCommandHandler;
use super::ExecCommandHandlerOptions;
use super::exec_command::ExecCommandMode;
use super::exec_command::rewrite_unified_exec_hook_input;
use super::exec_command::unified_exec_pre_tool_use_payload;
use super::post_unified_exec_tool_use_payload;

pub struct MonitorHandler {
    exec: ExecCommandHandler,
    options: ExecCommandHandlerOptions,
}

impl MonitorHandler {
    pub(crate) fn new(options: ExecCommandHandlerOptions) -> Self {
        Self {
            exec: ExecCommandHandler::new(options),
            options,
        }
    }
}

impl ToolExecutor<ToolInvocation> for MonitorHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain("monitor")
    }

    fn spec(&self) -> ToolSpec {
        create_monitor_tool(
            CommandToolOptions {
                allow_login_shell: self.options.allow_login_shell,
                exec_permission_approvals_enabled: self.options.exec_permission_approvals_enabled,
            },
            self.options.include_environment_id,
            self.options.include_shell_parameter,
        )
    }

    fn handle(&self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'_> {
        Box::pin(self.exec.handle_call(invocation, ExecCommandMode::Monitor))
    }
}

impl CoreToolRuntime for MonitorHandler {
    fn matches_kind(&self, payload: &ToolPayload) -> bool {
        matches!(payload, ToolPayload::Function { .. })
    }

    fn pre_tool_use_payload(&self, invocation: &ToolInvocation) -> Option<PreToolUsePayload> {
        let ToolPayload::Function { arguments } = &invocation.payload else {
            return None;
        };
        unified_exec_pre_tool_use_payload(arguments)
    }

    fn with_updated_hook_input(
        &self,
        invocation: ToolInvocation,
        updated_input: serde_json::Value,
    ) -> Result<ToolInvocation, FunctionCallError> {
        rewrite_unified_exec_hook_input(invocation, updated_input, "monitor")
    }

    fn post_tool_use_payload(
        &self,
        invocation: &ToolInvocation,
        result: &dyn crate::tools::context::ToolOutput,
    ) -> Option<PostToolUsePayload> {
        post_unified_exec_tool_use_payload(invocation, result)
    }
}

pub struct MonitorStopHandler;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct MonitorStopArgs {
    session_id: i32,
}

impl ToolExecutor<ToolInvocation> for MonitorStopHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain("monitor_stop")
    }

    fn spec(&self) -> ToolSpec {
        create_monitor_stop_tool()
    }

    fn handle(&self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'_> {
        Box::pin(async move {
            let ToolInvocation {
                session, payload, ..
            } = invocation;
            let ToolPayload::Function { arguments } = payload else {
                return Err(FunctionCallError::RespondToModel(
                    "monitor_stop handler received unsupported payload".to_string(),
                ));
            };
            let args: MonitorStopArgs = parse_arguments(&arguments)?;
            let stopped = session
                .services
                .unified_exec_manager
                .terminate_monitor(args.session_id)
                .await;
            let message = if stopped {
                format!("Stopped monitor {}.", args.session_id)
            } else {
                format!("No running monitor found for session {}.", args.session_id)
            };
            Ok(boxed_tool_output(FunctionToolOutput::from_text(
                message,
                Some(stopped),
            )))
        })
    }
}

impl CoreToolRuntime for MonitorStopHandler {}
