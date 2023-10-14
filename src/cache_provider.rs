use std::{
    error::Error,
    sync::{Arc, OnceLock},
};

use crate::{
    crate_provider::{CrateProvider, TraitImpl},
    impls::{ImplTable, ImplTableLazy},
    ir::IRFunction,
    items::{AdtInfo, AssocValue, CrateId, Item, ItemId, ItemPath},
    lazy_collections::{LazyArray, LazyTable},
    persist::{Persist, PersistReadContext, PersistReader},
    persist_header::{persist_header_read, PersistCrateHeader},
    types::{SubList, Type},
    vm::VM,
    CratePath,
};

pub struct CacheProvider<'vm> {
    read_context: Arc<PersistReadContext<'vm>>,
    impls: ImplTableLazy<'vm>,
}

impl<'vm> CacheProvider<'vm> {
    pub fn new(
        crate_path: &CratePath,
        vm: &'vm VM<'vm>,
        this_crate: CrateId,
    ) -> Result<Self, Box<dyn Error>> {
        let bytes = std::fs::read(crate_path.cache_path())?;
        let bytes = vm.alloc_constant(bytes);

        let read_context = Arc::new(PersistReadContext {
            this_crate,
            vm,
            types: OnceLock::new(),
            items: OnceLock::new(),
        });

        let mut reader = PersistReader::new(bytes, read_context.clone());
        persist_header_read(&mut reader)?;

        let crate_header = PersistCrateHeader::persist_read(&mut reader);
        crate_header.validate()?;

        if let Some(root_file) = crate_header.files.get(0) {
            if root_file.path != crate_path.source_path() {
                return Err("cache file refers to incorrect source file".into());
            }
        }

        let items = LazyTable::read(&mut reader);
        read_context
            .items
            .set(items)
            .map_err(|_| "double-assign to items")?;

        let impls = ImplTableLazy::new(&mut reader);

        let types = LazyArray::read(&mut reader);
        read_context
            .types
            .set(types)
            .map_err(|_| "double-assign to types")?;

        Ok(Self {
            read_context,
            impls,
        })
    }
}

impl<'vm> CrateProvider<'vm> for CacheProvider<'vm> {
    fn item_by_id(&self, id: ItemId) -> &'vm Item<'vm> {
        let items = self.read_context.items.get().unwrap();
        items.array.get(id.index())
    }

    fn item_by_path(&self, path: &ItemPath<'vm>) -> Option<&'vm Item<'vm>> {
        let items = self.read_context.items.get().unwrap();
        items.get(path).copied()
    }

    fn build_ir(&self, id: ItemId) -> Arc<IRFunction<'vm>> {
        let items = self.read_context.items.get().unwrap();
        let item = items.array.get(id.index());

        if let Some(saved_data) = item.saved_data {
            let mut reader = PersistReader::new(saved_data, self.read_context.clone());
            let ir = IRFunction::persist_read(&mut reader);
            Arc::new(ir)
        } else {
            panic!("no ir available for {:?}", id);
        }
    }

    fn build_adt(&self, id: ItemId) -> AdtInfo<'vm> {
        let items = self.read_context.items.get().unwrap();
        let item = items.array.get(id.index());

        if let Some(saved_info) = item.saved_data {
            let mut reader = PersistReader::new(saved_info, self.read_context.clone());
            AdtInfo::persist_read(&mut reader)
        } else {
            panic!("no adt info available for {:?}", id);
        }
    }

    fn trait_impl(&self, trait_item: &Item<'vm>, for_tys: &SubList<'vm>) -> Option<TraitImpl<'vm>> {
        self.impls.find_trait(trait_item, for_tys)
    }

    fn inherent_impl(&self, full_key: &str, ty: Type<'vm>) -> Option<AssocValue<'vm>> {
        self.impls.find_inherent(full_key, ty)
    }
}
