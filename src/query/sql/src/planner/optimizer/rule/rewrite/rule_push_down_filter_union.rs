// Copyright 2021 Datafuse Labs
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::sync::Arc;

use ahash::HashMap;
use common_exception::ErrorCode;
use common_exception::Result;

use crate::binder::ColumnBindingBuilder;
use crate::optimizer::rule::Rule;
use crate::optimizer::rule::TransformResult;
use crate::optimizer::RuleID;
use crate::optimizer::SExpr;
use crate::plans::AggregateFunction;
use crate::plans::BoundColumnRef;
use crate::plans::CastExpr;
use crate::plans::Filter;
use crate::plans::FunctionCall;
use crate::plans::LagLeadFunction;
use crate::plans::NthValueFunction;
use crate::plans::PatternPlan;
use crate::plans::RelOp;
use crate::plans::ScalarExpr;
use crate::plans::UnionAll;
use crate::plans::WindowFunc;
use crate::plans::WindowFuncType;
use crate::plans::WindowOrderBy;
use crate::IndexType;
use crate::Visibility;

// For a union query, it's not allowed to add `filter` after union
// Such as: `(select * from t1 union all select * from t2) where a > 1`, it's invalid.
// However, it's possible to have `filter` after `union` when involved `view`
// Such as: `create view v_t as (select * from t1 union all select * from t2)`.
// Then use the view with filter, `select * from v_t where a > 1`;
// So it'll be efficient to push down `filter` to `union`, reduce the size of data to pull from table.
pub struct RulePushDownFilterUnion {
    id: RuleID,
    patterns: Vec<SExpr>,
}

impl RulePushDownFilterUnion {
    pub fn new() -> Self {
        Self {
            id: RuleID::PushDownFilterUnion,
            // Filter
            //  \
            //   UnionAll
            //     /  \
            //   ...   ...
            patterns: vec![SExpr::create_unary(
                Arc::new(
                    PatternPlan {
                        plan_type: RelOp::Filter,
                    }
                    .into(),
                ),
                Arc::new(SExpr::create_binary(
                    Arc::new(
                        PatternPlan {
                            plan_type: RelOp::UnionAll,
                        }
                        .into(),
                    ),
                    Arc::new(SExpr::create_leaf(Arc::new(
                        PatternPlan {
                            plan_type: RelOp::Pattern,
                        }
                        .into(),
                    ))),
                    Arc::new(SExpr::create_leaf(Arc::new(
                        PatternPlan {
                            plan_type: RelOp::Pattern,
                        }
                        .into(),
                    ))),
                )),
            )],
        }
    }
}

impl Rule for RulePushDownFilterUnion {
    fn id(&self) -> RuleID {
        self.id
    }

    fn apply(&self, s_expr: &SExpr, state: &mut TransformResult) -> Result<()> {
        let filter: Filter = s_expr.plan().clone().try_into()?;
        let union_s_expr = s_expr.child(0)?;
        let union: UnionAll = union_s_expr.plan().clone().try_into()?;

        // Create a filter which matches union's right child.
        let index_pairs: HashMap<IndexType, IndexType> =
            union.pairs.iter().map(|pair| (pair.0, pair.1)).collect();
        let new_predicates = filter
            .predicates
            .iter()
            .map(|predicate| replace_column_binding(&index_pairs, predicate.clone()))
            .collect::<Result<Vec<_>>>()?;
        let right_filer = Filter {
            predicates: new_predicates,
            is_having: filter.is_having,
        };

        let mut union_left_child = union_s_expr.child(0)?.clone();
        let mut union_right_child = union_s_expr.child(1)?.clone();

        // Add filter to union children
        union_left_child = SExpr::create_unary(Arc::new(filter.into()), Arc::new(union_left_child));
        union_right_child =
            SExpr::create_unary(Arc::new(right_filer.into()), Arc::new(union_right_child));

        let result = SExpr::create_binary(
            Arc::new(union.into()),
            Arc::new(union_left_child),
            Arc::new(union_right_child),
        );
        state.add_result(result);

        Ok(())
    }

    fn patterns(&self) -> &Vec<SExpr> {
        &self.patterns
    }
}

fn replace_column_binding(
    index_pairs: &HashMap<IndexType, IndexType>,
    scalar: ScalarExpr,
) -> Result<ScalarExpr> {
    match scalar {
        ScalarExpr::BoundColumnRef(column) => {
            let index = column.column.index;
            if index_pairs.contains_key(&index) {
                let new_column = ColumnBindingBuilder::new(
                    column.column.column_name.clone(),
                    *index_pairs.get(&index).unwrap(),
                    column.column.data_type,
                    Visibility::Visible,
                )
                .virtual_computed_expr(column.column.virtual_computed_expr.clone())
                .build();
                return Ok(ScalarExpr::BoundColumnRef(BoundColumnRef {
                    span: column.span,
                    column: new_column,
                }));
            }
            Ok(ScalarExpr::BoundColumnRef(column))
        }
        constant_expr @ ScalarExpr::ConstantExpr(_) => Ok(constant_expr),
        ScalarExpr::WindowFunction(expr) => Ok(ScalarExpr::WindowFunction(WindowFunc {
            span: expr.span,
            display_name: expr.display_name,
            func: match expr.func {
                WindowFuncType::Aggregate(arg) => WindowFuncType::Aggregate(AggregateFunction {
                    display_name: arg.display_name,
                    func_name: arg.func_name,
                    distinct: arg.distinct,
                    params: arg.params,
                    args: arg
                        .args
                        .into_iter()
                        .map(|arg| replace_column_binding(index_pairs, arg))
                        .collect::<Result<Vec<_>>>()?,
                    return_type: arg.return_type,
                }),
                WindowFuncType::LagLead(ll) => {
                    let new_arg = replace_column_binding(index_pairs, *ll.arg)?;
                    let new_default = match &ll.default {
                        None => None,
                        Some(d) => Some(Box::new(replace_column_binding(index_pairs, *d.clone())?)),
                    };
                    WindowFuncType::LagLead(LagLeadFunction {
                        is_lag: ll.is_lag,
                        arg: Box::new(new_arg),
                        offset: ll.offset,
                        default: new_default,
                        return_type: ll.return_type.clone(),
                    })
                }
                WindowFuncType::NthValue(func) => {
                    let new_arg = replace_column_binding(index_pairs, *func.arg)?;
                    WindowFuncType::NthValue(NthValueFunction {
                        n: func.n,
                        arg: Box::new(new_arg),
                        return_type: func.return_type.clone(),
                    })
                }
                t => t,
            },
            partition_by: expr
                .partition_by
                .into_iter()
                .map(|p| replace_column_binding(index_pairs, p))
                .collect::<Result<Vec<_>>>()?,
            order_by: expr
                .order_by
                .into_iter()
                .map(|p| {
                    Ok(WindowOrderBy {
                        expr: replace_column_binding(index_pairs, p.expr)?,
                        asc: p.asc,
                        nulls_first: p.nulls_first,
                    })
                })
                .collect::<Result<Vec<_>>>()?,
            frame: expr.frame,
        })),
        ScalarExpr::AggregateFunction(expr) => {
            Ok(ScalarExpr::AggregateFunction(AggregateFunction {
                display_name: expr.display_name,
                func_name: expr.func_name,
                distinct: expr.distinct,
                params: expr.params,
                args: expr
                    .args
                    .into_iter()
                    .map(|arg| replace_column_binding(index_pairs, arg))
                    .collect::<Result<Vec<_>>>()?,
                return_type: expr.return_type,
            }))
        }
        ScalarExpr::FunctionCall(expr) => Ok(ScalarExpr::FunctionCall(FunctionCall {
            span: expr.span,
            func_name: expr.func_name,
            params: expr.params,
            arguments: expr
                .arguments
                .into_iter()
                .map(|arg| replace_column_binding(index_pairs, arg))
                .collect::<Result<Vec<_>>>()?,
        })),
        ScalarExpr::CastExpr(expr) => Ok(ScalarExpr::CastExpr(CastExpr {
            span: expr.span,
            is_try: expr.is_try,
            argument: Box::new(replace_column_binding(index_pairs, *(expr.argument))?),
            target_type: expr.target_type,
        })),
        ScalarExpr::SubqueryExpr(_) => Err(ErrorCode::Unimplemented(
            "replace_column_binding: don't support subquery",
        )),
    }
}
