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

use std::any::Any;
use std::sync::Arc;

use arrow_schema::Schema as ArrowSchema;
use async_trait::async_trait;
use chrono::Utc;
use common_arrow::arrow::datatypes::Field as Arrow2Field;
use common_arrow::arrow::datatypes::Schema as Arrow2Schema;
use common_catalog::plan::DataSourcePlan;
use common_catalog::plan::PartInfo;
use common_catalog::plan::PartStatistics;
use common_catalog::plan::Partitions;
use common_catalog::plan::PartitionsShuffleKind;
use common_catalog::plan::PushDownInfo;
use common_catalog::table::Table;
use common_catalog::table_args::TableArgs;
use common_catalog::table_context::TableContext;
use common_exception::ErrorCode;
use common_exception::Result;
use common_expression::DataSchema;
use common_expression::DataSchemaRefExt;
use common_expression::TableSchema;
use common_expression::TableSchemaRef;
use common_meta_app::schema::TableIdent;
use common_meta_app::schema::TableInfo;
use common_meta_app::schema::TableMeta;
use common_pipeline_core::processors::port::OutputPort;
use common_pipeline_core::Pipeline;
use common_pipeline_core::SourcePipeBuilder;
use common_storage::DataOperator;

use crate::partition::IcebergPartInfo;
use crate::table_source::IcebergTableSource;

/// accessor wrapper as a table
///
/// TODO: we should use icelake Table instead.
pub struct IcebergTable {
    info: TableInfo,
    op: opendal::Operator,

    table: icelake::Table,
}

impl IcebergTable {
    /// create a new table on the table directory
    ///
    /// TODO: we should use icelake Table instead.
    #[async_backtrace::framed]
    pub async fn try_create(
        catalog: &str,
        database: &str,
        table_name: &str,
        tbl_root: DataOperator,
    ) -> Result<IcebergTable> {
        let op = tbl_root.operator();
        let table = icelake::Table::open_with_op(op.clone())
            .await
            .map_err(|e| ErrorCode::ReadTableDataError(format!("Cannot load metadata: {e:?}")))?;

        let meta = table.current_table_metadata();

        // Build arrow schema from iceberg metadata.
        let arrow_schema: ArrowSchema = meta
            .schemas
            .last()
            .ok_or_else(|| {
                ErrorCode::ReadTableDataError("Iceberg table schema is empty".to_string())
            })?
            .clone()
            .try_into()
            .map_err(|e| {
                ErrorCode::ReadTableDataError(format!("Cannot convert table metadata: {e:?}"))
            })?;

        // Build arrow2 schema from arrow schema.
        let fields: Vec<Arrow2Field> = arrow_schema
            .fields()
            .into_iter()
            .map(|f| f.into())
            .collect();
        let arrow2_schema = Arrow2Schema::from(fields);

        let table_schema = TableSchema::from(&arrow2_schema);

        // construct table info
        let info = TableInfo {
            ident: TableIdent::new(0, 0),
            desc: format!("{database}.{table_name}"),
            name: table_name.to_string(),
            meta: TableMeta {
                schema: Arc::new(table_schema),
                catalog: catalog.to_string(),
                engine: "iceberg".to_string(),
                created_on: Utc::now(),
                storage_params: Some(tbl_root.params()),
                ..Default::default()
            },
            ..Default::default()
        };

        Ok(Self { info, op, table })
    }

    pub fn do_read_data(
        &self,
        ctx: Arc<dyn TableContext>,
        plan: &DataSourcePlan,
        pipeline: &mut Pipeline,
    ) -> Result<()> {
        let parts_len = plan.parts.len();
        let max_threads = ctx.get_settings().get_max_threads()? as usize;
        let max_threads = std::cmp::min(parts_len, max_threads);

        let table_schema: TableSchemaRef = self.info.schema();
        let source_projection =
            PushDownInfo::projection_of_push_downs(&table_schema, &plan.push_downs);

        // The front of the src_fields are prewhere columns (if exist).
        // The back of the src_fields are remain columns.
        let mut src_fields = Vec::with_capacity(source_projection.len());

        // The schema of the data block `read_data` output.
        let output_schema: Arc<DataSchema> = Arc::new(plan.schema().into());

        // TODO: we need to support top_k.
        // TODO: we need to support prewhere.

        // Since we don't support prewhere, we can use the source_reader directly.
        src_fields.extend_from_slice(output_schema.fields());
        let src_schema = DataSchemaRefExt::create(src_fields);

        let mut source_builder = SourcePipeBuilder::create();
        for _ in 0..std::cmp::max(1, max_threads) {
            let output = OutputPort::create();
            source_builder.add_source(
                output.clone(),
                IcebergTableSource::create(
                    ctx.clone(),
                    self.op.clone(),
                    output,
                    src_schema.clone(),
                    output_schema.clone(),
                )?,
            );
        }

        pipeline.add_pipe(source_builder.finalize());
        Ok(())
    }

    #[minitrace::trace]
    #[async_backtrace::framed]
    async fn do_read_partitions(
        &self,
        _ctx: Arc<dyn TableContext>,
    ) -> Result<(PartStatistics, Partitions)> {
        let data_files = self.table.current_data_files().await.map_err(|e| {
            ErrorCode::ReadTableDataError(format!("Cannot get current data files: {e:?}"))
        })?;

        let partitions = data_files
            .into_iter()
            .map(|v: icelake::types::DataFile| match v.file_format {
                icelake::types::DataFileFormat::Parquet => Arc::new(Box::new(IcebergPartInfo {
                    path: self
                        .table
                        .rel_path(&v.file_path)
                        .expect("file path must be rel to table"),
                    size: v.file_size_in_bytes as u64,
                })
                    as Box<dyn PartInfo>),
                _ => {
                    unimplemented!("Only parquet format is supported for iceberg table")
                }
            })
            .collect();

        Ok((
            PartStatistics::default(),
            Partitions::create_nolazy(PartitionsShuffleKind::Seq, partitions),
        ))
    }
}

#[async_trait]
impl Table for IcebergTable {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn is_local(&self) -> bool {
        false
    }

    fn get_table_info(&self) -> &TableInfo {
        &self.info
    }

    fn name(&self) -> &str {
        &self.get_table_info().name
    }

    #[async_backtrace::framed]
    async fn read_partitions(
        &self,
        ctx: Arc<dyn TableContext>,
        // TODO: we will support push down later.
        _push_downs: Option<PushDownInfo>,
        // TODO: we will support dry run later.
        _dry_run: bool,
    ) -> Result<(PartStatistics, Partitions)> {
        self.do_read_partitions(ctx).await
    }

    fn read_data(
        &self,
        ctx: Arc<dyn TableContext>,
        plan: &DataSourcePlan,
        pipeline: &mut Pipeline,
    ) -> Result<()> {
        self.do_read_data(ctx, plan, pipeline)
    }

    fn table_args(&self) -> Option<TableArgs> {
        None
    }
}
