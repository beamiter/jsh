/// Workflows: parameterized command templates with fuzzy search and interactive filling.
/// Inspired by Warp's Workflows but local-first and extensible.
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

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
            self.workflows.extend(wfs);
        }
    }

    fn load_user(&mut self) {
        if !self.user_dir.exists() {
            return;
        }
        if let Ok(entries) = std::fs::read_dir(&self.user_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().map(|e| e == "json").unwrap_or(false) {
                    if let Ok(content) = std::fs::read_to_string(&path) {
                        if let Ok(wf) = serde_json::from_str::<Workflow>(&content) {
                            self.workflows.push(wf);
                        } else if let Ok(wfs) = serde_json::from_str::<Vec<Workflow>>(&content) {
                            self.workflows.extend(wfs);
                        }
                    }
                }
            }
        }
    }

    pub fn search(&self, query: &str) -> Vec<&Workflow> {
        if query.is_empty() {
            return self.workflows.iter().collect();
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

        results.sort_by(|a, b| b.1.cmp(&a.1));
        results.into_iter().map(|(wf, _)| wf).collect()
    }

    pub fn count(&self) -> usize {
        self.workflows.len()
    }
}

/// Extract parameter placeholders from a workflow command template.
/// Format: {{param_name}}
pub fn extract_placeholders(command: &str) -> Vec<String> {
    let mut params = Vec::new();
    let mut i = 0;
    let bytes = command.as_bytes();
    while i < bytes.len().saturating_sub(3) {
        if bytes[i] == b'{' && bytes[i + 1] == b'{' {
            if let Some(end) = command[i + 2..].find("}}") {
                let name = &command[i + 2..i + 2 + end];
                if !params.contains(&name.to_string()) {
                    params.push(name.to_string());
                }
                i += end + 4;
            } else {
                i += 1;
            }
        } else {
            i += 1;
        }
    }
    params
}

/// Fill a workflow template with parameter values.
pub fn fill_template(command: &str, values: &[(String, String)]) -> String {
    let mut result = command.to_string();
    for (name, value) in values {
        let placeholder = format!("{{{{{}}}}}", name);
        result = result.replace(&placeholder, value);
    }
    result
}

/// Active workflow session state (held by editor during parameter filling)
#[derive(Debug, Clone)]
pub struct WorkflowSession {
    pub workflow: Workflow,
    pub param_values: Vec<(String, String)>,
    pub current_param: usize,
}

impl WorkflowSession {
    pub fn new(workflow: Workflow) -> Self {
        let param_values = workflow
            .parameters
            .iter()
            .map(|p| (p.name.clone(), p.default.clone().unwrap_or_default()))
            .collect();
        WorkflowSession {
            workflow,
            param_values,
            current_param: 0,
        }
    }

    pub fn current_placeholder(&self) -> Option<&WorkflowParam> {
        self.workflow.parameters.get(self.current_param)
    }

    pub fn advance(&mut self) -> bool {
        if self.current_param + 1 < self.workflow.parameters.len() {
            self.current_param += 1;
            true
        } else {
            false
        }
    }

    pub fn set_current_value(&mut self, value: String) {
        if self.current_param < self.param_values.len() {
            self.param_values[self.current_param].1 = value;
        }
    }

    pub fn render(&self) -> String {
        fill_template(&self.workflow.command, &self.param_values)
    }

    pub fn is_complete(&self) -> bool {
        self.current_param >= self.workflow.parameters.len()
    }
}
