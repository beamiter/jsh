/// Extensible completion spec system: data-driven CLI completion definitions.
/// Supports JSON specs for 500+ CLI tools, compatible with Fig spec subset.
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommandSpec {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub subcommands: Vec<SubcommandSpec>,
    #[serde(default)]
    pub options: Vec<OptionSpec>,
    #[serde(default)]
    pub args: Vec<ArgSpec>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubcommandSpec {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub subcommands: Vec<SubcommandSpec>,
    #[serde(default)]
    pub options: Vec<OptionSpec>,
    #[serde(default)]
    pub args: Vec<ArgSpec>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OptionSpec {
    pub names: Vec<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub args: Vec<ArgSpec>,
    #[serde(default)]
    pub is_repeatable: bool,
    #[serde(default)]
    pub exclusive_on: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArgSpec {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub template: ArgTemplate,
    #[serde(default)]
    pub is_variadic: bool,
    #[serde(default)]
    pub is_optional: bool,
    #[serde(default)]
    pub suggestions: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ArgTemplate {
    #[default]
    None,
    FilePath,
    FolderPath,
    /// Dynamic generator: runs a command or calls a builtin generator
    Generator(String),
}

/// Registry of loaded completion specs
pub struct SpecRegistry {
    specs: HashMap<String, CommandSpec>,
    user_dir: PathBuf,
}

impl SpecRegistry {
    pub fn new() -> Self {
        let user_dir = dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("/tmp"))
            .join(".jsh")
            .join("completions");

        let mut registry = SpecRegistry {
            specs: HashMap::new(),
            user_dir,
        };
        registry.load_builtin_specs();
        registry.load_user_specs();
        registry
    }

    fn load_builtin_specs(&mut self) {
        // Embedded specs for common CLI tools
        let builtin_specs: &[(&str, &str)] = &[
            ("git", include_str!("specs/git.json")),
            ("docker", include_str!("specs/docker.json")),
            ("cargo", include_str!("specs/cargo.json")),
            ("kubectl", include_str!("specs/kubectl.json")),
            ("npm", include_str!("specs/npm.json")),
        ];

        for (name, json) in builtin_specs {
            if let Ok(spec) = serde_json::from_str::<CommandSpec>(json) {
                self.specs.insert(name.to_string(), spec);
            }
        }
    }

    fn load_user_specs(&mut self) {
        if !self.user_dir.exists() {
            return;
        }
        if let Ok(entries) = std::fs::read_dir(&self.user_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().map(|e| e == "json").unwrap_or(false) {
                    if let Ok(content) = std::fs::read_to_string(&path) {
                        if let Ok(spec) = serde_json::from_str::<CommandSpec>(&content) {
                            self.specs.insert(spec.name.clone(), spec);
                        }
                    }
                }
            }
        }
    }

    pub fn get(&self, command: &str) -> Option<&CommandSpec> {
        self.specs.get(command)
    }

    pub fn has(&self, command: &str) -> bool {
        self.specs.contains_key(command)
    }

    /// Resolve completion context: walk the spec tree based on typed words.
    /// Returns the active node (subcommand or root) and remaining args.
    pub fn resolve_context<'a>(
        &'a self,
        command: &str,
        words: &[&str],
    ) -> Option<CompletionContext<'a>> {
        let spec = self.specs.get(command)?;

        let mut current_options = &spec.options;
        let mut current_subcommands = &spec.subcommands;
        let mut current_args = &spec.args;
        let mut depth = 0;

        // Walk through typed words to find current context
        for &word in words.iter().skip(1) {
            // Skip flags
            if word.starts_with('-') {
                continue;
            }

            // Check if word matches a subcommand
            if let Some(sub) = current_subcommands.iter().find(|s| s.name == word) {
                current_options = &sub.options;
                current_subcommands = &sub.subcommands;
                current_args = &sub.args;
                depth += 1;
            } else {
                break;
            }
        }

        Some(CompletionContext {
            options: current_options,
            subcommands: current_subcommands,
            args: current_args,
            depth,
        })
    }
}

pub struct CompletionContext<'a> {
    pub options: &'a [OptionSpec],
    pub subcommands: &'a [SubcommandSpec],
    pub args: &'a [ArgSpec],
    pub depth: usize,
}

impl<'a> CompletionContext<'a> {
    pub fn complete_prefix(
        &self,
        prefix: &str,
    ) -> Vec<(String, Option<String>, SpecCompletionKind)> {
        let mut results = Vec::new();

        // If prefix starts with -, complete options
        if prefix.starts_with('-') {
            for opt in self.options {
                for name in &opt.names {
                    if name.starts_with(prefix) {
                        results.push((
                            name.clone(),
                            opt.description.clone(),
                            SpecCompletionKind::Option,
                        ));
                    }
                }
            }
        } else {
            // Complete subcommands
            for sub in self.subcommands {
                if sub.name.starts_with(prefix) {
                    results.push((
                        sub.name.clone(),
                        sub.description.clone(),
                        SpecCompletionKind::Subcommand,
                    ));
                }
            }

            // Complete arg suggestions
            for arg in self.args {
                for suggestion in &arg.suggestions {
                    if suggestion.starts_with(prefix) {
                        results.push((
                            suggestion.clone(),
                            arg.description.clone(),
                            SpecCompletionKind::Argument,
                        ));
                    }
                }
            }
        }

        results
    }
}

#[derive(Debug, Clone)]
pub enum SpecCompletionKind {
    Subcommand,
    Option,
    Argument,
}
