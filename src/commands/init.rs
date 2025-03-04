//! Install any hooks, aliases, etc. to set up `git-branchless` in this repo.

use std::fmt::Display;
use std::io::{stdin, stdout, BufRead, BufReader, Write};
use std::path::PathBuf;

use anyhow::Context;
use console::style;
use fn_error_context::context;
use log::warn;

use crate::core::config::get_core_hooks_path;
use crate::util::{get_repo, run_git_silent, wrap_git_error, GitExecutable, GitVersion};

#[derive(Debug)]
enum Hook {
    /// Regular Git hook.
    RegularHook { path: PathBuf },

    /// For Twitter multihooks.
    MultiHook { path: PathBuf },
}

#[context("Determining hook path")]
fn determine_hook_path(repo: &git2::Repository, hook_type: &str) -> anyhow::Result<Hook> {
    let multi_hooks_path = repo.path().join("hooks_multi");
    let hook = if multi_hooks_path.exists() {
        let path = multi_hooks_path
            .join(format!("{}.d", hook_type))
            .join("00_local_branchless");
        Hook::MultiHook { path }
    } else {
        let hooks_dir = get_core_hooks_path(repo)?;
        let path = hooks_dir.join(hook_type);
        Hook::RegularHook { path }
    };
    Ok(hook)
}

const SHEBANG: &str = "#!/bin/sh";
const UPDATE_MARKER_START: &str = "## START BRANCHLESS CONFIG";
const UPDATE_MARKER_END: &str = "## END BRANCHLESS CONFIG";

fn update_between_lines(lines: &str, updated_lines: &str) -> String {
    let mut new_lines = String::new();
    let mut is_ignoring_lines = false;
    for line in lines.lines() {
        if line == UPDATE_MARKER_START {
            is_ignoring_lines = true;
            new_lines.push_str(UPDATE_MARKER_START);
            new_lines.push('\n');
            new_lines.push_str(updated_lines);
            new_lines.push_str(UPDATE_MARKER_END);
            new_lines.push('\n');
        } else if line == UPDATE_MARKER_END {
            is_ignoring_lines = false;
        } else if !is_ignoring_lines {
            new_lines.push_str(line);
            new_lines.push('\n');
        }
    }
    if is_ignoring_lines {
        warn!("Unterminated branchless config comment in hook");
    }
    new_lines
}

#[context("Updating hook contents: {:?}", hook)]
fn update_hook_contents(hook: &Hook, hook_contents: &str) -> anyhow::Result<()> {
    let (hook_path, hook_contents) = match hook {
        Hook::RegularHook { path } => match std::fs::read_to_string(path) {
            Ok(lines) => {
                let lines = update_between_lines(&lines, hook_contents);
                (path, lines)
            }
            Err(ref err) if err.kind() == std::io::ErrorKind::NotFound => {
                let hook_contents = format!(
                    "{}\n{}\n{}\n{}\n",
                    SHEBANG, UPDATE_MARKER_START, hook_contents, UPDATE_MARKER_END
                );
                (path, hook_contents)
            }
            Err(other) => {
                return Err(anyhow::anyhow!(other));
            }
        },
        Hook::MultiHook { path } => (path, format!("{}\n{}", SHEBANG, hook_contents)),
    };

    let hook_dir = hook_path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("No parent for dir {:?}", hook_path))?;
    std::fs::create_dir_all(hook_dir)
        .with_context(|| format!("Creating hook dir {:?}", hook_path))?;
    std::fs::write(hook_path, hook_contents)
        .with_context(|| format!("Writing hook contents to {:?}", hook_path))?;

    // Setting hook file as executable only supported on Unix systems.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let metadata = std::fs::metadata(hook_path)
            .with_context(|| format!("Reading hook permissions for {:?}", hook_path))?;
        let mut permissions = metadata.permissions();
        let mode = permissions.mode();
        // Set execute bits.
        let mode = mode | 0o111;
        permissions.set_mode(mode);
        std::fs::set_permissions(hook_path, permissions)
            .with_context(|| format!("Marking {:?} as executable", hook_path))?;
    }

    Ok(())
}

#[context("Installing hook of type: {:?}", hook_type)]
fn install_hook(repo: &git2::Repository, hook_type: &str, hook_script: &str) -> anyhow::Result<()> {
    println!("Installing hook: {}", hook_type);
    let hook = determine_hook_path(repo, hook_type)?;
    update_hook_contents(&hook, hook_script)?;
    Ok(())
}

#[context("Installing all hooks")]
fn install_hooks(repo: &git2::Repository) -> anyhow::Result<()> {
    install_hook(
        repo,
        "post-commit",
        r#"
git branchless hook-post-commit "$@"
"#,
    )?;
    install_hook(
        repo,
        "post-rewrite",
        r#"
git branchless hook-post-rewrite "$@"
"#,
    )?;
    install_hook(
        repo,
        "post-checkout",
        r#"
git branchless hook-post-checkout "$@"
"#,
    )?;
    install_hook(
        repo,
        "pre-auto-gc",
        r#"
git branchless hook-pre-auto-gc "$@"
"#,
    )?;
    install_hook(
        repo,
        "reference-transaction",
        r#"
# Avoid canceling the reference transaction in the case that `branchless` fails
# for whatever reason.
git branchless hook-reference-transaction "$@" || (
    echo 'branchless: Failed to process reference transaction!'
    echo 'branchless: Some events (e.g. branch updates) may have been lost.'
    echo 'branchless: This is a bug. Please report it.'
)
"#,
    )?;
    Ok(())
}

#[context("Installing alias: git {:?} -> git branchless {:?}", from, to)]
fn install_alias(config: &mut git2::Config, from: &str, to: &str) -> anyhow::Result<()> {
    println!(
        "Installing alias (non-global): git {} -> git branchless {}",
        from, to
    );
    config
        .set_str(
            format!("alias.{}", from).as_str(),
            format!("branchless {}", to).as_str(),
        )
        .map_err(wrap_git_error)?;
    Ok(())
}

fn detect_main_branch_name(repo: &git2::Repository) -> Option<String> {
    [
        "master",
        "main",
        "mainline",
        "devel",
        "develop",
        "development",
        "trunk",
    ]
    .iter()
    .find_map(|branch_name| {
        if repo
            .find_branch(branch_name, git2::BranchType::Local)
            .is_ok()
        {
            Some(branch_name.to_string())
        } else {
            None
        }
    })
}

#[context("Installing all aliases")]
fn install_aliases(
    repo: &mut git2::Repository,
    config: &mut git2::Config,
    git_executable: &GitExecutable,
) -> anyhow::Result<()> {
    install_alias(config, "smartlog", "smartlog")?;
    install_alias(config, "sl", "smartlog")?;
    install_alias(config, "hide", "hide")?;
    install_alias(config, "unhide", "unhide")?;
    install_alias(config, "prev", "prev")?;
    install_alias(config, "next", "next")?;
    install_alias(config, "restack", "restack")?;
    install_alias(config, "undo", "undo")?;
    install_alias(config, "move", "move")?;

    let version_str = run_git_silent(repo, git_executable, None, &["version"])
        .with_context(|| "Determining Git version")?;
    let version_str = version_str.trim();
    let version: GitVersion = version_str
        .parse()
        .with_context(|| format!("Parsing Git version string: {}", version_str))?;
    if version < GitVersion(2, 29, 0) {
        print!(
            "\
{warning_str}: the branchless workflow's `git undo` command requires Git
v2.29 or later, but your Git version is: {version_str}

Some operations, such as branch updates, won't be correctly undone. Other
operations may be undoable. Attempt at your own risk.

Once you upgrade to Git v2.29, run `git branchless init` again. Any work you
do from then on will be correctly undoable.

This only applies to the `git undo` command. Other commands which are part of
the branchless workflow will work properly.
",
            warning_str = style("Warning").yellow().bold(),
            version_str = version_str,
        );
    }

    Ok(())
}

#[derive(Debug)]
enum ConfigValue {
    Bool(bool),
    String(String),
}

impl Display for ConfigValue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConfigValue::Bool(value) => write!(f, "{}", value),
            ConfigValue::String(value) => write!(f, "{}", value),
        }
    }
}

#[context("Setting config {}", name)]
fn set_config(config: &mut git2::Config, name: &str, value: ConfigValue) -> anyhow::Result<()> {
    println!("Setting config (non-global): {} = {}", name, value);
    match value {
        ConfigValue::Bool(value) => config.set_bool(name, value)?,
        ConfigValue::String(value) => config.set_str(name, &value)?,
    }
    Ok(())
}

#[context("Setting all configs")]
fn set_configs(
    r#in: &mut impl BufRead,
    repo: &git2::Repository,
    config: &mut git2::Config,
) -> anyhow::Result<()> {
    let main_branch_name = match detect_main_branch_name(repo) {
        Some(main_branch_name) => {
            println!(
                "Auto-detected your main branch as: {}",
                console::style(&main_branch_name).bold()
            );
            println!("If this is incorrect, run: git config branchless.core.mainBranch <branch>");
            main_branch_name
        }
        None => {
            println!("Your main branch name could not be auto-detected.");
            println!("Examples of a main branch: master, main, trunk, etc.");
            println!("See https://github.com/arxanas/git-branchless/wiki/Concepts#main-branch");
            print!("Enter the name of your main branch: ");
            stdout().flush()?;
            let mut input = String::new();
            r#in.read_line(&mut input)?;
            match input.trim() {
                "" => anyhow::bail!("No main branch name provided"),
                main_branch_name => main_branch_name.to_string(),
            }
        }
    };
    set_config(
        config,
        "branchless.core.mainBranch",
        ConfigValue::String(main_branch_name),
    )?;
    set_config(config, "advice.detachedHead", ConfigValue::Bool(false))?;
    Ok(())
}

/// Initialize `git-branchless` in the current repo.
///
/// Args:
/// * `out`: The output stream to write to.
/// * `git_executable`: The path to the `git` executable on disk.
#[context("Initializing git-branchless for repo")]
pub fn init(git_executable: &GitExecutable) -> anyhow::Result<()> {
    let mut in_ = BufReader::new(stdin());
    let mut repo = get_repo()?;
    let mut config = repo.config().with_context(|| "Getting repo config")?;
    set_configs(&mut in_, &repo, &mut config)?;
    install_hooks(&repo)?;
    install_aliases(&mut repo, &mut config, git_executable)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{update_between_lines, UPDATE_MARKER_END, UPDATE_MARKER_START};

    #[test]
    fn test_update_between_lines() {
        let input = format!(
            "\
hello, world
{}
contents 1
{}
goodbye, world
",
            UPDATE_MARKER_START, UPDATE_MARKER_END
        );
        let expected = format!(
            "\
hello, world
{}
contents 2
contents 3
{}
goodbye, world
",
            UPDATE_MARKER_START, UPDATE_MARKER_END
        );

        assert_eq!(
            update_between_lines(
                &input,
                "\
contents 2
contents 3
"
            ),
            expected
        )
    }
}
