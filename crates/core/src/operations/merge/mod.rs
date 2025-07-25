//! Merge data from a source dataset with the target Delta Table based on a join
//! predicate.  A full outer join is performed which results in source and
//! target records that match, source records that do not match, or target
//! records that do not match.
//!
//! Users can specify update, delete, and insert operations for these categories
//! and specify additional predicates for finer control. The order of operations
//! specified matter.  See [`MergeBuilder`] for more information
//!
//! # Example
//! ```rust ignore
//! let table = open_table("../path/to/table")?;
//! let (table, metrics) = DeltaOps(table)
//!     .merge(source, col("target.id").eq(col("source.id")))
//!     .with_source_alias("source")
//!     .with_target_alias("target")
//!     .when_matched_update(|update| {
//!         update
//!             .update("value", col("source.value") + lit(1))
//!             .update("modified", col("source.modified"))
//!     })?
//!     .when_not_matched_insert(|insert| {
//!         insert
//!             .set("id", col("source.id"))
//!             .set("value", col("source.value"))
//!             .set("modified", col("source.modified"))
//!     })?
//!     .await?
//! ````
use std::collections::HashMap;
use std::fmt::Debug;
use std::ops::Deref;
use std::sync::Arc;
use std::time::Instant;

use arrow_schema::{DataType, Field, SchemaBuilder};
use async_trait::async_trait;
use datafusion::common::tree_node::{Transformed, TreeNode};
use datafusion::common::{Column, DFSchema, ExprSchema, ScalarValue, TableReference};
use datafusion::datasource::provider_as_source;
use datafusion::error::Result as DataFusionResult;
use datafusion::execution::context::SessionConfig;
use datafusion::execution::session_state::SessionStateBuilder;
use datafusion::logical_expr::build_join_schema;
use datafusion::logical_expr::execution_props::ExecutionProps;
use datafusion::logical_expr::simplify::SimplifyContext;
use datafusion::logical_expr::{
    col, conditional_expressions::CaseBuilder, lit, when, Expr, JoinType,
};
use datafusion::logical_expr::{
    Extension, LogicalPlan, LogicalPlanBuilder, UserDefinedLogicalNode, UNNAMED_TABLE,
};
use datafusion::optimizer::simplify_expressions::ExprSimplifier;
use datafusion::physical_plan::metrics::MetricBuilder;
use datafusion::physical_planner::{ExtensionPlanner, PhysicalPlanner};
use datafusion::{
    execution::context::SessionState,
    physical_plan::ExecutionPlan,
    prelude::{cast, DataFrame, SessionContext},
};

use delta_kernel::engine::arrow_conversion::{TryIntoArrow as _, TryIntoKernel as _};
use delta_kernel::schema::{ColumnMetadataKey, StructType};
use filter::try_construct_early_filter;
use futures::future::BoxFuture;
use parquet::file::properties::WriterProperties;
use serde::Serialize;
use tracing::log::*;
use uuid::Uuid;

use self::barrier::{MergeBarrier, MergeBarrierExec};
use super::datafusion_utils::{into_expr, maybe_into_expr, Expression};
use super::{CustomExecuteHandler, Operation};
use crate::delta_datafusion::expr::{fmt_expr_to_sql, parse_predicate_expression};
use crate::delta_datafusion::logical::MetricObserver;
use crate::delta_datafusion::physical::{find_metric_node, get_metric, MetricObserverExec};
use crate::delta_datafusion::planner::DeltaPlanner;
use crate::delta_datafusion::{
    register_store, DataFusionMixins, DeltaColumn, DeltaScan, DeltaScanConfigBuilder,
    DeltaSessionConfig, DeltaTableProvider,
};
use crate::kernel::schema::cast::{merge_arrow_field, merge_arrow_schema};
use crate::kernel::transaction::{CommitBuilder, CommitProperties, PROTOCOL};
use crate::kernel::{Action, Metadata, StructTypeExt};
use crate::logstore::LogStoreRef;
use crate::operations::cdc::*;
use crate::operations::merge::barrier::find_node;
use crate::operations::write::execution::write_execution_plan_v2;
use crate::operations::write::generated_columns::{
    able_to_gc, add_generated_columns, add_missing_generated_columns,
};
use crate::operations::write::WriterStatsConfig;
use crate::protocol::{DeltaOperation, MergePredicate};
use crate::table::state::DeltaTableState;
use crate::{DeltaResult, DeltaTable, DeltaTableError};

mod barrier;
mod filter;

const SOURCE_COLUMN: &str = "__delta_rs_source";
const TARGET_COLUMN: &str = "__delta_rs_target";

const OPERATION_COLUMN: &str = "__delta_rs_operation";
const DELETE_COLUMN: &str = "__delta_rs_delete";
pub(crate) const TARGET_INSERT_COLUMN: &str = "__delta_rs_target_insert";
pub(crate) const TARGET_UPDATE_COLUMN: &str = "__delta_rs_target_update";
pub(crate) const TARGET_DELETE_COLUMN: &str = "__delta_rs_target_delete";
pub(crate) const TARGET_COPY_COLUMN: &str = "__delta_rs_target_copy";

const SOURCE_COUNT_METRIC: &str = "num_source_rows";
const TARGET_COUNT_METRIC: &str = "num_target_rows";
const TARGET_COPY_METRIC: &str = "num_copied_rows";
const TARGET_INSERTED_METRIC: &str = "num_target_inserted_rows";
const TARGET_UPDATED_METRIC: &str = "num_target_updated_rows";
const TARGET_DELETED_METRIC: &str = "num_target_deleted_rows";

const SOURCE_COUNT_ID: &str = "merge_source_count";
const TARGET_COUNT_ID: &str = "merge_target_count";
const OUTPUT_COUNT_ID: &str = "merge_output_count";

/// Merge records into a Delta Table.
pub struct MergeBuilder {
    /// The join predicate
    predicate: Expression,
    /// Operations to perform when a source record and target record match
    match_operations: Vec<MergeOperationConfig>,
    /// Operations to perform on source records when they do not pair with a target record
    not_match_operations: Vec<MergeOperationConfig>,
    /// Operations to perform on target records when they do not pair with a source record
    not_match_source_operations: Vec<MergeOperationConfig>,
    ///Prefix the source columns with a user provided prefix
    source_alias: Option<String>,
    ///Prefix target columns with a user provided prefix
    target_alias: Option<String>,
    /// A snapshot of the table's state. AKA the target table in the operation
    snapshot: DeltaTableState,
    /// The source data
    source: DataFrame,
    /// Whether the source is a streaming source (if true, stats deducing to prune target is disabled)
    streaming: bool,
    /// Enable merge schema evolution
    merge_schema: bool,
    /// Delta object store for handling data files
    log_store: LogStoreRef,
    /// Datafusion session state relevant for executing the input plan
    state: Option<SessionState>,
    /// Properties passed to underlying parquet writer for when files are rewritten
    writer_properties: Option<WriterProperties>,
    /// Additional information to add to the commit
    commit_properties: CommitProperties,
    /// safe_cast determines how data types that do not match the underlying table are handled
    /// By default an error is returned
    safe_cast: bool,
    custom_execute_handler: Option<Arc<dyn CustomExecuteHandler>>,
}

impl super::Operation<()> for MergeBuilder {
    fn log_store(&self) -> &LogStoreRef {
        &self.log_store
    }
    fn get_custom_execute_handler(&self) -> Option<Arc<dyn CustomExecuteHandler>> {
        self.custom_execute_handler.clone()
    }
}

impl MergeBuilder {
    /// Create a new [`MergeBuilder`]
    pub fn new<E: Into<Expression>>(
        log_store: LogStoreRef,
        snapshot: DeltaTableState,
        predicate: E,
        source: DataFrame,
    ) -> Self {
        let predicate = predicate.into();
        Self {
            predicate,
            source,
            snapshot,
            log_store,
            source_alias: None,
            target_alias: None,
            state: None,
            commit_properties: CommitProperties::default(),
            writer_properties: None,
            merge_schema: false,
            match_operations: Vec::new(),
            not_match_operations: Vec::new(),
            not_match_source_operations: Vec::new(),
            safe_cast: false,
            streaming: false,
            custom_execute_handler: None,
        }
    }

    /// Update a target record when it matches with a source record
    ///
    /// The update expressions can specify both source and target columns.
    ///
    /// Multiple match clauses can be specified and their predicates are
    /// evaluated to determine if the corresponding operation are performed.
    /// Only the first clause that results in a satisfy predicate is executed.
    /// The order of match clauses matter.
    ///
    /// #Example
    /// ```rust ignore
    /// let table = open_table("../path/to/table")?;
    /// let (table, metrics) = DeltaOps(table)
    ///     .merge(source, col("target.id").eq(col("source.id")))
    ///     .with_source_alias("source")
    ///     .with_target_alias("target")
    ///     .when_matched_update(|update| {
    ///         update
    ///             .predicate(col("source.value").lt(lit(0)))
    ///             .update("value", lit(0))
    ///             .update("modified", col("source.modified"))
    ///     })?
    ///     .when_matched_update(|update| {
    ///         update
    ///             .update("value", col("source.value") + lit(1))
    ///             .update("modified", col("source.modified"))
    ///     })?
    ///     .await?
    /// ```
    pub fn when_matched_update<F>(mut self, builder: F) -> DeltaResult<MergeBuilder>
    where
        F: FnOnce(UpdateBuilder) -> UpdateBuilder,
    {
        let builder = builder(UpdateBuilder::default());
        let op =
            MergeOperationConfig::new(builder.predicate, builder.updates, OperationType::Update)?;
        self.match_operations.push(op);
        Ok(self)
    }

    /// Delete a target record when it matches with a source record
    ///
    /// Multiple match clauses can be specified and their predicates are
    /// evaluated to determine if the corresponding operation are performed.
    /// Only the first clause that results in a satisfy predicate is executed.
    /// The order of match clauses matter.
    ///
    /// #Example
    /// ```rust ignore
    /// let table = open_table("../path/to/table")?;
    /// let (table, metrics) = DeltaOps(table)
    ///     .merge(source, col("target.id").eq(col("source.id")))
    ///     .with_source_alias("source")
    ///     .with_target_alias("target")
    ///     .when_matched_delete(|delete| {
    ///         delete.predicate(col("source.delete"))
    ///     })?
    ///     .await?
    /// ```
    pub fn when_matched_delete<F>(mut self, builder: F) -> DeltaResult<MergeBuilder>
    where
        F: FnOnce(DeleteBuilder) -> DeleteBuilder,
    {
        let builder = builder(DeleteBuilder::default());
        let op = MergeOperationConfig::new(
            builder.predicate,
            HashMap::default(),
            OperationType::Delete,
        )?;
        self.match_operations.push(op);
        Ok(self)
    }

    /// Insert a source record when it does not match with a target record
    ///
    /// Multiple not match clauses can be specified and their predicates are
    /// evaluated to determine if the corresponding operation are performed.
    /// Only the first clause that results in a satisfy predicate is executed.
    /// The order of not match clauses matter.
    ///
    /// #Example
    /// ```rust ignore
    /// let table = open_table("../path/to/table")?;
    /// let (table, metrics) = DeltaOps(table)
    ///     .merge(source, col("target.id").eq(col("source.id")))
    ///     .with_source_alias("source")
    ///     .with_target_alias("target")
    ///     .when_not_matched_insert(|insert| {
    ///         insert
    ///             .set("id", col("source.id"))
    ///             .set("value", col("source.value"))
    ///             .set("modified", col("source.modified"))
    ///     })?
    ///     .await?
    /// ```
    pub fn when_not_matched_insert<F>(mut self, builder: F) -> DeltaResult<MergeBuilder>
    where
        F: FnOnce(InsertBuilder) -> InsertBuilder,
    {
        let builder = builder(InsertBuilder::default());
        let op = MergeOperationConfig::new(builder.predicate, builder.set, OperationType::Insert)?;
        self.not_match_operations.push(op);
        Ok(self)
    }

    /// Update a target record when it does not match with a
    /// source record
    ///
    /// The update expressions can specify only target columns.
    ///
    /// Multiple source not match clauses can be specified and their predicates
    /// are evaluated to determine if the corresponding operation are performed.
    /// Only the first clause that results in a satisfy predicate is executed.
    /// The order of source not match clauses matter.
    ///
    /// #Example
    /// ```rust ignore
    /// let table = open_table("../path/to/table")?;
    /// let (table, metrics) = DeltaOps(table)
    ///     .merge(source, col("target.id").eq(col("source.id")))
    ///     .with_source_alias("source")
    ///     .with_target_alias("target")
    ///     .when_not_matched_by_source_update(|update| {
    ///         update
    ///             .update("active", lit(false))
    ///             .update("to_dt", lit("2023-07-11"))
    ///     })?
    ///     .await?
    /// ```
    pub fn when_not_matched_by_source_update<F>(mut self, builder: F) -> DeltaResult<MergeBuilder>
    where
        F: FnOnce(UpdateBuilder) -> UpdateBuilder,
    {
        let builder = builder(UpdateBuilder::default());
        let op =
            MergeOperationConfig::new(builder.predicate, builder.updates, OperationType::Update)?;
        self.not_match_source_operations.push(op);
        Ok(self)
    }

    /// Delete a target record when it does not match with a source record
    ///
    /// Multiple source "not match" clauses can be specified and their predicates
    /// are evaluated to determine if the corresponding operations are performed.
    /// Only the first clause that results in a satisfy predicate is executed.
    /// The order of source "not match" clauses matter.
    ///
    /// #Example
    /// ```rust ignore
    /// let table = open_table("../path/to/table")?;
    /// let (table, metrics) = DeltaOps(table)
    ///     .merge(source, col("target.id").eq(col("source.id")))
    ///     .with_source_alias("source")
    ///     .with_target_alias("target")
    ///     .when_not_matched_by_source_delete(|delete| {
    ///         delete
    ///     })?
    ///     .await?
    /// ```
    pub fn when_not_matched_by_source_delete<F>(mut self, builder: F) -> DeltaResult<MergeBuilder>
    where
        F: FnOnce(DeleteBuilder) -> DeleteBuilder,
    {
        let builder = builder(DeleteBuilder::default());
        let op = MergeOperationConfig::new(
            builder.predicate,
            HashMap::default(),
            OperationType::Delete,
        )?;
        self.not_match_source_operations.push(op);
        Ok(self)
    }

    /// Rename columns in the source dataset to have a prefix of `alias`.`original column name`
    pub fn with_source_alias<S: ToString>(mut self, alias: S) -> Self {
        self.source_alias = Some(alias.to_string());
        self
    }

    /// Rename columns in the target dataset to have a prefix of `alias`.`original column name`
    pub fn with_target_alias<S: ToString>(mut self, alias: S) -> Self {
        self.target_alias = Some(alias.to_string());
        self
    }
    /// Add Schema Write Mode
    pub fn with_merge_schema(mut self, merge_schema: bool) -> Self {
        self.merge_schema = merge_schema;
        self
    }

    /// The Datafusion session state to use
    pub fn with_session_state(mut self, state: SessionState) -> Self {
        self.state = Some(state);
        self
    }

    /// Additional metadata to be added to commit info
    pub fn with_commit_properties(mut self, commit_properties: CommitProperties) -> Self {
        self.commit_properties = commit_properties;
        self
    }

    /// Writer properties passed to parquet writer for when fiiles are rewritten
    pub fn with_writer_properties(mut self, writer_properties: WriterProperties) -> Self {
        self.writer_properties = Some(writer_properties);
        self
    }

    /// Specify the cast options to use when casting columns that do not match
    /// the table's schema.  When `cast_options.safe` is set true then any
    /// failures to cast a datatype will use null instead of returning an error
    /// to the user.
    ///
    /// Example (column's type is int):
    /// Input               Output
    /// 123         ->      123
    /// Test123     ->      null
    pub fn with_safe_cast(mut self, safe_cast: bool) -> Self {
        self.safe_cast = safe_cast;
        self
    }

    /// Set streaming mode execution
    pub fn with_streaming(mut self, streaming: bool) -> Self {
        self.streaming = streaming;
        self
    }

    /// Set a custom execute handler, for pre and post execution
    pub fn with_custom_execute_handler(mut self, handler: Arc<dyn CustomExecuteHandler>) -> Self {
        self.custom_execute_handler = Some(handler);
        self
    }
}

#[derive(Default)]
/// Builder for update clauses
pub struct UpdateBuilder {
    /// Only update records that match the predicate
    predicate: Option<Expression>,
    /// How to update columns in the target table
    updates: HashMap<Column, Expression>,
}

impl UpdateBuilder {
    /// Perform the update operation when the predicate is satisfied
    pub fn predicate<E: Into<Expression>>(mut self, predicate: E) -> Self {
        self.predicate = Some(predicate.into());
        self
    }

    /// How a column from the target table should be updated.
    /// In the match case the expression may contain both source and target columns.
    /// In the source not match case the expression may only contain target columns
    pub fn update<C: Into<DeltaColumn>, E: Into<Expression>>(
        mut self,
        column: C,
        expression: E,
    ) -> Self {
        self.updates.insert(column.into().into(), expression.into());
        self
    }
}

/// Builder for insert clauses
#[derive(Default)]
pub struct InsertBuilder {
    /// Only insert records that match the predicate
    predicate: Option<Expression>,
    /// What value each column is inserted with
    set: HashMap<Column, Expression>,
}

impl InsertBuilder {
    /// Perform the insert operation when the predicate is satisfied
    pub fn predicate<E: Into<Expression>>(mut self, predicate: E) -> Self {
        self.predicate = Some(predicate.into());
        self
    }

    /// Which values to insert into the target tables. If a target column is not
    /// specified then null is inserted.
    pub fn set<C: Into<DeltaColumn>, E: Into<Expression>>(
        mut self,
        column: C,
        expression: E,
    ) -> Self {
        self.set.insert(column.into().into(), expression.into());
        self
    }
}

/// Builder for delete clauses
#[derive(Default)]
pub struct DeleteBuilder {
    predicate: Option<Expression>,
}

impl DeleteBuilder {
    /// Delete a record when the predicate is satisfied
    pub fn predicate<E: Into<Expression>>(mut self, predicate: E) -> Self {
        self.predicate = Some(predicate.into());
        self
    }
}

#[derive(Debug, Copy, Clone)]
enum OperationType {
    Update,
    Delete,
    SourceDelete,
    Insert,
    Copy,
}

//Encapsute the User's Merge configuration for later processing
struct MergeOperationConfig {
    /// Which records to update
    predicate: Option<Expression>,
    /// How to update columns in a record that match the predicate
    operations: HashMap<Column, Expression>,
    r#type: OperationType,
}

struct MergeOperation {
    /// Which records to update
    predicate: Option<Expr>,
    /// How to update columns in a record that match the predicate
    operations: HashMap<Column, Expr>,
    r#type: OperationType,
}

impl MergeOperation {
    fn try_from(
        config: MergeOperationConfig,
        schema: &DFSchema,
        state: &SessionState,
        target_alias: &Option<String>,
    ) -> DeltaResult<MergeOperation> {
        let mut ops = HashMap::with_capacity(config.operations.capacity());

        for (column, expression) in config.operations.into_iter() {
            // Normalize the column name to contain the target alias. If a table reference was provided ensure it's the target.
            let column = match target_alias {
                Some(alias) => {
                    let r = TableReference::bare(alias.to_owned());
                    match column {
                        Column {
                            relation: None,
                            name,
                            spans,
                        } => Column {
                            relation: Some(r),
                            name,
                            spans,
                        },
                        Column {
                            relation: Some(TableReference::Bare { table }),
                            name,
                            spans,
                        } => {
                            if table.as_ref() == alias {
                                Column {
                                    relation: Some(r),
                                    name,
                                    spans,
                                }
                            } else {
                                return Err(DeltaTableError::Generic(
                                    format!("Table alias '{table}' in column reference '{table}.{name}' unknown. Hint: You must reference the Delta Table with alias '{alias}'.")
                                ));
                            }
                        }
                        _ => {
                            return Err(DeltaTableError::Generic(
                                "Column must reference column in Delta table".into(),
                            ))
                        }
                    }
                }
                None => column,
            };
            ops.insert(column, into_expr(expression, schema, state)?);
        }

        Ok(MergeOperation {
            predicate: maybe_into_expr(config.predicate, schema, state)?,
            operations: ops,
            r#type: config.r#type,
        })
    }
}

impl MergeOperationConfig {
    pub fn new(
        predicate: Option<Expression>,
        operations: HashMap<Column, Expression>,
        r#type: OperationType,
    ) -> DeltaResult<Self> {
        Ok(MergeOperationConfig {
            predicate,
            operations,
            r#type,
        })
    }
}

#[derive(Default, Serialize, Debug)]
/// Metrics for the Merge Operation
pub struct MergeMetrics {
    /// Number of rows in the source data
    pub num_source_rows: usize,
    /// Number of rows inserted into the target table
    pub num_target_rows_inserted: usize,
    /// Number of rows updated in the target table
    pub num_target_rows_updated: usize,
    /// Number of rows deleted in the target table
    pub num_target_rows_deleted: usize,
    /// Number of target rows copied
    pub num_target_rows_copied: usize,
    /// Total number of rows written out
    pub num_output_rows: usize,
    /// Amount of files considered during table scan
    pub num_target_files_scanned: usize,
    /// Amount of files not considered (pruned) during table scan
    pub num_target_files_skipped_during_scan: usize,
    /// Number of files added to the sink(target)
    pub num_target_files_added: usize,
    /// Number of files removed from the sink(target)
    pub num_target_files_removed: usize,
    /// Time taken to execute the entire operation
    pub execution_time_ms: u64,
    /// Time taken to scan the files for matches
    pub scan_time_ms: u64,
    /// Time taken to rewrite the matched files
    pub rewrite_time_ms: u64,
}
#[derive(Clone, Debug)]
struct MergeMetricExtensionPlanner {}

#[async_trait]
impl ExtensionPlanner for MergeMetricExtensionPlanner {
    async fn plan_extension(
        &self,
        planner: &dyn PhysicalPlanner,
        node: &dyn UserDefinedLogicalNode,
        _logical_inputs: &[&LogicalPlan],
        physical_inputs: &[Arc<dyn ExecutionPlan>],
        session_state: &SessionState,
    ) -> DataFusionResult<Option<Arc<dyn ExecutionPlan>>> {
        if let Some(metric_observer) = node.as_any().downcast_ref::<MetricObserver>() {
            if metric_observer.id.eq(SOURCE_COUNT_ID) {
                return Ok(Some(MetricObserverExec::try_new(
                    SOURCE_COUNT_ID.into(),
                    physical_inputs,
                    |batch, metrics| {
                        MetricBuilder::new(metrics)
                            .global_counter(SOURCE_COUNT_METRIC)
                            .add(batch.num_rows());
                    },
                )?));
            }

            if metric_observer.id.eq(TARGET_COUNT_ID) {
                return Ok(Some(MetricObserverExec::try_new(
                    TARGET_COUNT_ID.into(),
                    physical_inputs,
                    |batch, metrics| {
                        MetricBuilder::new(metrics)
                            .global_counter(TARGET_COUNT_METRIC)
                            .add(batch.num_rows());
                    },
                )?));
            }

            if metric_observer.id.eq(OUTPUT_COUNT_ID) {
                return Ok(Some(MetricObserverExec::try_new(
                    OUTPUT_COUNT_ID.into(),
                    physical_inputs,
                    |batch, metrics| {
                        MetricBuilder::new(metrics)
                            .global_counter(TARGET_INSERTED_METRIC)
                            .add(
                                batch
                                    .column_by_name(TARGET_INSERT_COLUMN)
                                    .unwrap()
                                    .null_count(),
                            );
                        MetricBuilder::new(metrics)
                            .global_counter(TARGET_UPDATED_METRIC)
                            .add(
                                batch
                                    .column_by_name(TARGET_UPDATE_COLUMN)
                                    .unwrap()
                                    .null_count(),
                            );
                        MetricBuilder::new(metrics)
                            .global_counter(TARGET_DELETED_METRIC)
                            .add(
                                batch
                                    .column_by_name(TARGET_DELETE_COLUMN)
                                    .unwrap()
                                    .null_count(),
                            );
                        MetricBuilder::new(metrics)
                            .global_counter(TARGET_COPY_METRIC)
                            .add(
                                batch
                                    .column_by_name(TARGET_COPY_COLUMN)
                                    .unwrap()
                                    .null_count(),
                            );
                    },
                )?));
            }
        }

        if let Some(barrier) = node.as_any().downcast_ref::<MergeBarrier>() {
            let schema = barrier.input.schema();
            return Ok(Some(Arc::new(MergeBarrierExec::new(
                physical_inputs.first().unwrap().clone(),
                barrier.file_column.clone(),
                planner.create_physical_expr(&barrier.expr, schema, session_state)?,
            ))));
        }

        Ok(None)
    }
}

#[allow(clippy::too_many_arguments)]
async fn execute(
    predicate: Expression,
    mut source: DataFrame,
    log_store: LogStoreRef,
    snapshot: DeltaTableState,
    state: SessionState,
    writer_properties: Option<WriterProperties>,
    mut commit_properties: CommitProperties,
    _safe_cast: bool,
    streaming: bool,
    source_alias: Option<String>,
    target_alias: Option<String>,
    merge_schema: bool,
    match_operations: Vec<MergeOperationConfig>,
    not_match_target_operations: Vec<MergeOperationConfig>,
    not_match_source_operations: Vec<MergeOperationConfig>,
    operation_id: Uuid,
    handle: Option<&Arc<dyn CustomExecuteHandler>>,
) -> DeltaResult<(DeltaTableState, MergeMetrics)> {
    let mut metrics = MergeMetrics::default();
    let exec_start = Instant::now();
    // Determining whether we should write change data once so that computation of change data can
    // be disabled in the common case(s)
    let should_cdc = should_write_cdc(&snapshot)?;
    // Change data may be collected and then written out at the completion of the merge

    if should_cdc {
        debug!("Executing a merge and I should write CDC!");
    }

    let current_metadata = snapshot.metadata();
    let merge_planner = DeltaPlanner::<MergeMetricExtensionPlanner> {
        extension_planner: MergeMetricExtensionPlanner {},
    };

    let state = SessionStateBuilder::new_from_existing(state)
        .with_query_planner(Arc::new(merge_planner))
        .build();

    // TODO: Given the join predicate, remove any expression that involve the
    // source table and keep expressions that only involve the target table.
    // This would allow us to perform statistics/partition pruning E.G
    // Expression source.id = id and to_dt = '9999-12-31' -Becomes-> to_dt =
    // '9999-12-31'
    //
    // If the user specified any not_source_match operations then those
    // predicates also need to be considered when pruning

    let source_name = match &source_alias {
        Some(alias) => TableReference::bare(alias.to_string()),
        None => TableReference::bare(UNNAMED_TABLE),
    };

    let target_name = match &target_alias {
        Some(alias) => TableReference::bare(alias.to_string()),
        None => TableReference::bare(UNNAMED_TABLE),
    };

    let mut generated_col_exp = None;
    let mut missing_generated_col = None;

    if able_to_gc(&snapshot)? {
        let generated_col_expressions = snapshot.schema().get_generated_columns()?;
        let (source_with_gc, missing_generated_columns) =
            add_missing_generated_columns(source, &generated_col_expressions)?;

        source = source_with_gc;
        generated_col_exp = Some(generated_col_expressions);
        missing_generated_col = Some(missing_generated_columns);
    }
    // This is only done to provide the source columns with a correct table reference. Just renaming the columns does not work
    let source = LogicalPlanBuilder::scan(
        source_name.clone(),
        provider_as_source(source.into_view()),
        None,
    )?
    .build()?;

    let source = LogicalPlan::Extension(Extension {
        node: Arc::new(MetricObserver {
            id: SOURCE_COUNT_ID.into(),
            input: source,
            enable_pushdown: false,
        }),
    });

    let scan_config = DeltaScanConfigBuilder::default()
        .with_file_column(true)
        .with_parquet_pushdown(false)
        .with_schema(snapshot.input_schema()?)
        .build(&snapshot)?;

    let target_provider = Arc::new(DeltaTableProvider::try_new(
        snapshot.clone(),
        log_store.clone(),
        scan_config.clone(),
    )?);

    let target_provider = provider_as_source(target_provider);
    let target =
        LogicalPlanBuilder::scan(target_name.clone(), target_provider.clone(), None)?.build()?;

    let source_schema = source.schema();
    let target_schema = target.schema();

    let join_schema_df = build_join_schema(source_schema, target_schema, &JoinType::Full)?;

    let predicate = match predicate {
        Expression::DataFusion(expr) => expr,
        Expression::String(s) => parse_predicate_expression(&join_schema_df, s, &state)?,
    };

    // Attempt to construct an early filter that we can apply to the Add action list and the delta scan.
    // In the case where there are partition columns in the join predicate, we can scan the source table
    // to get the distinct list of partitions affected and constrain the search to those.

    let target_subset_filter: Option<Expr> = if !not_match_source_operations.is_empty() {
        // It's only worth trying to create an early filter where there are no `when_not_matched_source` operators, since
        // that implies a full scan
        None
    } else {
        try_construct_early_filter(
            predicate.clone(),
            &snapshot,
            &state,
            &source,
            &source_name,
            &target_name,
            streaming,
        )
        .await?
    }
    .map(|e| {
        // simplify the expression so we have
        let props = ExecutionProps::new();
        let simplify_context = SimplifyContext::new(&props).with_schema(target.schema().clone());
        let simplifier = ExprSimplifier::new(simplify_context).with_max_cycles(10);
        simplifier.simplify(e).unwrap()
    });

    // Predicate will be used for conflict detection
    let commit_predicate = match target_subset_filter.clone() {
        None => None, // No predicate means it's a full table merge
        Some(some_filter) => {
            let predict_expr = match &target_alias {
                None => some_filter,
                Some(alias) => remove_table_alias(some_filter, alias),
            };
            Some(fmt_expr_to_sql(&predict_expr)?)
        }
    };

    debug!("Using target subset filter: {commit_predicate:?}");

    let file_column = Arc::new(scan_config.file_column_name.clone().unwrap());
    // Need to manually push this filter into the scan... We want to PRUNE files not FILTER RECORDS
    let target = match target_subset_filter {
        Some(filter) => {
            let filter = match &target_alias {
                Some(alias) => remove_table_alias(filter, alias),
                None => filter,
            };
            LogicalPlanBuilder::scan_with_filters(
                target_name.clone(),
                target_provider,
                None,
                vec![filter],
            )?
            .build()?
        }
        None => LogicalPlanBuilder::scan(target_name.clone(), target_provider, None)?.build()?,
    };

    let source = DataFrame::new(state.clone(), source.clone());
    let source = source.with_column(SOURCE_COLUMN, lit(true))?;

    // Not match operations imply a full scan of the target table is required
    let enable_pushdown =
        not_match_source_operations.is_empty() && not_match_target_operations.is_empty();
    let target = LogicalPlan::Extension(Extension {
        node: Arc::new(MetricObserver {
            id: TARGET_COUNT_ID.into(),
            input: target,
            enable_pushdown,
        }),
    });
    let target = DataFrame::new(state.clone(), target);
    let target = target.with_column(TARGET_COLUMN, lit(true))?;

    let join = source.join(target, JoinType::Full, &[], &[], Some(predicate.clone()))?;
    let join_schema_df = join.schema().to_owned();

    let match_operations: Vec<MergeOperation> = match_operations
        .into_iter()
        .map(|op| MergeOperation::try_from(op, &join_schema_df, &state, &target_alias))
        .collect::<Result<Vec<MergeOperation>, DeltaTableError>>()?;

    let not_match_target_operations: Vec<MergeOperation> = not_match_target_operations
        .into_iter()
        .map(|op| MergeOperation::try_from(op, &join_schema_df, &state, &target_alias))
        .collect::<Result<Vec<MergeOperation>, DeltaTableError>>()?;

    let not_match_source_operations: Vec<MergeOperation> = not_match_source_operations
        .into_iter()
        .map(|op| MergeOperation::try_from(op, &join_schema_df, &state, &target_alias))
        .collect::<Result<Vec<MergeOperation>, DeltaTableError>>()?;

    // merge_arrow_schema is used to tell whether the two schema can be merge but we use the operation statement to pick new columns
    // this avoid the side effect of adding unnecessary columns (eg. target.id = source.ID) "ID" will not be added since "id" exist in target and end user intended it to be "id"
    let mut new_schema = None;
    let mut schema_action = None;
    if merge_schema {
        let merge_schema = merge_arrow_schema(
            snapshot.input_schema()?,
            source_schema.inner().clone(),
            false,
        )?;

        let mut schema_builder = SchemaBuilder::from(merge_schema.deref());

        modify_schema(
            &mut schema_builder,
            target_schema,
            source_schema,
            &match_operations,
        )?;

        modify_schema(
            &mut schema_builder,
            target_schema,
            source_schema,
            &not_match_source_operations,
        )?;

        modify_schema(
            &mut schema_builder,
            target_schema,
            source_schema,
            &not_match_target_operations,
        )?;
        let schema = Arc::new(schema_builder.finish());
        new_schema = Some(schema.clone());
        let schema_struct: StructType = schema.try_into_kernel()?;
        if &schema_struct != snapshot.schema() {
            let action = Action::Metadata(Metadata::try_new(
                schema_struct,
                current_metadata.partition_columns.clone(),
                snapshot.metadata().configuration.clone(),
            )?);
            schema_action = Some(action);
        }
    }

    let matched = col(SOURCE_COLUMN)
        .is_true()
        .and(col(TARGET_COLUMN).is_true());
    let not_matched_target = col(SOURCE_COLUMN)
        .is_true()
        .and(col(TARGET_COLUMN).is_null());
    let not_matched_source = col(SOURCE_COLUMN)
        .is_null()
        .and(col(TARGET_COLUMN))
        .is_true();

    // Plus 3 for the default operations for each match category
    let operations_size = match_operations.len()
        + not_match_source_operations.len()
        + not_match_target_operations.len()
        + 3;

    let mut when_expr = Vec::with_capacity(operations_size);
    let mut then_expr = Vec::with_capacity(operations_size);
    let mut ops = Vec::with_capacity(operations_size);

    fn update_case(
        operations: Vec<MergeOperation>,
        ops: &mut Vec<(HashMap<Column, Expr>, OperationType)>,
        when_expr: &mut Vec<Expr>,
        then_expr: &mut Vec<Expr>,
        base_expr: &Expr,
    ) -> DeltaResult<Vec<MergePredicate>> {
        let mut predicates = Vec::with_capacity(operations.len());

        for op in operations {
            let predicate = match &op.predicate {
                Some(predicate) => base_expr.clone().and(predicate.to_owned()),
                None => base_expr.clone(),
            };

            when_expr.push(predicate);
            then_expr.push(lit(ops.len() as i32));

            ops.push((op.operations, op.r#type));

            let action_type = match op.r#type {
                OperationType::Update => "update",
                OperationType::Delete => "delete",
                OperationType::Insert => "insert",
                OperationType::SourceDelete => {
                    return Err(DeltaTableError::Generic("Invalid action type".to_string()))
                }
                OperationType::Copy => {
                    return Err(DeltaTableError::Generic("Invalid action type".to_string()))
                }
            };

            let action_type = action_type.to_string();
            let predicate = op
                .predicate
                .map(|expr| fmt_expr_to_sql(&expr))
                .transpose()?;

            predicates.push(MergePredicate {
                action_type,
                predicate,
            });
        }
        Ok(predicates)
    }

    let match_operations = update_case(
        match_operations,
        &mut ops,
        &mut when_expr,
        &mut then_expr,
        &matched,
    )?;

    let not_match_target_operations = update_case(
        not_match_target_operations,
        &mut ops,
        &mut when_expr,
        &mut then_expr,
        &not_matched_target,
    )?;

    let not_match_source_operations = update_case(
        not_match_source_operations,
        &mut ops,
        &mut when_expr,
        &mut then_expr,
        &not_matched_source,
    )?;

    when_expr.push(matched);
    then_expr.push(lit(ops.len() as i32));
    ops.push((HashMap::new(), OperationType::Copy));

    when_expr.push(not_matched_target);
    then_expr.push(lit(ops.len() as i32));
    ops.push((HashMap::new(), OperationType::SourceDelete));

    when_expr.push(not_matched_source);
    then_expr.push(lit(ops.len() as i32));
    ops.push((HashMap::new(), OperationType::Copy));

    let case = CaseBuilder::new(None, when_expr, then_expr, None).end()?;

    let projection = join.with_column(OPERATION_COLUMN, case)?;

    let mut new_columns = vec![];

    let mut write_projection = Vec::new();
    let mut write_projection_with_cdf = Vec::new();

    let schema = if let Some(schema) = new_schema {
        &schema.try_into_kernel()?
    } else {
        snapshot.schema()
    };

    for delta_field in schema.fields() {
        let mut when_expr = Vec::with_capacity(operations_size);
        let mut then_expr = Vec::with_capacity(operations_size);

        let qualifier = match &target_alias {
            Some(alias) => Some(TableReference::Bare {
                table: alias.to_owned().into(),
            }),
            None => TableReference::none(),
        };
        let mut null_target_column = None;

        let source_qualifier = match &source_alias {
            Some(alias) => Some(TableReference::Bare {
                table: alias.to_owned().into(),
            }),
            None => TableReference::none(),
        };
        let name = delta_field.name();
        let mut cast_type: DataType = delta_field.data_type().try_into_arrow()?;

        // Receive the correct column reference given that some columns are only in source table
        let column = if let Some(field) = snapshot.schema().field(name) {
            if field == delta_field {
                Column::new(qualifier.clone(), name)
            } else {
                // when there is a change in the field such as an added column in a nested data types casts will break with the new field data type
                let col_ref = Column::new(source_qualifier.clone(), name);
                cast_type = source_schema.data_type(&col_ref)?.to_owned();
                col_ref
            }
        } else {
            null_target_column = Some(cast(
                lit(ScalarValue::Null).alias(name),
                delta_field.data_type().try_into_arrow()?,
            ));
            Column::new(source_qualifier.clone(), name)
        };

        for (idx, (operations, _)) in ops.iter().enumerate() {
            let op = operations
                .get(&column)
                .map(|expr| expr.to_owned())
                .unwrap_or_else(|| col(column.clone()));

            when_expr.push(lit(idx as i32));
            then_expr.push(op);
        }

        let case = CaseBuilder::new(
            Some(Box::new(col(OPERATION_COLUMN))),
            when_expr,
            then_expr,
            None,
        )
        .end()?;

        let name = "__delta_rs_c_".to_owned() + delta_field.name();

        write_projection.push(cast(
            Expr::Column(Column::from_name(name.clone())).alias(delta_field.name()),
            cast_type.clone(),
        ));

        write_projection_with_cdf.push(
            when(
                col(CDC_COLUMN_NAME).not_eq(lit("update_preimage")),
                cast(
                    Expr::Column(Column::from_name(name.clone())),
                    cast_type.clone(),
                ),
            )
            .otherwise(null_target_column.unwrap_or(cast(
                Expr::Column(Column::new(qualifier, delta_field.name())),
                cast_type,
            )))? // We take the column from target table but in case of schema evolution we assign the column as null
            .alias(delta_field.name()),
        );
        new_columns.push((name, case));
    }

    write_projection_with_cdf.push(col("_change_type"));

    let mut insert_when = Vec::with_capacity(ops.len());
    let mut insert_then = Vec::with_capacity(ops.len());

    let mut update_when = Vec::with_capacity(ops.len());
    let mut update_then = Vec::with_capacity(ops.len());

    let mut target_delete_when = Vec::with_capacity(ops.len());
    let mut target_delete_then = Vec::with_capacity(ops.len());

    let mut delete_when = Vec::with_capacity(ops.len());
    let mut delete_then = Vec::with_capacity(ops.len());

    let mut copy_when = Vec::with_capacity(ops.len());
    let mut copy_then = Vec::with_capacity(ops.len());

    for (idx, (_operations, r#type)) in ops.iter().enumerate() {
        let op = idx as i32;

        // Used to indicate the record should be dropped prior to write
        delete_when.push(lit(op));
        delete_then.push(lit(matches!(
            r#type,
            OperationType::Delete | OperationType::SourceDelete
        )));

        // Use the null count on these arrays to determine how many records satisfy the predicate
        insert_when.push(lit(op));
        insert_then.push(
            when(
                lit(matches!(r#type, OperationType::Insert)),
                lit(ScalarValue::Boolean(None)),
            )
            .otherwise(lit(false))?,
        );

        update_when.push(lit(op));
        update_then.push(
            when(
                lit(matches!(r#type, OperationType::Update)),
                lit(ScalarValue::Boolean(None)),
            )
            .otherwise(lit(false))?,
        );

        target_delete_when.push(lit(op));
        target_delete_then.push(
            when(
                lit(matches!(r#type, OperationType::Delete)),
                lit(ScalarValue::Boolean(None)),
            )
            .otherwise(lit(false))?,
        );

        copy_when.push(lit(op));
        copy_then.push(
            when(
                lit(matches!(r#type, OperationType::Copy)),
                lit(ScalarValue::Boolean(None)),
            )
            .otherwise(lit(false))?,
        );
    }

    fn build_case(when: Vec<Expr>, then: Vec<Expr>) -> DataFusionResult<Expr> {
        CaseBuilder::new(
            Some(Box::new(col(OPERATION_COLUMN))),
            when,
            then,
            Some(Box::new(lit(false))),
        )
        .end()
    }

    new_columns.push((
        DELETE_COLUMN.to_owned(),
        build_case(delete_when, delete_then)?,
    ));
    new_columns.push((
        TARGET_INSERT_COLUMN.to_owned(),
        build_case(insert_when, insert_then)?,
    ));
    new_columns.push((
        TARGET_UPDATE_COLUMN.to_owned(),
        build_case(update_when, update_then)?,
    ));
    new_columns.push((
        TARGET_DELETE_COLUMN.to_owned(),
        build_case(target_delete_when, target_delete_then)?,
    ));
    new_columns.push((
        TARGET_COPY_COLUMN.to_owned(),
        build_case(copy_when, copy_then)?,
    ));

    let new_columns = {
        let plan = projection.into_unoptimized_plan();
        let mut fields: Vec<Expr> = plan
            .schema()
            .columns()
            .iter()
            .map(|f| col(f.clone()))
            .collect();

        fields.extend(new_columns.into_iter().map(|(name, ex)| ex.alias(name)));

        LogicalPlanBuilder::from(plan).project(fields)?.build()?
    };

    let distribute_expr = col(file_column.as_str());

    let merge_barrier = LogicalPlan::Extension(Extension {
        node: Arc::new(MergeBarrier {
            input: new_columns.clone(),
            expr: distribute_expr,
            file_column,
        }),
    });

    // We should observe the metrics before we union the merge plan with the cdf_merge plan
    // so that we get the metrics only for the merge plan.
    let operation_count = LogicalPlan::Extension(Extension {
        node: Arc::new(MetricObserver {
            id: OUTPUT_COUNT_ID.into(),
            input: merge_barrier,
            enable_pushdown: false,
        }),
    });

    let operation_count = DataFrame::new(state.clone(), operation_count);

    let mut projected = if should_cdc {
        operation_count
            .clone()
            .with_column(
                CDC_COLUMN_NAME,
                when(col(TARGET_DELETE_COLUMN).is_null(), lit("delete")) // nulls are equal to True
                    .when(col(DELETE_COLUMN).is_null(), lit("source_delete"))
                    .when(col(TARGET_COPY_COLUMN).is_null(), lit("copy"))
                    .when(col(TARGET_INSERT_COLUMN).is_null(), lit("insert"))
                    .when(col(TARGET_UPDATE_COLUMN).is_null(), lit("update"))
                    .end()?,
            )?
            .drop_columns(&["__delta_rs_path"])? // WEIRD bug caused by interaction with unnest_columns, has to be dropped otherwise throws schema error
            .with_column(
                "__delta_rs_update_expanded",
                when(
                    col(CDC_COLUMN_NAME).eq(lit("update")),
                    lit(ScalarValue::List(ScalarValue::new_list(
                        &[
                            ScalarValue::Utf8(Some("update_preimage".into())),
                            ScalarValue::Utf8(Some("update_postimage".into())),
                        ],
                        &DataType::List(Field::new("element", DataType::Utf8, false).into()),
                        true,
                    ))),
                )
                .end()?,
            )?
            .unnest_columns(&["__delta_rs_update_expanded"])?
            .with_column(
                CDC_COLUMN_NAME,
                when(
                    col(CDC_COLUMN_NAME).eq(lit("update")),
                    col("__delta_rs_update_expanded"),
                )
                .otherwise(col(CDC_COLUMN_NAME))?,
            )?
            .drop_columns(&["__delta_rs_update_expanded"])?
            .select(write_projection_with_cdf)?
    } else {
        operation_count
            .filter(col(DELETE_COLUMN).is_false())?
            .select(write_projection)?
    };

    if let Some(generated_col_expressions) = generated_col_exp {
        if let Some(missing_generated_columns) = missing_generated_col {
            projected = add_generated_columns(
                projected,
                &generated_col_expressions,
                &missing_generated_columns,
                &state,
            )?;
        }
    }

    let merge_final = &projected.into_unoptimized_plan();
    let write = state.create_physical_plan(merge_final).await?;

    let err = || DeltaTableError::Generic("Unable to locate expected metric node".into());
    let source_count = find_metric_node(SOURCE_COUNT_ID, &write).ok_or_else(err)?;
    let op_count = find_metric_node(OUTPUT_COUNT_ID, &write).ok_or_else(err)?;
    let barrier = find_node::<MergeBarrierExec>(&write).ok_or_else(err)?;
    let scan_count = find_node::<DeltaScan>(&write).ok_or_else(err)?;

    let table_partition_cols = current_metadata.partition_columns.clone();

    let writer_stats_config = WriterStatsConfig::new(
        snapshot.table_config().num_indexed_cols(),
        snapshot
            .table_config()
            .stats_columns()
            .map(|v| v.iter().map(|v| v.to_string()).collect::<Vec<String>>()),
    );

    let rewrite_start = Instant::now();
    let mut actions: Vec<Action> = write_execution_plan_v2(
        Some(&snapshot),
        state.clone(),
        write,
        table_partition_cols.clone(),
        log_store.object_store(Some(operation_id)),
        Some(snapshot.table_config().target_file_size() as usize),
        None,
        writer_properties.clone(),
        writer_stats_config.clone(),
        None,
        should_cdc, // if true, write execution plan splits batches in [normal, cdc] data before writing
    )
    .await?;
    if let Some(schema_metadata) = schema_action {
        actions.push(schema_metadata);
    }

    metrics.rewrite_time_ms = Instant::now().duration_since(rewrite_start).as_millis() as u64;
    metrics.num_target_files_added = actions.len();

    let survivors = barrier
        .as_any()
        .downcast_ref::<MergeBarrierExec>()
        .unwrap()
        .survivors();

    {
        let lock = survivors.lock().unwrap();
        for action in snapshot.log_data() {
            if lock.contains(action.path().as_ref()) {
                metrics.num_target_files_removed += 1;
                actions.push(action.remove_action(true).into());
            }
        }
    }

    let source_count_metrics = source_count.metrics().unwrap();
    let target_count_metrics = op_count.metrics().unwrap();
    let scan_count_metrics = scan_count.metrics().unwrap();

    metrics.num_source_rows = get_metric(&source_count_metrics, SOURCE_COUNT_METRIC);
    metrics.num_target_rows_inserted = get_metric(&target_count_metrics, TARGET_INSERTED_METRIC);
    metrics.num_target_rows_updated = get_metric(&target_count_metrics, TARGET_UPDATED_METRIC);
    metrics.num_target_rows_deleted = get_metric(&target_count_metrics, TARGET_DELETED_METRIC);
    metrics.num_target_rows_copied = get_metric(&target_count_metrics, TARGET_COPY_METRIC);
    metrics.num_output_rows = metrics.num_target_rows_inserted
        + metrics.num_target_rows_updated
        + metrics.num_target_rows_copied;
    metrics.num_target_files_scanned = get_metric(&scan_count_metrics, "files_scanned");
    metrics.num_target_files_skipped_during_scan = get_metric(&scan_count_metrics, "files_pruned");
    metrics.execution_time_ms = Instant::now().duration_since(exec_start).as_millis() as u64;

    let app_metadata = &mut commit_properties.app_metadata;
    app_metadata.insert("readVersion".to_owned(), snapshot.version().into());
    if let Ok(map) = serde_json::to_value(&metrics) {
        app_metadata.insert("operationMetrics".to_owned(), map);
    }

    // Do not make a commit when there are zero updates to the state
    let operation = DeltaOperation::Merge {
        predicate: commit_predicate,
        merge_predicate: Some(fmt_expr_to_sql(&predicate)?),
        matched_predicates: match_operations,
        not_matched_predicates: not_match_target_operations,
        not_matched_by_source_predicates: not_match_source_operations,
    };

    if actions.is_empty() {
        return Ok((snapshot, metrics));
    }

    let commit = CommitBuilder::from(commit_properties)
        .with_actions(actions)
        .with_operation_id(operation_id)
        .with_post_commit_hook_handler(handle.cloned())
        .build(Some(&snapshot), log_store.clone(), operation)
        .await?;
    Ok((commit.snapshot(), metrics))
}

fn modify_schema(
    ending_schema: &mut SchemaBuilder,
    target_schema: &DFSchema,
    source_schema: &DFSchema,
    operations: &[MergeOperation],
) -> DeltaResult<()> {
    for columns in operations
        .iter()
        .filter(|ops| matches!(ops.r#type, OperationType::Update | OperationType::Insert))
        .flat_map(|ops| ops.operations.keys())
    {
        // This assume that all the columns in the MERGE operations of UPDATE and INSERT at least exists in the source table
        let source_field = source_schema.field_with_unqualified_name(columns.name())?;
        if source_field
            .metadata()
            .contains_key(ColumnMetadataKey::GenerationExpression.as_ref())
        {
            let error = arrow::error::ArrowError::SchemaError("Schema evolved fields cannot have generated expressions. Recreate the table to achieve this.".to_string());
            return Err(DeltaTableError::Arrow { source: error });
        }

        if let Ok(target_field) = target_schema.field_from_column(columns) {
            // for nested data types we need to first merge then see if there a change then replace the pre-existing field
            let new_field = merge_arrow_field(target_field, source_field, true)?;
            if &new_field == target_field {
                continue;
            }
            ending_schema.try_merge(&Arc::new(new_field))?;
        } else {
            ending_schema.push(source_field.to_owned().with_nullable(true));
        }
    }
    Ok(())
}

fn remove_table_alias(expr: Expr, table_alias: &str) -> Expr {
    expr.transform(&|expr| match expr {
        Expr::Column(c) => match c.relation {
            Some(rel) if rel.table() == table_alias => Ok(Transformed::yes(Expr::Column(
                Column::new_unqualified(c.name),
            ))),
            _ => Ok(Transformed::no(Expr::Column(Column::new(
                c.relation, c.name,
            )))),
        },
        _ => Ok(Transformed::no(expr)),
    })
    .unwrap()
    .data
}

impl std::future::IntoFuture for MergeBuilder {
    type Output = DeltaResult<(DeltaTable, MergeMetrics)>;
    type IntoFuture = BoxFuture<'static, Self::Output>;

    fn into_future(self) -> Self::IntoFuture {
        let this = self;

        Box::pin(async move {
            PROTOCOL.can_write_to(&this.snapshot.snapshot)?;

            if !this.snapshot.load_config().require_files {
                return Err(DeltaTableError::NotInitializedWithFiles("MERGE".into()));
            }

            let operation_id = this.get_operation_id();
            this.pre_execute(operation_id).await?;

            let state = this.state.unwrap_or_else(|| {
                let config: SessionConfig = DeltaSessionConfig::default().into();
                let session = SessionContext::new_with_config(config);

                // If a user provides their own their DF state then they must register the store themselves
                register_store(this.log_store.clone(), session.runtime_env());

                session.state()
            });

            let (snapshot, metrics) = execute(
                this.predicate,
                this.source,
                this.log_store.clone(),
                this.snapshot,
                state,
                this.writer_properties,
                this.commit_properties,
                this.safe_cast,
                this.streaming,
                this.source_alias,
                this.target_alias,
                this.merge_schema,
                this.match_operations,
                this.not_match_operations,
                this.not_match_source_operations,
                operation_id,
                this.custom_execute_handler.as_ref(),
            )
            .await?;

            if let Some(handler) = this.custom_execute_handler {
                handler.post_execute(&this.log_store, operation_id).await?;
            }

            Ok((
                DeltaTable::new_with_state(this.log_store, snapshot),
                metrics,
            ))
        })
    }
}

#[cfg(test)]
mod tests {
    use crate::kernel::Action;
    use crate::kernel::DataType;
    use crate::kernel::PrimitiveType;
    use crate::kernel::StructField;
    use crate::operations::load_cdf::collect_batches;
    use crate::operations::merge::filter::generalize_filter;
    use crate::operations::DeltaOps;
    use crate::protocol::*;
    use crate::writer::test_utils::datafusion::get_data;
    use crate::writer::test_utils::get_arrow_schema;
    use crate::writer::test_utils::get_delta_schema;
    use crate::writer::test_utils::setup_table_with_configuration;
    use crate::DeltaTable;
    use crate::TableProperty;
    use arrow::datatypes::Schema as ArrowSchema;
    use arrow::record_batch::RecordBatch;
    use arrow_schema::DataType as ArrowDataType;
    use arrow_schema::Field;
    use datafusion::assert_batches_sorted_eq;
    use datafusion::common::Column;
    use datafusion::common::TableReference;
    use datafusion::logical_expr::col;
    use datafusion::logical_expr::expr::Placeholder;
    use datafusion::logical_expr::lit;
    use datafusion::logical_expr::Expr;
    use datafusion::prelude::*;
    use delta_kernel::engine::arrow_conversion::TryIntoKernel;
    use delta_kernel::schema::StructType;
    use itertools::Itertools;
    use regex::Regex;
    use serde_json::json;
    use std::ops::Neg;
    use std::sync::Arc;

    use super::MergeMetrics;

    pub(crate) async fn setup_table(partitions: Option<Vec<&str>>) -> DeltaTable {
        let table_schema = get_delta_schema();

        let table = DeltaOps::new_in_memory()
            .create()
            .with_columns(table_schema.fields().cloned())
            .with_partition_columns(partitions.unwrap_or_default())
            .await
            .unwrap();
        assert_eq!(table.version(), Some(0));
        table
    }

    // TODO(ion): property keys are not passed through or translated as table features.. fix this as well
    #[tokio::test]
    async fn test_merge_when_delta_table_is_append_only() {
        let schema = get_arrow_schema(&None);
        let table = setup_table_with_configuration(TableProperty::AppendOnly, Some("true")).await;
        // append some data
        let table = write_data(table, &schema).await;
        // merge
        let _err = DeltaOps(table)
            .merge(merge_source(schema), col("target.id").eq(col("source.id")))
            .with_source_alias("source")
            .with_target_alias("target")
            .when_not_matched_by_source_delete(|delete| delete)
            .unwrap()
            .await
            .expect_err("Remove action is included when Delta table is append-only. Should error");
    }

    async fn write_data(table: DeltaTable, schema: &Arc<ArrowSchema>) -> DeltaTable {
        let batch = RecordBatch::try_new(
            Arc::clone(schema),
            vec![
                Arc::new(arrow::array::StringArray::from(vec!["A", "B", "C", "D"])),
                Arc::new(arrow::array::Int32Array::from(vec![1, 10, 10, 100])),
                Arc::new(arrow::array::StringArray::from(vec![
                    "2021-02-01",
                    "2021-02-01",
                    "2021-02-02",
                    "2021-02-02",
                ])),
            ],
        )
        .unwrap();
        // write some data
        DeltaOps(table)
            .write(vec![batch.clone()])
            .with_save_mode(SaveMode::Append)
            .await
            .unwrap()
    }

    async fn write_data_struct(table: DeltaTable, schema: &Arc<ArrowSchema>) -> DeltaTable {
        let count_array = arrow::array::Int64Array::from(vec![Some(1), Some(2), Some(3), Some(4)]);
        let nested_schema = Arc::new(ArrowSchema::new(vec![Field::new(
            "count",
            ArrowDataType::Int64,
            true,
        )]));
        let batch = RecordBatch::try_new(
            Arc::clone(schema),
            vec![
                Arc::new(arrow::array::StringArray::from(vec!["A", "B", "C", "D"])),
                Arc::new(arrow::array::Int32Array::from(vec![1, 10, 10, 100])),
                Arc::new(arrow::array::StringArray::from(vec![
                    "2021-02-01",
                    "2021-02-01",
                    "2021-02-02",
                    "2021-02-02",
                ])),
                Arc::new(arrow::array::StructArray::from(
                    RecordBatch::try_new(nested_schema, vec![Arc::new(count_array)]).unwrap(),
                )),
            ],
        )
        .unwrap();
        // write some data
        DeltaOps(table)
            .write(vec![batch.clone()])
            .with_schema_mode(crate::operations::write::SchemaMode::Overwrite)
            .with_save_mode(SaveMode::Overwrite)
            .await
            .unwrap()
    }

    fn merge_source(schema: Arc<ArrowSchema>) -> DataFrame {
        let ctx = SessionContext::new();
        let batch = RecordBatch::try_new(
            Arc::clone(&schema),
            vec![
                Arc::new(arrow::array::StringArray::from(vec!["B", "C", "X"])),
                Arc::new(arrow::array::Int32Array::from(vec![10, 20, 30])),
                Arc::new(arrow::array::StringArray::from(vec![
                    "2021-02-02",
                    "2023-07-04",
                    "2023-07-04",
                ])),
            ],
        )
        .unwrap();
        ctx.read_batch(batch).unwrap()
    }

    async fn setup() -> (DeltaTable, DataFrame) {
        let schema = get_arrow_schema(&None);
        let table = setup_table(None).await;

        let table = write_data(table, &schema).await;
        assert_eq!(table.version(), Some(1));
        assert_eq!(table.get_files_count(), 1);

        (table, merge_source(schema))
    }

    async fn assert_merge(table: DeltaTable, metrics: MergeMetrics) {
        assert_eq!(table.version(), Some(2));
        assert!(table.get_files_count() >= 1);
        assert!(metrics.num_target_files_added >= 1);
        assert_eq!(metrics.num_target_files_removed, 1);
        assert_eq!(metrics.num_target_rows_copied, 1);
        assert_eq!(metrics.num_target_rows_updated, 3);
        assert_eq!(metrics.num_target_rows_inserted, 1);
        assert_eq!(metrics.num_target_rows_deleted, 0);
        assert_eq!(metrics.num_output_rows, 5);
        assert_eq!(metrics.num_source_rows, 3);

        let expected = vec![
            "+----+-------+------------+",
            "| id | value | modified   |",
            "+----+-------+------------+",
            "| A  | 2     | 2021-02-01 |",
            "| B  | 10    | 2021-02-02 |",
            "| C  | 20    | 2023-07-04 |",
            "| D  | 100   | 2021-02-02 |",
            "| X  | 30    | 2023-07-04 |",
            "+----+-------+------------+",
        ];
        let actual = get_data(&table).await;
        assert_batches_sorted_eq!(&expected, &actual);
    }

    #[tokio::test]
    async fn test_merge() {
        let (table, source) = setup().await;

        let (table, metrics) = DeltaOps(table)
            .merge(source, col("target.id").eq(col("source.id")))
            .with_source_alias("source")
            .with_target_alias("target")
            .when_matched_update(|update| {
                update
                    .update("value", col("source.value"))
                    .update("modified", col("source.modified"))
            })
            .unwrap()
            .when_not_matched_by_source_update(|update| {
                update
                    .predicate(col("target.value").eq(lit(1)))
                    .update("value", col("target.value") + lit(1))
            })
            .unwrap()
            .when_not_matched_insert(|insert| {
                insert
                    .set("id", col("source.id"))
                    .set("value", col("source.value"))
                    .set("modified", col("source.modified"))
            })
            .unwrap()
            .await
            .unwrap();

        let commit_info = table.history(None).await.unwrap();

        let last_commit = &commit_info[0];
        let parameters = last_commit.operation_parameters.clone().unwrap();
        assert!(!parameters.contains_key("predicate"));
        assert_eq!(parameters["mergePredicate"], json!("target.id = source.id"));
        assert_eq!(
            parameters["matchedPredicates"],
            json!(r#"[{"actionType":"update"}]"#)
        );
        assert_eq!(
            parameters["notMatchedPredicates"],
            json!(r#"[{"actionType":"insert"}]"#)
        );
        assert_eq!(
            parameters["notMatchedBySourcePredicates"],
            json!(r#"[{"actionType":"update","predicate":"target.value = 1"}]"#)
        );

        assert_merge(table, metrics).await;
    }
    #[tokio::test]
    async fn test_merge_with_schema_merge_no_change_of_schema() {
        let (table, _) = setup().await;

        let schema = Arc::new(ArrowSchema::new(vec![
            Field::new("id", ArrowDataType::LargeUtf8, true),
            Field::new("value", ArrowDataType::Int32, true),
            Field::new("modified", ArrowDataType::Utf8, true),
        ]));
        let ctx = SessionContext::new();
        let batch = RecordBatch::try_new(
            Arc::clone(&schema),
            vec![
                Arc::new(arrow::array::LargeStringArray::from(vec!["B", "C", "X"])),
                Arc::new(arrow::array::Int32Array::from(vec![10, 20, 30])),
                Arc::new(arrow::array::StringArray::from(vec![
                    "2021-02-02",
                    "2023-07-04",
                    "2023-07-04",
                ])),
            ],
        )
        .unwrap();
        let source = ctx.read_batch(batch).unwrap();

        let (after_table, metrics) = DeltaOps(table.clone())
            .merge(source, col("target.id").eq(col("source.id")))
            .with_source_alias("source")
            .with_target_alias("target")
            .with_merge_schema(true)
            .when_matched_update(|update| {
                update
                    .update("value", col("source.value"))
                    .update("modified", col("source.modified"))
            })
            .unwrap()
            .when_not_matched_by_source_update(|update| {
                update
                    .predicate(col("target.value").eq(lit(1)))
                    .update("value", col("target.value") + lit(1))
            })
            .unwrap()
            .when_not_matched_insert(|insert| {
                insert
                    .set("id", col("source.id"))
                    .set("value", col("source.value"))
                    .set("modified", col("source.modified"))
            })
            .unwrap()
            .await
            .unwrap();

        let commit_info = after_table.history(None).await.unwrap();
        let last_commit = &commit_info[0];

        let parameters = last_commit.operation_parameters.clone().unwrap();
        assert!(!parameters.contains_key("predicate"));
        assert_eq!(parameters["mergePredicate"], json!("target.id = source.id"));
        assert_eq!(
            parameters["matchedPredicates"],
            json!(r#"[{"actionType":"update"}]"#)
        );
        assert_eq!(
            parameters["notMatchedPredicates"],
            json!(r#"[{"actionType":"insert"}]"#)
        );
        assert_eq!(
            parameters["notMatchedBySourcePredicates"],
            json!(r#"[{"actionType":"update","predicate":"target.value = 1"}]"#)
        );

        assert_eq!(table.schema(), after_table.schema());

        let snapshot_bytes = after_table
            .log_store
            .read_commit_entry(2)
            .await
            .unwrap()
            .expect("failed to get snapshot bytes");
        let actions = crate::logstore::get_actions(2, snapshot_bytes)
            .await
            .unwrap();

        let schema_actions = actions
            .iter()
            .any(|action| matches!(action, Action::Metadata(_)));

        assert!(!schema_actions);
        assert_merge(after_table, metrics).await;
    }

    #[tokio::test]
    async fn test_merge_with_schema_merge_and_struct() {
        let (table, _) = setup().await;

        let nested_schema = Arc::new(ArrowSchema::new(vec![Field::new(
            "count",
            ArrowDataType::Int64,
            true,
        )]));

        let schema = Arc::new(ArrowSchema::new(vec![
            Field::new("id", ArrowDataType::Utf8, true),
            Field::new("value", ArrowDataType::Int32, true),
            Field::new("modified", ArrowDataType::Utf8, true),
            Field::new(
                "nested",
                ArrowDataType::Struct(nested_schema.fields().clone()),
                true,
            ),
        ]));
        let count_array = arrow::array::Int64Array::from(vec![Some(1)]);
        let id_array = arrow::array::StringArray::from(vec![Some("X")]);
        let value_array = arrow::array::Int32Array::from(vec![Some(1)]);
        let modified_array = arrow::array::StringArray::from(vec![Some("2021-02-02")]);

        let batch = RecordBatch::try_new(
            schema,
            vec![
                Arc::new(id_array),
                Arc::new(value_array),
                Arc::new(modified_array),
                Arc::new(arrow::array::StructArray::from(
                    RecordBatch::try_new(nested_schema, vec![Arc::new(count_array)]).unwrap(),
                )),
            ],
        )
        .unwrap();

        let ctx = SessionContext::new();

        let source = ctx.read_batch(batch).unwrap();

        let (table, _) = DeltaOps(table.clone())
            .merge(source, col("target.id").eq(col("source.id")))
            .with_source_alias("source")
            .with_target_alias("target")
            .with_merge_schema(true)
            .when_not_matched_insert(|insert| {
                insert
                    .set("id", col("source.id"))
                    .set("value", col("source.value"))
                    .set("modified", col("source.modified"))
                    .set("nested", col("source.nested"))
            })
            .unwrap()
            .await
            .unwrap();

        let snapshot_bytes = table
            .log_store
            .read_commit_entry(2)
            .await
            .unwrap()
            .expect("failed to get snapshot bytes");
        let actions = crate::logstore::get_actions(2, snapshot_bytes)
            .await
            .unwrap();

        let schema_actions = actions
            .iter()
            .any(|action| matches!(action, Action::Metadata(_)));

        dbg!(&schema_actions);

        assert!(schema_actions);
        let expected = vec![
            "+----+-------+------------+------------+",
            "| id | value | modified   | nested     |",
            "+----+-------+------------+------------+",
            "| A  | 1     | 2021-02-01 |            |",
            "| B  | 10    | 2021-02-01 |            |",
            "| C  | 10    | 2021-02-02 |            |",
            "| D  | 100   | 2021-02-02 |            |",
            "| X  | 1     | 2021-02-02 | {count: 1} |",
            "+----+-------+------------+------------+",
        ];
        let actual = get_data(&table).await;

        assert_batches_sorted_eq!(&expected, &actual);
    }

    #[tokio::test]
    async fn test_merge_with_schema_merge_and_pre_existing_struct_added_column() {
        let table = setup_table(None).await;

        let nested_schema = Arc::new(ArrowSchema::new(vec![Field::new(
            "count",
            ArrowDataType::Int64,
            true,
        )]));

        let schema = Arc::new(ArrowSchema::new(vec![
            Field::new("id", ArrowDataType::Utf8, true),
            Field::new("value", ArrowDataType::Int32, true),
            Field::new("modified", ArrowDataType::Utf8, true),
            Field::new(
                "nested",
                ArrowDataType::Struct(nested_schema.fields().clone()),
                true,
            ),
        ]));

        let table_with_struct = write_data_struct(table, &schema).await;

        let nested_schema_source = Arc::new(ArrowSchema::new(vec![Field::new(
            "name",
            ArrowDataType::Utf8,
            true,
        )]));

        let schema_source = Arc::new(ArrowSchema::new(vec![
            Field::new("id", ArrowDataType::Utf8, true),
            Field::new("value", ArrowDataType::Int32, true),
            Field::new("modified", ArrowDataType::Utf8, true),
            Field::new(
                "nested",
                ArrowDataType::Struct(nested_schema_source.fields().clone()),
                true,
            ),
        ]));

        let name_array = arrow::array::StringArray::from(vec![Some("John")]);
        let id_array = arrow::array::StringArray::from(vec![Some("X")]);
        let value_array = arrow::array::Int32Array::from(vec![Some(1)]);
        let modified_array = arrow::array::StringArray::from(vec![Some("2021-02-02")]);

        let batch = RecordBatch::try_new(
            schema_source,
            vec![
                Arc::new(id_array),
                Arc::new(value_array),
                Arc::new(modified_array),
                Arc::new(arrow::array::StructArray::from(
                    RecordBatch::try_new(nested_schema_source, vec![Arc::new(name_array)]).unwrap(),
                )),
            ],
        )
        .unwrap();

        let ctx = SessionContext::new();

        let source = ctx.read_batch(batch).unwrap();

        let (table, _) = DeltaOps(table_with_struct)
            .merge(source, col("target.id").eq(col("source.id")))
            .with_source_alias("source")
            .with_target_alias("target")
            .with_merge_schema(true)
            .when_not_matched_insert(|insert| {
                insert
                    .set("id", col("source.id"))
                    .set("value", col("source.value"))
                    .set("modified", col("source.modified"))
                    .set("nested", col("source.nested"))
            })
            .unwrap()
            .await
            .unwrap();

        let snapshot_bytes = table
            .log_store
            .read_commit_entry(2)
            .await
            .unwrap()
            .expect("failed to get snapshot bytes");
        let actions = crate::logstore::get_actions(2, snapshot_bytes)
            .await
            .unwrap();

        let schema_actions = actions
            .iter()
            .any(|action| matches!(action, Action::Metadata(_)));

        dbg!(&schema_actions);

        assert!(schema_actions);
        let expected = vec![
            "+----+-------+------------+-----------------------+",
            "| id | value | modified   | nested                |",
            "+----+-------+------------+-----------------------+",
            "| A  | 1     | 2021-02-01 | {count: 1, name: }    |",
            "| B  | 10    | 2021-02-01 | {count: 2, name: }    |",
            "| C  | 10    | 2021-02-02 | {count: 3, name: }    |",
            "| D  | 100   | 2021-02-02 | {count: 4, name: }    |",
            "| X  | 1     | 2021-02-02 | {count: , name: John} |",
            "+----+-------+------------+-----------------------+",
        ];
        let actual = get_data(&table).await;

        assert_batches_sorted_eq!(&expected, &actual);
    }

    #[tokio::test]
    async fn test_merge_schema_evolution_simple_update() {
        let (table, _) = setup().await;

        let schema = Arc::new(ArrowSchema::new(vec![
            Field::new("id", ArrowDataType::Utf8, true),
            Field::new("value", ArrowDataType::Int32, true),
            Field::new("modified", ArrowDataType::Utf8, true),
            Field::new("inserted_by", ArrowDataType::Utf8, true),
        ]));
        let ctx = SessionContext::new();
        let batch = RecordBatch::try_new(
            Arc::clone(&schema),
            vec![
                Arc::new(arrow::array::StringArray::from(vec!["B", "C", "X"])),
                Arc::new(arrow::array::Int32Array::from(vec![50, 200, 30])),
                Arc::new(arrow::array::StringArray::from(vec![
                    "2021-02-02",
                    "2023-07-04",
                    "2023-07-04",
                ])),
                Arc::new(arrow::array::StringArray::from(vec!["B1", "C1", "X1"])),
            ],
        )
        .unwrap();
        let source = ctx.read_batch(batch).unwrap();

        let (table, _) = DeltaOps(table)
            .merge(source, col("target.id").eq(col("source.id")))
            .with_source_alias("source")
            .with_target_alias("target")
            .with_merge_schema(true)
            .when_matched_update(|update| {
                update
                    .update("value", col("source.value").add(lit(1)))
                    .update("modified", col("source.modified"))
                    .update("inserted_by", col("source.inserted_by"))
            })
            .unwrap()
            .await
            .unwrap();

        let commit_info = table.history(None).await.unwrap();

        let last_commit = &commit_info[0];
        let parameters = last_commit.operation_parameters.clone().unwrap();
        assert_eq!(parameters["mergePredicate"], json!("target.id = source.id"));
        let expected = vec![
            "+----+-------+------------+-------------+",
            "| id | value | modified   | inserted_by |",
            "+----+-------+------------+-------------+",
            "| A  | 1     | 2021-02-01 |             |",
            "| B  | 51    | 2021-02-02 | B1          |",
            "| C  | 201   | 2023-07-04 | C1          |",
            "| D  | 100   | 2021-02-02 |             |",
            "+----+-------+------------+-------------+",
        ];
        let actual = get_data(&table).await;
        let expected_schema_struct: StructType = Arc::clone(&schema).try_into_kernel().unwrap();
        assert_eq!(&expected_schema_struct, table.schema().unwrap());
        assert_batches_sorted_eq!(&expected, &actual);
    }

    #[tokio::test]
    async fn test_merge_schema_evolution_simple_insert() {
        let (table, _) = setup().await;

        let schema = Arc::new(ArrowSchema::new(vec![
            Field::new("id", ArrowDataType::Utf8, true),
            Field::new("value", ArrowDataType::Int32, true),
            Field::new("modified", ArrowDataType::Utf8, true),
            Field::new("inserted_by", ArrowDataType::Utf8, true),
        ]));
        let ctx = SessionContext::new();
        let batch = RecordBatch::try_new(
            Arc::clone(&schema),
            vec![
                Arc::new(arrow::array::StringArray::from(vec!["B", "C", "X"])),
                Arc::new(arrow::array::Int32Array::from(vec![10, 20, 30])),
                Arc::new(arrow::array::StringArray::from(vec![
                    "2021-02-02",
                    "2023-07-04",
                    "2023-07-04",
                ])),
                Arc::new(arrow::array::StringArray::from(vec!["B1", "C1", "X1"])),
            ],
        )
        .unwrap();
        let source = ctx.read_batch(batch).unwrap();

        let (table, _) = DeltaOps(table)
            .merge(source, col("target.id").eq(col("source.id")))
            .with_source_alias("source")
            .with_target_alias("target")
            .with_merge_schema(true)
            .when_not_matched_insert(|insert| {
                insert
                    .set("id", col("source.id"))
                    .set("value", col("source.value"))
                    .set("modified", col("source.modified"))
                    .set("inserted_by", "source.inserted_by")
            })
            .unwrap()
            .await
            .unwrap();

        let commit_info = table.history(None).await.unwrap();
        let last_commit = &commit_info[0];
        let parameters = last_commit.operation_parameters.clone().unwrap();
        assert_eq!(parameters["mergePredicate"], json!("target.id = source.id"));
        assert_eq!(
            parameters["notMatchedPredicates"],
            json!(r#"[{"actionType":"insert"}]"#)
        );
        let expected = vec![
            "+----+-------+------------+-------------+",
            "| id | value | modified   | inserted_by |",
            "+----+-------+------------+-------------+",
            "| A  | 1     | 2021-02-01 |             |",
            "| B  | 10    | 2021-02-01 |             |",
            "| C  | 10    | 2021-02-02 |             |",
            "| D  | 100   | 2021-02-02 |             |",
            "| X  | 30    | 2023-07-04 | X1          |",
            "+----+-------+------------+-------------+",
        ];
        let actual = get_data(&table).await;
        let expected_schema_struct: StructType = Arc::clone(&schema).try_into_kernel().unwrap();
        assert_eq!(&expected_schema_struct, table.schema().unwrap());
        assert_batches_sorted_eq!(&expected, &actual);
    }

    #[tokio::test]
    async fn test_merge_str() {
        // Validate that users can use string predicates
        // Also validates that update and set operations can contain the target alias
        let (table, source) = setup().await;

        let (table, metrics) = DeltaOps(table)
            .merge(source, "target.id = source.id")
            .with_source_alias("source")
            .with_target_alias("target")
            .when_matched_update(|update| {
                update
                    .update("target.value", "source.value")
                    .update("modified", "source.modified")
            })
            .unwrap()
            .when_not_matched_by_source_update(|update| {
                update
                    .predicate("target.value = 1")
                    .update("value", "target.value + cast(1 as int)")
            })
            .unwrap()
            .when_not_matched_insert(|insert| {
                insert
                    .set("target.id", "source.id")
                    .set("value", "source.value")
                    .set("modified", "source.modified")
            })
            .unwrap()
            .await
            .unwrap();

        let commit_info = table.history(None).await.unwrap();
        let last_commit = &commit_info[0];
        let parameters = last_commit.operation_parameters.clone().unwrap();
        assert!(!parameters.contains_key("predicate"));
        assert_eq!(parameters["mergePredicate"], json!("target.id = source.id"));
        assert_eq!(
            parameters["matchedPredicates"],
            json!(r#"[{"actionType":"update"}]"#)
        );
        assert_eq!(
            parameters["notMatchedPredicates"],
            json!(r#"[{"actionType":"insert"}]"#)
        );
        assert_eq!(
            parameters["notMatchedBySourcePredicates"],
            json!(r#"[{"actionType":"update","predicate":"target.value = 1"}]"#)
        );

        assert_merge(table, metrics).await;
    }

    #[tokio::test]
    async fn test_merge_no_alias() {
        // Validate merge can be used without specifying an alias
        let (table, source) = setup().await;

        let source = source
            .with_column_renamed("id", "source_id")
            .unwrap()
            .with_column_renamed("value", "source_value")
            .unwrap()
            .with_column_renamed("modified", "source_modified")
            .unwrap();

        let (table, metrics) = DeltaOps(table)
            .merge(source, "id = source_id")
            .when_matched_update(|update| {
                update
                    .update("value", "source_value")
                    .update("modified", "source_modified")
            })
            .unwrap()
            .when_not_matched_by_source_update(|update| {
                update.predicate("value = 1").update("value", "value + 1")
            })
            .unwrap()
            .when_not_matched_insert(|insert| {
                insert
                    .set("id", "source_id")
                    .set("value", "source_value")
                    .set("modified", "source_modified")
            })
            .unwrap()
            .await
            .unwrap();

        assert_merge(table, metrics).await;
    }

    #[tokio::test]
    async fn test_merge_with_alias_mix() {
        // Validate merge can be used with an alias and unambiguous column references
        // I.E users should be able to specify an alias and still reference columns without using that alias when there is no ambiguity
        let (table, source) = setup().await;

        let source = source
            .with_column_renamed("id", "source_id")
            .unwrap()
            .with_column_renamed("value", "source_value")
            .unwrap()
            .with_column_renamed("modified", "source_modified")
            .unwrap();

        let (table, metrics) = DeltaOps(table)
            .merge(source, "id = source_id")
            .with_target_alias("target")
            .when_matched_update(|update| {
                update
                    .update("value", "source_value")
                    .update("modified", "source_modified")
            })
            .unwrap()
            .when_not_matched_by_source_update(|update| {
                update
                    .predicate("value = 1")
                    .update("value", "target.value + 1")
            })
            .unwrap()
            .when_not_matched_insert(|insert| {
                insert
                    .set("id", "source_id")
                    .set("target.value", "source_value")
                    .set("modified", "source_modified")
            })
            .unwrap()
            .await
            .unwrap();

        assert_merge(table, metrics).await;
    }

    #[tokio::test]
    async fn test_merge_failures() {
        // Validate target columns cannot be from the source
        let (table, source) = setup().await;
        let res = DeltaOps(table)
            .merge(source, col("target.id").eq(col("source.id")))
            .with_source_alias("source")
            .with_target_alias("target")
            .when_matched_update(|update| {
                update
                    .update("source.value", "source.value")
                    .update("modified", "source.modified")
            })
            .unwrap()
            .await;
        assert!(res.is_err());

        // Validate failure when aliases are the same
        let (table, source) = setup().await;
        let res = DeltaOps(table)
            .merge(source, col("target.id").eq(col("source.id")))
            .with_source_alias("source")
            .with_target_alias("source")
            .when_matched_update(|update| {
                update
                    .update("target.value", "source.value")
                    .update("modified", "source.modified")
            })
            .unwrap()
            .await;
        assert!(res.is_err())
    }

    #[tokio::test]
    async fn test_merge_partitions() {
        /* Validate the join predicate works with table partitions */
        let schema = get_arrow_schema(&None);
        let table = setup_table(Some(vec!["modified"])).await;

        let table = write_data(table, &schema).await;
        assert_eq!(table.version(), Some(1));
        assert_eq!(table.get_files_count(), 2);

        let ctx = SessionContext::new();
        let batch = RecordBatch::try_new(
            Arc::clone(&schema),
            vec![
                Arc::new(arrow::array::StringArray::from(vec!["B", "C", "X"])),
                Arc::new(arrow::array::Int32Array::from(vec![10, 20, 30])),
                Arc::new(arrow::array::StringArray::from(vec![
                    "2021-02-02",
                    "2023-07-04",
                    "2023-07-04",
                ])),
            ],
        )
        .unwrap();
        let source = ctx.read_batch(batch).unwrap();

        let (table, metrics) = DeltaOps(table)
            .merge(
                source,
                col("target.id")
                    .eq(col("source.id"))
                    .and(col("target.modified").eq(lit("2021-02-02"))),
            )
            .with_source_alias("source")
            .with_target_alias("target")
            .when_matched_update(|update| {
                update
                    .update("value", col("source.value"))
                    .update("modified", col("source.modified"))
            })
            .unwrap()
            .when_not_matched_by_source_update(|update| {
                update
                    .predicate(col("target.value").eq(lit(1)))
                    .update("value", col("target.value") + lit(1))
            })
            .unwrap()
            .when_not_matched_by_source_update(|update| {
                update
                    .predicate(col("target.modified").eq(lit("2021-02-01")))
                    .update("value", col("target.value") - lit(1))
            })
            .unwrap()
            .when_not_matched_insert(|insert| {
                insert
                    .set("id", col("source.id"))
                    .set("value", col("source.value"))
                    .set("modified", col("source.modified"))
            })
            .unwrap()
            .await
            .unwrap();

        assert_eq!(table.version(), Some(2));
        assert!(table.get_files_count() >= 3);
        assert!(metrics.num_target_files_added >= 3);
        assert_eq!(metrics.num_target_files_removed, 2);
        assert_eq!(metrics.num_target_rows_copied, 1);
        assert_eq!(metrics.num_target_rows_updated, 3);
        assert_eq!(metrics.num_target_rows_inserted, 2);
        assert_eq!(metrics.num_target_rows_deleted, 0);
        assert_eq!(metrics.num_output_rows, 6);
        assert_eq!(metrics.num_source_rows, 3);

        let commit_info = table.history(None).await.unwrap();
        let last_commit = &commit_info[0];
        let parameters = last_commit.operation_parameters.clone().unwrap();
        assert!(!parameters.contains_key("predicate"));
        assert_eq!(
            parameters["mergePredicate"],
            "target.id = source.id AND target.modified = '2021-02-02'"
        );

        let expected = vec![
            "+----+-------+------------+",
            "| id | value | modified   |",
            "+----+-------+------------+",
            "| A  | 2     | 2021-02-01 |",
            "| B  | 9     | 2021-02-01 |",
            "| B  | 10    | 2021-02-02 |",
            "| C  | 20    | 2023-07-04 |",
            "| D  | 100   | 2021-02-02 |",
            "| X  | 30    | 2023-07-04 |",
            "+----+-------+------------+",
        ];
        let actual = get_data(&table).await;
        assert_batches_sorted_eq!(&expected, &actual);
    }

    #[tokio::test]
    async fn test_merge_partition_filtered() {
        let schema = get_arrow_schema(&None);
        let table = setup_table(Some(vec!["modified"])).await;
        let table = write_data(table, &schema).await;
        assert_eq!(table.version(), Some(1));
        let ctx = SessionContext::new();
        let batch = RecordBatch::try_new(
            Arc::clone(&schema),
            vec![
                Arc::new(arrow::array::StringArray::from(vec!["B", "C"])),
                Arc::new(arrow::array::Int32Array::from(vec![10, 20])),
                Arc::new(arrow::array::StringArray::from(vec![
                    "2021-02-02",
                    "2021-02-02",
                ])),
            ],
        )
        .unwrap();
        let source = ctx.read_batch(batch).unwrap();
        let (table, _metrics) = DeltaOps(table)
            .merge(
                source,
                col("target.id")
                    .eq(col("source.id"))
                    .and(col("target.modified").eq(lit("2021-02-02"))),
            )
            .with_source_alias("source")
            .with_target_alias("target")
            .when_matched_update(|update| {
                update
                    .update("value", col("source.value"))
                    .update("modified", col("source.modified"))
            })
            .unwrap()
            .when_not_matched_insert(|insert| {
                insert
                    .set("id", col("source.id"))
                    .set("value", col("source.value"))
                    .set("modified", col("source.modified"))
            })
            .unwrap()
            .await
            .unwrap();
        assert_eq!(table.version(), Some(2));
        let commit_info = table.history(None).await.unwrap();
        let last_commit = &commit_info[0];
        let parameters = last_commit.operation_parameters.clone().unwrap();
        assert_eq!(
            parameters["predicate"],
            "id >= 'B' AND id <= 'C' AND modified = '2021-02-02'"
        );
        assert_eq!(
            parameters["mergePredicate"],
            "target.id = source.id AND target.modified = '2021-02-02'"
        );
    }

    #[tokio::test]
    async fn test_merge_partitions_skipping() {
        /* Validate the join predicate can be used for skipping partitions */
        let schema = get_arrow_schema(&None);
        let table = setup_table(Some(vec!["id"])).await;

        let table = write_data(table, &schema).await;
        assert_eq!(table.version(), Some(1));
        assert_eq!(table.get_files_count(), 4);

        let ctx = SessionContext::new();
        let batch = RecordBatch::try_new(
            Arc::clone(&schema),
            vec![
                Arc::new(arrow::array::StringArray::from(vec!["B", "C", "X"])),
                Arc::new(arrow::array::Int32Array::from(vec![999, 999, 999])),
                Arc::new(arrow::array::StringArray::from(vec![
                    "2023-07-04",
                    "2023-07-04",
                    "2023-07-04",
                ])),
            ],
        )
        .unwrap();
        let source = ctx.read_batch(batch).unwrap();

        let (table, metrics) = DeltaOps(table)
            .merge(source, col("target.id").eq(col("source.id")))
            .with_source_alias("source")
            .with_target_alias("target")
            .when_matched_update(|update| {
                update
                    .update("value", col("source.value"))
                    .update("modified", col("source.modified"))
            })
            .unwrap()
            .when_not_matched_insert(|insert| {
                insert
                    .set("id", col("source.id"))
                    .set("value", col("source.value"))
                    .set("modified", col("source.modified"))
            })
            .unwrap()
            .await
            .unwrap();

        assert_eq!(table.version(), Some(2));
        assert!(table.get_files_count() >= 3);
        assert_eq!(metrics.num_target_files_added, 3);
        assert_eq!(metrics.num_target_files_removed, 2);
        assert_eq!(metrics.num_target_rows_copied, 0);
        assert_eq!(metrics.num_target_rows_updated, 2);
        assert_eq!(metrics.num_target_rows_inserted, 1);
        assert_eq!(metrics.num_target_rows_deleted, 0);
        assert_eq!(metrics.num_output_rows, 3);
        assert_eq!(metrics.num_source_rows, 3);

        let commit_info = table.history(None).await.unwrap();
        let last_commit = &commit_info[0];
        let parameters = last_commit.operation_parameters.clone().unwrap();
        let predicate = parameters["predicate"].as_str().unwrap();
        let re = Regex::new(r"^id = '(C|X|B)' OR id = '(C|X|B)' OR id = '(C|X|B)'$").unwrap();
        assert!(re.is_match(predicate));

        let expected = vec![
            "+-------+------------+----+",
            "| value | modified   | id |",
            "+-------+------------+----+",
            "| 1     | 2021-02-01 | A  |",
            "| 100   | 2021-02-02 | D  |",
            "| 999   | 2023-07-04 | B  |",
            "| 999   | 2023-07-04 | C  |",
            "| 999   | 2023-07-04 | X  |",
            "+-------+------------+----+",
        ];
        let actual = get_data(&table).await;
        assert_batches_sorted_eq!(&expected, &actual);
    }

    #[tokio::test]
    async fn test_merge_partitions_with_in() {
        /* Validate the join predicate works with table partitions */
        let schema = get_arrow_schema(&None);
        let table = setup_table(Some(vec!["modified"])).await;

        let table = write_data(table, &schema).await;
        assert_eq!(table.version(), Some(1));
        assert_eq!(table.get_files_count(), 2);

        let ctx = SessionContext::new();
        let batch = RecordBatch::try_new(
            Arc::clone(&schema),
            vec![
                Arc::new(arrow::array::StringArray::from(vec!["B", "C", "X"])),
                Arc::new(arrow::array::Int32Array::from(vec![10, 20, 30])),
                Arc::new(arrow::array::StringArray::from(vec![
                    "2021-02-02",
                    "2023-07-04",
                    "2023-07-04",
                ])),
            ],
        )
        .unwrap();
        let source = ctx.read_batch(batch).unwrap();

        let (table, metrics) = DeltaOps(table)
            .merge(
                source,
                col("target.id")
                    .eq(col("source.id"))
                    .and(col("target.id").in_list(
                        vec![
                            col("source.id"),
                            col("source.modified"),
                            col("source.value"),
                        ],
                        false,
                    ))
                    .and(col("target.modified").in_list(vec![lit("2021-02-02")], false)),
            )
            .with_source_alias("source")
            .with_target_alias("target")
            .when_matched_update(|update| {
                update
                    .update("value", col("source.value"))
                    .update("modified", col("source.modified"))
            })
            .unwrap()
            .when_not_matched_by_source_update(|update| {
                update
                    .predicate(col("target.value").eq(lit(1)))
                    .update("value", col("target.value") + lit(1))
            })
            .unwrap()
            .when_not_matched_by_source_update(|update| {
                update
                    .predicate(col("target.modified").eq(lit("2021-02-01")))
                    .update("value", col("target.value") - lit(1))
            })
            .unwrap()
            .when_not_matched_insert(|insert| {
                insert
                    .set("id", col("source.id"))
                    .set("value", col("source.value"))
                    .set("modified", col("source.modified"))
            })
            .unwrap()
            .await
            .unwrap();

        assert_eq!(table.version(), Some(2));
        assert!(table.get_files_count() >= 3);
        assert!(metrics.num_target_files_added >= 3);
        assert_eq!(metrics.num_target_files_removed, 2);
        assert_eq!(metrics.num_target_rows_copied, 1);
        assert_eq!(metrics.num_target_rows_updated, 3);
        assert_eq!(metrics.num_target_rows_inserted, 2);
        assert_eq!(metrics.num_target_rows_deleted, 0);
        assert_eq!(metrics.num_output_rows, 6);
        assert_eq!(metrics.num_source_rows, 3);

        let commit_info = table.history(None).await.unwrap();
        let last_commit = &commit_info[0];
        let parameters = last_commit.operation_parameters.clone().unwrap();
        assert!(!parameters.contains_key("predicate"));
        assert_eq!(
            parameters["mergePredicate"],
            "target.id = source.id AND \
            target.id IN (source.id, source.modified, source.value) AND \
            target.modified IN ('2021-02-02')"
        );

        let expected = vec![
            "+----+-------+------------+",
            "| id | value | modified   |",
            "+----+-------+------------+",
            "| A  | 2     | 2021-02-01 |",
            "| B  | 9     | 2021-02-01 |",
            "| B  | 10    | 2021-02-02 |",
            "| C  | 20    | 2023-07-04 |",
            "| D  | 100   | 2021-02-02 |",
            "| X  | 30    | 2023-07-04 |",
            "+----+-------+------------+",
        ];
        let actual = get_data(&table).await;
        assert_batches_sorted_eq!(&expected, &actual);
    }

    #[tokio::test]
    async fn test_merge_delete_matched() {
        // Validate behaviours of match delete

        let schema = get_arrow_schema(&None);
        let table = setup_table(Some(vec!["modified"])).await;

        let table = write_data(table, &schema).await;
        assert_eq!(table.version(), Some(1));
        assert_eq!(table.get_files_count(), 2);

        let ctx = SessionContext::new();
        let batch = RecordBatch::try_new(
            Arc::clone(&schema),
            vec![
                Arc::new(arrow::array::StringArray::from(vec!["B", "C", "X"])),
                Arc::new(arrow::array::Int32Array::from(vec![10, 20, 30])),
                Arc::new(arrow::array::StringArray::from(vec![
                    "2021-02-02",
                    "2023-07-04",
                    "2023-07-04",
                ])),
            ],
        )
        .unwrap();
        let source = ctx.read_batch(batch).unwrap();

        let (table, metrics) = DeltaOps(table)
            .merge(source, col("target.id").eq(col("source.id")))
            .with_source_alias("source")
            .with_target_alias("target")
            .when_matched_delete(|delete| delete)
            .unwrap()
            .await
            .unwrap();

        assert_eq!(table.version(), Some(2));
        assert!(table.get_files_count() >= 2);
        assert_eq!(metrics.num_target_files_added, 2);
        assert_eq!(metrics.num_target_files_removed, 2);
        assert_eq!(metrics.num_target_rows_copied, 2);
        assert_eq!(metrics.num_target_rows_updated, 0);
        assert_eq!(metrics.num_target_rows_inserted, 0);
        assert_eq!(metrics.num_target_rows_deleted, 2);
        assert_eq!(metrics.num_output_rows, 2);
        assert_eq!(metrics.num_source_rows, 3);

        let commit_info = table.history(None).await.unwrap();
        let last_commit = &commit_info[0];
        let parameters = last_commit.operation_parameters.clone().unwrap();
        let extra_info = last_commit.info.clone();
        assert_eq!(
            extra_info["operationMetrics"],
            serde_json::to_value(&metrics).unwrap()
        );
        assert_eq!(parameters["predicate"], "id >= 'B' AND id <= 'X'");
        assert_eq!(parameters["mergePredicate"], json!("target.id = source.id"));
        assert_eq!(
            parameters["matchedPredicates"],
            json!(r#"[{"actionType":"delete"}]"#)
        );

        let expected = vec![
            "+----+-------+------------+",
            "| id | value | modified   |",
            "+----+-------+------------+",
            "| A  | 1     | 2021-02-01 |",
            "| D  | 100   | 2021-02-02 |",
            "+----+-------+------------+",
        ];
        let actual = get_data(&table).await;
        assert_batches_sorted_eq!(&expected, &actual);

        // Test match delete again but with a predicate
        let schema = get_arrow_schema(&None);
        let table = setup_table(Some(vec!["modified"])).await;

        let table = write_data(table, &schema).await;
        assert_eq!(table.version(), Some(1));
        assert_eq!(table.get_files_count(), 2);

        let ctx = SessionContext::new();
        let batch = RecordBatch::try_new(
            Arc::clone(&schema),
            vec![
                Arc::new(arrow::array::StringArray::from(vec!["B", "C", "X"])),
                Arc::new(arrow::array::Int32Array::from(vec![10, 20, 30])),
                Arc::new(arrow::array::StringArray::from(vec![
                    "2021-02-02",
                    "2023-07-04",
                    "2023-07-04",
                ])),
            ],
        )
        .unwrap();
        let source = ctx.read_batch(batch).unwrap();

        let (table, metrics) = DeltaOps(table)
            .merge(source, col("target.id").eq(col("source.id")))
            .with_source_alias("source")
            .with_target_alias("target")
            .when_matched_delete(|delete| delete.predicate(col("source.value").lt_eq(lit(10))))
            .unwrap()
            .await
            .unwrap();

        assert_eq!(table.version(), Some(2));
        assert!(table.get_files_count() >= 2);
        assert_eq!(metrics.num_target_files_added, 1);
        assert_eq!(metrics.num_target_files_removed, 1);
        assert_eq!(metrics.num_target_rows_copied, 1);
        assert_eq!(metrics.num_target_rows_updated, 0);
        assert_eq!(metrics.num_target_rows_inserted, 0);
        assert_eq!(metrics.num_target_rows_deleted, 1);
        assert_eq!(metrics.num_output_rows, 1);
        assert_eq!(metrics.num_source_rows, 3);

        let commit_info = table.history(None).await.unwrap();
        let last_commit = &commit_info[0];
        let parameters = last_commit.operation_parameters.clone().unwrap();
        assert_eq!(parameters["mergePredicate"], json!("target.id = source.id"));
        assert_eq!(
            parameters["matchedPredicates"],
            json!(r#"[{"actionType":"delete","predicate":"source.value <= 10"}]"#)
        );

        let expected = vec![
            "+----+-------+------------+",
            "| id | value | modified   |",
            "+----+-------+------------+",
            "| A  | 1     | 2021-02-01 |",
            "| C  | 10    | 2021-02-02 |",
            "| D  | 100   | 2021-02-02 |",
            "+----+-------+------------+",
        ];
        let actual = get_data(&table).await;
        assert_batches_sorted_eq!(&expected, &actual);
    }

    #[tokio::test]
    async fn test_merge_delete_not_matched() {
        // Validate behaviours of not match delete

        let schema = get_arrow_schema(&None);
        let table = setup_table(Some(vec!["modified"])).await;

        let table = write_data(table, &schema).await;
        assert_eq!(table.version(), Some(1));
        assert_eq!(table.get_files_count(), 2);

        let ctx = SessionContext::new();
        let batch = RecordBatch::try_new(
            Arc::clone(&schema),
            vec![
                Arc::new(arrow::array::StringArray::from(vec!["B", "C", "X"])),
                Arc::new(arrow::array::Int32Array::from(vec![10, 20, 30])),
                Arc::new(arrow::array::StringArray::from(vec![
                    "2021-02-02",
                    "2023-07-04",
                    "2023-07-04",
                ])),
            ],
        )
        .unwrap();
        let source = ctx.read_batch(batch).unwrap();

        let (table, metrics) = DeltaOps(table)
            .merge(source, col("target.id").eq(col("source.id")))
            .with_source_alias("source")
            .with_target_alias("target")
            .when_not_matched_by_source_delete(|delete| delete)
            .unwrap()
            .await
            .unwrap();

        assert_eq!(table.version(), Some(2));
        assert_eq!(table.get_files_count(), 2);
        assert_eq!(metrics.num_target_files_added, 2);
        assert_eq!(metrics.num_target_files_removed, 2);
        assert_eq!(metrics.num_target_rows_copied, 2);
        assert_eq!(metrics.num_target_rows_updated, 0);
        assert_eq!(metrics.num_target_rows_inserted, 0);
        assert_eq!(metrics.num_target_rows_deleted, 2);
        assert_eq!(metrics.num_output_rows, 2);
        assert_eq!(metrics.num_source_rows, 3);

        let commit_info = table.history(None).await.unwrap();
        let last_commit = &commit_info[0];
        let parameters = last_commit.operation_parameters.clone().unwrap();
        assert!(!parameters.contains_key("predicate"));
        assert_eq!(parameters["mergePredicate"], json!("target.id = source.id"));
        assert_eq!(
            parameters["notMatchedBySourcePredicates"],
            json!(r#"[{"actionType":"delete"}]"#)
        );

        let expected = vec![
            "+----+-------+------------+",
            "| id | value | modified   |",
            "+----+-------+------------+",
            "| B  | 10    | 2021-02-01 |",
            "| C  | 10    | 2021-02-02 |",
            "+----+-------+------------+",
        ];
        let actual = get_data(&table).await;
        assert_batches_sorted_eq!(&expected, &actual);

        let schema = get_arrow_schema(&None);
        let table = setup_table(Some(vec!["modified"])).await;

        let table = write_data(table, &schema).await;
        assert_eq!(table.version(), Some(1));
        assert_eq!(table.get_files_count(), 2);

        let ctx = SessionContext::new();
        let batch = RecordBatch::try_new(
            Arc::clone(&schema),
            vec![
                Arc::new(arrow::array::StringArray::from(vec!["B", "C", "X"])),
                Arc::new(arrow::array::Int32Array::from(vec![10, 20, 30])),
                Arc::new(arrow::array::StringArray::from(vec![
                    "2021-02-02",
                    "2023-07-04",
                    "2023-07-04",
                ])),
            ],
        )
        .unwrap();
        let source = ctx.read_batch(batch).unwrap();

        let (table, metrics) = DeltaOps(table)
            .merge(source, col("target.id").eq(col("source.id")))
            .with_source_alias("source")
            .with_target_alias("target")
            .when_not_matched_by_source_delete(|delete| {
                delete.predicate(col("target.modified").gt(lit("2021-02-01")))
            })
            .unwrap()
            .await
            .unwrap();

        assert_eq!(table.version(), Some(2));
        assert!(metrics.num_target_files_added == 1);
        assert_eq!(metrics.num_target_files_removed, 1);
        assert_eq!(metrics.num_target_rows_copied, 1);
        assert_eq!(metrics.num_target_rows_updated, 0);
        assert_eq!(metrics.num_target_rows_inserted, 0);
        assert_eq!(metrics.num_target_rows_deleted, 1);
        assert_eq!(metrics.num_output_rows, 1);
        assert_eq!(metrics.num_source_rows, 3);

        let commit_info = table.history(None).await.unwrap();
        let last_commit = &commit_info[0];
        let parameters = last_commit.operation_parameters.clone().unwrap();
        assert_eq!(parameters["mergePredicate"], json!("target.id = source.id"));
        assert_eq!(
            parameters["notMatchedBySourcePredicates"],
            json!(r#"[{"actionType":"delete","predicate":"target.modified > '2021-02-01'"}]"#)
        );

        let expected = vec![
            "+----+-------+------------+",
            "| id | value | modified   |",
            "+----+-------+------------+",
            "| A  | 1     | 2021-02-01 |",
            "| B  | 10    | 2021-02-01 |",
            "| C  | 10    | 2021-02-02 |",
            "+----+-------+------------+",
        ];
        let actual = get_data(&table).await;
        assert_batches_sorted_eq!(&expected, &actual);
    }

    #[tokio::test]
    async fn test_merge_delete_not_matched_with_schema_merge() {
        // Validate behaviours of not match delete

        let schema = get_arrow_schema(&None);
        let table = setup_table(Some(vec!["modified"])).await;

        let table = write_data(table, &schema).await;
        assert_eq!(table.version(), Some(1));
        assert_eq!(table.get_files_count(), 2);

        let ctx = SessionContext::new();
        let batch = RecordBatch::try_new(
            Arc::clone(&schema),
            vec![
                Arc::new(arrow::array::StringArray::from(vec!["B", "C", "X"])),
                Arc::new(arrow::array::Int32Array::from(vec![10, 20, 30])),
                Arc::new(arrow::array::StringArray::from(vec![
                    "2021-02-02",
                    "2023-07-04",
                    "2023-07-04",
                ])),
            ],
        )
        .unwrap();
        let source = ctx.read_batch(batch).unwrap();

        let (table, metrics) = DeltaOps(table)
            .merge(source, col("target.id").eq(col("source.id")))
            .with_source_alias("source")
            .with_target_alias("target")
            .with_merge_schema(true)
            .when_not_matched_by_source_delete(|delete| delete)
            .unwrap()
            .await
            .unwrap();

        assert_eq!(table.version(), Some(2));
        assert_eq!(table.get_files_count(), 2);
        assert_eq!(metrics.num_target_files_added, 2);
        assert_eq!(metrics.num_target_files_removed, 2);
        assert_eq!(metrics.num_target_rows_copied, 2);
        assert_eq!(metrics.num_target_rows_updated, 0);
        assert_eq!(metrics.num_target_rows_inserted, 0);
        assert_eq!(metrics.num_target_rows_deleted, 2);
        assert_eq!(metrics.num_output_rows, 2);
        assert_eq!(metrics.num_source_rows, 3);

        let commit_info = table.history(None).await.unwrap();
        let last_commit = &commit_info[0];
        let parameters = last_commit.operation_parameters.clone().unwrap();
        assert!(!parameters.contains_key("predicate"));
        assert_eq!(parameters["mergePredicate"], json!("target.id = source.id"));
        assert_eq!(
            parameters["notMatchedBySourcePredicates"],
            json!(r#"[{"actionType":"delete"}]"#)
        );

        let expected = vec![
            "+----+-------+------------+",
            "| id | value | modified   |",
            "+----+-------+------------+",
            "| B  | 10    | 2021-02-01 |",
            "| C  | 10    | 2021-02-02 |",
            "+----+-------+------------+",
        ];
        let actual = get_data(&table).await;
        assert_batches_sorted_eq!(&expected, &actual);

        let schema = get_arrow_schema(&None);
        let table = setup_table(Some(vec!["modified"])).await;

        let table = write_data(table, &schema).await;
        assert_eq!(table.version(), Some(1));
        assert_eq!(table.get_files_count(), 2);

        let ctx = SessionContext::new();
        let batch = RecordBatch::try_new(
            Arc::clone(&schema),
            vec![
                Arc::new(arrow::array::StringArray::from(vec!["B", "C", "X"])),
                Arc::new(arrow::array::Int32Array::from(vec![10, 20, 30])),
                Arc::new(arrow::array::StringArray::from(vec![
                    "2021-02-02",
                    "2023-07-04",
                    "2023-07-04",
                ])),
            ],
        )
        .unwrap();
        let source = ctx.read_batch(batch).unwrap();

        let (table, metrics) = DeltaOps(table)
            .merge(source, col("target.id").eq(col("source.id")))
            .with_source_alias("source")
            .with_target_alias("target")
            .when_not_matched_by_source_delete(|delete| {
                delete.predicate(col("target.modified").gt(lit("2021-02-01")))
            })
            .unwrap()
            .await
            .unwrap();

        assert_eq!(table.version(), Some(2));
        assert!(metrics.num_target_files_added == 1);
        assert_eq!(metrics.num_target_files_removed, 1);
        assert_eq!(metrics.num_target_rows_copied, 1);
        assert_eq!(metrics.num_target_rows_updated, 0);
        assert_eq!(metrics.num_target_rows_inserted, 0);
        assert_eq!(metrics.num_target_rows_deleted, 1);
        assert_eq!(metrics.num_output_rows, 1);
        assert_eq!(metrics.num_source_rows, 3);

        let commit_info = table.history(None).await.unwrap();
        let last_commit = &commit_info[0];
        let parameters = last_commit.operation_parameters.clone().unwrap();
        assert_eq!(parameters["mergePredicate"], json!("target.id = source.id"));
        assert_eq!(
            parameters["notMatchedBySourcePredicates"],
            json!(r#"[{"actionType":"delete","predicate":"target.modified > '2021-02-01'"}]"#)
        );

        let expected = vec![
            "+----+-------+------------+",
            "| id | value | modified   |",
            "+----+-------+------------+",
            "| A  | 1     | 2021-02-01 |",
            "| B  | 10    | 2021-02-01 |",
            "| C  | 10    | 2021-02-02 |",
            "+----+-------+------------+",
        ];
        let actual = get_data(&table).await;
        assert_batches_sorted_eq!(&expected, &actual);
    }

    #[tokio::test]
    async fn test_merge_empty_table() {
        let schema = get_arrow_schema(&None);
        let table = setup_table(Some(vec!["modified"])).await;

        assert_eq!(table.version(), Some(0));
        assert_eq!(table.get_files_count(), 0);

        let ctx = SessionContext::new();
        let batch = RecordBatch::try_new(
            Arc::clone(&schema),
            vec![
                Arc::new(arrow::array::StringArray::from(vec!["B", "C", "X"])),
                Arc::new(arrow::array::Int32Array::from(vec![10, 20, 30])),
                Arc::new(arrow::array::StringArray::from(vec![
                    "2021-02-02",
                    "2023-07-04",
                    "2023-07-04",
                ])),
            ],
        )
        .unwrap();
        let source = ctx.read_batch(batch).unwrap();

        let (table, metrics) = DeltaOps(table)
            .merge(
                source,
                col("target.id")
                    .eq(col("source.id"))
                    .and(col("target.modified").eq(lit("2021-02-02"))),
            )
            .with_source_alias("source")
            .with_target_alias("target")
            .when_matched_update(|update| {
                update
                    .update("value", col("source.value"))
                    .update("modified", col("source.modified"))
            })
            .unwrap()
            .when_not_matched_insert(|insert| {
                insert
                    .set("id", col("source.id"))
                    .set("value", col("source.value"))
                    .set("modified", col("source.modified"))
            })
            .unwrap()
            .await
            .unwrap();

        assert_eq!(table.version(), Some(1));
        assert!(table.get_files_count() >= 2);
        assert!(metrics.num_target_files_added >= 2);
        assert_eq!(metrics.num_target_files_removed, 0);
        assert_eq!(metrics.num_target_rows_copied, 0);
        assert_eq!(metrics.num_target_rows_updated, 0);
        assert_eq!(metrics.num_target_rows_inserted, 3);
        assert_eq!(metrics.num_target_rows_deleted, 0);
        assert_eq!(metrics.num_output_rows, 3);
        assert_eq!(metrics.num_source_rows, 3);

        let commit_info = table.history(None).await.unwrap();
        let last_commit = &commit_info[0];
        let parameters = last_commit.operation_parameters.clone().unwrap();

        assert_eq!(
            parameters["predicate"],
            json!("id >= 'B' AND id <= 'X' AND modified = '2021-02-02'")
        );

        let expected = vec![
            "+----+-------+------------+",
            "| id | value | modified   |",
            "+----+-------+------------+",
            "| B  | 10    | 2021-02-02 |",
            "| C  | 20    | 2023-07-04 |",
            "| X  | 30    | 2023-07-04 |",
            "+----+-------+------------+",
        ];
        let actual = get_data(&table).await;
        assert_batches_sorted_eq!(&expected, &actual);
    }

    #[tokio::test]
    async fn test_empty_table_with_schema_merge() {
        let schema = Arc::new(ArrowSchema::new(vec![
            Field::new("id", ArrowDataType::Utf8, true),
            Field::new("value", ArrowDataType::Int32, true),
            Field::new("modified", ArrowDataType::Utf8, true),
            Field::new("inserted_by", ArrowDataType::Utf8, true),
        ]));
        let table = setup_table(Some(vec!["modified"])).await;

        assert_eq!(table.version(), Some(0));
        assert_eq!(table.get_files_count(), 0);

        let ctx = SessionContext::new();
        let batch = RecordBatch::try_new(
            Arc::clone(&schema),
            vec![
                Arc::new(arrow::array::StringArray::from(vec!["B", "C", "X"])),
                Arc::new(arrow::array::Int32Array::from(vec![10, 20, 30])),
                Arc::new(arrow::array::StringArray::from(vec![
                    "2021-02-02",
                    "2023-07-04",
                    "2023-07-04",
                ])),
                Arc::new(arrow::array::StringArray::from(vec!["B1", "C1", "X1"])),
            ],
        )
        .unwrap();
        let source = ctx.read_batch(batch).unwrap();

        let (table, metrics) = DeltaOps(table)
            .merge(
                source,
                col("target.id")
                    .eq(col("source.id"))
                    .and(col("target.modified").eq(lit("2021-02-02"))),
            )
            .with_merge_schema(true)
            .with_source_alias("source")
            .with_target_alias("target")
            .when_not_matched_insert(|insert| {
                insert
                    .set("id", col("source.id"))
                    .set("value", col("source.value"))
                    .set("modified", col("source.modified"))
                    .set("inserted_by", col("source.inserted_by"))
            })
            .unwrap()
            .await
            .unwrap();

        assert_eq!(table.version(), Some(1));
        assert!(table.get_files_count() >= 2);
        assert!(metrics.num_target_files_added >= 2);
        assert_eq!(metrics.num_target_files_removed, 0);
        assert_eq!(metrics.num_target_rows_copied, 0);
        assert_eq!(metrics.num_target_rows_updated, 0);
        assert_eq!(metrics.num_target_rows_inserted, 3);
        assert_eq!(metrics.num_target_rows_deleted, 0);
        assert_eq!(metrics.num_output_rows, 3);
        assert_eq!(metrics.num_source_rows, 3);

        let commit_info = table.history(None).await.unwrap();
        let last_commit = &commit_info[0];
        let parameters = last_commit.operation_parameters.clone().unwrap();

        assert_eq!(
            parameters["predicate"],
            json!("id >= 'B' AND id <= 'X' AND modified = '2021-02-02'")
        );

        let expected = vec![
            "+----+-------+-------------+------------+",
            "| id | value | inserted_by | modified   |",
            "+----+-------+-------------+------------+",
            "| B  | 10    | B1          | 2021-02-02 |",
            "| C  | 20    | C1          | 2023-07-04 |",
            "| X  | 30    | X1          | 2023-07-04 |",
            "+----+-------+-------------+------------+",
        ];
        let actual = get_data(&table).await;
        let expected_schema_struct: StructType = Arc::clone(&schema).try_into_kernel().unwrap();
        assert_eq!(&expected_schema_struct, table.schema().unwrap());
        assert_batches_sorted_eq!(&expected, &actual);
    }

    #[tokio::test]
    async fn test_merge_case_sensitive() {
        let schema = vec![
            StructField::new(
                "Id".to_string(),
                DataType::Primitive(PrimitiveType::String),
                true,
            ),
            StructField::new(
                "vAlue".to_string(), // spellchecker:disable-line
                DataType::Primitive(PrimitiveType::Integer),
                true,
            ),
            StructField::new(
                "mOdifieD".to_string(),
                DataType::Primitive(PrimitiveType::String),
                true,
            ),
        ];

        let arrow_schema = Arc::new(ArrowSchema::new(vec![
            Field::new("Id", ArrowDataType::Utf8, true),
            Field::new("vAlue", ArrowDataType::Int32, true), // spellchecker:disable-line
            Field::new("mOdifieD", ArrowDataType::Utf8, true),
        ]));

        let table = DeltaOps::new_in_memory()
            .create()
            .with_columns(schema)
            .await
            .unwrap();

        let ctx = SessionContext::new();
        let batch = RecordBatch::try_new(
            Arc::clone(&arrow_schema.clone()),
            vec![
                Arc::new(arrow::array::StringArray::from(vec!["B", "C", "X"])),
                Arc::new(arrow::array::Int32Array::from(vec![10, 20, 30])),
                Arc::new(arrow::array::StringArray::from(vec![
                    "2021-02-02",
                    "2023-07-04",
                    "2023-07-04",
                ])),
            ],
        )
        .unwrap();
        let source = ctx.read_batch(batch).unwrap();

        let table = write_data(table, &arrow_schema).await;
        assert_eq!(table.version(), Some(1));
        assert_eq!(table.get_files_count(), 1);

        let (table, _metrics) = DeltaOps(table)
            .merge(source, "target.Id = source.Id")
            .with_source_alias("source")
            .with_target_alias("target")
            .when_not_matched_insert(|insert| {
                insert
                    .set("Id", "source.Id")
                    .set("vAlue", "source.vAlue + 1") // spellchecker:disable-line
                    .set("mOdifieD", "source.mOdifieD")
            })
            .unwrap()
            .await
            .unwrap();

        let expected = vec![
            "+----+-------+------------+",
            "| Id | vAlue | mOdifieD   |", // spellchecker:disable-line
            "+----+-------+------------+",
            "| A  | 1     | 2021-02-01 |",
            "| B  | 10    | 2021-02-01 |",
            "| C  | 10    | 2021-02-02 |",
            "| D  | 100   | 2021-02-02 |",
            "| X  | 31    | 2023-07-04 |",
            "+----+-------+------------+",
        ];
        let actual = get_data(&table).await;
        assert_batches_sorted_eq!(&expected, &actual);
    }

    #[tokio::test]
    async fn test_generalize_filter_with_partitions() {
        let source = TableReference::parse_str("source");
        let target = TableReference::parse_str("target");

        let parsed_filter = col(Column::new(source.clone().into(), "id"))
            .eq(col(Column::new(target.clone().into(), "id")));

        let mut placeholders = Vec::default();

        let generalized = generalize_filter(
            parsed_filter,
            &vec!["id".to_owned()],
            &source,
            &target,
            &mut placeholders,
            false,
        )
        .unwrap();

        let expected_filter = Expr::Placeholder(Placeholder {
            id: "id_0".to_owned(),
            data_type: None,
        })
        .eq(col(Column::new(target.clone().into(), "id")));

        assert_eq!(generalized, expected_filter);
    }

    #[tokio::test]
    async fn test_generalize_filter_with_partitions_nulls() {
        let source = TableReference::parse_str("source");
        let target = TableReference::parse_str("target");

        let source_id = col(Column::new(source.clone().into(), "id"));
        let target_id = col(Column::new(target.clone().into(), "id"));

        // source.id = target.id OR (source.id is null and target.id is null)
        let parsed_filter = (source_id.clone().eq(target_id.clone()))
            .or(source_id.clone().is_null().and(target_id.clone().is_null()));

        let mut placeholders = Vec::default();

        let generalized = generalize_filter(
            parsed_filter,
            &vec!["id".to_owned()],
            &source,
            &target,
            &mut placeholders,
            false,
        )
        .unwrap();

        // id_1 = target.id OR (id_2 and target.id is null)
        let expected_filter = Expr::Placeholder(Placeholder {
            id: "id_0".to_owned(),
            data_type: None,
        })
        .eq(target_id.clone())
        .or(Expr::Placeholder(Placeholder {
            id: "id_1".to_owned(),
            data_type: None,
        })
        .and(target_id.clone().is_null()));

        assert_eq!(placeholders.len(), 2);

        let captured_expressions = placeholders.into_iter().map(|p| p.expr).collect_vec();

        assert!(captured_expressions.contains(&source_id));
        assert!(captured_expressions.contains(&source_id.is_null()));

        assert_eq!(generalized, expected_filter);
    }

    #[tokio::test]
    async fn test_generalize_filter_with_partitions_captures_expression() {
        // Check that when generalizing the filter, the placeholder map captures the expression needed to make the statement the same
        // when the distinct values are substitiuted in
        let source = TableReference::parse_str("source");
        let target = TableReference::parse_str("target");

        let parsed_filter = col(Column::new(source.clone().into(), "id"))
            .neg()
            .eq(col(Column::new(target.clone().into(), "id")));

        let mut placeholders = Vec::default();

        let generalized = generalize_filter(
            parsed_filter,
            &vec!["id".to_owned()],
            &source,
            &target,
            &mut placeholders,
            false,
        )
        .unwrap();

        let expected_filter = Expr::Placeholder(Placeholder {
            id: "id_0".to_owned(),
            data_type: None,
        })
        .eq(col(Column::new(target.clone().into(), "id")));

        assert_eq!(generalized, expected_filter);

        assert_eq!(placeholders.len(), 1);
        let placeholder_expr = placeholders.first().unwrap();

        let expected_placeholder = col(Column::new(source.clone().into(), "id")).neg();

        assert_eq!(placeholder_expr.expr, expected_placeholder);
        assert_eq!(placeholder_expr.alias, "id_0");
        assert!(!placeholder_expr.is_aggregate);
    }

    #[tokio::test]
    async fn test_generalize_filter_keeps_static_target_references() {
        let source = TableReference::parse_str("source");
        let target = TableReference::parse_str("target");

        // source.id = target.id and target.id = 'C'
        let parsed_filter = col(Column::new(source.clone().into(), "id"))
            .eq(col(Column::new(target.clone().into(), "id")))
            .and(col(Column::new(target.clone().into(), "id")).eq(lit("C")));

        let mut placeholders = Vec::default();

        let generalized = generalize_filter(
            parsed_filter,
            &vec!["id".to_owned()],
            &source,
            &target,
            &mut placeholders,
            false,
        )
        .unwrap();

        // id_0 = target.id and target.id = 'C'
        let expected_filter = Expr::Placeholder(Placeholder {
            id: "id_0".to_owned(),
            data_type: None,
        })
        .eq(col(Column::new(target.clone().into(), "id")))
        .and(col(Column::new(target.clone().into(), "id")).eq(lit("C")));

        assert_eq!(generalized, expected_filter);
    }

    #[tokio::test]
    async fn test_generalize_filter_with_dynamic_target_range_references() {
        let source = TableReference::parse_str("source");
        let target = TableReference::parse_str("target");

        let parsed_filter = col(Column::new(source.clone().into(), "id"))
            .eq(col(Column::new(target.clone().into(), "id")));

        let mut placeholders = Vec::default();

        let generalized = generalize_filter(
            parsed_filter,
            &vec!["other".to_owned()],
            &source,
            &target,
            &mut placeholders,
            false,
        )
        .unwrap();
        let expected_filter_l = Expr::Placeholder(Placeholder {
            id: "id_0_min".to_owned(),
            data_type: None,
        });
        let expected_filter_h = Expr::Placeholder(Placeholder {
            id: "id_0_max".to_owned(),
            data_type: None,
        });
        let expected_filter = col(Column::new(target.clone().into(), "id"))
            .between(expected_filter_l, expected_filter_h);

        assert_eq!(generalized, expected_filter);
    }

    #[tokio::test]
    async fn test_generalize_filter_removes_source_references() {
        let source = TableReference::parse_str("source");
        let target = TableReference::parse_str("target");

        let parsed_filter = col(Column::new(source.clone().into(), "id"))
            .eq(col(Column::new(target.clone().into(), "id")))
            .and(col(Column::new(source.clone().into(), "id")).eq(lit("C")));

        let mut placeholders = Vec::default();

        let generalized = generalize_filter(
            parsed_filter,
            &vec!["id".to_owned()],
            &source,
            &target,
            &mut placeholders,
            false,
        )
        .unwrap();

        let expected_filter = Expr::Placeholder(Placeholder {
            id: "id_0".to_owned(),
            data_type: None,
        })
        .eq(col(Column::new(target.clone().into(), "id")));

        assert_eq!(generalized, expected_filter);
    }

    #[tokio::test]
    async fn test_merge_pushdowns() {
        //See https://github.com/delta-io/delta-rs/issues/2158
        let schema = vec![
            StructField::new(
                "id".to_string(),
                DataType::Primitive(PrimitiveType::String),
                true,
            ),
            StructField::new(
                "cost".to_string(),
                DataType::Primitive(PrimitiveType::Float),
                true,
            ),
            StructField::new(
                "month".to_string(),
                DataType::Primitive(PrimitiveType::String),
                true,
            ),
        ];

        let arrow_schema = Arc::new(ArrowSchema::new(vec![
            Field::new("id", ArrowDataType::Utf8, true),
            Field::new("cost", ArrowDataType::Float32, true),
            Field::new("month", ArrowDataType::Utf8, true),
        ]));

        let table = DeltaOps::new_in_memory()
            .create()
            .with_columns(schema)
            .await
            .unwrap();

        let ctx = SessionContext::new();
        let batch = RecordBatch::try_new(
            Arc::clone(&arrow_schema.clone()),
            vec![
                Arc::new(arrow::array::StringArray::from(vec!["A", "B"])),
                Arc::new(arrow::array::Float32Array::from(vec![Some(10.15), None])),
                Arc::new(arrow::array::StringArray::from(vec![
                    "2023-07-04",
                    "2023-07-04",
                ])),
            ],
        )
        .unwrap();

        let table = DeltaOps(table)
            .write(vec![batch.clone()])
            .with_save_mode(SaveMode::Append)
            .await
            .unwrap();
        assert_eq!(table.version(), Some(1));
        assert_eq!(table.get_files_count(), 1);

        let batch = RecordBatch::try_new(
            Arc::clone(&arrow_schema.clone()),
            vec![
                Arc::new(arrow::array::StringArray::from(vec!["A", "B"])),
                Arc::new(arrow::array::Float32Array::from(vec![
                    Some(12.15),
                    Some(11.15),
                ])),
                Arc::new(arrow::array::StringArray::from(vec![
                    "2023-07-04",
                    "2023-07-04",
                ])),
            ],
        )
        .unwrap();
        let source = ctx.read_batch(batch).unwrap();

        let (table, _metrics) = DeltaOps(table)
            .merge(source, "target.id = source.id and target.cost is null")
            .with_source_alias("source")
            .with_target_alias("target")
            .when_matched_update(|insert| {
                insert
                    .update("id", "target.id")
                    .update("cost", "source.cost")
                    .update("month", "target.month")
            })
            .unwrap()
            .await
            .unwrap();

        let expected = vec![
            "+----+-------+------------+",
            "| id | cost  | month      |",
            "+----+-------+------------+",
            "| A  | 10.15 | 2023-07-04 |",
            "| B  | 11.15 | 2023-07-04 |",
            "+----+-------+------------+",
        ];
        let actual = get_data(&table).await;
        assert_batches_sorted_eq!(&expected, &actual);
    }

    #[tokio::test]
    async fn test_merge_row_groups_parquet_pushdown() {
        //See https://github.com/delta-io/delta-rs/issues/2362
        let schema = vec![
            StructField::new(
                "id".to_string(),
                DataType::Primitive(PrimitiveType::String),
                true,
            ),
            StructField::new(
                "cost".to_string(),
                DataType::Primitive(PrimitiveType::Float),
                true,
            ),
            StructField::new(
                "month".to_string(),
                DataType::Primitive(PrimitiveType::String),
                true,
            ),
        ];

        let arrow_schema = Arc::new(ArrowSchema::new(vec![
            Field::new("id", ArrowDataType::Utf8, true),
            Field::new("cost", ArrowDataType::Float32, true),
            Field::new("month", ArrowDataType::Utf8, true),
        ]));

        let table = DeltaOps::new_in_memory()
            .create()
            .with_columns(schema)
            .await
            .unwrap();

        let ctx = SessionContext::new();
        let batch1 = RecordBatch::try_new(
            Arc::clone(&arrow_schema.clone()),
            vec![
                Arc::new(arrow::array::StringArray::from(vec!["A", "B"])),
                Arc::new(arrow::array::Float32Array::from(vec![Some(10.15), None])),
                Arc::new(arrow::array::StringArray::from(vec![
                    "2023-07-04",
                    "2023-07-04",
                ])),
            ],
        )
        .unwrap();

        let batch2 = RecordBatch::try_new(
            Arc::clone(&arrow_schema.clone()),
            vec![
                Arc::new(arrow::array::StringArray::from(vec!["C", "D"])),
                Arc::new(arrow::array::Float32Array::from(vec![
                    Some(11.0),
                    Some(12.0),
                ])),
                Arc::new(arrow::array::StringArray::from(vec![
                    "2023-07-04",
                    "2023-07-04",
                ])),
            ],
        )
        .unwrap();

        let table = DeltaOps(table)
            .write(vec![batch1, batch2])
            .with_write_batch_size(2)
            .with_save_mode(SaveMode::Append)
            .await
            .unwrap();
        assert_eq!(table.version(), Some(1));
        assert_eq!(table.get_files_count(), 1);

        let batch = RecordBatch::try_new(
            Arc::clone(&arrow_schema.clone()),
            vec![
                Arc::new(arrow::array::StringArray::from(vec!["C", "E"])),
                Arc::new(arrow::array::Float32Array::from(vec![
                    Some(12.15),
                    Some(11.15),
                ])),
                Arc::new(arrow::array::StringArray::from(vec![
                    "2023-07-04",
                    "2023-07-04",
                ])),
            ],
        )
        .unwrap();
        let source = ctx.read_batch(batch).unwrap();

        let (table, _metrics) = DeltaOps(table)
            .merge(source, "target.id = source.id and target.id >= 'C'")
            .with_source_alias("source")
            .with_target_alias("target")
            .when_matched_update(|insert| {
                insert
                    .update("id", "target.id")
                    .update("cost", "source.cost")
                    .update("month", "target.month")
            })
            .unwrap()
            .when_not_matched_insert(|insert| {
                insert
                    .set("id", "source.id")
                    .set("cost", "source.cost")
                    .set("month", "source.month")
            })
            .unwrap()
            .await
            .unwrap();

        let expected = vec![
            "+----+-------+------------+",
            "| id | cost  | month      |",
            "+----+-------+------------+",
            "| A  | 10.15 | 2023-07-04 |",
            "| B  |       | 2023-07-04 |",
            "| C  | 12.15 | 2023-07-04 |",
            "| D  | 12.0  | 2023-07-04 |",
            "| E  | 11.15 | 2023-07-04 |",
            "+----+-------+------------+",
        ];
        let actual = get_data(&table).await;
        assert_batches_sorted_eq!(&expected, &actual);
    }

    #[tokio::test]
    async fn test_merge_pushdowns_partitioned() {
        //See #2158
        let schema = vec![
            StructField::new(
                "id".to_string(),
                DataType::Primitive(PrimitiveType::String),
                true,
            ),
            StructField::new(
                "cost".to_string(),
                DataType::Primitive(PrimitiveType::Float),
                true,
            ),
            StructField::new(
                "month".to_string(),
                DataType::Primitive(PrimitiveType::String),
                true,
            ),
        ];

        let arrow_schema = Arc::new(ArrowSchema::new(vec![
            Field::new("id", ArrowDataType::Utf8, true),
            Field::new("cost", ArrowDataType::Float32, true),
            Field::new("month", ArrowDataType::Utf8, true),
        ]));

        let part_cols = vec!["month"];
        let table = DeltaOps::new_in_memory()
            .create()
            .with_columns(schema)
            .with_partition_columns(part_cols)
            .await
            .unwrap();

        let ctx = SessionContext::new();
        let batch = RecordBatch::try_new(
            Arc::clone(&arrow_schema.clone()),
            vec![
                Arc::new(arrow::array::StringArray::from(vec!["A", "B"])),
                Arc::new(arrow::array::Float32Array::from(vec![Some(10.15), None])),
                Arc::new(arrow::array::StringArray::from(vec![
                    "2023-07-04",
                    "2023-07-04",
                ])),
            ],
        )
        .unwrap();

        let table = DeltaOps(table)
            .write(vec![batch.clone()])
            .with_save_mode(SaveMode::Append)
            .await
            .unwrap();
        assert_eq!(table.version(), Some(1));
        assert_eq!(table.get_files_count(), 1);

        let batch = RecordBatch::try_new(
            Arc::clone(&arrow_schema.clone()),
            vec![
                Arc::new(arrow::array::StringArray::from(vec!["A", "B"])),
                Arc::new(arrow::array::Float32Array::from(vec![
                    Some(12.15),
                    Some(11.15),
                ])),
                Arc::new(arrow::array::StringArray::from(vec![
                    "2023-07-04",
                    "2023-07-04",
                ])),
            ],
        )
        .unwrap();
        let source = ctx.read_batch(batch).unwrap();

        let (table, _metrics) = DeltaOps(table)
            .merge(source, "target.id = source.id and target.cost is null")
            .with_source_alias("source")
            .with_target_alias("target")
            .when_matched_update(|insert| {
                insert
                    .update("id", "target.id")
                    .update("cost", "source.cost")
                    .update("month", "target.month")
            })
            .unwrap()
            .await
            .unwrap();

        let expected = vec![
            "+----+-------+------------+",
            "| id | cost  | month      |",
            "+----+-------+------------+",
            "| A  | 10.15 | 2023-07-04 |",
            "| B  | 11.15 | 2023-07-04 |",
            "+----+-------+------------+",
        ];
        let actual = get_data(&table).await;
        assert_batches_sorted_eq!(&expected, &actual);
    }

    #[tokio::test]
    async fn test_merge_cdc_disabled() {
        let (table, source) = setup().await;

        let (table, metrics) = DeltaOps(table)
            .merge(source, col("target.id").eq(col("source.id")))
            .with_source_alias("source")
            .with_target_alias("target")
            .when_matched_update(|update| {
                update
                    .update("value", col("source.value"))
                    .update("modified", col("source.modified"))
            })
            .unwrap()
            .when_not_matched_by_source_update(|update| {
                update
                    .predicate(col("target.value").eq(lit(1)))
                    .update("value", col("target.value") + lit(1))
            })
            .unwrap()
            .when_not_matched_insert(|insert| {
                insert
                    .set("id", col("source.id"))
                    .set("value", col("source.value"))
                    .set("modified", col("source.modified"))
            })
            .unwrap()
            .await
            .unwrap();

        assert_merge(table.clone(), metrics).await;

        // Just checking that the data wasn't actually written instead!
        if let Ok(files) = crate::logstore::tests::flatten_list_stream(
            &table.object_store(),
            Some(&object_store::path::Path::from("_change_data")),
        )
        .await
        {
            assert_eq!(
                0,
                files.len(),
                "This test should not find any written CDC files! {files:#?}"
            );
        }
    }

    #[tokio::test]
    async fn test_merge_cdc_enabled_simple() {
        // Manually creating the desired table with the right minimum CDC features
        use crate::kernel::Protocol;
        use crate::operations::merge::Action;

        let schema = get_delta_schema();

        let actions = vec![Action::Protocol(Protocol::new(1, 4))];
        let table: DeltaTable = DeltaOps::new_in_memory()
            .create()
            .with_columns(schema.fields().cloned())
            .with_actions(actions)
            .with_configuration_property(TableProperty::EnableChangeDataFeed, Some("true"))
            .await
            .unwrap();
        assert_eq!(table.version(), Some(0));

        let schema = get_arrow_schema(&None);
        let table = write_data(table, &schema).await;

        assert_eq!(table.version(), Some(1));
        assert_eq!(table.get_files_count(), 1);
        let source = merge_source(schema);

        let (table, metrics) = DeltaOps(table)
            .merge(source, col("target.id").eq(col("source.id")))
            .with_source_alias("source")
            .with_target_alias("target")
            .when_matched_update(|update| {
                update
                    .update("value", col("source.value"))
                    .update("modified", col("source.modified"))
            })
            .unwrap()
            .when_not_matched_by_source_update(|update| {
                update
                    .predicate(col("target.value").eq(lit(1)))
                    .update("value", col("target.value") + lit(1))
            })
            .unwrap()
            .when_not_matched_insert(|insert| {
                insert
                    .set("id", col("source.id"))
                    .set("value", col("source.value"))
                    .set("modified", col("source.modified"))
            })
            .unwrap()
            .await
            .unwrap();

        assert_merge(table.clone(), metrics).await;

        let ctx = SessionContext::new();
        let table = DeltaOps(table)
            .load_cdf()
            .with_starting_version(0)
            .build(&ctx.state(), None)
            .await
            .expect("Failed to load CDF");

        let mut batches = collect_batches(
            table.properties().output_partitioning().partition_count(),
            table,
            ctx,
        )
        .await
        .expect("Failed to collect batches");

        let _ = arrow::util::pretty::print_batches(&batches);

        // The batches will contain a current _commit_timestamp which shouldn't be check_append_only
        let _: Vec<_> = batches.iter_mut().map(|b| b.remove_column(5)).collect();

        assert_batches_sorted_eq! {[
        "+----+-------+------------+------------------+-----------------+",
        "| id | value | modified   | _change_type     | _commit_version |",
        "+----+-------+------------+------------------+-----------------+",
        "| A  | 1     | 2021-02-01 | update_preimage  | 2               |",
        "| A  | 2     | 2021-02-01 | update_postimage | 2               |",
        "| B  | 10    | 2021-02-01 | update_preimage  | 2               |",
        "| B  | 10    | 2021-02-02 | update_postimage | 2               |",
        "| C  | 10    | 2021-02-02 | update_preimage  | 2               |",
        "| C  | 20    | 2023-07-04 | update_postimage | 2               |",
        "| X  | 30    | 2023-07-04 | insert           | 2               |",
        "| A  | 1     | 2021-02-01 | insert           | 1               |",
        "| B  | 10    | 2021-02-01 | insert           | 1               |",
        "| C  | 10    | 2021-02-02 | insert           | 1               |",
        "| D  | 100   | 2021-02-02 | insert           | 1               |",
        "+----+-------+------------+------------------+-----------------+",
        ], &batches }
    }

    #[tokio::test]
    async fn test_merge_cdc_enabled_simple_with_schema_merge() {
        // Manually creating the desired table with the right minimum CDC features
        use crate::kernel::Protocol;
        use crate::operations::merge::Action;

        let schema = get_delta_schema();

        let actions = vec![Action::Protocol(Protocol::new(1, 4))];
        let table: DeltaTable = DeltaOps::new_in_memory()
            .create()
            .with_columns(schema.fields().cloned())
            .with_actions(actions)
            .with_configuration_property(TableProperty::EnableChangeDataFeed, Some("true"))
            .await
            .unwrap();
        assert_eq!(table.version(), Some(0));

        let schema = get_arrow_schema(&None);

        let source_schema = Arc::new(ArrowSchema::new(vec![
            Field::new("id", ArrowDataType::Utf8, true),
            Field::new("value", ArrowDataType::Int32, true),
            Field::new("modified", ArrowDataType::Utf8, true),
            Field::new("inserted_by", ArrowDataType::Utf8, true),
        ]));
        let table = write_data(table, &schema).await;

        assert_eq!(table.version(), Some(1));
        assert_eq!(table.get_files_count(), 1);
        let source = merge_source(schema);
        let source = source.with_column("inserted_by", lit("new_value")).unwrap();

        let (table, _) = DeltaOps(table)
            .merge(source.clone(), col("target.id").eq(col("source.id")))
            .with_source_alias("source")
            .with_target_alias("target")
            .with_merge_schema(true)
            .when_matched_update(|update| {
                update
                    .update("value", col("source.value"))
                    .update("modified", col("source.modified"))
            })
            .unwrap()
            .when_not_matched_by_source_update(|update| {
                update
                    .predicate(col("target.value").eq(lit(1)))
                    .update("value", col("target.value") + lit(1))
            })
            .unwrap()
            .when_not_matched_insert(|insert| {
                insert
                    .set("id", col("source.id"))
                    .set("value", col("source.value"))
                    .set("modified", col("source.modified"))
                    .set("inserted_by", col("source.inserted_by"))
            })
            .unwrap()
            .await
            .unwrap();

        let expected = vec![
            "+----+-------+------------+-------------+",
            "| id | value | modified   | inserted_by |",
            "+----+-------+------------+-------------+",
            "| A  | 2     | 2021-02-01 |             |",
            "| B  | 10    | 2021-02-02 | new_value   |",
            "| C  | 20    | 2023-07-04 | new_value   |",
            "| D  | 100   | 2021-02-02 |             |",
            "| X  | 30    | 2023-07-04 | new_value   |",
            "+----+-------+------------+-------------+",
        ];
        let actual = get_data(&table).await;
        let expected_schema_struct: StructType = source_schema.try_into_kernel().unwrap();
        assert_eq!(&expected_schema_struct, table.schema().unwrap());
        assert_batches_sorted_eq!(&expected, &actual);

        let ctx = SessionContext::new();
        let table = DeltaOps(table)
            .load_cdf()
            .with_starting_version(0)
            .build(&ctx.state(), None)
            .await
            .expect("Failed to load CDF");

        let mut batches = collect_batches(
            table.properties().output_partitioning().partition_count(),
            table,
            ctx,
        )
        .await
        .expect("Failed to collect batches");

        let _ = arrow::util::pretty::print_batches(&batches);

        // The batches will contain a current _commit_timestamp which shouldn't be check_append_only
        let _: Vec<_> = batches.iter_mut().map(|b| b.remove_column(6)).collect();

        assert_batches_sorted_eq! {[
        "+----+-------+------------+-------------+------------------+-----------------+",
        "| id | value | modified   | inserted_by | _change_type     | _commit_version |",
        "+----+-------+------------+-------------+------------------+-----------------+",
        "| A  | 1     | 2021-02-01 |             | insert           | 1               |",
        "| A  | 1     | 2021-02-01 |             | update_preimage  | 2               |",
        "| A  | 2     | 2021-02-01 |             | update_postimage | 2               |",
        "| B  | 10    | 2021-02-01 |             | insert           | 1               |",
        "| B  | 10    | 2021-02-01 |             | update_preimage  | 2               |",
        "| B  | 10    | 2021-02-02 | new_value   | update_postimage | 2               |",
        "| C  | 10    | 2021-02-02 |             | insert           | 1               |",
        "| C  | 10    | 2021-02-02 |             | update_preimage  | 2               |",
        "| C  | 20    | 2023-07-04 | new_value   | update_postimage | 2               |",
        "| D  | 100   | 2021-02-02 |             | insert           | 1               |",
        "| X  | 30    | 2023-07-04 | new_value   | insert           | 2               |",
        "+----+-------+------------+-------------+------------------+-----------------+",
            ], &batches }
    }

    #[tokio::test]
    async fn test_merge_cdc_enabled_delete() {
        // Manually creating the desired table with the right minimum CDC features
        use crate::kernel::Protocol;
        use crate::operations::merge::Action;

        let schema = get_delta_schema();

        let actions = vec![Action::Protocol(Protocol::new(1, 4))];
        let table: DeltaTable = DeltaOps::new_in_memory()
            .create()
            .with_columns(schema.fields().cloned())
            .with_actions(actions)
            .with_configuration_property(TableProperty::EnableChangeDataFeed, Some("true"))
            .await
            .unwrap();
        assert_eq!(table.version(), Some(0));

        let schema = get_arrow_schema(&None);
        let table = write_data(table, &schema).await;

        assert_eq!(table.version(), Some(1));
        assert_eq!(table.get_files_count(), 1);
        let source = merge_source(schema);

        let (table, _metrics) = DeltaOps(table)
            .merge(source, col("target.id").eq(col("source.id")))
            .with_source_alias("source")
            .with_target_alias("target")
            .when_not_matched_by_source_delete(|delete| {
                delete.predicate(col("target.modified").gt(lit("2021-02-01")))
            })
            .unwrap()
            .await
            .unwrap();

        let expected = vec![
            "+----+-------+------------+",
            "| id | value | modified   |",
            "+----+-------+------------+",
            "| A  | 1     | 2021-02-01 |",
            "| B  | 10    | 2021-02-01 |",
            "| C  | 10    | 2021-02-02 |",
            "+----+-------+------------+",
        ];
        let actual = get_data(&table).await;
        assert_batches_sorted_eq!(&expected, &actual);

        let ctx = SessionContext::new();
        let table = DeltaOps(table)
            .load_cdf()
            .with_starting_version(0)
            .build(&ctx.state(), None)
            .await
            .expect("Failed to load CDF");

        let mut batches = collect_batches(
            table.properties().output_partitioning().partition_count(),
            table,
            ctx,
        )
        .await
        .expect("Failed to collect batches");

        let _ = arrow::util::pretty::print_batches(&batches);

        // The batches will contain a current _commit_timestamp which shouldn't be check_append_only
        let _: Vec<_> = batches.iter_mut().map(|b| b.remove_column(5)).collect();

        assert_batches_sorted_eq! {[
        "+----+-------+------------+--------------+-----------------+",
        "| id | value | modified   | _change_type | _commit_version |",
        "+----+-------+------------+--------------+-----------------+",
        "| D  | 100   | 2021-02-02 | delete       | 2               |",
        "| A  | 1     | 2021-02-01 | insert       | 1               |",
        "| B  | 10    | 2021-02-01 | insert       | 1               |",
        "| C  | 10    | 2021-02-02 | insert       | 1               |",
        "| D  | 100   | 2021-02-02 | insert       | 1               |",
        "+----+-------+------------+--------------+-----------------+",
        ], &batches }
    }
}
