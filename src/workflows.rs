/// Workflows: parameterized command templates with fuzzy search and interactive filling.
/// Inspired by Warp's Workflows but local-first and extensible.
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::path::PathBuf;

const MAX_WORKFLOW_FILE_BYTES: usize = 1024 * 1024;
const MAX_WORKFLOW_FILES: usize = 256;
const MAX_WORKFLOWS: usize = 4096;
const MAX_WORKFLOW_TEXT_BYTES: usize = 16 * 1024;
const MAX_WORKFLOW_COMMAND_BYTES: usize = 64 * 1024;
const MAX_WORKFLOW_PARAMETERS: usize = 128;
const MAX_WORKFLOW_SUGGESTIONS: usize = 1024;
const MAX_RENDERED_COMMAND_BYTES: usize = 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Workflow {
    pub name: String,
    pub description: String,
    pub command: String,
    #[serde(default)]
    pub parameters: Vec<WorkflowParam>,
    #[serde(default)]
    pub tags: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowParam {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub default: Option<String>,
    #[serde(default)]
    pub suggestions: Vec<String>,
}

pub struct WorkflowRegistry {
    workflows: Vec<Workflow>,
    user_dir: PathBuf,
}

impl Default for WorkflowRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl WorkflowRegistry {
    pub fn new() -> Self {
        let user_dir = dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("/tmp"))
            .join(".jsh")
            .join("workflows");

        let mut registry = WorkflowRegistry {
            workflows: Vec::new(),
            user_dir,
        };
        registry.load_builtin();
        registry.load_user();
        registry
    }

    fn load_builtin(&mut self) {
        let builtin: &str = include_str!("specs/workflows.json");
        if let Ok(wfs) = serde_json::from_str::<Vec<Workflow>>(builtin) {
            self.extend_validated(wfs);
        }
    }

    fn load_user(&mut self) {
        if !self.user_dir.exists() {
            return;
        }
        if let Ok(entries) = std::fs::read_dir(&self.user_dir) {
            let mut paths: Vec<PathBuf> = entries.flatten().map(|entry| entry.path()).collect();
            paths.sort();
            for path in paths.into_iter().take(MAX_WORKFLOW_FILES) {
                if path.extension().map(|e| e == "json").unwrap_or(false) {
                    if let Ok(content) =
                        crate::io_guard::read_regular_text(&path, MAX_WORKFLOW_FILE_BYTES)
                    {
                        if let Ok(wf) = serde_json::from_str::<Workflow>(&content) {
                            self.extend_validated(std::iter::once(wf));
                        } else if let Ok(wfs) = serde_json::from_str::<Vec<Workflow>>(&content) {
                            self.extend_validated(wfs);
                        }
                    }
                }
            }
        }
    }

    fn extend_validated(&mut self, workflows: impl IntoIterator<Item = Workflow>) {
        for workflow in workflows {
            if validate_workflow(&workflow).is_err() {
                continue;
            }
            if let Some(existing) = self
                .workflows
                .iter_mut()
                .find(|existing| existing.name == workflow.name)
            {
                // A user workflow with the same exact name intentionally
                // replaces the built-in definition. Sorted file loading keeps
                // this deterministic even when several user files overlap.
                *existing = workflow;
            } else if self.workflows.len() < MAX_WORKFLOWS {
                self.workflows.push(workflow);
            }
        }
    }

    pub fn search(&self, query: &str) -> Vec<&Workflow> {
        if query.is_empty() {
            return self.all();
        }
        let query_lower = query.to_lowercase();
        let mut results: Vec<(&Workflow, i32)> = Vec::new();

        for wf in &self.workflows {
            let mut score = 0i32;
            let name_lower = wf.name.to_lowercase();
            let desc_lower = wf.description.to_lowercase();

            if name_lower.contains(&query_lower) {
                score += 100;
                if name_lower.starts_with(&query_lower) {
                    score += 50;
                }
            }
            if desc_lower.contains(&query_lower) {
                score += 50;
            }
            for tag in &wf.tags {
                if tag.to_lowercase().contains(&query_lower) {
                    score += 30;
                }
            }

            // Fuzzy match on name
            if score == 0 {
                let query_chars: Vec<char> = query_lower.chars().collect();
                let name_chars: Vec<char> = name_lower.chars().collect();
                let mut qi = 0;
                for &nc in &name_chars {
                    if qi < query_chars.len() && nc == query_chars[qi] {
                        qi += 1;
                    }
                }
                if qi == query_chars.len() {
                    score += 20;
                }
            }

            if score > 0 {
                results.push((wf, score));
            }
        }

        results.sort_by(
            |(left_workflow, left_score), (right_workflow, right_score)| {
                right_score
                    .cmp(left_score)
                    .then_with(|| left_workflow.name.cmp(&right_workflow.name))
            },
        );
        results.into_iter().map(|(wf, _)| wf).collect()
    }

    /// Find a workflow by its exact, case-sensitive name.
    pub fn get(&self, name: &str) -> Option<&Workflow> {
        self.workflows.iter().find(|workflow| workflow.name == name)
    }

    /// Return every workflow in stable name order.
    pub fn all(&self) -> Vec<&Workflow> {
        let mut workflows: Vec<&Workflow> = self.workflows.iter().collect();
        workflows.sort_by(|left, right| {
            left.name
                .cmp(&right.name)
                .then_with(|| left.command.cmp(&right.command))
        });
        workflows
    }

    pub fn count(&self) -> usize {
        self.workflows.len()
    }
}

fn bounded_safe_text(value: &str, max_bytes: usize) -> bool {
    value.len() <= max_bytes && crate::terminal_text::is_safe_inline(value)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkflowError {
    InvalidField(&'static str),
    InvalidTemplate(&'static str),
    InvalidParameterName(String),
    DuplicateParameter(String),
    UndeclaredPlaceholder(String),
    UnusedParameter(String),
    DuplicateValue(String),
    MissingValue(String),
    InvalidValue(String),
    RenderedCommandTooLong,
    SessionComplete,
    SessionIncomplete,
}

impl fmt::Display for WorkflowError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidField(field) => write!(formatter, "invalid workflow {field}"),
            Self::InvalidTemplate(reason) => {
                write!(formatter, "invalid workflow template: {reason}")
            }
            Self::InvalidParameterName(name) => write!(formatter, "invalid parameter name: {name}"),
            Self::DuplicateParameter(name) => write!(formatter, "duplicate parameter: {name}"),
            Self::UndeclaredPlaceholder(name) => {
                write!(formatter, "template placeholder has no parameter: {name}")
            }
            Self::UnusedParameter(name) => {
                write!(formatter, "parameter is not used by the template: {name}")
            }
            Self::DuplicateValue(name) => {
                write!(formatter, "duplicate value for parameter: {name}")
            }
            Self::MissingValue(name) => write!(formatter, "missing value for parameter: {name}"),
            Self::InvalidValue(name) => write!(formatter, "invalid value for parameter: {name}"),
            Self::RenderedCommandTooLong => write!(
                formatter,
                "rendered workflow command exceeds {MAX_RENDERED_COMMAND_BYTES} bytes"
            ),
            Self::SessionComplete => write!(formatter, "workflow session is already complete"),
            Self::SessionIncomplete => write!(formatter, "workflow session is not complete"),
        }
    }
}

impl std::error::Error for WorkflowError {}

fn valid_parameter_name(name: &str) -> bool {
    let mut chars = name.chars();
    matches!(chars.next(), Some(first) if first == '_' || first.is_alphabetic())
        && chars
            .all(|character| character == '_' || character == '-' || character.is_alphanumeric())
}

fn parse_placeholders(command: &str) -> Result<Vec<String>, WorkflowError> {
    let mut parameters = Vec::new();
    let mut seen = HashSet::new();
    let mut offset = 0;

    while offset < command.len() {
        let remaining = &command[offset..];
        let opening = remaining.find("{{");
        let Some(opening) = opening else {
            break;
        };
        let name_start = offset + opening + 2;
        let Some(relative_end) = command[name_start..].find("}}") else {
            // `{{` is also valid literal text in shell snippets and embedded
            // Go/Helm/Jinja templates. Only a complete, valid jsh parameter
            // marker is discoverable here.
            offset = name_start;
            continue;
        };
        let name_end = name_start + relative_end;
        let name = &command[name_start..name_end];
        if valid_parameter_name(name) {
            if seen.insert(name) {
                parameters.push(name.to_string());
            }
            offset = name_end + 2;
        } else {
            // Do not consume an unknown moustache wholesale: it may contain a
            // later, valid jsh marker (for example an unclosed literal before
            // `{{port}}`). Move past only this opening delimiter.
            offset = name_start;
        }
    }

    Ok(parameters)
}

/// Validate text bounds and ensure the declared parameters exactly match the
/// placeholders used by the command template.
pub fn validate_workflow(workflow: &Workflow) -> Result<(), WorkflowError> {
    if !bounded_safe_text(&workflow.name, MAX_WORKFLOW_TEXT_BYTES) {
        return Err(WorkflowError::InvalidField("name"));
    }
    if !bounded_safe_text(&workflow.description, MAX_WORKFLOW_TEXT_BYTES) {
        return Err(WorkflowError::InvalidField("description"));
    }
    if !bounded_safe_text(&workflow.command, MAX_WORKFLOW_COMMAND_BYTES) {
        return Err(WorkflowError::InvalidField("command"));
    }
    if workflow.tags.len() > MAX_WORKFLOW_SUGGESTIONS
        || !workflow
            .tags
            .iter()
            .all(|tag| bounded_safe_text(tag, MAX_WORKFLOW_TEXT_BYTES))
    {
        return Err(WorkflowError::InvalidField("tags"));
    }
    if workflow.parameters.len() > MAX_WORKFLOW_PARAMETERS {
        return Err(WorkflowError::InvalidField("parameters"));
    }

    let mut declared = HashSet::new();
    for parameter in &workflow.parameters {
        if !bounded_safe_text(&parameter.name, MAX_WORKFLOW_TEXT_BYTES)
            || !valid_parameter_name(&parameter.name)
        {
            return Err(WorkflowError::InvalidParameterName(parameter.name.clone()));
        }
        if !declared.insert(parameter.name.as_str()) {
            return Err(WorkflowError::DuplicateParameter(parameter.name.clone()));
        }
        if parameter
            .description
            .as_deref()
            .is_some_and(|value| !bounded_safe_text(value, MAX_WORKFLOW_TEXT_BYTES))
            || parameter
                .default
                .as_deref()
                .is_some_and(|value| !bounded_safe_text(value, MAX_WORKFLOW_TEXT_BYTES))
            || parameter.suggestions.len() > MAX_WORKFLOW_SUGGESTIONS
            || !parameter
                .suggestions
                .iter()
                .all(|value| bounded_safe_text(value, MAX_WORKFLOW_TEXT_BYTES))
        {
            return Err(WorkflowError::InvalidField("parameter metadata"));
        }
    }

    if let Some(parameter) = workflow.parameters.iter().find(|parameter| {
        !workflow
            .command
            .contains(&format!("{{{{{}}}}}", parameter.name))
    }) {
        return Err(WorkflowError::UnusedParameter(parameter.name.clone()));
    }
    Ok(())
}

fn workflow_is_safe_and_bounded(workflow: &Workflow) -> bool {
    validate_workflow(workflow).is_ok()
}

/// Extract parameter placeholders from a workflow command template.
/// Format: {{param_name}}
pub fn extract_placeholders(command: &str) -> Result<Vec<String>, WorkflowError> {
    parse_placeholders(command)
}

fn push_rendered(result: &mut String, value: &str) -> Result<(), WorkflowError> {
    if result
        .len()
        .checked_add(value.len())
        .is_none_or(|length| length > MAX_RENDERED_COMMAND_BYTES)
    {
        return Err(WorkflowError::RenderedCommandTooLong);
    }
    result.push_str(value);
    Ok(())
}

/// Fill a workflow template in one pass. Only names supplied in `values` are
/// jsh parameters; other moustache expressions are preserved for tools such as
/// Docker, Helm, Go templates, and Jinja. Parameter values are always literal,
/// so a value containing `{{another_name}}` is not expanded a second time.
pub fn fill_template(command: &str, values: &[(String, String)]) -> Result<String, WorkflowError> {
    if !bounded_safe_text(command, MAX_WORKFLOW_COMMAND_BYTES) {
        return Err(WorkflowError::InvalidField("command"));
    }
    let mut by_name = HashMap::with_capacity(values.len());
    for (name, value) in values {
        if !valid_parameter_name(name) || !bounded_safe_text(value, MAX_WORKFLOW_TEXT_BYTES) {
            return Err(WorkflowError::InvalidValue(name.clone()));
        }
        if by_name.insert(name.as_str(), value.as_str()).is_some() {
            return Err(WorkflowError::DuplicateValue(name.clone()));
        }
    }

    let mut result = String::with_capacity(command.len().min(MAX_RENDERED_COMMAND_BYTES));
    let mut offset = 0;
    while let Some(relative_opening) = command[offset..].find("{{") {
        let opening = offset + relative_opening;
        push_rendered(&mut result, &command[offset..opening])?;
        let name_start = opening + 2;
        let Some(relative_end) = command[name_start..].find("}}") else {
            push_rendered(&mut result, "{{")?;
            offset = name_start;
            continue;
        };
        let name_end = name_start + relative_end;
        let name = &command[name_start..name_end];
        if let Some(value) = by_name.get(name) {
            push_rendered(&mut result, value)?;
            offset = name_end + 2;
        } else {
            // Preserve only this opening delimiter now, then continue scanning
            // inside the unknown expression so a nested declared parameter is
            // not accidentally hidden from substitution.
            push_rendered(&mut result, "{{")?;
            offset = name_start;
        }
    }
    push_rendered(&mut result, &command[offset..])?;
    Ok(result)
}

/// Active workflow session state (held by editor during parameter filling)
#[derive(Debug, Clone)]
pub struct WorkflowSession {
    workflow: Workflow,
    param_values: Vec<(String, String)>,
    current_param: usize,
}

impl WorkflowSession {
    pub fn new(workflow: Workflow) -> Result<Self, WorkflowError> {
        validate_workflow(&workflow)?;
        let param_values = workflow
            .parameters
            .iter()
            .map(|p| (p.name.clone(), p.default.clone().unwrap_or_default()))
            .collect();
        Ok(WorkflowSession {
            workflow,
            param_values,
            current_param: 0,
        })
    }

    pub fn workflow(&self) -> &Workflow {
        &self.workflow
    }

    pub fn current_index(&self) -> usize {
        self.current_param
    }

    pub fn parameter_count(&self) -> usize {
        self.workflow.parameters.len()
    }

    pub fn current_placeholder(&self) -> Option<&WorkflowParam> {
        self.workflow.parameters.get(self.current_param)
    }

    pub fn advance(&mut self) -> bool {
        if self.current_param < self.workflow.parameters.len() {
            self.current_param += 1;
        }
        !self.is_complete()
    }

    pub fn current_value(&self) -> Option<&str> {
        self.param_values
            .get(self.current_param)
            .map(|(_, value)| value.as_str())
    }

    pub fn set_current_value(&mut self, value: String) -> Result<(), WorkflowError> {
        let Some((name, current_value)) = self.param_values.get_mut(self.current_param) else {
            return Err(WorkflowError::SessionComplete);
        };
        if !bounded_safe_text(&value, MAX_WORKFLOW_TEXT_BYTES) {
            return Err(WorkflowError::InvalidValue(name.clone()));
        }
        *current_value = value;
        Ok(())
    }

    pub fn push_current(&mut self, character: char) -> Result<(), WorkflowError> {
        let mut value = self
            .current_value()
            .ok_or(WorkflowError::SessionComplete)?
            .to_string();
        value.push(character);
        self.set_current_value(value)
    }

    pub fn pop_current(&mut self) -> Option<char> {
        self.param_values
            .get_mut(self.current_param)
            .and_then(|(_, value)| value.pop())
    }

    pub fn preview(&self) -> Result<String, WorkflowError> {
        fill_template(&self.workflow.command, &self.param_values)
    }

    pub fn render(&self) -> Result<String, WorkflowError> {
        if !self.is_complete() {
            return Err(WorkflowError::SessionIncomplete);
        }
        self.preview()
    }

    pub fn is_complete(&self) -> bool {
        self.current_param >= self.workflow.parameters.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn workflow(command: &str, parameters: &[(&str, Option<&str>)]) -> Workflow {
        Workflow {
            name: "test".into(),
            description: "test workflow".into(),
            command: command.into(),
            parameters: parameters
                .iter()
                .map(|(name, default)| WorkflowParam {
                    name: (*name).into(),
                    description: None,
                    default: default.map(str::to_string),
                    suggestions: Vec::new(),
                })
                .collect(),
            tags: Vec::new(),
        }
    }

    #[test]
    fn unsafe_workflow_text_is_rejected_before_it_reaches_the_editor() {
        let safe = Workflow {
            name: "status".into(),
            description: "show status".into(),
            command: "git status".into(),
            parameters: Vec::new(),
            tags: Vec::new(),
        };
        assert!(workflow_is_safe_and_bounded(&safe));
        assert!(!workflow_is_safe_and_bounded(&Workflow {
            command: "git\u{202e} status".into(),
            ..safe.clone()
        }));
        assert!(!workflow_is_safe_and_bounded(&Workflow {
            description: "first\nsecond".into(),
            ..safe
        }));
    }

    #[test]
    fn placeholders_are_deduplicated_and_literal_moustaches_are_ignored() {
        assert_eq!(
            extract_placeholders("echo {{目标}} {{first}} {{目标}}"),
            Ok(vec!["目标".into(), "first".into()])
        );
        assert_eq!(extract_placeholders("echo {{not closed"), Ok(Vec::new()));
        assert_eq!(extract_placeholders("echo {{two words}}"), Ok(Vec::new()));
        assert_eq!(extract_placeholders("echo value}}"), Ok(Vec::new()));
    }

    #[test]
    fn literal_closing_braces_remain_compatible_with_json_and_awk_commands() {
        let literal_json = workflow("printf '%s\\n' '{\"outer\":{\"inner\":1}}'", &[]);
        assert!(validate_workflow(&literal_json).is_ok());
        assert_eq!(
            fill_template(&literal_json.command, &[]),
            Ok(literal_json.command.clone())
        );

        let mixed = workflow(
            "printf '{{value}}' | awk '{ print $1 }}'",
            &[("value", None)],
        );
        assert!(validate_workflow(&mixed).is_ok());
        assert_eq!(
            fill_template(&mixed.command, &[("value".into(), "ok".into())]),
            Ok("printf 'ok' | awk '{ print $1 }}'".into())
        );

        let docker = workflow("docker inspect --format '{{.Id}}' container", &[]);
        assert!(validate_workflow(&docker).is_ok());
        assert_eq!(
            fill_template(&docker.command, &[]),
            Ok(docker.command.clone())
        );

        let literal_moustache = workflow("printf '{{literal}}'", &[]);
        assert!(validate_workflow(&literal_moustache).is_ok());
        assert_eq!(
            fill_template(&literal_moustache.command, &[]),
            Ok(literal_moustache.command.clone())
        );

        assert_eq!(
            fill_template(
                "printf '{{ unclosed {{value}}'",
                &[("value".into(), "ok".into())]
            ),
            Ok("printf '{{ unclosed ok'".into())
        );
    }

    #[test]
    fn validation_requires_unique_exactly_matching_parameter_declarations() {
        assert!(validate_workflow(&workflow("echo {{name}} {{name}}", &[("name", None)])).is_ok());
        assert!(
            validate_workflow(&workflow("echo {{missing}}", &[])).is_ok(),
            "undeclared moustaches are literal text for compatibility"
        );
        assert!(matches!(
            validate_workflow(&workflow("echo plain", &[("unused", None)])),
            Err(WorkflowError::UnusedParameter(name)) if name == "unused"
        ));
        assert!(matches!(
            validate_workflow(&workflow(
                "echo {{same}}",
                &[("same", None), ("same", Some("again"))]
            )),
            Err(WorkflowError::DuplicateParameter(name)) if name == "same"
        ));
    }

    #[test]
    fn every_builtin_template_has_a_complete_parameter_declaration() {
        let builtins: Vec<Workflow> =
            serde_json::from_str(include_str!("specs/workflows.json")).unwrap();
        for builtin in builtins {
            assert!(
                validate_workflow(&builtin).is_ok(),
                "built-in workflow {} must be internally complete",
                builtin.name
            );
        }
    }

    #[test]
    fn filling_is_single_pass_and_does_not_reinterpret_values() {
        let values = vec![
            ("a".to_string(), "{{b}}".to_string()),
            ("b".to_string(), "expanded once".to_string()),
        ];
        assert_eq!(
            fill_template("{{a}} / {{b}}", &values),
            Ok("{{b}} / expanded once".into())
        );

        let reversed = values.into_iter().rev().collect::<Vec<_>>();
        assert_eq!(
            fill_template("{{a}} / {{b}}", &reversed),
            Ok("{{b}} / expanded once".into()),
            "value order cannot change rendering"
        );
    }

    #[test]
    fn filling_preserves_unknowns_and_rejects_duplicate_and_oversized_results() {
        assert_eq!(
            fill_template("echo {{name}}", &[]),
            Ok("echo {{name}}".into())
        );
        assert!(matches!(
            fill_template(
                "{{name}}",
                &[("name".into(), "one".into()), ("name".into(), "two".into())]
            ),
            Err(WorkflowError::DuplicateValue(name)) if name == "name"
        ));

        let command = "{{value}}".repeat(MAX_WORKFLOW_COMMAND_BYTES / "{{value}}".len());
        let value = "x".repeat(MAX_WORKFLOW_TEXT_BYTES);
        assert_eq!(
            fill_template(&command, &[("value".into(), value)]),
            Err(WorkflowError::RenderedCommandTooLong)
        );
    }

    #[test]
    fn workflow_session_advances_past_the_last_parameter_before_rendering() {
        let mut session = WorkflowSession::new(workflow(
            "echo {{first}} {{second}}",
            &[("first", Some("one")), ("second", None)],
        ))
        .unwrap();

        assert_eq!(session.current_index(), 0);
        assert_eq!(session.current_value(), Some("one"));
        assert_eq!(session.render(), Err(WorkflowError::SessionIncomplete));
        assert!(session.advance(), "one parameter remains");
        session.set_current_value("two".into()).unwrap();
        assert!(!session.advance(), "the session reached its terminal state");
        assert!(session.is_complete());
        assert!(session.current_placeholder().is_none());
        assert_eq!(session.render(), Ok("echo one two".into()));
    }

    #[test]
    fn zero_parameter_session_is_complete_but_never_executes_anything() {
        let session = WorkflowSession::new(workflow("git status", &[])).unwrap();
        assert!(session.is_complete());
        assert_eq!(session.render(), Ok("git status".into()));
    }

    #[test]
    fn registry_exact_lookup_and_listing_are_stable() {
        let mut registry = WorkflowRegistry {
            workflows: Vec::new(),
            user_dir: PathBuf::new(),
        };
        let mut zebra = workflow("echo zebra", &[]);
        zebra.name = "zebra".into();
        let mut alpha = workflow("echo alpha", &[]);
        alpha.name = "alpha".into();
        registry.extend_validated([zebra, alpha]);

        assert_eq!(
            registry.get("alpha").map(|item| item.command.as_str()),
            Some("echo alpha")
        );
        assert!(registry.get("Alpha").is_none(), "lookup is exact");
        assert_eq!(
            registry
                .all()
                .into_iter()
                .map(|item| item.name.as_str())
                .collect::<Vec<_>>(),
            vec!["alpha", "zebra"]
        );
    }
}
