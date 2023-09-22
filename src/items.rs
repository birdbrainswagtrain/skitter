use std::{
    borrow::{BorrowMut, Cow},
    hash::Hash,
    sync::{Arc, Mutex, OnceLock, RwLock},
};

use crate::{
    builtins::BuiltinTrait,
    bytecode_compiler::BytecodeCompiler,
    closure::Closure,
    ir::{glue_builder::glue_for_ctor, IRFunction},
    lazy_collections::{LazyItem, LazyKey},
    persist::{Persist, PersistReader, PersistWriter},
    rustc_worker::RustCContext,
    types::{ItemWithSubs, Sub, SubList, Type, TypeKind},
    vm::{Function, VM},
};
use ahash::AHashMap;

pub struct ExternCrate {
    pub name: String,
    pub id: CrateId,
}

#[derive(Eq, PartialEq, Hash, Clone, Debug, PartialOrd, Ord)]
pub struct ItemPath<'vm>(NameSpace, &'vm str);

impl<'vm> ItemPath<'vm> {
    pub fn main() -> Self {
        Self(NameSpace::Value, "::main")
    }
    pub fn can_lookup(&self) -> bool {
        match self.0 {
            NameSpace::DebugOnly => false,
            _ => true,
        }
    }
    pub fn for_debug(name: &'vm str) -> Self {
        Self(NameSpace::DebugOnly, name)
    }
    pub fn for_type(name: &'vm str) -> Self {
        Self(NameSpace::Type, name)
    }
    pub fn for_value(name: &'vm str) -> Self {
        Self(NameSpace::Value, name)
    }
    pub fn as_string(&self) -> &str {
        &self.1
    }
}

impl<'vm> Persist<'vm> for ItemPath<'vm> {
    fn persist_write(&self, writer: &mut PersistWriter<'vm>) {
        match self.0 {
            NameSpace::Type => writer.write_byte('t' as u8),
            NameSpace::Value => writer.write_byte('v' as u8),
            NameSpace::DebugOnly => writer.write_byte('d' as u8),
        }
        writer.write_str(self.1);
    }

    fn persist_read(reader: &mut PersistReader<'vm>) -> Self {
        let ns = reader.read_byte() as char;
        let ns = match ns {
            't' => NameSpace::Type,
            'v' => NameSpace::Value,
            'd' => NameSpace::DebugOnly,
            _ => panic!(),
        };

        let string = reader.read_str();

        Self(ns, string)
    }
}

#[derive(Eq, PartialEq, Hash, Clone, Copy, Debug, PartialOrd, Ord)]
enum NameSpace {
    /// Structs, Enums, etc.
    Type,
    /// Functions
    Value,
    /// Not used for real paths (impls and ???)
    DebugOnly,
}

#[derive(PartialEq, Clone, Copy, Debug)]
pub struct CrateId(u32);

impl CrateId {
    pub fn new(n: u32) -> Self {
        Self(n)
    }

    pub fn index(&self) -> usize {
        self.0 as usize
    }
}

#[derive(PartialEq, Clone, Copy, Debug)]
pub struct ItemId(u32);

impl ItemId {
    pub fn new(n: u32) -> Self {
        Self(n)
    }

    pub fn index(&self) -> usize {
        self.0 as usize
    }
}

pub struct Item<'vm> {
    pub vm: &'vm VM<'vm>,
    pub crate_id: CrateId,
    pub item_id: ItemId,
    pub path: ItemPath<'vm>,
    pub saved_ir: Option<&'vm [u8]>,
    kind: ItemKind<'vm>,
}

impl<'vm> Item<'vm> {
    pub fn new(
        vm: &'vm VM<'vm>,
        crate_id: CrateId,
        item_id: ItemId,
        path: ItemPath<'vm>,
        kind: ItemKind<'vm>,
    ) -> Self {
        Item {
            vm,
            crate_id,
            item_id,
            path,
            saved_ir: None,
            kind,
        }
    }
}

impl<'vm> std::fmt::Debug for Item<'vm> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Item({:?})", self.path.as_string())
    }
}

impl<'vm> PartialEq for Item<'vm> {
    fn eq(&self, other: &Self) -> bool {
        self.crate_id == other.crate_id && self.item_id == other.item_id
    }
}

impl<'vm> Eq for Item<'vm> {}

impl<'vm> Hash for Item<'vm> {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        state.write_u32(self.crate_id.0);
        state.write_u32(self.item_id.0);
    }
}

impl<'vm> LazyItem<'vm> for &'vm Item<'vm> {
    type Input = Item<'vm>;

    fn input(&self) -> &Self::Input {
        self
    }

    fn build(input: Self::Input, vm: &'vm VM<'vm>) -> Self {
        vm.alloc_item(input)
    }
}

impl<'vm> LazyKey<'vm> for &'vm Item<'vm> {
    type Key = ItemPath<'vm>;

    fn key(input: &Self::Input) -> Option<&Self::Key> {
        if input.path.0 == NameSpace::DebugOnly {
            None
        } else {
            Some(&input.path)
        }
    }
}

impl<'vm> Persist<'vm> for Item<'vm> {
    fn persist_write(&self, writer: &mut PersistWriter<'vm>) {
        self.item_id.0.persist_write(writer);
        self.path.persist_write(writer);
        match &self.kind {
            ItemKind::Function {
                virtual_info,
                extern_name,
                ctor_for,
                ..
            } => {
                writer.write_byte('f' as u8);
                virtual_info.persist_write(writer);
                ctor_for.map(|(x, y)| (x.0, y)).persist_write(writer);
                extern_name.persist_write(writer);

                let ir_block = self
                    .raw_ir()
                    .map(|ir| {
                        let mut writer = writer.new_child_writer();
                        ir.persist_write(&mut writer);
                        writer.flip()
                    })
                    .unwrap_or_else(|| Vec::new());

                writer.write_byte_slice(&ir_block);
            }
            ItemKind::Constant {
                virtual_info,
                ctor_for,
                ..
            } => {
                writer.write_byte('c' as u8);
                virtual_info.persist_write(writer);
                ctor_for.map(|(x, y)| (x.0, y)).persist_write(writer);

                let ir_block = self
                    .raw_ir()
                    .map(|ir| {
                        let mut writer = writer.new_child_writer();
                        ir.persist_write(&mut writer);
                        writer.flip()
                    })
                    .unwrap_or_else(|| Vec::new());

                writer.write_byte_slice(&ir_block);
            }
            ItemKind::AssociatedType { virtual_info } => {
                writer.write_byte('y' as u8);
                virtual_info.persist_write(writer);
            }
            ItemKind::Adt { info } => {
                writer.write_byte('a' as u8);

                let adt_block = {
                    let mut writer = writer.new_child_writer();
                    info.get().unwrap().persist_write(&mut writer);
                    writer.flip()
                };
                writer.write_byte_slice(&adt_block);
            }
            ItemKind::Trait {
                builtin,
                assoc_value_map,
                ..
            } => {
                writer.write_byte('t' as u8);
                builtin.get().copied().persist_write(writer);
                assoc_value_map.get().unwrap().persist_write(writer);
            }
        }
    }

    fn persist_read(reader: &mut PersistReader<'vm>) -> Self {
        let item_id = ItemId(u32::persist_read(reader));
        let path = ItemPath::persist_read(reader);

        // todo actual kind
        let kind_c = reader.read_byte() as char;

        let mut ir: &[u8] = &[];

        let kind = match kind_c {
            'f' => {
                let virtual_info = Option::<VirtualInfo>::persist_read(reader);
                let ctor_for =
                    Option::<(u32, u32)>::persist_read(reader).map(|(a, b)| (ItemId(a), b));
                let extern_name = Option::<(FunctionAbi, String)>::persist_read(reader);
                let kind = ItemKind::Function {
                    ir: Default::default(),
                    mono_instances: Default::default(),
                    virtual_info,
                    extern_name,
                    ctor_for,
                    closures: Default::default(),
                };
                ir = reader.read_byte_slice();
                kind
            }
            'c' => {
                let virtual_info = Option::<VirtualInfo>::persist_read(reader);
                let ctor_for =
                    Option::<(u32, u32)>::persist_read(reader).map(|(a, b)| (ItemId(a), b));
                let kind = ItemKind::Constant {
                    ir: Default::default(),
                    mono_values: Default::default(),
                    virtual_info,
                    ctor_for,
                };
                ir = reader.read_byte_slice();
                kind
            }
            'y' => {
                let virtual_info = VirtualInfo::persist_read(reader);
                let kind = ItemKind::AssociatedType { virtual_info };
                kind
            }
            'a' => {
                ir = reader.read_byte_slice();
                ItemKind::Adt {
                    info: OnceLock::new(),
                }
            }
            't' => {
                let builtin = Option::<BuiltinTrait>::persist_read(reader);
                let assoc_value_map = AHashMap::<ItemPath, u32>::persist_read(reader);

                ItemKind::new_trait_with(assoc_value_map, builtin)
            }
            _ => panic!(),
        };

        let saved_ir = if ir.len() > 0 { Some(ir) } else { None };

        Item {
            vm: reader.context.vm,
            crate_id: reader.context.this_crate,
            item_id,
            path,
            kind,
            saved_ir,
        }
    }
}

impl<'vm> Persist<'vm> for BoundKind<'vm> {
    fn persist_write(&self, writer: &mut PersistWriter<'vm>) {
        match self {
            BoundKind::Trait(item) => {
                writer.write_byte(0);
                item.persist_write(writer);
            }
            BoundKind::Projection(item, ty) => {
                writer.write_byte(1);
                item.persist_write(writer);
                ty.persist_write(writer);
            }
        }
    }

    fn persist_read(reader: &mut PersistReader<'vm>) -> Self {
        let b = reader.read_byte();
        match b {
            0 => {
                let item = ItemWithSubs::persist_read(reader);
                BoundKind::Trait(item)
            }
            1 => {
                let item = ItemWithSubs::persist_read(reader);
                let ty = Type::persist_read(reader);
                BoundKind::Projection(item, ty)
            }
            _ => panic!(),
        }
    }
}

impl<'vm> Persist<'vm> for AssocValue<'vm> {
    fn persist_write(&self, writer: &mut PersistWriter<'vm>) {
        match self {
            AssocValue::Item(item) => {
                writer.write_byte(0);
                item.index().persist_write(writer);
            }
            AssocValue::Type(ty) => {
                writer.write_byte(1);
                ty.persist_write(writer);
            }
            AssocValue::RawFunctionIR(..) => {
                panic!("attempt to persist raw IR");
            }
        }
    }

    fn persist_read(reader: &mut PersistReader<'vm>) -> Self {
        let b = reader.read_byte();
        match b {
            0 => {
                let item_id = u32::persist_read(reader);
                AssocValue::Item(ItemId::new(item_id))
            }
            1 => {
                let ty = Type::persist_read(reader);
                AssocValue::Type(ty)
            }
            _ => panic!(),
        }
    }
}

#[derive(PartialEq)]
pub enum FunctionAbi {
    RustIntrinsic,
}

impl<'vm> Persist<'vm> for (FunctionAbi, String) {
    fn persist_write(&self, writer: &mut PersistWriter<'vm>) {
        assert!(self.0 == FunctionAbi::RustIntrinsic);
        self.1.persist_write(writer);
    }

    fn persist_read(reader: &mut PersistReader<'vm>) -> Self {
        let ident = String::persist_read(reader);
        (FunctionAbi::RustIntrinsic, ident)
    }
}

///
/// `virtual_info` is attached to each item appearing in a trait declaration,
/// and is used to resolve concrete implementations of those items.
pub enum ItemKind<'vm> {
    Function {
        ir: Mutex<Option<Arc<IRFunction<'vm>>>>,
        mono_instances: Mutex<AHashMap<SubList<'vm>, &'vm Function<'vm>>>,
        virtual_info: Option<VirtualInfo>,
        ctor_for: Option<(ItemId, u32)>,
        extern_name: Option<(FunctionAbi, String)>,
        closures: Mutex<AHashMap<Vec<u32>, &'vm Closure<'vm>>>,
    },
    /// Constants operate very similarly to functions, but are evaluated
    /// greedily when encountered in IR and converted directly to values.
    Constant {
        ir: Mutex<Option<Arc<IRFunction<'vm>>>>,
        mono_values: Mutex<AHashMap<SubList<'vm>, &'vm [u8]>>,
        virtual_info: Option<VirtualInfo>,
        ctor_for: Option<(ItemId, u32)>,
    },
    AssociatedType {
        virtual_info: VirtualInfo,
    },
    Adt {
        info: OnceLock<AdtInfo<'vm>>,
    },
    Trait {
        assoc_value_map: OnceLock<AHashMap<ItemPath<'vm>, u32>>,
        impl_list: RwLock<Vec<TraitImpl<'vm>>>,
        builtin: OnceLock<BuiltinTrait>,
    },
}

#[derive(Debug, Clone, Eq, PartialEq, Hash)]
pub struct FunctionSig<'vm> {
    pub inputs: Vec<Type<'vm>>,
    pub output: Type<'vm>,
}

impl<'vm> FunctionSig<'vm> {
    pub fn from_rustc<'tcx>(
        rs_sig: &rustc_middle::ty::FnSig<'tcx>,
        ctx: &RustCContext<'vm, 'tcx>,
    ) -> Self {
        let inputs = rs_sig
            .inputs()
            .iter()
            .map(|ty| ctx.vm.types.type_from_rustc(*ty, ctx))
            .collect();
        let output = ctx.vm.types.type_from_rustc(rs_sig.output(), ctx);

        Self { inputs, output }
    }

    pub fn sub(&self, subs: &SubList<'vm>) -> Self {
        let inputs = self.inputs.iter().map(|ty| ty.sub(subs)).collect();
        let output = self.output.sub(subs);

        Self { inputs, output }
    }
}

impl<'vm> Persist<'vm> for FunctionSig<'vm> {
    fn persist_read(reader: &mut PersistReader<'vm>) -> Self {
        let inputs = Persist::persist_read(reader);
        let output = Persist::persist_read(reader);
        Self { inputs, output }
    }

    fn persist_write(&self, writer: &mut PersistWriter<'vm>) {
        self.inputs.persist_write(writer);
        self.output.persist_write(writer);
    }
}

pub enum AdtKind<'vm> {
    Struct,
    EnumWithDiscriminant(Type<'vm>),
    EnumNonZero,
}

pub struct AdtInfo<'vm> {
    pub variant_fields: Vec<Vec<Type<'vm>>>,
    pub kind: AdtKind<'vm>,
}

impl<'vm> AdtInfo<'vm> {
    pub fn is_enum(&self) -> bool {
        match self.kind {
            AdtKind::Struct => false,
            AdtKind::EnumNonZero | AdtKind::EnumWithDiscriminant(_) => true,
        }
    }

    pub fn discriminant_ty(&self) -> Option<Type<'vm>> {
        match self.kind {
            AdtKind::EnumWithDiscriminant(ty) => Some(ty),
            _ => None,
        }
    }
}

impl<'vm> Persist<'vm> for AdtInfo<'vm> {
    fn persist_write(&self, writer: &mut PersistWriter<'vm>) {
        self.variant_fields.persist_write(writer);

        match self.kind {
            AdtKind::Struct => {
                writer.write_byte(0);
            }
            AdtKind::EnumWithDiscriminant(ty) => {
                writer.write_byte(1);
                ty.persist_write(writer);
            }
            AdtKind::EnumNonZero => {
                writer.write_byte(2);
            }
        }
    }

    fn persist_read(reader: &mut PersistReader<'vm>) -> Self {
        let variant_fields = <Vec<Vec<Type<'vm>>>>::persist_read(reader);

        let kind = match reader.read_byte() {
            0 => AdtKind::Struct,
            1 => {
                let ty = Persist::persist_read(reader);
                AdtKind::EnumWithDiscriminant(ty)
            }
            2 => AdtKind::EnumNonZero,
            _ => panic!(),
        };

        AdtInfo {
            variant_fields,
            kind,
        }
    }
}

/// Always refers to a trait item in the same crate.
pub struct VirtualInfo {
    pub trait_id: ItemId,
    pub member_index: u32,
}

impl<'vm> Persist<'vm> for VirtualInfo {
    fn persist_write(&self, writer: &mut PersistWriter<'vm>) {
        self.trait_id.index().persist_write(writer);
        self.member_index.persist_write(writer);
    }

    fn persist_read(reader: &mut PersistReader<'vm>) -> Self {
        let trait_id = ItemId(u32::persist_read(reader));
        let member_index = u32::persist_read(reader);
        Self {
            trait_id,
            member_index,
        }
    }
}

pub struct TraitImpl<'vm> {
    pub for_types: SubList<'vm>,
    //impl_params: Vec<Sub<'vm>>,
    pub crate_id: CrateId,
    /// A vector of associated values. The trait item contains an index of identifiers to indices.
    pub assoc_values: Vec<Option<AssocValue<'vm>>>,
    pub bounds: Vec<BoundKind<'vm>>,
    pub generics: GenericCounts,
}

#[derive(Clone)]
pub enum AssocValue<'vm> {
    /// An item. Can be either a function or a constant.
    Item(ItemId),
    /// A type.
    Type(Type<'vm>),
    /// Used to inject IR into builtin traits without building entire new items.
    RawFunctionIR(Arc<IRFunction<'vm>>, IRFlag),
}

/// This is a hack to get correct subs in closures.
#[derive(Clone, PartialEq)]
pub enum IRFlag {
    None,
    UseClosureSubs,
}

pub enum BoundKind<'vm> {
    /// Is the trait item implemented for the given subs?
    Trait(ItemWithSubs<'vm>),
    /// Are the associated type and the second type equal? Can update params in the impl.
    Projection(ItemWithSubs<'vm>, Type<'vm>),
}

#[derive(Default, Debug)]
pub struct GenericCounts {
    pub lifetimes: u32,
    pub types: u32,
    pub consts: u32,
}

impl<'vm> ItemKind<'vm> {
    pub fn new_function() -> Self {
        Self::Function {
            ir: Default::default(),
            mono_instances: Default::default(),
            virtual_info: None,
            extern_name: None,
            ctor_for: None,
            closures: Default::default(),
        }
    }

    pub fn new_function_virtual(trait_id: ItemId, member_index: u32) -> Self {
        Self::Function {
            ir: Default::default(),
            mono_instances: Default::default(),
            virtual_info: Some(VirtualInfo {
                trait_id,
                member_index,
            }),
            extern_name: None,
            ctor_for: None,
            closures: Default::default(),
        }
    }

    pub fn new_function_extern(abi: FunctionAbi, name: String) -> Self {
        Self::Function {
            ir: Default::default(),
            mono_instances: Default::default(),
            virtual_info: None,
            extern_name: Some((abi, name)),
            ctor_for: None,
            closures: Default::default(),
        }
    }

    pub fn new_function_ctor(adt_id: ItemId, variant: u32) -> Self {
        Self::Function {
            ir: Default::default(),
            mono_instances: Default::default(),
            virtual_info: None,
            extern_name: None,
            ctor_for: Some((adt_id, variant)),
            closures: Default::default(),
        }
    }

    pub fn new_const() -> Self {
        Self::Constant {
            ir: Default::default(),
            mono_values: Default::default(),
            virtual_info: None,
            ctor_for: None,
        }
    }

    pub fn new_const_virtual(trait_id: ItemId, member_index: u32) -> Self {
        Self::Constant {
            ir: Default::default(),
            mono_values: Default::default(),
            virtual_info: Some(VirtualInfo {
                trait_id,
                member_index,
            }),
            ctor_for: None,
        }
    }

    pub fn new_const_ctor(adt_id: ItemId, variant: u32) -> Self {
        Self::Constant {
            ir: Default::default(),
            mono_values: Default::default(),
            virtual_info: None,
            ctor_for: Some((adt_id, variant)),
        }
    }

    pub fn new_associated_type(trait_id: ItemId, member_index: u32) -> Self {
        Self::AssociatedType {
            virtual_info: VirtualInfo {
                trait_id,
                member_index,
            },
        }
    }

    pub fn new_adt() -> Self {
        Self::Adt {
            info: Default::default(),
        }
    }

    pub fn new_trait() -> Self {
        Self::Trait {
            assoc_value_map: Default::default(),
            impl_list: Default::default(),
            builtin: Default::default(),
        }
    }

    pub fn new_trait_with(
        assoc_value_map: AHashMap<ItemPath<'vm>, u32>,
        builtin: Option<BuiltinTrait>,
    ) -> Self {
        let mut builtin_lock = OnceLock::new();

        if let Some(builtin) = builtin {
            builtin_lock.set(builtin).unwrap();
        }

        Self::Trait {
            assoc_value_map: assoc_value_map.into(),
            impl_list: Default::default(),
            builtin: builtin_lock,
        }
    }
}

impl<'vm> Item<'vm> {
    /// Get a monomorphic VM function from a function item.
    pub fn func_mono(&'vm self, subs: &SubList<'vm>) -> &'vm Function<'vm> {
        let ItemKind::Function{mono_instances,..} = &self.kind else {
            panic!("item kind mismatch");
        };

        let mut mono_instances = mono_instances.lock().unwrap();
        let result_func = mono_instances
            .entry(subs.clone())
            .or_insert_with(|| self.vm.alloc_function(self, subs.clone()));

        result_func
    }

    pub fn is_function(&self) -> bool {
        if let ItemKind::Function { .. } = &self.kind {
            true
        } else {
            false
        }
    }

    pub fn func_extern(&self) -> &Option<(FunctionAbi, String)> {
        let ItemKind::Function{extern_name,..} = &self.kind else {
            panic!("item kind mismatch");
        };

        extern_name
    }

    pub fn func_sig(&self, subs: &SubList<'vm>) -> FunctionSig<'vm> {
        let (ir, new_subs) = self.ir(subs);

        ir.sig.sub(&new_subs)
    }

    /// Get the IR for a function OR a constant. Subs are used to find specialized IR for trait items.
    pub fn ir<'a>(&self, subs: &'a SubList<'vm>) -> (Arc<IRFunction<'vm>>, Cow<'a, SubList<'vm>>) {
        let (ir, virtual_info, ctor_for, is_constant) = match &self.kind {
            ItemKind::Function {
                ir,
                virtual_info,
                ctor_for,
                ..
            } => (ir, virtual_info, ctor_for, false),
            ItemKind::Constant {
                ir,
                virtual_info,
                ctor_for,
                ..
            } => (ir, virtual_info, ctor_for, true),
            _ => panic!("item kind mismatch"),
        };

        // handle ctors
        if let Some((ctor_item_id, ctor_variant)) = ctor_for {
            let crate_items = self.vm.crate_provider(self.crate_id);
            let ctor_item = crate_items.item_by_id(*ctor_item_id);

            let ctor_ty = self.vm.ty_adt(ItemWithSubs {
                item: ctor_item,
                subs: subs.clone(),
            });

            let ir = glue_for_ctor(ctor_ty, *ctor_variant, is_constant);
            return (Arc::new(ir), Cow::Borrowed(subs));
        }

        // if this is virtual, try finding a concrete impl
        if let Some(virtual_info) = virtual_info {
            let crate_items = self.vm.crate_provider(self.crate_id);
            let trait_item = crate_items.item_by_id(virtual_info.trait_id);
            let resolved_func = trait_item.find_trait_item_ir(subs, virtual_info.member_index);
            if let Some((ir, new_subs)) = resolved_func {
                assert!(ir.is_constant == is_constant);
                return (ir, Cow::Owned(new_subs));
            }
        }

        // Normal IR lookup
        {
            let mut ir = ir.lock().unwrap();
            if let Some(ir) = ir.as_ref() {
                assert!(ir.is_constant == is_constant);
                return (ir.clone(), Cow::Borrowed(subs));
            } else {
                let new_ir = self.vm.crate_provider(self.crate_id).build_ir(self.item_id);
                *ir = Some(new_ir.clone());
                return (new_ir, Cow::Borrowed(subs));
            }
        }
    }

    /// Try getting the IR without any complicated lookup.
    /// This is used when saving IR to the disk.
    pub fn raw_ir(&self) -> Option<Arc<IRFunction<'vm>>> {
        let ir = match &self.kind {
            ItemKind::Function { ir, .. } => ir,
            ItemKind::Constant { ir, .. } => ir,
            _ => panic!("item kind mismatch"),
        };

        let ir = ir.lock().unwrap();

        ir.clone()
    }

    pub fn set_raw_ir(&self, new_ir: Arc<IRFunction<'vm>>) {
        let ir = match &self.kind {
            ItemKind::Function { ir, .. } => ir,
            ItemKind::Constant { ir, .. } => ir,
            _ => panic!("item kind mismatch"),
        };

        let mut ir = ir.lock().unwrap();

        *ir = Some(new_ir);
    }

    pub fn const_value(&self, subs: &SubList<'vm>) -> &'vm [u8] {
        let ItemKind::Constant{mono_values,..} = &self.kind else {
            panic!("item kind mismatch");
        };

        let mut mono_values = mono_values.lock().unwrap();
        let result_val = mono_values.entry(subs.clone()).or_insert_with(|| {
            let (ir, new_subs) = self.ir(subs);

            let bc = BytecodeCompiler::compile(self.vm, &ir, &new_subs, self.path.as_string());

            let const_thread = self.vm.make_thread();
            const_thread.run_bytecode(&bc, 0);
            let ty = ir.sig.output; // todo sub?

            let const_bytes = const_thread.copy_result(0, ty.layout().assert_size() as usize);
            self.vm.alloc_constant(const_bytes)
        });

        result_val
    }

    pub fn ctor_info(&self) -> Option<(ItemId, u32)> {
        match &self.kind {
            ItemKind::Function { ctor_for, .. } => ctor_for.clone(),
            ItemKind::Constant { ctor_for, .. } => ctor_for.clone(),
            _ => panic!("item kind mismatch"),
        }
    }

    pub fn child_closure(&self, path_indices: Vec<u32>) -> &'vm Closure<'vm> {
        // TODO just place the closures map on all items?
        // fns and consts will probably account for a majority of items anyway
        match &self.kind {
            ItemKind::Function { closures, .. } => {
                let mut closures = closures.lock().unwrap();

                closures
                    .entry(path_indices)
                    .or_insert_with(|| self.vm.alloc_closure())
            }
            ItemKind::Constant { .. } => {
                println!("TODO CLOSURE IN CONSTANT!");
                self.vm.alloc_closure()
            }
            _ => {
                panic!("attempt to get child closure on {:?}", self)
            }
        }
    }

    pub fn adt_info(&self) -> &AdtInfo<'vm> {
        let ItemKind::Adt{info} = &self.kind else {
            panic!("item kind mismatch");
        };

        if let Some(info) = info.get() {
            info
        } else {
            let new_info = self
                .vm
                .crate_provider(self.crate_id)
                .build_adt(self.item_id);
            info.set(new_info).ok();
            info.get().expect("adt missing fields after forced init")
        }
    }

    pub fn set_adt_info(&self, new_info: AdtInfo<'vm>) {
        let ItemKind::Adt{info} = &self.kind else {
            panic!("item kind mismatch");
        };

        info.set(new_info).ok();
    }

    pub fn add_trait_impl(&self, info: TraitImpl<'vm>) -> usize {
        let ItemKind::Trait{impl_list,..} = &self.kind else {
            panic!("item kind mismatch");
        };

        let mut impl_list = impl_list.write().unwrap();
        let index = impl_list.len();
        impl_list.push(info);
        index
    }

    pub fn trait_set_builtin(&self, new_builtin: BuiltinTrait) {
        let ItemKind::Trait{builtin,..} = &self.kind else {
            panic!("item kind mismatch");
        };
        builtin.set(new_builtin).ok();
    }

    pub fn trait_set_assoc_value_map(&self, new_map: AHashMap<ItemPath<'vm>, u32>) {
        let ItemKind::Trait{assoc_value_map,..} = &self.kind else {
            panic!("item kind mismatch");
        };
        assoc_value_map.set(new_map).ok();
    }

    pub fn trait_build_assoc_values_for_impl(
        &self,
        pairs: &[(ItemPath, AssocValue<'vm>)],
    ) -> Vec<Option<AssocValue<'vm>>> {
        let ItemKind::Trait{assoc_value_map,..} = &self.kind else {
            panic!("item kind mismatch");
        };

        let assoc_value_map = assoc_value_map.get().unwrap();

        let mut results = vec![None; assoc_value_map.len()];

        for (key, val) in pairs {
            if let Some(index) = assoc_value_map.get(key) {
                results[*index as usize] = Some(val.clone());
            } else {
                panic!("failed to find impl member index");
            }
        }

        results
    }

    pub fn resolve_associated_ty(&self, subs: &SubList<'vm>) -> Type<'vm> {
        let ItemKind::AssociatedType{virtual_info} = &self.kind else {
            panic!("item kind mismatch");
        };

        let crate_items = self.vm.crate_provider(self.crate_id);
        let trait_item = crate_items.item_by_id(virtual_info.trait_id);

        trait_item
            .find_trait_impl(subs, &mut None, |trait_impl, subs| {
                let ty = &trait_impl.assoc_values[virtual_info.member_index as usize];

                if let Some(AssocValue::Type(ty)) = ty {
                    ty.sub(&subs)
                } else {
                    panic!("failed to find associated type")
                }
            })
            .unwrap_or_else(|| {
                panic!("failed to find {} for {}", self.path.as_string(), subs);
            })
    }

    fn find_trait_item_ir(
        &self,
        for_tys: &SubList<'vm>,
        member_index: u32,
    ) -> Option<(Arc<IRFunction<'vm>>, SubList<'vm>)> {
        self.find_trait_impl(for_tys, &mut None, |trait_impl, subs| {
            let crate_items = self.vm.crate_provider(trait_impl.crate_id);
            let ir_source = &trait_impl.assoc_values[member_index as usize];

            if let Some(ir_source) = ir_source {
                match ir_source {
                    AssocValue::Item(fn_item_id) => {
                        let fn_item = crate_items.item_by_id(*fn_item_id);
                        let (ir, _) = fn_item.ir(&subs);
                        return Some((ir, subs));
                    }
                    AssocValue::RawFunctionIR(ir, flag) => {
                        if *flag == IRFlag::UseClosureSubs {
                            let for_ty = for_tys.list[0].assert_ty();
                            if let TypeKind::Closure(_, _, closure_subs) = for_ty.kind() {
                                return Some((ir.clone(), closure_subs.clone()));
                            } else {
                                panic!("attempt to use closure subs on non-closure");
                            }
                        } else {
                            return Some((ir.clone(), subs));
                        }
                    }
                    AssocValue::Type(_) => panic!("attempt to fetch IR for associated type"),
                }
            } else {
                None
            }
        })
        .unwrap_or_else(|| {
            panic!("failed to find {} for {}", self.path.as_string(), for_tys);
        })
    }

    pub fn trait_has_impl(
        &self,
        for_tys: &SubList<'vm>,
        update_tys: &mut Option<&mut SubList<'vm>>,
    ) -> bool {
        self.find_trait_impl(for_tys, update_tys, |_, _| ())
            .is_some()
    }

    pub fn write_trait_impl(&self, index: usize, writer: &mut PersistWriter<'vm>) {
        let ItemKind::Trait{impl_list,..} = &self.kind else {
            panic!("item kind mismatch");
        };

        let impl_list = impl_list.read().unwrap();
        let impl_ref = &impl_list[index];

        // write a ref to the trait item
        writer.write_item_ref(self);

        impl_ref.for_types.persist_write(writer);
        impl_ref.bounds.persist_write(writer);

        impl_ref.generics.lifetimes.persist_write(writer);
        impl_ref.generics.types.persist_write(writer);
        impl_ref.generics.consts.persist_write(writer);

        impl_ref.assoc_values.persist_write(writer);
    }

    pub fn read_trait_impl(&self, reader: &mut PersistReader<'vm>) {
        let for_types = SubList::persist_read(reader);
        let bounds = Vec::<BoundKind>::persist_read(reader);

        let generics = {
            let lifetimes = u32::persist_read(reader);
            let types = u32::persist_read(reader);
            let consts = u32::persist_read(reader);
            GenericCounts {
                lifetimes,
                types,
                consts,
            }
        };

        let assoc_values = Vec::<Option<AssocValue>>::persist_read(reader);

        // build the impl and add it to our list

        let ItemKind::Trait{impl_list,..} = &self.kind else {
            panic!("item kind mismatch");
        };

        let mut impl_list = impl_list.write().unwrap();

        impl_list.push(TraitImpl {
            crate_id: reader.context.this_crate,
            for_types,
            bounds,
            generics,
            assoc_values,
        });
    }

    /// Find a trait implementation for a given list of types.
    pub fn find_trait_impl<T>(
        &self,
        for_tys: &SubList<'vm>,
        update_tys: &mut Option<&mut SubList<'vm>>,
        callback: impl FnOnce(&TraitImpl<'vm>, SubList<'vm>) -> T,
    ) -> Option<T> {
        let ItemKind::Trait{impl_list,builtin,..} = &self.kind else {
            panic!("item kind mismatch");
        };

        if let Some(builtin) = builtin.get() {
            let builtin_res = builtin.find_candidate(for_tys, self.vm, self);
            if let Some(candidate) = builtin_res {
                if let Some(trait_subs) = self.check_trait_impl(for_tys, &candidate, update_tys) {
                    return Some(callback(&candidate, trait_subs));
                }
            }
        }

        let impl_list = impl_list.read().unwrap();

        for candidate in impl_list.iter() {
            if let Some(trait_subs) = self.check_trait_impl(for_tys, candidate, update_tys) {
                return Some(callback(candidate, trait_subs));
            }
        }

        None
    }

    /// Builds a sub list for the impl, and checks it against the impl bounds.
    fn check_trait_impl(
        &self,
        for_tys: &SubList<'vm>,
        candidate: &TraitImpl<'vm>,
        update_tys: &mut Option<&mut SubList<'vm>>,
    ) -> Option<SubList<'vm>> {
        if let Some(sub_map) = trait_match(for_tys, &candidate.for_types) {
            let mut trait_subs = SubList::from_summary(&candidate.generics, self.vm);
            sub_map.apply_to(SubSide::Rhs, &mut trait_subs);

            if candidate.bounds.len() > 0 {
                for bound in &candidate.bounds {
                    match bound {
                        BoundKind::Trait(trait_bound) => {
                            let types_to_check = trait_bound.subs.sub(&trait_subs);
                            let res = trait_bound
                                .item
                                .trait_has_impl(&types_to_check, &mut Some(&mut trait_subs));

                            if !res {
                                return None;
                            }
                        }
                        BoundKind::Projection(assoc_ty, eq_ty) => {
                            let types_to_check = assoc_ty.subs.sub(&trait_subs);

                            let resolved_assoc_ty =
                                assoc_ty.item.resolve_associated_ty(&types_to_check);

                            let mut res_map = Default::default();

                            if !type_match(resolved_assoc_ty, *eq_ty, &mut res_map) {
                                panic!("unmatched {} = {}", resolved_assoc_ty, eq_ty);
                            }
                            res_map.assert_empty(SubSide::Lhs);
                            res_map.apply_to(SubSide::Rhs, &mut trait_subs);
                        }
                    }
                }
            }

            // Is this needed?
            assert!(trait_subs.is_concrete());

            //sub_map.assert_empty(SubSide::Lhs);
            // yucky. this does appear to be needed for some code (see iter test!)
            // probably not a great way of doing things though
            /*if let Some(update_tys) = update_tys {
                sub_map.apply_to(SubSide::Lhs, update_tys);
            }*/
            Some(trait_subs)
        } else {
            None
        }
    }
}

#[derive(PartialEq, Debug)]
enum SubSide {
    Lhs,
    Rhs,
}

#[derive(Default,Debug)]
struct SubMap<'vm> {
    map: Vec<((SubSide, u32), Type<'vm>)>,
}

impl<'vm> SubMap<'vm> {
    fn set(&mut self, side: SubSide, n: u32, val: Type<'vm>) -> bool {
        let key = (side, n);
        for (ek, ev) in &self.map {
            if *ek == key {
                assert!(*ev == val);
                return true;
            }
        }
        self.map.push((key, val));
        true
    }

    fn apply_to(&self, target_side: SubSide, target_subs: &mut SubList<'vm>) {
        for ((side, n), val) in &self.map {
            if *side == target_side {
                target_subs.list[*n as usize] = Sub::Type(*val);
            }
        }
    }

    fn assert_empty(&self, target_side: SubSide) {
        for ((side, _), _) in &self.map {
            if *side == target_side {
                for entry in &self.map {
                    println!(" - {:?}", entry);
                }
                panic!("SubMap::assert_empty failed");
            }
        }
    }
    /*fn get(&self, side: SubSide, n: u32) -> Option<Type<'vm>> {
        let key = (side,n);
        for (ek,ev) in &self.map {
            if *ek == key {
                return Some(*ev);
            }
        }
        println!("warning! failed to resolve substitution: {:?} / {:?}",self.map,key);
        None
    }*/
}

/// Compares a list of concrete types to a candidate type.
fn trait_match<'vm>(lhs: &SubList<'vm>, rhs: &SubList<'vm>) -> Option<SubMap<'vm>> {
    let mut res_map = SubMap::default();

    if subs_match(lhs, rhs, &mut res_map) {
        Some(res_map)
    } else {
        None
    }
}

fn subs_match<'vm>(lhs: &SubList<'vm>, rhs: &SubList<'vm>, res_map: &mut SubMap<'vm>) -> bool {
    for pair in lhs.list.iter().zip(&rhs.list) {
        match pair {
            (Sub::Type(lhs_ty), Sub::Type(rhs_ty)) => {
                if !type_match(*lhs_ty, *rhs_ty, res_map) {
                    return false;
                }
            }
            _ => {
                if pair.0 != pair.1 {
                    return false;
                }
            }
        }
    }
    true
}

/// Compares types loosely, allowing param types to match anything.
fn type_match<'vm>(lhs_ty: Type<'vm>, rhs_ty: Type<'vm>, res_map: &mut SubMap<'vm>) -> bool {
    if lhs_ty.is_concrete() && lhs_ty == rhs_ty {
        return true;
    }

    match (lhs_ty.kind(), rhs_ty.kind()) {
        (TypeKind::Adt(a), TypeKind::Adt(b)) => {
            (a.item == b.item) && subs_match(&a.subs, &b.subs, res_map)
        }
        (TypeKind::Ptr(in_ref, in_mut), TypeKind::Ptr(trait_ref, trait_mut)) => {
            (in_mut == trait_mut) && type_match(*in_ref, *trait_ref, res_map)
        }
        (TypeKind::Ref(in_ref, in_mut), TypeKind::Ref(trait_ref, trait_mut)) => {
            (in_mut == trait_mut) && type_match(*in_ref, *trait_ref, res_map)
        }
        (TypeKind::Slice(in_elem), TypeKind::Slice(trait_elem)) => {
            type_match(*in_elem, *trait_elem, res_map)
        }
        (TypeKind::Tuple(lhs_children), TypeKind::Tuple(rhs_children)) => {
            if lhs_children.len() != rhs_children.len() {
                false
            } else {
                for (lhs_child, rhs_child) in lhs_children.iter().zip(rhs_children) {
                    if !type_match(*lhs_child, *rhs_child, res_map) {
                        return false;
                    }
                }
                true
            }
        }

        (TypeKind::Param(lhs_param), TypeKind::Param(rhs_param)) => {
            panic!("fixme? this looks annoying");
        }

        (_, TypeKind::Param(param_num)) => res_map.set(SubSide::Rhs, *param_num, lhs_ty),
        (TypeKind::Param(param_num), _) => res_map.set(SubSide::Lhs, *param_num, rhs_ty),

        (TypeKind::Adt(_), _)
        | (_, TypeKind::Adt(_))
        | (TypeKind::Ptr(..), _)
        | (_, TypeKind::Ptr(..))
        | (TypeKind::Ref(..), _)
        | (_, TypeKind::Ref(..))
        | (TypeKind::Bool, _)
        | (_, TypeKind::Bool)
        | (TypeKind::Char, _)
        | (_, TypeKind::Char)
        | (TypeKind::Never, _)
        | (_, TypeKind::Never)
        | (TypeKind::Int(..), _)
        | (_, TypeKind::Int(..))
        | (TypeKind::Float(..), _)
        | (_, TypeKind::Float(..)) => false,
        _ => {
            panic!("match types {} == {}", lhs_ty, rhs_ty)
        }
    }
}

/// Get a path from rustc.
pub fn path_from_rustc<'vm>(
    in_path: &rustc_hir::definitions::DefPath,
    vm: &'vm VM<'vm>,
) -> ItemPath<'vm> {
    use rustc_hir::definitions::DefPathData;

    let mut result = String::new();

    let mut is_debug = false;

    // we only handle trivial paths
    for elem in in_path.data.iter() {
        match elem.data {
            DefPathData::ValueNs(sym) | DefPathData::TypeNs(sym) => {
                if sym.as_str() == "_" {
                    is_debug = true;
                }
                result.push_str("::");
                result.push_str(sym.as_str());
            }
            DefPathData::Impl => {
                result.push_str("::{impl}");
                is_debug = true;
            }
            DefPathData::ClosureExpr => {
                result.push_str("::{closure}");
                is_debug = true;
            }
            DefPathData::Ctor => {
                // do nothing
            }
            DefPathData::ForeignMod => {
                // do nothing
            }
            //DefPathData
            _ => panic!(
                "todo path {:?} {}",
                elem.data,
                in_path.to_string_no_crate_verbose()
            ),
        }
    }

    let result = vm.alloc_path(&result);

    if is_debug {
        ItemPath(NameSpace::DebugOnly, result)
    } else if let Some(last_elem) = in_path.data.last() {
        match last_elem.data {
            DefPathData::ValueNs(_) => ItemPath(NameSpace::Value, result),
            DefPathData::TypeNs(_) => ItemPath(NameSpace::Type, result),
            DefPathData::Ctor => ItemPath(NameSpace::Value, result),
            _ => panic!("can't determine path namespace: {:?}", last_elem.data),
        }
    } else {
        panic!("zero element path?");
    }
}

/// Get a single identifier form a rustc path. Used for trait members.
pub fn ident_from_rustc<'vm>(
    in_path: &rustc_hir::definitions::DefPath,
    vm: &'vm VM<'vm>,
) -> ItemPath<'vm> {
    use rustc_hir::definitions::DefPathData;

    if let Some(last_elem) = in_path.data.last() {
        match last_elem.data {
            DefPathData::ValueNs(sym) => {
                let interned = vm.alloc_path(sym.as_str());
                ItemPath(NameSpace::Value, interned)
            }
            DefPathData::TypeNs(sym) => {
                let interned = vm.alloc_path(sym.as_str());
                ItemPath(NameSpace::Type, interned)
            }
            //DefPathData::Ctor => ItemPath(NameSpace::Value, result),
            _ => panic!("can't determine ident namespace: {:?}", last_elem.data),
        }
    } else {
        panic!("zero element path?");
    }
}
