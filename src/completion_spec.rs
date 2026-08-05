/// Extensible completion spec system: data-driven CLI completion definitions.
/// Supports JSON specs for 500+ CLI tools, compatible with Fig spec subset.
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

const MAX_SPEC_FILE_BYTES: usize = 1024 * 1024;
const MAX_SPEC_FILES: usize = 256;
const MAX_SPECS: usize = 4096;
const MAX_SPEC_NODES: usize = 4096;
const MAX_SPEC_DEPTH: usize = 32;
const MAX_SPEC_COLLECTION: usize = 1024;
const MAX_SPEC_TEXT_BYTES: usize = 16 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommandSpec {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    /// Another command whose spec describes this one too. `batcat` wraps
    /// `bat`, `nvim` wraps `vim`, a wrapper script wraps what it wraps —
    /// each is the same interface under a different name, and saying so is
    /// cheaper and more honest than copying the spec.
    #[serde(default)]
    pub wraps: Option<String>,
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

impl Default for SpecRegistry {
    fn default() -> Self {
        Self::new()
    }
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
            ("gh", include_str!("specs/gh.json")),
            ("systemctl", include_str!("specs/systemctl.json")),
            ("tmux", include_str!("specs/tmux.json")),
            ("terraform", include_str!("specs/terraform.json")),
            ("apt", include_str!("specs/apt.json")),
            ("podman", include_str!("specs/podman.json")),
            ("pnpm", include_str!("specs/pnpm.json")),
        ];

        for (name, json) in builtin_specs {
            if let Ok(spec) = serde_json::from_str::<CommandSpec>(json) {
                if command_spec_is_safe_and_bounded(&spec) {
                    self.specs.insert(name.to_string(), spec);
                }
            }
        }
    }

    fn load_user_specs(&mut self) {
        if !self.user_dir.exists() {
            return;
        }
        if let Ok(entries) = std::fs::read_dir(&self.user_dir) {
            for entry in entries.flatten().take(MAX_SPEC_FILES) {
                let path = entry.path();
                if path.extension().map(|e| e == "json").unwrap_or(false) {
                    if self.specs.len() >= MAX_SPECS {
                        break;
                    }
                    if let Ok(content) =
                        crate::io_guard::read_regular_text(&path, MAX_SPEC_FILE_BYTES)
                    {
                        if let Ok(spec) = serde_json::from_str::<CommandSpec>(&content) {
                            if command_spec_is_safe_and_bounded(&spec) {
                                self.specs.insert(spec.name.clone(), spec);
                            }
                        }
                    }
                }
            }
        }
    }

    /// The spec that describes `command`, following `wraps` links. Bounded
    /// and cycle-safe: a spec that wraps itself, or a pair that wrap each
    /// other, resolves to the last one reached rather than spinning.
    pub fn resolve_wraps(&self, command: &str) -> Option<&CommandSpec> {
        let mut spec = self.specs.get(command)?;
        let mut seen = vec![command];
        for _ in 0..8 {
            let Some(target) = spec.wraps.as_deref() else {
                break;
            };
            if seen.contains(&target) {
                break;
            }
            let Some(next) = self.specs.get(target) else {
                break;
            };
            seen.push(target);
            spec = next;
        }
        Some(spec)
    }

    pub fn get(&self, command: &str) -> Option<&CommandSpec> {
        self.specs.get(command)
    }

    pub fn has(&self, command: &str) -> bool {
        self.specs.contains_key(command)
    }

    /// Names in the registry, for the shipped-spec checks and for listing.
    pub fn command_names(&self) -> Vec<&str> {
        let mut names: Vec<&str> = self.specs.keys().map(String::as_str).collect();
        names.sort_unstable();
        names
    }

    /// Resolve completion context: walk the spec tree based on typed words.
    /// Returns the active node (subcommand or root) and remaining args.
    pub fn resolve_context<'a>(
        &'a self,
        command: &str,
        words: &[&str],
    ) -> Option<CompletionContext<'a>> {
        let spec = self.resolve_wraps(command)?;

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

fn safe_spec_text(value: &str) -> bool {
    value.len() <= MAX_SPEC_TEXT_BYTES && crate::terminal_text::is_safe_inline(value)
}

fn safe_optional_text(value: Option<&str>) -> bool {
    value.is_none_or(safe_spec_text)
}

fn safe_args(args: &[ArgSpec]) -> bool {
    args.len() <= MAX_SPEC_COLLECTION
        && args.iter().all(|arg| {
            safe_optional_text(arg.name.as_deref())
                && safe_optional_text(arg.description.as_deref())
                && arg.suggestions.len() <= MAX_SPEC_COLLECTION
                && arg.suggestions.iter().all(|value| safe_spec_text(value))
                && match &arg.template {
                    ArgTemplate::Generator(command) => safe_spec_text(command),
                    _ => true,
                }
        })
}

fn safe_options(options: &[OptionSpec]) -> bool {
    options.len() <= MAX_SPEC_COLLECTION
        && options.iter().all(|option| {
            !option.names.is_empty()
                && option.names.len() <= MAX_SPEC_COLLECTION
                && option.names.iter().all(|name| safe_spec_text(name))
                && safe_optional_text(option.description.as_deref())
                && safe_args(&option.args)
                && option.exclusive_on.len() <= MAX_SPEC_COLLECTION
                && option
                    .exclusive_on
                    .iter()
                    .all(|value| safe_spec_text(value))
        })
}

fn safe_subcommands(subcommands: &[SubcommandSpec], depth: usize, nodes: &mut usize) -> bool {
    if depth > MAX_SPEC_DEPTH || subcommands.len() > MAX_SPEC_COLLECTION {
        return false;
    }
    for subcommand in subcommands {
        *nodes = nodes.saturating_add(1);
        if *nodes > MAX_SPEC_NODES
            || !safe_spec_text(&subcommand.name)
            || !safe_optional_text(subcommand.description.as_deref())
            || !safe_options(&subcommand.options)
            || !safe_args(&subcommand.args)
            || !safe_subcommands(&subcommand.subcommands, depth + 1, nodes)
        {
            return false;
        }
    }
    true
}

fn command_spec_is_safe_and_bounded(spec: &CommandSpec) -> bool {
    if !safe_optional_text(spec.wraps.as_deref()) {
        return false;
    }
    let mut nodes = 1usize;
    safe_spec_text(&spec.name)
        && safe_optional_text(spec.description.as_deref())
        && safe_options(&spec.options)
        && safe_args(&spec.args)
        && safe_subcommands(&spec.subcommands, 1, &mut nodes)
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Every spec this binary carries, as the registry loads them.
    fn shipped_specs() -> Vec<(&'static str, CommandSpec)> {
        let sources: &[(&str, &str)] = &[
            ("git", include_str!("specs/git.json")),
            ("docker", include_str!("specs/docker.json")),
            ("cargo", include_str!("specs/cargo.json")),
            ("kubectl", include_str!("specs/kubectl.json")),
            ("npm", include_str!("specs/npm.json")),
            ("gh", include_str!("specs/gh.json")),
            ("systemctl", include_str!("specs/systemctl.json")),
            ("tmux", include_str!("specs/tmux.json")),
            ("terraform", include_str!("specs/terraform.json")),
            ("apt", include_str!("specs/apt.json")),
            ("podman", include_str!("specs/podman.json")),
            ("pnpm", include_str!("specs/pnpm.json")),
        ];
        sources
            .iter()
            .map(|(name, json)| {
                let spec: CommandSpec = serde_json::from_str(json)
                    .unwrap_or_else(|error| panic!("{name}.json does not parse: {error}"));
                (*name, spec)
            })
            .collect()
    }

    #[test]
    fn every_shipped_spec_parses_and_passes_the_safety_check() {
        // A spec that fails the bound check is dropped at load time and the
        // command silently loses its completions, which is the kind of
        // regression only a test notices.
        for (name, spec) in shipped_specs() {
            assert!(
                command_spec_is_safe_and_bounded(&spec),
                "{name}.json is rejected by the safety and bound checks"
            );
            assert_eq!(spec.name, name, "{name}.json names itself differently");
            assert!(
                !spec.subcommands.is_empty() || !spec.options.is_empty() || spec.wraps.is_some(),
                "{name}.json describes nothing"
            );
        }
    }

    #[test]
    fn shipped_specs_describe_what_they_offer() {
        // A candidate with no description is a name with no explanation,
        // which is most of the value of a spec over a bare word list.
        fn check(path: &str, subs: &[SubcommandSpec]) {
            for sub in subs {
                let here = format!("{path} {}", sub.name);
                assert!(
                    sub.description.as_ref().is_some_and(|d| !d.is_empty()),
                    "{here} has no description"
                );
                for option in &sub.options {
                    assert!(!option.names.is_empty(), "{here} has a nameless option");
                    assert!(
                        option.description.as_ref().is_some_and(|d| !d.is_empty()),
                        "{here} {:?} has no description",
                        option.names
                    );
                    for name in &option.names {
                        assert!(name.starts_with('-'), "{here} option {name} is not a flag");
                    }
                }
                check(&here, &sub.subcommands);
            }
        }
        for (name, spec) in shipped_specs() {
            check(name, &spec.subcommands);
            for option in &spec.options {
                assert!(
                    option.description.as_ref().is_some_and(|d| !d.is_empty()),
                    "{name} {:?} has no description",
                    option.names
                );
            }
        }
    }

    #[test]
    fn a_wrapping_spec_completes_as_what_it_wraps() {
        let registry = SpecRegistry::new();

        // `podman` ships as nothing but a link to `docker`, and resolves to
        // docker's own subcommands.
        let resolved = registry
            .resolve_wraps("podman")
            .expect("podman ships as a wrapping spec");
        assert_eq!(resolved.name, "docker");
        assert!(!resolved.subcommands.is_empty());

        let context = registry
            .resolve_context("podman", &["podman"])
            .expect("a wrapping spec resolves a context");
        assert!(context.subcommands.iter().any(|sub| sub.name == "run"));

        // An unwrapped spec resolves to itself, and an unknown command to
        // nothing at all.
        assert_eq!(
            registry.resolve_wraps("git").map(|s| s.name.as_str()),
            Some("git")
        );
        assert!(registry.resolve_wraps("no-such-command").is_none());
    }

    #[test]
    fn wrapping_cycles_and_dangling_links_terminate() {
        let mut registry = SpecRegistry {
            specs: HashMap::new(),
            user_dir: PathBuf::from("/nonexistent"),
        };
        let spec = |name: &str, wraps: Option<&str>| CommandSpec {
            name: name.to_string(),
            description: None,
            wraps: wraps.map(str::to_string),
            subcommands: Vec::new(),
            options: Vec::new(),
            args: Vec::new(),
        };
        registry.specs.insert("a".into(), spec("a", Some("b")));
        registry.specs.insert("b".into(), spec("b", Some("a")));
        registry
            .specs
            .insert("self".into(), spec("self", Some("self")));
        registry
            .specs
            .insert("dangling".into(), spec("dangling", Some("absent")));

        // Each terminates at the last spec it could reach.
        assert!(registry.resolve_wraps("a").is_some());
        assert_eq!(
            registry.resolve_wraps("self").map(|s| s.name.as_str()),
            Some("self")
        );
        assert_eq!(
            registry.resolve_wraps("dangling").map(|s| s.name.as_str()),
            Some("dangling")
        );
    }

    #[test]
    fn unsafe_completion_spec_text_is_rejected() {
        let mut spec = CommandSpec {
            name: "tool".into(),
            wraps: None,
            description: Some("safe".into()),
            subcommands: Vec::new(),
            options: Vec::new(),
            args: Vec::new(),
        };
        assert!(command_spec_is_safe_and_bounded(&spec));
        spec.description = Some("hidden\u{2066}text".into());
        assert!(!command_spec_is_safe_and_bounded(&spec));
    }
}
