//! Skills: pi/Hermes/Claude-Code-compatible `SKILL.md` folders (owner
//! request, 2026-08-02).
//!
//! One folder per skill under [`crate::paths::Paths::skills_dir`], each with
//! a `SKILL.md` — YAML frontmatter (`name`, `description`, optionally
//! `disable-model-invocation`) over a markdown body. The folder may carry
//! scripts and assets; the body documents how to run them, and the agent's
//! ordinary bash tool is the plugin interface — exactly pi's contract, so
//! skills move between the agents unchanged.
//!
//! Token economics, also pi's: the system prompt carries only names,
//! descriptions and file paths; the agent reads a body on demand with its
//! `read` tool when the task matches. `/skill <name>` front-loads the body
//! instead.

use std::path::{Path, PathBuf};

/// Max `name` length per the Agent Skills spec.
const MAX_NAME: usize = 64;

/// Max `description` length per the spec.
const MAX_DESCRIPTION: usize = 1024;

/// One loadable skill.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skill {
    /// Spec-valid name (lowercase a–z, 0–9, hyphens).
    pub name: String,
    /// One-line description; what the model matches tasks against.
    pub description: String,
    /// Absolute path of the `SKILL.md`.
    pub file_path: PathBuf,
    /// The skill's folder; relative references resolve against it.
    pub base_dir: PathBuf,
    /// `disable-model-invocation: true` — hidden from the prompt, usable
    /// only through the explicit `/skill` spelling.
    pub hidden: bool,
}

/// Load every valid skill under `dir`, sorted by name. Missing dir is an
/// empty catalogue, not an error — the folder appears with the first skill.
pub fn load(dir: &Path) -> Vec<Skill> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut skills: Vec<Skill> = entries
        .flatten()
        .filter_map(|entry| {
            let base_dir = entry.path();
            let file_path = base_dir.join("SKILL.md");
            let body = std::fs::read_to_string(&file_path).ok()?;
            match parse(&body, &base_dir) {
                Ok((name, description, hidden)) => Some(Skill {
                    name,
                    description,
                    file_path,
                    base_dir,
                    hidden,
                }),
                Err(reason) => {
                    tracing::warn!(path = %file_path.display(), %reason, "skipping invalid skill");
                    None
                }
            }
        })
        .collect();
    skills.sort_by(|a, b| a.name.cmp(&b.name));
    skills
}

/// pi's `<available_skills>` block, appended to the agent system prompt.
/// Empty when nothing is installed — no section costs no tokens.
pub fn prompt_section(skills: &[Skill]) -> String {
    let visible: Vec<&Skill> = skills.iter().filter(|skill| !skill.hidden).collect();
    if visible.is_empty() {
        return String::new();
    }
    let mut out = String::from(
        "\n\nThe following skills provide specialized instructions for specific tasks.\n\
         Use the read tool to load a skill's file when the task matches its description.\n\
         When a skill file references a relative path, resolve it against the skill's \
         directory (the parent of its SKILL.md) and use the absolute path in tool calls.\n\n\
         <available_skills>\n",
    );
    for skill in visible {
        use std::fmt::Write as _;
        let _ = writeln!(
            out,
            "  <skill>\n    <name>{}</name>\n    <description>{}</description>\n    <location>{}</location>\n  </skill>",
            escape_xml(&skill.name),
            escape_xml(&skill.description),
            escape_xml(&skill.file_path.display().to_string()),
        );
    }
    out.push_str("</available_skills>");
    out
}

/// Expand a skill for the explicit `/skill <name>` spelling: the full body,
/// fenced and located, exactly pi's `<skill>` block.
pub fn expand(skill: &Skill) -> std::io::Result<String> {
    let content = std::fs::read_to_string(&skill.file_path)?;
    let body = strip_frontmatter(&content).trim().to_owned();
    Ok(format!(
        "<skill name=\"{}\" location=\"{}\">\nReferences are relative to {}.\n\n{}\n</skill>",
        skill.name,
        skill.file_path.display(),
        skill.base_dir.display(),
        body,
    ))
}

/// `/skill <name> [args]` — the explicit invocation, recognised on the way
/// into the runtime like `/agent`. Returns the named skill token and the
/// trailing arguments.
pub fn strip_skill_command(input: &str) -> Option<(&str, &str)> {
    let rest = input.trim_start().strip_prefix("/skill")?;
    // "/skills" is the catalogue, not an invocation.
    let rest = rest.strip_prefix(char::is_whitespace)?;
    let rest = rest.trim_start();
    match rest.split_once(char::is_whitespace) {
        Some((name, args)) => Some((name, args.trim_start())),
        None if rest.is_empty() => None,
        None => Some((rest, "")),
    }
}

fn parse(body: &str, base_dir: &Path) -> Result<(String, String, bool), String> {
    let front = frontmatter(body).ok_or("no YAML frontmatter")?;
    let mut name = None;
    let mut description = None;
    let mut hidden = false;
    for line in front.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim().trim_matches('"').trim_matches('\'');
        match key.trim() {
            "name" => name = Some(value.to_owned()),
            "description" => description = Some(value.to_owned()),
            "disable-model-invocation" => hidden = value == "true",
            _ => {}
        }
    }
    // The folder names the skill when the frontmatter does not — lenient on
    // authorship, strict on shape.
    let name = name
        .filter(|name| !name.is_empty())
        .or_else(|| {
            base_dir
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
        })
        .ok_or("no usable name")?;
    if name.len() > MAX_NAME
        || !name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        || name.starts_with('-')
        || name.ends_with('-')
        || name.contains("--")
    {
        return Err(format!("invalid skill name {name:?}"));
    }
    let description = description
        .filter(|description| !description.is_empty())
        .ok_or("missing description")?;
    if description.len() > MAX_DESCRIPTION {
        return Err("description over the spec limit".to_owned());
    }
    Ok((name, description, hidden))
}

fn frontmatter(body: &str) -> Option<&str> {
    let rest = body.strip_prefix("---")?;
    let end = rest.find("\n---")?;
    Some(&rest[..end])
}

fn strip_frontmatter(body: &str) -> &str {
    let Some(rest) = body.strip_prefix("---") else {
        return body;
    };
    match rest.find("\n---") {
        Some(end) => rest[end + 4..].trim_start_matches(['-', '\n']),
        None => body,
    }
}

fn escape_xml(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_skill(root: &Path, folder: &str, front: &str, body: &str) {
        let dir = root.join(folder);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("SKILL.md"), format!("---\n{front}\n---\n\n{body}")).unwrap();
    }

    #[test]
    fn loads_valid_skills_and_skips_broken_ones() {
        let dir = tempfile::tempdir().unwrap();
        write_skill(
            dir.path(),
            "brave-search",
            "name: brave-search\ndescription: Web search via Brave.",
            "# Search\nRun {baseDir}/search.js",
        );
        write_skill(dir.path(), "broken", "name: broken", "no description");
        write_skill(
            dir.path(),
            "quiet",
            "name: quiet\ndescription: Hidden.\ndisable-model-invocation: true",
            "body",
        );

        let skills = load(dir.path());
        assert_eq!(skills.len(), 2, "the description-less skill is skipped");
        assert_eq!(skills[0].name, "brave-search");
        assert!(!skills[0].hidden);
        assert!(skills[1].hidden);

        let section = prompt_section(&skills);
        assert!(section.contains("<name>brave-search</name>"));
        assert!(
            !section.contains("quiet"),
            "disable-model-invocation keeps a skill out of the prompt"
        );
    }

    #[test]
    fn the_folder_names_a_nameless_skill() {
        let dir = tempfile::tempdir().unwrap();
        write_skill(dir.path(), "gdcli", "description: Google Drive CLI.", "b");
        let skills = load(dir.path());
        assert_eq!(skills[0].name, "gdcli");
    }

    #[test]
    fn expand_carries_the_body_and_the_base_dir() {
        let dir = tempfile::tempdir().unwrap();
        write_skill(
            dir.path(),
            "transcribe",
            "name: transcribe\ndescription: d.",
            "Run the script.",
        );
        let skills = load(dir.path());
        let block = expand(&skills[0]).unwrap();
        assert!(block.starts_with("<skill name=\"transcribe\""));
        assert!(block.contains("Run the script."));
        assert!(!block.contains("description: d."), "frontmatter stripped");
    }

    #[test]
    fn skill_command_parses_and_skills_does_not() {
        assert_eq!(
            strip_skill_command("/skill transcribe do the podcast"),
            Some(("transcribe", "do the podcast"))
        );
        assert_eq!(strip_skill_command("/skill gdcli"), Some(("gdcli", "")));
        assert_eq!(strip_skill_command("/skills"), None);
        assert_eq!(strip_skill_command("/skill "), None);
        assert_eq!(strip_skill_command("plain text"), None);
    }

    #[test]
    fn an_empty_prompt_section_costs_nothing() {
        assert_eq!(prompt_section(&[]), "");
    }
}
