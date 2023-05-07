use std::{time::Duration, sync::{Mutex, Barrier, Arc, OnceLock}, borrow::BorrowMut, collections::HashMap};

use rustc_session::config;
use rustc_middle::ty::TyCtxt;

use crate::{cli::CliArgs, ir::{IRFunction, IRFunctionBuilder}, vm::VM, items::{Item, CrateId}};

#[derive(Debug)]
pub struct ItemId(u32);

/////////////////////////

pub struct RustCWorker<'vm> {
    sender: Mutex<std::sync::mpsc::Sender<Box<dyn WorkerCommandDyn>>>, // dyn WorkerCommandDyn<'vm>
    vm: &'vm VM<'vm>
}

pub struct WorkerResult<T> {
    complete: Barrier,
    value: Mutex<Option<T>>
}

impl<T> WorkerResult<T> {
    pub fn wait(&self) {
        self.complete.wait();
    }
}

struct WorkerCommand<T,F> {
    func: F,
    result: Arc<WorkerResult<T>>
}

trait WorkerCommandDyn: Send + Sync {
    fn call<'vm>(&self, tcx: TyCtxt, vm: &'vm VM<'vm>, items: &ItemMap<'vm>);
}

impl<T,F> WorkerCommandDyn for WorkerCommand<T,F> where
    F: for<'v> Fn(TyCtxt,&'v VM<'v>,&ItemMap<'v>)->T + Send + Sync,
    T: Send + Sync
{
    fn call<'vm>(&self, tcx: TyCtxt, vm: &'vm VM<'vm>, items: &ItemMap<'vm>) {
        let res = (self.func)(tcx, vm, items);
        {
            let mut guard = self.result.value.lock().unwrap();
            *guard = Some(res);
        }
        self.result.complete.wait();
    }
}

type ItemMap<'vm> = HashMap<String,(rustc_hir::def_id::LocalDefId,Item<'vm>)>;

impl<'vm> RustCWorker<'vm> {
    pub fn new<'s>(args: CliArgs, scope: &'s std::thread::Scope<'s,'vm>, vm: &'vm VM<'vm>) -> Self {

        let (sender,recv) =
            std::sync::mpsc::channel::<Box<dyn WorkerCommandDyn>>();

        let self_profile = if args.profile { config::SwitchWithOptPath::Enabled(None) } else { config::SwitchWithOptPath::Disabled };

        let config = rustc_interface::Config {
            opts: config::Options {
                //maybe_sysroot: Some(path::PathBuf::from(sysroot)), //Some(path::PathBuf::from("./bunk/")), // sysroot
                edition: rustc_span::edition::Edition::Edition2021,
                unstable_features: rustc_feature::UnstableFeatures::Cheat,
                cg: config::CodegenOptions {
                    overflow_checks: Some(false),
                    ..config::CodegenOptions::default()
                },
                unstable_opts: config::UnstableOptions {
                    self_profile,
                    ..config::UnstableOptions::default()
                },
                ..config::Options::default()
            },
            input: config::Input::File(args.file_name.into()),
            crate_cfg: rustc_hash::FxHashSet::default(),
            crate_check_cfg: config::CheckCfg::default(),
            // (Some(Mode::Std), "backtrace_in_libstd", None),
            output_dir: None,
            output_file: None,
            file_loader: None,
            locale_resources: rustc_driver::DEFAULT_LOCALE_RESOURCES,
            lint_caps: rustc_hash::FxHashMap::default(),
            parse_sess_created: None,
            register_lints: None,
            override_queries: None,
            make_codegen_backend: None,
            registry: rustc_errors::registry::Registry::new(&rustc_error_codes::DIAGNOSTICS),
        };
        
        scope.spawn(move || {
            rustc_interface::run_compiler(config, |compiler| {
                compiler.enter(move |queries| {
                    queries.global_ctxt().unwrap().enter(|tcx| {

                        let mut items: ItemMap = Default::default();

                        let hir = tcx.hir();

                        let this_crate = CrateId::new(0);

                        for item in hir.items() {
                            let local_id = item.owner_id.def_id;
                            let item_path = hir.def_path(local_id).to_string_no_crate_verbose();

                            let vm_item = vm.items.get_item(this_crate, &item_path, vm);

                            items.insert(item_path,(local_id,vm_item));
                        }

                        println!("ready to process!");

                        loop {
                            let cmd = recv.recv().unwrap();
                            cmd.call(tcx, vm, &items);
                        }
                    })
                });
            });
        });

        RustCWorker {
            sender: Mutex::new(sender),
            vm
        }
    }

    fn call<T,F>(&self, func: F) -> Arc<WorkerResult<T>> where
        F: for<'v> Fn(TyCtxt,&'v VM<'v>,&ItemMap<'v>)->T + Send + Sync + 'static,
        T: Send + Sync + 'static
    {
        let result = Arc::new(WorkerResult::<T>{
            complete: Barrier::new(2),
            value: Mutex::new(None)
        });

        let cmd = Box::new(WorkerCommand{
            func,
            result: result.clone()
        });

        {
            let sender = self.sender.lock().unwrap();
            sender.send(cmd).unwrap();
        }

        result
    }

    /*pub fn setup(&self) -> Arc<WorkerResult<()>> {
        self.call(move |tcx| {

            /*let hir = tcx.hir();

            for item in hir.items() {
                let local_id = item.owner_id.def_id;
                let item_path = hir.def_path(local_id).to_string_no_crate_verbose();
                println!("{:?}",item_path);
                /*if item_path == path {
                    return Some(ItemId(local_id.local_def_index.as_u32()));
                }*/
            }*/

            ()
        })
    }*/

    pub fn function_ir(&self, path: String) {
        let res = self.call(move |tcx,vm,items| {
            if let Some((did,vm_item)) = &items.get(&path) {

                let (thir,root) = tcx.thir_body(did).unwrap();
                let ir = IRFunctionBuilder::build(vm, tcx, *did, root, &thir.borrow());
                vm_item.set_ir(ir);
            } else {
                panic!("item not found: {:?}",path);
            }
        });
        res.wait();
    }
}
