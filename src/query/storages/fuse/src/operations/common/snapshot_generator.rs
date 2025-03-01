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

use std::collections::HashMap;
use std::ops::Range;
use std::sync::Arc;

use common_catalog::table_context::TableContext;
use common_exception::ErrorCode;
use common_exception::Result;
use common_expression::ColumnId;
use common_expression::Scalar;
use common_expression::TableSchema;
use common_expression::TableSchemaRef;
use common_sql::field_default_value;
use log::info;
use storages_common_table_meta::meta::ClusterKey;
use storages_common_table_meta::meta::ColumnStatistics;
use storages_common_table_meta::meta::Location;
use storages_common_table_meta::meta::Statistics;
use storages_common_table_meta::meta::TableSnapshot;
use uuid::Uuid;

use crate::metrics::metrics_inc_commit_mutation_latest_snapshot_append_only;
use crate::metrics::metrics_inc_commit_mutation_modified_segment_exists_in_latest;
use crate::metrics::metrics_inc_commit_mutation_unresolvable_conflict;
use crate::statistics::merge_statistics;
use crate::statistics::reducers::deduct_statistics;
use crate::statistics::reducers::deduct_statistics_mut;
use crate::statistics::reducers::merge_statistics_mut;

#[async_trait::async_trait]
pub trait SnapshotGenerator {
    fn set_conflict_resolve_context(&mut self, ctx: ConflictResolveContext);

    async fn fill_default_values(
        &mut self,
        _schema: TableSchema,
        _snapshot: &Option<Arc<TableSnapshot>>,
    ) -> Result<()> {
        Ok(())
    }

    fn generate_new_snapshot(
        &self,
        schema: TableSchema,
        cluster_key_meta: Option<ClusterKey>,
        previous: Option<Arc<TableSnapshot>>,
    ) -> Result<TableSnapshot>;
}

#[derive(Clone, serde::Serialize, serde::Deserialize, Debug, PartialEq)]
pub struct SnapshotChanges {
    pub removed_segment_indexes: Vec<usize>,
    pub added_segments: Vec<Option<Location>>,
    pub removed_statistics: Statistics,
    pub added_statistics: Statistics,
}

#[derive(Clone, serde::Serialize, serde::Deserialize, Debug, PartialEq)]
pub struct SnapshotMerged {
    pub merged_segments: Vec<Location>,
    pub merged_statistics: Statistics,
}

#[derive(Clone, serde::Serialize, serde::Deserialize, Debug, PartialEq)]
pub enum ConflictResolveContext {
    AppendOnly((SnapshotMerged, TableSchemaRef)),
    LatestSnapshotAppendOnly(SnapshotMerged),
    ModifiedSegmentExistsInLatest(SnapshotChanges),
}

impl ConflictResolveContext {
    pub fn is_latest_snapshot_append_only(
        base: &TableSnapshot,
        latest: &TableSnapshot,
    ) -> Option<Range<usize>> {
        let base_segments = &base.segments;
        let latest_segments = &latest.segments;

        let base_segments_len = base_segments.len();
        let latest_segments_len = latest_segments.len();

        if latest_segments_len >= base_segments_len
            && base_segments[0..base_segments_len]
                == latest_segments[(latest_segments_len - base_segments_len)..latest_segments_len]
        {
            Some(0..(latest_segments_len - base_segments_len))
        } else {
            None
        }
    }

    pub fn is_modified_segments_exists_in_latest(
        base: &TableSnapshot,
        latest: &TableSnapshot,
        removed_segments: &[usize],
    ) -> Option<Vec<usize>> {
        let mut positions = Vec::with_capacity(removed_segments.len());
        let latest_segments = latest
            .segments
            .iter()
            .enumerate()
            .map(|(i, x)| (x, i))
            .collect::<HashMap<_, usize>>();
        for removed_segment in removed_segments {
            let removed_segment = &base.segments[*removed_segment];
            if let Some(position) = latest_segments.get(removed_segment) {
                positions.push(*position);
            } else {
                return None;
            }
        }
        Some(positions)
    }

    pub fn merge_segments(
        mut base_segments: Vec<Location>,
        added_segments: Vec<Option<Location>>,
        removed_segment_indexes: Vec<usize>,
    ) -> Vec<Location> {
        let mut blanks = vec![];
        for (position, added_segment) in removed_segment_indexes
            .into_iter()
            .zip(added_segments.into_iter())
        {
            if let Some(added) = added_segment {
                base_segments[position] = added;
            } else {
                blanks.push(position);
            }
        }
        blanks.sort_unstable();
        let mut merged_segments = Vec::with_capacity(base_segments.len() - blanks.len());
        if !blanks.is_empty() {
            let mut last = 0;
            for blank in blanks {
                merged_segments.extend_from_slice(&base_segments[last..blank]);
                last = blank + 1;
            }
            merged_segments.extend_from_slice(&base_segments[last..]);
        } else {
            merged_segments = base_segments;
        }
        merged_segments
    }
}

#[derive(Clone)]
pub struct MutationGenerator {
    base_snapshot: Arc<TableSnapshot>,
    conflict_resolve_ctx: Option<ConflictResolveContext>,
}

impl MutationGenerator {
    pub fn new(base_snapshot: Arc<TableSnapshot>) -> Self {
        MutationGenerator {
            base_snapshot,
            conflict_resolve_ctx: None,
        }
    }
}

impl SnapshotGenerator for MutationGenerator {
    fn generate_new_snapshot(
        &self,
        schema: TableSchema,
        cluster_key_meta: Option<ClusterKey>,
        previous: Option<Arc<TableSnapshot>>,
    ) -> Result<TableSnapshot> {
        let default_cluster_key_id = cluster_key_meta.clone().map(|v| v.0);

        let previous =
            previous.unwrap_or_else(|| Arc::new(TableSnapshot::new_empty_snapshot(schema.clone())));
        let ctx = self
            .conflict_resolve_ctx
            .as_ref()
            .ok_or(ErrorCode::Internal("conflict_solve_ctx not set"))?;
        match ctx {
            ConflictResolveContext::AppendOnly(_) => {
                return Err(ErrorCode::Internal(
                    "conflict_resolve_ctx should not be AppendOnly in MutationGenerator",
                ));
            }
            ConflictResolveContext::LatestSnapshotAppendOnly(ctx) => {
                if let Some(range_of_newly_append) =
                    ConflictResolveContext::is_latest_snapshot_append_only(
                        &self.base_snapshot,
                        &previous,
                    )
                {
                    info!("resolvable conflicts detected");
                    metrics_inc_commit_mutation_latest_snapshot_append_only();
                    let append_segments = &previous.segments[range_of_newly_append];
                    let append_statistics =
                        deduct_statistics(&previous.summary, &self.base_snapshot.summary);

                    let new_segments = append_segments
                        .iter()
                        .chain(ctx.merged_segments.iter())
                        .cloned()
                        .collect::<Vec<_>>();
                    let new_summary = merge_statistics(
                        &ctx.merged_statistics,
                        &append_statistics,
                        default_cluster_key_id,
                    );
                    let new_snapshot = TableSnapshot::new(
                        Uuid::new_v4(),
                        &previous.timestamp,
                        Some((previous.snapshot_id, previous.format_version)),
                        schema,
                        new_summary,
                        new_segments,
                        cluster_key_meta,
                        previous.table_statistics_location.clone(),
                    );
                    return Ok(new_snapshot);
                }
            }
            ConflictResolveContext::ModifiedSegmentExistsInLatest(ctx) => {
                if let Some(positions) =
                    ConflictResolveContext::is_modified_segments_exists_in_latest(
                        &self.base_snapshot,
                        &previous,
                        &ctx.removed_segment_indexes,
                    )
                {
                    info!("resolvable conflicts detected");
                    metrics_inc_commit_mutation_modified_segment_exists_in_latest();
                    let new_segments = ConflictResolveContext::merge_segments(
                        previous.segments.clone(),
                        ctx.added_segments.clone(),
                        positions,
                    );
                    let mut new_summary = merge_statistics(
                        &ctx.added_statistics,
                        &previous.summary,
                        default_cluster_key_id,
                    );
                    deduct_statistics_mut(&mut new_summary, &ctx.removed_statistics);
                    let new_snapshot = TableSnapshot::new(
                        Uuid::new_v4(),
                        &previous.timestamp,
                        Some((previous.snapshot_id, previous.format_version)),
                        schema,
                        new_summary,
                        new_segments,
                        cluster_key_meta,
                        previous.table_statistics_location.clone(),
                    );
                    return Ok(new_snapshot);
                }
            }
        }
        metrics_inc_commit_mutation_unresolvable_conflict();
        Err(ErrorCode::UnresolvableConflict(format!(
            "conflict resolve context:{:?}",
            ctx
        )))
    }

    fn set_conflict_resolve_context(&mut self, ctx: ConflictResolveContext) {
        self.conflict_resolve_ctx = Some(ctx);
    }
}

#[derive(Clone)]
pub struct AppendGenerator {
    ctx: Arc<dyn TableContext>,
    leaf_default_values: HashMap<ColumnId, Scalar>,
    overwrite: bool,
    conflict_resolve_ctx: Option<ConflictResolveContext>,
}

impl AppendGenerator {
    pub fn new(ctx: Arc<dyn TableContext>, overwrite: bool) -> Self {
        AppendGenerator {
            ctx,
            leaf_default_values: HashMap::new(),
            overwrite,
            conflict_resolve_ctx: None,
        }
    }

    fn check_fill_default(&self, summary: &Statistics) -> Result<bool> {
        let mut fill_default_values = false;
        // check if need to fill default value in statistics
        for column_id in self
            .conflict_resolve_ctx()?
            .0
            .merged_statistics
            .col_stats
            .keys()
        {
            if !summary.col_stats.contains_key(column_id) {
                fill_default_values = true;
                break;
            }
        }
        Ok(fill_default_values)
    }
}

impl AppendGenerator {
    fn conflict_resolve_ctx(&self) -> Result<(&SnapshotMerged, &TableSchema)> {
        let ctx = self
            .conflict_resolve_ctx
            .as_ref()
            .ok_or(ErrorCode::Internal("conflict_solve_ctx not set"))?;
        match ctx {
            ConflictResolveContext::AppendOnly((ctx, schema)) => Ok((ctx, schema.as_ref())),
            _ => Err(ErrorCode::Internal(
                "conflict_resolve_ctx should only be Appendonly in AppendGenerator",
            )),
        }
    }
}

#[async_trait::async_trait]
impl SnapshotGenerator for AppendGenerator {
    fn set_conflict_resolve_context(&mut self, ctx: ConflictResolveContext) {
        self.conflict_resolve_ctx = Some(ctx);
    }

    async fn fill_default_values(
        &mut self,
        schema: TableSchema,
        previous: &Option<Arc<TableSnapshot>>,
    ) -> Result<()> {
        if let Some(snapshot) = previous {
            if !self.overwrite && self.check_fill_default(&snapshot.summary)? {
                let mut default_values = Vec::with_capacity(schema.num_fields());
                for field in schema.fields() {
                    default_values.push(field_default_value(self.ctx.clone(), field)?);
                }
                self.leaf_default_values = schema.field_leaf_default_values(&default_values);
            }
        }
        Ok(())
    }

    fn generate_new_snapshot(
        &self,
        schema: TableSchema,
        cluster_key_meta: Option<ClusterKey>,
        previous: Option<Arc<TableSnapshot>>,
    ) -> Result<TableSnapshot> {
        let (snapshot_merged, expected_schema) = self.conflict_resolve_ctx()?;
        if is_column_type_modified(&schema, expected_schema) {
            return Err(ErrorCode::UnresolvableConflict(format!(
                "schema was changed during insert, expected:{:?}, actual:{:?}",
                expected_schema, schema
            )));
        }
        let mut prev_timestamp = None;
        let mut prev_snapshot_id = None;
        let mut table_statistics_location = None;
        let mut new_segments = snapshot_merged.merged_segments.clone();
        let mut new_summary = snapshot_merged.merged_statistics.clone();

        if let Some(snapshot) = &previous {
            prev_timestamp = snapshot.timestamp;
            prev_snapshot_id = Some((snapshot.snapshot_id, snapshot.format_version));
            table_statistics_location = snapshot.table_statistics_location.clone();

            if !self.overwrite {
                let mut summary = snapshot.summary.clone();
                if self.check_fill_default(&summary)? {
                    self.leaf_default_values
                        .iter()
                        .for_each(|(col_id, default_value)| {
                            if !summary.col_stats.contains_key(col_id) {
                                let (null_count, distinct_of_values) = if default_value.is_null() {
                                    (summary.row_count, Some(0))
                                } else {
                                    (0, Some(1))
                                };
                                let col_stat = ColumnStatistics {
                                    min: default_value.to_owned(),
                                    max: default_value.to_owned(),
                                    null_count,
                                    in_memory_size: 0,
                                    distinct_of_values,
                                };
                                summary.col_stats.insert(*col_id, col_stat);
                            }
                        });
                }

                new_segments = snapshot_merged
                    .merged_segments
                    .iter()
                    .chain(snapshot.segments.iter())
                    .cloned()
                    .collect();

                merge_statistics_mut(
                    &mut new_summary,
                    &summary,
                    cluster_key_meta.clone().map(|v| v.0),
                );
            }
        }

        Ok(TableSnapshot::new(
            Uuid::new_v4(),
            &prev_timestamp,
            prev_snapshot_id,
            schema,
            new_summary,
            new_segments,
            cluster_key_meta,
            table_statistics_location,
        ))
    }
}

fn is_column_type_modified(schema: &TableSchema, expected_schema: &TableSchema) -> bool {
    let expected: HashMap<_, _> = expected_schema
        .fields()
        .iter()
        .map(|f| (f.column_id, &f.data_type))
        .collect();
    schema.fields().iter().any(|f| {
        expected
            .get(&f.column_id)
            .is_some_and(|ty| **ty != f.data_type)
    })
}
