// Copyright 2021 Datafuse Labs.
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

use common_exception::ErrorCode;
use common_exception::Result;
use common_planners::Expression;

use crate::sessions::QueryContext;
use crate::sql::statements::query::query_schema_joined::JoinedTableDesc;
use crate::sql::statements::query::JoinedSchema;
use crate::sql::statements::QueryASTIR;

pub struct QualifiedRewriter {
    tables_schema: JoinedSchema,
    ctx: Arc<QueryContext>,
}

impl QualifiedRewriter {
    pub fn create(tables_schema: JoinedSchema, ctx: Arc<QueryContext>) -> QualifiedRewriter {
        QualifiedRewriter { tables_schema, ctx }
    }

    pub async fn rewrite(&self, mut ir: QueryASTIR) -> Result<QueryASTIR> {
        self.rewrite_group(&mut ir)?;
        self.rewrite_order(&mut ir)?;
        self.rewrite_aggregate(&mut ir)?;
        self.rewrite_projection(&mut ir)?;

        if let Some(predicate) = &ir.filter_predicate {
            match self.rewrite_expr(predicate) {
                Ok(predicate) => {
                    ir.filter_predicate = Some(predicate);
                }
                Err(cause) => {
                    return Err(cause.add_message_back(format!(
                        " (while in analyze filter predicate {:?})",
                        predicate
                    )));
                }
            }
        }

        if let Some(predicate) = &ir.having_predicate {
            match self.rewrite_expr(predicate) {
                Ok(predicate) => {
                    ir.having_predicate = Some(predicate);
                }
                Err(cause) => {
                    return Err(cause.add_message_back(format!(
                        " (while in analyze having predicate {:?})",
                        predicate
                    )));
                }
            }
        }

        Ok(ir)
    }

    fn rewrite_group(&self, mut ir: &mut QueryASTIR) -> Result<()> {
        let mut group_expressions = Vec::with_capacity(ir.group_by_expressions.len());

        for group_by_expression in &ir.group_by_expressions {
            match self.rewrite_expr(group_by_expression) {
                Ok(expr) => {
                    group_expressions.push(expr);
                }
                Err(cause) => {
                    return Err(cause.add_message_back(format!(
                        " (while in analyze group expr: {:?})",
                        group_by_expression
                    )));
                }
            }
        }

        ir.group_by_expressions = group_expressions;
        Ok(())
    }

    fn rewrite_aggregate(&self, mut ir: &mut QueryASTIR) -> Result<()> {
        let mut aggregate_expressions = Vec::with_capacity(ir.aggregate_expressions.len());

        for aggregate_expression in &ir.aggregate_expressions {
            match self.rewrite_expr(aggregate_expression) {
                Ok(expr) => {
                    aggregate_expressions.push(expr);
                }
                Err(cause) => {
                    return Err(cause.add_message_back(format!(
                        " (while in analyze aggregate expr: {:?})",
                        aggregate_expression
                    )));
                }
            }
        }

        ir.aggregate_expressions = aggregate_expressions;
        Ok(())
    }

    fn rewrite_order(&self, mut ir: &mut QueryASTIR) -> Result<()> {
        let mut order_expressions = Vec::with_capacity(ir.order_by_expressions.len());

        for order_by_expression in &ir.order_by_expressions {
            match self.rewrite_expr(order_by_expression) {
                Ok(expr) => {
                    order_expressions.push(expr);
                }
                Err(cause) => {
                    return Err(cause.add_message_back(format!(
                        " (while in analyze order expr: {:?})",
                        order_by_expression
                    )));
                }
            }
        }

        ir.order_by_expressions = order_expressions;
        Ok(())
    }

    fn rewrite_projection(&self, mut ir: &mut QueryASTIR) -> Result<()> {
        let mut projection_expressions = Vec::with_capacity(ir.projection_expressions.len());

        // TODO: alias.*
        for projection_expression in &ir.projection_expressions {
            if let Expression::Alias(_, x) = projection_expression {
                if let Expression::Wildcard = x.as_ref() {
                    return Err(ErrorCode::SyntaxException("* AS alias is wrong syntax"));
                }
            }

            match projection_expression {
                Expression::Wildcard => self.expand_wildcard(&mut projection_expressions),
                _ => match self.rewrite_expr(projection_expression) {
                    Ok(expr) => {
                        projection_expressions.push(expr);
                    }
                    Err(cause) => {
                        return Err(cause.add_message_back(format!(
                            " (while in analyze projection expr: {:?})",
                            projection_expression
                        )));
                    }
                },
            }
        }

        ir.projection_expressions = projection_expressions;
        Ok(())
    }

    fn expand_wildcard(&self, columns_expression: &mut Vec<Expression>) {
        for table_desc in self.tables_schema.get_tables_desc() {
            for column_desc in table_desc.get_columns_desc() {
                let name = column_desc.short_name.clone();
                match column_desc.is_ambiguity {
                    true => {
                        let prefix = table_desc.get_name_parts().join(".");
                        columns_expression.push(Expression::Column(format!("{}.{}", prefix, name)));
                    }
                    false => columns_expression.push(Expression::Column(name)),
                }
            }
        }
    }

    fn rewrite_expr(&self, expr: &Expression) -> Result<Expression> {
        match expr {
            Expression::Column(v) => match self.tables_schema.contains_column(v) {
                true => Ok(Expression::Column(v.clone())),
                false => Err(ErrorCode::UnknownColumn(format!("Unknown column {}", v))),
            },
            Expression::QualifiedColumn(names) => self.rewrite_qualified_column(names),
            Expression::Alias(alias, expr) => Ok(Expression::Alias(
                alias.clone(),
                Box::new(self.rewrite_expr(expr)?),
            )),
            Expression::UnaryExpression { op, expr } => Ok(Expression::UnaryExpression {
                op: op.clone(),
                expr: Box::new(self.rewrite_expr(expr)?),
            }),
            Expression::BinaryExpression { left, op, right } => Ok(Expression::BinaryExpression {
                op: op.clone(),
                left: Box::new(self.rewrite_expr(left)?),
                right: Box::new(self.rewrite_expr(right)?),
            }),
            Expression::ScalarFunction { op, args } => {
                let mut new_args = Vec::with_capacity(args.len());

                for arg in args {
                    new_args.push(self.rewrite_expr(arg)?);
                }

                Ok(Expression::ScalarFunction {
                    op: op.clone(),
                    args: new_args,
                })
            }
            Expression::AggregateFunction {
                op,
                distinct,
                params,
                args,
            } => {
                let mut new_args = Vec::with_capacity(args.len());

                for arg in args {
                    new_args.push(self.rewrite_expr(arg)?);
                }

                Ok(Expression::AggregateFunction {
                    op: op.clone(),
                    distinct: *distinct,
                    params: params.clone(),
                    args: new_args,
                })
            }
            Expression::Sort {
                expr,
                asc,
                nulls_first,
                origin_expr,
            } => Ok(Expression::Sort {
                expr: Box::new(self.rewrite_expr(expr)?),
                asc: *asc,
                nulls_first: *nulls_first,
                origin_expr: Box::new(self.rewrite_expr(origin_expr)?),
            }),
            Expression::Cast { expr, data_type } => Ok(Expression::Cast {
                expr: Box::new(self.rewrite_expr(expr)?),
                data_type: data_type.clone(),
            }),
            Expression::Wildcard
            | Expression::Literal { .. }
            | Expression::Subquery { .. }
            | Expression::ScalarSubquery { .. } => Ok(expr.clone()),
        }
    }

    fn rewrite_qualified_column(&self, ref_names: &[String]) -> Result<Expression> {
        match self.best_match_table(ref_names) {
            None => Err(ErrorCode::UnknownColumn(format!(
                "Unknown column {}",
                ref_names.join(".")
            ))),
            Some((pos, table_ref)) => {
                let column_name = &ref_names[pos..];
                match column_name.len() {
                    1 => Self::find_column(&table_ref, &column_name[0]),
                    // TODO: column.field_a.field_b => GetField(field_b, GetField(field_a, column))
                    _ => Err(ErrorCode::SyntaxException(
                        "Unsupported complex type field access",
                    )),
                }
            }
        }
    }

    fn find_column(table_desc: &JoinedTableDesc, name: &str) -> Result<Expression> {
        let name_parts = table_desc.get_name_parts();
        for column_desc in table_desc.get_columns_desc() {
            if column_desc.short_name == name {
                return match column_desc.is_ambiguity {
                    true => Ok(Expression::Column(format!(
                        "{}.{}",
                        name_parts.join("."),
                        name
                    ))),
                    false => Ok(Expression::Column(name.to_string())),
                };
            }
        }

        Err(ErrorCode::UnknownColumn(format!(
            "Unknown column: {}.{}",
            name_parts.join("."),
            name
        )))
    }

    fn first_diff_pos(left: &[String], right: &[String]) -> usize {
        let min_len = std::cmp::min(left.len(), right.len());

        for index in 0..min_len {
            if left[index] != right[index] {
                return index;
            }
        }

        min_len
    }

    fn best_match_table(&self, ref_names: &[String]) -> Option<(usize, JoinedTableDesc)> {
        if ref_names.len() <= 1 {
            return None;
        }

        let current_database = self.ctx.get_current_database();
        for table_desc in self.tables_schema.get_tables_desc() {
            let name_parts = table_desc.get_name_parts();
            if Self::first_diff_pos(ref_names, name_parts) == name_parts.len() {
                // alias.column or database.table.column
                return Some((name_parts.len(), table_desc.clone()));
            }

            if name_parts.len() > 1
                && Self::first_diff_pos(ref_names, &name_parts[1..]) == 1
                && current_database == name_parts[0]
            {
                // use current_database; table.column
                return Some((1, table_desc.clone()));
            }
        }

        None
    }
}
