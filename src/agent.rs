//! The local agent, run from this binary through the `smartloop` crate.
//!
//! `framework::launch` starts `<this exe> __agent-serve` detached; that
//! process serves the agent API in the foreground. The agent in turn spawns
//! its own workers (project agents, the model host, the stdio MCP system
//! server, model probes) as `<current exe> -m <module>` or a hidden `__*`
//! subcommand, the way the `slp` binary does, so all of those forms are
//! recognised here on the raw argv before clap sees it.

use clap::Parser;

/// Hidden subcommand that runs the agent server in the foreground.
pub const SERVE_SUBCOMMAND: &str = "__agent-serve";
/// The CLI version a served agent was started by, so a CLI upgraded in place
/// can tell the agent it left running is not its own build.
pub const CLI_VERSION_FLAG: &str = "--cli-version";

/// Run the internal entry point `argv` names, if it names one, and return
/// the process exit code. `None` means a normal CLI invocation.
pub fn dispatch(argv: &[String]) -> Option<i32> {
    let first = argv.get(1)?.as_str();
    let result = match first {
        "-m" if argv.len() >= 3 => {
            prepare_process();
            run_module(&argv[2], argv[3..].to_vec())
        }
        SERVE_SUBCOMMAND => {
            prepare_process();
            serve(ServeArgs::parse_from(&argv[1..]))
        }
        _ if first == smartloop::llms::llm_factory::PROBE_SUBCOMMAND => {
            prepare_process();
            let config = argv.get(2).cloned().unwrap_or_default();
            return Some(smartloop::llms::llm_factory::LlmFactory::run_probe(&config));
        }
        _ => {
            let module = hidden_subcommand_module(first)?;
            prepare_process();
            run_module(module, argv[2..].to_vec())
        }
    };
    Some(match result {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("Error: {:#}", e);
            1
        }
    })
}

/// Process set-up `slp` does first thing in `main`, while the process is
/// still single-threaded: console code page, a bundled Vulkan ICD, and
/// quieter HTTP/gRPC loggers.
fn prepare_process() {
    smartloop::utils::runtime_env::apply_runtime_env();
    smartloop::init_process_env();
    for name in ["reqwest", "hyper", "hyper_util", "h2", "rustls", "tonic", "tower", "want", "mio"] {
        smartloop::utils::logging_setup::get_logger(name).set_level("WARNING");
    }
}

#[derive(Parser)]
#[command(name = SERVE_SUBCOMMAND)]
struct ServeArgs {
    /// Port to bind; without one, `SLP_API_PORT` or the settings decide, and
    /// 0 binds a free port that the agent writes to `server.port`.
    #[arg(long)]
    port: Option<u16>,
    /// Only read back by `framework::find_agent`.
    #[arg(long = "cli-version")]
    _cli_version: Option<String>,
}

/// `slp agent start`'s foreground half: supervised (a process per project)
/// and restarted on a crash.
fn serve(args: ServeArgs) -> anyhow::Result<()> {
    let settings = smartloop::config::AppSettings::new();
    let host = std::env::var("SLP_API_HOST").unwrap_or_else(|_| "127.0.0.1".to_string());
    let port = args.port.filter(|&p| p != 0).unwrap_or(settings.api_port);
    std::fs::create_dir_all(settings.home_dir())?;
    smartloop::server::start_server(&host, port, false, true, true)
}

fn runtime() -> anyhow::Result<tokio::runtime::Runtime> {
    Ok(tokio::runtime::Builder::new_multi_thread().enable_all().build()?)
}

/// `slp -m <module> [args...]`: the module's parser sees its own name as the
/// program, then the remaining arguments.
fn run_module(module: &str, args: Vec<String>) -> anyhow::Result<()> {
    use smartloop::api::project_agent::{self, ProjectAgentArgs};
    use smartloop::model_host::bench::{self, BenchArgs};
    use smartloop::model_host::entry::{self, ModelHostArgs};

    let argv = std::iter::once(module.to_string()).chain(args);
    match module {
        "smartloop.api.project_agent" => project_agent::main(ProjectAgentArgs::parse_from(argv)),
        "smartloop.api.mcp.system_server" => runtime()?.block_on(smartloop::api::mcp::system_server::run_stdio()),
        "smartloop.model_host" | "smartloop.model_host.__main__" => entry::run(ModelHostArgs::parse_from(argv)),
        "smartloop.model_host.bench" => bench::run(BenchArgs::parse_from(argv)),
        "smartloop.api.main" => {
            smartloop::api::run_server();
            Ok(())
        }
        other => anyhow::bail!("No module named {other}"),
    }
}

/// The module a hidden `__*` spawn-site subcommand stands for.
fn hidden_subcommand_module(name: &str) -> Option<&'static str> {
    match name {
        "__project-agent" => Some("smartloop.api.project_agent"),
        "__system-server" => Some("smartloop.api.mcp.system_server"),
        "__model-host" => Some("smartloop.model_host"),
        "__model-host-bench" => Some("smartloop.model_host.bench"),
        "__api-server" => Some("smartloop.api.main"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(args: &[&str]) -> Vec<String> {
        std::iter::once("smartloop").chain(args.iter().copied()).map(str::to_string).collect()
    }

    #[test]
    fn cli_commands_are_not_internal() {
        assert_eq!(dispatch(&argv(&[])), None);
        assert_eq!(dispatch(&argv(&["agent", "start"])), None);
        assert_eq!(dispatch(&argv(&["run"])), None);
    }

    #[test]
    fn hidden_subcommands_map_to_modules() {
        assert_eq!(hidden_subcommand_module("__project-agent"), Some("smartloop.api.project_agent"));
        assert_eq!(hidden_subcommand_module("__model-host"), Some("smartloop.model_host"));
        assert_eq!(hidden_subcommand_module("agent"), None);
    }

    #[test]
    fn serve_reads_its_port_and_version() {
        let args = ServeArgs::parse_from([SERVE_SUBCOMMAND, "--port", "9000", CLI_VERSION_FLAG, "1.0.34"]);
        assert_eq!(args.port, Some(9000));
        assert_eq!(args._cli_version.as_deref(), Some("1.0.34"));
    }
}
