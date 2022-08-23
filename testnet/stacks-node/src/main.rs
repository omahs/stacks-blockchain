extern crate libc;
extern crate rand;
extern crate serde;

#[macro_use]
extern crate lazy_static;
#[macro_use]
extern crate serde_derive;
#[macro_use]
extern crate serde_json;
#[macro_use]
extern crate stacks_common;

extern crate stacks;

#[allow(unused_imports)]
#[macro_use(o, slog_log, slog_trace, slog_debug, slog_info, slog_warn, slog_error)]
extern crate slog;

pub use stacks::util;
use stacks::util::hash::hex_bytes;

pub mod monitoring;

pub mod burnchains;
pub mod config;
pub mod event_dispatcher;
pub mod genesis_data;
pub mod keychain;
pub mod neon_node;
pub mod node;
pub mod operations;
pub mod run_loop;
pub mod syncctl;
pub mod tenure;

pub use self::burnchains::{
    BitcoinRegtestController, BurnchainController, BurnchainTip, MocknetController,
};
pub use self::config::{Config, ConfigFile};
pub use self::event_dispatcher::EventDispatcher;
pub use self::keychain::Keychain;
pub use self::node::{ChainTip, Node};
pub use self::run_loop::{helium, neon};
pub use self::tenure::Tenure;

use pico_args::Arguments;
use std::{env, thread};

use std::convert::TryInto;
use std::panic;
use std::process;

use backtrace::Backtrace;

fn main() {
    panic::set_hook(Box::new(|panic_info| {
        error!("Process abort due to thread panic: {}", panic_info);
        let bt = Backtrace::new();
        error!("Panic backtrace: {:?}", &bt);

        // force a core dump
        #[cfg(unix)]
        {
            let pid = process::id();
            eprintln!("Dumping core for pid {}", std::process::id());

            use libc::kill;
            use libc::SIGQUIT;

            // *should* trigger a core dump, if you run `ulimit -c unlimited` first!
            unsafe { kill(pid.try_into().unwrap(), SIGQUIT) };
        }

        // just in case
        process::exit(1);
    }));

    let mut args = Arguments::from_env();
    let subcommand = args.subcommand().unwrap().unwrap_or_default();

    info!("{}", version());

    let mine_start: Option<u64> = args
        .opt_value_from_str("--mine-at-height")
        .expect("Failed to parse --mine-at-height argument");

    if let Some(mine_start) = mine_start {
        info!(
            "Will begin mining once Stacks chain has synced to height >= {}",
            mine_start
        );
    }

    let mut start_config_path: Option<String> = None;

    let config_file = match subcommand.as_str() {
        "mocknet" => {
            args.finish().unwrap();
            ConfigFile::mocknet()
        }
        "helium" => {
            args.finish().unwrap();
            ConfigFile::helium()
        }
        "testnet" => {
            args.finish().unwrap();
            ConfigFile::xenon()
        }
        "mainnet" => {
            args.finish().unwrap();
            ConfigFile::mainnet()
        }
        "check-config" => {
            let config_path: String = args.value_from_str("--config").unwrap();
            args.finish().unwrap();
            info!("Loading config at path {}", config_path);
            let config_file = match ConfigFile::from_path(&config_path) {
                Ok(config_file) => config_file,
                Err(e) => {
                    warn!("Invalid config file: {}", e);
                    process::exit(1);
                }
            };
            let conf = match Config::from_config_file(config_file) {
                Ok(conf) => {
                    info!("Loaded config!");
                    process::exit(0);
                }
                Err(e) => {
                    warn!("Invalid config: {}", e);
                    process::exit(1);
                }
            };
        }
        "start" => {
            let config_path: String = args.value_from_str("--config").unwrap();
            start_config_path = Some(config_path.clone());
            args.finish().unwrap();
            info!("Loading config at path {}", config_path);
            match ConfigFile::from_path(&config_path) {
                Ok(config_file) => config_file,
                Err(e) => {
                    warn!("Invalid config file: {}", e);
                    process::exit(1);
                }
            }
        }
        "version" => {
            println!("{}", &version());
            return;
        }
        "key-for-seed" => {
            let seed = {
                let config_path: Option<String> = args.opt_value_from_str("--config").unwrap();
                if let Some(config_path) = config_path {
                    let conf =
                        Config::from_config_file(ConfigFile::from_path(&config_path).unwrap())
                            .unwrap();
                    args.finish().unwrap();
                    conf.node.seed
                } else {
                    let free_args = args.free().unwrap();
                    let seed_hex = free_args
                        .first()
                        .expect("`wif-for-seed` must be passed either a config file via the `--config` flag or a hex seed string");
                    hex_bytes(seed_hex).expect("Seed should be a hex encoded string")
                }
            };
            let keychain = Keychain::default(seed);
            println!(
                "Hex formatted secret key: {}",
                keychain.generate_op_signer().get_sk_as_hex()
            );
            println!(
                "WIF formatted secret key: {}",
                keychain.generate_op_signer().get_sk_as_wif()
            );
            return;
        }
        _ => {
            print_help();
            return;
        }
    };

    let config = match Config::from_config_file(config_file.clone()) {
        Ok(config) => config,
        Err(e) => {
            error!("Invalid config: {}", e);
            process::exit(1);
        }
    };

    debug!("node configuration {:?}", &config.node);
    debug!("burnchain configuration {:?}", &config.burnchain);
    debug!("connection configuration {:?}", &config.connection_options);

    let num_round: u64 = 0; // Infinite number of rounds

    if config.burnchain.mode == "helium" || config.burnchain.mode == "mocknet" {
        let mut run_loop = helium::RunLoop::new(config);
        if let Err(e) = run_loop.start(num_round) {
            warn!("Helium runloop exited: {}", e);
            return;
        }
    } else if config.burnchain.mode == "neon"
        || config.burnchain.mode == "xenon"
        || config.burnchain.mode == "krypton"
        || config.burnchain.mode == "mainnet"
    {
        let mut run_loop = neon::RunLoop::new(config);
        run_loop.start(None, mine_start.unwrap_or(0));

        if let Some(config_path) = start_config_path {
            let dyn_config = run_loop.dyn_config().clone();
            let mut signals =
                signal_hook::iterator::Signals::new(&[signal_hook::consts::SIGUSR1]).unwrap();
            thread::Builder::new()
                .name("config-loader".to_string())
                .spawn(move || {
                    for sig in signals.forever() {
                        let config_file = ConfigFile::from_path(&config_path);
                        match config_file {
                            Err(e) => {
                                error!("Failed to load config file: {}", e);
                            }
                            Ok(config_file) => {
                                match Config::from_config_file(config_file.clone()) {
                                    Err(e) => {
                                        error!("Failed to load config: {}", e);
                                    }
                                    Ok(config) => {
                                        info!("Loaded config at {}", config_path);
                                        let prior_config = dyn_config.get();
                                        visit_diff(
                                            &prior_config,
                                            &config,
                                            |s| &s.burnchain.burn_fee_cap,
                                            "burnchain.burn_fee_cap",
                                        );
                                        visit_diff(
                                            &prior_config,
                                            &config,
                                            |s| &s.burnchain.satoshis_per_byte,
                                            "burnchain.satoshis_per_byte",
                                        );
                                        visit_diff(
                                            &prior_config,
                                            &config,
                                            |s| &s.burnchain.rbf_fee_increment,
                                            "burnchain.rbf_fee_increment",
                                        );
                                        visit_diff(
                                            &prior_config,
                                            &config,
                                            |s| &s.burnchain.max_rbf,
                                            "burnchain.max_rbf",
                                        );
                                        visit_diff(
                                            &prior_config,
                                            &config,
                                            |s| &s.node.microblock_frequency,
                                            "node.microblock_frequency",
                                        );
                                        visit_diff(
                                            &prior_config,
                                            &config,
                                            |s| &s.node.wait_time_for_microblocks,
                                            "node.wait_time_for_microblocks",
                                        );
                                        dyn_config.replace(&config);
                                    }
                                }
                            }
                        }
                    }
                })
                .unwrap();
        }
    } else {
        println!("Burnchain mode '{}' not supported", config.burnchain.mode);
    }
}

fn visit_diff<Struct, Field: PartialEq + std::fmt::Display>(
    struct1: &Struct,
    struct2: &Struct,
    extractor: fn(&Struct) -> &Field,
    field_name: &str,
) {
    let field1 = extractor(struct1);
    let field2 = extractor(struct2);
    if field1 != field2 {
        info!("{} changed from {} to {}", field_name, field1, field2)
    }
}

fn version() -> String {
    stacks::version_string(
        "stacks-node",
        option_env!("STACKS_NODE_VERSION")
            .or(option_env!("CARGO_PKG_VERSION"))
            .unwrap_or("0.0.0.0"),
    )
}

fn print_help() {
    let argv: Vec<_> = env::args().collect();

    eprintln!(
        "\
{} <SUBCOMMAND>
Run a stacks-node.

USAGE:
stacks-node <SUBCOMMAND>

SUBCOMMANDS:

mainnet\t\tStart a node that will join and stream blocks from the public mainnet.

mocknet\t\tStart a node based on a fast local setup emulating a burnchain. Ideal for smart contract development. 

helium\t\tStart a node based on a local setup relying on a local instance of bitcoind.
\t\tThe following bitcoin.conf is expected:
\t\t  chain=regtest
\t\t  disablewallet=0
\t\t  txindex=1
\t\t  server=1
\t\t  rpcuser=helium
\t\t  rpcpassword=helium

testnet\t\tStart a node that will join and stream blocks from the public testnet, relying on Bitcoin Testnet.

start\t\tStart a node with a config of your own. Can be used for joining a network, starting new chain, etc.
\t\tArguments:
\t\t  --config: path of the config (such as https://github.com/blockstack/stacks-blockchain/blob/master/testnet/stacks-node/conf/testnet-follower-conf.toml).
\t\tExample:
\t\t  stacks-node start --config=/path/to/config.toml

check-config\t\tValidates the config file without starting up the node. Uses same arguments as start subcommand.

version\t\tDisplay information about the current version and our release cycle.

key-for-seed\tOutput the associated secret key for a burnchain signer created with a given seed.
\t\tCan be passed a config file for the seed via the `--config=<file>` option *or* by supplying the hex seed on
\t\tthe command line directly.

help\t\tDisplay this help.

OPTIONAL ARGUMENTS:

\t\t--mine-at-height=<height>: optional argument for a miner to not attempt mining until Stacks block has sync'ed to <height>

", argv[0]);
}

#[cfg(test)]
pub mod tests;
