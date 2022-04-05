//! Install any hooks, aliases, etc. to set up `git-branchless` in this repo.

use std::fmt::Write;
use std::io::{stdin, stdout, BufRead, BufReader, Write as WriteIo};
use std::path::{Path, PathBuf};

use console::style;
use eyre::Context;
use path_slash::PathExt;
use tracing::{instrument, warn};

use crate::opts::write_man_pages;
use lib::core::config::{get_core_hooks_path, get_default_branch_name};
use lib::core::effects::Effects;
use lib::git::{Config, ConfigRead, ConfigWrite, GitRunInfo, GitVersion, Repo};

const ALL_HOOKS: &[(&str, &str)] = &[
    (
        "post-commit",
        r#"
git branchless hook-post-commit "$@"
"#,
    ),
    (
        "post-merge",
        r#"
git branchless hook-post-merge "$@"
"#,
    ),
    (
        "post-rewrite",
        r#"
git branchless hook-post-rewrite "$@"
"#,
    ),
    (
        "post-checkout",
        r#"
git branchless hook-post-checkout "$@"
"#,
    ),
    (
        "pre-auto-gc",
        r#"
git branchless hook-pre-auto-gc "$@"
"#,
    ),
    (
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
    ),
];

const ALL_ALIASES: &[(&str, &str)] = &[
    ("amend", "amend"),
    ("co", "checkout"),
    ("hide", "hide"),
    ("move", "move"),
    ("next", "next"),
    ("prev", "prev"),
    ("restack", "restack"),
    ("reword", "reword"),
    ("sl", "smartlog"),
    ("smartlog", "smartlog"),
    ("sync", "sync"),
    ("undo", "undo"),
    ("unhide", "unhide"),
];

#[derive(Debug)]
enum Hook {
    /// Regular Git hook.
    RegularHook { path: PathBuf },

    /// For Twitter multihooks.
    MultiHook { path: PathBuf },
}

#[instrument]
fn determine_hook_path(repo: &Repo, hook_type: &str) -> eyre::Result<Hook> {
    let multi_hooks_path = repo.get_path().join("hooks_multi");
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

fn append_hook(new_lines: &mut String, hook_contents: &str) {
    new_lines.push_str(UPDATE_MARKER_START);
    new_lines.push('\n');
    new_lines.push_str(hook_contents);
    new_lines.push_str(UPDATE_MARKER_END);
    new_lines.push('\n');
}

fn update_between_lines(lines: &str, updated_lines: &str) -> String {
    let mut new_lines = String::new();
    let mut found_marker = false;
    let mut is_ignoring_lines = false;
    for line in lines.lines() {
        if line == UPDATE_MARKER_START {
            found_marker = true;
            is_ignoring_lines = true;
            append_hook(&mut new_lines, updated_lines);
        } else if line == UPDATE_MARKER_END {
            is_ignoring_lines = false;
        } else if !is_ignoring_lines {
            new_lines.push_str(line);
            new_lines.push('\n');
        }
    }
    if is_ignoring_lines {
        warn!("Unterminated branchless config comment in hook");
    } else if !found_marker {
        append_hook(&mut new_lines, updated_lines);
    }
    new_lines
}

#[instrument]
fn write_script(path: &Path, contents: &str) -> eyre::Result<()> {
    let script_dir = path
        .parent()
        .ok_or_else(|| eyre::eyre!("No parent for dir {:?}", path))?;
    std::fs::create_dir_all(script_dir).wrap_err("Creating script dir")?;

    std::fs::write(path, contents).wrap_err("Writing script contents")?;

    // Setting hook file as executable only supported on Unix systems.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let metadata = std::fs::metadata(path).wrap_err("Reading script permissions")?;
        let mut permissions = metadata.permissions();
        let mode = permissions.mode();
        // Set execute bits.
        let mode = mode | 0o111;
        permissions.set_mode(mode);
        std::fs::set_permissions(path, permissions)
            .wrap_err_with(|| format!("Marking {:?} as executable", path))?;
    }

    Ok(())
}

#[instrument]
fn update_hook_contents(hook: &Hook, hook_contents: &str) -> eyre::Result<()> {
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
                return Err(eyre::eyre!(other));
            }
        },
        Hook::MultiHook { path } => (path, format!("{}\n{}", SHEBANG, hook_contents)),
    };

    write_script(hook_path, &hook_contents).wrap_err("Writing hook script")?;

    Ok(())
}

#[instrument]
fn install_hook(repo: &Repo, hook_type: &str, hook_script: &str) -> eyre::Result<()> {
    let hook = determine_hook_path(repo, hook_type)?;
    update_hook_contents(&hook, hook_script)?;
    Ok(())
}

#[instrument]
fn install_hooks(effects: &Effects, repo: &Repo) -> eyre::Result<()> {
    for (hook_type, hook_script) in ALL_HOOKS {
        writeln!(
            effects.get_output_stream(),
            "Installing hook: {}",
            hook_type
        )?;
        install_hook(repo, hook_type, hook_script)?;
    }

    let hooks_path: Option<PathBuf> = repo.get_readonly_config()?.get("core.hooksPath")?;
    if let Some(hooks_path) = hooks_path {
        writeln!(
            effects.get_output_stream(),
            "{}: the configuration value core.hooksPath was set to: {}",
            style("Warning").yellow().bold(),
            hooks_path.to_string_lossy()
        )?;
        writeln!(
            effects.get_output_stream(),
            "The Git hooks above may have been installed to an unexpected location."
        )?;
    }

    Ok(())
}

#[instrument]
fn uninstall_hooks(effects: &Effects, repo: &Repo) -> eyre::Result<()> {
    for (hook_type, _hook_script) in ALL_HOOKS {
        writeln!(
            effects.get_output_stream(),
            "Uninstalling hook: {}",
            hook_type
        )?;
        install_hook(
            repo,
            hook_type,
            r#"
# This hook has been uninstalled.
# Run `git branchless init` to reinstall.
"#,
        )?;
    }
    Ok(())
}

/// Determine if we should make an alias of the form `branchless smartlog` or
/// `branchless-smartlog`.
///
/// The form of the alias is important because it determines what command Git
/// tries to look up with `man` when you run e.g. `git smartlog --help`:
///
/// - `branchless smartlog`: invokes `man git-branchless`, which means that the
/// subcommand is not included in the `man` invocation, so it can only show
/// generic help.
/// - `branchless-smartlog`: invokes `man git-branchless-smartlog, so the
/// subcommand is included in the `man` invocation, so it can show more specific
/// help.
fn should_use_wrapped_command_alias() -> bool {
    cfg!(feature = "man-pages")
}

#[instrument]
fn install_alias(
    effects: &Effects,
    repo: &Repo,
    config: &mut Config,
    default_config: &Config,
    from: &str,
    to: &str,
) -> eyre::Result<()> {
    let alias_key = format!("alias.{}", from);

    let existing_alias: Option<String> = config.get(&alias_key)?;
    if existing_alias.is_some() {
        config.remove(&alias_key)?;
    }

    let default_alias: Option<String> = default_config.get(&alias_key)?;
    if default_alias.is_some() {
        writeln!(
            effects.get_output_stream(),
            "Alias {} already installed, skipping",
            from
        )?;
        return Ok(());
    }

    let alias = if should_use_wrapped_command_alias() {
        format!("branchless-{}", to)
    } else {
        format!("branchless {}", to)
    };
    config.set(&alias_key, alias)?;
    Ok(())
}

#[instrument]
fn detect_main_branch_name(repo: &Repo) -> eyre::Result<Option<String>> {
    if let Some(default_branch_name) = get_default_branch_name(repo)? {
        if repo
            .find_branch(&default_branch_name, git2::BranchType::Local)?
            .is_some()
        {
            return Ok(Some(default_branch_name));
        }
    }

    for branch_name in [
        "master",
        "main",
        "mainline",
        "devel",
        "develop",
        "development",
        "trunk",
    ] {
        if repo
            .find_branch(branch_name, git2::BranchType::Local)?
            .is_some()
        {
            return Ok(Some(branch_name.to_string()));
        }
    }
    Ok(None)
}

#[instrument]
fn install_aliases(
    effects: &Effects,
    repo: &mut Repo,
    config: &mut Config,
    default_config: &Config,
    git_run_info: &GitRunInfo,
) -> eyre::Result<()> {
    for (from, to) in ALL_ALIASES {
        install_alias(effects, repo, config, default_config, from, to)?;
    }

    let version_str = git_run_info
        .run_silent(repo, None, &["version"], Default::default())
        .wrap_err("Determining Git version")?
        .stdout;
    let version_str =
        String::from_utf8(version_str).wrap_err("Decoding stdout from Git subprocess")?;
    let version_str = version_str.trim();
    let version: GitVersion = version_str
        .parse()
        .wrap_err_with(|| format!("Parsing Git version string: {}", version_str))?;
    if version < GitVersion(2, 29, 0) {
        write!(
            effects.get_output_stream(),
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
        )?;
    }

    Ok(())
}

#[instrument]
fn install_man_pages(effects: &Effects, repo: &Repo, config: &mut Config) -> eyre::Result<()> {
    let should_install = cfg!(feature = "man-pages");
    if !should_install {
        return Ok(());
    }

    let man_dir = repo.get_man_dir();
    let man_dir_relative = {
        let man_dir_relative = man_dir.strip_prefix(repo.get_path()).wrap_err_with(|| {
            format!(
                "Getting relative path for {:?} with respect to {:?}",
                &man_dir,
                repo.get_path()
            )
        })?;
        &man_dir_relative.to_str().ok_or_else(|| {
            eyre::eyre!(
                "Could not convert man dir to UTF-8 string: {:?}",
                &man_dir_relative
            )
        })?
    };
    config.set(
        "man.branchless.cmd",
        format!(
            // FIXME: the path to the man directory is not shell-escaped.
            //
            // NB: the trailing `:` at the end of `MANPATH` indicates to `man`
            // that it should try its normal lookup paths if the requested
            // `man`-page cannot be found in the provided `MANPATH`.
            "env MANPATH=.git/{}: man",
            man_dir_relative
        ),
    )?;
    config.set("man.viewer", "branchless")?;

    write_man_pages(&man_dir).wrap_err_with(|| format!("Writing man-pages to: {:?}", &man_dir))?;
    Ok(())
}

#[instrument(skip(r#in))]
fn set_configs(
    r#in: &mut impl BufRead,
    effects: &Effects,
    repo: &Repo,
    config: &mut Config,
    main_branch_name: Option<&str>,
) -> eyre::Result<()> {
    let main_branch_name = match main_branch_name {
        Some(main_branch_name) => main_branch_name.to_string(),

        None => match detect_main_branch_name(repo)? {
            Some(main_branch_name) => {
                writeln!(
                    effects.get_output_stream(),
                    "Auto-detected your main branch as: {}",
                    console::style(&main_branch_name).bold()
                )?;
                writeln!(
                    effects.get_output_stream(),
                    "If this is incorrect, run: git config branchless.core.mainBranch <branch>"
                )?;
                main_branch_name
            }

            None => {
                writeln!(
                    effects.get_output_stream(),
                    "{}",
                    console::style("Your main branch name could not be auto-detected!")
                        .yellow()
                        .bold()
                )?;
                writeln!(
                    effects.get_output_stream(),
                    "Examples of a main branch: master, main, trunk, etc."
                )?;
                writeln!(
                    effects.get_output_stream(),
                    "See https://github.com/arxanas/git-branchless/wiki/Concepts#main-branch"
                )?;
                write!(
                    effects.get_output_stream(),
                    "Enter the name of your main branch: "
                )?;
                stdout().flush()?;
                let mut input = String::new();
                r#in.read_line(&mut input)?;
                match input.trim() {
                    "" => eyre::bail!("No main branch name provided"),
                    main_branch_name => main_branch_name.to_string(),
                }
            }
        },
    };

    config.set("branchless.core.mainBranch", main_branch_name)?;
    config.set("advice.detachedHead", false)?;
    config.set("log.excludeDecoration", "refs/branchless/*")?;

    Ok(())
}

const INCLUDE_PATH_REGEX: &str = r"^branchless/";

/// Create an isolated configuration file under `.git/branchless`, which is then
/// included into the repository's main configuration file. This makes it easier
/// to uninstall our settings (or for the user to override our settings) without
/// needing to modify the user's configuration file.
#[instrument]
fn create_isolated_config(
    effects: &Effects,
    repo: &Repo,
    mut parent_config: Config,
) -> eyre::Result<Config> {
    let config_path = repo.get_config_path();
    let config_dir = config_path
        .parent()
        .ok_or_else(|| eyre::eyre!("Could not get parent config directory"))?;
    std::fs::create_dir_all(config_dir).wrap_err("Creating config path parent")?;

    let config = Config::open(&config_path)?;
    let config_path_relative = config_path
        .strip_prefix(repo.get_path())
        .wrap_err("Getting relative config path")?;
    // Be careful when setting paths on Windows. Since the path would have a
    // backslash, naively using it produces
    //
    //    Git error GenericError: invalid escape at config
    //
    // We need to convert it to forward-slashes for Git. See also
    // https://stackoverflow.com/a/28520596.
    let config_path_relative = config_path_relative.to_slash().ok_or_else(|| {
        eyre::eyre!(
            "Could not convert config path to UTF-8 string: {:?}",
            &config_path_relative
        )
    })?;
    parent_config.set_multivar("include.path", INCLUDE_PATH_REGEX, config_path_relative)?;

    writeln!(
        effects.get_output_stream(),
        "Created config file at {}",
        config_path.to_string_lossy()
    )?;
    Ok(config)
}

/// Delete the configuration file created by `create_isolated_config` and remove
/// its `include` directive from the repository's configuration file.
#[instrument]
fn delete_isolated_config(
    effects: &Effects,
    repo: &Repo,
    mut parent_config: Config,
) -> eyre::Result<()> {
    writeln!(
        effects.get_output_stream(),
        "Removing config file: {}",
        repo.get_config_path().to_string_lossy()
    )?;
    parent_config.remove_multivar("include.path", INCLUDE_PATH_REGEX)?;
    let result = match std::fs::remove_file(repo.get_config_path()) {
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            writeln!(
                effects.get_output_stream(),
                "(The config file was not present, ignoring)"
            )?;
            Ok(())
        }
        result => result,
    };
    let result = result.wrap_err("Deleting isolated config")?;
    Ok(result)
}

/// Initialize `git-branchless` in the current repo.
#[instrument]
pub fn init(
    effects: &Effects,
    git_run_info: &GitRunInfo,
    main_branch_name: Option<&str>,
) -> eyre::Result<()> {
    let mut in_ = BufReader::new(stdin());
    let mut repo = Repo::from_current_dir()?;
    let default_config = Config::open_default()?;
    let readonly_config = repo.get_readonly_config()?;
    let mut config = create_isolated_config(effects, &repo, readonly_config.into_config())?;

    set_configs(&mut in_, effects, &repo, &mut config, main_branch_name)?;
    install_hooks(effects, &repo)?;
    install_aliases(
        effects,
        &mut repo,
        &mut config,
        &default_config,
        git_run_info,
    )?;
    install_man_pages(effects, &repo, &mut config)?;
    writeln!(
        effects.get_output_stream(),
        "{}",
        console::style("Successfully installed git-branchless.")
            .green()
            .bold()
    )?;
    writeln!(
        effects.get_output_stream(),
        "To uninstall, run: {}",
        console::style("git branchless init --uninstall").bold()
    )?;
    Ok(())
}

/// Uninstall `git-branchless` in the current repo.
#[instrument]
pub fn uninstall(effects: &Effects) -> eyre::Result<()> {
    let repo = Repo::from_current_dir()?;
    let readonly_config = repo.get_readonly_config().wrap_err("Getting repo config")?;
    delete_isolated_config(effects, &repo, readonly_config.into_config())?;
    uninstall_hooks(effects, &repo)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{update_between_lines, ALL_ALIASES, UPDATE_MARKER_END, UPDATE_MARKER_START};

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

    #[test]
    fn test_all_alias_binaries_exist() {
        let all_alias_binaries_installed = cfg!(feature = "man-pages");
        if !all_alias_binaries_installed {
            return;
        }

        for (_from, to) in ALL_ALIASES {
            let executable_name = format!("git-branchless-{}", to);

            // For each subcommand that's been aliased, asserts that a binary
            // with the corresponding name exists in `Cargo.toml`. If this test
            // fails, then it may mean that a new binary entry should be added.
            //
            // Note that this check may require a `cargo clean` to clear out any
            // old executables in order to produce deterministic results.
            assert_cmd::cmd::Command::cargo_bin(executable_name)
                .unwrap()
                .arg("--help")
                .assert()
                .success();
        }
    }
}
