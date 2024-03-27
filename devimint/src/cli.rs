use std::ffi;
use std::fmt::Write;
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use clap::{Parser, Subcommand};
use fedimint_core::task::TaskGroup;
use fedimint_core::util::write_overwrite_async;
use fedimint_logging::LOG_DEVIMINT;
use rand::distributions::Alphanumeric;
use rand::Rng as _;
use tokio::fs;
use tokio::net::TcpStream;
use tokio::time::Instant;
use tracing::{debug, error, info, trace, warn};

use crate::devfed::DevJitFed;
use crate::federation::Fedimintd;
use crate::util::{poll, poll_with_timeout, ProcessManager};
use crate::{external_daemons, vars, ExternalDaemons};

fn random_test_dir_suffix() -> String {
    rand::thread_rng()
        .sample_iter(&Alphanumeric)
        .filter(|c| c.is_ascii_digit())
        .take(3)
        .map(char::from)
        .collect::<String>()
}

#[derive(Parser, Clone)]
pub struct CommonArgs {
    #[clap(short = 'd', long, env = "FM_TEST_DIR")]
    pub test_dir: Option<PathBuf>,
    #[clap(short = 'n', long, env = "FM_FED_SIZE", default_value = "4")]
    pub fed_size: usize,

    #[clap(long, env = "FM_LINK_TEST_DIR")]
    /// Create a link to the test dir under this path
    pub link_test_dir: Option<PathBuf>,

    #[clap(long, default_value_t = random_test_dir_suffix())]
    pub link_test_dir_suffix: String,

    /// Run degraded federation with FM_OFFLINE_NODES shutdown
    #[clap(long, env = "FM_OFFLINE_NODES", default_value = "0")]
    pub offline_nodes: usize,
}

impl CommonArgs {
    pub fn mk_test_dir(&self) -> Result<PathBuf> {
        let path = self.test_dir();

        std::fs::create_dir_all(&path)
            .with_context(|| format!("Creating tmp directory {}", path.display()))?;

        Ok(path)
    }

    pub fn test_dir(&self) -> PathBuf {
        self.test_dir.clone().unwrap_or_else(|| {
            std::env::temp_dir().join(format!(
                "devimint-{}-{}",
                std::process::id(),
                self.link_test_dir_suffix
            ))
        })
    }
}

#[derive(Subcommand)]
pub enum Cmd {
    /// Spins up bitcoind, cln, lnd, electrs, esplora, and opens a channel
    /// between the two lightning nodes
    ExternalDaemons {
        #[arg(long, trailing_var_arg = true, allow_hyphen_values = true, num_args=1..)]
        exec: Option<Vec<ffi::OsString>>,
    },
    /// Spins up bitcoind, cln w/ gateway, lnd w/ gateway, a faucet, electrs,
    /// esplora, and a federation sized from FM_FED_SIZE it opens LN channel
    /// between the two nodes. it connects the gateways to the federation.
    /// it finally switches to use the CLN gateway using the fedimint-cli
    DevFed {
        #[arg(long, trailing_var_arg = true, allow_hyphen_values = true, num_args=1..)]
        exec: Option<Vec<ffi::OsString>>,
    },
    /// Runs bitcoind, spins up FM_FED_SIZE worth of fedimints
    RunUi,
    /// Rpc commands to the long running devimint instance. Could be entry point
    /// for devimint as a cli
    #[clap(flatten)]
    Rpc(RpcCmd),
}

#[derive(Subcommand)]
pub enum RpcCmd {
    Wait,
    Env,
}

pub async fn setup(arg: CommonArgs) -> Result<(ProcessManager, TaskGroup)> {
    let globals = vars::Global::new(&arg.mk_test_dir()?, arg.fed_size, arg.offline_nodes).await?;

    let log_file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .append(true)
        .open(globals.FM_LOGS_DIR.join("devimint.log"))
        .await?
        .into_std()
        .await;

    fedimint_logging::TracingSetup::default()
        .with_file(Some(log_file))
        // jsonrpsee is expected to fail during startup
        .with_directive("jsonrpsee-client=off")
        .init()?;

    info!(target: LOG_DEVIMINT, path=%globals.FM_DATA_DIR.display() , "Setting up test dir");
    if let Some(link_test_dir) = arg.link_test_dir.as_ref() {
        update_test_dir_link(link_test_dir, &arg.test_dir()).await?;
    }

    let mut env_string = String::new();
    for (var, value) in globals.vars() {
        debug!(var, value, "Env variable set");
        writeln!(env_string, r#"export {var}="{value}""#)?; // hope that value doesn't contain a "
        std::env::set_var(var, value);
    }
    write_overwrite_async(globals.FM_TEST_DIR.join("env"), env_string).await?;
    let process_mgr = ProcessManager::new(globals);
    let task_group = TaskGroup::new();
    task_group.install_kill_handler();
    Ok((process_mgr, task_group))
}

pub async fn update_test_dir_link(
    link_test_dir: &Path,
    test_dir: &Path,
) -> Result<(), anyhow::Error> {
    let make_link = if let Ok(existing) = fs::read_link(link_test_dir).await {
        if existing != test_dir {
            debug!(
                old = %existing.display(),
                new = %test_dir.display(),
                link = %link_test_dir.display(),
                "Updating exinst test dir link"
            );

            fs::remove_file(link_test_dir).await?;
            true
        } else {
            false
        }
    } else {
        true
    };
    if make_link {
        debug!(src = %test_dir.display(), dst = %link_test_dir.display(), "Linking test dir");
        fs::symlink(&test_dir, link_test_dir).await?;
    }
    Ok(())
}

pub async fn cleanup_on_exit<T>(
    main_process: impl futures::Future<Output = Result<T>>,
    task_group: TaskGroup,
) -> Result<()> {
    match task_group
        .make_handle()
        .cancel_on_shutdown(main_process)
        .await
    {
        Err(_) => {
            info!("Received shutdown signal before finishing main process, exiting early");
            Ok(())
        }
        Ok(Ok(v)) => {
            debug!(target: LOG_DEVIMINT, "Main process finished successfully, will wait for shutdown signal");
            task_group.make_handle().make_shutdown_rx().await.await;
            debug!(target: LOG_DEVIMINT, "Received shutdown signal, shutting down");
            drop(v); // execute destructors
            Ok(())
        }
        Ok(Err(err)) => {
            warn!(target: LOG_DEVIMINT, %err, "Main process failed, will shutdown");
            Err(err)
        }
    }
}

pub async fn write_ready_file<T>(global: &vars::Global, result: Result<T>) -> Result<T> {
    let ready_file = &global.FM_READY_FILE;
    match result {
        Ok(_) => write_overwrite_async(ready_file, "READY").await?,
        Err(_) => write_overwrite_async(ready_file, "ERROR").await?,
    }
    result
}

pub async fn handle_command(cmd: Cmd, common_args: CommonArgs) -> Result<()> {
    match cmd {
        Cmd::ExternalDaemons { exec } => {
            let (process_mgr, task_group) = setup(common_args).await?;
            let _daemons =
                write_ready_file(&process_mgr.globals, external_daemons(&process_mgr).await)
                    .await?;
            if let Some(exec) = exec {
                exec_user_command(exec).await?;
                task_group.shutdown();
            }
            task_group.make_handle().make_shutdown_rx().await.await;
        }
        Cmd::DevFed { exec } => {
            trace!(target: LOG_DEVIMINT, "Starting dev fed");
            let start_time = Instant::now();
            let (process_mgr, task_group) = setup(common_args).await?;
            let main = {
                let task_group = task_group.clone();
                async move {
                    let dev_fed = DevJitFed::new(&process_mgr)?;

                    let pegin_start_time = Instant::now();
                    info!(target: LOG_DEVIMINT, "Peging in client and gateways");

                    let gw_pegin_amount = 20_000;
                    let client_pegin_amount = 10_000;
                    let (_, _, _) = tokio::try_join!(
                        async {
                            let (address, operation_id) = dev_fed
                                .client_registered()
                                .await?
                                .get_deposit_addr()
                                .await?;
                            dev_fed
                                .bitcoind()
                                .await?
                                .send_to(address, client_pegin_amount)
                                .await?;
                            dev_fed.bitcoind().await?.mine_blocks_no_wait(11).await?;
                            dev_fed
                                .client_registered()
                                .await?
                                .await_deposit(&operation_id)
                                .await
                        },
                        async {
                            let pegin_addr = dev_fed
                                .gw_cln_registered()
                                .await?
                                .get_pegin_addr(
                                    &dev_fed.fed().await?.calculate_federation_id().await,
                                )
                                .await?;
                            dev_fed
                                .bitcoind()
                                .await?
                                .send_to(pegin_addr, gw_pegin_amount)
                                .await?;
                            dev_fed.bitcoind().await?.mine_blocks_no_wait(11).await
                        },
                        async {
                            let pegin_addr = dev_fed
                                .gw_lnd_registered()
                                .await?
                                .get_pegin_addr(
                                    &dev_fed.fed().await?.calculate_federation_id().await,
                                )
                                .await?;
                            dev_fed
                                .bitcoind()
                                .await?
                                .send_to(pegin_addr, gw_pegin_amount)
                                .await?;
                            dev_fed.bitcoind().await?.mine_blocks_no_wait(11).await
                        },
                    )?;

                    info!(target: LOG_DEVIMINT,
                        elapsed_ms = %pegin_start_time.elapsed().as_millis(),
                        "Pegins completed");
                    std::env::set_var("FM_INVITE_CODE", dev_fed.fed().await?.invite_code()?);

                    dev_fed.finalize(&process_mgr).await?;

                    let daemons = write_ready_file(&process_mgr.globals, Ok(dev_fed)).await?;

                    if let Some(exec) = exec {
                        debug!(target: LOG_DEVIMINT, elapsed_ms = %start_time.elapsed().as_millis(), "Starting exec command");
                        exec_user_command(exec).await?;
                        task_group.shutdown();
                    }

                    Ok::<_, anyhow::Error>(daemons)
                }
            };
            cleanup_on_exit(main, task_group).await?;
        }
        Cmd::Rpc(rpc_cmd) => rpc_command(rpc_cmd, common_args).await?,
        Cmd::RunUi => {
            let (process_mgr, task_group) = setup(common_args).await?;
            let main = async move {
                let result = run_ui(&process_mgr).await;
                let daemons = write_ready_file(&process_mgr.globals, result).await?;
                Ok::<_, anyhow::Error>(daemons)
            };
            cleanup_on_exit(main, task_group).await?;
        }
    }
    Ok(())
}

pub async fn exec_user_command(exec: Vec<ffi::OsString>) -> Result<(), anyhow::Error> {
    let cmd_str = exec
        .join(ffi::OsStr::new(" "))
        .to_string_lossy()
        .to_string();
    debug!(target: LOG_DEVIMINT, cmd = %cmd_str, "Executing user command");
    if !tokio::process::Command::new(&exec[0])
        .args(&exec[1..])
        .kill_on_drop(true)
        .status()
        .await
        .with_context(|| format!("Executing user command failed: {cmd_str}"))?
        .success()
    {
        error!(cmd = %cmd_str, "User command failed");
        return Err(anyhow!("User command failed: {cmd_str}"));
    }
    Ok(())
}

pub async fn rpc_command(rpc: RpcCmd, common: CommonArgs) -> Result<()> {
    fedimint_logging::TracingSetup::default().init()?;
    match rpc {
        RpcCmd::Env => {
            let env_file = common.test_dir().join("env");
            poll("env file", || async {
                if fs::try_exists(&env_file)
                    .await
                    .context("env file")
                    .map_err(ControlFlow::Continue)?
                {
                    Ok(())
                } else {
                    Err(ControlFlow::Continue(anyhow!("env file not found")))
                }
            })
            .await?;
            let env = fs::read_to_string(&env_file).await?;
            print!("{env}");
            Ok(())
        }
        RpcCmd::Wait => {
            let ready_file = common.test_dir().join("ready");
            poll_with_timeout("ready file", Duration::from_secs(60), || async {
                if fs::try_exists(&ready_file)
                    .await
                    .context("ready file")
                    .map_err(ControlFlow::Continue)?
                {
                    Ok(())
                } else {
                    Err(ControlFlow::Continue(anyhow!("ready file not found")))
                }
            })
            .await?;
            let env = fs::read_to_string(&ready_file).await?;
            print!("{env}");

            // Append invite code to devimint env
            let test_dir = &common.test_dir();
            let env_file = test_dir.join("env");
            let invite_file = test_dir.join("cfg/invite-code");
            if fs::try_exists(&env_file).await.ok().unwrap_or(false)
                && fs::try_exists(&invite_file).await.ok().unwrap_or(false)
            {
                let invite = fs::read_to_string(&invite_file).await?;
                let mut env_string = fs::read_to_string(&env_file).await?;
                writeln!(env_string, r#"export FM_INVITE_CODE="{invite}""#)?;
                std::env::set_var("FM_INVITE_CODE", invite);
                write_overwrite_async(env_file, env_string).await?;
            }

            Ok(())
        }
    }
}

async fn run_ui(process_mgr: &ProcessManager) -> Result<(Vec<Fedimintd>, ExternalDaemons)> {
    let externals = external_daemons(process_mgr).await?;
    let fed_size = process_mgr.globals.FM_FED_SIZE;
    let fedimintds = futures::future::try_join_all((0..fed_size).map(|peer| {
        let bitcoind = externals.bitcoind.clone();
        async move {
            let peer_port = 10000 + 8137 + peer * 2;
            let api_port = peer_port + 1;
            let metrics_port = 3510 + peer;

            let vars = vars::Fedimintd {
                FM_BIND_P2P: format!("127.0.0.1:{peer_port}"),
                FM_P2P_URL: format!("fedimint://127.0.0.1:{peer_port}"),
                FM_BIND_API: format!("127.0.0.1:{api_port}"),
                FM_API_URL: format!("ws://127.0.0.1:{api_port}"),
                FM_DATA_DIR: process_mgr
                    .globals
                    .FM_DATA_DIR
                    .join(format!("fedimintd-{peer}")),
                FM_BIND_METRICS_API: format!("127.0.0.1:{metrics_port}"),
            };
            let fm = Fedimintd::new(process_mgr, bitcoind.clone(), peer, &vars).await?;
            let server_addr = &vars.FM_BIND_API;

            poll("waiting for api startup", || async {
                TcpStream::connect(server_addr)
                    .await
                    .context("connect to api")
                    .map_err(ControlFlow::Continue)
            })
            .await?;

            anyhow::Ok(fm)
        }
    }))
    .await?;

    Ok((fedimintds, externals))
}
