use std::collections::BTreeMap;
use std::sync::Arc;

use crate::function_tool::FunctionCallError;
use crate::tools::context::FunctionToolOutput;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolPayload;
use crate::tools::context::boxed_tool_output;
use crate::tools::handlers::parse_arguments;
use crate::tools::handlers::resolve_tool_environment;
use crate::tools::registry::CoreToolRuntime;
use crate::tools::registry::ToolExecutor;
use crate::unified_exec::ExecCommandRequest;
use crate::unified_exec::MonitorPipeline;
use crate::unified_exec::UnifiedExecContext;
use crate::unified_exec::UnifiedExecOutputMode;
use codex_tools::JsonSchema;
use codex_tools::ResponsesApiTool;
use codex_tools::ToolName;
use codex_tools::ToolSpec;
use serde::Deserialize;
use serde_json::json;

use super::ExecCommandArgs;
use super::get_command;
use super::shell_mode_for_environment;

const MONITOR_TOOL_NAME: &str = "monitor";

/// Time the spawn blocks for initial output before returning. Kept short so the
/// tool call returns quickly while the watcher keeps running in the background.
const MONITOR_YIELD_MS: u64 = 250;

pub struct MonitorHandler;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "lowercase")]
enum MonitorAction {
    Start,
    Stop,
    List,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct MonitorArgs {
    action: MonitorAction,
    #[serde(default)]
    command: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    id: Option<String>,
}

fn create_monitor_tool() -> ToolSpec {
    let properties = BTreeMap::from([
        (
            "action".to_string(),
            JsonSchema::string_enum(
                vec![json!("start"), json!("stop"), json!("list")],
                Some("Which monitor operation to perform.".to_string()),
            ),
        ),
        (
            "command".to_string(),
            JsonSchema::string(Some(
                "Shell command to run as the watcher (action=start). Each line it prints (stdout or stderr) becomes one notification, so filter to the lines you care about (e.g. `tail -F app.log | grep --line-buffered ERROR`).".to_string(),
            )),
        ),
        (
            "description".to_string(),
            JsonSchema::string(Some(
                "Short label prefixed to every notification this watcher emits (action=start), e.g. \"errors in app.log\".".to_string(),
            )),
        ),
        (
            "id".to_string(),
            JsonSchema::string(Some(
                "The monitor id returned by a previous start (action=stop).".to_string(),
            )),
        ),
    ]);

    ToolSpec::Function(ResponsesApiTool {
        name: MONITOR_TOOL_NAME.to_string(),
        description: "Run a shell command as a long-lived background watcher. Each line the command prints (stdout or stderr) is delivered to you as a notification, prefixed with the label; lines emitted close together are batched. The watch ends when the command exits. Use it to react to events without polling: `fswatch <path>` or `inotifywait -m <path>` for file changes, `tail -F <log> | grep --line-buffered <pattern>` for log signals, or a poll loop for remote state. action=start begins a watch and returns its id; action=stop ends the watch with that id; action=list shows active watches."
            .to_string(),
        strict: false,
        defer_loading: None,
        parameters: JsonSchema::object(
            properties,
            Some(vec!["action".to_string()]),
            /*additional_properties*/ Some(false.into()),
        ),
        output_schema: None,
    })
}

impl ToolExecutor<ToolInvocation> for MonitorHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain(MONITOR_TOOL_NAME)
    }

    fn spec(&self) -> ToolSpec {
        create_monitor_tool()
    }

    fn handle<'a>(&'a self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'a>
    where
        ToolInvocation: 'a,
    {
        Box::pin(handle_call(invocation))
    }
}

impl CoreToolRuntime for MonitorHandler {}

async fn handle_call(
    invocation: ToolInvocation,
) -> Result<Box<dyn crate::tools::context::ToolOutput>, FunctionCallError> {
    let ToolInvocation {
        session,
        turn,
        step_context,
        cancellation_token,
        call_id,
        payload,
        ..
    } = invocation;
    let ToolPayload::Function { arguments } = payload else {
        return Err(FunctionCallError::RespondToModel(format!(
            "{MONITOR_TOOL_NAME} handler received unsupported payload"
        )));
    };
    let args: MonitorArgs = parse_arguments(&arguments)?;

    match args.action {
        MonitorAction::Start => {
            start(
                &session,
                &turn,
                step_context,
                cancellation_token,
                call_id,
                args.command,
                args.description,
            )
            .await
        }
        MonitorAction::Stop => {
            let Some(id) = args.id else {
                return Err(FunctionCallError::RespondToModel(
                    "action=stop requires `id`".to_string(),
                ));
            };
            let message = match session.services.monitor_manager.remove(&id).await {
                Some(_) => {
                    format!("Stopped monitor {id}.")
                }
                None => format!("No active monitor with id {id}."),
            };
            Ok(text_output(message))
        }
        MonitorAction::List => {
            let monitors = session.services.monitor_manager.list().await;
            let message = if monitors.is_empty() {
                "No active monitors.".to_string()
            } else {
                monitors
                    .iter()
                    .map(|m| format!("{}  [{}]  {}", m.id, m.description, m.command))
                    .collect::<Vec<_>>()
                    .join("\n")
            };
            Ok(text_output(message))
        }
    }
}

async fn start(
    session: &Arc<crate::session::session::Session>,
    turn: &Arc<crate::session::turn_context::TurnContext>,
    step_context: Arc<crate::session::step_context::StepContext>,
    cancellation_token: tokio_util::sync::CancellationToken,
    call_id: String,
    command: Option<String>,
    description: Option<String>,
) -> Result<Box<dyn crate::tools::context::ToolOutput>, FunctionCallError> {
    let command = command.filter(|c| !c.trim().is_empty()).ok_or_else(|| {
        FunctionCallError::RespondToModel("action=start requires a non-empty `command`".to_string())
    })?;
    let description = description
        .filter(|d| !d.trim().is_empty())
        .ok_or_else(|| {
            FunctionCallError::RespondToModel(
                "action=start requires a non-empty `description`".to_string(),
            )
        })?;

    let context = UnifiedExecContext::new(
        session.clone(),
        Arc::clone(&step_context),
        cancellation_token,
        call_id,
    );
    let Some(turn_environment) =
        resolve_tool_environment(&step_context.environments, /* environment_id */ None)?
    else {
        return Err(FunctionCallError::RespondToModel(
            "unified exec is unavailable in this session".to_string(),
        ));
    };
    let cwd = turn_environment.cwd().clone();
    let environment = Arc::clone(&turn_environment.environment);
    let shell_mode =
        shell_mode_for_environment(&turn.unified_exec_shell_mode, environment.as_ref());
    let shell = turn_environment
        .shell
        .clone()
        .map(Arc::new)
        .unwrap_or_else(|| session.user_shell());

    // Resolve `command` to a concrete shell invocation with the session default
    // shell and no permission escalation; the monitor only needs the resolved
    // command + shell type back from `get_command`.
    let exec_args = ExecCommandArgs {
        cmd: command.clone(),
        shell: None,
        login: None,
        tty: false,
        yield_time_ms: 0,
        timeout_ms: None,
        max_output_tokens: None,
        sandbox_permissions: Default::default(),
        additional_permissions: None,
        justification: None,
        prefix_rule: None,
    };
    let resolved = get_command(
        &exec_args,
        shell,
        &shell_mode,
        turn.config.permissions.allow_login_shell,
    )
    .map_err(FunctionCallError::RespondToModel)?;

    let (pipeline, sink) = MonitorPipeline::new();
    let request = ExecCommandRequest {
        output_mode: UnifiedExecOutputMode::Tagged { sink },
        command: resolved.command,
        shell_type: resolved.shell_type,
        hook_command: command.clone(),
        process_id: 0,
        yield_time_ms: MONITOR_YIELD_MS,
        max_output_tokens: None,
        cwd: cwd.clone(),
        sandbox_cwd: cwd,
        turn_environment: turn_environment.clone(),
        shell_mode,
        network: turn.network.clone(),
        tty: false,
        sandbox_permissions: Default::default(),
        additional_permissions: None,
        additional_permissions_preapproved: false,
        justification: None,
        prefix_rule: None,
    };

    let id = session
        .services
        .monitor_manager
        .start_with_pipeline(session, &context, request, description.clone(), pipeline)
        .await
        .map_err(|error| {
            FunctionCallError::RespondToModel(format!("failed to start monitor: {error}"))
        })?;
    Ok(text_output(format!(
        "Started monitor {id}: watching \"{description}\". Stop it with action=stop, id={id}."
    )))
}

fn text_output(message: String) -> Box<dyn crate::tools::context::ToolOutput> {
    boxed_tool_output(FunctionToolOutput::from_text(
        message,
        /*success*/ Some(true),
    ))
}
