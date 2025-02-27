// Copyright Materialize, Inc. and contributors. All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

//! Data definition language (DDL).
//!
//! This module houses the handlers for statements that modify the catalog, like
//! `ALTER`, `CREATE`, and `DROP`.

use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{anyhow, bail};
use aws_arn::ARN;
use chrono::{NaiveDate, NaiveDateTime};
use globset::GlobBuilder;
use itertools::Itertools;
use regex::Regex;
use reqwest::Url;
use tracing::{debug, warn};

use mz_dataflow_types::{
    sinks::{
        AvroOcfSinkConnectorBuilder, KafkaSinkConnectorBuilder, KafkaSinkConnectorRetention,
        KafkaSinkFormat, SinkConnectorBuilder, SinkEnvelope,
    },
    sources::{
        encoding::{
            included_column_desc, AvroEncoding, AvroOcfEncoding, ColumnSpec, CsvEncoding,
            DataEncoding, ProtobufEncoding, RegexEncoding, SourceDataEncoding,
        },
        provide_default_metadata, DebeziumDedupProjection, DebeziumEnvelope, DebeziumMode,
        DebeziumSourceProjection, ExternalSourceConnector, FileSourceConnector, IncludedColumnPos,
        KafkaSourceConnector, KeyEnvelope, KinesisSourceConnector, PostgresSourceConnector,
        PubNubSourceConnector, S3SourceConnector, SourceConnector, SourceEnvelope, Timeline,
        UnplannedSourceEnvelope, UpsertStyle,
    },
};
use mz_expr::GlobalId;
use mz_interchange::avro::{self, AvroSchemaGenerator};
use mz_interchange::envelopes;
use mz_ore::collections::CollectionExt;
use mz_ore::str::StrExt;
use mz_repr::{strconv, ColumnName, RelationDesc, RelationType, ScalarType};
use mz_sql_parser::ast::{CsrSeedCompiledOrLegacy, SourceIncludeMetadata};

use crate::ast::display::AstDisplay;
use crate::ast::{
    AlterIndexAction, AlterIndexStatement, AlterObjectRenameStatement, AvroSchema, ColumnOption,
    Compression, CreateDatabaseStatement, CreateIndexStatement, CreateRoleOption,
    CreateRoleStatement, CreateSchemaStatement, CreateSinkConnector, CreateSinkStatement,
    CreateSourceConnector, CreateSourceFormat, CreateSourceStatement, CreateTableStatement,
    CreateTypeAs, CreateTypeStatement, CreateViewStatement, CreateViewsDefinitions,
    CreateViewsStatement, CsrConnectorAvro, CsrConnectorProto, CsrSeedCompiled, CsvColumns,
    DbzMode, DropDatabaseStatement, DropObjectsStatement, Envelope, Expr, Format, Ident,
    IfExistsBehavior, KafkaConsistency, KeyConstraint, ObjectType, ProtobufSchema, Raw,
    SourceIncludeMetadataType, SqlOption, Statement, TableConstraint, UnresolvedObjectName, Value,
    ViewDefinition, WithOption,
};
use crate::catalog::{CatalogItem, CatalogItemType, CatalogType, CatalogTypeDetails};
use crate::kafka_util;
use crate::names::{
    resolve_names_data_type, DatabaseSpecifier, FullName, ResolvedDataType, SchemaName,
};
use crate::normalize;
use crate::plan::error::PlanError;
use crate::plan::query::QueryLifetime;
use crate::plan::statement::{StatementContext, StatementDesc};
use crate::plan::{
    plan_utils, query, AlterIndexEnablePlan, AlterIndexResetOptionsPlan, AlterIndexSetOptionsPlan,
    AlterItemRenamePlan, AlterNoopPlan, CreateDatabasePlan, CreateIndexPlan, CreateRolePlan,
    CreateSchemaPlan, CreateSinkPlan, CreateSourcePlan, CreateTablePlan, CreateTypePlan,
    CreateViewPlan, CreateViewsPlan, DropDatabasePlan, DropItemsPlan, DropRolesPlan,
    DropSchemaPlan, HirRelationExpr, Index, IndexOption, IndexOptionName, Params, Plan, Sink,
    Source, Table, Type, View,
};
use crate::pure::Schema;

pub fn describe_create_database(
    _: &StatementContext,
    _: CreateDatabaseStatement,
) -> Result<StatementDesc, anyhow::Error> {
    Ok(StatementDesc::new(None))
}

pub fn plan_create_database(
    _: &StatementContext,
    CreateDatabaseStatement {
        name,
        if_not_exists,
    }: CreateDatabaseStatement,
) -> Result<Plan, anyhow::Error> {
    Ok(Plan::CreateDatabase(CreateDatabasePlan {
        name: normalize::ident(name),
        if_not_exists,
    }))
}

pub fn describe_create_schema(
    _: &StatementContext,
    _: CreateSchemaStatement,
) -> Result<StatementDesc, anyhow::Error> {
    Ok(StatementDesc::new(None))
}

pub fn plan_create_schema(
    scx: &StatementContext,
    CreateSchemaStatement {
        mut name,
        if_not_exists,
    }: CreateSchemaStatement,
) -> Result<Plan, anyhow::Error> {
    if name.0.len() > 2 {
        bail!("schema name {} has more than two components", name);
    }
    let schema_name = normalize::ident(
        name.0
            .pop()
            .expect("names always have at least one component"),
    );
    let database_name = match name.0.pop() {
        None => DatabaseSpecifier::Name(scx.catalog.default_database().into()),
        Some(n) => DatabaseSpecifier::Name(normalize::ident(n)),
    };
    Ok(Plan::CreateSchema(CreateSchemaPlan {
        database_name,
        schema_name,
        if_not_exists,
    }))
}

pub fn describe_create_table(
    _: &StatementContext,
    _: CreateTableStatement<Raw>,
) -> Result<StatementDesc, anyhow::Error> {
    Ok(StatementDesc::new(None))
}

pub fn plan_create_table(
    scx: &StatementContext,
    stmt: CreateTableStatement<Raw>,
) -> Result<Plan, anyhow::Error> {
    let CreateTableStatement {
        name,
        columns,
        constraints,
        with_options,
        if_not_exists,
        temporary,
    } = &stmt;

    if !with_options.is_empty() {
        bail_unsupported!("WITH options");
    }

    let names: Vec<_> = columns
        .iter()
        .map(|c| normalize::column_name(c.name.clone()))
        .collect();

    if let Some(dup) = names.iter().duplicates().next() {
        bail!("column {} specified more than once", dup.as_str().quoted());
    }

    // Build initial relation type that handles declared data types
    // and NOT NULL constraints.
    let mut column_types = Vec::with_capacity(columns.len());
    let mut defaults = Vec::with_capacity(columns.len());
    let mut depends_on = Vec::new();
    let mut keys = Vec::new();

    for (i, c) in columns.into_iter().enumerate() {
        let (aug_data_type, ids) = resolve_names_data_type(scx, c.data_type.clone())?;
        let ty = query::scalar_type_from_sql(scx, &aug_data_type)?;
        let mut nullable = true;
        let mut default = Expr::null();
        for option in &c.options {
            match &option.option {
                ColumnOption::NotNull => nullable = false,
                ColumnOption::Default(expr) => {
                    // Ensure expression can be planned and yields the correct
                    // type.
                    let (_, expr_depends_on) = query::plan_default_expr(scx, expr, &ty)?;
                    depends_on.extend(expr_depends_on);
                    default = expr.clone();
                }
                ColumnOption::Unique { is_primary } => {
                    keys.push(vec![i]);
                    if *is_primary {
                        nullable = false;
                    }
                }
                other => {
                    bail_unsupported!(format!("CREATE TABLE with column constraint: {}", other))
                }
            }
        }
        column_types.push(ty.nullable(nullable));
        defaults.push(default);
        depends_on.extend(ids);
    }

    for constraint in constraints {
        match constraint {
            TableConstraint::Unique {
                name: _,
                columns,
                is_primary,
            } => {
                let mut key = vec![];
                for column in columns {
                    let column = normalize::column_name(column.clone());
                    match names.iter().position(|name| *name == column) {
                        None => bail!("unknown column in constraint: {}", column),
                        Some(i) => {
                            key.push(i);
                            if *is_primary {
                                column_types[i].nullable = false;
                            }
                        }
                    }
                }
                keys.push(key);
            }
            TableConstraint::ForeignKey { .. } => {
                // Foreign key constraints are not presently enforced. We allow
                // them in experimental mode for sqllogictest's sake.
                scx.require_experimental_mode("CREATE TABLE with a foreign key")?
            }
            TableConstraint::Check { .. } => {
                // Check constraints are not presently enforced. We allow them
                // in experimental mode for sqllogictest's sake.
                scx.require_experimental_mode("CREATE TABLE with a check constraint")?
            }
        }
    }

    if !keys.is_empty() {
        // Unique constraints are not presently enforced. We allow them in
        // experimental mode for sqllogictest's sake.
        scx.require_experimental_mode("CREATE TABLE with a primary key or unique constraint")?;
    }

    let typ = RelationType::new(column_types).with_keys(keys);

    let temporary = *temporary;
    let name = if temporary {
        scx.allocate_temporary_name(normalize::unresolved_object_name(name.to_owned())?)
    } else {
        scx.allocate_name(normalize::unresolved_object_name(name.to_owned())?)
    };
    let desc = RelationDesc::new(typ, names);

    let create_sql = normalize::create_statement(&scx, Statement::CreateTable(stmt.clone()))?;
    let table = Table {
        create_sql,
        desc,
        defaults,
        temporary,
        depends_on,
    };
    Ok(Plan::CreateTable(CreateTablePlan {
        name,
        table,
        if_not_exists: *if_not_exists,
    }))
}

pub fn describe_create_source(
    _: &StatementContext,
    _: CreateSourceStatement<Raw>,
) -> Result<StatementDesc, anyhow::Error> {
    Ok(StatementDesc::new(None))
}

pub fn plan_create_source(
    scx: &StatementContext,
    stmt: CreateSourceStatement<Raw>,
) -> Result<Plan, anyhow::Error> {
    let CreateSourceStatement {
        name,
        col_names,
        connector,
        with_options,
        envelope,
        if_not_exists,
        materialized,
        format,
        key_constraint,
        include_metadata,
    } = &stmt;

    let with_options_original = with_options;
    let mut with_options = normalize::options(with_options);

    let ts_frequency = match with_options.remove("timestamp_frequency_ms") {
        Some(val) => match val {
            Value::Number(n) => match n.parse::<u64>() {
                Ok(n) => Duration::from_millis(n),
                Err(_) => bail!("timestamp_frequency_ms must be an u64"),
            },
            _ => bail!("timestamp_frequency_ms must be an u64"),
        },
        None => scx.catalog.config().timestamp_frequency,
    };
    if !matches!(connector, CreateSourceConnector::Kafka { .. }) && !include_metadata.is_empty() {
        bail_unsupported!("INCLUDE metadata with non-Kafka sources");
    }

    let (external_connector, encoding) = match connector {
        CreateSourceConnector::Kafka { broker, topic, .. } => {
            let config_options = kafka_util::extract_config(&mut with_options)?;

            let group_id_prefix = match with_options.remove("group_id_prefix") {
                None => None,
                Some(Value::String(s)) => Some(s),
                Some(_) => bail!("group_id_prefix must be a string"),
            };

            let parse_offset = |s: &str| match s.parse::<i64>() {
                Ok(n) if n >= 0 => Ok(n),
                _ => bail!("start_offset must be a nonnegative integer"),
            };

            let mut start_offsets = HashMap::new();
            match with_options.remove("start_offset") {
                None => {
                    start_offsets.insert(0, 0);
                }
                Some(Value::Number(n)) => {
                    start_offsets.insert(0, parse_offset(&n)?);
                }
                Some(Value::Array(vs)) => {
                    for (i, v) in vs.iter().enumerate() {
                        match v {
                            Value::Number(n) => {
                                start_offsets.insert(i32::try_from(i)?, parse_offset(n)?);
                            }
                            _ => bail!("start_offset value must be a number: {}", v),
                        }
                    }
                }
                Some(v) => bail!("invalid start_offset value: {}", v),
            }

            let encoding = get_encoding(format, envelope, with_options_original)?;

            let mut connector = KafkaSourceConnector {
                addrs: broker.parse()?,
                topic: topic.clone(),
                config_options,
                start_offsets,
                group_id_prefix,
                cluster_id: scx.catalog.config().cluster_id,
                include_timestamp: None,
                include_partition: None,
                include_topic: None,
                include_offset: None,
            };

            let unwrap_name = |alias: Option<Ident>, default, pos| {
                Some(IncludedColumnPos {
                    name: alias
                        .map(|a| a.to_string())
                        .unwrap_or_else(|| String::from(default)),
                    pos,
                })
            };

            if !include_metadata.is_empty()
                && matches!(envelope, Envelope::Debezium(DbzMode::Plain))
            {
                for kind in include_metadata {
                    if !matches!(kind.ty, SourceIncludeMetadataType::Key) {
                        bail!(
                            "INCLUDE {} with Debezium requires UPSERT semantics",
                            kind.ty
                        );
                    }
                }
            }

            for (pos, item) in include_metadata.iter().cloned().enumerate() {
                match item.ty {
                    SourceIncludeMetadataType::Timestamp => {
                        connector.include_timestamp = unwrap_name(item.alias, "timestamp", pos);
                    }
                    SourceIncludeMetadataType::Partition => {
                        connector.include_partition = unwrap_name(item.alias, "partition", pos);
                    }
                    SourceIncludeMetadataType::Topic => {
                        // TODO(bwm): This requires deeper thought, the current structure of the
                        // code requires us to clone the topic name around all over the place
                        // whether or not anyone ever uses it. Considering we expect the
                        // overwhelming majority of people will *not* want topics in dataflows that
                        // is an unnacceptable cost.
                        bail_unsupported!("INCLUDE TOPIC");
                    }
                    SourceIncludeMetadataType::Offset => {
                        connector.include_offset = unwrap_name(item.alias, "offset", pos);
                    }
                    SourceIncludeMetadataType::Key => {} // handled below
                }
            }

            let connector = ExternalSourceConnector::Kafka(connector);

            (connector, encoding)
        }
        CreateSourceConnector::Kinesis { arn, .. } => {
            let arn: ARN = arn
                .parse()
                .map_err(|e| anyhow!("Unable to parse provided ARN: {:#?}", e))?;
            let stream_name = match arn.resource.strip_prefix("stream/") {
                Some(path) => path.to_owned(),
                _ => bail!(
                    "Unable to parse stream name from resource path: {}",
                    arn.resource
                ),
            };

            let region = arn
                .region
                .ok_or_else(|| anyhow!("Provided ARN does not include an AWS region"))?;

            let aws = normalize::aws_config(&mut with_options, Some(region.into()))?;
            let connector =
                ExternalSourceConnector::Kinesis(KinesisSourceConnector { stream_name, aws });
            let encoding = get_encoding(format, envelope, with_options_original)?;
            (connector, encoding)
        }
        CreateSourceConnector::File { path, compression } => {
            let tail = match with_options.remove("tail") {
                None => false,
                Some(Value::Boolean(b)) => b,
                Some(_) => bail!("tail must be a boolean"),
            };

            let connector = ExternalSourceConnector::File(FileSourceConnector {
                path: path.clone().into(),
                compression: match compression {
                    Compression::Gzip => mz_dataflow_types::sources::Compression::Gzip,
                    Compression::None => mz_dataflow_types::sources::Compression::None,
                },
                tail,
            });
            let encoding = get_encoding(format, envelope, with_options_original)?;
            if matches!(encoding, SourceDataEncoding::KeyValue { .. }) {
                bail!("File sources do not support key decoding");
            }
            (connector, encoding)
        }
        CreateSourceConnector::S3 {
            key_sources,
            pattern,
            compression,
        } => {
            let aws = normalize::aws_config(&mut with_options, None)?;
            let mut converted_sources = Vec::new();
            for ks in key_sources {
                let dtks = match ks {
                    mz_sql_parser::ast::S3KeySource::Scan { bucket } => {
                        mz_dataflow_types::sources::S3KeySource::Scan {
                            bucket: bucket.clone(),
                        }
                    }
                    mz_sql_parser::ast::S3KeySource::SqsNotifications { queue } => {
                        mz_dataflow_types::sources::S3KeySource::SqsNotifications {
                            queue: queue.clone(),
                        }
                    }
                };
                converted_sources.push(dtks);
            }
            let connector = ExternalSourceConnector::S3(S3SourceConnector {
                key_sources: converted_sources,
                pattern: pattern
                    .as_ref()
                    .map(|p| {
                        GlobBuilder::new(p)
                            .literal_separator(true)
                            .backslash_escape(true)
                            .build()
                    })
                    .transpose()?,
                aws,
                compression: match compression {
                    Compression::Gzip => mz_dataflow_types::sources::Compression::Gzip,
                    Compression::None => mz_dataflow_types::sources::Compression::None,
                },
            });
            let encoding = get_encoding(format, envelope, with_options_original)?;
            if matches!(encoding, SourceDataEncoding::KeyValue { .. }) {
                bail!("S3 sources do not support key decoding");
            }
            (connector, encoding)
        }
        CreateSourceConnector::Postgres {
            conn,
            publication,
            slot,
        } => {
            let slot_name = slot
                .as_ref()
                .ok_or_else(|| anyhow!("Postgres sources must provide a slot name"))?;

            let connector = ExternalSourceConnector::Postgres(PostgresSourceConnector {
                conn: conn.clone(),
                publication: publication.clone(),
                slot_name: slot_name.clone(),
            });

            let encoding = SourceDataEncoding::Single(DataEncoding::Postgres);
            (connector, encoding)
        }
        CreateSourceConnector::PubNub {
            subscribe_key,
            channel,
        } => {
            match format {
                CreateSourceFormat::None | CreateSourceFormat::Bare(Format::Text) => (),
                _ => bail!("CREATE SOURCE ... PUBNUB must specify FORMAT TEXT"),
            }
            let connector = ExternalSourceConnector::PubNub(PubNubSourceConnector {
                subscribe_key: subscribe_key.clone(),
                channel: channel.clone(),
            });
            (connector, SourceDataEncoding::Single(DataEncoding::Text))
        }
        CreateSourceConnector::AvroOcf { path, .. } => {
            let tail = match with_options.remove("tail") {
                None => false,
                Some(Value::Boolean(b)) => b,
                Some(_) => bail!("tail must be a boolean"),
            };

            let connector = ExternalSourceConnector::AvroOcf(FileSourceConnector {
                path: path.clone().into(),
                compression: mz_dataflow_types::sources::Compression::None,
                tail,
            });
            if !matches!(format, CreateSourceFormat::None) {
                bail!("avro ocf sources cannot specify a format");
            }
            let reader_schema = match with_options
                .remove("reader_schema")
                .expect("purification guarantees presence of reader_schema")
            {
                Value::String(s) => s,
                _ => bail!("reader_schema option must be a string"),
            };
            let encoding = SourceDataEncoding::Single(DataEncoding::AvroOcf(AvroOcfEncoding {
                reader_schema,
            }));
            (connector, encoding)
        }
    };
    let (key_desc, value_desc) = encoding.desc()?;

    let key_envelope = get_key_envelope(include_metadata, envelope, &encoding)?;

    // TODO: remove bails as more support for upsert is added.
    let envelope = match &envelope {
        // TODO: fixup key envelope
        mz_sql_parser::ast::Envelope::None => {
            UnplannedSourceEnvelope::None(key_envelope.unwrap_or(KeyEnvelope::None))
        }
        mz_sql_parser::ast::Envelope::Debezium(mode) => {
            //TODO check that key envelope is not set
            let (before_idx, after_idx) = typecheck_debezium(&value_desc)?;

            match mode {
                DbzMode::Upsert => {
                    UnplannedSourceEnvelope::Upsert(UpsertStyle::Debezium { after_idx })
                }
                DbzMode::Plain => {
                    let dedup_projection = typecheck_debezium_dedup(&value_desc);

                    let dedup_mode = match with_options.remove("deduplication") {
                        None => match dedup_projection {
                            Ok(_) => Cow::from("ordered"),
                            Err(_) => Cow::from("none"),
                        },
                        Some(Value::String(s)) => Cow::from(s),
                        _ => bail!("deduplication option must be a string"),
                    };

                    match dedup_mode.as_ref() {
                        "ordered" => UnplannedSourceEnvelope::Debezium(DebeziumEnvelope {
                            before_idx,
                            after_idx,
                            mode: DebeziumMode::Ordered(dedup_projection?),
                        }),
                        "full" => UnplannedSourceEnvelope::Debezium(DebeziumEnvelope {
                            before_idx,
                            after_idx,
                            mode: DebeziumMode::Full(dedup_projection?),
                        }),
                        "none" => UnplannedSourceEnvelope::Debezium(DebeziumEnvelope {
                            before_idx,
                            after_idx,
                            mode: DebeziumMode::None,
                        }),
                        "full_in_range" => {
                            let parse_datetime = |s: &str| {
                                let formats = ["%Y-%m-%d %H:%M:%S%.f", "%Y-%m-%d %H:%M:%S"];
                                for format in formats {
                                    if let Ok(dt) = NaiveDateTime::parse_from_str(s, format) {
                                        return Ok(dt);
                                    }
                                }
                                if let Ok(d) = NaiveDate::parse_from_str(s, "%Y-%m-%d") {
                                    return Ok(d.and_hms(0, 0, 0));
                                }

                                bail!(
                                    "UTC DateTime specifier '{}' should match \
                                    'YYYY-MM-DD', 'YYYY-MM-DD HH:MM:SS' \
                                    or 'YYYY-MM-DD HH:MM:SS.FF",
                                    s
                                )
                            };

                            let dedup_start = match with_options.remove("deduplication_start") {
                                None => None,
                                Some(Value::String(start)) => Some(parse_datetime(&start)?),
                                _ => bail!("deduplication_start option must be a string"),
                            };

                            let dedup_end = match with_options.remove("deduplication_end") {
                                None => None,
                                Some(Value::String(end)) => Some(parse_datetime(&end)?),
                                _ => bail!("deduplication_end option must be a string"),
                            };

                            match dedup_start.zip(dedup_end) {
                                Some((start, end)) => {
                                    if start >= end {
                                        bail!(
                                            "Debezium deduplication start {} is not before end {}",
                                            start,
                                            end
                                        );
                                    }

                                    let pad_start =
                                        match with_options.remove("deduplication_pad_start") {
                                            None => None,
                                            Some(Value::String(pad_start)) => {
                                                Some(parse_datetime(&pad_start)?)
                                            }
                                            _ => bail!(
                                                "deduplication_pad_start option must be a string"
                                            ),
                                        };

                                    UnplannedSourceEnvelope::Debezium(DebeziumEnvelope {
                                        before_idx,
                                        after_idx,
                                        mode: DebeziumMode::FullInRange {
                                            start,
                                            end,
                                            pad_start,
                                            projection: dedup_projection?,
                                        }
                                    })
                                }
                                _ => bail!(
                                    "deduplication full_in_range requires both \
                                 'deduplication_start' and 'deduplication_end' parameters"
                                ),
                            }
                        }
                        _ => bail!(
                            "deduplication must be one of 'none', 'ordered', 'full' or 'full_in_range'."
                        ),
                    }
                }
            }
        }
        mz_sql_parser::ast::Envelope::Upsert => {
            if encoding.key_ref().is_none() {
                bail_unsupported!(format!("upsert requires a key/value format: {:?}", format));
            }
            //TODO(petrosagg): remove this check. it will be a breaking change
            let key_envelope = match encoding.key_ref() {
                Some(DataEncoding::Avro(_)) => key_envelope.unwrap_or(KeyEnvelope::Flattened),
                _ => key_envelope.unwrap_or(KeyEnvelope::LegacyUpsert),
            };
            UnplannedSourceEnvelope::Upsert(UpsertStyle::Default(key_envelope))
        }
        mz_sql_parser::ast::Envelope::CdcV2 => {
            //TODO check that key envelope is not set
            if let CreateSourceConnector::AvroOcf { .. } = connector {
                // TODO[btv] - there is no fundamental reason not to support this eventually,
                // but OCF goes through a separate pipeline that it hasn't been implemented for.
                bail_unsupported!("ENVELOPE MATERIALIZE over OCF (Avro files)")
            }
            match format {
                CreateSourceFormat::Bare(Format::Avro(_)) => {}
                _ => bail_unsupported!("non-Avro-encoded ENVELOPE MATERIALIZE"),
            }
            UnplannedSourceEnvelope::CdcV2
        }
    };

    // TODO(petrosagg): remove this inconsistency once INCLUDE (offset) syntax is implemented
    let include_defaults = provide_default_metadata(&envelope, encoding.value_ref());
    let metadata_columns = external_connector.metadata_columns(include_defaults);
    let metadata_column_types = external_connector.metadata_column_types(include_defaults);
    let metadata_desc = included_column_desc(metadata_columns.clone());
    let (envelope, mut desc) = envelope.desc(key_desc, value_desc, metadata_desc)?;

    // Append default metadata columns if column aliases were provided but do not include them.
    //
    // This is a confusing hack due to two combined facts:
    //
    // * we used to not allow users to refer to/alias the metadata columns because they were
    //   specified in render, instead of here in plan
    // * we don't follow postgres semantics and allow a shorter rename list than total column list
    //
    // TODO: probably we should just migrate to pg semantics and allow specifying fewer columns than
    // actually exist?
    let tmp_col;
    let col_names = if include_defaults
        && !col_names.is_empty()
        && metadata_columns.len() + col_names.len() == desc.arity()
    {
        let mut tmp = Vec::with_capacity(desc.arity());
        tmp.extend(col_names.iter().cloned());
        tmp.push(Ident::from(
            external_connector.default_metadata_column_name().unwrap(),
        ));
        tmp_col = tmp;
        &tmp_col
    } else {
        col_names
    };

    let ignore_source_keys = match with_options.remove("ignore_source_keys") {
        None => false,
        Some(Value::Boolean(b)) => b,
        Some(_) => bail!("ignore_source_keys must be a boolean"),
    };

    if ignore_source_keys {
        desc = desc.without_keys();
    }

    desc = plan_utils::maybe_rename_columns(format!("source {}", name), desc, &col_names)?;

    // Apply user-specified key constraint
    if let Some(KeyConstraint::PrimaryKeyNotEnforced { columns }) = key_constraint.clone() {
        let key_columns = columns
            .into_iter()
            .map(normalize::column_name)
            .collect::<Vec<_>>();

        let mut uniq = HashSet::new();
        for col in key_columns.iter() {
            if !uniq.insert(col) {
                bail!("Repeated column name in source key constraint: {}", col);
            }
        }

        let key_indices = key_columns
            .iter()
            .map(|col| -> anyhow::Result<usize> {
                let name_idx = desc
                    .get_by_name(col)
                    .map(|(idx, _type)| idx)
                    .ok_or_else(|| anyhow!("No such column in source key constraint: {}", col))?;
                if desc.get_unambiguous_name(name_idx).is_none() {
                    bail!("Ambiguous column in source key constraint: {}", col);
                }
                Ok(name_idx)
            })
            .collect::<Result<Vec<_>, _>>()?;

        if !desc.typ().keys.is_empty() {
            return Err(key_constraint_err(&desc, &key_columns));
        } else {
            desc = desc.with_key(key_indices);
        }
    }

    let if_not_exists = *if_not_exists;
    let materialized = *materialized;
    let name = scx.allocate_name(normalize::unresolved_object_name(name.clone())?);
    let create_sql = normalize::create_statement(&scx, Statement::CreateSource(stmt))?;

    // Allow users to specify a timeline. If they do not, determine a default timeline for the source.
    let timeline = if let Some(timeline) = with_options.remove("timeline") {
        match timeline {
            Value::String(timeline) => Timeline::User(timeline),
            v => bail!("unsupported timeline value {}", v.to_ast_string()),
        }
    } else {
        match envelope {
            SourceEnvelope::CdcV2 => match with_options.remove("epoch_ms_timeline") {
                None => Timeline::External(name.to_string()),
                Some(Value::Boolean(true)) => Timeline::EpochMilliseconds,
                Some(v) => bail!("unsupported epoch_ms_timeline value {}", v),
            },
            _ => Timeline::EpochMilliseconds,
        }
    };

    let expr = HirRelationExpr::Get {
        id: mz_expr::Id::LocalBareSource,
        typ: desc.typ().clone(),
    }
    .lower();

    let source = Source {
        create_sql,
        connector: SourceConnector::External {
            connector: external_connector,
            encoding,
            envelope,
            metadata_columns: metadata_column_types,
            ts_frequency,
            timeline,
        },
        expr,
        desc,
    };

    normalize::ensure_empty_options(&with_options, "CREATE SOURCE")?;

    Ok(Plan::CreateSource(CreateSourcePlan {
        name,
        source,
        if_not_exists,
        materialized,
    }))
}

fn typecheck_debezium(value_desc: &RelationDesc) -> Result<(usize, usize), anyhow::Error> {
    let (before_idx, before_ty) = value_desc
        .get_by_name(&"before".into())
        .ok_or_else(|| anyhow!("'before' column missing from debezium input"))?;
    let (after_idx, after_ty) = value_desc
        .get_by_name(&"after".into())
        .ok_or_else(|| anyhow!("'after' column missing from debezium input"))?;
    if !matches!(before_ty.scalar_type, ScalarType::Record { .. }) {
        bail!("'before' column must be of type record");
    }
    if before_ty != after_ty {
        bail!("'before' type differs from 'after' column");
    }
    Ok((before_idx, after_idx))
}

fn typecheck_debezium_dedup(
    value_desc: &RelationDesc,
) -> Result<DebeziumDedupProjection, anyhow::Error> {
    let (source_idx, source_ty) = value_desc
        .get_by_name(&"source".into())
        .ok_or_else(|| anyhow!("'source' column missing from debezium input"))?;

    let source_fields = match &source_ty.scalar_type {
        ScalarType::Record { fields, .. } => fields,
        _ => bail!("'source' column must be of type record"),
    };

    let snapshot = source_fields
        .iter()
        .enumerate()
        .find(|(_, f)| f.0.as_str() == "snapshot");
    let snapshot_idx = match snapshot {
        Some((idx, (_, ty))) => match &ty.scalar_type {
            ScalarType::String | ScalarType::Bool => idx,
            _ => bail!("'snapshot' column must be a string or boolean"),
        },
        None => bail!("'snapshot' field missing from source record"),
    };

    let mut mysql = (None, None, None);
    let mut postgres = (None, None);
    let mut sqlserver = (None, None);

    for (idx, (name, ty)) in source_fields.iter().enumerate() {
        match name.as_str() {
            "file" => {
                mysql.0 = match &ty.scalar_type {
                    ScalarType::String => Some(idx),
                    t => bail!(r#""source"."file" must be of type string, found {:?}"#, t),
                }
            }
            "pos" => {
                mysql.1 = match &ty.scalar_type {
                    ScalarType::Int64 => Some(idx),
                    t => bail!(r#""source"."pos" must be of type bigint, found {:?}"#, t),
                }
            }
            "row" => {
                mysql.2 = match &ty.scalar_type {
                    ScalarType::Int32 => Some(idx),
                    t => bail!(r#""source"."file" must be of type int, found {:?}"#, t),
                }
            }
            "sequence" => {
                postgres.0 = match &ty.scalar_type {
                    ScalarType::String => Some(idx),
                    t => bail!(
                        r#""source"."sequence" must be of type string, found {:?}"#,
                        t
                    ),
                }
            }
            "lsn" => {
                postgres.1 = match &ty.scalar_type {
                    ScalarType::Int64 => Some(idx),
                    t => bail!(r#""source"."lsn" must be of type bigint, found {:?}"#, t),
                }
            }
            "change_lsn" => {
                sqlserver.0 = match &ty.scalar_type {
                    ScalarType::String => Some(idx),
                    t => bail!(
                        r#""source"."change_lsn" must be of type string, found {:?}"#,
                        t
                    ),
                }
            }
            "event_serial_no" => {
                sqlserver.1 = match &ty.scalar_type {
                    ScalarType::Int64 => Some(idx),
                    t => bail!(
                        r#""source"."event_serial_no" must be of type bigint, found {:?}"#,
                        t
                    ),
                }
            }
            _ => {}
        }
    }

    let source_projection = if let (Some(file), Some(pos), Some(row)) = mysql {
        DebeziumSourceProjection::MySql { file, pos, row }
    } else if let (Some(change_lsn), Some(event_serial_no)) = sqlserver {
        DebeziumSourceProjection::SqlServer {
            change_lsn,
            event_serial_no,
        }
    } else if let (sequence, Some(lsn)) = postgres {
        DebeziumSourceProjection::Postgres { sequence, lsn }
    } else {
        bail!("unknown type of upstream database")
    };

    let (transaction_idx, transaction_ty) = value_desc
        .get_by_name(&"transaction".into())
        .ok_or_else(|| anyhow!("'transaction' column missing from debezium input"))?;

    let tx_fields = match &transaction_ty.scalar_type {
        ScalarType::Record { fields, .. } => fields,
        _ => bail!("'transaction' column must be of type record"),
    };

    let total_order = tx_fields
        .iter()
        .enumerate()
        .find(|(_, f)| f.0.as_str() == "total_order");
    let total_order_idx = match total_order {
        Some((idx, (_, ty))) => match &ty.scalar_type {
            ScalarType::Int64 => idx,
            _ => bail!("'total_order' column must be an bigint"),
        },
        None => bail!("'total_order' field missing from tx record"),
    };
    Ok(DebeziumDedupProjection {
        source_idx,
        snapshot_idx,
        source_projection,
        transaction_idx,
        total_order_idx,
    })
}

fn get_encoding<T: mz_sql_parser::ast::AstInfo>(
    format: &CreateSourceFormat<Raw>,
    envelope: &Envelope,
    with_options: &Vec<SqlOption<T>>,
) -> Result<SourceDataEncoding, anyhow::Error> {
    let encoding = match format {
        CreateSourceFormat::None => bail!("Source format must be specified"),
        CreateSourceFormat::Bare(format) => get_encoding_inner(format, with_options)?,
        CreateSourceFormat::KeyValue { key, value } => {
            let key = match get_encoding_inner(key, with_options)? {
                SourceDataEncoding::Single(key) => key,
                SourceDataEncoding::KeyValue { key, .. } => key,
            };
            let value = match get_encoding_inner(value, with_options)? {
                SourceDataEncoding::Single(value) => value,
                SourceDataEncoding::KeyValue { value, .. } => value,
            };
            SourceDataEncoding::KeyValue { key, value }
        }
    };

    let requires_keyvalue = matches!(
        envelope,
        Envelope::Debezium(DbzMode::Upsert) | Envelope::Upsert
    );
    let is_keyvalue = matches!(encoding, SourceDataEncoding::KeyValue { .. });
    if requires_keyvalue && !is_keyvalue {
        bail!("ENVELOPE [DEBEZIUM] UPSERT requires that KEY FORMAT be specified");
    };

    Ok(encoding)
}

fn get_encoding_inner<T: mz_sql_parser::ast::AstInfo>(
    format: &Format<Raw>,
    with_options: &Vec<SqlOption<T>>,
) -> Result<SourceDataEncoding, anyhow::Error> {
    // Avro/CSR can return a `SourceDataEncoding::KeyValue`
    Ok(SourceDataEncoding::Single(match format {
        Format::Bytes => DataEncoding::Bytes,
        Format::Avro(schema) => {
            let Schema {
                key_schema,
                value_schema,
                schema_registry_config,
                confluent_wire_format,
            } = match schema {
                // TODO(jldlaughlin): we need a way to pass in primary key information
                // when building a source from a string or file.
                AvroSchema::InlineSchema {
                    schema: mz_sql_parser::ast::Schema::Inline(schema),
                    with_options,
                } => {
                    with_options! {
                        struct ConfluentMagic {
                            confluent_wire_format: bool,
                        }
                    }

                    Schema {
                        key_schema: None,
                        value_schema: schema.clone(),
                        schema_registry_config: None,
                        confluent_wire_format: ConfluentMagic::try_from(with_options.clone())?
                            .confluent_wire_format
                            .unwrap_or(true),
                    }
                }
                AvroSchema::InlineSchema {
                    schema: mz_sql_parser::ast::Schema::File(_),
                    ..
                } => {
                    unreachable!("File schema should already have been inlined")
                }
                AvroSchema::Csr {
                    csr_connector:
                        CsrConnectorAvro {
                            url,
                            seed,
                            with_options: ccsr_options,
                        },
                } => {
                    let mut ccsr_with_options = normalize::options(&ccsr_options);
                    let ccsr_config = kafka_util::generate_ccsr_client_config(
                        url.parse()?,
                        &kafka_util::extract_config(&mut normalize::options(with_options))?,
                        &mut ccsr_with_options,
                    )?;
                    normalize::ensure_empty_options(
                        &ccsr_with_options,
                        "CONFLUENT SCHEMA REGISTRY",
                    )?;
                    if let Some(seed) = seed {
                        Schema {
                            key_schema: seed.key_schema.clone(),
                            value_schema: seed.value_schema.clone(),
                            schema_registry_config: Some(ccsr_config),
                            confluent_wire_format: true,
                        }
                    } else {
                        unreachable!("CSR seed resolution should already have been called: Avro")
                    }
                }
            };

            if let Some(key_schema) = key_schema {
                return Ok(SourceDataEncoding::KeyValue {
                    key: DataEncoding::Avro(AvroEncoding {
                        schema: key_schema,
                        schema_registry_config: schema_registry_config.clone(),
                        confluent_wire_format,
                    }),
                    value: DataEncoding::Avro(AvroEncoding {
                        schema: value_schema,
                        schema_registry_config,
                        confluent_wire_format,
                    }),
                });
            } else {
                DataEncoding::Avro(AvroEncoding {
                    schema: value_schema,
                    schema_registry_config,
                    confluent_wire_format,
                })
            }
        }
        Format::Protobuf(schema) => match schema {
            ProtobufSchema::Csr {
                csr_connector:
                    CsrConnectorProto {
                        url,
                        seed,
                        with_options: ccsr_options,
                    },
            } => {
                if let Some(CsrSeedCompiledOrLegacy::Compiled(CsrSeedCompiled { key, value })) =
                    seed
                {
                    let mut ccsr_with_options = normalize::options(&ccsr_options);

                    // We validate here instead of in purification, to match the behavior of avro
                    let _ccsr_config = kafka_util::generate_ccsr_client_config(
                        url.parse()?,
                        &kafka_util::extract_config(&mut normalize::options(with_options))?,
                        &mut ccsr_with_options,
                    )?;
                    normalize::ensure_empty_options(
                        &ccsr_with_options,
                        "CONFLUENT SCHEMA REGISTRY",
                    )?;

                    let value = DataEncoding::Protobuf(ProtobufEncoding {
                        descriptors: strconv::parse_bytes(&value.schema)?,
                        message_name: value.message_name.clone(),
                        confluent_wire_format: true,
                    });
                    if let Some(key) = key {
                        return Ok(SourceDataEncoding::KeyValue {
                            key: DataEncoding::Protobuf(ProtobufEncoding {
                                descriptors: strconv::parse_bytes(&key.schema)?,
                                message_name: key.message_name.clone(),
                                confluent_wire_format: true,
                            }),
                            value,
                        });
                    }
                    value
                } else {
                    unreachable!("CSR seed resolution should already have been called: Proto")
                }
            }
            ProtobufSchema::InlineSchema {
                message_name,
                schema,
            } => {
                let descriptors = match schema {
                    mz_sql_parser::ast::Schema::Inline(bytes) => strconv::parse_bytes(&bytes)?,
                    mz_sql_parser::ast::Schema::File(_) => {
                        unreachable!("File schema should already have been inlined")
                    }
                };

                DataEncoding::Protobuf(ProtobufEncoding {
                    descriptors,
                    message_name: message_name.to_owned(),
                    confluent_wire_format: false,
                })
            }
        },
        Format::Regex(regex) => {
            let regex = Regex::new(&regex)?;
            DataEncoding::Regex(RegexEncoding {
                regex: mz_repr::adt::regex::Regex(regex),
            })
        }
        Format::Csv { columns, delimiter } => {
            let columns = match columns {
                CsvColumns::Header { names } => {
                    if names.is_empty() {
                        bail!("[internal error] column spec should get names in purify")
                    }
                    ColumnSpec::Header {
                        names: names.iter().cloned().map(|n| n.into_string()).collect(),
                    }
                }
                CsvColumns::Count(n) => ColumnSpec::Count(*n),
            };
            DataEncoding::Csv(CsvEncoding {
                columns,
                delimiter: match *delimiter as u32 {
                    0..=127 => *delimiter as u8,
                    _ => bail!("CSV delimiter must be an ASCII character"),
                },
            })
        }
        Format::Json => bail_unsupported!("JSON sources"),
        Format::Text => DataEncoding::Text,
    }))
}

/// Extract the key envelope, if it is requested
fn get_key_envelope(
    included_items: &[SourceIncludeMetadata],
    envelope: &Envelope,
    encoding: &SourceDataEncoding,
) -> Result<Option<KeyEnvelope>, anyhow::Error> {
    let key_definition = included_items
        .iter()
        .find(|i| i.ty == SourceIncludeMetadataType::Key);
    if matches!(envelope, Envelope::Debezium { .. }) && key_definition.is_some() {
        bail!("Cannot use INCLUDE KEY with ENVELOPE DEBEZIUM: Debezium values include all keys.");
    }
    if let Some(kd) = key_definition {
        Ok(Some(match (&kd.alias, encoding) {
            (Some(name), SourceDataEncoding::KeyValue { .. }) => {
                KeyEnvelope::Named(name.as_str().to_string())
            }
            (None, _) if matches!(envelope, Envelope::Upsert { .. }) => KeyEnvelope::LegacyUpsert,
            (None, SourceDataEncoding::KeyValue { key, .. }) => {
                // If the key is requested but comes from an unnamed type then it gets the name "key"
                //
                // Otherwise it gets the names of the columns in the type
                let is_composite = match key {
                    DataEncoding::AvroOcf { .. } | DataEncoding::Postgres => {
                        bail!("{} sources cannot use INCLUDE KEY", key.op_name())
                    }
                    DataEncoding::Bytes | DataEncoding::Text => false,
                    DataEncoding::Avro(_)
                    | DataEncoding::Csv(_)
                    | DataEncoding::Protobuf(_)
                    | DataEncoding::Regex { .. } => true,
                };

                if is_composite {
                    KeyEnvelope::Flattened
                } else {
                    KeyEnvelope::Named("key".to_string())
                }
            }
            (_, SourceDataEncoding::Single(_)) => {
                // `kd.alias` == `None` means `INCLUDE KEY`
                // `kd.alias` == `Some(_) means INCLUDE KEY AS ___`
                // These both make sense with the same error message
                bail!(
                    "INCLUDE KEY requires specifying KEY FORMAT .. VALUE FORMAT, \
                        got bare FORMAT"
                );
            }
        }))
    } else {
        Ok(None)
    }
}

pub fn describe_create_view(
    _: &StatementContext,
    _: CreateViewStatement<Raw>,
) -> Result<StatementDesc, anyhow::Error> {
    Ok(StatementDesc::new(None))
}

pub fn plan_view(
    scx: &StatementContext,
    def: &mut ViewDefinition<Raw>,
    params: &Params,
    temporary: bool,
) -> Result<(FullName, View), anyhow::Error> {
    let create_sql = normalize::create_statement(
        scx,
        Statement::CreateView(CreateViewStatement {
            if_exists: IfExistsBehavior::Error,
            temporary,
            materialized: false,
            definition: def.clone(),
        }),
    )?;

    let ViewDefinition {
        name,
        columns,
        query,
        with_options,
    } = def;

    if !with_options.is_empty() {
        bail_unsupported!("WITH options");
    }
    let query::PlannedQuery {
        mut expr,
        mut desc,
        finishing,
        depends_on,
    } = query::plan_root_query(scx, query.clone(), QueryLifetime::Static)?;

    expr.bind_parameters(&params)?;
    //TODO: materialize#724 - persist finishing information with the view?
    expr.finish(finishing);
    let relation_expr = expr.optimize_and_lower(&scx.into());

    let name = if temporary {
        scx.allocate_temporary_name(normalize::unresolved_object_name(name.to_owned())?)
    } else {
        scx.allocate_name(normalize::unresolved_object_name(name.to_owned())?)
    };

    desc = plan_utils::maybe_rename_columns(format!("view {}", name), desc, &columns)?;
    let names: Vec<ColumnName> = desc.iter_names().cloned().collect();

    if let Some(dup) = names.iter().duplicates().next() {
        bail!("column {} specified more than once", dup.as_str().quoted());
    }

    let view = View {
        create_sql,
        expr: relation_expr,
        column_names: names,
        temporary,
        depends_on,
    };

    Ok((name, view))
}

pub fn plan_create_view(
    scx: &StatementContext,
    mut stmt: CreateViewStatement<Raw>,
    params: &Params,
) -> Result<Plan, anyhow::Error> {
    let CreateViewStatement {
        temporary,
        materialized,
        if_exists,
        definition,
    } = &mut stmt;
    let (name, view) = plan_view(scx, definition, params, *temporary)?;
    let replace = if *if_exists == IfExistsBehavior::Replace {
        if let Ok(item) = scx.catalog.resolve_item(&name.clone().into()) {
            if view.expr.global_uses().contains(&item.id()) {
                bail!(
                    "cannot replace view {0}: depended upon by new {0} definition",
                    item.name()
                );
            }
            let cascade = false;
            plan_drop_item(scx, ObjectType::View, item, cascade)?
        } else {
            None
        }
    } else {
        None
    };
    Ok(Plan::CreateView(CreateViewPlan {
        name,
        view,
        replace,
        materialize: *materialized,
        if_not_exists: *if_exists == IfExistsBehavior::Skip,
    }))
}

pub fn describe_create_views(
    _: &StatementContext,
    _: CreateViewsStatement<Raw>,
) -> Result<StatementDesc, anyhow::Error> {
    Ok(StatementDesc::new(None))
}

pub fn plan_create_views(
    scx: &StatementContext,
    CreateViewsStatement {
        definitions,
        if_exists,
        materialized,
        temporary,
    }: CreateViewsStatement<Raw>,
) -> Result<Plan, anyhow::Error> {
    match definitions {
        CreateViewsDefinitions::Literal(view_definitions) => {
            let mut views = Vec::with_capacity(view_definitions.len());
            for mut definition in view_definitions {
                let view = plan_view(scx, &mut definition, &Params::empty(), temporary)?;
                views.push(view);
            }
            Ok(Plan::CreateViews(CreateViewsPlan {
                views,
                if_not_exists: if_exists == IfExistsBehavior::Skip,
                materialize: materialized,
            }))
        }
        CreateViewsDefinitions::Source { .. } => bail!("cannot create view from source"),
    }
}

#[allow(clippy::too_many_arguments)]
fn kafka_sink_builder(
    format: Option<Format<Raw>>,
    consistency: Option<KafkaConsistency<Raw>>,
    with_options: &mut BTreeMap<String, Value>,
    broker: String,
    topic_prefix: String,
    relation_key_indices: Option<Vec<usize>>,
    key_desc_and_indices: Option<(RelationDesc, Vec<usize>)>,
    value_desc: RelationDesc,
    topic_suffix_nonce: String,
    root_dependencies: &[&dyn CatalogItem],
) -> Result<SinkConnectorBuilder, anyhow::Error> {
    let consistency_topic = match with_options.remove("consistency_topic") {
        None => None,
        Some(Value::String(topic)) => Some(topic),
        Some(_) => bail!("consistency_topic must be a string"),
    };
    if consistency_topic.is_some() && consistency.is_some() {
        // We're keeping consistency_topic around for backwards compatibility. Users
        // should not be able to specify consistency_topic and the newer CONSISTENCY options.
        bail!("Cannot specify consistency_topic and CONSISTENCY options simultaneously");
    }
    let reuse_topic = match with_options.remove("reuse_topic") {
        Some(Value::Boolean(b)) => b,
        None => false,
        Some(_) => bail!("reuse_topic must be a boolean"),
    };
    let config_options = kafka_util::extract_config(with_options)?;

    let avro_key_fullname = match with_options.remove("avro_key_fullname") {
        Some(Value::String(s)) => Some(s),
        None => None,
        Some(_) => bail!("avro_key_fullname must be a string"),
    };

    if key_desc_and_indices.is_none() && avro_key_fullname.is_some() {
        bail!("Cannot specify avro_key_fullname without a corresponding KEY field");
    }

    let avro_value_fullname = match with_options.remove("avro_value_fullname") {
        Some(Value::String(s)) => Some(s),
        None => None,
        Some(_) => bail!("avro_value_fullname must be a string"),
    };

    if key_desc_and_indices.is_some()
        && (avro_key_fullname.is_some() ^ avro_value_fullname.is_some())
    {
        bail!("Must specify both avro_key_fullname and avro_value_fullname when specifying generated schema names");
    }

    let format = match format {
        Some(Format::Avro(AvroSchema::Csr {
            csr_connector:
                CsrConnectorAvro {
                    url,
                    seed,
                    with_options,
                },
        })) => {
            if seed.is_some() {
                bail!("SEED option does not make sense with sinks");
            }
            let mut ccsr_with_options = normalize::options(&with_options);

            let schema_registry_url = url.parse::<Url>()?;
            let ccsr_config = kafka_util::generate_ccsr_client_config(
                schema_registry_url.clone(),
                &config_options,
                &mut ccsr_with_options,
            )?;

            let include_transaction =
                reuse_topic || consistency_topic.is_some() || consistency.is_some();
            let schema_generator = AvroSchemaGenerator::new(
                avro_key_fullname.as_deref(),
                avro_value_fullname.as_deref(),
                key_desc_and_indices
                    .as_ref()
                    .map(|(desc, _indices)| desc.clone()),
                value_desc.clone(),
                include_transaction,
            );
            let value_schema = schema_generator.value_writer_schema().to_string();
            let key_schema = schema_generator
                .key_writer_schema()
                .map(|key_schema| key_schema.to_string());

            normalize::ensure_empty_options(&ccsr_with_options, "CONFLUENT SCHEMA REGISTRY")?;

            KafkaSinkFormat::Avro {
                schema_registry_url,
                key_schema,
                value_schema,
                ccsr_config,
            }
        }
        Some(Format::Json) => KafkaSinkFormat::Json,
        Some(format) => bail_unsupported!(format!("sink format {:?}", format)),
        None => bail_unsupported!("sink without format"),
    };

    let consistency_config = get_kafka_sink_consistency_config(
        &topic_prefix,
        &format,
        &config_options,
        reuse_topic,
        consistency,
        consistency_topic,
    )?;

    let broker_addrs = broker.parse()?;

    let transitive_source_dependencies: Vec<_> = if reuse_topic {
        for item in root_dependencies.iter() {
            if item.item_type() == CatalogItemType::Source {
                if !item.source_connector()?.yields_stable_input() {
                    bail!(
                    "reuse_topic requires that sink input dependencies are replayable, {} is not",
                    item.name()
                );
                }
            } else if item.item_type() != CatalogItemType::Source {
                bail!(
                    "reuse_topic requires that sink input dependencies are sources, {} is not",
                    item.name()
                );
            };
        }

        root_dependencies.iter().map(|i| i.id()).collect()
    } else {
        Vec::new()
    };

    // Use the user supplied value for partition count, or default to -1 (broker default)
    let partition_count = match with_options.remove("partition_count") {
        None => -1,
        Some(Value::Number(n)) => n.parse::<i32>()?,
        Some(_) => bail!("partition count for sink topics must be an integer"),
    };

    if partition_count == 0 || partition_count < -1 {
        bail!(
            "partition count for sink topics must be a positive integer or -1 for broker default"
        );
    }

    // Use the user supplied value for replication factor, or default to -1 (broker default)
    let replication_factor = match with_options.remove("replication_factor") {
        None => -1,
        Some(Value::Number(n)) => n.parse::<i32>()?,
        Some(_) => bail!("replication factor for sink topics must be an integer"),
    };

    if replication_factor == 0 || replication_factor < -1 {
        bail!(
            "replication factor for sink topics must be a positive integer or -1 for broker default"
        );
    }

    let retention_duration = match with_options.remove("retention_ms") {
        None => None,
        Some(Value::Number(n)) => match n.parse::<i64>()? {
            -1 => Some(None),
            millis @ 0.. => Some(Some(Duration::from_millis(millis as u64))),
            _ => bail!("retention ms for sink topics must be greater than or equal to -1"),
        },
        Some(_) => bail!("retention ms for sink topics must be an integer"),
    };

    let retention_bytes = match with_options.remove("retention_bytes") {
        None => None,
        Some(Value::Number(n)) => Some(n.parse::<i64>()?),
        Some(_) => bail!("retention bytes for sink topics must be an integer"),
    };

    if retention_bytes.unwrap_or(0) < -1 {
        bail!("retention bytes for sink topics must be greater than or equal to -1");
    }
    let retention = KafkaSinkConnectorRetention {
        duration: retention_duration,
        bytes: retention_bytes,
    };

    let consistency_topic = consistency_config.clone().map(|config| config.0);
    let consistency_format = consistency_config.map(|config| config.1);

    Ok(SinkConnectorBuilder::Kafka(KafkaSinkConnectorBuilder {
        broker_addrs,
        format,
        topic_prefix,
        consistency_topic_prefix: consistency_topic,
        consistency_format,
        topic_suffix_nonce,
        partition_count,
        replication_factor,
        fuel: 10000,
        config_options,
        relation_key_indices,
        key_desc_and_indices,
        value_desc,
        reuse_topic,
        transitive_source_dependencies,
        retention,
    }))
}

/// Determines the consistency configuration (topic and format) that should be used for a Kafka
/// sink based on the given configuration items.
///
/// This is slightly complicated because of a desire to maintain backwards compatibility with
/// previous ways of specifying consistency configuration. [`KafkaConsistency`] is the new way of
/// doing things, we support specifying just a topic name (via `consistency_topic`) for backwards
/// compatibility.
fn get_kafka_sink_consistency_config(
    topic_prefix: &str,
    sink_format: &KafkaSinkFormat,
    config_options: &BTreeMap<String, String>,
    reuse_topic: bool,
    consistency: Option<KafkaConsistency<Raw>>,
    consistency_topic: Option<String>,
) -> Result<Option<(String, KafkaSinkFormat)>, anyhow::Error> {
    let result = match consistency {
        Some(KafkaConsistency {
            topic,
            topic_format,
        }) => match topic_format {
            Some(Format::Avro(AvroSchema::Csr {
                csr_connector:
                    CsrConnectorAvro {
                        url,
                        seed,
                        with_options,
                    },
            })) => {
                if seed.is_some() {
                    bail!("SEED option does not make sense with sinks");
                }
                let schema_registry_url = url.parse::<Url>()?;
                let mut ccsr_with_options = normalize::options(&with_options);
                let ccsr_config = kafka_util::generate_ccsr_client_config(
                    schema_registry_url.clone(),
                    config_options,
                    &mut ccsr_with_options,
                )?;

                Some((
                    topic,
                    KafkaSinkFormat::Avro {
                        schema_registry_url,
                        key_schema: None,
                        value_schema: avro::get_debezium_transaction_schema().canonical_form(),
                        ccsr_config,
                    },
                ))
            }
            None => {
                // If a CONSISTENCY FORMAT is not provided, default to the FORMAT of the sink.
                match sink_format {
                    format @ KafkaSinkFormat::Avro { .. } => Some((topic, format.clone())),
                    KafkaSinkFormat::Json => bail_unsupported!("CONSISTENCY FORMAT JSON"),
                }
            }
            Some(other) => bail_unsupported!(format!("CONSISTENCY FORMAT {}", &other)),
        },
        None => {
            // Support use of `consistency_topic` with option if the sink is Avro-formatted
            // for backwards compatibility.
            if reuse_topic | consistency_topic.is_some() {
                match sink_format {
                    KafkaSinkFormat::Avro {
                        schema_registry_url,
                        ccsr_config,
                        ..
                    } => {
                        let consistency_topic = match consistency_topic {
                            Some(topic) => topic,
                            None => {
                                let default_consistency_topic =
                                    format!("{}-consistency", topic_prefix);
                                debug!(
                                    "Using default consistency topic '{}' for topic '{}'",
                                    default_consistency_topic, topic_prefix
                                );
                                default_consistency_topic
                            }
                        };
                        Some((
                            consistency_topic,
                            KafkaSinkFormat::Avro {
                                schema_registry_url: schema_registry_url.clone(),
                                key_schema: None,
                                value_schema: avro::get_debezium_transaction_schema()
                                    .canonical_form(),
                                ccsr_config: ccsr_config.clone(),
                            },
                        ))
                    }
                    KafkaSinkFormat::Json => bail!("For FORMAT JSON, you need to manually specify an Avro consistency topic using 'CONSISTENCY TOPIC consistency_topic CONSISTENCY FORMAT AVRO USING CONFLUENT SCHEMA REGISTRY url'. The default of using a JSON consistency topic is not supported."),
                }
            } else {
                None
            }
        }
    };

    Ok(result)
}

fn avro_ocf_sink_builder(
    format: Option<Format<Raw>>,
    path: String,
    file_name_suffix: String,
    value_desc: RelationDesc,
) -> Result<SinkConnectorBuilder, anyhow::Error> {
    if format.is_some() {
        bail!("avro ocf sinks cannot specify a format");
    }

    let path = PathBuf::from(path);

    if path.is_dir() {
        bail!("avro ocf sink cannot write to a directory");
    }

    Ok(SinkConnectorBuilder::AvroOcf(AvroOcfSinkConnectorBuilder {
        path,
        file_name_suffix,
        value_desc,
    }))
}

pub fn describe_create_sink(
    _: &StatementContext,
    _: CreateSinkStatement<Raw>,
) -> Result<StatementDesc, anyhow::Error> {
    Ok(StatementDesc::new(None))
}

pub fn plan_create_sink(
    scx: &StatementContext,
    stmt: CreateSinkStatement<Raw>,
) -> Result<Plan, anyhow::Error> {
    let create_sql = normalize::create_statement(scx, Statement::CreateSink(stmt.clone()))?;
    let CreateSinkStatement {
        name,
        from,
        connector,
        with_options,
        format,
        envelope,
        with_snapshot,
        as_of,
        if_not_exists,
    } = stmt;

    let envelope = match envelope {
        None | Some(Envelope::Debezium(mz_sql_parser::ast::DbzMode::Plain)) => {
            SinkEnvelope::Debezium
        }
        Some(Envelope::Upsert) => SinkEnvelope::Upsert,
        Some(Envelope::CdcV2) => bail_unsupported!("CDCv2 sinks"),
        Some(Envelope::Debezium(mz_sql_parser::ast::DbzMode::Upsert)) => {
            bail_unsupported!("UPSERT doesn't make sense for sinks")
        }
        Some(Envelope::None) => bail_unsupported!("\"ENVELOPE NONE\" sinks"),
    };
    let name = scx.allocate_name(normalize::unresolved_object_name(name)?);
    let from = scx.resolve_item(from)?;
    let suffix_nonce = format!(
        "{}-{}",
        scx.catalog.config().start_time.timestamp(),
        scx.catalog.config().nonce
    );

    let mut with_options = normalize::options(&with_options);

    let desc = from.desc()?;
    let key_indices = match &connector {
        CreateSinkConnector::Kafka { key, .. } => {
            if let Some(key) = key.clone() {
                let key_columns = key
                    .key_columns
                    .into_iter()
                    .map(normalize::column_name)
                    .collect::<Vec<_>>();
                let mut uniq = HashSet::new();
                for col in key_columns.iter() {
                    if !uniq.insert(col) {
                        bail!("Repeated column name in sink key: {}", col);
                    }
                }
                let indices = key_columns
                    .iter()
                    .map(|col| -> anyhow::Result<usize> {
                        let name_idx = desc
                            .get_by_name(col)
                            .map(|(idx, _type)| idx)
                            .ok_or_else(|| anyhow!("No such column: {}", col))?;
                        if desc.get_unambiguous_name(name_idx).is_none() {
                            bail!("Ambiguous column: {}", col);
                        }
                        Ok(name_idx)
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                let is_valid_key =
                    desc.typ().keys.iter().any(|key_columns| {
                        key_columns.iter().all(|column| indices.contains(column))
                    });
                if key.not_enforced && envelope == SinkEnvelope::Upsert {
                    // TODO: We should report a warning notice back to the user via the pgwire
                    // protocol. See https://github.com/MaterializeInc/materialize/issues/9333.
                    warn!(
                        "Verification of upsert key disabled for sink '{}' via 'NOT ENFORCED'. This is potentially dangerous and can lead to crashing materialize when the specified key is not in fact a unique key of the sinked view.",
                        name
                    );
                } else if !is_valid_key && envelope == SinkEnvelope::Upsert {
                    return Err(invalid_upsert_key_err(&desc, &key_columns));
                }
                Some(indices)
            } else {
                None
            }
        }
        CreateSinkConnector::AvroOcf { .. } => None,
    };

    // pick the first valid natural relation key, if any
    let relation_key_indices = desc.typ().keys.get(0).cloned();

    let key_desc_and_indices = key_indices.map(|key_indices| {
        let cols = desc.clone().into_iter().collect::<Vec<_>>();
        let (names, types): (Vec<_>, Vec<_>) =
            key_indices.iter().map(|&idx| cols[idx].clone()).unzip();
        let typ = RelationType::new(types);
        (RelationDesc::new(typ, names), key_indices)
    });

    if key_desc_and_indices.is_none() && envelope == SinkEnvelope::Upsert {
        return Err(PlanError::UpsertSinkWithoutKey.into());
    }

    let value_desc = match envelope {
        SinkEnvelope::Debezium => envelopes::dbz_desc(desc.clone()),
        SinkEnvelope::Upsert => desc.clone(),
    };

    if as_of.is_some() {
        bail!("CREATE SINK ... AS OF is no longer supported");
    }

    let mut depends_on = vec![from.id()];
    depends_on.extend(from.uses());

    let root_user_dependencies = get_root_dependencies(scx, &depends_on);

    let connector_builder = match connector {
        CreateSinkConnector::Kafka {
            broker,
            topic,
            consistency,
            ..
        } => kafka_sink_builder(
            format,
            consistency,
            &mut with_options,
            broker,
            topic,
            relation_key_indices,
            key_desc_and_indices,
            value_desc,
            suffix_nonce,
            &root_user_dependencies,
        )?,
        CreateSinkConnector::AvroOcf { path } => {
            avro_ocf_sink_builder(format, path, suffix_nonce, value_desc)?
        }
    };

    normalize::ensure_empty_options(&with_options, "CREATE SINK")?;

    Ok(Plan::CreateSink(CreateSinkPlan {
        name,
        sink: Sink {
            create_sql,
            from: from.id(),
            connector_builder,
            envelope,
            depends_on,
        },
        with_snapshot,
        if_not_exists,
    }))
}

fn invalid_upsert_key_err(desc: &RelationDesc, requested_user_key: &[ColumnName]) -> anyhow::Error {
    let requested_user_key = requested_user_key
        .iter()
        .map(|column| column.as_str())
        .join(", ");
    let requested_user_key = format!("({})", requested_user_key);
    let valid_keys = if desc.typ().keys.is_empty() {
        "there are no valid keys".to_owned()
    } else {
        let valid_keys = desc
            .typ()
            .keys
            .iter()
            .map(|key_columns| {
                let columns_string = key_columns
                    .iter()
                    .map(|col| desc.get_name(*col).as_str())
                    .join(", ");
                format!("({})", columns_string)
            })
            .join(", ");
        format!("valid keys are: {}", valid_keys)
    };
    anyhow!("Invalid upsert key: {}, {}", requested_user_key, valid_keys)
}

fn key_constraint_err(desc: &RelationDesc, user_keys: &[ColumnName]) -> anyhow::Error {
    let user_keys = user_keys.iter().map(|column| column.as_str()).join(", ");

    let existing_keys = desc
        .typ()
        .keys
        .iter()
        .map(|key_columns| {
            key_columns
                .iter()
                .map(|col| desc.get_name(*col).as_str())
                .join(", ")
        })
        .join(", ");

    anyhow!(
        "Key constraint ({}) conflicts with existing key ({})",
        user_keys,
        existing_keys
    )
}

/// Returns only those `CatalogItem`s that don't have any other user
/// dependencies. Those are the root dependencies.
fn get_root_dependencies<'a>(
    scx: &'a StatementContext,
    depends_on: &[GlobalId],
) -> Vec<&'a dyn CatalogItem> {
    let mut result = Vec::new();
    let mut work_queue: Vec<&GlobalId> = Vec::new();
    let mut visited = HashSet::new();
    work_queue.extend(depends_on.iter().filter(|id| id.is_user()));

    while let Some(dep) = work_queue.pop() {
        let item = scx.get_item_by_id(&dep);
        let transitive_uses = item.uses().iter().filter(|id| id.is_user());
        let mut transitive_uses = transitive_uses.peekable();
        if let Some(_) = transitive_uses.peek() {
            for transitive_dep in transitive_uses {
                if visited.insert(transitive_dep) {
                    work_queue.push(transitive_dep);
                }
            }
        } else {
            // no transitive uses, so we must be a root dependency
            result.push(item);
        }
    }
    result
}

pub fn describe_create_index(
    _: &StatementContext,
    _: CreateIndexStatement<Raw>,
) -> Result<StatementDesc, anyhow::Error> {
    Ok(StatementDesc::new(None))
}

pub fn plan_create_index(
    scx: &StatementContext,
    mut stmt: CreateIndexStatement<Raw>,
) -> Result<Plan, anyhow::Error> {
    let CreateIndexStatement {
        name,
        on_name,
        key_parts,
        with_options,
        if_not_exists,
    } = &mut stmt;
    let on = scx.resolve_item(on_name.clone())?;

    if CatalogItemType::View != on.item_type()
        && CatalogItemType::Source != on.item_type()
        && CatalogItemType::Table != on.item_type()
    {
        bail!(
            "index cannot be created on {} because it is a {}",
            on.name(),
            on.item_type()
        )
    }

    let on_desc = on.desc()?;

    let filled_key_parts = match key_parts {
        Some(kp) => kp.to_vec(),
        None => {
            // `key_parts` is None if we're creating a "default" index, i.e.
            // creating the index as if the index had been created alongside the
            // view source, e.g. `CREATE MATERIALIZED...`
            on.desc()?
                .typ()
                .default_key()
                .iter()
                .map(|i| match on_desc.get_unambiguous_name(*i) {
                    Some(n) => Expr::Identifier(vec![Ident::new(n.to_string())]),
                    _ => Expr::Value(Value::Number((i + 1).to_string())),
                })
                .collect()
        }
    };
    let (keys, exprs_depend_on) = query::plan_index_exprs(scx, on_desc, filled_key_parts.clone())?;

    let index_name = if let Some(name) = name {
        FullName {
            database: on.name().database.clone(),
            schema: on.name().schema.clone(),
            item: normalize::ident(name.clone()),
        }
    } else {
        let mut idx_name = on.name().clone();
        if key_parts.is_none() {
            // We're trying to create the "default" index.
            idx_name.item += "_primary_idx";
        } else {
            // Use PG schema for automatically naming indexes:
            // `<table>_<_-separated indexed expressions>_idx`
            let index_name_col_suffix = keys
                .iter()
                .map(|k| match k {
                    mz_expr::MirScalarExpr::Column(i) => match on_desc.get_unambiguous_name(*i) {
                        Some(col_name) => col_name.to_string(),
                        None => format!("{}", i + 1),
                    },
                    _ => "expr".to_string(),
                })
                .join("_");
            idx_name.item += &format!("_{}_idx", index_name_col_suffix);
            idx_name.item = normalize::ident(Ident::new(idx_name.item))
        }
        if !*if_not_exists {
            scx.catalog.find_available_name(idx_name)
        } else {
            idx_name
        }
    };

    let options = plan_index_options(with_options.clone())?;

    // Normalize `stmt`.
    *name = Some(Ident::new(index_name.item.clone()));
    *key_parts = Some(filled_key_parts);
    let if_not_exists = *if_not_exists;
    let create_sql = normalize::create_statement(scx, Statement::CreateIndex(stmt))?;
    let mut depends_on = vec![on.id()];
    depends_on.extend(exprs_depend_on);

    Ok(Plan::CreateIndex(CreateIndexPlan {
        name: index_name,
        index: Index {
            create_sql,
            on: on.id(),
            keys,
            depends_on,
        },
        options,
        if_not_exists,
    }))
}

pub fn describe_create_type(
    _: &StatementContext,
    _: CreateTypeStatement<Raw>,
) -> Result<StatementDesc, anyhow::Error> {
    Ok(StatementDesc::new(None))
}

pub fn plan_create_type(
    scx: &StatementContext,
    stmt: CreateTypeStatement<Raw>,
) -> Result<Plan, anyhow::Error> {
    let create_sql = normalize::create_statement(scx, Statement::CreateType(stmt.clone()))?;
    let CreateTypeStatement {
        name,
        as_type,
        with_options,
    } = stmt;

    let mut with_options = normalize::option_objects(&with_options);

    let option_keys = match as_type {
        CreateTypeAs::List => vec!["element_type"],
        CreateTypeAs::Map => vec!["key_type", "value_type"],
    };

    let mut ids = vec![];
    for key in option_keys {
        let item = match with_options.remove(&key.to_string()) {
            Some(SqlOption::DataType { data_type, .. }) => {
                let (data_type, dt_ids) = resolve_names_data_type(scx, data_type)?;
                ids.extend(dt_ids);
                match data_type {
                    ResolvedDataType::Named {
                        name,
                        id,
                        modifiers,
                        print_id: _,
                    } => {
                        if !modifiers.is_empty() {
                            bail!(
                                "CREATE TYPE ... AS {}option {} cannot accept type modifier on \
                                {}, you must use the default type",
                                as_type.to_string().quoted(),
                                key,
                                name
                            )
                        }
                        scx.catalog.get_item_by_id(&id)
                    }
                    d => bail!(
                        "CREATE TYPE ... AS {}option {} can only use named data types, but \
                        found unnamed data type {}. Use CREATE TYPE to create a named type first",
                        as_type.to_string().quoted(),
                        key,
                        d.to_ast_string(),
                    ),
                }
            }
            Some(_) => bail!("{} must be a data type", key),
            None => bail!("{} parameter required", key),
        };
        match scx.catalog.get_item_by_id(&item.id()).type_details() {
            None => bail!(
                "{} must be of class type, but received {} which is of class {}",
                key,
                item.name(),
                item.item_type()
            ),
            Some(CatalogTypeDetails {
                typ: CatalogType::Char,
                ..
            }) if as_type == CreateTypeAs::List => {
                bail_unsupported!("char list")
            }
            _ => {}
        }
    }

    normalize::ensure_empty_options(&with_options, "CREATE TYPE")?;

    let name = scx.allocate_name(normalize::unresolved_object_name(name)?);
    if scx.catalog.item_exists(&name) {
        bail!("catalog item {} already exists", name.to_string().quoted());
    }

    let inner = match as_type {
        CreateTypeAs::List => CatalogType::List {
            element_id: *ids.get(0).expect("custom type to have element id"),
        },
        CreateTypeAs::Map => {
            let key_id = *ids.get(0).expect("key");
            let entry = scx.catalog.get_item_by_id(&key_id);
            match entry.type_details() {
                Some(CatalogTypeDetails {
                    typ: CatalogType::String,
                    ..
                }) => {}
                Some(_) => bail!("key_type must be text, got {}", entry.name()),
                None => unreachable!("already guaranteed id correlates to a type"),
            }

            CatalogType::Map {
                key_id,
                value_id: *ids.get(1).expect("value"),
            }
        }
    };

    Ok(Plan::CreateType(CreateTypePlan {
        name,
        typ: Type {
            create_sql,
            inner,
            depends_on: ids,
        },
    }))
}

pub fn describe_create_role(
    _: &StatementContext,
    _: CreateRoleStatement,
) -> Result<StatementDesc, anyhow::Error> {
    Ok(StatementDesc::new(None))
}

pub fn plan_create_role(
    _: &StatementContext,
    CreateRoleStatement {
        name,
        is_user,
        options,
    }: CreateRoleStatement,
) -> Result<Plan, anyhow::Error> {
    let mut login = None;
    let mut super_user = None;
    for option in options {
        match option {
            CreateRoleOption::Login | CreateRoleOption::NoLogin if login.is_some() => {
                bail!("conflicting or redundant options");
            }
            CreateRoleOption::SuperUser | CreateRoleOption::NoSuperUser if super_user.is_some() => {
                bail!("conflicting or redundant options");
            }
            CreateRoleOption::Login => login = Some(true),
            CreateRoleOption::NoLogin => login = Some(false),
            CreateRoleOption::SuperUser => super_user = Some(true),
            CreateRoleOption::NoSuperUser => super_user = Some(false),
        }
    }
    if is_user && login.is_none() {
        login = Some(true);
    }
    if login != Some(true) {
        bail_unsupported!("non-login users");
    }
    if super_user != Some(true) {
        bail_unsupported!("non-superusers");
    }
    Ok(Plan::CreateRole(CreateRolePlan {
        name: normalize::ident(name),
    }))
}

pub fn describe_drop_database(
    _: &StatementContext,
    _: DropDatabaseStatement,
) -> Result<StatementDesc, anyhow::Error> {
    Ok(StatementDesc::new(None))
}

pub fn plan_drop_database(
    scx: &StatementContext,
    DropDatabaseStatement {
        name,
        if_exists,
        restrict,
    }: DropDatabaseStatement,
) -> Result<Plan, anyhow::Error> {
    let name = match scx.resolve_database_ident(name) {
        Ok(database) => {
            let name = String::from(database.name());
            if restrict && database.has_schemas() {
                bail!(
                    "database '{}' cannot be dropped with RESTRICT while it contains schemas",
                    database.name(),
                );
            }
            name
        }
        Err(_) if if_exists => {
            // TODO(benesch): generate a notice indicating that the database
            // does not exist.
            //
            // TODO(benesch): adjust the type here so we can more clearly
            // indicate that we don't want to drop any database at all.
            String::new()
        }
        Err(err) => return Err(err.into()),
    };
    Ok(Plan::DropDatabase(DropDatabasePlan { name }))
}

pub fn describe_drop_objects(
    _: &StatementContext,
    _: DropObjectsStatement,
) -> Result<StatementDesc, anyhow::Error> {
    Ok(StatementDesc::new(None))
}

pub fn plan_drop_objects(
    scx: &StatementContext,
    DropObjectsStatement {
        materialized,
        object_type,
        if_exists,
        names,
        cascade,
    }: DropObjectsStatement,
) -> Result<Plan, anyhow::Error> {
    if materialized {
        bail!(
            "DROP MATERIALIZED {0} is not allowed, use DROP {0}",
            object_type
        );
    }
    match object_type {
        ObjectType::Schema => plan_drop_schema(scx, if_exists, names, cascade),
        ObjectType::Source
        | ObjectType::Table
        | ObjectType::View
        | ObjectType::Index
        | ObjectType::Sink
        | ObjectType::Type => plan_drop_items(scx, object_type, if_exists, names, cascade),
        ObjectType::Role => plan_drop_role(scx, if_exists, names),
        ObjectType::Object => unreachable!("cannot drop generic OBJECT, must provide object type"),
    }
}

pub fn plan_drop_schema(
    scx: &StatementContext,
    if_exists: bool,
    names: Vec<UnresolvedObjectName>,
    cascade: bool,
) -> Result<Plan, anyhow::Error> {
    if names.len() != 1 {
        bail_unsupported!("DROP SCHEMA with multiple schemas");
    }
    match scx.resolve_schema(names.into_element()) {
        Ok(schema) => {
            if let DatabaseSpecifier::Ambient = schema.name().database {
                bail!(
                    "cannot drop schema {} because it is required by the database system",
                    schema.name()
                );
            }
            if !cascade && schema.has_items() {
                bail!(
                    "schema '{}' cannot be dropped without CASCADE while it contains objects",
                    schema.name(),
                );
            }
            Ok(Plan::DropSchema(DropSchemaPlan {
                name: schema.name().clone(),
            }))
        }
        Err(_) if if_exists => {
            // TODO(benesch): generate a notice indicating that the
            // schema does not exist.
            // TODO(benesch): adjust the types here properly, rather than making
            // up a nonexistent database.
            Ok(Plan::DropSchema(DropSchemaPlan {
                name: SchemaName {
                    database: DatabaseSpecifier::Ambient,
                    schema: "noexist".into(),
                },
            }))
        }
        Err(e) => Err(e.into()),
    }
}

pub fn plan_drop_role(
    scx: &StatementContext,
    if_exists: bool,
    names: Vec<UnresolvedObjectName>,
) -> Result<Plan, anyhow::Error> {
    let mut out = vec![];
    for name in names {
        let name = if name.0.len() == 1 {
            normalize::ident(name.0.into_element())
        } else {
            bail!("invalid role name {}", name.to_string().quoted())
        };
        if name == scx.catalog.user() {
            bail!("current user cannot be dropped");
        }
        match scx.catalog.resolve_role(&name) {
            Ok(_) => out.push(name),
            Err(_) if if_exists => {
                // TODO(benesch): generate a notice indicating that the
                // role does not exist.
            }
            Err(e) => return Err(e.into()),
        }
    }
    Ok(Plan::DropRoles(DropRolesPlan { names: out }))
}

pub fn plan_drop_items(
    scx: &StatementContext,
    object_type: ObjectType,
    if_exists: bool,
    names: Vec<UnresolvedObjectName>,
    cascade: bool,
) -> Result<Plan, anyhow::Error> {
    let items = names
        .into_iter()
        .map(|n| scx.resolve_item(n))
        .collect::<Vec<_>>();
    let mut ids = vec![];
    for item in items {
        match item {
            Ok(item) => ids.extend(plan_drop_item(scx, object_type, item, cascade)?),
            Err(_) if if_exists => {
                // TODO(benesch): generate a notice indicating this
                // item does not exist.
            }
            Err(err) => return Err(err.into()),
        }
    }
    Ok(Plan::DropItems(DropItemsPlan {
        items: ids,
        ty: object_type,
    }))
}

pub fn plan_drop_item(
    scx: &StatementContext,
    object_type: ObjectType,
    catalog_entry: &dyn CatalogItem,
    cascade: bool,
) -> Result<Option<GlobalId>, anyhow::Error> {
    if catalog_entry.id().is_system() {
        bail!(
            "cannot drop item {} because it is required by the database system",
            catalog_entry.name(),
        );
    }
    if object_type != catalog_entry.item_type() {
        bail!("{} is not of type {}", catalog_entry.name(), object_type);
    }
    if !cascade {
        for id in catalog_entry.used_by() {
            let dep = scx.catalog.get_item_by_id(id);
            match object_type {
                ObjectType::Type => bail!(
                    "cannot drop {}: still depended upon by catalog item '{}'",
                    catalog_entry.name(),
                    dep.name()
                ),
                _ => match dep.item_type() {
                    CatalogItemType::Func
                    | CatalogItemType::Table
                    | CatalogItemType::Source
                    | CatalogItemType::View
                    | CatalogItemType::Sink
                    | CatalogItemType::Type => {
                        bail!(
                            "cannot drop {}: still depended upon by catalog item '{}'",
                            catalog_entry.name(),
                            dep.name()
                        );
                    }
                    CatalogItemType::Index => (),
                },
            }
        }
    }
    Ok(Some(catalog_entry.id()))
}

with_options! {
    struct IndexWithOptions {
        logical_compaction_window: String,
    }
}

pub fn describe_alter_index_options(
    _: &StatementContext,
    _: AlterIndexStatement,
) -> Result<StatementDesc, anyhow::Error> {
    Ok(StatementDesc::new(None))
}

fn plan_index_options(with_opts: Vec<WithOption>) -> Result<Vec<IndexOption>, anyhow::Error> {
    let with_opts = IndexWithOptions::try_from(with_opts)?;
    let mut out = vec![];

    match with_opts.logical_compaction_window.as_deref() {
        None => (),
        Some("off") => out.push(IndexOption::LogicalCompactionWindow(None)),
        Some(s) => {
            let window = Some(mz_repr::util::parse_duration(s)?);
            out.push(IndexOption::LogicalCompactionWindow(window))
        }
    };

    Ok(out)
}

pub fn plan_alter_index_options(
    scx: &StatementContext,
    AlterIndexStatement {
        index_name,
        if_exists,
        action: actions,
    }: AlterIndexStatement,
) -> Result<Plan, anyhow::Error> {
    let entry = match scx.resolve_item(index_name) {
        Ok(index) => index,
        Err(_) if if_exists => {
            // TODO(benesch): generate a notice indicating this index does not
            // exist.
            return Ok(Plan::AlterNoop(AlterNoopPlan {
                object_type: ObjectType::Index,
            }));
        }
        Err(e) => return Err(e.into()),
    };
    if entry.item_type() != CatalogItemType::Index {
        bail!("{} is a {} not a index", entry.name(), entry.item_type())
    }
    let id = entry.id();

    match actions {
        AlterIndexAction::ResetOptions(options) => {
            let options = options
                .into_iter()
                .filter_map(|o| match normalize::ident(o).as_str() {
                    "logical_compaction_window" => Some(IndexOptionName::LogicalCompactionWindow),
                    // Follow Postgres and don't complain if unknown parameters
                    // are passed into `ALTER INDEX ... RESET`.
                    _ => None,
                })
                .collect();
            Ok(Plan::AlterIndexResetOptions(AlterIndexResetOptionsPlan {
                id,
                options,
            }))
        }
        AlterIndexAction::SetOptions(options) => {
            let options = plan_index_options(options)?;
            Ok(Plan::AlterIndexSetOptions(AlterIndexSetOptionsPlan {
                id,
                options,
            }))
        }
        AlterIndexAction::Enable => Ok(Plan::AlterIndexEnable(AlterIndexEnablePlan { id })),
    }
}

pub fn describe_alter_object_rename(
    _: &StatementContext,
    _: AlterObjectRenameStatement,
) -> Result<StatementDesc, anyhow::Error> {
    Ok(StatementDesc::new(None))
}

pub fn plan_alter_object_rename(
    scx: &StatementContext,
    AlterObjectRenameStatement {
        name,
        object_type,
        if_exists,
        to_item_name,
    }: AlterObjectRenameStatement,
) -> Result<Plan, anyhow::Error> {
    let id = match scx.resolve_item(name.clone()) {
        Ok(entry) => {
            if entry.item_type() != object_type {
                bail!("{} is a {} not a {}", name, entry.item_type(), object_type)
            }
            let mut proposed_name = name.0;
            let last = proposed_name.last_mut().unwrap();
            *last = to_item_name.clone();
            if scx
                .resolve_item(UnresolvedObjectName(proposed_name))
                .is_ok()
            {
                bail!("{} is already taken by item in schema", to_item_name)
            }
            entry.id()
        }
        Err(_) if if_exists => {
            // TODO(benesch): generate a notice indicating this
            // item does not exist.
            return Ok(Plan::AlterNoop(AlterNoopPlan { object_type }));
        }
        Err(err) => return Err(err.into()),
    };

    Ok(Plan::AlterItemRename(AlterItemRenamePlan {
        id,
        to_name: normalize::ident(to_item_name),
        object_type,
    }))
}
