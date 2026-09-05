//! Pinned Pi system-prompt.ts/resource-loader.ts/footer-data-provider.ts port.
//! See PI_UPSTREAM.md and PARITY_STATUS.md for the explicit directory/skills mapping.
use crate::{paths::AppPaths, tools::ToolDefinition};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashSet,
    fs,
    path::{Component, Path, PathBuf},
};

pub const PASSIVE_SKILLS: &str = "Optional skills are ordinary Markdown files in ~/.config/bashkitten/skills/. When a task may benefit from one, use ls to inspect the filenames and read only the files you consider relevant. You may create or edit skill files with the ordinary tools when useful or requested.";
pub const DOCS_ROOT: &str = "/usr/share/doc/bashkitten/pi-reference";
const DEFAULT_PROMPT: &str = include_str!("prompts/pi-default-system.txt");
const CONTEXT_NAMES: &[&str] = &[
    "AGENTS.override.md",
    "AGENTS.md",
    "AGENTS.MD",
    "CLAUDE.md",
    "CLAUDE.MD",
];

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextFile {
    pub path: PathBuf,
    pub content: String,
}

fn resolve(path: &Path, base: &Path) -> PathBuf {
    let absolute = if path.is_absolute() {
        path.to_owned()
    } else {
        base.join(path)
    };
    let mut result = PathBuf::new();
    for part in absolute.components() {
        match part {
            Component::CurDir => {}
            Component::ParentDir => {
                result.pop();
            }
            other => result.push(other.as_os_str()),
        }
    }
    result
}
fn canonical(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| path.to_owned())
}
fn read_text(path: &Path) -> std::io::Result<String> {
    let bytes = fs::read(path)?;
    let text = String::from_utf8_lossy(&bytes);
    Ok(text.strip_prefix('\u{feff}').unwrap_or(&text).to_owned())
}
fn read_context(dir: &Path) -> Option<ContextFile> {
    for name in CONTEXT_NAMES {
        let path = dir.join(name);
        if !path.exists() {
            continue;
        }
        match fs::metadata(&path) {
            Ok(info) if !info.is_file() => continue,
            Ok(_) => match read_text(&path) {
                Ok(content) => return Some(ContextFile { path, content }),
                Err(error) => eprintln!("Warning: Could not read {}: {error}", path.display()),
            },
            Err(error) => eprintln!("Warning: Could not read {}: {error}", path.display()),
        }
    }
    None
}

/// Pi's findGitPaths: inspect only metadata files; do not spawn git or a watcher.
fn git_paths(cwd: &Path) -> Option<(PathBuf, PathBuf)> {
    for dir in cwd.ancestors() {
        let path = dir.join(".git");
        if !path.exists() {
            continue;
        }
        let stat = fs::metadata(&path).ok()?;
        if stat.is_file() {
            let content = read_text(&path).ok()?;
            if let Some(gitdir) = content.trim().strip_prefix("gitdir: ") {
                let gitdir = resolve(Path::new(gitdir.trim()), dir);
                if !gitdir.join("HEAD").exists() {
                    return None;
                }
                let common = gitdir.join("commondir");
                let common = if common.exists() {
                    resolve(Path::new(read_text(&common).ok()?.trim()), &gitdir)
                } else {
                    gitdir
                };
                return Some((dir.to_owned(), common));
            }
        } else if stat.is_dir() {
            return path.join("HEAD").exists().then(|| (dir.to_owned(), path));
        }
    }
    None
}
fn shadowed_context(cwd: &Path) -> Option<PathBuf> {
    let (worktree, common) = git_paths(cwd)?;
    let worktree = canonical(&worktree);
    let common = canonical(&common);
    let main = common.parent()?;
    if worktree == main || !worktree.starts_with(main) || canonical(&main.join(".git")) != common {
        return None;
    }
    let context = read_context(&worktree)?;
    Some(main.join(context.path.file_name()?))
}

pub fn load_project_context(cwd: &Path, agent_dir: &Path) -> Vec<ContextFile> {
    let base = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
    let cwd = resolve(cwd, &base);
    let agent_dir = resolve(agent_dir, &base);
    let mut files = Vec::new();
    let mut seen = HashSet::new();
    if let Some(file) = read_context(&agent_dir) {
        seen.insert(file.path.clone());
        files.push(file);
    }
    let shadow = shadowed_context(&cwd);
    let mut ancestors = Vec::new();
    for dir in cwd.ancestors() {
        if let Some(file) = read_context(dir) {
            if shadow.as_ref() == Some(&canonical(&file.path)) || !seen.insert(file.path.clone()) {
                continue;
            }
            ancestors.push(file);
        }
    }
    ancestors.reverse();
    files.extend(ancestors);
    files
}

/// Preserve project-over-global precedence, empty files, BOM stripping, and Pi's
/// failed-file-read fallback to the path itself. No skill directory is inspected.
pub fn load_prompt_file(cwd: &Path, agent_dir: &Path, name: &str) -> Option<String> {
    let project = cwd.join(".pi").join(name);
    let global = agent_dir.join(name);
    let path = if project.exists() {
        project
    } else if global.exists() {
        global
    } else {
        return None;
    };
    Some(read_text(&path).unwrap_or_else(|error| {
        let description = if name == "SYSTEM.md" {
            "system prompt"
        } else {
            "append system prompt"
        };
        eprintln!(
            "Warning: Could not read {description} file {}: {error}",
            path.display()
        );
        path.to_string_lossy().into_owned()
    }))
}

pub fn build(
    cwd: &Path,
    custom: Option<&str>,
    append: Option<&str>,
    context: &[ContextFile],
    tools: &[ToolDefinition],
    passive_skills: bool,
) -> String {
    let custom = custom.filter(|value| !value.is_empty());
    let mut prompt = if let Some(custom) = custom {
        custom.to_owned()
    } else {
        let snippets = tools
            .iter()
            .filter(|tool| !tool.prompt_snippet.is_empty())
            .map(|tool| format!("- {}: {}", tool.name, tool.prompt_snippet))
            .collect::<Vec<_>>();
        let mut guidelines = Vec::<String>::new();
        let has = |name: &str| tools.iter().any(|tool| tool.name == name);
        if has("bash") && !has("grep") && !has("find") && !has("ls") {
            guidelines.push("Use bash for file operations like ls, rg, find".into());
        }
        for line in tools
            .iter()
            .flat_map(|tool| &tool.prompt_guidelines)
            .map(|line| line.trim())
            .chain([
                "Be concise in your responses",
                "Show file paths clearly when working with files",
            ])
        {
            if !line.is_empty() && !guidelines.iter().any(|value| value == line) {
                guidelines.push(line.to_owned());
            }
        }
        DEFAULT_PROMPT
            .replace(
                "${toolsList}",
                &if snippets.is_empty() {
                    "(none)".into()
                } else {
                    snippets.join("\n")
                },
            )
            .replace(
                "${guidelines}",
                &guidelines
                    .iter()
                    .map(|line| format!("- {line}"))
                    .collect::<Vec<_>>()
                    .join("\n"),
            )
            .replace("${readmePath}", &format!("{DOCS_ROOT}/README.md"))
            .replace("${docsPath}", &format!("{DOCS_ROOT}/docs"))
            .replace("${examplesPath}", &format!("{DOCS_ROOT}/examples"))
    };
    if let Some(append) = append.filter(|value| !value.is_empty()) {
        prompt.push_str("\n\n");
        prompt.push_str(append);
    }
    if !context.is_empty() {
        prompt
            .push_str("\n\n<project_context>\n\nProject-specific instructions and guidelines:\n\n");
        for file in context {
            prompt.push_str(&format!(
                "<project_instructions path=\"{}\">\n{}\n</project_instructions>\n\n",
                file.path.display(),
                file.content
            ));
        }
        prompt.push_str("</project_context>\n");
    }
    if passive_skills {
        prompt.push_str("\n\n");
        prompt.push_str(PASSIVE_SKILLS);
    }
    prompt.push_str(&format!(
        "\nCurrent working directory: {}",
        cwd.to_string_lossy().replace('\\', "/")
    ));
    if custom.is_some() {
        prompt.push('\n');
    }
    prompt
}

pub fn load(paths: &AppPaths, cwd: &Path) -> String {
    let custom = load_prompt_file(cwd, &paths.config, "SYSTEM.md");
    let append = load_prompt_file(cwd, &paths.config, "APPEND_SYSTEM.md");
    build(
        cwd,
        custom.as_deref(),
        append.as_deref(),
        &load_project_context(cwd, &paths.config),
        &crate::tools::tool_definitions(),
        true,
    )
}
