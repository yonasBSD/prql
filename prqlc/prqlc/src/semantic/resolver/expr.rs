use itertools::Itertools;

use crate::ir::decl::{DeclKind, Module};
use crate::ir::pl;
use crate::ir::pl::PlFold;
use crate::pr::{Ty, TyKind, TyTupleField};
use crate::semantic::resolver::{flatten, types, Resolver};
use crate::semantic::{NS_INFER, NS_SELF, NS_THAT, NS_THIS};
use crate::utils::IdGenerator;
use crate::Result;
use crate::{Error, Reason, Span, WithErrorInfo};

impl pl::PlFold for Resolver<'_> {
    fn fold_stmts(&mut self, _: Vec<pl::Stmt>) -> Result<Vec<pl::Stmt>> {
        unreachable!()
    }

    fn fold_type(&mut self, ty: Ty) -> Result<Ty> {
        Ok(match ty.kind {
            TyKind::Ident(ident) => {
                self.root_mod.module.shadow(NS_THIS);
                self.root_mod.module.shadow(NS_THAT);

                let fq_ident = self.resolve_ident(&ident)?;

                let decl = self.root_mod.module.get(&fq_ident).unwrap();
                let decl_ty = decl.kind.as_ty().ok_or_else(|| {
                    Error::new(Reason::Expected {
                        who: None,
                        expected: "a type".to_string(),
                        found: decl.to_string(),
                    })
                })?;
                let mut ty = decl_ty.clone();
                ty.name = ty.name.or(Some(fq_ident.name));

                self.root_mod.module.unshadow(NS_THIS);
                self.root_mod.module.unshadow(NS_THAT);

                ty
            }
            _ => pl::fold_type(self, ty)?,
        })
    }

    fn fold_var_def(&mut self, var_def: pl::VarDef) -> Result<pl::VarDef> {
        let value = match var_def.value {
            Some(value) if matches!(value.kind, pl::ExprKind::Func(_)) => Some(value),
            Some(value) => Some(Box::new(flatten::Flattener::fold(self.fold_expr(*value)?))),
            None => None,
        };

        Ok(pl::VarDef {
            name: var_def.name,
            value,
            ty: var_def.ty.map(|x| self.fold_type(x)).transpose()?,
        })
    }

    fn fold_expr(&mut self, node: pl::Expr) -> Result<pl::Expr> {
        if node.id.is_some() && !matches!(node.kind, pl::ExprKind::Func(_)) {
            return Ok(node);
        }

        let id = self.id.gen();
        let alias = Box::new(node.alias.clone());
        let span = Box::new(node.span);

        if let Some(span) = *span {
            self.root_mod.span_map.insert(id, span);
        }

        log::trace!("folding expr [{id:?}] {node:?}");

        let r = match node.kind {
            pl::ExprKind::Ident(ident) => {
                log::debug!("resolving ident {ident}...");
                let fq_ident = self.resolve_ident(&ident).with_span(node.span)?;
                log::debug!("... resolved to {fq_ident}");
                let entry = self.root_mod.module.get(&fq_ident).unwrap();
                log::debug!("... which is {entry}");

                match &entry.kind {
                    DeclKind::Infer(_) => pl::Expr {
                        kind: pl::ExprKind::Ident(fq_ident),
                        target_id: entry.declared_at,
                        ..node
                    },
                    DeclKind::Column(target_id) => pl::Expr {
                        kind: pl::ExprKind::Ident(fq_ident),
                        target_id: Some(*target_id),
                        ..node
                    },

                    DeclKind::TableDecl(_) => {
                        let input_name = ident.name.clone();

                        let lineage = self.lineage_of_table_decl(&fq_ident, input_name, id);

                        pl::Expr {
                            kind: pl::ExprKind::Ident(fq_ident),
                            ty: Some(ty_of_lineage(&lineage)),
                            lineage: Some(lineage),
                            alias: None,
                            ..node
                        }
                    }

                    DeclKind::Expr(expr) => match &expr.kind {
                        pl::ExprKind::Func(closure) => {
                            let closure = self.fold_function_types(closure.clone())?;

                            let expr = pl::Expr::new(pl::ExprKind::Func(closure));

                            if self.in_func_call_name {
                                expr
                            } else {
                                self.fold_expr(expr)?
                            }
                        }
                        _ => self.fold_expr(expr.as_ref().clone())?,
                    },

                    DeclKind::InstanceOf(_, ty) => {
                        let ty = ty.clone();

                        let fields = self.construct_wildcard_include(&fq_ident);

                        pl::Expr {
                            kind: pl::ExprKind::Tuple(fields),
                            ty,
                            ..node
                        }
                    }

                    DeclKind::Ty(_) => {
                        return Err(Error::new(Reason::Expected {
                            who: None,
                            expected: "a value".to_string(),
                            found: "a type".to_string(),
                        })
                        .with_span(*span));
                    }

                    _ => pl::Expr {
                        kind: pl::ExprKind::Ident(fq_ident),
                        ..node
                    },
                }
            }

            pl::ExprKind::FuncCall(pl::FuncCall { name, args, .. })
                if (name.kind.as_ident()).is_some_and(|i| i.to_string() == "std.not")
                    && matches!(args[0].kind, pl::ExprKind::Tuple(_)) =>
            {
                let arg = args.into_iter().exactly_one().unwrap();
                self.resolve_column_exclusion(arg)?
            }

            pl::ExprKind::FuncCall(pl::FuncCall {
                name,
                args,
                named_args,
            }) => {
                // fold function name
                self.default_namespace = None;
                let old = self.in_func_call_name;
                self.in_func_call_name = true;
                let name = Box::new(self.fold_expr(*name)?);
                self.in_func_call_name = old;

                let func = name.try_cast(|n| n.into_func(), None, "a function")?;

                // fold function
                let func = self.apply_args_to_closure(func, args, named_args)?;
                self.fold_function(func, *span)?
            }

            pl::ExprKind::Func(closure) => self.fold_function(closure, *span)?,

            pl::ExprKind::Tuple(exprs) => {
                let exprs = self.fold_exprs(exprs)?;

                // flatten
                let exprs = exprs
                    .into_iter()
                    .flat_map(|e| match e.kind {
                        pl::ExprKind::Tuple(items) if e.flatten => items,
                        _ => vec![e],
                    })
                    .collect_vec();

                pl::Expr {
                    kind: pl::ExprKind::Tuple(exprs),
                    ..node
                }
            }

            item => pl::Expr {
                kind: pl::fold_expr_kind(self, item)?,
                ..node
            },
        };
        self.finish_expr_resolve(r, id, *alias, *span)
    }
}

impl Resolver<'_> {
    fn finish_expr_resolve(
        &mut self,
        expr: pl::Expr,
        id: usize,
        alias: Option<String>,
        span: Option<Span>,
    ) -> Result<pl::Expr> {
        let mut r = Box::new(self.maybe_static_eval(expr)?);

        r.id = r.id.or(Some(id));
        r.alias = r.alias.or(alias);
        r.span = r.span.or(span);

        if r.ty.is_none() {
            r.ty = Resolver::infer_type(&r)?;
        }
        if r.lineage.is_none() {
            if let pl::ExprKind::TransformCall(call) = &r.kind {
                r.lineage = Some(call.infer_lineage()?);
            } else if let Some(relation_columns) = r.ty.as_ref().and_then(|t| t.as_relation()) {
                log::debug!("found a relational type without lineage: declaring a new table for it: {relation_columns:?}");

                // lineage from ty
                let columns = Some(relation_columns.clone());

                let name = r.alias.clone();
                let frame = self.declare_table_for_literal(id, columns, name);

                r.lineage = Some(frame);
            }
        }
        if let Some(lineage) = &mut r.lineage {
            if let Some(alias) = r.alias.take() {
                lineage.rename(alias.clone());

                if let Some(ty) = &mut r.ty {
                    types::rename_relation(&mut ty.kind, alias);
                }
            }
        }
        Ok(*r)
    }

    pub fn resolve_column_exclusion(&mut self, expr: pl::Expr) -> Result<pl::Expr> {
        let expr = self.fold_expr(expr)?;
        let except = self.coerce_into_tuple(expr)?;

        self.fold_expr(pl::Expr::new(pl::ExprKind::All {
            within: Box::new(pl::Expr::new(pl::Ident::from_name(NS_THIS))),
            except: Box::new(except),
        }))
    }

    pub fn construct_wildcard_include(&mut self, module_fq_self: &pl::Ident) -> Vec<pl::Expr> {
        let module_fq = module_fq_self.clone().pop().unwrap();

        let decl = self.root_mod.module.get(&module_fq).unwrap();
        let module = decl.kind.as_module().unwrap();

        let prefix = module_fq.iter().collect_vec();
        Self::construct_tuple_from_module(&mut self.id, &prefix, module)
    }

    pub fn construct_tuple_from_module(
        id: &mut IdGenerator<usize>,
        prefix: &[&String],
        module: &Module,
    ) -> Vec<pl::Expr> {
        let mut res = Vec::new();

        if let Some(decl) = module.names.get(NS_INFER) {
            let wildcard_field = pl::Expr {
                id: Some(id.gen()),
                target_id: decl.declared_at,
                flatten: true,
                ty: Some(Ty::new(TyKind::Tuple(vec![TyTupleField::Wildcard(None)]))),
                ..pl::Expr::new(pl::Ident::from_name(NS_SELF))
            };
            return vec![wildcard_field];
        }

        for (name, decl) in module.names.iter().sorted_by_key(|(_, d)| d.order) {
            res.push(match &decl.kind {
                DeclKind::Module(submodule) => {
                    let prefix = [prefix.to_vec(), vec![name]].concat();
                    let sub_fields = Self::construct_tuple_from_module(id, &prefix, submodule);
                    pl::Expr {
                        id: Some(id.gen()),
                        alias: Some(name.clone()),
                        ..pl::Expr::new(pl::ExprKind::Tuple(sub_fields))
                    }
                }
                DeclKind::Column(target_id) => pl::Expr {
                    id: Some(id.gen()),
                    target_id: Some(*target_id),
                    // alias: Some(name.clone()),
                    ..pl::Expr::new(pl::Ident::from_path([prefix.to_vec(), vec![name]].concat()))
                },
                _ => continue,
            });
        }
        res
    }
}

fn ty_of_lineage(lineage: &pl::Lineage) -> Ty {
    Ty::relation(
        lineage
            .columns
            .iter()
            .map(|col| match col {
                pl::LineageColumn::All { .. } => TyTupleField::Wildcard(None),
                pl::LineageColumn::Single { name, .. } => {
                    TyTupleField::Single(name.as_ref().map(|i| i.name.clone()), None)
                }
            })
            .collect(),
    )
}
