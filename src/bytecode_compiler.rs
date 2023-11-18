use crate::abi::POINTER_SIZE;
use crate::builtins::compile_rust_intrinsic;
use crate::bytecode_select;
use crate::closure::FnTrait;
use crate::ir::const_util::ConstStatus;
use crate::ir::{
    BinaryOp, BindingMode, Block, ExprId, ExprKind, IRFunction, LogicOp, LoopId, MatchGuard,
    Pattern, PatternId, PatternKind, PointerCast, Stmt,
};
use crate::items::FunctionAbi;
use crate::types::{ItemWithSubs, Mutability, SubList, Type, TypeKind};
use crate::vm::instr::Instr;
use crate::vm::{self, instr::Slot};

pub struct BytecodeCompiler<'vm, 'f> {
    in_func_subs: &'f SubList<'vm>,
    in_func: &'f IRFunction<'vm>,
    out_bc: Vec<Instr<'vm>>,
    vm: &'vm vm::VM<'vm>,
    stack: CompilerStack,
    locals: Vec<(u32, Slot)>,
    loops: Vec<LoopInfo>,
    loop_breaks: Vec<BreakInfo>,
    closure: Option<ClosureInfo<'vm>>,
}

struct ClosureInfo<'vm> {
    kind: FnTrait,
    self_slot: Slot,
    self_ty: Type<'vm>,
}

struct LoopInfo {
    loop_id: LoopId,
    result_slot: Slot,
    start_index: usize,
}

struct BreakInfo {
    loop_id: LoopId,
    break_index: usize,
}

impl<'vm, 'f> BytecodeCompiler<'vm, 'f> {
    pub fn compile(
        vm: &'vm vm::VM<'vm>,
        ir: &'f IRFunction<'vm>,
        subs: &'f SubList<'vm>,
        path: &str,
        original_subs: &'f SubList<'vm>,
    ) -> Vec<Instr<'vm>> {
        if vm.cli_args.verbose {
            println!("compiling {}{}", path, original_subs);
            //ir.print();
        }

        let mut compiler = BytecodeCompiler {
            in_func_subs: subs,
            in_func: &ir,
            out_bc: Vec::new(),
            vm,
            stack: Default::default(),
            locals: Vec::new(),
            loops: Vec::new(),
            loop_breaks: Vec::new(),
            closure: None,
        };

        let out_ty = compiler.apply_subs(ir.sig.output);

        let ret_slot = compiler.stack.alloc_no_drop(out_ty);
        assert_eq!(ret_slot.index(), 0);

        let body_scope = compiler.stack.push_scope("body");

        let param_slots: Vec<Slot> = ir
            .params
            .iter()
            .map(|pat| compiler.alloc_pattern(*pat))
            .collect();

        // setup closure
        if let Some(closure_kind) = ir.closure_kind {
            let self_ty_base = compiler.apply_subs(ir.pattern(ir.params[0]).ty);

            let self_ty = match closure_kind {
                FnTrait::Fn | FnTrait::FnMut => {
                    if let TypeKind::Ref(sub_ty, _) = self_ty_base.kind() {
                        *sub_ty
                    } else {
                        panic!("closure type assumption failed");
                    }
                }
                FnTrait::FnOnce => self_ty_base,
            };

            compiler.closure = Some(ClosureInfo {
                kind: closure_kind,
                self_slot: param_slots[0],
                self_ty,
            })
        }

        for (pat_id, slot) in ir.params.iter().zip(param_slots) {
            let pat = compiler.in_func.pattern(*pat_id);
            compiler.match_pattern_internal(pat, Place::Local(slot), true, None);
        }

        compiler.lower_expr(ir.root_expr, Some(Slot::new(0)));

        compiler.stack.pop_scope(body_scope);
        compiler.out_bc.push(Instr::Return);

        if vm.cli_args.verbose {
            for (i, bc) in compiler.out_bc.iter().enumerate() {
                println!("  {} {:?}", i, bc);
            }
            println!("<-");
        }

        //println!("{:?}",compiler.out_bc);
        compiler.out_bc
    }

    pub fn compile_promoted_const(
        vm: &'vm vm::VM<'vm>,
        ir: &'f IRFunction<'vm>,
        subs: &'f SubList<'vm>,
        root_expr: ExprId,
    ) -> (usize, Option<usize>) {
        if vm.cli_args.verbose {
            println!("compiling promoted const");
        }
        // NOTE: Currently we don't create an outer scope, so we possibly miss calling destructors for const values.
        // Unsure whether this will be a problem.

        let mut compiler = BytecodeCompiler {
            in_func_subs: subs,
            in_func: &ir,
            out_bc: Vec::new(),
            vm,
            stack: Default::default(),
            locals: Vec::new(),
            loops: Vec::new(),
            loop_breaks: Vec::new(),
            closure: None,
        };

        let place = compiler.expr_to_place(root_expr);
        compiler.out_bc.push(Instr::Return);

        if vm.cli_args.verbose {
            for (i, bc) in compiler.out_bc.iter().enumerate() {
                println!("  {} {:?}", i, bc);
            }
            println!("<-");
        }

        let const_thread = vm.make_thread();
        const_thread.run_bytecode(&compiler.out_bc, 0);

        match place {
            Place::Local(slot) => {
                let size = compiler.expr_ty(root_expr).layout().assert_size() as usize;
                let data = const_thread.copy_result(slot.index(), size);
                let ptr = vm.alloc_constant(data).as_ptr() as usize;
                (ptr, None)
            }
            Place::Ptr(slot, offset, ptr_kind) => {
                assert!(offset == 0);

                let ptr = const_thread.copy_ptr(slot);
                let meta = if ptr_kind.is_fat() {
                    Some(const_thread.copy_ptr(slot.offset_by(POINTER_SIZE.bytes() as i32)))
                } else {
                    None
                };

                (ptr, meta)
            }
        }
    }

    fn debug<S: Into<String>>(&mut self, f: impl Fn() -> S) {
        if self.vm.cli_args.verbose {
            let bc = Instr::Debug(Box::new(f().into()));
            self.out_bc.push(bc);
        }
    }

    fn lower_block(&mut self, block: &Block, dst_slot: Option<Slot>) -> Slot {
        // allocate result before entering scope
        let dst_slot = dst_slot.unwrap_or_else(|| {
            if let Some(expr) = block.result {
                self.stack.alloc(self.expr_ty(expr))
            } else {
                Slot::DUMMY
            }
        });

        let scope = self.stack.push_scope("block");

        for stmt in block.stmts.iter() {
            self.lower_stmt(stmt);
        }
        if let Some(expr) = block.result {
            // ALWAYS have a slot allocated for the final expression
            // prevents wrongly aliasing locals - TODO we may want to allocate this up-front for saner stack layout

            self.lower_expr(expr, Some(dst_slot));
        }

        self.stack.pop_scope(scope);

        dst_slot
    }

    fn lower_stmt(&mut self, stmt: &Stmt) {
        match stmt {
            Stmt::Let {
                pattern,
                init,
                else_block,
                ..
            } => {
                assert!(else_block.is_none());

                self.match_pattern_expr(*pattern, *init, None);
            }
            Stmt::Expr(expr) => {
                let stmt_scope = self.stack.push_scope("stmt");
                self.lower_expr(*expr, None);
                self.stack.pop_scope(stmt_scope);
            }
        }
    }

    fn lower_expr(&mut self, id: ExprId, dst_slot: Option<Slot>) -> Slot {
        let expr = self.in_func.expr(id);
        let expr_ty = self.expr_ty(id);

        if self.expr_is_place(id) {
            match self.expr_to_place(id) {
                Place::Local(local_slot) => {
                    if let Some(dst_slot) = dst_slot {
                        if let Some(instr) = bytecode_select::copy(dst_slot, local_slot, expr_ty) {
                            self.out_bc.push(instr);
                        }

                        dst_slot
                    } else {
                        local_slot
                    }
                }
                Place::Ptr(ptr_slot, offset, _) => {
                    let dst_slot = dst_slot.unwrap_or_else(|| self.stack.alloc(expr_ty));

                    if let Some(instr) =
                        bytecode_select::copy_from_ptr(dst_slot, ptr_slot, expr_ty, offset)
                    {
                        self.out_bc.push(instr);
                    }

                    dst_slot
                }
            }
        } else {
            match &expr.kind {
                ExprKind::Dummy(value) => self.lower_expr(*value, dst_slot),
                ExprKind::Block(block) => self.lower_block(block, dst_slot),
                ExprKind::LiteralValue(n) => {
                    let dst_slot = dst_slot.unwrap_or_else(|| self.stack.alloc(expr_ty));

                    self.out_bc.push(bytecode_select::literal(
                        *n,
                        expr_ty.layout().assert_size(),
                        dst_slot,
                    ));
                    dst_slot
                }
                ExprKind::LiteralVoid => dst_slot.unwrap_or_else(|| Slot::DUMMY),
                ExprKind::LiteralBytes(bytes) => {
                    let dst_slot = dst_slot.unwrap_or_else(|| self.stack.alloc(expr_ty));

                    let ptr = bytes.as_ptr() as usize;
                    let len = bytes.len();

                    let ptr_size = POINTER_SIZE.bytes();

                    self.out_bc
                        .push(bytecode_select::literal(ptr as i128, ptr_size, dst_slot));
                    self.out_bc.push(bytecode_select::literal(
                        len as i128,
                        ptr_size,
                        dst_slot.offset_by(ptr_size as i32),
                    ));

                    dst_slot
                }
                ExprKind::Unary(op, arg) => {
                    let dst_slot = dst_slot.unwrap_or_else(|| self.stack.alloc(expr_ty));

                    let arg_slot = self.lower_expr(*arg, None);

                    let arg_ty = self.expr_ty(*arg);

                    let ctor = bytecode_select::unary(*op, arg_ty);
                    self.out_bc.push(ctor(dst_slot, arg_slot));
                    dst_slot
                }
                ExprKind::Cast(source) => {
                    let arg_ty = self.expr_ty(*source);
                    let dst_ty = expr_ty;

                    // ALWAYS allocate a destination to avoid aliasing locals
                    let dst_slot = dst_slot.unwrap_or_else(|| self.stack.alloc(expr_ty));

                    let arg_slot = self.lower_expr(*source, None);

                    // Sometimes we will encounter a no-op cast.
                    if arg_ty == dst_ty {
                        self.out_bc
                            .push(bytecode_select::copy(dst_slot, arg_slot, arg_ty).unwrap());

                        dst_slot
                    } else {
                        let ctor = bytecode_select::cast(arg_ty, dst_ty);
                        self.out_bc.push(ctor(dst_slot, arg_slot));
                        dst_slot
                    }
                }
                ExprKind::Binary(op, lhs, rhs) => {
                    let dst_slot = dst_slot.unwrap_or_else(|| self.stack.alloc(expr_ty));

                    let lhs_slot = self.lower_expr(*lhs, None);
                    let rhs_slot = self.lower_expr(*rhs, None);

                    let lhs_ty = self.expr_ty(*lhs);

                    let (ctor, swap) = bytecode_select::binary(*op, lhs_ty);
                    if swap {
                        self.out_bc.push(ctor(dst_slot, rhs_slot, lhs_slot));
                    } else {
                        self.out_bc.push(ctor(dst_slot, lhs_slot, rhs_slot));
                    }
                    dst_slot
                }
                ExprKind::Assign(lhs, rhs) => {
                    let rhs_slot = self.lower_expr(*rhs, None);

                    let assign_ty = self.expr_ty(*lhs);

                    match self.expr_to_place(*lhs) {
                        Place::Local(lhs_slot) => {
                            if let Some(instr) =
                                bytecode_select::copy(lhs_slot, rhs_slot, assign_ty)
                            {
                                self.out_bc.push(instr);
                            }
                        }
                        Place::Ptr(ptr_slot, offset, _) => {
                            if let Some(instr) =
                                bytecode_select::copy_to_ptr(ptr_slot, rhs_slot, assign_ty, offset)
                            {
                                self.out_bc.push(instr);
                            }
                        }
                    }

                    // the destination is (), just return a dummy value
                    dst_slot.unwrap_or(Slot::DUMMY)
                }
                ExprKind::AssignOp(op, lhs, rhs) => {
                    let rhs_slot = self.lower_expr(*rhs, None);

                    let lhs_ty = self.expr_ty(*lhs);

                    let (ctor, swap) = bytecode_select::binary(*op, lhs_ty);
                    assert!(!swap);

                    match self.expr_to_place(*lhs) {
                        Place::Local(lhs_slot) => {
                            self.out_bc.push(ctor(lhs_slot, lhs_slot, rhs_slot));
                        }
                        Place::Ptr(ptr_slot, offset, _) => {
                            let assign_ty = self.expr_ty(*lhs);

                            let tmp_slot = self.stack.alloc(assign_ty);

                            self.out_bc.push(
                                bytecode_select::copy_from_ptr(
                                    tmp_slot, ptr_slot, assign_ty, offset,
                                )
                                .unwrap(),
                            );
                            self.out_bc.push(ctor(tmp_slot, tmp_slot, rhs_slot));
                            self.out_bc.push(
                                bytecode_select::copy_to_ptr(ptr_slot, tmp_slot, assign_ty, offset)
                                    .unwrap(),
                            );
                        }
                    }

                    // the destination is (), just return a dummy value
                    dst_slot.unwrap_or(Slot::DUMMY)
                }
                ExprKind::Call { func, args } => {
                    let ty = self.expr_ty(*func);
                    if let Some(func_ref) = ty.func_item() {
                        if let Some((FunctionAbi::RustIntrinsic, extern_name)) =
                            func_ref.item.get_extern()
                        {
                            // inline rust intrinsics
                            let dst_slot = dst_slot.unwrap_or_else(|| self.stack.alloc(expr_ty));

                            let arg_slots =
                                args.iter().map(|arg| self.lower_expr(*arg, None)).collect();

                            compile_rust_intrinsic(
                                extern_name,
                                &func_ref.subs,
                                self.vm,
                                &mut self.out_bc,
                                &mut self.stack,
                                arg_slots,
                                dst_slot,
                            );

                            dst_slot
                        } else {
                            let ret_slot = if let Some(vtable_index) =
                                func_ref.item.is_function_dyn(&func_ref.subs)
                            {
                                // special case for virtual functions

                                let (receiver_ptr, vtable_ptr) = {
                                    let arg_ty = self.expr_ty(args[0]);
                                    assert!(
                                        arg_ty.layout().assert_size() == POINTER_SIZE.bytes() * 2
                                    );
                                    let receiver = self.lower_expr(args[0], None);
                                    (receiver, receiver.offset_by(POINTER_SIZE.bytes() as i32))
                                };

                                let func_ptr = self.stack.alloc(self.vm.common_types().usize);

                                self.out_bc.push(Instr::VTableFunc(
                                    func_ptr,
                                    vtable_ptr,
                                    vtable_index,
                                ));

                                let ret_slot = self.build_call(expr_ty, args, Some(receiver_ptr));

                                self.out_bc.push(Instr::CallPtr {
                                    frame: ret_slot,
                                    func_ptr,
                                });

                                ret_slot
                            } else {
                                // normal function calls
                                let ret_slot = self.build_call(expr_ty, args, None);

                                let func = func_ref.item.func_mono(&func_ref.subs);
                                self.out_bc.push(Instr::Call(ret_slot, func));

                                ret_slot
                            };

                            if let Some(dst_slot) = dst_slot {
                                if let Some(instr) =
                                    bytecode_select::copy(dst_slot, ret_slot, expr_ty)
                                {
                                    self.out_bc.push(instr);
                                }
                                dst_slot
                            } else {
                                ret_slot
                            }
                        }
                    } else if let TypeKind::FunctionPointer(_) = ty.kind() {
                        let ret_slot = self.build_call(expr_ty, args, None);

                        let func_slot = self.lower_expr(*func, None);

                        self.out_bc.push(Instr::CallPtr {
                            frame: ret_slot,
                            func_ptr: func_slot,
                        });
                        if let Some(dst_slot) = dst_slot {
                            if let Some(instr) = bytecode_select::copy(dst_slot, ret_slot, expr_ty)
                            {
                                self.out_bc.push(instr);
                            }
                            dst_slot
                        } else {
                            ret_slot
                        }
                    } else {
                        panic!("bad call");
                    }
                }
                ExprKind::If {
                    cond,
                    then,
                    else_opt,
                    ..
                } => {
                    let cond_slot = self.lower_expr(*cond, None);

                    let jump_index_1 = self.skip_instr();

                    if let Some(else_expr) = else_opt {
                        let dst_slot = dst_slot.unwrap_or_else(|| self.stack.alloc(expr_ty));

                        self.lower_expr(*then, Some(dst_slot));

                        let jump_index_2 = self.skip_instr();

                        let jump_offset_1 = -self.get_jump_offset(jump_index_1);
                        self.out_bc[jump_index_1] = Instr::JumpF(jump_offset_1, cond_slot);

                        self.lower_expr(*else_expr, Some(dst_slot));

                        let jump_offset_2 = -self.get_jump_offset(jump_index_2);
                        self.out_bc[jump_index_2] = Instr::Jump(jump_offset_2);

                        dst_slot
                    } else {
                        // note: dst_slot should be void
                        let res = self.lower_expr(*then, dst_slot);

                        let jump_offset_1 = -self.get_jump_offset(jump_index_1);
                        self.out_bc[jump_index_1] = Instr::JumpF(jump_offset_1, cond_slot);

                        res
                    }
                }
                ExprKind::LogicOp(op, lhs, rhs) => {
                    let dst_slot = dst_slot.unwrap_or_else(|| self.stack.alloc(expr_ty));

                    self.lower_expr(*lhs, Some(dst_slot));

                    let jump_index_1 = self.skip_instr();

                    self.lower_expr(*rhs, Some(dst_slot));

                    let jump_offset_1 = -self.get_jump_offset(jump_index_1);

                    match op {
                        LogicOp::And => {
                            self.out_bc[jump_index_1] = Instr::JumpF(jump_offset_1, dst_slot);
                        }
                        LogicOp::Or => {
                            self.out_bc[jump_index_1] = Instr::JumpT(jump_offset_1, dst_slot);
                        }
                    }

                    dst_slot
                }
                ExprKind::Loop(body, loop_id) => {
                    let dst_slot = dst_slot.unwrap_or_else(|| self.stack.alloc(expr_ty));

                    let loop_index = self.out_bc.len();

                    self.loops.push(LoopInfo {
                        loop_id: *loop_id,
                        result_slot: dst_slot,
                        start_index: loop_index,
                    });
                    let loop_count = self.loops.len();

                    self.lower_block(body, Some(Slot::DUMMY));

                    assert_eq!(loop_count, self.loops.len());
                    self.loops.pop();

                    // ending jump
                    let offset = self.get_jump_offset(loop_index);
                    self.out_bc.push(Instr::Jump(offset));

                    // fix breaks
                    let break_indices: Vec<_> = self
                        .loop_breaks
                        .drain_filter(|break_info| break_info.loop_id == *loop_id)
                        .map(|break_info| break_info.break_index)
                        .collect();

                    for break_index in break_indices {
                        let offset = -self.get_jump_offset(break_index);
                        self.out_bc[break_index] = Instr::Jump(offset);
                    }

                    dst_slot
                }
                ExprKind::Break { loop_id, value } => {
                    if let Some(value) = value {
                        let loop_slot = self
                            .loops
                            .iter()
                            .find(|x| x.loop_id == *loop_id)
                            .unwrap()
                            .result_slot;
                        self.lower_expr(*value, Some(loop_slot));
                    }

                    let break_index = self.skip_instr(); //self.out_bc.len();
                    self.loop_breaks.push(BreakInfo {
                        loop_id: *loop_id,
                        break_index,
                    });

                    // the destination is (), just return a dummy value
                    dst_slot.unwrap_or(Slot::DUMMY)
                }
                ExprKind::Continue { loop_id } => {
                    let loop_start = self
                        .loops
                        .iter()
                        .find(|x| x.loop_id == *loop_id)
                        .unwrap()
                        .start_index;

                    let offset = self.get_jump_offset(loop_start); // + 1;
                    self.out_bc.push(Instr::Jump(offset));

                    // the destination is (), just return a dummy value
                    dst_slot.unwrap_or(Slot::DUMMY)
                }
                ExprKind::Return(value) => {
                    if let Some(value) = value {
                        self.lower_expr(*value, Some(Slot::new(0)));
                    }
                    self.out_bc.push(Instr::Return);

                    // the destination is (), just return a dummy value
                    dst_slot.unwrap_or(Slot::DUMMY)
                }
                ExprKind::Ref(arg, ref_mutability) => {
                    let arg_ty = self.expr_ty(*arg);

                    let const_promote = *ref_mutability == Mutability::Const
                        && self.in_func.const_status(*arg) == ConstStatus::CanPromote;

                    let place = if const_promote {
                        let (ptr, ptr_meta) = BytecodeCompiler::compile_promoted_const(
                            self.vm,
                            self.in_func,
                            self.in_func_subs,
                            *arg,
                        );

                        let dst_slot = dst_slot.unwrap_or_else(|| self.stack.alloc(expr_ty));

                        let ptr_size = POINTER_SIZE.bytes();

                        self.out_bc
                            .push(bytecode_select::literal(ptr as _, ptr_size, dst_slot));

                        if let Some(ptr_meta) = ptr_meta {
                            self.out_bc.push(bytecode_select::literal(
                                ptr_meta as _,
                                ptr_size,
                                dst_slot.offset_by(ptr_size as i32),
                            ));
                        }

                        return dst_slot;
                    } else {
                        self.expr_to_place(*arg)
                    };

                    match place {
                        Place::Local(src_slot) => {
                            let dst_slot = dst_slot.unwrap_or_else(|| self.stack.alloc(expr_ty));

                            self.out_bc.push(Instr::SlotAddr(dst_slot, src_slot));
                            // unsized types not a problem (can't appear on stack)
                            dst_slot
                        }
                        Place::Ptr(src_slot, offset, ptr_kind) => {
                            let dst_size = expr_ty.layout().assert_size();
                            let is_dst_fat = dst_size == POINTER_SIZE.bytes() * 2;

                            let is_mut =
                                *ref_mutability == Mutability::Mut || arg_ty.is_interior_mut();
                            let copy_alloc = is_mut && self.in_func.is_const_alloc(*arg);

                            if copy_alloc {
                                // We are mutably referencing a const allocation. It must be copied!

                                // allocation copying and const promotion should be mutually exclusive
                                assert!(!const_promote);

                                // todo?!?
                                assert!(!is_dst_fat);

                                let copy_slot = self.stack.alloc(arg_ty);

                                if let Some(copy_bc) = bytecode_select::copy_from_ptr(
                                    copy_slot, src_slot, arg_ty, offset,
                                ) {
                                    self.out_bc.push(copy_bc);
                                }

                                let dst_slot =
                                    dst_slot.unwrap_or_else(|| self.stack.alloc(expr_ty));

                                self.out_bc.push(Instr::SlotAddr(dst_slot, copy_slot));

                                dst_slot
                            } else {
                                if offset != 0 {
                                    let dst_slot =
                                        dst_slot.unwrap_or_else(|| self.stack.alloc(expr_ty));

                                    self.out_bc
                                        .push(Instr::PointerOffset2(dst_slot, src_slot, offset));

                                    if is_dst_fat {
                                        assert!(ptr_kind.is_fat());
                                        let ptr_size = POINTER_SIZE.bytes();
                                        let usize_ty = self.vm.common_types().usize;

                                        assert!(expr_ty.layout().assert_size() == ptr_size * 2);
                                        self.out_bc.push(
                                            bytecode_select::copy(
                                                dst_slot.offset_by(ptr_size as i32),
                                                src_slot.offset_by(ptr_size as i32),
                                                usize_ty,
                                            )
                                            .unwrap(),
                                        );
                                    }

                                    dst_slot
                                } else {
                                    if let Some(dst_slot) = dst_slot {
                                        self.out_bc.push(
                                            bytecode_select::copy(dst_slot, src_slot, expr_ty)
                                                .unwrap(),
                                        );
                                        if is_dst_fat {
                                            assert!(ptr_kind.is_fat());
                                        }

                                        dst_slot
                                    } else {
                                        src_slot
                                    }
                                }
                            }
                        }
                    }
                }
                ExprKind::PointerCast(source, cast) => {
                    match cast {
                        PointerCast::UnSize => {
                            assert_eq!(expr_ty.layout().assert_size(), POINTER_SIZE.bytes() * 2);

                            let mut src_ty = self
                                .expr_ty(*source)
                                .try_get_ref_ty()
                                .expect("pointer cast: no src ref");

                            let mut dst_ty =
                                expr_ty.try_get_ref_ty().expect("pointer cast: no dst ref");

                            // handle structs by finding the changed element in their sub list
                            // this is NOT super robust, but should be good enough for our purposes
                            {
                                match (src_ty.kind(), dst_ty.kind()) {
                                    (TypeKind::Adt(src_item), TypeKind::Adt(dst_item)) => {
                                        assert!(src_item.item == dst_item.item);

                                        let mut changed_index = None;

                                        for (i, (a, b)) in src_item
                                            .subs
                                            .list
                                            .iter()
                                            .zip(&dst_item.subs.list)
                                            .enumerate()
                                        {
                                            if a != b {
                                                assert!(changed_index.is_none());
                                                changed_index = Some(i);
                                            }
                                        }

                                        let changed_index =
                                            changed_index.expect("no single changed index");

                                        src_ty = src_item.subs.list[changed_index].assert_ty();
                                        dst_ty = dst_item.subs.list[changed_index].assert_ty();
                                    }
                                    _ => (),
                                }
                            }

                            let meta = match (src_ty.kind(), dst_ty.kind()) {
                                (TypeKind::Array(_, elem_count), TypeKind::Slice(_)) => {
                                    elem_count.get_value() as usize
                                }
                                (
                                    _,
                                    TypeKind::Dynamic {
                                        primary_trait,
                                        is_dyn_star,
                                        ..
                                    },
                                ) => {
                                    let primary_trait =
                                        primary_trait.as_ref().expect("no primary trait");
                                    assert!(!is_dyn_star);
                                    assert!(primary_trait.subs.list.len() == 0);

                                    if let TypeKind::Dynamic {
                                        primary_trait: src_primary_trait,
                                        is_dyn_star: src_is_dyn_star,
                                        ..
                                    } = src_ty.kind()
                                    {
                                        // cast dyn to dyn -- currently we only handle the trivial case
                                        assert_eq!(src_is_dyn_star, is_dyn_star);
                                        assert_eq!(src_primary_trait.as_ref(), Some(primary_trait));

                                        let dst_slot =
                                            dst_slot.unwrap_or_else(|| self.stack.alloc(expr_ty));

                                        let src_slot = self.lower_expr(*source, None);

                                        self.out_bc.push(
                                            bytecode_select::copy(dst_slot, src_slot, expr_ty)
                                                .unwrap(),
                                        );

                                        return dst_slot;
                                    } else {
                                        let vtable =
                                            self.vm.find_vtable(primary_trait.item, src_ty);

                                        vtable as *const _ as usize
                                    }
                                }
                                (_, _) => {
                                    panic!("unsized cast from {} to {}", src_ty, dst_ty);
                                }
                            };

                            /*let meta = match src_kind {
                                TypeKind::Array(_, elem_count) => elem_count.assert_static() as usize,
                                _ => panic!("unsized cast ptr to {:?}", src_kind),
                            };*/

                            let dst_slot = dst_slot.unwrap_or_else(|| self.stack.alloc(expr_ty));

                            let src_slot = self.lower_expr(*source, None);

                            // copy base
                            let ptr_size = POINTER_SIZE.bytes();
                            self.out_bc.push(
                                bytecode_select::copy(
                                    dst_slot,
                                    src_slot,
                                    self.vm.common_types().usize,
                                )
                                .unwrap(),
                            );
                            self.out_bc.push(bytecode_select::literal(
                                meta as i128,
                                ptr_size,
                                dst_slot.offset_by(ptr_size as i32),
                            ));

                            dst_slot
                        }
                        PointerCast::MutToConstPointer => {
                            // a simple copy
                            let dst_slot = dst_slot.unwrap_or_else(|| self.stack.alloc(expr_ty));

                            let src_slot = self.lower_expr(*source, None);

                            self.out_bc
                                .push(bytecode_select::copy(dst_slot, src_slot, expr_ty).unwrap());

                            dst_slot
                        }
                        PointerCast::ReifyFnPointer => {
                            let src_ty = self.expr_ty(*source);

                            if let Some(func_ref) = src_ty.func_item() {
                                let dst_slot =
                                    dst_slot.unwrap_or_else(|| self.stack.alloc(expr_ty));

                                let func_ptr = func_ref.item.func_mono(&func_ref.subs) as *const _;

                                self.out_bc.push(bytecode_select::literal(
                                    func_ptr as i128,
                                    POINTER_SIZE.bytes(),
                                    dst_slot,
                                ));
                                dst_slot
                            } else {
                                panic!("cannot reify function: {}", src_ty)
                            }
                        }
                        // similar to the above
                        PointerCast::ClosureFnPointer => {
                            let src_ty = self.expr_ty(*source);
                            if let TypeKind::Closure(closure, subs) = src_ty.kind() {
                                let dst_slot =
                                    dst_slot.unwrap_or_else(|| self.stack.alloc(expr_ty));

                                assert!(subs.is_concrete());

                                let func_ptr = closure.func_mono(subs) as *const _;

                                self.out_bc.push(bytecode_select::literal(
                                    func_ptr as i128,
                                    POINTER_SIZE.bytes(),
                                    dst_slot,
                                ));
                                dst_slot
                            } else {
                                panic!("cannot convert closure to function pointer: {}", src_ty)
                            }
                        }
                        _ => panic!("todo ptr cast {:?}", cast),
                    }
                }
                ExprKind::Tuple(fields) => {
                    let dst_slot = dst_slot.unwrap_or_else(|| self.stack.alloc(expr_ty));

                    for (field, offset) in fields
                        .iter()
                        .zip(expr_ty.layout().field_offsets.assert_single().iter())
                    {
                        let field_slot = dst_slot.offset_by(*offset as i32);
                        self.lower_expr(*field, Some(field_slot));
                    }

                    dst_slot
                }
                ExprKind::Array(fields) => {
                    let TypeKind::Array(elem_ty,_) = expr_ty.kind() else {
                        panic!("bad array layout");
                    };

                    let elem_size = elem_ty.layout().assert_size();

                    let dst_slot = dst_slot.unwrap_or_else(|| self.stack.alloc(expr_ty));

                    for (i, field) in fields.iter().enumerate() {
                        let field_slot = dst_slot.offset_by(i as i32 * elem_size as i32);
                        self.lower_expr(*field, Some(field_slot));
                    }

                    dst_slot
                }
                ExprKind::ArrayRepeat(arg, size) => {
                    let count = size
                        .sub(self.in_func_subs, self.vm.common_types().usize)
                        .get_value() as u32;

                    let elem_size = self.expr_ty(*arg).layout().assert_size();

                    let dst_slot = dst_slot.unwrap_or_else(|| self.stack.alloc(expr_ty));

                    self.lower_expr(*arg, Some(dst_slot));

                    self.out_bc.push(Instr::ArrayRepeat {
                        base: dst_slot,
                        size: elem_size,
                        count,
                    });

                    dst_slot
                }
                ExprKind::Adt {
                    variant,
                    fields,
                    rest,
                } => {
                    let dst_slot = dst_slot.unwrap_or_else(|| self.stack.alloc(expr_ty));

                    let expr_layout = expr_ty.layout();

                    if let Some(enum_info) = expr_ty.adt_info().enum_info() {
                        let disc = expr_layout.field_offsets.get_discriminant(*variant);

                        if let Some(disc_val) = disc.value() {
                            let dl = enum_info.discriminant_internal.layout();
                            self.out_bc.push(bytecode_select::literal(
                                disc_val,
                                dl.assert_size(),
                                dst_slot,
                            ));
                        }
                    }

                    for (field_index, expr) in fields.iter() {
                        let offset = expr_layout.field_offsets.get(*variant)[*field_index as usize];
                        let field_slot = dst_slot.offset_by(offset as i32);
                        self.lower_expr(*expr, Some(field_slot));
                    }

                    // TODO this will need careful handling for drop
                    if let Some(rest) = rest {
                        let rest_ty = self.expr_ty(*rest);
                        assert!(expr_ty == rest_ty);

                        let rest_source = self.lower_expr(*rest, None);

                        // YUCK: we need to get the exact types of all the struct's fields
                        let TypeKind::Adt(ItemWithSubs{item,subs: adt_subs}) = expr_ty.kind() else {
                            panic!("non-adt in adt-rest compile");
                        };

                        let adt_info = item.adt_info();
                        let adt_subs = adt_subs.sub(self.in_func_subs);

                        let all_offsets = expr_layout.field_offsets.get(*variant);
                        let all_fields = adt_info.variant_fields.get(*variant);

                        for (field_n, (field_ty, field_offset)) in
                            all_fields.iter().zip(all_offsets).enumerate()
                        {
                            if fields.iter().all(|(f, _)| *f != field_n as u32) {
                                let field_ty = field_ty.sub(&adt_subs);
                                let field_src_slot = rest_source.offset_by(*field_offset as i32);
                                let field_dst_slot = dst_slot.offset_by(*field_offset as i32);

                                let copy_instr =
                                    bytecode_select::copy(field_dst_slot, field_src_slot, field_ty);

                                if let Some(copy_instr) = copy_instr {
                                    self.out_bc.push(copy_instr);
                                }
                            }
                        }
                    }

                    dst_slot
                }
                ExprKind::Let { pattern, init } => {
                    // the bool result
                    let dst_slot = dst_slot.unwrap_or_else(|| self.stack.alloc(expr_ty));

                    self.match_pattern_expr(*pattern, Some(*init), Some(dst_slot));

                    dst_slot
                }
                ExprKind::Match { arg, arms } => {
                    // the result value
                    let dst_slot = dst_slot.unwrap_or_else(|| self.stack.alloc(expr_ty));

                    // the bool pattern match result slot
                    let bool_ty = self.vm.common_types().bool;
                    let match_result_slot = self.stack.alloc(bool_ty);

                    let source = self.expr_to_place(*arg);

                    let mut jump_gaps = Vec::new();

                    for arm in arms {
                        self.match_pattern_place(arm.pattern, source, Some(match_result_slot));
                        let check_end_index = self.skip_instr();

                        // guard clause
                        let guard_end_index = if let MatchGuard::If(guard) = arm.guard {
                            self.lower_expr(guard, Some(match_result_slot));
                            Some(self.skip_instr())
                        } else if let MatchGuard::IfLet = arm.guard {
                            panic!("if-let match arm");
                        } else {
                            None
                        };

                        self.lower_expr(arm.body, Some(dst_slot));
                        jump_gaps.push(self.skip_instr());

                        self.out_bc[check_end_index] =
                            Instr::JumpF(-self.get_jump_offset(check_end_index), match_result_slot);

                        if let Some(guard_end_index) = guard_end_index {
                            self.out_bc[guard_end_index] = Instr::JumpF(
                                -self.get_jump_offset(guard_end_index),
                                match_result_slot,
                            );
                        }
                    }

                    for gap_index in jump_gaps {
                        self.out_bc[gap_index] = Instr::Jump(-self.get_jump_offset(gap_index));
                    }

                    dst_slot
                }
                ExprKind::ConstParam(n) => {
                    // the result value
                    let dst_slot = dst_slot.unwrap_or_else(|| self.stack.alloc(expr_ty));

                    let param = self
                        .in_func_subs
                        .list
                        .get(*n as usize)
                        .expect("can't find const param");
                    let c = param.assert_const(expr_ty);
                    let val = c.get_value();
                    self.out_bc.push(bytecode_select::literal(
                        val,
                        expr_ty.layout().assert_size(),
                        dst_slot,
                    ));

                    dst_slot
                }
                _ => panic!("todo lower expr {:?}", expr.kind),
            }
        }
    }

    fn expr_is_place(&self, id: ExprId) -> bool {
        let expr = self.in_func.expr(id);

        match &expr.kind {
            ExprKind::Dummy(value) => self.expr_is_place(*value),
            ExprKind::DeRef(_)
            | ExprKind::VarRef(_)
            | ExprKind::UpVar(_)
            | ExprKind::Field { .. }
            | ExprKind::Index { .. }
            | ExprKind::ConstBlock(_)
            | ExprKind::NamedConst(_)
            | ExprKind::Static(_) => true,

            ExprKind::Block { .. }
            | ExprKind::Tuple { .. }
            | ExprKind::Binary { .. }
            | ExprKind::Unary { .. }
            | ExprKind::AssignOp(..)
            | ExprKind::Assign(..)
            | ExprKind::LogicOp(..)
            | ExprKind::Ref { .. }
            | ExprKind::Array(_)
            | ExprKind::Cast(_)
            | ExprKind::PointerCast(..)
            | ExprKind::Adt { .. }
            | ExprKind::Call { .. }
            | ExprKind::ArrayRepeat(..)
            | ExprKind::Let { .. }
            | ExprKind::If { .. }
            | ExprKind::Match { .. }
            | ExprKind::Loop(..)
            | ExprKind::Break { .. }
            | ExprKind::Continue { .. }
            | ExprKind::Return(..)
            | ExprKind::ConstParam(_)
            | ExprKind::LiteralBytes(..)
            | ExprKind::LiteralVoid
            | ExprKind::LiteralValue { .. } => false,

            _ => panic!("expr_is_place {:?}", expr.kind),
        }
    }

    fn expr_to_place(&mut self, id: ExprId) -> Place {
        let expr = self.in_func.expr(id);

        if self.expr_is_place(id) {
            match &expr.kind {
                ExprKind::Dummy(value) => self.expr_to_place(*value),
                ExprKind::DeRef(arg) => {
                    let arg_ty = self.expr_ty(id);
                    let ptr_kind = if arg_ty.is_sized() {
                        PointerKind::Thin
                    } else {
                        PointerKind::Fat
                    };
                    let addr_slot = self.lower_expr(*arg, None);
                    Place::Ptr(addr_slot, 0, ptr_kind)
                }
                ExprKind::VarRef(local_id) => {
                    let local_slot = self.find_local(*local_id);
                    Place::Local(local_slot)
                }
                ExprKind::UpVar(upvar) => {
                    let closure = self.closure.as_ref().expect("upvar in non-closure");

                    let field_offset = closure.self_ty.layout().field_offsets.assert_single()
                        [upvar.index as usize] as i32;

                    match closure.kind {
                        // self is a ref to a tuple-like env
                        FnTrait::Fn | FnTrait::FnMut => {
                            let ref_ty = expr.ty.ref_to(Mutability::Const);

                            // assert thin pointer is correct
                            assert!(ref_ty.layout().assert_size() == POINTER_SIZE.bytes());

                            if upvar.is_ref {
                                let upvar_ref = self.stack.alloc(ref_ty);

                                self.out_bc.push(
                                    bytecode_select::copy_from_ptr(
                                        upvar_ref,
                                        closure.self_slot,
                                        ref_ty,
                                        field_offset,
                                    )
                                    .unwrap(),
                                );
                                Place::Ptr(upvar_ref, 0, PointerKind::Thin)
                            } else {
                                Place::Ptr(closure.self_slot, field_offset, PointerKind::Thin)
                            }
                        }
                        // self is a tuple-like env (not a ref)
                        FnTrait::FnOnce => {
                            let field_slot = closure.self_slot.offset_by(field_offset);
                            if upvar.is_ref {
                                // stored inline, must be a thin pointer
                                Place::Ptr(field_slot, 0, PointerKind::Thin)
                            } else {
                                Place::Local(field_slot)
                            }
                        }
                    }
                }
                ExprKind::Field {
                    lhs,
                    variant,
                    field,
                } => {
                    // can we ever use a field with an enum?
                    assert!(variant.index() == 0);

                    let layout = self.expr_ty(*lhs).layout();
                    let field_offset = layout.field_offsets.get(*variant)[*field as usize];

                    self.expr_to_place(*lhs).offset_by(field_offset as i32)
                }
                ExprKind::Index { lhs, index } => {
                    let index_ty = self.expr_ty(*index);
                    assert!(index_ty.layout().assert_size() == POINTER_SIZE.bytes());

                    // this index slot is re-used for the resulting pointer
                    let index_slot = self.stack.alloc(index_ty);
                    self.lower_expr(*index, Some(index_slot));

                    let lhs_ty = self.expr_ty(*lhs);
                    let lhs_kind = lhs_ty.kind();
                    match lhs_kind {
                        TypeKind::Array(elem_ty, elem_count) => {
                            let elem_size = elem_ty.layout().assert_size();
                            let elem_count = elem_count.get_value() as u32;

                            self.out_bc.push(Instr::IndexCalc {
                                arg_out: index_slot,
                                elem_size,
                                elem_count,
                            });

                            match self.expr_to_place(*lhs) {
                                Place::Local(base_slot) => {
                                    self.out_bc.push(Instr::SlotAddrOffset {
                                        out: index_slot,
                                        arg: base_slot,
                                        offset: index_slot,
                                    });
                                }
                                Place::Ptr(ptr_slot, offset, _) => {
                                    self.out_bc
                                        .push(Instr::PointerOffset3(index_slot, ptr_slot, offset));
                                }
                            }
                        }
                        TypeKind::Slice(elem_ty) => {
                            let elem_size = elem_ty.layout().assert_size();

                            let lhs_place = self.expr_to_place(*lhs);
                            if let Place::Ptr(ptr_slot, offset, ptr_kind) = lhs_place {
                                assert!(ptr_kind.is_fat());
                                assert_eq!(offset, 0);
                                let elem_count = ptr_slot.offset_by(POINTER_SIZE.bytes() as i32);
                                self.out_bc.push(Instr::IndexCalcDyn {
                                    arg_out: index_slot,
                                    elem_size,
                                    elem_count,
                                });
                                self.out_bc
                                    .push(Instr::PointerOffset3(index_slot, ptr_slot, 0));
                            } else {
                                panic!("invalid local(?!) slice index");
                            }
                        }
                        _ => panic!("cannot index {} with {}", lhs_ty, index_ty),
                    }
                    // must be a thin pointer, stored inline in array
                    Place::Ptr(index_slot, 0, PointerKind::Thin)
                }
                ExprKind::ConstBlock(const_ir) => {
                    let bc = BytecodeCompiler::compile(
                        self.vm,
                        &const_ir,
                        self.in_func_subs,
                        "<const block>",
                        self.in_func_subs,
                    );

                    let eval_thread = self.vm.make_thread();
                    eval_thread.run_bytecode(&bc, 0);

                    let ty = const_ir.sig.output; // todo sub?
                    let ptr_ty = ty.ref_to(Mutability::Const);
                    let ptr_size = ptr_ty.layout().assert_size();

                    let eval_bytes = eval_thread.copy_result(0, ty.layout().assert_size() as usize);
                    let const_ptr = self.vm.alloc_constant(eval_bytes).as_ptr() as usize;

                    let ptr_slot = self.stack.alloc(ptr_ty);
                    self.out_bc.push(bytecode_select::literal(
                        const_ptr as i128,
                        ptr_size,
                        ptr_slot,
                    ));

                    // TODO unsized?
                    assert!(ptr_size == POINTER_SIZE.bytes());
                    Place::Ptr(ptr_slot, 0, PointerKind::Thin)
                }
                ExprKind::NamedConst(const_ref) => {
                    let ty = self.expr_ty(id);
                    let ptr_ty = ty.ref_to(Mutability::Const);
                    let ptr_size = ptr_ty.layout().assert_size();

                    let fixed_subs = const_ref.subs.sub(&self.in_func_subs);

                    let const_ptr = const_ref.item.const_value(&fixed_subs).as_ptr() as usize;

                    let ptr_slot = self.stack.alloc(ptr_ty);
                    self.out_bc.push(bytecode_select::literal(
                        const_ptr as i128,
                        ptr_size,
                        ptr_slot,
                    ));

                    // TODO unsized?
                    assert!(ptr_size == POINTER_SIZE.bytes());
                    Place::Ptr(ptr_slot, 0, PointerKind::Thin)
                }
                ExprKind::Static(static_ref) => {
                    let ty = self.expr_ty(id);
                    let ptr_ty = ty.ref_to(Mutability::Const);
                    let ptr_size = ptr_ty.layout().assert_size();

                    let static_ptr = self.vm.static_value(static_ref) as i128;

                    let ptr_slot = self.stack.alloc(ptr_ty);
                    self.out_bc
                        .push(bytecode_select::literal(static_ptr, ptr_size, ptr_slot));

                    // TODO unsized?
                    assert!(ptr_size == POINTER_SIZE.bytes());
                    Place::Ptr(ptr_slot, 0, PointerKind::Thin)
                }
                _ => panic!("expr_to_place {:?}", expr.kind),
            }
        } else {
            // WARNING: be wary of expressions that could alias locals! (???)
            let res = self.lower_expr(id, None);
            Place::Local(res)
        }
    }

    /// `override_arg_0` MUST be a thin pointer if present.
    fn build_call(&mut self, res_ty: Type, args: &[ExprId], override_arg_0: Option<Slot>) -> Slot {
        // this is annoying and not very performant, but it's the least buggy way to build call frames I've found

        // start by evaluating the args
        let args_src: Vec<_> = args
            .iter()
            .enumerate()
            .map(|(i, arg)| {
                if i == 0 {
                    if let Some(override_arg_0) = override_arg_0 {
                        return override_arg_0;
                    }
                }
                self.lower_expr(*arg, None)
            })
            .collect();

        // allocate return slot
        let base_slot = self.stack.align_for_call();
        let ret_slot = self.stack.alloc(res_ty);
        assert_eq!(ret_slot, base_slot);

        // allocate arg slots
        let args_dst: Vec<_> = args
            .iter()
            .enumerate()
            .map(|(i, arg)| {
                let arg_ty = if i == 0 && override_arg_0.is_some() {
                    self.vm.common_types().usize
                } else {
                    self.expr_ty(*arg)
                };

                (self.stack.alloc(arg_ty), arg_ty)
            })
            .collect();

        // copy args to their final slots
        for (src, (dst, ty)) in args_src.into_iter().zip(args_dst) {
            if let Some(copy) = bytecode_select::copy(dst, src, ty) {
                self.out_bc.push(copy);
            }
        }

        ret_slot
    }

    fn apply_subs(&self, ty: Type<'vm>) -> Type<'vm> {
        let res = ty.sub(self.in_func_subs);
        //println!("sub {} with {} => {}",ty,self.in_func_subs,res);
        res
    }

    fn expr_ty(&self, id: ExprId) -> Type<'vm> {
        let ty = self.in_func.expr(id).ty;
        self.apply_subs(ty)
    }

    fn alloc_pattern(&mut self, pat_id: PatternId) -> Slot {
        let pat = self.in_func.pattern(pat_id);

        let ty = self.apply_subs(pat.ty);
        let slot = self.stack.alloc(ty);
        slot
    }

    fn match_pattern_place(
        &mut self,
        pat_id: PatternId,
        source: Place,
        match_result_slot: Option<Slot>,
    ) {
        let pat = self.in_func.pattern(pat_id);

        let can_alias = false;

        let refutable = self.match_pattern_internal(pat, source, can_alias, match_result_slot);

        if let Some(match_result_slot) = match_result_slot {
            if !refutable {
                self.out_bc.push(Instr::I8_Const(match_result_slot, 1));
            }
        }
    }

    /// WARNING: This lowers the expression -- do NOT use it repeatedly for match cases
    fn match_pattern_expr(
        &mut self,
        pat_id: PatternId,
        init_id: Option<ExprId>,
        match_result_slot: Option<Slot>,
    ) {
        let (source, can_alias) = if let Some(init_id) = init_id {
            let is_place = self.expr_is_place(init_id);
            (self.expr_to_place(init_id), !is_place)
        } else {
            let slot = self.alloc_pattern(pat_id);
            (Place::Local(slot), true)
        };

        let pat = self.in_func.pattern(pat_id);

        let refutable = self.match_pattern_internal(pat, source, can_alias, match_result_slot);

        if let Some(match_result_slot) = match_result_slot {
            if !refutable {
                self.out_bc.push(Instr::I8_Const(match_result_slot, 1));
            }
        }
    }

    /// If can_alias is false, we must copy values from the source. Otherwise we are free to re-use locals.
    /// The returned value indicates whether the pattern is refutable.
    /// If not, match_result_slot will NOT be written to, and the caller must assume the match was successful
    fn match_pattern_internal(
        &mut self,
        pat: &Pattern<'vm>,
        source: Place,
        can_alias: bool,
        match_result_slot: Option<Slot>,
    ) -> bool {
        match &pat.kind {
            PatternKind::LocalBinding {
                local_id,
                mode,
                sub_pattern,
            } => {
                match (mode, source) {
                    (BindingMode::Value, Place::Local(source_slot)) => {
                        // copy values if there are possible aliases
                        if !can_alias || sub_pattern.is_some() {
                            let ty = self.apply_subs(pat.ty);
                            let var_slot = self.find_or_alloc_local(*local_id, ty);
                            if let Some(instr) = bytecode_select::copy(var_slot, source_slot, ty) {
                                self.out_bc.push(instr);
                            }
                        } else {
                            self.assert_local_undef(*local_id);
                            self.locals.push((*local_id, source_slot));
                        }
                    }
                    (BindingMode::Value, Place::Ptr(ref_slot, ref_offset, ptr_kind)) => {
                        assert!(ptr_kind.is_thin());
                        // copy value from pointer
                        let ty = self.apply_subs(pat.ty);
                        let var_slot = self.find_or_alloc_local(*local_id, ty);
                        if let Some(instr) =
                            bytecode_select::copy_from_ptr(var_slot, ref_slot, ty, ref_offset)
                        {
                            self.out_bc.push(instr);
                        }
                    }
                    (BindingMode::Ref, Place::Local(source_slot)) => {
                        // ref to local
                        let ref_ty = self.apply_subs(pat.ty);
                        let var_slot = self.find_or_alloc_local(*local_id, ref_ty);

                        self.out_bc.push(Instr::SlotAddr(var_slot, source_slot));
                    }
                    (BindingMode::Ref, Place::Ptr(ref_slot, ref_offset, ptr_kind)) => {
                        // offset pointer
                        let ref_ty = self.apply_subs(pat.ty);
                        let var_slot = self.find_or_alloc_local(*local_id, ref_ty);

                        self.out_bc
                            .push(Instr::PointerOffset2(var_slot, ref_slot, ref_offset));

                        let ref_size = ref_ty.layout().assert_size();
                        let ptr_size = POINTER_SIZE.bytes();

                        // assert the RESULT POINTER is thin
                        if ref_size != ptr_size {
                            assert!(ptr_kind.is_fat());
                            panic!(".");
                        }
                        /*if ptr_kind.is_fat() {
                            println!("??? {}",ref_ty);
                            let usize_ty = self.vm.common_types().usize;

                            assert!(ref_ty.layout().assert_size() == ptr_size * 2);
                            self.out_bc.push(
                                bytecode_select::copy(
                                    var_slot.offset_by(ptr_size as i32),
                                    ref_slot.offset_by(ptr_size as i32),
                                    usize_ty,
                                )
                                .unwrap(),
                            );
                        }*/
                    }
                }
                if let Some(sub_pattern) = sub_pattern {
                    // copy values if there are possible aliases
                    let sub_pattern = self.in_func.pattern(*sub_pattern);
                    self.match_pattern_internal(sub_pattern, source, false, match_result_slot)
                } else {
                    false
                }
            }
            PatternKind::Struct { fields } => {
                let layout = self.apply_subs(pat.ty).layout();

                let mut jump_gaps = Vec::new();
                let mut result = false;

                for field in fields {
                    let field_pattern = self.in_func.pattern(field.pattern);

                    let offset = layout.field_offsets.assert_single()[field.field as usize];
                    let refutable = self.match_pattern_internal(
                        field_pattern,
                        source.offset_by(offset as i32),
                        can_alias,
                        match_result_slot,
                    );
                    result |= refutable;

                    if refutable && match_result_slot.is_some() {
                        jump_gaps.push(self.skip_instr());
                    }
                }

                // cut out unnecessary jumps
                if let Some(last_gap) = jump_gaps.last() {
                    if *last_gap == self.out_bc.len() - 1 {
                        jump_gaps.pop();
                        self.out_bc.pop();
                    }
                }

                if let Some(match_result_slot) = match_result_slot {
                    for gap_index in jump_gaps.iter() {
                        let gap_offset = -self.get_jump_offset(*gap_index);
                        self.out_bc[*gap_index] = Instr::JumpF(gap_offset, match_result_slot);
                    }
                }

                result
            }
            PatternKind::Enum {
                fields,
                variant_index,
            } => {
                let layout = self.apply_subs(pat.ty).layout();

                let mut jump_gaps = Vec::new();

                // test the discriminator by building a fake literal pattern
                {
                    let discriminant_ty = pat
                        .ty
                        .adt_info()
                        .enum_info()
                        .expect("not an enum?")
                        .discriminant_internal;

                    let discriminant = layout.field_offsets.get_discriminant(*variant_index);

                    let Some(disc_val) = discriminant.value() else {
                        panic!("no discriminant!");
                    };

                    let fake_pattern = Pattern {
                        kind: PatternKind::LiteralValue(disc_val),
                        ty: discriminant_ty,
                    };
                    self.match_pattern_internal(
                        &fake_pattern,
                        source,
                        can_alias,
                        match_result_slot,
                    );
                    jump_gaps.push(self.skip_instr());
                }

                for field in fields {
                    let field_pattern = self.in_func.pattern(field.pattern);

                    let offset = layout.field_offsets.get(*variant_index)[field.field as usize];

                    let refutable = self.match_pattern_internal(
                        field_pattern,
                        source.offset_by(offset as i32),
                        can_alias,
                        match_result_slot,
                    );

                    if refutable && match_result_slot.is_some() {
                        jump_gaps.push(self.skip_instr());
                    }
                }

                // cut out unnecessary jumps
                if let Some(last_gap) = jump_gaps.last() {
                    if *last_gap == self.out_bc.len() - 1 {
                        jump_gaps.pop();
                        self.out_bc.pop();
                    }
                }

                if let Some(match_result_slot) = match_result_slot {
                    for gap_index in jump_gaps.iter() {
                        let gap_offset = -self.get_jump_offset(*gap_index);
                        self.out_bc[*gap_index] = Instr::JumpF(gap_offset, match_result_slot);
                    }
                }

                // we could technically optimize for the case of single-variant enums, but why?
                true
            }
            PatternKind::Or { options } => {
                let mut jump_gaps = Vec::new();
                let mut result = true;

                assert!(options.len() >= 2);

                for option in options {
                    let option = self.in_func.pattern(*option);

                    // always copy values since a local may appear multiple times
                    let refutable =
                        self.match_pattern_internal(option, source, false, match_result_slot);
                    if !refutable {
                        // irrefutable pattern, no point in testing anything else
                        result = false;
                        break;
                    } else {
                        jump_gaps.push(self.skip_instr());
                    }
                }

                // cut out unnecessary jumps
                if let Some(last_gap) = jump_gaps.last() {
                    if *last_gap == self.out_bc.len() - 1 {
                        jump_gaps.pop();
                        self.out_bc.pop();
                    }
                }

                if let Some(match_result_slot) = match_result_slot {
                    for gap_index in jump_gaps.iter() {
                        let gap_offset = -self.get_jump_offset(*gap_index);
                        self.out_bc[*gap_index] = Instr::JumpT(gap_offset, match_result_slot);
                    }
                } else {
                    panic!("or-pattern should always have a result slot");
                }

                result
            }
            PatternKind::DeRef { sub_pattern } => {
                let pat_ty = self.apply_subs(pat.ty);
                let sub_ty = self.apply_subs(self.in_func.pattern(*sub_pattern).ty);
                let ptr_kind = if sub_ty.is_sized() {
                    PointerKind::Thin
                } else {
                    PointerKind::Fat
                };

                let sub_pattern = self.in_func.pattern(*sub_pattern);
                match source {
                    // aliasing is probably irrelevant at this stage
                    Place::Local(source_slot) => self.match_pattern_internal(
                        sub_pattern,
                        Place::Ptr(source_slot, 0, ptr_kind),
                        false,
                        match_result_slot,
                    ),
                    Place::Ptr(ptr_slot, ptr_offset, _) => {
                        let new_slot = self.stack.alloc(pat_ty);
                        if let Some(copy) =
                            bytecode_select::copy_from_ptr(new_slot, ptr_slot, pat_ty, ptr_offset)
                        {
                            self.out_bc.push(copy);
                        }

                        self.match_pattern_internal(
                            sub_pattern,
                            Place::Ptr(new_slot, 0, ptr_kind),
                            false,
                            match_result_slot,
                        )
                    }
                }
            }
            PatternKind::LiteralValue(n) => {
                // refutable pattern, should have a result
                let match_result_slot = match_result_slot.unwrap();

                let val_slot = match source {
                    Place::Local(slot) => slot,
                    Place::Ptr(ref_slot, ref_offset, _) => {
                        let ty = self.apply_subs(pat.ty);
                        let slot = self.stack.alloc(ty);
                        if let Some(instr) =
                            bytecode_select::copy_from_ptr(slot, ref_slot, ty, ref_offset)
                        {
                            self.out_bc.push(instr);
                        }
                        slot
                    }
                };

                let size = pat.ty.layout().assert_size();
                let lit_slot = self.stack.alloc(pat.ty);
                self.out_bc
                    .push(bytecode_select::literal(*n, size, lit_slot));

                let (cmp_ctor, _) = bytecode_select::binary(BinaryOp::Eq, pat.ty);
                self.out_bc
                    .push(cmp_ctor(match_result_slot, val_slot, lit_slot));
                true
            }
            PatternKind::NamedConst(const_with_subs) => {
                // refutable pattern, should have a result
                let match_result_slot = match_result_slot.unwrap();

                let val_slot = match source {
                    Place::Local(slot) => slot,
                    Place::Ptr(ref_slot, ref_offset, _) => {
                        let ty = self.apply_subs(pat.ty);
                        let slot = self.stack.alloc(ty);
                        if let Some(instr) =
                            bytecode_select::copy_from_ptr(slot, ref_slot, ty, ref_offset)
                        {
                            self.out_bc.push(instr);
                        }
                        slot
                    }
                };

                // this is pretty gross, and probably not even sufficient for some cases:
                // 1. load pointer to the constant
                // 2. load constant to stack slot
                // 3. compare

                // fail early if there's no valid comparison (for a larger struct, most likely)
                let (cmp_ctor, _) = bytecode_select::binary(BinaryOp::Eq, pat.ty);

                let const_ptr = const_with_subs.item.const_value(&const_with_subs.subs);
                let ptr_ty = self.vm.common_types().usize;
                let ptr_slot = self.stack.alloc(ptr_ty);
                self.out_bc.push(bytecode_select::literal(
                    const_ptr.as_ptr() as usize as _,
                    ptr_ty.layout().assert_size(),
                    ptr_slot,
                ));

                let cmp_slot = self.stack.alloc(pat.ty);
                self.out_bc
                    .push(bytecode_select::copy_from_ptr(cmp_slot, ptr_slot, pat.ty, 0).unwrap());

                self.out_bc
                    .push(cmp_ctor(match_result_slot, cmp_slot, val_slot));
                true
            }
            PatternKind::LiteralBytes(bytes) => {
                // refutable pattern, should have a result
                let match_result_slot = match_result_slot.unwrap();

                let val_slot = match source {
                    Place::Local(slot) => slot,
                    Place::Ptr(ref_slot, ref_offset, _) => {
                        let ty = self.apply_subs(pat.ty);
                        let slot = self.stack.alloc(ty);
                        if let Some(instr) =
                            bytecode_select::copy_from_ptr(slot, ref_slot, ty, ref_offset)
                        {
                            self.out_bc.push(instr);
                        }
                        slot
                    }
                };

                // check type
                {
                    let TypeKind::Ref(slice_ty,_) = pat.ty.kind() else {
                        panic!("string pattern on {}",pat.ty);
                    };

                    match slice_ty.kind() {
                        TypeKind::StringSlice => (),
                        _ => panic!("string pattern on {}", pat.ty),
                    }
                }

                assert!(pat.ty.layout().assert_size() == POINTER_SIZE.bytes() * 2);
                let ref_slot = self.stack.alloc(pat.ty);

                self.out_bc.push(bytecode_select::literal(
                    bytes.as_ptr() as _,
                    POINTER_SIZE.bytes(),
                    ref_slot,
                ));
                self.out_bc.push(bytecode_select::literal(
                    bytes.len() as _,
                    POINTER_SIZE.bytes(),
                    ref_slot.offset_by(POINTER_SIZE.bytes() as i32),
                ));
                self.out_bc
                    .push(Instr::MemCompare(match_result_slot, val_slot, ref_slot));

                true
            }
            PatternKind::Range {
                start,
                end,
                end_is_inclusive,
            } => {
                // refutable pattern, should have a result
                let match_result_slot = match_result_slot.unwrap();

                let val_slot = match source {
                    Place::Local(slot) => slot,
                    Place::Ptr(ref_slot, ref_offset, _) => {
                        let ty = self.apply_subs(pat.ty);
                        let slot = self.stack.alloc(ty);
                        if let Some(instr) =
                            bytecode_select::copy_from_ptr(slot, ref_slot, ty, ref_offset)
                        {
                            self.out_bc.push(instr);
                        }
                        slot
                    }
                };

                if let Some(end) = end {
                    let end_cmp_val = self.lower_expr(*end, None);

                    let (end_cmp_ctor, _) = if *end_is_inclusive {
                        bytecode_select::binary(BinaryOp::LtEq, pat.ty)
                    } else {
                        bytecode_select::binary(BinaryOp::Lt, pat.ty)
                    };

                    self.out_bc
                        .push(end_cmp_ctor(match_result_slot, val_slot, end_cmp_val));

                    if let Some(start) = start {
                        // start AND end, we need to insert a jump
                        let jump_index = self.skip_instr();

                        let start_cmp_val = self.lower_expr(*start, None);

                        let (start_cmp_ctor, _) = bytecode_select::binary(BinaryOp::LtEq, pat.ty);

                        self.out_bc.push(start_cmp_ctor(
                            match_result_slot,
                            start_cmp_val,
                            val_slot,
                        ));

                        let gap_offset = -self.get_jump_offset(jump_index);
                        self.out_bc[jump_index] = Instr::JumpF(gap_offset, match_result_slot);
                    }
                } else if let Some(start) = start {
                    let start_cmp_val = self.lower_expr(*start, None);

                    let (start_cmp_ctor, _) = bytecode_select::binary(BinaryOp::LtEq, pat.ty);

                    self.out_bc
                        .push(start_cmp_ctor(match_result_slot, start_cmp_val, val_slot));
                } else {
                    panic!("bad range")
                }

                true
            }
            PatternKind::Slice { start, mid, end } => {
                let ty = self.apply_subs(pat.ty);

                match ty.kind() {
                    TypeKind::Array(elem_ty, array_len) => {
                        let array_len = array_len.get_value() as usize;
                        let elem_size = elem_ty.layout().assert_size();

                        let mut jump_gaps = Vec::new();
                        let mut result = false;

                        // start group
                        for (i, pat_id) in start.iter().enumerate() {
                            let sub_pat = self.in_func.pattern(*pat_id);

                            let offset = i as i32 * elem_size as i32;
                            let refutable = self.match_pattern_internal(
                                sub_pat,
                                source.offset_by(offset),
                                can_alias,
                                match_result_slot,
                            );
                            result |= refutable;

                            if refutable && match_result_slot.is_some() {
                                jump_gaps.push(self.skip_instr());
                            }
                        }

                        // middle group
                        if let Some(pat_id) = mid {
                            let sub_pat = self.in_func.pattern(*pat_id);
                            let sub_ty = self.apply_subs(sub_pat.ty);

                            match sub_ty.kind() {
                                TypeKind::Array(..) => (), // ok
                                TypeKind::Ref(child, _) => {
                                    match child.kind() {
                                        TypeKind::Array(..) => (), // ok
                                        _ => panic!("bad slice middle: {}", sub_ty),
                                    }
                                }
                                _ => panic!("bad slice middle: {}", sub_ty),
                            }

                            let offset = start.len() as i32 * elem_size as i32;
                            let refutable = self.match_pattern_internal(
                                sub_pat,
                                source.offset_by(offset),
                                can_alias,
                                match_result_slot,
                            );
                            result |= refutable;

                            if refutable && match_result_slot.is_some() {
                                jump_gaps.push(self.skip_instr());
                            }
                        }

                        // end group
                        for (i, pat_id) in end.iter().enumerate() {
                            let sub_pat = self.in_func.pattern(*pat_id);

                            let i = array_len - end.len() + i;

                            let offset = i as i32 * elem_size as i32;
                            let refutable = self.match_pattern_internal(
                                sub_pat,
                                source.offset_by(offset),
                                can_alias,
                                match_result_slot,
                            );
                            result |= refutable;

                            if refutable && match_result_slot.is_some() {
                                jump_gaps.push(self.skip_instr());
                            }
                        }

                        // cut out unnecessary jumps
                        if let Some(last_gap) = jump_gaps.last() {
                            if *last_gap == self.out_bc.len() - 1 {
                                jump_gaps.pop();
                                self.out_bc.pop();
                            }
                        }

                        if let Some(match_result_slot) = match_result_slot {
                            for gap_index in jump_gaps.iter() {
                                let gap_offset = -self.get_jump_offset(*gap_index);
                                self.out_bc[*gap_index] =
                                    Instr::JumpF(gap_offset, match_result_slot);
                            }
                        }

                        result
                    }
                    TypeKind::Slice(elem_ty) => {
                        let len_req = start.len() + end.len();
                        let len_req_is_exact = mid.is_none();

                        let Place::Ptr(ref_slot, ref_offset,ptr_kind) = source else {
                            panic!("attempt to match value slice");
                        };
                        assert!(ref_offset == 0);
                        assert!(ptr_kind.is_fat());

                        let len_slot = ref_slot.offset_by(POINTER_SIZE.bytes() as i32);
                        let elem_size = elem_ty.layout().assert_size();

                        let mut jump_gaps = Vec::new();
                        let result = len_req > 0 || len_req_is_exact;

                        let len_ty = self.vm.common_types().usize;

                        // len check
                        if result {
                            let match_result_slot =
                                match_result_slot.expect("no result for refutable slice pattern?");

                            let len_check_slot = self.stack.alloc(len_ty);
                            self.out_bc.push(bytecode_select::literal(
                                len_req as _,
                                len_ty.layout().assert_size(),
                                len_check_slot,
                            ));

                            let len_cmp_op = if len_req_is_exact {
                                BinaryOp::Eq
                            } else {
                                BinaryOp::LtEq
                            };

                            let (len_cmp_ctor, _) = bytecode_select::binary(len_cmp_op, len_ty);

                            self.out_bc.push(len_cmp_ctor(
                                match_result_slot,
                                len_check_slot,
                                len_slot,
                            ));

                            jump_gaps.push(self.skip_instr());
                        }

                        // start group
                        for (i, pat_id) in start.iter().enumerate() {
                            let sub_pat = self.in_func.pattern(*pat_id);

                            let offset = i as i32 * elem_size as i32;
                            let refutable = self.match_pattern_internal(
                                sub_pat,
                                source.offset_by(offset),
                                can_alias,
                                match_result_slot,
                            );

                            if refutable && match_result_slot.is_some() {
                                jump_gaps.push(self.skip_instr());
                            }
                        }

                        // middle group
                        if let Some(pat_id) = mid {
                            let sub_pat = self.in_func.pattern(*pat_id);

                            match sub_pat.kind {
                                PatternKind::Hole => (), // do nothing
                                PatternKind::LocalBinding { local_id, mode, .. } => {
                                    assert!(mode == BindingMode::Ref);

                                    let sub_ty = self.apply_subs(sub_pat.ty);

                                    match sub_ty.kind() {
                                        TypeKind::Ref(child, _) => {
                                            match child.kind() {
                                                TypeKind::Slice(..) => (), // ok
                                                _ => panic!("bad slice middle: {}", sub_ty),
                                            }
                                        }
                                        _ => panic!("bad slice middle: {}", sub_ty),
                                    }

                                    // don't worry whether we "must copy" -- we always create a new slice here
                                    let var_slot = self.find_or_alloc_local(local_id, sub_ty);

                                    let offset = start.len() as i32 * elem_size as i32;
                                    let len_sub = (start.len() + end.len()) as i32;

                                    self.out_bc
                                        .push(Instr::PointerOffset2(var_slot, ref_slot, offset));

                                    self.out_bc.push(Instr::PointerOffset2(
                                        var_slot.offset_by(POINTER_SIZE.bytes() as i32),
                                        len_slot,
                                        -len_sub,
                                    ));
                                }
                                _ => println!("? {:?}", sub_pat.kind),
                            }
                        }

                        // end group
                        if end.len() > 0 {
                            let end_slot = self.stack.alloc(len_ty);
                            self.out_bc.push(Instr::IndexCalcEndPointer {
                                out: end_slot,
                                slice: ref_slot,
                                elem_size,
                            });

                            for (i, pat_id) in end.iter().enumerate() {
                                let sub_pat = self.in_func.pattern(*pat_id);

                                let i = i - end.len();

                                let offset = i as i32 * elem_size as i32;
                                let refutable = self.match_pattern_internal(
                                    sub_pat,
                                    Place::Ptr(end_slot, offset, PointerKind::Thin),
                                    can_alias,
                                    match_result_slot,
                                );

                                if refutable && match_result_slot.is_some() {
                                    jump_gaps.push(self.skip_instr());
                                }
                            }
                        }

                        // cut out unnecessary jumps
                        if let Some(last_gap) = jump_gaps.last() {
                            if *last_gap == self.out_bc.len() - 1 {
                                jump_gaps.pop();
                                self.out_bc.pop();
                            }
                        }

                        if let Some(match_result_slot) = match_result_slot {
                            for gap_index in jump_gaps.iter() {
                                let gap_offset = -self.get_jump_offset(*gap_index);
                                self.out_bc[*gap_index] =
                                    Instr::JumpF(gap_offset, match_result_slot);
                            }
                        }

                        result
                    }
                    _ => panic!("todo slice pattern on {}", ty),
                }
            }
            PatternKind::Hole => false,
            PatternKind::Error(msg) => panic!("error pattern: {}", msg),
        }
    }

    fn find_local(&self, local: u32) -> Slot {
        for (id, slot) in &self.locals {
            if *id == local {
                return *slot;
            }
        }
        panic!("failed to find local {}", local);
    }

    fn find_or_alloc_local(&mut self, local: u32, ty: Type) -> Slot {
        for (id, slot) in &self.locals {
            if *id == local {
                return *slot;
            }
        }
        let var_slot = self.stack.alloc(ty);
        self.locals.push((local, var_slot));
        var_slot
    }

    fn assert_local_undef(&self, local: u32) {
        for (id, _) in &self.locals {
            if *id == local {
                panic!("assert_local_undef failed");
            }
        }
    }

    fn skip_instr(&mut self) -> usize {
        let index = self.out_bc.len();
        self.out_bc.push(Instr::Skipped);
        index
    }

    fn get_jump_offset(&mut self, other: usize) -> i32 {
        other as i32 - self.out_bc.len() as i32
    }
}

#[derive(Debug, Clone, Copy)]
enum Place {
    Local(Slot),
    /// Pointer with offset
    Ptr(Slot, i32, PointerKind),
}

impl Place {
    pub fn offset_by(&self, n: i32) -> Self {
        match self {
            Place::Local(slot) => Place::Local(slot.offset_by(n)),
            Place::Ptr(slot, offset, kind) => Place::Ptr(*slot, offset + n, *kind),
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum PointerKind {
    Thin,
    Fat,
}

impl PointerKind {
    fn is_fat(&self) -> bool {
        match self {
            PointerKind::Thin => false,
            PointerKind::Fat => true,
        }
    }

    fn is_thin(&self) -> bool {
        !self.is_fat()
    }
}

#[derive(Default)]
pub struct CompilerStack {
    entries: Vec<StackEntry>,
    top: u32,
    next_scope_id: u32,
}

struct StackScope {
    old_entries: usize,
    old_top: u32,
    name: &'static str,
}

impl Drop for StackScope {
    fn drop(&mut self) {
        panic!("stack scope was not returned to the stack!");
    }
}

impl CompilerStack {
    pub fn alloc(&mut self, ty: Type) -> Slot {
        let drop_info = ty.drop_info();
        assert!(drop_info.is_none());

        self.alloc_no_drop(ty)
    }

    pub fn alloc_no_drop(&mut self, ty: Type) -> Slot {
        let layout = ty.layout();

        self.align(layout.align);

        let base = self.top;
        let size = layout.assert_size();

        self.entries.push(StackEntry { base, size });
        self.top += size;
        Slot::new(base)
    }

    pub fn align(&mut self, align: u32) {
        self.top = crate::abi::align(self.top, align);
    }

    pub fn align_for_call(&mut self) -> Slot {
        self.align(16);
        Slot::new(self.top)
    }

    fn push_scope(&mut self, name: &'static str) -> StackScope {
        eprintln!("- enter {}", name);
        StackScope {
            old_entries: self.entries.len(),
            old_top: self.top,
            name,
        }
    }

    fn pop_scope(&mut self, scope: StackScope) {
        eprintln!("- exit {}", scope.name);
        self.entries.truncate(scope.old_entries);
        self.top = scope.old_top;
        std::mem::forget(scope);
    }
}

struct StackEntry {
    base: u32,
    size: u32,
}

// A simplified callstack for
#[derive(Default)]
struct CallFrame {
    top: u32,
}

impl CallFrame {
    fn alloc(&mut self, ty: Type) -> Slot {
        let layout = ty.layout();

        self.align(layout.align);

        let base = self.top;
        let size = layout.assert_size();

        self.top += size;
        Slot::new_frame_sub(base)
    }

    fn align(&mut self, align: u32) {
        self.top = crate::abi::align(self.top, align);
    }
}
