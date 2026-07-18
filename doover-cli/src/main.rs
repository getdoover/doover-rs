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
//!
//! Anything pydoover's argparse accepted still parses here, so a caller can
//! swap binaries without editing its command lines. Where an argument no longer
//! selects anything — because the Rust clients dial on demand, take their
//! service name from the compiled-in proto, and always emit JSON — it is
//! accepted, hidden from `--help`, and ignored (see `ConnCompat`,
//! `modbus::BusCompat` and `device_agent::FilesCompat`).
//!
//! The exception is pydoover's `--shell`, which is rejected: it existed only to
//! amortise Python's ~0.8s interpreter startup across calls, which Rust doesn't
//! pay. Clients that probe for it (`doover-cockpit`'s `CockpitShellTransport`)
//! treat the rejection as "unsupported" and fall back to one-off spawns, which
//! against this binary is the faster path anyway.

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
/// The app key stamped into request headers. Matches the key pydoover's CLI
/// hardcoded — it identifies the writer to the agent and the cloud, so keeping
/// it means anything filtering on `app_id` doesn't care which binary ran.
pub const DEFAULT_APP_KEY: &str = "pydoover-cli";

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

    /// App key stamped into request headers [default: pydoover-cli]
    #[arg(long = "app-key", alias = "app_key", global = true, env = "APP_KEY")]
    pub app_key: Option<String>,

    /// Enable verbose tracing output (to stderr).
    #[arg(long, global = true)]
    pub debug: bool,

    /// Accepted for pydoover compatibility: results are always JSON.
    #[arg(long, global = true, hide = true)]
    pub json: bool,

    /// Accepted for pydoover compatibility; see --debug for verbose output.
    #[arg(
        long = "enable-traceback",
        alias = "enable_traceback",
        global = true,
        hide = true
    )]
    pub enable_traceback: bool,

    #[command(flatten)]
    pub conn_compat: ConnCompat,

    #[command(subcommand)]
    pub section: Section,
}

/// gRPC connection-tuning arguments pydoover exposed per section, derived from
/// its interface classes' `__init__` signatures. The Rust clients dial on
/// demand and take their service name from the compiled-in proto, so none of
/// these select anything; accepted and ignored so existing scripts keep parsing.
#[derive(clap::Args, Debug)]
pub struct ConnCompat {
    #[arg(long = "service_name", alias = "service-name", global = true, hide = true)]
    pub service_name: Option<String>,
    #[arg(long = "dda_timeout", alias = "dda-timeout", global = true, hide = true)]
    pub dda_timeout: Option<u64>,
    #[arg(long = "max_conn_attempts", alias = "max-conn-attempts", global = true, hide = true)]
    pub max_conn_attempts: Option<u32>,
    #[arg(
        long = "time_between_connection_attempts",
        alias = "time-between-connection-attempts",
        global = true,
        hide = true
    )]
    pub time_between_connection_attempts: Option<u64>,
    #[arg(long = "timeout", global = true, hide = true)]
    pub timeout: Option<u64>,
    #[arg(long = "config", global = true, hide = true)]
    pub config: Option<String>,
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

    fn parse_section(args: &[&str]) -> Section {
        parse(args).section
    }

    #[test]
    fn update_aggregate_flags_match_pydoover_naming() {
        let section = parse_section(&[
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
        let Section::DeviceAgent { cmd, .. } = section else {
            panic!("expected device_agent section");
        };
        let device_agent::DeviceAgentCmd::UpdateChannelAggregate {
            channel_name,
            data,
            replace_data,
            clear_attachments,
            save_log,
            max_age_secs,
            return_aggregate,
            ..
        } = cmd
        else {
            panic!("expected update_channel_aggregate");
        };
        assert_eq!(channel_name, "my_channel");
        assert_eq!(data, json!({"level": 42}));
        assert!(replace_data && save_log && !clear_attachments);
        assert_eq!(max_age_secs, 5.5);
        // pydoover's return_aggregate defaults True; the flag turns it off.
        assert!(return_aggregate);
    }

    #[test]
    fn aggregate_aliases_and_kebab_flags_also_parse() {
        for name in ["get_aggregate", "get-aggregate", "fetch-channel-aggregate"] {
            let Section::DeviceAgent {
                cmd: device_agent::DeviceAgentCmd::FetchChannelAggregate { channel_name },
                ..
            } = parse_section(&["doover", "device_agent", name, "ch"])
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
        let Section::Platform { cmd: platform::PlatformCmd::SetDo { r#do, value }, .. } =
            parse_section(&["doover", "platform", "set_do", "[1,2]", "1"])
        else {
            panic!("expected set_do");
        };
        assert_eq!(r#do.0, vec![1, 2]);
        assert_eq!(value.0, vec![true]);
    }

    #[test]
    fn modbus_read_registers_defaults_match_pydoover() {
        let Section::Modbus {
            cmd:
                modbus::ModbusCmd::ReadRegisters {
                    modbus_id,
                    start_address,
                    num_registers,
                    register_type,
                    retries,
                    ..
                },
            ..
        } = parse_section(&["doover", "modbus", "read_registers"])
        else {
            panic!("expected read_registers");
        };
        assert_eq!(
            (modbus_id, start_address, num_registers, register_type, retries),
            (1, 0, 1, 4, None)
        );
    }

    #[test]
    fn fetch_message_attachment_takes_url_positionally() {
        // pydoover maps defaulted params to flags and the rest to positionals,
        // so `url` is positional there and must stay positional here.
        let Section::DeviceAgent {
            cmd: device_agent::DeviceAgentCmd::FetchMessageAttachment { url, output, force, base64 },
            ..
        } = parse_section(&[
            "doover",
            "device_agent",
            "fetch_message_attachment",
            "https://x/a.bin",
            "--output",
            "/tmp/a.bin",
            "--force",
        ])
        else {
            panic!("expected fetch_message_attachment");
        };
        assert_eq!(url, "https://x/a.bin");
        assert_eq!(output.as_deref(), Some(std::path::Path::new("/tmp/a.bin")));
        assert!(force && !base64);
    }

    #[test]
    fn attachment_output_and_base64_are_mutually_exclusive() {
        assert!(Cli::try_parse_from([
            "doover",
            "device_agent",
            "fetch_message_attachment",
            "https://x/a.bin",
            "--output",
            "/tmp/a.bin",
            "--base64",
        ])
        .is_err());
    }

    /// pydoover put `--json` and `--enable-traceback` on every subcommand, and
    /// callers (e.g. cockpit's data client) pass `--json` on every call.
    #[test]
    fn pydoover_output_flags_are_still_accepted() {
        let cli = parse(&[
            "doover",
            "device_agent",
            "fetch_channel_aggregate",
            "ui_state",
            "--json",
            "--enable-traceback",
        ]);
        assert!(cli.json && cli.enable_traceback);
        parse(&["doover", "platform", "fetch_ai", "0", "--enable_traceback"]);
    }

    /// The connection-tuning args pydoover derived from its interface classes'
    /// `__init__` signatures. They tune nothing here, but must not error.
    #[test]
    fn pydoover_connection_flags_are_still_accepted() {
        parse(&[
            "doover",
            "device_agent",
            "get_is_dda_online",
            "--service_name",
            "doover.DeviceAgent",
            "--dda_timeout",
            "7",
            "--max_conn_attempts",
            "5",
            "--time_between_connection_attempts",
            "10",
        ]);
        parse(&["doover", "modbus", "read_registers", "--timeout", "7", "--config", "{}"]);
    }

    #[test]
    fn update_aggregate_return_flag_disables_the_echo() {
        let Section::DeviceAgent {
            cmd: device_agent::DeviceAgentCmd::UpdateChannelAggregate { return_aggregate, .. },
            ..
        } = parse_section(&[
            "doover",
            "device_agent",
            "update_aggregate",
            "ch",
            "{}",
            "--return_aggregate",
        ])
        else {
            panic!("expected update_channel_aggregate");
        };
        // pydoover's store_false: passing the flag turns the echo OFF.
        assert!(!return_aggregate);
    }

    /// pydoover's `--files` mangled its input and could never carry an
    /// attachment, but scripts that pass it must still parse.
    #[test]
    fn files_flag_is_accepted_and_ignored() {
        for cmd in ["update_aggregate", "create_message", "update_message"] {
            let args: Vec<&str> = match cmd {
                "update_message" => {
                    vec!["doover", "device_agent", cmd, "ch", "1", "{}", "--files", "[]"]
                }
                _ => vec!["doover", "device_agent", cmd, "ch", "{}", "--files", "[]"],
            };
            parse(&args);
        }
    }

    #[test]
    fn write_registers_accepts_values_positionally_and_as_a_flag() {
        for args in [
            vec!["doover", "modbus", "write_registers", "[1,2]"],
            // pydoover defaulted `values`, so it was a flag there.
            vec!["doover", "modbus", "write_registers", "--values", "[1,2]"],
        ] {
            let Section::Modbus {
                cmd: modbus::ModbusCmd::WriteRegisters { values, values_flag, .. },
                ..
            } = parse_section(&args)
            else {
                panic!("expected write_registers");
            };
            assert_eq!(values.or(values_flag).map(|v| v.0), Some(vec![1, 2]));
        }
        // The two spellings are one argument — giving both is a mistake.
        assert!(Cli::try_parse_from([
            "doover", "modbus", "write_registers", "[1]", "--values", "[2]",
        ])
        .is_err());
        assert!(Cli::try_parse_from(["doover", "modbus", "write_registers"]).is_err());
    }

    #[test]
    fn register_bus_selection_flags_are_accepted_and_ignored() {
        parse(&[
            "doover",
            "modbus",
            "read_registers",
            "--bus_id",
            "default",
            "--configure_bus",
        ]);
        parse(&["doover", "modbus", "write_registers", "[1]", "--bus-id", "my_bus"]);
    }

    /// pydoover typed `set_do`'s value `int | list[int]` and passed it through
    /// untouched, so any integer was accepted — not just 0/1.
    #[test]
    fn set_do_accepts_arbitrary_ints_as_pydoover_did() {
        let Section::Platform { cmd: platform::PlatformCmd::SetDo { value, .. }, .. } =
            parse_section(&["doover", "platform", "set_do", "0", "7"])
        else {
            panic!("expected set_do");
        };
        assert_eq!(value.0, vec![true]);
    }

    #[test]
    fn normalize_uri_adds_scheme() {
        assert_eq!(normalize_uri("localhost:50051"), "http://localhost:50051");
        assert_eq!(normalize_uri("http://x:1"), "http://x:1");
    }
}
