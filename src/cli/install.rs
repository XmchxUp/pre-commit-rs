use std::fmt::Write as _;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::Result;
use indoc::indoc;
use owo_colors::OwoColorize;

use crate::cli::run;
use crate::cli::{ExitStatus, HookType};
use crate::fs::Simplified;
use crate::git;
use crate::hook::Project;
use crate::printer::Printer;
use crate::store::Store;

pub(crate) async fn install(
    config: Option<PathBuf>,
    hook_types: Vec<HookType>,
    install_hooks: bool,
    overwrite: bool,
    allow_missing_config: bool,
    printer: Printer,
) -> Result<ExitStatus> {
    if git::has_hooks_path_set().await? {
        writeln!(
            printer.stderr(),
            indoc::indoc! {"
                Cowardly refusing to install hooks with `core.hooksPath` set.
                hint: `git config --unset-all core.hooksPath` to fix this.
            "}
        )?;
        return Ok(ExitStatus::Failure);
    }

    let hook_types = get_hook_types(config.clone(), hook_types);

    let hooks_path = git::get_git_common_dir().await?.join("hooks");
    fs_err::create_dir_all(&hooks_path)?;

    let project = Project::from_config_file(config);
    let config_file = project.as_ref().ok().map(Project::config_file);
    for hook_type in hook_types {
        install_hook_script(
            config_file,
            hook_type,
            &hooks_path,
            overwrite,
            allow_missing_config,
            printer,
        )?;
    }

    if install_hooks {
        let mut project = project?;
        let store = Store::from_settings()?.init()?;
        let _lock = store.lock_async().await?;

        let hooks = project.init_hooks(&store, printer).await?;
        run::install_hooks(&hooks, printer).await?;
    }

    Ok(ExitStatus::Success)
}

fn get_hook_types(config_file: Option<PathBuf>, hook_types: Vec<HookType>) -> Vec<HookType> {
    let project = Project::from_config_file(config_file);

    let mut hook_types = if hook_types.is_empty() {
        if let Ok(ref project) = project {
            project
                .config()
                .default_install_hook_types
                .clone()
                .unwrap_or_default()
        } else {
            vec![]
        }
    } else {
        hook_types
    };
    if hook_types.is_empty() {
        hook_types = vec![HookType::PreCommit];
    }

    hook_types
}

fn install_hook_script(
    config_file: Option<&Path>,
    hook_type: HookType,
    hooks_path: &Path,
    overwrite: bool,
    skip_on_missing_config: bool,
    printer: Printer,
) -> Result<()> {
    let hook_path = hooks_path.join(hook_type.as_str());

    if hook_path.try_exists()? {
        if overwrite {
            writeln!(
                printer.stdout(),
                "Overwriting existing hook at {}",
                hook_path.user_display().cyan()
            )?;
        } else {
            if !is_our_script(&hook_path)? {
                let legacy_path = format!("{}.legacy", hook_path.display());
                fs_err::rename(&hook_path, &legacy_path)?;
                writeln!(
                    printer.stdout(),
                    "Hook already exists at {}, move it to {}.",
                    hook_path.user_display().cyan(),
                    legacy_path.user_display().yellow()
                )?;
            }
        }
    }

    let mut args = vec![
        "hook-impl".to_string(),
        format!("--hook-type={}", hook_type.as_str()),
    ];
    if let Some(config_file) = config_file {
        args.push(format!(r#"--config="{}""#, config_file.user_display()));
    }
    if skip_on_missing_config {
        args.push("--skip-on-missing-config".to_string());
    }

    let pre_commit = std::env::current_exe()?;
    let pre_commit = pre_commit.simplified().display().to_string();
    let hook_script = HOOK_TMPL
        .replace("ARGS=(hook-impl)", &format!("ARGS=({})", args.join(" ")))
        .replace(
            r#"PRE_COMMIT="pre-commit""#,
            &format!(r#"PRE_COMMIT="{pre_commit}""#),
        );
    fs_err::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(&hook_path)?
        .write_all(hook_script.as_bytes())?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        let mut perms = hook_path.metadata()?.permissions();
        perms.set_mode(0o755);
        fs_err::set_permissions(&hook_path, perms)?;
    }

    writeln!(
        printer.stdout(),
        "pre-commit installed at {}",
        hook_path.user_display().cyan()
    )?;

    Ok(())
}

static HOOK_TMPL: &str = indoc! { r#"
#!/usr/bin/env bash
# File generated by pre-commit-rs: https://github.com/j178/pre-commit-rs
# ID: 182c10f181da4464a3eec51b83331688

ARGS=(hook-impl)

HERE="$(cd "$(dirname "$0")" && pwd)"
ARGS+=(--hook-dir "$HERE" -- "$@")
PRE_COMMIT="pre-commit"

exec "$PRE_COMMIT" "${ARGS[@]}"

"# };

static PRIOR_HASHES: &[&str] = &[];

// Use a different hash for each change to the script.
// Use a different hash from `pre-commit` since our script is different.
static CURRENT_HASH: &str = "182c10f181da4464a3eec51b83331688";

/// Checks if the script contains any of the hashes that `pre-commit` has used in the past.
fn is_our_script(hook_path: &Path) -> Result<bool> {
    let content = fs_err::read_to_string(hook_path)?;
    Ok(std::iter::once(CURRENT_HASH)
        .chain(PRIOR_HASHES.iter().copied())
        .any(|hash| content.contains(hash)))
}

pub(crate) async fn uninstall(
    config: Option<PathBuf>,
    hook_types: Vec<HookType>,
    printer: Printer,
) -> Result<ExitStatus> {
    for hook_type in get_hook_types(config, hook_types) {
        let hooks_path = git::get_git_common_dir().await?.join("hooks");
        let hook_path = hooks_path.join(hook_type.as_str());
        let legacy_path = hooks_path.join(format!("{}.legacy", hook_type.as_str()));

        if !hook_path.try_exists()? {
            writeln!(
                printer.stderr(),
                "{} does not exist, skipping.",
                hook_path.user_display().cyan()
            )?;
        } else if !is_our_script(&hook_path)? {
            writeln!(
                printer.stderr(),
                "{} is not managed by pre-commit, skipping.",
                hook_path.user_display().cyan()
            )?;
        } else {
            fs_err::remove_file(&hook_path)?;
            writeln!(
                printer.stdout(),
                "Uninstalled {}",
                hook_type.as_str().cyan()
            )?;

            if legacy_path.try_exists()? {
                fs_err::rename(&legacy_path, &hook_path)?;
                writeln!(
                    printer.stdout(),
                    "Restored previous hook to {}",
                    hook_path.user_display().cyan()
                )?;
            }
        }
    }

    Ok(ExitStatus::Success)
}
