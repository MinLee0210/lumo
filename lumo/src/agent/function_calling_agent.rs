use anyhow::Result;
use async_trait::async_trait;
use futures::future::join_all;
use opentelemetry::trace::{FutureExt, TraceContextExt};
use serde_json::json;
use std::collections::HashMap;

use crate::{
    agent::Agent,
    errors::AgentError,
    models::{
        model_traits::Model,
        openai::{FunctionCall, ToolCall},
        types::Message,
    },
    prompts::TOOL_CALLING_SYSTEM_PROMPT,
    telemetry::AgentTelemetry,
    tools::{AsyncTool, ToolFunctionInfo, ToolGroup, ToolInfo, ToolType},
};
use tracing::instrument;

use super::{agent_step::Step, multistep_agent::MultiStepAgent, AgentStep};

#[cfg(feature = "stream")]
use super::agent_trait::AgentStream;

pub struct FunctionCallingAgent<M>
where
    M: Model + Send + Sync + 'static,
{
    base_agent: MultiStepAgent<M>,
    telemetry: AgentTelemetry,
}

impl<M: Model + Send + Sync + 'static> FunctionCallingAgent<M> {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        name: Option<&str>,
        model: M,
        tools: Vec<Box<dyn AsyncTool>>,
        system_prompt: Option<&str>,
        managed_agents: Vec<Box<dyn Agent>>,
        description: Option<&str>,
        max_steps: Option<usize>,
        planning_interval: Option<usize>,
        history: Option<Vec<Message>>,
        logging_level: Option<log::LevelFilter>,
    ) -> Result<Self> {
        let system_prompt = system_prompt.unwrap_or(TOOL_CALLING_SYSTEM_PROMPT);
        let base_agent = MultiStepAgent::new(
            name,
            model,
            tools,
            Some(system_prompt),
            managed_agents,
            description,
            max_steps,
            planning_interval,
            history,
            logging_level,
        )?;
        Ok(Self {
            base_agent,
            telemetry: AgentTelemetry::new("lumo"),
        })
    }
}

pub struct FunctionCallingAgentBuilder<'a, M>
where
    M: Model + std::fmt::Debug + Send + Sync + 'static,
{
    name: Option<&'a str>,
    model: M,
    tools: Vec<Box<dyn AsyncTool>>,
    system_prompt: Option<&'a str>,
    managed_agents: Vec<Box<dyn Agent>>,
    description: Option<&'a str>,
    max_steps: Option<usize>,
    planning_interval: Option<usize>,
    history: Option<Vec<Message>>,
    logging_level: Option<log::LevelFilter>,
}

impl<'a, M: Model + std::fmt::Debug + Send + Sync + 'static> FunctionCallingAgentBuilder<'a, M> {
    pub fn new(model: M) -> Self {
        Self {
            name: None,
            model,
            tools: vec![],
            system_prompt: None,
            managed_agents: vec![],
            description: None,
            max_steps: None,
            planning_interval: None,
            history: None,
            logging_level: None,
        }
    }
    pub fn with_name(mut self, name: Option<&'a str>) -> Self {
        self.name = name;
        self
    }
    pub fn with_tools(mut self, tools: Vec<Box<dyn AsyncTool>>) -> Self {
        self.tools = tools;
        self
    }
    pub fn with_system_prompt(mut self, system_prompt: Option<&'a str>) -> Self {
        self.system_prompt = system_prompt;
        self
    }
    pub fn with_managed_agents(mut self, managed_agents: Vec<Box<dyn Agent>>) -> Self {
        self.managed_agents = managed_agents;
        self
    }
    pub fn with_description(mut self, description: Option<&'a str>) -> Self {
        self.description = description;
        self
    }
    pub fn with_max_steps(mut self, max_steps: Option<usize>) -> Self {
        self.max_steps = max_steps;
        self
    }
    pub fn with_planning_interval(mut self, planning_interval: Option<usize>) -> Self {
        self.planning_interval = planning_interval;
        self
    }
    pub fn with_history(mut self, history: Option<Vec<Message>>) -> Self {
        self.history = history;
        self
    }
    pub fn with_logging_level(mut self, logging_level: Option<log::LevelFilter>) -> Self {
        self.logging_level = logging_level;
        self
    }
    pub fn build(self) -> Result<FunctionCallingAgent<M>> {
        FunctionCallingAgent::new(
            self.name,
            self.model,
            self.tools,
            self.system_prompt,
            self.managed_agents,
            self.description,
            self.max_steps,
            self.planning_interval,
            self.history,
            self.logging_level,
        )
    }
}

#[async_trait]
impl<M: Model + std::fmt::Debug + Send + Sync + 'static> Agent for FunctionCallingAgent<M> {
    fn name(&self) -> &'static str {
        self.base_agent.name()
    }
    fn description(&self) -> &'static str {
        self.base_agent.description()
    }
    fn set_task(&mut self, task: &str) {
        self.base_agent.set_task(task);
    }
    fn get_task(&self) -> &str {
        self.base_agent.get_task()
    }
    fn get_system_prompt(&self) -> &str {
        self.base_agent.get_system_prompt()
    }
    fn get_planning_interval(&self) -> Option<usize> {
        self.base_agent.get_planning_interval()
    }
    fn get_max_steps(&self) -> usize {
        self.base_agent.get_max_steps()
    }
    fn get_step_number(&self) -> usize {
        self.base_agent.get_step_number()
    }
    fn set_step_number(&mut self, step_number: usize) {
        self.base_agent.set_step_number(step_number)
    }
    fn reset_step_number(&mut self) {
        self.base_agent.reset_step_number();
    }
    fn increment_step_number(&mut self) {
        self.base_agent.increment_step_number();
    }
    fn get_logs_mut(&mut self) -> &mut Vec<Step> {
        self.base_agent.get_logs_mut()
    }
    fn model(&self) -> &dyn Model {
        self.base_agent.model()
    }
    fn set_planning_interval(&mut self, planning_interval: Option<usize>) {
        self.base_agent.set_planning_interval(planning_interval);
    }
    async fn planning_step(
        &mut self,
        task: &str,
        is_first_step: bool,
        step: usize,
    ) -> Result<Option<Step>> {
        self.base_agent
            .planning_step(task, is_first_step, step)
            .await
    }

    /// Perform one step in the ReAct framework: the agent thinks, acts, and observes the result.
    ///
    /// Returns None if the step is not final.
    #[instrument(skip(self, log_entry), fields(step = ?self.get_step_number()))]
    async fn step(&mut self, log_entry: &mut Step) -> Result<Option<AgentStep>, AgentError> {
        match log_entry {
            Step::ActionStep(step_log) => {
                let cx = self.telemetry.start_step(self.get_step_number() as i64);

                let agent_memory = self.base_agent.write_inner_memory_from_logs(None)?;
                self.base_agent.input_messages = Some(agent_memory.clone());
                step_log.agent_memory = Some(agent_memory.clone());
                self.telemetry
                    .log_agent_memory(&serde_json::to_value(&agent_memory).unwrap_or_default());

                let mut tools = self
                    .base_agent
                    .tools
                    .iter()
                    .map(|tool| tool.tool_info())
                    .collect::<Vec<_>>();
                let managed_agents = self
                    .base_agent
                    .managed_agents
                    .iter()
                    .map(|agent| ToolInfo {
                        tool_type: ToolType::Function,
                        function: ToolFunctionInfo {
                            name: agent.name().to_string(),
                            description: agent.description().to_string(),
                            parameters: json!({
                                "type": "object",
                                "properties": {
                                    "task": {
                                        "type": "string",
                                        "description": "The task to perform"
                                    }
                                }
                            }),
                        },
                    })
                    .collect::<Vec<_>>();

                tools.extend(managed_agents);

                let model_message = self
                    .base_agent
                    .model
                    .run(
                        self.base_agent.input_messages.as_ref().unwrap().clone(),
                        self.base_agent.history.clone(),
                        tools,
                        None,
                        Some(HashMap::from([(
                            "stop".to_string(),
                            vec!["Observation:".to_string()],
                        )])),
                    )
                    .with_context(cx.clone())
                    .await?;

                step_log.llm_output = Some(model_message.get_response().unwrap_or_default());
                let mut observations = Vec::new();
                let mut tools = model_message.get_tools_used()?;
                step_log.tool_call = if tools.is_empty() {
                    None
                } else {
                    Some(tools.clone())
                };

                self.telemetry.log_tool_calls(&tools, &cx);

                if let Ok(response) = model_message.get_response() {
                    if !response.trim().is_empty() {
                        if let Ok(action) = parse_response(&response) {
                            tools = vec![ToolCall {
                                id: Some(format!("call_{}", nanoid::nanoid!())),
                                call_type: Some("function".to_string()),
                                function: FunctionCall {
                                    name: action["name"].as_str().unwrap_or_default().to_string(),
                                    arguments: action["arguments"].clone(),
                                },
                            }];
                            step_log.tool_call = Some(tools.clone());
                            self.telemetry.log_tool_calls(&tools, &cx);
                        }
                    }
                    if tools.is_empty() {
                        self.base_agent.write_inner_memory_from_logs(None)?;
                        step_log.final_answer = Some(response.clone());
                        step_log.observations = Some(vec![response.clone()]);
                        self.telemetry.log_final_answer(&response);
                        cx.span().set_attribute(opentelemetry::KeyValue::new(
                            "end_time",
                            chrono::Utc::now().to_rfc3339(),
                        ));
                        cx.span().end_with_timestamp(std::time::SystemTime::now());
                        return Ok(Some(step_log.clone()));
                    }
                }

                if tools.is_empty() {
                    step_log.tool_call = None;
                    observations = vec!["No tool call was made. If this is the final answer, use the final_answer tool to return your answer.".to_string()];
                } else {
                    let tools_ref = &self.base_agent.tools;
                    let mut futures = vec![];
                    let managed_agent_names = self
                        .base_agent
                        .managed_agents
                        .iter()
                        .map(|agent| agent.name())
                        .collect::<Vec<_>>();

                    let mut called_tools = Vec::new();
                    for tool in &tools {
                        let function_name = tool.function.name.clone();
                        match function_name.as_str() {
                            "final_answer" => {
                                let answer = tools_ref.call(&tool.function).await?;
                                step_log.final_answer = Some(answer.clone());
                                step_log.observations = Some(vec![answer.clone()]);
                                self.telemetry.log_final_answer(&answer);
                                cx.span().set_attribute(opentelemetry::KeyValue::new(
                                    "end_time",
                                    chrono::Utc::now().to_rfc3339(),
                                ));
                                cx.span().end_with_timestamp(std::time::SystemTime::now());
                                return Ok(Some(step_log.clone()));
                            }
                            _ => {
                                if !managed_agent_names.contains(&function_name.as_str()) {
                                    let tool_call = tools_ref.call(&tool.function);
                                    tracing::info!(
                                        tool = %function_name,
                                        args = ?tool.function.arguments,
                                        "Executing tool call:"
                                    );
                                    called_tools.push(tool.function.clone());
                                    futures.push(tool_call);
                                } else {
                                    let task = tool.function.arguments.get("task");
                                    if let Some(task) = task {
                                        if let Some(task_str) = task.as_str() {
                                            tracing::info!(
                                                tool = %function_name,
                                                args = ?tool.function.arguments,
                                                "Executing tool call: Agent Selected {}",
                                                function_name
                                            );
                                            let result = self
                                                .base_agent
                                                .managed_agents
                                                .iter_mut()
                                                .find(|agent| {
                                                    agent.name() == function_name.as_str()
                                                })
                                                .unwrap()
                                                .run(task_str, true)
                                                .await?;
                                            observations.push(result);
                                        }
                                    }
                                }
                            }
                        }
                    }
                    // }

                    let results = join_all(futures).await;
                    for (i, result) in results.into_iter().enumerate() {
                        let cx = self.telemetry.log_tool_execution(
                            &called_tools[i].name,
                            &called_tools[i].arguments,
                            &cx,
                        );
                        match result {
                            Ok(result) => {
                                observations.push(result.clone());
                                self.telemetry.log_tool_result(&result, true, &cx);
                            }
                            Err(e) => {
                                observations.push(e.to_string());
                                self.telemetry.log_tool_result(&e.to_string(), false, &cx);
                            }
                        }
                        cx.span().set_attribute(opentelemetry::KeyValue::new(
                            "end_time",
                            chrono::Local::now().to_rfc3339(),
                        ));
                        cx.span().end_with_timestamp(std::time::SystemTime::now());
                    }
                  
                }

                step_log.observations = Some(observations);
                self.telemetry
                    .log_observations(&step_log.observations.clone().unwrap_or_default());
                cx.span().set_attribute(opentelemetry::KeyValue::new(
                    "end_time",
                    chrono::Local::now().to_rfc3339(),
                ));
                cx.span().end_with_timestamp(std::time::SystemTime::now());
                Ok(Some(step_log.clone()))
            }
            _ => {
                todo!()
            }
        }
    }
}

fn extract_action_json(text: &str) -> Option<String> {
    // First try to extract from Action: format
    if let Some(action_part) = text.split("Action:").nth(1) {
        let start = action_part.find('{');
        let end = action_part.rfind('}');
        if let (Some(start_idx), Some(end_idx)) = (start, end) {
            let json_str = action_part[start_idx..=end_idx].to_string();
            // Clean and escape the string
            return Some(json_str.replace('\n', "\\n").replace('\r', "\\r"));
        }
    }

    // If no Action: format found, try extracting from tool_call tags
    if let Some(tool_call_part) = text.split("<tool_call>").nth(1) {
        if let Some(content) = tool_call_part.split("</tool_call>").next() {
            let trimmed = content.trim();
            if trimmed.starts_with('{') && trimmed.ends_with('}') {
                // Clean and escape the string
                return Some(trimmed.replace('\n', "\\n").replace('\r', "\\r"));
            }
        }
    }

    None
}
// Example usage in your parse_response function:
pub fn parse_response(response: &str) -> Result<serde_json::Value, AgentError> {
    if let Some(json_str) = extract_action_json(response) {
        serde_json::from_str(&json_str).map_err(|e| AgentError::Parsing(e.to_string()))
    } else {
        Err(AgentError::Parsing(
            "No valid action JSON found".to_string(),
        ))
    }
}

#[cfg(feature = "stream")]
impl<M: Model + std::fmt::Debug + Send + Sync + 'static> AgentStream for FunctionCallingAgent<M> {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_action_json() {
        let response = r#"<tool_call>
{"name": "final_answer", "arguments": {"answer": "This is the final answer"}}
</tool_call>"#;
        let json_str = extract_action_json(response);
        assert_eq!(json_str, Some("{\"name\": \"final_answer\", \"arguments\": {\"answer\": \"This is the final answer\"}}".to_string()));
    }

    #[test]
    fn test_parse_response() {
        let response = r#"Okay, let's check the weather in Eindhoven.

<tool_call>
{"name": "search", "arguments": {"query": "weather in eindhoven"}}
</tool_call>"#;
        let json_str = parse_response(response).unwrap();
        println!(
            "json_str: {}",
            serde_json::to_string_pretty(&json_str).unwrap()
        );
        // assert_eq!(json_str, serde_json::json!({"name": "final_answer", "arguments": {"answer": "This is the final answer"}}));
    }
}
