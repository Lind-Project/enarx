// SPDX-License-Identifier: Apache-2.0

//! The Enarx Wasm runtime and all related functionality

mod identity;
mod io;
//mod net;

use self::io::null::Null;

use super::{Package, Workload};

use anyhow::{anyhow, bail, Context, Result};
use cap_std::fs::Dir;
use enarx_config::{Config, File};
use rawposix::safeposix::dispatcher::lind_syscall_api;
use std::sync::{atomic::AtomicU64, Arc};
use wasi_common::sync::WasiCtxBuilder;
use wasmtime::{
    AsContextMut, Engine, Func, InstantiateType, Linker, Module, Store, StoreLimits, Val, ValType,
};
use wasmtime_lind_common::LindCommonCtx;
use wasmtime_lind_multi_process::{LindCtx, LindHost, CAGE_START_ID, THREAD_START_ID};
use wasmtime_lind_utils::{lind_syscall_numbers::EXIT_SYSCALL, LindCageManager};
use wasmtime_wasi_threads::WasiThreadsCtx;
use wiggle::tracing::trace_span;

/// The base directory to preopen during the Wasm module linking stage,
/// used to grant ambient directory access (via capability-based I/O)
/// before instantiating the module.
static HOME_DIR_PATH: &str = "/home";

/// The HostCtx host structure stores all relevant execution context objects
/// `preview1_ctx`: the WASI preview1 context (used by glibc and POSIX emulation);
/// `lind_common_ctx`: the context responsible for per-cage state management (e.g., signal handling, cage ID tracking);
/// `lind_fork_ctx`: the multi-process management structure, encapsulating fork/exec state;
/// `wasi_threads`: which manages WASI thread-related capabilities.
#[derive(Default, Clone)]
struct HostCtx {
    preview1_ctx: Option<wasi_common::WasiCtx>,
    wasi_threads: Option<Arc<WasiThreadsCtx<HostCtx>>>,
    lind_common_ctx: Option<LindCommonCtx>,
    lind_fork_ctx: Option<LindCtx<HostCtx, Option<enarx_config::Config>>>,
}

/// This implementation allows HostCtx to be used where a mutable reference to `wasi_common::WasiCtx`
/// is expected.
impl AsMut<wasi_common::WasiCtx> for HostCtx {
    fn as_mut(&mut self) -> &mut wasi_common::WasiCtx {
        self.preview1_ctx
            .as_mut()
            .expect("preview1_ctx must be initialized before use")
    }
}

impl HostCtx {
    /// Performs a partial deep clone of the host context. It explicitly forks the WASI preview1
    /// context(`preview1_ctx`), the lind multi-process context (`lind_fork_ctx`), and the lind common
    /// context (`lind_common_ctx`). Other parts of the context, such as `wasi_threads`, are shared
    /// between forks since they are not required to be process-isolated.
    pub fn fork(&self) -> Self {
        // we want to do a real fork for wasi_preview1 context since glibc uses the environment variable
        // related interface here
        let forked_preview1_ctx = match &self.preview1_ctx {
            Some(ctx) => Some(ctx.fork()),
            None => None,
        };

        // and we also want to fork the lind-common context and lind-multi-process context
        let forked_lind_fork_ctx = match &self.lind_fork_ctx {
            Some(ctx) => Some(ctx.fork()),
            None => None,
        };

        let forked_lind_common_ctx = match &self.lind_common_ctx {
            Some(ctx) => Some(ctx.fork()),
            None => None,
        };

        // besides preview1_ctx, lind_common_ctx and forked_lind_fork_ctx, we do not
        // care about other context since they are not used by glibc so we can just share
        // them between processes
        let forked_host = Self {
            preview1_ctx: forked_preview1_ctx,
            lind_fork_ctx: forked_lind_fork_ctx,
            lind_common_ctx: forked_lind_common_ctx,
            wasi_threads: self.wasi_threads.clone(),
        };

        return forked_host;
    }
}

impl LindHost<HostCtx, Option<enarx_config::Config>> for HostCtx {
    fn get_ctx(&self) -> LindCtx<HostCtx, Option<enarx_config::Config>> {
        self.lind_fork_ctx.clone().unwrap()
    }
}

// The Enarx Wasm runtime
#[derive(Clone)]
pub struct Runtime;

impl Runtime {
    // Execute an Enarx [Package]
    /// This function runs the first Wasm module in an Enarx Keep. It parses the Enarx package,
    /// generates or attests an identity, sets up the Wasmtime engine, creates the initial store
    /// and linker, and injects various contexts (WASI, lind-common, lind-multi-process). The
    /// module is instantiated, and the main function is executed via load_main_module. This
    /// function is the primary entry point for initial Wasm execution.
    pub fn execute(package: Package) -> anyhow::Result<Vec<Val>> {
        let (prvkey, crtreq) =
            identity::generate().context("failed to generate a private key and CSR")?;

        let Workload { webasm, config } = package.try_into()?;
        let enarx_conf = config;
        let Config {
            steward,
            args,
            files,
            env,
        } = enarx_conf.clone().unwrap_or_default();

        let certs = if let Some(url) = steward {
            // Obtaining attestation certificates
            identity::steward(&url, crtreq).context("failed to attest to Steward")?
        } else {
            // Generating a self-signed certificate
            identity::selfsigned(&prvkey).context("failed to generate self-signed certificates")?
        }
        .into_iter()
        .map(rustls::Certificate)
        .collect::<Vec<_>>();

        let mut config = wasmtime::Config::new();

        let engine = trace_span!("initialize Wasmtime engine")
            .in_scope(|| Engine::new(&config))
            .context("failed to create execution engine")?;

        let host = HostCtx::default();

        let mut wstore =
            trace_span!("initialize Wasmtime store").in_scope(|| Store::new(&engine, host));

        let module = trace_span!("compile Wasm")
            .in_scope(|| Module::from_binary(&engine, &webasm))
            .context("failed to compile Wasm module")?;

        let lind_manager = Arc::new(LindCageManager::new(0));
        rawposix::safeposix::dispatcher::lindrustinit(0);
        lind_manager.increment();

        // Set up the WASI. In lind-wasm, we predefine all the features we need are `thread` and `wasipreview1`
        // so we manually add them to the linker without checking the input
        let mut linker = trace_span!("setup linker").in_scope(|| Linker::new(&engine));
        // Setup WASI-p1
        trace_span!("link WASI")
            .in_scope(|| {
                wasi_common::sync::add_to_linker(&mut linker, |s: &mut HostCtx| {
                    AsMut::<wasi_common::WasiCtx>::as_mut(s)
                })
            })
            .context("failed to setup linker and link WASI")?;
        let mut builder = WasiCtxBuilder::new();
        // In WASI, the argv semantics follow the POSIX convention: argv[0] is expected to be the program name, and argv[1..]
        // are the actual arguments. However, in Enarx, we don’t have access to the original program name since the Wasm
        // binary is typically loaded from a file descriptor rather than a path. As a result, we insert a placeholder
        // value as argv[0] when constructing the argument list.
        let mut full_args = vec!["main.wasm".to_string()];
        full_args.extend(args.clone());
        builder.inherit_stdio().args(&full_args);
        builder.inherit_stdin();
        builder.inherit_stderr();

        let dir = Dir::open_ambient_dir(HOME_DIR_PATH, cap_std::ambient_authority())
            .expect(&format!("failed to open {}", HOME_DIR_PATH));
        builder
            .preopened_dir(dir, ".")
            .expect("failed to open current directory");
        wstore.data_mut().preview1_ctx = Some(builder.build());

        // Setup WASI-thread
        trace_span!("link WASI-thread")
            .in_scope(|| {
                wasmtime_wasi_threads::add_to_linker(
                    &mut linker,
                    &wstore,
                    &module,
                    |s: &mut HostCtx| s.wasi_threads.as_ref().unwrap(),
                )
            })
            .context("failed to setup linker and link WASI")?;

        // attach Lind-Common-Context to the host
        let shared_next_cageid = Arc::new(AtomicU64::new(1));
        {
            wasmtime_lind_common::add_to_linker::<HostCtx, Option<enarx_config::Config>>(
                &mut linker,
                |host| host.lind_common_ctx.as_ref().unwrap(),
            )?;
            wstore.data_mut().lind_common_ctx =
                Some(LindCommonCtx::new(shared_next_cageid.clone())?);
        }

        // attach Lind-Multi-Process-Context to the host
        {
            wstore.data_mut().lind_fork_ctx = Some(LindCtx::new(
                module.clone(),
                linker.clone(),
                lind_manager.clone(),
                webasm.clone(),
                enarx_conf.clone(),
                shared_next_cageid.clone(),
                |host| host.lind_fork_ctx.as_mut().unwrap(),
                |host| host.fork(),
                |webasm, enarx_conf, path, args, pid, next_cageid, lind_manager, envs| {
                    // This closure is invoked during exec from `lind-multi-process::LindCtx::execve_call` in the lind-wasm.
                    // At that point, `args` has already been populated based on the inputs to `execv()`.
                    // In the current design of lind-wasm-enarx, the arguments field from Enarx.toml is only applied to the
                    // initial wasm module, and is not used for subsequent exec calls. Therefore, we explicitly override the
                    // `args` field inside `enarx_conf` here.
                    // Since the wasm binary is selected via the argument passed to `exec()`, we skip the first element of
                    // `args` (which represents the binary name).
                    // Additionally, when Enarx.toml is not provided, `enarx_conf` will be `None`, so we insert a default
                    // `enarx_config::Config` object as needed before updating its `args` field.
                    let mut new_enarx_conf = enarx_conf.clone();
                    let conf = new_enarx_conf.get_or_insert_with(|| Config {
                        args: vec![],
                        ..Default::default()
                    });
                    conf.args = args.get(1..).map_or(vec![], |s| s.to_vec());

                    Runtime::execute_with_lind(
                        webasm.clone(),
                        Some(conf.clone()),
                        lind_manager.clone(),
                        pid as u64,
                        next_cageid.clone(),
                    )
                },
            )?);
        }

        wstore.data_mut().wasi_threads = Some(Arc::new(WasiThreadsCtx::new(
            module.clone(),
            Arc::new(linker.clone()),
        )?));

        let result = wasmtime_wasi::runtime::with_ambient_tokio_runtime(|| {
            Runtime::load_main_module(
                &mut wstore,
                &mut linker,
                &module,
                CAGE_START_ID as u64,
                &args,
            )
            .with_context(|| format!("failed to run main module"))
        });

        match result {
            Ok(ref res) => {
                let mut code = 0;
                let retval: &Val = res.get(0).unwrap();
                if let Val::I32(res) = retval {
                    code = *res;
                }
                // exit the thread
                if rawposix::interface::lind_thread_exit(
                    CAGE_START_ID as u64,
                    THREAD_START_ID as u64,
                ) {
                    // we clean the cage only if this is the last thread in the cage
                    // exit the cage with the exit code
                    lind_syscall_api(1, EXIT_SYSCALL as u32, 0, code as u64, 0, 0, 0, 0, 0);

                    // main cage exits
                    lind_manager.decrement();
                }
            }
            Err(e) => {
                // Exit the process if Wasmtime understands the error;
                // otherwise, fall back on Rust's default error printing/return
                // code.
                return Err(wasi_common::maybe_exit_on_error(e));
            }
        }

        result
    }

    /// This function is called when a new Wasm module is executed via an exec() syscall inside
    /// a Wasm process. It mirrors much of the behavior of execute, but instead of reading
    /// configuration from Enarx.toml, it uses an updated or synthetic config passed in at runtime.
    /// This config has its args explicitly overridden. A new HostCtx is created, associated with
    /// a new PID, and the module is launched in its own cage.
    pub fn execute_with_lind(
        // Wasm module
        webasm: Vec<u8>,
        // Enarx keep configuration
        config: Option<Config>,
        lind_manager: Arc<LindCageManager>,
        pid: u64,
        next_cageid: Arc<AtomicU64>,
    ) -> Result<Vec<Val>> {
        let enarx_conf = config;
        let Config {
            steward,
            args,
            files,
            env,
        } = enarx_conf.clone().unwrap_or_default();

        let mut config = wasmtime::Config::new();

        let engine = trace_span!("initialize Wasmtime engine")
            .in_scope(|| Engine::new(&config))
            .context("failed to create execution engine")?;

        let host = HostCtx::default();

        let mut wstore =
            trace_span!("initialize Wasmtime store").in_scope(|| Store::new(&engine, host));

        let module = trace_span!("compile Wasm")
            .in_scope(|| Module::from_binary(&engine, &webasm))
            .context("failed to compile Wasm module")?;

        // Set up the WASI. In lind-wasm, we predefine all the features we need are `thread` and `wasipreview1`
        // so we manually add them to the linker without checking the input
        let mut linker = trace_span!("setup linker").in_scope(|| Linker::new(&engine));
        // Setup WASI-p1
        trace_span!("link WASI")
            .in_scope(|| {
                wasi_common::sync::add_to_linker(&mut linker, |s: &mut HostCtx| {
                    AsMut::<wasi_common::WasiCtx>::as_mut(s)
                })
            })
            .context("failed to setup linker and link WASI")?;
        let mut builder = WasiCtxBuilder::new();
        // In WASI, the argv semantics follow the POSIX convention: argv[0] is expected to be the program name, and argv[1..]
        // are the actual arguments. However, in Enarx, we don’t have access to the original program name since the Wasm
        // binary is typically loaded from a file descriptor rather than a path. As a result, we insert a placeholder
        // value as argv[0] when constructing the argument list.
        let mut full_args = vec!["main.wasm".to_string()];
        full_args.extend(args.clone());
        builder.inherit_stdio().args(&full_args);
        builder.inherit_stdin();
        builder.inherit_stderr();

        let dir = Dir::open_ambient_dir(HOME_DIR_PATH, cap_std::ambient_authority())
            .expect(&format!("failed to open {}", HOME_DIR_PATH));
        builder
            .preopened_dir(dir, ".")
            .expect("failed to open current directory");
        wstore.data_mut().preview1_ctx = Some(builder.build());

        // Setup WASI-thread
        trace_span!("link WASI-thread")
            .in_scope(|| {
                wasmtime_wasi_threads::add_to_linker(
                    &mut linker,
                    &wstore,
                    &module,
                    |s: &mut HostCtx| s.wasi_threads.as_ref().unwrap(),
                )
            })
            .context("failed to setup linker and link WASI")?;

        // attach Lind-Common-Context to the host
        let shared_next_cageid = Arc::new(AtomicU64::new(1));
        {
            wasmtime_lind_common::add_to_linker::<HostCtx, Option<enarx_config::Config>>(
                &mut linker,
                |host| host.lind_common_ctx.as_ref().unwrap(),
            )?;
            // Create a new lind ctx with the next cage ID since we are going to fork
            wstore.data_mut().lind_common_ctx = Some(LindCommonCtx::new_with_pid(
                pid as i32,
                next_cageid.clone(),
            )?);
        }

        // attach Lind-Multi-Process-Context to the host
        {
            wstore.data_mut().lind_fork_ctx = Some(LindCtx::new_with_pid(
                module.clone(),
                linker.clone(),
                lind_manager.clone(),
                webasm.clone(),
                enarx_conf.clone(),
                pid as i32,
                next_cageid.clone(),
                |host| host.lind_fork_ctx.as_mut().unwrap(),
                |host| host.fork(),
                |webasm, enarx_conf, path, args, pid, next_cageid, lind_manager, envs| {
                    // This closure is invoked during exec from `lind-multi-process::LindCtx::execve_call` in the lind-wasm.
                    // At that point, `args` has already been populated based on the inputs to `execv()`.
                    // In the current design of lind-wasm-enarx, the arguments field from Enarx.toml is only applied to the
                    // initial wasm module, and is not used for subsequent exec calls. Therefore, we explicitly override the
                    // `args` field inside `enarx_conf` here.
                    // Since the wasm binary is selected via the argument passed to `exec()`, we skip the first element of
                    // `args` (which represents the binary name).
                    // Additionally, when Enarx.toml is not provided, `enarx_conf` will be `None`, so we insert a default
                    // `enarx_config::Config` object as needed before updating its `args` field.
                    let mut new_enarx_conf = enarx_conf.clone();
                    let conf = new_enarx_conf.get_or_insert_with(|| Config {
                        args: vec![],
                        ..Default::default()
                    });
                    conf.args = args.get(1..).map_or(vec![], |s| s.to_vec());

                    Runtime::execute_with_lind(
                        webasm.clone(),
                        Some(conf.clone()),
                        lind_manager.clone(),
                        pid as u64,
                        next_cageid.clone(),
                    )
                },
            )?);
        }

        wstore.data_mut().wasi_threads = Some(Arc::new(WasiThreadsCtx::new(
            module.clone(),
            Arc::new(linker.clone()),
        )?));

        let result = wasmtime_wasi::runtime::with_ambient_tokio_runtime(|| {
            Runtime::load_main_module(&mut wstore, &mut linker, &module, pid as u64, &args)
                .with_context(|| format!("failed to run main module"))
        });

        result
    }

    /// This function takes a compiled module, instantiates it with the current store and linker,
    /// and executes its entry point. This is the point where the Wasm "process" actually starts
    /// executing.
    fn load_main_module(
        store: &mut Store<HostCtx>,
        linker: &mut Linker<HostCtx>,
        module: &Module,
        pid: u64,
        args: &[String],
    ) -> Result<Vec<Val>> {
        let instance = linker
            .instantiate_with_lind(&mut *store, &module, InstantiateType::InstantiateFirst(pid))
            .context(format!("failed to instantiate"))?;

        // If `_initialize` is present, meaning a reactor, then invoke
        // the function.
        if let Some(func) = instance.get_func(&mut *store, "_initialize") {
            func.typed::<(), ()>(&store)?.call(&mut *store, ())?;
        }

        // Look for the specific function provided or otherwise look for
        // "" or "_start" exports to run as a "main" function.
        let func = instance
            .get_func(&mut *store, "")
            .or_else(|| instance.get_func(&mut *store, "_start"));

        let stack_low = instance.get_stack_low(store.as_context_mut()).unwrap();
        let stack_pointer = instance.get_stack_pointer(store.as_context_mut()).unwrap();
        store.as_context_mut().set_stack_base(stack_pointer as u64);
        store.as_context_mut().set_stack_top(stack_low as u64);

        // retrieve the epoch global
        let lind_epoch = instance
            .get_export(&mut *store, "epoch")
            .and_then(|export| export.into_global())
            .expect("Failed to find epoch global export!");

        // retrieve the handler (underlying pointer) for the epoch global
        let pointer = lind_epoch.get_handler(&mut *store);

        // initialize the signal for the main thread of the cage
        rawposix::interface::lind_signal_init(
            pid,
            pointer as *mut u64,
            THREAD_START_ID,
            true, /* this is the main thread */
        );

        // see comments at signal_may_trigger for more details
        rawposix::interface::signal_may_trigger(pid);

        let result = match func {
            Some(func) => Runtime::invoke_func(store, func, &args),
            None => Ok(vec![]),
        };
        result
    }

    /// This function takes a Wasm function (Func) and a list of string arguments, parses the
    /// arguments into Wasm values based on expected types (ValType), and invokes the function
    fn invoke_func(store: &mut Store<HostCtx>, func: Func, args: &[String]) -> Result<Vec<Val>> {
        let ty = func.ty(&store);
        if ty.params().len() > 0 {
            eprintln!(
                "warning: using `--invoke` with a function that takes arguments \
                 is experimental and may break in the future"
            );
        }
        let mut args = args.iter();
        let mut values = Vec::new();
        for ty in ty.params() {
            let val_str = args
                .next()
                .ok_or_else(|| anyhow!("not enough arguments for invoke"))?;
            let val = match ty {
                ValType::I32 => Val::I32(val_str.parse()?),
                ValType::I64 => Val::I64(val_str.parse()?),
                ValType::F32 => Val::F32(val_str.parse::<f32>()?.to_bits()),
                ValType::F64 => Val::F64(val_str.parse::<f64>()?.to_bits()),
                _ => bail!("unsupported argument type {:?}", ty),
            };
            values.push(val);
        }

        // Invoke the function and then afterwards print all the results that came
        // out, if there are any.
        let mut results = vec![Val::null_func_ref(); ty.results().len()];
        func.call(&mut *store, &values, &mut results)
            .with_context(|| format!("failed to invoke command default"));

        Ok(results)
    }
}
