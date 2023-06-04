use rustc_hir as hir;
use rustc_hir::def_id::LocalDefId;
use rustc_middle::ty::TypeckResults;

use std::str::FromStr;

use crate::{rustc_worker::RustCContext, types::{TypeKind, FloatWidth}};

use super::{
    IRFunctionBuilder, LoopId, IRFunction, BinaryOp, ExprId, UnaryOp, LogicOp, Expr, Block, Stmt, PatternId, FieldPattern,
    ExprKind, PointerCast, BindingMode, PatternKind, Pattern, MatchArm
};

/// Converts rust IR to skitter IR.
/// 
/// Now uses TypeckResults instead of THIR because THIR can get randomly stolen when querying other THIR.
pub struct IRFunctionConverter<'vm, 'tcx> {
    ctx: RustCContext<'vm, 'tcx>,
    func_id: LocalDefId,
    types: &'tcx TypeckResults<'tcx>,
    loops: Vec<(hir::HirId,LoopId)>,
    builder: IRFunctionBuilder<'vm>
}

impl<'vm, 'tcx, 'a> IRFunctionConverter<'vm, 'tcx> {
    pub fn run(
        ctx: RustCContext<'vm, 'tcx>,
        func_id: LocalDefId,
        body: &rustc_hir::Body,
        types: &'tcx TypeckResults<'tcx>,
        is_constant: bool
    ) -> IRFunction<'vm> {

        assert!(body.generator_kind.is_none());
        
        let mut converter = Self {
            ctx,
            func_id,
            types,
            loops: vec!(),
            builder: Default::default()
        };
        
        let params: Vec<_> = body.params.iter().map(|param| converter.pattern(param.pat)).collect();

        let root_expr = converter.expr(body.value);

        converter.builder.finish(root_expr, is_constant, params)
    }

    /// Convert binary op from rust IR. Does not handle logical ops.
    fn bin_op(&self, op: hir::BinOpKind) -> BinaryOp {
        use hir::BinOpKind;
        match op {
            BinOpKind::Add => BinaryOp::Add,
            BinOpKind::Sub => BinaryOp::Sub,
            BinOpKind::Mul => BinaryOp::Mul,
            BinOpKind::Div => BinaryOp::Div,
            BinOpKind::Rem => BinaryOp::Rem,

            BinOpKind::BitAnd => BinaryOp::BitAnd,
            BinOpKind::BitOr => BinaryOp::BitOr,
            BinOpKind::BitXor => BinaryOp::BitXor,
            BinOpKind::Shr => BinaryOp::ShiftR,
            BinOpKind::Shl => BinaryOp::ShiftL,

            BinOpKind::Eq => BinaryOp::Eq,
            BinOpKind::Ne => BinaryOp::NotEq,
            BinOpKind::Lt => BinaryOp::Lt,
            BinOpKind::Gt => BinaryOp::Gt,
            BinOpKind::Le => BinaryOp::LtEq,
            BinOpKind::Ge => BinaryOp::GtEq,
            _ => panic!()
        }
    }

    fn expr(&mut self, expr: &hir::Expr) -> ExprId {
        let rs_ty = self.types.expr_ty(expr);
        let ty = self.ctx.type_from_rustc(rs_ty);

        let expr_kind = match expr.kind {
            hir::ExprKind::Lit(lit) => {
                use rustc_ast::ast::LitKind;
                match lit.node {
                    LitKind::Int(n, _) => {
                        let n = n as i128;
                        ExprKind::LiteralValue(n)
                    }
                    LitKind::Float(sym, _) => {
                        let TypeKind::Float(float_width) = ty.kind() else {
                            panic!("non-float float literal");
                        };
                        match float_width {
                            FloatWidth::F32 => {
                                let n = f32::from_str(sym.as_str()).unwrap();
                                let x: i32 = unsafe { std::mem::transmute(n) };
                                ExprKind::LiteralValue(x as i128)
                            }
                            FloatWidth::F64 => {
                                let n = f64::from_str(sym.as_str()).unwrap();
                                let x: i64 = unsafe { std::mem::transmute(n) };
                                ExprKind::LiteralValue(x as i128)
                            }
                        }
                    }
                    LitKind::Bool(b) => ExprKind::LiteralValue(if b { 1 } else { 0 }),
                    LitKind::Char(c) => ExprKind::LiteralValue(c as i128),
                    LitKind::Byte(b) => {
                        ExprKind::LiteralValue(b as i128)
                    }
                    LitKind::Str(sym, _) => {
                        let bytes = sym.as_str().as_bytes().to_owned();
                        ExprKind::LiteralBytes(self.ctx.vm.alloc_constant(bytes))
                    }
                    LitKind::ByteStr(ref bytes,_) => {
                        let bytes = bytes.iter().copied().collect();
                        ExprKind::LiteralBytes(self.ctx.vm.alloc_constant(bytes))
                    }
                    _ => panic!("lit other {:?}", lit.node),
                }
            }
            hir::ExprKind::Cast(arg,_) => {
                ExprKind::Cast(self.expr(arg))
            }
            hir::ExprKind::AddrOf(_,_,arg) => {
                ExprKind::Ref(self.expr(arg))
            }
            hir::ExprKind::Unary(op,arg) => {
                if let hir::UnOp::Deref = op {
                    ExprKind::DeRef(self.expr(arg))
                } else {
                    let op = match op {
                        hir::UnOp::Not => UnaryOp::Not,
                        hir::UnOp::Neg => UnaryOp::Neg,
                        _ => panic!() 
                    };

                    ExprKind::Unary(op, self.expr(arg))
                }
            }
            hir::ExprKind::Binary(op,lhs,rhs) => {
                let lhs = self.expr(lhs);
                let rhs = self.expr(rhs);

                match op.node {
                    hir::BinOpKind::And => {
                        ExprKind::LogicOp(LogicOp::And, lhs, rhs)
                    },
                    hir::BinOpKind::Or => {
                        ExprKind::LogicOp(LogicOp::Or, lhs, rhs)
                    },
                    _ => {
                        if let Some(func_did) = self.types.type_dependent_def_id(expr.hir_id) {
                            let subs = self.types.node_substs(expr.hir_id);
                            let func_item = self.ctx.vm.types.def_from_rustc(func_did, subs, &self.ctx);
                            let func_ty = self.ctx.vm.ty_func_def(func_item);

                            let func = self.builder.add_expr(Expr{
                                kind: ExprKind::LiteralVoid,
                                ty: func_ty
                            });

                            ExprKind::Call{ func, args: vec!(lhs,rhs) }
                        } else {
                            let op = self.bin_op(op.node);

                            ExprKind::Binary(op, lhs, rhs)
                        }
                    }
                }
            }
            hir::ExprKind::AssignOp(op,lhs,rhs) => {
                let op = self.bin_op(op.node);
                let lhs = self.expr(lhs);
                let rhs = self.expr(rhs);

                ExprKind::AssignOp(op, lhs, rhs)
            }
            hir::ExprKind::Assign(lhs,rhs,_) => {
                let lhs = self.expr(lhs);
                let rhs = self.expr(rhs);

                ExprKind::Assign(lhs, rhs)
            }
            hir::ExprKind::Call(func,args) => {
                if let Some(func_did) = self.types.type_dependent_def_id(expr.hir_id) {
                    panic!("{:?}",func_did)
                } else {
                    let func = self.expr(func);
    
                    let args = args.iter().map(|arg| {
                        self.expr(arg)
                    }).collect();
    
                    ExprKind::Call{ func, args }
                }
            }
            hir::ExprKind::MethodCall(_,lhs,args,_) => {
                // We need to pull the actual method def from typeck results.
                let method_did = self.types.type_dependent_def_id(expr.hir_id).unwrap();
                let method_subs = self.types.node_substs(expr.hir_id);
                let method_item = self.ctx.vm.types.def_from_rustc(method_did, method_subs, &self.ctx);
                let method_ty = self.ctx.vm.ty_func_def(method_item);

                let lhs = self.expr(lhs);
                let args = args.iter().map(|arg| self.expr(arg));

                let full_args = std::iter::once(lhs).chain(args).collect();

                let func = self.builder.add_expr(Expr{
                    kind: ExprKind::LiteralVoid,
                    ty: method_ty
                });

                ExprKind::Call{ func, args: full_args }
            }
            hir::ExprKind::If(cond,then,else_opt) => {
                let cond = self.expr(cond);
                let then = self.expr(then);
                let else_opt = else_opt.map(|e| self.expr(e));
                ExprKind::If{ cond, then, else_opt }
            }
            hir::ExprKind::Match(arg,arms,_) => {
                let arg = self.expr(arg);
                let arms = arms.iter().map(|arm| {
                    MatchArm{
                        pattern: self.pattern(arm.pat),
                        body: self.expr(arm.body),
                        has_guard: arm.guard.is_some(),
                    }
                }).collect();
                ExprKind::Match{ arg, arms }
            }
            hir::ExprKind::Loop(body,..) => {
                let loop_id = LoopId::new(self.loops.len() as u32);
                self.loops.push((expr.hir_id,loop_id));

                let body = self.block(body);
                ExprKind::Loop(body,loop_id)
            }
            hir::ExprKind::Break(dest,value) => {
                let target = dest.target_id.unwrap();
                let (_,loop_id) = self.loops.iter().find(|(h,_)| h == &target).copied().unwrap();

                let value = value.map(|e| self.expr(e));

                ExprKind::Break { loop_id, value }
            }
            hir::ExprKind::Continue(dest) => {
                let target = dest.target_id.unwrap();
                let (_,loop_id) = self.loops.iter().find(|(h,_)| h == &target).copied().unwrap();

                ExprKind::Continue { loop_id }
            }
            hir::ExprKind::Ret(value) => {
                let value = value.map(|e| self.expr(e));

                ExprKind::Return(value)
            }
            hir::ExprKind::Tup(args) => {
                let args = args.iter().map(|a| self.expr(a)).collect();
                ExprKind::Tuple(args)
            }
            hir::ExprKind::Struct(path,fields,rest) => {
                assert!(rest.is_none());
                let res = self.types.qpath_res(&path,expr.hir_id);

                let variant = match res {
                    // never refer to a variant
                    hir::def::Res::Def(hir::def::DefKind::TyAlias,_) |
                    hir::def::Res::SelfTyAlias{..} => 0,
                    
                    hir::def::Res::Def(_,def_id) => {
                        let rustc_middle::ty::TyKind::Adt(adt_def,_) = rs_ty.kind() else {
                            panic!("attempt to convert struct without adt");
                        };
                        adt_def.variant_index_with_id(def_id).as_u32()
                    }
                    _ => {
                        panic!("struct from {:?}",res)
                    }
                };

                let fields = fields.iter().map(|field| {
                    let id = self.types.field_index(field.hir_id).as_u32();
                    let expr = self.expr(field.expr);
                    (id,expr)
                }).collect();

                ExprKind::Adt{
                    variant,
                    fields
                }
            }
            hir::ExprKind::Array(args) => {
                let args = args.iter().map(|a| self.expr(a)).collect();
                ExprKind::Array(args)
            }
            hir::ExprKind::Index(lhs,index) => {
                let lhs = self.expr(lhs);
                let index = self.expr(index);
                ExprKind::Index{ lhs, index }
            }
            hir::ExprKind::Field(lhs,field) => {
                let index = self.types.field_index(expr.hir_id);
                let lhs = self.expr(lhs);
                ExprKind::Field{ lhs, variant: 0, field: index.as_u32() }
            }
            hir::ExprKind::Path(path) => {
                let res = self.types.qpath_res(&path,expr.hir_id);
                
                match res {
                    hir::def::Res::Def(def_kind,did) => {
                        match def_kind {
                            hir::def::DefKind::Fn |
                            hir::def::DefKind::AssocFn |
                            hir::def::DefKind::Ctor(..) => {
                                // return void expression, any information needed
                                // can be pulled from the type
                                ExprKind::LiteralVoid
                            }
                            hir::def::DefKind::Const |
                            hir::def::DefKind::AssocConst => {
                                let subs = self.types.node_substs(expr.hir_id);
                                let const_item = self.ctx.vm.types.def_from_rustc(did, subs, &self.ctx);
                                ExprKind::NamedConst(const_item)
                            }
                            _ => panic!("def = {:?}",def_kind)
                        }
                    }
                    hir::def::Res::SelfCtor(_) => {
                        // return void expression, any information needed
                        // can be pulled from the type
                        ExprKind::LiteralVoid
                    }
                    hir::def::Res::Local(hir_id) => {

                        assert_eq!(hir_id.owner.def_id, self.func_id);
                        let local_id = hir_id.local_id.as_u32();
    
                        ExprKind::VarRef(local_id)
                    }
                    _ => panic!("path = {:?}",res)
                }
            }
            hir::ExprKind::Let(let_expr) => {
                ExprKind::Let {
                    pattern: self.pattern(let_expr.pat),
                    init: self.expr(let_expr.init),
                }
            }
            hir::ExprKind::Block(block,_) => {
                let block = self.block(block);
                ExprKind::Block(block)
            }
            hir::ExprKind::DropTemps(e) => {
                // TODO TODO TODO
                // this may pose an issue when implementing destructors / drop
                return self.expr(e);
            }
            _ => panic!("todo expr kind {:?}",expr.kind)
        };

        let mut expr_id = self.builder.add_expr(Expr{
            kind: expr_kind,
            ty
        });

        let adjust_list = self.types.expr_adjustments(expr);
        for adjust in adjust_list {
            let adjust_ty = self.ctx.type_from_rustc(adjust.target);

            use rustc_middle::ty::adjustment::Adjust;
            match adjust.kind {
                Adjust::NeverToAny => {
                    expr_id = self.builder.add_expr(Expr{
                        kind: ExprKind::Dummy(expr_id),
                        ty: adjust_ty
                    });
                }
                Adjust::Deref(overloaded) => {
                    assert!(overloaded.is_none());
                    expr_id = self.builder.add_expr(Expr{
                        kind: ExprKind::DeRef(expr_id),
                        ty: adjust_ty
                    });
                }
                Adjust::Borrow(_) => {
                    expr_id = self.builder.add_expr(Expr{
                        kind: ExprKind::Ref(expr_id),
                        ty: adjust_ty
                    });
                }
                Adjust::Pointer(ptr_cast) => {
                    use rustc_middle::ty::adjustment::PointerCast as PC;
                    let ptr_cast: PointerCast = match ptr_cast {
                        PC::ReifyFnPointer => PointerCast::ReifyFnPointer,
                        PC::UnsafeFnPointer => PointerCast::UnsafeFnPointer,
                        PC::ClosureFnPointer(_) => PointerCast::ClosureFnPointer,
                        PC::MutToConstPointer => PointerCast::MutToConstPointer,
                        PC::ArrayToPointer => PointerCast::ArrayToPointer,
                        PC::Unsize => PointerCast::UnSize,
                    };

                    expr_id = self.builder.add_expr(Expr{
                        kind: ExprKind::PointerCast(expr_id,ptr_cast),
                        ty: adjust_ty
                    });
                }
                _ => panic!("todo adjust {:?}",adjust)
            }
        }

        expr_id
    }

    fn block(&mut self, block: &hir::Block) -> Block {
        let mut stmts = vec!();

        for stmt in block.stmts {
            if let Some(stmt) = self.stmt(&stmt) {
                stmts.push(stmt);
            }
        }

        let result = block.expr.map(|expr| self.expr(expr));

        Block{
            stmts,
            result
        }
    }

    fn stmt(&mut self, stmt: &hir::Stmt) -> Option<Stmt> {
        let res = match stmt.kind {
            hir::StmtKind::Expr(expr) |
            hir::StmtKind::Semi(expr) => {
                Stmt::Expr(self.expr(&expr))
            }
            hir::StmtKind::Local(local) => {

                let pattern = self.pattern(local.pat);
                let init = local.init.map(|init| self.expr(init));
                let else_block = local.els.map(|else_block| self.block(else_block));

                Stmt::Let{ pattern, init, else_block }
            }
            hir::StmtKind::Item(_) => return None,
        };

        Some(res)
    }

    fn pattern(&mut self, pat: &hir::Pat) -> PatternId {
        let ty = self.types.pat_ty(pat);
        let ty = self.ctx.type_from_rustc(ty);

        let pattern_kind = match pat.kind {
            hir::PatKind::Binding(_,hir_id,_,sub_pattern) => {
                let all_binding_modes = self.types.pat_binding_modes();
                let mode = all_binding_modes.get(pat.hir_id).unwrap();

                let mode = match mode {
                    rustc_middle::ty::BindingMode::BindByReference(_) => BindingMode::Ref,
                    rustc_middle::ty::BindingMode::BindByValue(_) => BindingMode::Value,
                };

                // should hold true, even for closures? when are we ever going
                // to bind a variable that doesn't belong to our current function?
                assert_eq!(hir_id.owner.def_id, self.func_id);
                let local_id = hir_id.local_id.as_u32();

                let sub_pattern = sub_pattern.map(|sub_pat| self.pattern(sub_pat));

                PatternKind::LocalBinding{ local_id, mode, sub_pattern }
            }
            hir::PatKind::Lit(lit) => {
                if let hir::ExprKind::Lit(lit) = lit.kind {
                    use rustc_ast::ast::LitKind;
                    match lit.node {
                        LitKind::Int(n, _) => {
                            let n = n as i128;
                            PatternKind::LiteralValue(n)
                        }
                        _ => panic!("pat lit kind {:?}",lit.node)
                    }
                } else {
                    panic!("pat lit {:?}",lit);
                }
            }
            hir::PatKind::Tuple(children,gap_pos) => {
                let tup_size = if let TypeKind::Tuple(children) = ty.kind() {
                    children.len()
                } else {
                    panic!("tuple pattern is not a tuple?");
                };

                let gap_pos = gap_pos.as_opt_usize();

                let fields = children.iter().enumerate().map(|(i,child)| {

                    let field = if let Some(gap_pos) = gap_pos {
                        if i >= gap_pos {
                            let back_offset = children.len() - i;
                            (tup_size - back_offset) as u32
                        } else {
                            i as u32
                        }
                    } else {
                        i as u32
                    };

                    FieldPattern{
                        pattern: self.pattern(child),
                        field
                    }
                }).collect();

                PatternKind::Struct { fields }
            }
            hir::PatKind::Ref(sub_pattern,_) => {
                let sub_pattern = self.pattern(sub_pattern);
                PatternKind::DeRef{ sub_pattern }
            }
            hir::PatKind::Or(options) => {
                let options = options.iter().map(|p| self.pattern(p)).collect();
                PatternKind::Or{ options }
            }
            hir::PatKind::Wild => {
                PatternKind::Hole
            }
            _ => panic!("pat {:?}",pat.kind)
        };

        let mut res_pat = self.builder.add_pattern(Pattern{
            kind: pattern_kind,
            ty
        });

        let all_adjust = self.types.pat_adjustments();
        if let Some(adjust) = all_adjust.get(pat.hir_id) {
            for adjust_ty in adjust.iter().rev() {
                let adjust_ty = self.ctx.type_from_rustc(*adjust_ty);
                res_pat = self.builder.add_pattern(Pattern{
                    kind: PatternKind::DeRef{
                        sub_pattern: res_pat
                    },
                    ty: adjust_ty
                });
            }
        }

        res_pat
    }
}
