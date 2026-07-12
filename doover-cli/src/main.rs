//! `doover` — the Rust replacement for the pydoover CLI: a thin shell over
//! the three device-sidecar gRPC interfaces.
//!
//! ```text
//! doover device_agent <cmd> [args...]
//! doover platform     <cmd> [args...]
//! doover modbus       <cmd> [args...]
//! ```
//!
//! Subcommand and argument names match pydoover's (snake_case, with
//! kebab-case aliases), so existing device scripts keep working. Results are
//! printed as JSON on stdout; errors go to stderr with a nonzero exit.

mod device_agent;
mod modbus;
mod parse;
mod platform;

use clap::{Parser, Subcommand};
use serde_json::Value;

pub type CliResult = Result<(), Box<dyn std::error::Error + Send + Sync>>;

pub const DEFAULT_DDA_URI: &str = "localhost:50051";
pub const DEFAULT_PLT_URI: &str = "localhost:50053";
pub const DEFAULT_MODBUS_URI: &str = "localhost:50054";
/// The app key stamped into request headers (pydoover's CLI uses
/// "pydoover-cli").
pub const DEFAULT_APP_KEY: &str = "doover-cli";

#[derive(Parser, Debug)]
#[command(
    name = "doover",
    version,
    about = "Interact with the Doover device sidecars over gRPC.",
    arg_required_else_help = true
)]
pub struct Cli {
    /// Device agent gRPC URI [default: localhost:50051]
    #[arg(long = "dda-uri", alias = "dda_uri", global = true, env = "DDA_URI")]
    pub dda_uri: Option<String>,

    /// Platform interface gRPC URI [default: localhost:50053]
    #[arg(long = "plt-uri", alias = "plt_uri", global = true, env = "PLT_URI")]
    pub plt_uri: Option<String>,

    /// Modbus interface gRPC URI [default: localhost:50054]
    #[arg(long = "modbus-uri", alias = "modbus_uri", global = true, env = "MODBUS_URI")]
    pub modbus_uri: Option<String>,

    /// App key stamped into request headers [default: doover-cli]
    #[arg(long = "app-key", alias = "app_key", global = true, env = "APP_KEY")]
    pub app_key: Option<String>,

    /// Enable verbose tracing output (to stderr).
    #[arg(long, global = true)]
    pub debug: bool,

    #[command(subcommand)]
    pub section: Section,
}

#[derive(Subcommand, Debug)]
pub enum Section {
    /// Interact with a running Device Agent container.
    #[command(name = "device_agent", alias = "device-agent")]
    DeviceAgent {
        /// Override the device agent URI for this call (pydoover `--uri`).
        #[arg(long)]
        uri: Option<String>,
        #[command(subcommand)]
        cmd: device_agent::DeviceAgentCmd,
    },

    /// Interact with a running Platform Interface container.
    Platform {
        /// Override the platform interface URI for this call (pydoover `--uri`).
        #[arg(long)]
        uri: Option<String>,
        #[command(subcommand)]
        cmd: platform::PlatformCmd,
    },

    /// Interact with a running Modbus Interface container.
    Modbus {
        /// Override the modbus interface URI for this call (pydoover `--uri`).
        #[arg(long)]
        uri: Option<String>,
        #[command(subcommand)]
        cmd: modbus::ModbusCmd,
    },
}

/// Print a JSON result on its own stdout line.
pub fn print_json(value: &Value) {
    println!("{value}");
}

/// Ensure a URI has a scheme for tonic (`host:port` -> `http://host:port`) —
/// the same normalization the runtime applies.
pub fn normalize_uri(uri: &str) -> String {
    if uri.contains("://") {
        uri.to_string()
    } else {
        format!("http://{uri}")
    }
}

/// Resolve a URI: per-section `--uri` beats the global flag, beats the env
/// var (clap), beats the default. Empty strings (e.g. a blank env var) fall
/// through, matching the runtime's `RunOptions`.
fn resolve(section_uri: Option<String>, global: Option<String>, default: &str) -> String {
    section_uri
        .into_iter()
        .chain(global)
        .find(|s| !s.is_empty())
        .unwrap_or_else(|| default.to_string())
}

fn init_tracing(debug: bool) {
    let level = if debug {
        tracing_subscriber::filter::LevelFilter::DEBUG
    } else {
        tracing_subscriber::filter::LevelFilter::ERROR
    };
    tracing_subscriber::fmt()
        .with_max_level(level)
        .with_writer(std::io::stderr)
        .init();
}

#[tokio::main]
async fn main() -> std::process::ExitCode {
    let cli = Cli::parse();
    init_tracing(cli.debug);

    let app_key = resolve(None, cli.app_key, DEFAULT_APP_KEY);
    let result = match cli.section {
        Section::DeviceAgent { uri, cmd } => {
            let uri = resolve(uri, cli.dda_uri, DEFAULT_DDA_URI);
            device_agent::run(&uri, &app_key, cmd).await
        }
        Section::Platform { uri, cmd } => {
            let uri = resolve(uri, cli.plt_uri, DEFAULT_PLT_URI);
            platform::run(&uri, cmd).await
        }
        Section::Modbus { uri, cmd } => {
            let uri = resolve(uri, cli.modbus_uri, DEFAULT_MODBUS_URI);
            modbus::run(&uri, cmd).await
        }
    };

    match result {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            // Also print the root cause — "transport error" alone doesn't
            // say the sidecar is unreachable, the underlying io::Error does.
            let mut root = None;
            let mut source = e.source();
            while let Some(s) = source {
                root = Some(s);
                source = s.source();
            }
            match root {
                Some(cause) => eprintln!("Error: {e} ({cause})"),
                None => eprintln!("Error: {e}"),
            }
            std::process::ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;
    use serde_json::json;

    #[test]
    fn cli_definition_is_valid() {
        // clap's own consistency checks (conflicting names, bad defaults, ...).
        Cli::command().debug_assert();
    }

    fn parse(args: &[&str]) -> Cli {
        Cli::try_parse_from(args).expect("args should parse")
    }

    #[test]
    fn update_aggregate_flags_match_pydoover_naming() {
        let cli = parse(&[
            "doover",
            "device_agent",
            "update_channel_aggregate",
            "my_channel",
            r#"{"level": 42}"#,
            "--replace_data",
            "--save_log",
            "--max_age_secs",
            "5.5",
        ]);
        let Section::DeviceAgent { cmd, .. } = cli.section else {
            panic!("expected device_agent section");
        };
        let device_agent::DeviceAgentCmd::UpdateChannelAggregate {
            channel_name,
            data,
            replace_data,
            clear_attachments,
            save_log,
            max_age_secs,
        } = cmd
        else {
            panic!("expected update_channel_aggregate");
        };
        assert_eq!(channel_name, "my_channel");
        assert_eq!(data, json!({"level": 42}));
        assert!(replace_data && save_log && !clear_attachments);
        assert_eq!(max_age_secs, 5.5);
    }

    #[test]
    fn aggregate_aliases_and_kebab_flags_also_parse() {
        for name in ["get_aggregate", "get-aggregate", "fetch-channel-aggregate"] {
            let cli = parse(&["doover", "device_agent", name, "ch"]);
            let Section::DeviceAgent {
                cmd: device_agent::DeviceAgentCmd::FetchChannelAggregate { channel_name },
                ..
            } = cli.section
            else {
                panic!("alias {name} did not map to fetch_channel_aggregate");
            };
            assert_eq!(channel_name, "ch");
        }
        // kebab-case option aliases stay accepted alongside snake_case.
        parse(&[
            "doover", "device_agent", "update_aggregate", "ch", "{}", "--replace-data",
            "--max-age-secs", "3",
        ]);
    }

    #[test]
    fn invalid_json_payload_is_rejected_at_parse_time() {
        assert!(Cli::try_parse_from([
            "doover", "device_agent", "create_message", "ch", "not json",
        ])
        .is_err());
    }

    #[test]
    fn global_options_apply_after_the_subcommand() {
        let cli = parse(&[
            "doover", "platform", "fetch_ai", "0", "2", "--plt-uri", "10.0.0.1:50053", "--debug",
        ]);
        assert_eq!(cli.plt_uri.as_deref(), Some("10.0.0.1:50053"));
        assert!(cli.debug);
        let Section::Platform { cmd: platform::PlatformCmd::FetchAi { ai }, .. } = cli.section
        else {
            panic!("expected platform fetch_ai");
        };
        assert_eq!(ai, vec![0, 2]);
    }

    #[test]
    fn section_uri_beats_global_then_default() {
        assert_eq!(
            resolve(Some("a:1".into()), Some("b:2".into()), "c:3"),
            "a:1"
        );
        assert_eq!(resolve(None, Some("b:2".into()), "c:3"), "b:2");
        assert_eq!(resolve(None, None, "c:3"), "c:3");
        // Blank (e.g. empty env var) falls through to the default.
        assert_eq!(resolve(None, Some(String::new()), "c:3"), "c:3");
    }

    #[test]
    fn set_do_accepts_scalar_and_list_forms() {
        let cli = parse(&["doover", "platform", "set_do", "[1,2]", "1"]);
        let Section::Platform { cmd: platform::PlatformCmd::SetDo { r#do, value }, .. } =
            cli.section
        else {
            panic!("expected set_do");
        };
        assert_eq!(r#do.0, vec![1, 2]);
        assert_eq!(value.0, vec![true]);
    }

    #[test]
    fn modbus_read_registers_defaults_match_pydoover() {
        let cli = parse(&["doover", "modbus", "read_registers"]);
        let Section::Modbus {
            cmd:
                modbus::ModbusCmd::ReadRegisters {
                    modbus_id,
                    start_address,
                    num_registers,
                    register_type,
                    retries,
                },
            ..
        } = cli.section
        else {
            panic!("expected read_registers");
        };
        assert_eq!(
            (modbus_id, start_address, num_registers, register_type, retries),
            (1, 0, 1, 4, None)
        );
    }

    #[test]
    fn normalize_uri_adds_scheme() {
        assert_eq!(normalize_uri("localhost:50051"), "http://localhost:50051");
        assert_eq!(normalize_uri("http://x:1"), "http://x:1");
    }
}
