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
use std::collections::HashSet;
use std::sync::Arc;

use common_catalog::catalog_kind::CATALOG_DEFAULT;
use common_catalog::plan::PushDownInfo;
use common_catalog::table::Table;
use common_catalog::table_context::TableContext;
use common_exception::ErrorCode;
use common_exception::Result;
use common_expression::infer_table_schema;
use common_expression::types::StringType;
use common_expression::utils::FromData;
use common_expression::DataBlock;
use common_expression::Scalar;
use common_expression::TableDataType;
use common_expression::TableField;
use common_expression::TableSchemaRefExt;
use common_functions::BUILTIN_FUNCTIONS;
use common_meta_app::principal::GrantObject;
use common_meta_app::principal::UserGrantSet;
use common_meta_app::schema::TableIdent;
use common_meta_app::schema::TableInfo;
use common_meta_app::schema::TableMeta;
use common_sql::Planner;
use common_storages_view::view_table::QUERY;
use common_storages_view::view_table::VIEW_ENGINE;
use common_users::RoleCacheManager;

use crate::table::AsyncOneBlockSystemTable;
use crate::table::AsyncSystemTable;
use crate::util::find_eq_filter;

pub struct ColumnsTable {
    table_info: TableInfo,
}

#[async_trait::async_trait]
impl AsyncSystemTable for ColumnsTable {
    const NAME: &'static str = "system.columns";

    fn get_table_info(&self) -> &TableInfo {
        &self.table_info
    }

    #[async_backtrace::framed]
    async fn get_full_data(
        &self,
        ctx: Arc<dyn TableContext>,
        push_downs: Option<PushDownInfo>,
    ) -> Result<DataBlock> {
        let rows = self.dump_table_columns(ctx, push_downs).await?;
        let mut names: Vec<Vec<u8>> = Vec::with_capacity(rows.len());
        let mut tables: Vec<Vec<u8>> = Vec::with_capacity(rows.len());
        let mut databases: Vec<Vec<u8>> = Vec::with_capacity(rows.len());
        let mut types: Vec<Vec<u8>> = Vec::with_capacity(rows.len());
        let mut data_types: Vec<Vec<u8>> = Vec::with_capacity(rows.len());
        let mut default_kinds: Vec<Vec<u8>> = Vec::with_capacity(rows.len());
        let mut default_exprs: Vec<Vec<u8>> = Vec::with_capacity(rows.len());
        let mut is_nullables: Vec<Vec<u8>> = Vec::with_capacity(rows.len());
        let mut comments: Vec<Vec<u8>> = Vec::with_capacity(rows.len());
        for (database_name, table_name, field) in rows.into_iter() {
            names.push(field.name().clone().into_bytes());
            tables.push(table_name.into_bytes());
            databases.push(database_name.into_bytes());
            types.push(field.data_type().wrapped_display().into_bytes());
            let data_type = field.data_type().remove_recursive_nullable().sql_name();
            data_types.push(data_type.into_bytes());

            let mut default_kind = "".to_string();
            let mut default_expr = "".to_string();
            if let Some(expr) = field.default_expr() {
                default_kind = "DEFAULT".to_string();
                default_expr = expr.to_string();
            }
            default_kinds.push(default_kind.into_bytes());
            default_exprs.push(default_expr.into_bytes());
            if field.is_nullable() {
                is_nullables.push("YES".to_string().into_bytes());
            } else {
                is_nullables.push("NO".to_string().into_bytes());
            }

            comments.push("".to_string().into_bytes());
        }

        Ok(DataBlock::new_from_columns(vec![
            StringType::from_data(names),
            StringType::from_data(databases),
            StringType::from_data(tables),
            StringType::from_data(types),
            StringType::from_data(data_types),
            StringType::from_data(default_kinds),
            StringType::from_data(default_exprs),
            StringType::from_data(is_nullables),
            StringType::from_data(comments),
        ]))
    }
}

impl ColumnsTable {
    pub fn create(table_id: u64) -> Arc<dyn Table> {
        let schema = TableSchemaRefExt::create(vec![
            TableField::new("name", TableDataType::String),
            TableField::new("database", TableDataType::String),
            TableField::new("table", TableDataType::String),
            // inner wrapped display style
            TableField::new("type", TableDataType::String),
            // mysql display style for 3rd party tools
            TableField::new("data_type", TableDataType::String),
            TableField::new("default_kind", TableDataType::String),
            TableField::new("default_expression", TableDataType::String),
            TableField::new("is_nullable", TableDataType::String),
            TableField::new("comment", TableDataType::String),
        ]);

        let table_info = TableInfo {
            desc: "'system'.'columns'".to_string(),
            name: "columns".to_string(),
            ident: TableIdent::new(table_id, 0),
            meta: TableMeta {
                schema,
                engine: "SystemColumns".to_string(),
                ..Default::default()
            },
            ..Default::default()
        };

        AsyncOneBlockSystemTable::create(ColumnsTable { table_info })
    }

    #[async_backtrace::framed]
    async fn dump_table_columns(
        &self,
        ctx: Arc<dyn TableContext>,
        push_downs: Option<PushDownInfo>,
    ) -> Result<Vec<(String, String, TableField)>> {
        let tenant = ctx.get_tenant();
        let catalog = ctx.get_catalog(CATALOG_DEFAULT)?;

        let mut tables = Vec::new();
        let mut databases = Vec::new();
        if let Some(push_downs) = push_downs {
            if let Some(filter) = push_downs.filter {
                let expr = filter.as_expr(&BUILTIN_FUNCTIONS);
                find_eq_filter(&expr, &mut |col_name, scalar| {
                    if col_name == "database" {
                        if let Scalar::String(s) = scalar {
                            if let Ok(database) = String::from_utf8(s.clone()) {
                                databases.push(database);
                            }
                        }
                    } else if col_name == "table" {
                        if let Scalar::String(s) = scalar {
                            if let Ok(table) = String::from_utf8(s.clone()) {
                                tables.push(table);
                            }
                        }
                    }
                });
            }
        }

        if databases.is_empty() {
            let all_databases = catalog.list_databases(tenant.as_str()).await?;
            for db in all_databases {
                databases.push(db.name().to_string());
            }
        }

        let tenant = ctx.get_tenant();
        let user = ctx.get_current_user()?;
        let grant_set = user.grants;

        let (unique_object, global_object_priv) =
            generate_unique_object(&tenant, grant_set).await?;

        let mut access_dbs = HashMap::new();
        let mut final_dbs = vec![];
        let mut access_tables: HashSet<(String, String)> = HashSet::new();
        if !global_object_priv {
            for object in unique_object {
                match object {
                    GrantObject::Database(catalog, db) => {
                        if catalog == CATALOG_DEFAULT && databases.contains(&db) {
                            access_dbs.insert(db.clone(), false);
                        }
                    }
                    GrantObject::Table(catalog, db, table) => {
                        if catalog == CATALOG_DEFAULT && databases.contains(&db) {
                            access_tables.insert((db.clone(), table));
                            if !access_dbs.contains_key(&db) {
                                access_dbs.insert(db.clone(), true);
                            }
                        }
                    }
                    _ => {}
                }
            }
            for db in &databases {
                if access_dbs.contains_key(db) {
                    final_dbs.push(db.to_string());
                }
            }
        } else {
            final_dbs = databases;
        }

        let mut rows: Vec<(String, String, TableField)> = vec![];
        for database in final_dbs {
            let tables = if tables.is_empty() {
                if let Ok(table) = catalog.list_tables(tenant.as_str(), &database).await {
                    table
                } else {
                    vec![]
                }
            } else {
                let mut res = Vec::new();
                for table in &tables {
                    if let Ok(table) = catalog.get_table(tenant.as_str(), &database, table).await {
                        res.push(table);
                    }
                }
                res
            };

            for table in tables {
                if global_object_priv {
                    let fields = generate_fields(&ctx, &table).await?;
                    for field in fields {
                        rows.push((database.clone(), table.name().into(), field.clone()))
                    }
                } else if let Some(contain_table_priv) = access_dbs.get(&database) {
                    if *contain_table_priv {
                        if access_tables.contains(&(database.to_string(), table.name().to_string()))
                        {
                            let fields = generate_fields(&ctx, &table).await?;
                            for field in fields {
                                rows.push((database.clone(), table.name().into(), field.clone()))
                            }
                        }
                    } else {
                        let fields = generate_fields(&ctx, &table).await?;
                        for field in fields {
                            rows.push((database.clone(), table.name().into(), field.clone()))
                        }
                    }
                }
            }
        }

        Ok(rows)
    }
}

async fn generate_fields(
    ctx: &Arc<dyn TableContext>,
    table: &Arc<dyn Table>,
) -> Result<Vec<TableField>> {
    let fields = if table.engine() == VIEW_ENGINE {
        if let Some(query) = table.options().get(QUERY) {
            let mut planner = Planner::new(ctx.clone());
            let (plan, _) = planner.plan_sql(query).await?;
            let schema = infer_table_schema(&plan.schema())?;
            schema.fields().clone()
        } else {
            return Err(ErrorCode::Internal(
                "Logical error, View Table must have a SelectQuery inside.",
            ));
        }
    } else {
        table.schema().fields().clone()
    };
    Ok(fields)
}

pub(crate) async fn generate_unique_object(
    tenant: &str,
    grant_set: UserGrantSet,
) -> Result<(HashSet<GrantObject>, bool)> {
    let mut unique_object: HashSet<GrantObject> = HashSet::new();
    let mut global_object_priv = false;
    let _objects = RoleCacheManager::instance()
        .find_related_roles(tenant, &grant_set.roles())
        .await?
        .into_iter()
        .map(|role| role.grants)
        .fold(grant_set, |a, b| a | b)
        .entries()
        .iter()
        .map(|e| {
            let object = e.object();
            match object {
                GrantObject::Global => {
                    global_object_priv = true;
                }
                GrantObject::Database(catalog, ldb) => {
                    unique_object
                        .insert(GrantObject::Database(catalog.to_string(), ldb.to_string()));
                }
                GrantObject::Table(catalog, ldb, ltab) => {
                    unique_object.insert(GrantObject::Table(
                        catalog.to_string(),
                        ldb.to_string(),
                        ltab.to_string(),
                    ));
                }
            }
        })
        .collect::<Vec<_>>();

    Ok((unique_object, global_object_priv))
}
