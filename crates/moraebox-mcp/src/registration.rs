use std::{
    env,
    ffi::{OsStr, OsString},
    path::{Path, PathBuf},
    process::Command,
};

use clap::{Args, ValueEnum};
use serde_json::json;

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
enum Agent {
    Codex,
    ClaudeCode,
}

impl Agent {
    fn executable(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::ClaudeCode => "claude",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
enum RegistrationBackend {
    Libkrun,
    Process,
}

impl RegistrationBackend {
    fn as_str(self) -> &'static str {
        match self {
            Self::Libkrun => "libkrun",
            Self::Process => "process",
        }
    }
}

/// Register moraebox as a user-wide stdio MCP server.
#[derive(Debug, Args)]
pub struct InstallArgs {
    #[arg(value_enum)]
    agent: Agent,
    /// MCP server name in the agent configuration.
    #[arg(long, default_value = "moraebox", value_parser = parse_server_name)]
    name: String,
    /// Sandbox backend. `process` is deterministic but is not isolated.
    #[arg(long, value_enum, default_value_t = RegistrationBackend::Libkrun)]
    backend: RegistrationBackend,
    /// Root filesystem registered for the libkrun backend.
    #[arg(long, env = "MORAE_ROOTFS")]
    rootfs: Option<PathBuf>,
    /// Override the automatically discovered signed VMM helper.
    #[arg(long, env = "MORAE_HELPER_PATH")]
    helper: Option<PathBuf>,
    /// Override the automatically discovered libkrun library.
    #[arg(long, env = "MORAE_LIBKRUN_PATH")]
    libkrun: Option<PathBuf>,
    /// Override the automatically discovered libkrun dependency directories.
    #[arg(long, env = "MORAE_LIB_DIR")]
    lib_dir: Option<PathBuf>,
    #[arg(long, default_value_t = 2)]
    cpus: u8,
    #[arg(long, default_value_t = 512)]
    memory_mib: u32,
    /// Command agents should use to launch this MCP server.
    #[arg(long)]
    server_command: Option<OsString>,
    /// Print the exact agent CLI program and argv without changing configuration.
    #[arg(long)]
    dry_run: bool,
}

#[derive(Debug, Eq, PartialEq)]
struct CommandPlan {
    program: OsString,
    args: Vec<OsString>,
}

type Environment = Vec<(&'static str, OsString)>;
type ServerConfiguration = (Environment, Vec<OsString>);

impl CommandPlan {
    fn print_json(&self) -> Result<(), String> {
        let value = json!({
            "program": self.program.to_string_lossy(),
            "args": self
                .args
                .iter()
                .map(|arg| arg.to_string_lossy())
                .collect::<Vec<_>>(),
        });
        println!(
            "{}",
            serde_json::to_string_pretty(&value)
                .map_err(|error| format!("failed to render registration command: {error}"))?
        );
        Ok(())
    }
}

pub fn install(args: &InstallArgs) -> Result<(), String> {
    let server_command = resolve_server_command(args.server_command.as_deref())?;
    let plan = build_command_plan(args, server_command)?;

    if args.backend == RegistrationBackend::Process {
        eprintln!(
            "warning: the process backend is for deterministic development only; it does not provide VM isolation"
        );
    }
    if args.dry_run {
        return plan.print_json();
    }

    let status = Command::new(&plan.program)
        .args(&plan.args)
        .status()
        .map_err(|error| {
            format!(
                "failed to run {}: {error}; install the agent CLI or use --dry-run",
                plan.program.to_string_lossy()
            )
        })?;
    if status.success() {
        Ok(())
    } else {
        Err(format!(
            "{} failed with status {status}",
            plan.program.to_string_lossy()
        ))
    }
}

fn build_command_plan(
    install: &InstallArgs,
    server_command: OsString,
) -> Result<CommandPlan, String> {
    let (environment, server_args) = server_configuration(install)?;
    let mut args = vec![OsString::from("mcp"), OsString::from("add")];

    match install.agent {
        Agent::Codex => {
            args.push(install.name.clone().into());
            for (key, value) in &environment {
                args.push("--env".into());
                args.push(env_assignment(key, value));
            }
        }
        Agent::ClaudeCode => {
            args.extend(["--scope".into(), "user".into()]);
            if !environment.is_empty() {
                args.push("--env".into());
                args.extend(
                    environment
                        .iter()
                        .map(|(key, value)| env_assignment(key, value)),
                );
            }
            args.extend([
                "--transport".into(),
                "stdio".into(),
                install.name.clone().into(),
            ]);
        }
    }

    args.push("--".into());
    args.push(server_command);
    args.extend(server_args);
    Ok(CommandPlan {
        program: install.agent.executable().into(),
        args,
    })
}

fn server_configuration(install: &InstallArgs) -> Result<ServerConfiguration, String> {
    if install.backend == RegistrationBackend::Process {
        return Ok((
            Vec::new(),
            vec![
                "--backend".into(),
                RegistrationBackend::Process.as_str().into(),
            ],
        ));
    }

    let rootfs = install
        .rootfs
        .as_ref()
        .ok_or("libkrun registration requires --rootfs or MORAE_ROOTFS")?;
    let mut environment = vec![("MORAE_ROOTFS", rootfs.as_os_str().to_owned())];
    for (name, path) in [
        ("MORAE_HELPER_PATH", install.helper.as_ref()),
        ("MORAE_LIBKRUN_PATH", install.libkrun.as_ref()),
        ("MORAE_LIB_DIR", install.lib_dir.as_ref()),
    ] {
        if let Some(path) = path {
            environment.push((name, path.as_os_str().to_owned()));
        }
    }
    let server_args = vec![
        "--backend".into(),
        RegistrationBackend::Libkrun.as_str().into(),
        "--cpus".into(),
        install.cpus.to_string().into(),
        "--memory-mib".into(),
        install.memory_mib.to_string().into(),
    ];
    Ok((environment, server_args))
}

fn env_assignment(key: &str, value: &OsStr) -> OsString {
    let mut assignment = OsString::from(key);
    assignment.push("=");
    assignment.push(value);
    assignment
}

fn resolve_server_command(explicit: Option<&OsStr>) -> Result<OsString, String> {
    if let Some(explicit) = explicit {
        return Ok(explicit.to_owned());
    }

    let invoked_as = env::args_os()
        .next()
        .unwrap_or_else(|| OsString::from("morae-mcp"));
    let invoked_path = Path::new(&invoked_as);
    if invoked_path.components().count() > 1 {
        if invoked_path.is_absolute() {
            return Ok(invoked_as);
        }
        return env::current_dir()
            .map(|directory| directory.join(invoked_path).into_os_string())
            .map_err(|error| format!("failed to resolve morae-mcp path: {error}"));
    }

    if let Some(path) = env::var_os("PATH") {
        for directory in env::split_paths(&path) {
            let candidate = directory.join(&invoked_as);
            if candidate.is_file() {
                return Ok(candidate.into_os_string());
            }
            #[cfg(windows)]
            if candidate.extension().is_none() {
                let executable = candidate.with_extension("exe");
                if executable.is_file() {
                    return Ok(executable.into_os_string());
                }
            }
        }
    }

    env::current_exe()
        .map(PathBuf::into_os_string)
        .map_err(|error| format!("failed to resolve morae-mcp executable: {error}"))
}

fn parse_server_name(input: &str) -> Result<String, String> {
    if input.is_empty()
        || !input
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
    {
        return Err("server name must use only ASCII letters, numbers, '-' or '_'".into());
    }
    Ok(input.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn install_args(agent: Agent) -> InstallArgs {
        InstallArgs {
            agent,
            name: "moraebox".into(),
            backend: RegistrationBackend::Libkrun,
            rootfs: Some("/rootfs".into()),
            helper: None,
            libkrun: None,
            lib_dir: None,
            cpus: 2,
            memory_mib: 512,
            server_command: None,
            dry_run: true,
        }
    }

    fn string_args(plan: &CommandPlan) -> Vec<String> {
        plan.args
            .iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn builds_codex_user_registration() {
        let plan =
            build_command_plan(&install_args(Agent::Codex), "/bin/morae-mcp".into()).unwrap();
        assert_eq!(plan.program, "codex");
        assert_eq!(
            string_args(&plan),
            [
                "mcp",
                "add",
                "moraebox",
                "--env",
                "MORAE_ROOTFS=/rootfs",
                "--",
                "/bin/morae-mcp",
                "--backend",
                "libkrun",
                "--cpus",
                "2",
                "--memory-mib",
                "512",
            ]
        );
    }

    #[test]
    fn builds_claude_user_registration() {
        let plan =
            build_command_plan(&install_args(Agent::ClaudeCode), "/bin/morae-mcp".into()).unwrap();
        assert_eq!(plan.program, "claude");
        assert_eq!(
            string_args(&plan),
            [
                "mcp",
                "add",
                "--scope",
                "user",
                "--env",
                "MORAE_ROOTFS=/rootfs",
                "--transport",
                "stdio",
                "moraebox",
                "--",
                "/bin/morae-mcp",
                "--backend",
                "libkrun",
                "--cpus",
                "2",
                "--memory-mib",
                "512",
            ]
        );
    }

    #[test]
    fn process_registration_is_explicit_and_has_no_native_environment() {
        let mut install = install_args(Agent::Codex);
        install.backend = RegistrationBackend::Process;
        let (environment, server_args) = server_configuration(&install).unwrap();
        assert!(environment.is_empty());
        assert_eq!(server_args, ["--backend", "process"]);
    }

    #[test]
    fn libkrun_registration_requires_rootfs() {
        let mut install = install_args(Agent::Codex);
        install.rootfs = None;
        assert_eq!(
            server_configuration(&install).unwrap_err(),
            "libkrun registration requires --rootfs or MORAE_ROOTFS"
        );
    }

    #[test]
    fn validates_portable_server_names() {
        assert_eq!(parse_server_name("moraebox_2").unwrap(), "moraebox_2");
        assert!(parse_server_name("morae box").is_err());
    }
}
