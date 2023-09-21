use std::sync::{Arc, Mutex, OnceLock};

use ahash::AHashMap;

use crate::{
    ir::{FieldPattern, IRFunction, PatternKind},
    items::FunctionSig,
    types::{Mutability, SubList, Type},
    vm::{Function, VM},
};

#[derive(PartialEq, Clone, Copy)]
pub enum FnTrait {
    Fn,
    FnMut,
    FnOnce,
}

/// Plays a similar role to function items. Contains IR and a table of monomorphizations.
/// FUTURE CONSIDERATIONS: Unlike function items, the IR cannot be mutated once set.
/// Closures will NOT support hot loading. Functions will create new closures on being hot loaded.
pub struct Closure<'vm> {
    ir_base: OnceLock<Arc<IRFunction<'vm>>>,
    ir_fn: OnceLock<Arc<IRFunction<'vm>>>,
    ir_mut: OnceLock<Arc<IRFunction<'vm>>>,
    ir_once: OnceLock<Arc<IRFunction<'vm>>>,

    mono_instances: Mutex<AHashMap<SubList<'vm>, &'vm Function<'vm>>>,
    unique_id: u32,
}

impl<'vm> Closure<'vm> {
    pub fn new(unique_id: u32) -> Self {
        Self {
            ir_base: Default::default(),
            ir_fn: Default::default(),
            ir_mut: Default::default(),
            ir_once: Default::default(),

            mono_instances: Default::default(),
            unique_id,
        }
    }

    pub fn ir_base(&self) -> Arc<IRFunction<'vm>> {
        self.ir_base.get().expect("no ir for closure").clone()
    }

    pub fn set_ir_base(&self, ir: IRFunction<'vm>) {
        self.ir_base.set(Arc::new(ir)).ok();
    }

    pub fn ir_for_trait(
        &self,
        vm: &'vm VM<'vm>,
        kind: FnTrait,
        self_ty: Type<'vm>,
    ) -> Arc<IRFunction<'vm>> {
        match kind {
            FnTrait::Fn => self
                .ir_fn
                .get_or_init(|| Arc::new(build_ir_for_trait(vm, &self.ir_base(), kind, self_ty)))
                .clone(),
            FnTrait::FnMut => self
                .ir_mut
                .get_or_init(|| Arc::new(build_ir_for_trait(vm, &self.ir_base(), kind, self_ty)))
                .clone(),
            FnTrait::FnOnce => self
                .ir_once
                .get_or_init(|| Arc::new(build_ir_for_trait(vm, &self.ir_base(), kind, self_ty)))
                .clone(),
        }
    }
}

impl<'vm> std::fmt::Debug for Closure<'vm> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Closure")
    }
}

impl<'vm> std::hash::Hash for Closure<'vm> {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        state.write_u32(self.unique_id)
    }
}

impl<'vm> PartialEq for Closure<'vm> {
    fn eq(&self, other: &Self) -> bool {
        self.unique_id == other.unique_id
    }
}

impl<'vm> Eq for Closure<'vm> {}

/// We make the following transformations to the IR:
///
/// 1. Set the closure_kind.
/// 2. Replace the signature's (args) with (self,(args))
/// 3. Replace the param pattern's (args) with (self,(args))
fn build_ir_for_trait<'vm>(
    vm: &'vm VM<'vm>,
    ir_in: &IRFunction<'vm>,
    kind: FnTrait,
    self_ty: Type<'vm>,
) -> IRFunction<'vm> {
    let mut new_ir = ir_in.clone_ir();

    new_ir.closure_kind = Some(kind);

    let self_ty = match kind {
        FnTrait::Fn => vm.ty_ref(self_ty, Mutability::Const),
        FnTrait::FnMut => vm.ty_ref(self_ty, Mutability::Mut),
        _ => panic!("todo self ty"),
    };

    let args_tuple = vm.ty_tuple(ir_in.sig.inputs.clone());

    new_ir.sig.inputs = vec![self_ty, args_tuple];

    let self_pattern = new_ir.insert_pattern(PatternKind::Hole, self_ty);

    let param_fields: Vec<_> = new_ir
        .params
        .iter()
        .enumerate()
        .map(|(index, id)| FieldPattern {
            field: index as u32,
            pattern: *id,
        })
        .collect();

    let args_pattern = new_ir.insert_pattern(
        PatternKind::Struct {
            fields: param_fields,
        },
        args_tuple,
    );

    new_ir.params = vec![self_pattern, args_pattern];

    new_ir
}
