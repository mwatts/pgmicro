use turso_parser::ast::{self, SortOrder};

use crate::{
    function::AggFunc,
    schema::{PseudoCursorType, Table},
    translate::collate::{get_collseq_from_expr, CollationSeq},
    util::exprs_are_equivalent,
    vdbe::{
        builder::{CursorType, ProgramBuilder},
        insn::{HashDistinctData, Insn},
        BranchOffset,
    },
    LimboError, Result,
};

use super::{
    emitter::{OperationMode, Resolver, TranslateCtx},
    expr::{
        resolve_expr, translate_condition_expr, translate_expr, translate_expr_no_constant_opt,
        ConditionMetadata, NoConstantOptReason,
    },
    order_by::{custom_type_comparator, sorter_insert},
    plan::{Aggregate, Distinctness, QueryDestination, SelectPlan, TableReferences},
    result_row::emit_select_result,
};

/// True when the ungrouped-aggregation epilogue can emit Copy/ResultRow/Halt inline
/// after AggFinal. This avoids falling through to the init section (start_offset),
/// which re-nulls aggregate registers before copying them.
fn ungrouped_agg_can_emit_inline_result(plan: &SelectPlan, t_ctx: &TranslateCtx<'_>) -> bool {
    if !matches!(plan.query_destination, QueryDestination::ResultRows) {
        return false;
    }
    if plan.limit.is_some() || t_ctx.reg_offset.is_some() {
        return false;
    }
    if matches!(plan.distinctness, Distinctness::Distinct { .. }) {
        return false;
    }
    if plan
        .group_by
        .as_ref()
        .is_some_and(|gb| !gb.exprs.is_empty() || gb.having.as_ref().is_some_and(|h| !h.is_empty()))
    {
        return false;
    }
    // The inline fast path emits Copy+ResultRow directly and skips
    // result_row::emit_array_decode_for_results(), which is what converts an
    // array-typed aggregate's internal Blob payload into PG's `{...}` text
    // form. Bail out to the slower-but-correct emit_select_result() path
    // whenever a result column would need that decode step.
    if plan
        .result_columns
        .iter()
        .any(|rc| super::expr::expr_is_array(&rc.expr, Some(&plan.table_references)))
    {
        return false;
    }
    plan.result_columns.iter().all(|rc| {
        plan.aggregates
            .iter()
            .any(|agg| exprs_are_equivalent(&agg.original_expr, &rc.expr))
    })
}

fn emit_ungrouped_agg_inline_result(
    program: &mut ProgramBuilder,
    agg_start_reg: usize,
    plan: &SelectPlan,
    col_start: usize,
) {
    for (i, rc) in plan.result_columns.iter().enumerate() {
        let Some(agg_idx) = plan
            .aggregates
            .iter()
            .position(|agg| exprs_are_equivalent(&agg.original_expr, &rc.expr))
        else {
            continue;
        };
        let agg_reg = agg_start_reg + agg_idx;
        let dest = col_start + i;
        if agg_reg != dest {
            program.emit_no_constant_insn(Insn::Copy {
                src_reg: agg_reg,
                dst_reg: dest,
                extra_amount: 0,
            });
        }
    }
    program.emit_no_constant_insn(Insn::ResultRow {
        start_reg: col_start,
        count: plan.result_columns.len(),
    });
    program.emit_insn(Insn::Halt {
        err_code: 0,
        description: String::new(),
        on_error: None,
        description_reg: None,
    });
}

/// Metadata for sorting aggregate inputs before accumulation (ungrouped queries).
#[derive(Debug)]
pub struct AggOrderMetadata {
    pub sort_cursor: usize,
    pub reg_sorter_key: usize,
    pub sorter_column_count: usize,
    pub pseudo_cursor: usize,
    pub reg_row_start: usize,
    pub label_sort_loop_start: BranchOffset,
    pub label_sort_loop_end: BranchOffset,
}

pub fn aggregates_need_input_sort(aggregates: &[Aggregate]) -> bool {
    aggregates.iter().any(|agg| !agg.order_by.is_empty())
}

pub(crate) fn aggregate_order_by_sort_key(
    aggregates: &[Aggregate],
) -> Result<&[(ast::Expr, SortOrder, Option<ast::NullsOrder>)]> {
    let Some(key) = aggregates
        .iter()
        .find(|agg| !agg.order_by.is_empty())
        .map(|agg| agg.order_by.as_slice())
    else {
        return Ok(&[]);
    };

    for agg in aggregates {
        if agg.order_by.is_empty() {
            continue;
        }
        if agg.order_by.len() != key.len()
            || !agg
                .order_by
                .iter()
                .zip(key)
                .all(|((a, oa, na), (b, ob, nb))| {
                    exprs_are_equivalent(a, b) && oa == ob && na == nb
                })
        {
            crate::bail_parse_error!("conflicting ORDER BY clauses in aggregate functions");
        }
    }
    Ok(key)
}

pub fn init_ungrouped_agg_order_sorter(
    program: &mut ProgramBuilder,
    t_ctx: &mut TranslateCtx,
    plan: &SelectPlan,
) -> Result<()> {
    let sort_key = aggregate_order_by_sort_key(&plan.aggregates)?;
    let sort_key_len = sort_key.len();
    let agg_arg_count: usize = plan.aggregates.iter().map(|agg| agg.args.len()).sum();
    let sorter_column_count = sort_key_len + 1 + agg_arg_count;

    let mut order_collations_nulls: Vec<(
        SortOrder,
        Option<CollationSeq>,
        Option<ast::NullsOrder>,
    )> = sort_key
        .iter()
        .map(|(expr, order, nulls)| {
            let collation = get_collseq_from_expr(expr, &plan.table_references)?;
            Ok((*order, collation, *nulls))
        })
        .collect::<Result<Vec<_>>>()?;
    order_collations_nulls.push((SortOrder::Asc, Some(CollationSeq::default()), None));

    let mut comparators: Vec<Option<crate::vdbe::insn::SortComparatorType>> = sort_key
        .iter()
        .map(|(expr, _, _)| {
            custom_type_comparator(expr, &plan.table_references, t_ctx.resolver.schema())
        })
        .collect();
    comparators.push(None);

    let sort_cursor = program.alloc_cursor_id(CursorType::Sorter);
    program.emit_insn(Insn::SorterOpen {
        cursor_id: sort_cursor,
        columns: sorter_column_count,
        order_collations_nulls,
        comparators,
    });

    let reg_row_start = program.alloc_registers(sorter_column_count);
    t_ctx.meta_agg_order = Some(AggOrderMetadata {
        sort_cursor,
        reg_sorter_key: program.alloc_register(),
        sorter_column_count,
        pseudo_cursor: program.alloc_cursor_id(CursorType::Pseudo(PseudoCursorType {
            column_count: sorter_column_count,
        })),
        reg_row_start,
        label_sort_loop_start: program.allocate_label(),
        label_sort_loop_end: program.allocate_label(),
    });
    Ok(())
}

pub fn emit_ungrouped_agg_order_insert(
    program: &mut ProgramBuilder,
    t_ctx: &mut TranslateCtx,
    plan: &SelectPlan,
) -> Result<()> {
    let meta = t_ctx
        .meta_agg_order
        .as_ref()
        .expect("aggregate ORDER BY metadata must be initialized");
    let mut cur_reg = meta.reg_row_start;

    for (expr, _, _) in aggregate_order_by_sort_key(&plan.aggregates)? {
        translate_expr(
            program,
            Some(&plan.table_references),
            expr,
            cur_reg,
            &t_ctx.resolver,
        )?;
        cur_reg += 1;
    }

    program.emit_insn(Insn::Sequence {
        cursor_id: meta.sort_cursor,
        target_reg: cur_reg,
    });
    cur_reg += 1;

    for agg in &plan.aggregates {
        for arg in &agg.args {
            translate_expr(
                program,
                Some(&plan.table_references),
                arg,
                cur_reg,
                &t_ctx.resolver,
            )?;
            cur_reg += 1;
        }
    }

    sorter_insert(
        program,
        meta.reg_row_start,
        meta.sorter_column_count,
        meta.sort_cursor,
        meta.reg_sorter_key,
    );
    Ok(())
}

pub fn emit_ungrouped_agg_order_accumulate(
    program: &mut ProgramBuilder,
    t_ctx: &mut TranslateCtx,
    plan: &SelectPlan,
) -> Result<()> {
    let AggOrderMetadata {
        sort_cursor,
        reg_sorter_key,
        sorter_column_count,
        pseudo_cursor,
        label_sort_loop_start,
        label_sort_loop_end,
        ..
    } = t_ctx
        .meta_agg_order
        .as_ref()
        .expect("aggregate ORDER BY metadata");

    let sort_key_len = aggregate_order_by_sort_key(&plan.aggregates)?.len();

    program.emit_insn(Insn::OpenPseudo {
        cursor_id: *pseudo_cursor,
        content_reg: *reg_sorter_key,
        num_fields: *sorter_column_count,
    });
    program.emit_insn(Insn::SorterSort {
        cursor_id: *sort_cursor,
        pc_if_empty: *label_sort_loop_end,
    });
    program.preassign_label_to_next_insn(*label_sort_loop_start);

    program.emit_insn(Insn::SorterData {
        cursor_id: *sort_cursor,
        dest_reg: *reg_sorter_key,
        pseudo_cursor: *pseudo_cursor,
    });

    let mut arg_offset = sort_key_len + 1;
    for (i, agg) in plan.aggregates.iter().enumerate() {
        let agg_start_reg = t_ctx
            .reg_agg_start
            .expect("aggregate registers must be initialized");
        let agg_result_reg = agg_start_reg + i;

        let filter_skip_label = if let Some(filter_expr) = &agg.filter_expr {
            let label = program.allocate_label();
            let filter_reg = program.alloc_register();
            translate_expr(
                program,
                Some(&plan.table_references),
                filter_expr,
                filter_reg,
                &t_ctx.resolver,
            )?;
            program.emit_insn(Insn::IfNot {
                reg: filter_reg,
                target_pc: label,
                jump_if_null: true,
            });
            Some(label)
        } else {
            None
        };

        let arg_reg = program.alloc_register();
        program.emit_column_or_rowid(*pseudo_cursor, arg_offset, arg_reg);
        arg_offset += agg.args.len();

        let agg_arg_source =
            AggArgumentSource::new_from_expression(&agg.func, &agg.args, &agg.distinctness);
        handle_distinct(program, agg_arg_source.distinctness(), arg_reg);
        translate_aggregation_step_from_reg(
            program,
            &plan.table_references,
            agg_arg_source,
            arg_reg,
            agg_result_reg,
            &t_ctx.resolver,
        )?;

        if let Distinctness::Distinct { ctx } = &agg.distinctness {
            let ctx = ctx
                .as_ref()
                .expect("distinct aggregate context not populated");
            program.preassign_label_to_next_insn(ctx.label_on_conflict);
        }

        if let Some(label) = filter_skip_label {
            program.preassign_label_to_next_insn(label);
        }
    }

    program.emit_insn(Insn::SorterNext {
        cursor_id: *sort_cursor,
        pc_if_next: *label_sort_loop_start,
    });
    program.preassign_label_to_next_insn(*label_sort_loop_end);
    Ok(())
}

fn translate_aggregation_step_from_reg(
    program: &mut ProgramBuilder,
    referenced_tables: &TableReferences,
    agg_arg_source: AggArgumentSource,
    arg_reg: usize,
    target_register: usize,
    resolver: &Resolver,
) -> Result<()> {
    let func = agg_arg_source.agg_func();
    let num_args = agg_arg_source.num_args();
    let delimiter_reg = match func {
        AggFunc::GroupConcat if num_args == 2 => {
            agg_arg_source.translate(program, referenced_tables, resolver, 1)?
        }
        AggFunc::StringAgg if num_args == 2 => {
            agg_arg_source.translate(program, referenced_tables, resolver, 1)?
        }
        _ => 0,
    };

    if matches!(func, AggFunc::ArrayAgg) {
        resolver.require_custom_types("Array features")?;
    }

    let comparator = match func {
        AggFunc::Min | AggFunc::Max => {
            let expr = agg_arg_source.arg_at(0);
            emit_collseq_if_needed(program, referenced_tables, expr);
            super::order_by::custom_type_comparator(expr, referenced_tables, resolver.schema())
        }
        _ => None,
    };

    program.emit_insn(Insn::AggStep {
        acc_reg: target_register,
        col: arg_reg,
        delimiter: delimiter_reg,
        func: func.clone(),
        comparator,
    });
    program.reset_collation();
    Ok(())
}

/// Emits the bytecode for processing an aggregate without a GROUP BY clause.
/// This is called when the main query execution loop has finished processing,
/// and we can now materialize the aggregate results.
pub fn emit_ungrouped_aggregation<'a>(
    program: &mut ProgramBuilder,
    t_ctx: &mut TranslateCtx<'a>,
    plan: &'a SelectPlan,
) -> Result<()> {
    let agg_start_reg = t_ctx.reg_agg_start.unwrap();

    for (i, agg) in plan.aggregates.iter().enumerate() {
        let agg_result_reg = agg_start_reg + i;
        program.emit_insn(Insn::AggFinal {
            register: agg_result_reg,
            func: agg.func.clone(),
        });
    }
    // we now have the agg results in (agg_start_reg..agg_start_reg + aggregates.len() - 1)
    // we need to call translate_expr on each result column, but replace the expr with a register copy in case any part of the
    // result column expression matches a) a group by column or b) an aggregation result.
    for (i, agg) in plan.aggregates.iter().enumerate() {
        t_ctx.resolver.cache_expr_reg(
            std::borrow::Cow::Borrowed(&agg.original_expr),
            agg_start_reg + i,
            false,
            None,
        );
    }
    t_ctx.resolver.enable_expr_to_reg_cache();

    // Allocate a label for the end (used by both HAVING and OFFSET to skip row emission)
    let end_label = program.allocate_label();

    // Handle HAVING clause without GROUP BY for ungrouped aggregation
    if let Some(group_by) = &plan.group_by {
        if group_by.exprs.is_empty() {
            if let Some(having) = &group_by.having {
                for expr in having.iter() {
                    let if_true_target = program.allocate_label();
                    translate_condition_expr(
                        program,
                        &plan.table_references,
                        expr,
                        ConditionMetadata {
                            jump_if_condition_is_true: false,
                            jump_target_when_false: end_label,
                            jump_target_when_true: if_true_target,
                            // treat null result as false
                            jump_target_when_null: end_label,
                        },
                        &t_ctx.resolver,
                    )?;
                    program.preassign_label_to_next_insn(if_true_target);
                }
            }
        }
    }

    // Handle OFFSET for ungrouped aggregates
    // Since we only have one result row, either skip it (offset > 0) or emit it
    if let Some(offset_reg) = t_ctx.reg_offset {
        // If offset > 0, jump to end (skip the single row)
        program.emit_insn(Insn::IfPos {
            reg: offset_reg,
            target_pc: end_label,
            decrement_by: 0,
        });
    }

    // If the loop never ran (once-flag is still 0), we need to evaluate non-aggregate columns now.
    // This ensures literals return their values and column references return NULL (since no
    // rows matched). The once-flag mechanism normally evaluates non-agg columns on first
    // iteration, but if there were no iterations, we must do it here.
    //
    // We must emit NullRow for all table cursors first, because after a WHERE-filter
    // jump-out the cursor may still be positioned on a valid (but non-matching) row.
    // Without NullRow, Column instructions would read stale data from that row instead
    // of returning NULL.
    if let Some(once_flag) = t_ctx.reg_nonagg_emit_once_flag {
        let skip_nonagg_eval = program.allocate_label();
        // If once-flag is non-zero (loop ran at least once), skip evaluation
        program.emit_insn(Insn::If {
            reg: once_flag,
            target_pc: skip_nonagg_eval,
            jump_if_null: false,
        });
        // Set all table cursors to NullRow so that Column instructions return NULL
        // instead of leaking stale values from the last scanned (but non-matching) row.
        // Also null out coroutine output registers for CTEs/subqueries.
        for table_ref in plan.table_references.joined_tables() {
            let (table_cursor_id, index_cursor_id) =
                table_ref.resolve_cursors(program, OperationMode::SELECT)?;
            for cursor_id in [table_cursor_id, index_cursor_id].into_iter().flatten() {
                program.emit_insn(Insn::NullRow { cursor_id });
            }
            if let Table::FromClauseSubquery(subquery) = &table_ref.table {
                if let Some(start_reg) = subquery.result_columns_start_reg {
                    let num_cols = subquery.columns.len();
                    if num_cols > 0 {
                        program.emit_insn(Insn::Null {
                            dest: start_reg,
                            dest_end: if num_cols > 1 {
                                Some(start_reg + num_cols - 1)
                            } else {
                                None
                            },
                        });
                    }
                }
            }
        }
        // Evaluate non-aggregate columns now (with cursor in invalid state, columns return NULL)
        // Must use no_constant_opt to prevent constant hoisting which would place the label
        // after the hoisted constants, causing infinite loops in compound selects.
        let col_start = t_ctx.reg_result_cols_start.unwrap();
        for (i, rc) in plan.result_columns.iter().enumerate() {
            if !rc.contains_aggregates {
                translate_expr_no_constant_opt(
                    program,
                    Some(&plan.table_references),
                    &rc.expr,
                    col_start + i,
                    &t_ctx.resolver,
                    NoConstantOptReason::RegisterReuse,
                )?;
            }
        }
        program.preassign_label_to_next_insn(skip_nonagg_eval);
    }

    let col_start = t_ctx.reg_result_cols_start.unwrap();
    if ungrouped_agg_can_emit_inline_result(plan, t_ctx) {
        emit_ungrouped_agg_inline_result(program, agg_start_reg, plan, col_start);
    } else {
        // Emit the result row (if we didn't skip it due to HAVING or OFFSET)
        emit_select_result(
            program,
            &t_ctx.resolver,
            plan,
            None,
            None,
            t_ctx.reg_nonagg_emit_once_flag,
            None, // we've already handled offset
            col_start,
            t_ctx.limit_ctx,
        )?;
    }

    // Resolve the SELECT DISTINCT label if present
    // When a duplicate is found by the Found instruction, jump here to skip emitting the row
    if let Distinctness::Distinct { ctx } = &plan.distinctness {
        let distinct_ctx = ctx.as_ref().expect("distinct context must exist");
        program.preassign_label_to_next_insn(distinct_ctx.label_on_conflict);
    }

    program.preassign_label_to_next_insn(end_label);

    Ok(())
}

pub(crate) fn emit_collseq_if_needed(
    program: &mut ProgramBuilder,
    referenced_tables: &TableReferences,
    expr: &ast::Expr,
) {
    // Check if this is a column expression with explicit COLLATE clause
    if let ast::Expr::Collate(_, collation_name) = expr {
        if let Ok(collation) = CollationSeq::new(collation_name.as_str()) {
            program.emit_insn(Insn::CollSeq {
                reg: None,
                collation,
            });
        }
        return;
    }

    // If no explicit collation, check if this is a column with table-defined collation
    if let ast::Expr::Column { table, column, .. } = expr {
        if let Some((_, table_ref)) = referenced_tables.find_table_by_internal_id(*table) {
            if let Some(table_column) = table_ref.get_column_at(*column) {
                if let Some(c) = table_column.collation_opt() {
                    program.emit_insn(Insn::CollSeq {
                        reg: None,
                        collation: c,
                    });
                    return;
                }
            }
        }
    }

    // Always emit a CollSeq to reset to BINARY default, preventing collation
    // from a previous aggregate leaking into this one.
    program.emit_insn(Insn::CollSeq {
        reg: None,
        collation: CollationSeq::Binary,
    });
}

/// Emits the bytecode for handling duplicates in a distinct aggregate.
/// This is used in both GROUP BY and non-GROUP BY aggregations to jump over
/// the AggStep that would otherwise accumulate the same value multiple times.
pub fn handle_distinct(
    program: &mut ProgramBuilder,
    distinctness: &Distinctness,
    agg_arg_reg: usize,
) {
    let Distinctness::Distinct { ctx } = distinctness else {
        return;
    };
    let distinct_ctx = ctx
        .as_ref()
        .expect("distinct aggregate context not populated");
    let num_regs = 1;
    program.emit_insn(Insn::HashDistinct {
        data: Box::new(HashDistinctData {
            hash_table_id: distinct_ctx.hash_table_id,
            key_start_reg: agg_arg_reg,
            num_keys: num_regs,
            collations: distinct_ctx.collations.clone(),
            target_pc: distinct_ctx.label_on_conflict,
        }),
    });
}

/// Source of aggregate function arguments during bytecode emission.
///
/// * `Register`: arguments were pre-computed into contiguous registers
///   (used for GROUP BY without a sorter, where the main loop is already sorted).
/// * `Expression`: arguments are evaluated on-the-fly from the original AST
///   (used for ungrouped aggregates, window functions, and for the GROUP BY sorter
///   path where leaf columns are cached in `expr_to_reg_cache` before evaluation).
pub enum AggArgumentSource<'a> {
    Register {
        src_reg_start: usize,
        aggregate: &'a Aggregate,
    },
    Expression {
        func: &'a AggFunc,
        args: &'a Vec<ast::Expr>,
        distinctness: &'a Distinctness,
    },
}

impl<'a> AggArgumentSource<'a> {
    pub fn new_from_registers(src_reg_start: usize, aggregate: &'a Aggregate) -> Self {
        Self::Register {
            src_reg_start,
            aggregate,
        }
    }

    pub fn new_from_expression(
        func: &'a AggFunc,
        args: &'a Vec<ast::Expr>,
        distinctness: &'a Distinctness,
    ) -> Self {
        Self::Expression {
            func,
            args,
            distinctness,
        }
    }

    pub fn distinctness(&self) -> &Distinctness {
        match self {
            AggArgumentSource::Register { aggregate, .. } => &aggregate.distinctness,
            AggArgumentSource::Expression { distinctness, .. } => distinctness,
        }
    }

    pub fn agg_func(&self) -> &AggFunc {
        match self {
            AggArgumentSource::Register { aggregate, .. } => &aggregate.func,
            AggArgumentSource::Expression { func, .. } => func,
        }
    }

    pub fn arg_at(&self, idx: usize) -> &ast::Expr {
        match self {
            AggArgumentSource::Register { aggregate, .. } => &aggregate.args[idx],
            AggArgumentSource::Expression { args, .. } => &args[idx],
        }
    }

    pub fn num_args(&self) -> usize {
        match self {
            AggArgumentSource::Register { aggregate, .. } => aggregate.args.len(),
            AggArgumentSource::Expression { args, .. } => args.len(),
        }
    }

    /// Emit bytecode to read an aggregate function argument into a register.
    pub fn translate(
        &self,
        program: &mut ProgramBuilder,
        referenced_tables: &TableReferences,
        resolver: &Resolver,
        arg_idx: usize,
    ) -> Result<usize> {
        match self {
            AggArgumentSource::Register {
                src_reg_start: start_reg,
                ..
            } => Ok(*start_reg + arg_idx),
            AggArgumentSource::Expression { args, .. } => {
                resolve_expr(program, Some(referenced_tables), &args[arg_idx], resolver)
            }
        }
    }
}

/// Emits the bytecode for processing an aggregate step.
///
/// This is distinct from the final step, which is called after a single group has been entirely accumulated,
/// and the actual result value of the aggregation is materialized.
///
/// Ungrouped aggregation is a special case of grouped aggregation that involves a single group.
///
/// Examples:
/// * In `SELECT SUM(price) FROM t`, `price` is evaluated for each row and added to the accumulator.
/// * In `SELECT product_category, SUM(price) FROM t GROUP BY product_category`, `price` is evaluated for
///   each row in the group and added to that group’s accumulator.
pub fn translate_aggregation_step(
    program: &mut ProgramBuilder,
    referenced_tables: &TableReferences,
    agg_arg_source: AggArgumentSource,
    target_register: usize,
    resolver: &Resolver,
) -> Result<usize> {
    let num_args = agg_arg_source.num_args();
    let func = agg_arg_source.agg_func();
    let dest = match func {
        AggFunc::Avg => {
            if num_args != 1 {
                crate::bail_parse_error!("avg bad number of arguments");
            }
            let expr_reg = agg_arg_source.translate(program, referenced_tables, resolver, 0)?;
            handle_distinct(program, agg_arg_source.distinctness(), expr_reg);
            program.emit_insn(Insn::AggStep {
                acc_reg: target_register,
                col: expr_reg,
                delimiter: 0,
                func: AggFunc::Avg,
                comparator: None,
            });
            target_register
        }
        AggFunc::Count0 => {
            let expr = ast::Expr::Literal(ast::Literal::Numeric("1".to_string()));
            let expr_reg = translate_const_arg(program, referenced_tables, resolver, &expr)?;
            handle_distinct(program, agg_arg_source.distinctness(), expr_reg);
            program.emit_insn(Insn::AggStep {
                acc_reg: target_register,
                col: expr_reg,
                delimiter: 0,
                func: AggFunc::Count0,
                comparator: None,
            });
            target_register
        }
        AggFunc::Count => {
            if num_args != 1 {
                crate::bail_parse_error!("count bad number of arguments");
            }
            let expr_reg = agg_arg_source.translate(program, referenced_tables, resolver, 0)?;
            handle_distinct(program, agg_arg_source.distinctness(), expr_reg);
            program.emit_insn(Insn::AggStep {
                acc_reg: target_register,
                col: expr_reg,
                delimiter: 0,
                func: AggFunc::Count,
                comparator: None,
            });
            target_register
        }
        AggFunc::GroupConcat => {
            if num_args != 1 && num_args != 2 {
                crate::bail_parse_error!("group_concat bad number of arguments");
            }

            let delimiter_reg = if num_args == 2 {
                agg_arg_source.translate(program, referenced_tables, resolver, 1)?
            } else {
                let delimiter_expr =
                    ast::Expr::Literal(ast::Literal::String(String::from("\",\"")));
                translate_const_arg(program, referenced_tables, resolver, &delimiter_expr)?
            };

            let expr_reg = agg_arg_source.translate(program, referenced_tables, resolver, 0)?;
            handle_distinct(program, agg_arg_source.distinctness(), expr_reg);

            program.emit_insn(Insn::AggStep {
                acc_reg: target_register,
                col: expr_reg,
                delimiter: delimiter_reg,
                func: AggFunc::GroupConcat,
                comparator: None,
            });

            target_register
        }
        AggFunc::Max => {
            if num_args != 1 {
                crate::bail_parse_error!("max bad number of arguments");
            }
            let expr_reg = agg_arg_source.translate(program, referenced_tables, resolver, 0)?;
            handle_distinct(program, agg_arg_source.distinctness(), expr_reg);
            let expr = &agg_arg_source.arg_at(0);
            emit_collseq_if_needed(program, referenced_tables, expr);
            let comparator =
                super::order_by::custom_type_comparator(expr, referenced_tables, resolver.schema());
            program.emit_insn(Insn::AggStep {
                acc_reg: target_register,
                col: expr_reg,
                delimiter: 0,
                func: AggFunc::Max,
                comparator,
            });
            target_register
        }
        AggFunc::Min => {
            if num_args != 1 {
                crate::bail_parse_error!("min bad number of arguments");
            }
            let expr_reg = agg_arg_source.translate(program, referenced_tables, resolver, 0)?;
            handle_distinct(program, agg_arg_source.distinctness(), expr_reg);
            let expr = &agg_arg_source.arg_at(0);
            emit_collseq_if_needed(program, referenced_tables, expr);
            let comparator =
                super::order_by::custom_type_comparator(expr, referenced_tables, resolver.schema());
            program.emit_insn(Insn::AggStep {
                acc_reg: target_register,
                col: expr_reg,
                delimiter: 0,
                func: AggFunc::Min,
                comparator,
            });
            target_register
        }
        #[cfg(feature = "json")]
        AggFunc::JsonGroupObject | AggFunc::JsonbGroupObject => {
            if num_args != 2 {
                crate::bail_parse_error!("max bad number of arguments");
            }
            let expr_reg = agg_arg_source.translate(program, referenced_tables, resolver, 0)?;
            handle_distinct(program, agg_arg_source.distinctness(), expr_reg);
            let value_reg = agg_arg_source.translate(program, referenced_tables, resolver, 1)?;

            program.emit_insn(Insn::AggStep {
                acc_reg: target_register,
                col: expr_reg,
                delimiter: value_reg,
                func: AggFunc::JsonGroupObject,
                comparator: None,
            });
            target_register
        }
        #[cfg(feature = "json")]
        AggFunc::JsonGroupArray | AggFunc::JsonbGroupArray => {
            if num_args != 1 {
                crate::bail_parse_error!("max bad number of arguments");
            }
            let expr_reg = agg_arg_source.translate(program, referenced_tables, resolver, 0)?;
            handle_distinct(program, agg_arg_source.distinctness(), expr_reg);
            program.emit_insn(Insn::AggStep {
                acc_reg: target_register,
                col: expr_reg,
                delimiter: 0,
                func: AggFunc::JsonGroupArray,
                comparator: None,
            });
            target_register
        }
        AggFunc::StringAgg => {
            if num_args != 2 {
                crate::bail_parse_error!("string_agg bad number of arguments");
            }

            let expr_reg = agg_arg_source.translate(program, referenced_tables, resolver, 0)?;
            let delimiter_reg =
                agg_arg_source.translate(program, referenced_tables, resolver, 1)?;

            program.emit_insn(Insn::AggStep {
                acc_reg: target_register,
                col: expr_reg,
                delimiter: delimiter_reg,
                func: AggFunc::StringAgg,
                comparator: None,
            });

            target_register
        }
        AggFunc::Sum => {
            if num_args != 1 {
                crate::bail_parse_error!("sum bad number of arguments");
            }
            let expr_reg = agg_arg_source.translate(program, referenced_tables, resolver, 0)?;
            handle_distinct(program, agg_arg_source.distinctness(), expr_reg);
            program.emit_insn(Insn::AggStep {
                acc_reg: target_register,
                col: expr_reg,
                delimiter: 0,
                func: AggFunc::Sum,
                comparator: None,
            });
            target_register
        }
        AggFunc::Total => {
            if num_args != 1 {
                crate::bail_parse_error!("total bad number of arguments");
            }
            let expr_reg = agg_arg_source.translate(program, referenced_tables, resolver, 0)?;
            handle_distinct(program, agg_arg_source.distinctness(), expr_reg);
            program.emit_insn(Insn::AggStep {
                acc_reg: target_register,
                col: expr_reg,
                delimiter: 0,
                func: AggFunc::Total,
                comparator: None,
            });
            target_register
        }
        AggFunc::ArrayAgg => {
            resolver.require_custom_types("Array features")?;
            if num_args != 1 {
                crate::bail_parse_error!("array_agg bad number of arguments");
            }
            let expr_reg = agg_arg_source.translate(program, referenced_tables, resolver, 0)?;
            handle_distinct(program, agg_arg_source.distinctness(), expr_reg);
            program.emit_insn(Insn::AggStep {
                acc_reg: target_register,
                col: expr_reg,
                delimiter: 0,
                func: AggFunc::ArrayAgg,
                comparator: None,
            });
            target_register
        }
        AggFunc::External(ref func) => {
            let argc = func.agg_args().map_err(|_| {
                LimboError::ExtensionError(
                    "External aggregate function called with wrong number of arguments".to_string(),
                )
            })?;
            if argc != num_args {
                crate::bail_parse_error!(
                    "External aggregate function called with wrong number of arguments"
                );
            }
            let expr_reg = agg_arg_source.translate(program, referenced_tables, resolver, 0)?;
            for i in 0..argc {
                if i != 0 {
                    let _ = agg_arg_source.translate(program, referenced_tables, resolver, i)?;
                }
                // invariant: distinct aggregates are only supported for single-argument functions
                if argc == 1 {
                    handle_distinct(program, agg_arg_source.distinctness(), expr_reg + i);
                }
            }
            program.emit_insn(Insn::AggStep {
                acc_reg: target_register,
                col: expr_reg,
                delimiter: 0,
                func: AggFunc::External(func.clone()),
                comparator: None,
            });
            target_register
        }
    };
    // Aggregate arguments can carry column or explicit COLLATE metadata for the
    // aggregate's internal comparator, but that state must not leak to the
    // surrounding expression that consumes the aggregate result.
    program.reset_collation();
    Ok(dest)
}

fn translate_const_arg(
    program: &mut ProgramBuilder,
    referenced_tables: &TableReferences,
    resolver: &Resolver,
    expr: &ast::Expr,
) -> Result<usize> {
    let target_register = program.alloc_register();
    translate_expr(
        program,
        Some(referenced_tables),
        expr,
        target_register,
        resolver,
    )
}
