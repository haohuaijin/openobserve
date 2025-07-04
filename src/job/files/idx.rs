// Copyright 2025 OpenObserve Inc.
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU Affero General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
//
// This program is distributed in the hope that it will be useful
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU Affero General Public License for more details.
//
// You should have received a copy of the GNU Affero General Public License
// along with this program.  If not, see <http://www.gnu.org/licenses/>.

use std::sync::Arc;

use arrow::array::{
    Array, ArrayBuilder, BinaryArray, BinaryBuilder, BooleanArray, BooleanBuilder, Int64Array,
    Int64Builder, RecordBatch, StringArray, StringBuilder, make_builder,
};
use arrow_schema::{DataType, Schema};
use config::{
    FILE_EXT_PARQUET, ider,
    meta::stream::{FileMeta, StreamPartition, StreamPartitionType, StreamType},
    utils::{
        parquet::new_parquet_writer,
        record_batch_ext::{RecordBatchExt, concat_batches},
        schema::format_partition_key,
    },
};
use hashbrown::HashMap;
use infra::storage;

use crate::common::utils::stream::populate_file_meta;

fn generate_index_file_name_from_compacted_file(
    org_id: &str,
    stream_type: StreamType,
    stream_name: &str,
    compacted_file_name: &str,
    prefix: &str,
) -> String {
    // eg: files/default/logs/quickstart1/2024/02/16/16/7164299619311026293.parquet
    let file_columns = compacted_file_name.split('/').collect::<Vec<&str>>();
    let stream_key = format!("{org_id}/{stream_type}/{stream_name}");
    let file_date = format!(
        "{}/{}/{}/{}",
        file_columns[4], file_columns[5], file_columns[6], file_columns[7]
    );
    let file_name = ider::generate();
    if prefix.is_empty() {
        format!("files/{stream_key}/{file_date}/{file_name}{FILE_EXT_PARQUET}")
    } else {
        format!("files/{stream_key}/{file_date}/{prefix}/{file_name}{FILE_EXT_PARQUET}")
    }
}

pub(crate) async fn write_parquet_index_to_disk(
    batches: Vec<arrow::record_batch::RecordBatch>,
    org_id: &str,
    stream_type: StreamType,
    stream_name: &str,
    file_name: &str,
) -> Result<Vec<(String, String, FileMeta)>, anyhow::Error> {
    let start = std::time::Instant::now();
    let schema = if let Some(first_batch) = batches.first() {
        first_batch.schema()
    } else {
        return Err(anyhow::anyhow!("No record batches found"));
    };

    log::debug!(
        "[JOB:IDX] write_parquet_index_to_disk: batches row counts: {:?}",
        batches.iter().map(|b| b.num_rows()).sum::<usize>()
    );

    let stream_settings = infra::schema::get_settings(org_id, stream_name, stream_type)
        .await
        .unwrap_or_default();
    let term_prefix_partition = stream_settings.partition_keys.iter().any(|partition| {
        partition.field == "term"
            && partition.types == StreamPartitionType::Prefix
            && !partition.disabled
    });

    // partition the record batches
    let partitioned_batches = if term_prefix_partition {
        generate_prefixed_batches(schema.clone(), batches)?
    } else {
        let mut partitioned_batches = HashMap::new();
        let batch = concat_batches(schema.clone(), batches)?;
        partitioned_batches.insert("".to_string(), batch);
        partitioned_batches
    };

    let mut ret = Vec::new();
    for (prefix, batch) in partitioned_batches.into_iter() {
        // write metadata
        let batch_size = batch.size();
        let mut file_meta = FileMeta {
            original_size: batch_size as i64,
            ..Default::default()
        };
        populate_file_meta(&[&batch], &mut file_meta, Some("min_ts"), Some("max_ts")).await?;

        // write parquet file
        let mut buf_parquet = Vec::new();
        let bf_fields = vec!["term".to_string()];
        let mut writer = new_parquet_writer(
            &mut buf_parquet,
            &schema,
            &bf_fields,
            &file_meta,
            true,
            None,
        );
        writer.write(&batch).await?;
        writer.close().await?;
        file_meta.compressed_size = buf_parquet.len() as i64;

        let new_idx_file_name = generate_index_file_name_from_compacted_file(
            org_id,
            stream_type,
            stream_name,
            file_name,
            &prefix,
        );

        let buf_size = buf_parquet.len();
        let account = storage::get_account(&new_idx_file_name).unwrap_or_default();
        match storage::put(
            &account,
            &new_idx_file_name,
            bytes::Bytes::from(buf_parquet),
        )
        .await
        {
            Ok(_) => {
                log::info!(
                    "[JOB:IDX] index file upload successfully: {}, size: {}, took: {} ms",
                    &new_idx_file_name,
                    buf_size,
                    start.elapsed().as_millis()
                );
                ret.push((account, new_idx_file_name, file_meta));
            }
            Err(err) => {
                log::error!("[JOB] index file upload error: {:?}", err);
                return Err(anyhow::anyhow!(err));
            }
        }
    }
    Ok(ret)
}

/// Generate prefix batches from record batches
fn generate_prefixed_batches(
    schema: Arc<Schema>,
    batches: Vec<RecordBatch>,
) -> Result<HashMap<String, RecordBatch>, anyhow::Error> {
    // partiton the recordbatch with the partition key
    let partition = StreamPartition::new_prefix("term");
    let mut partition_buf: HashMap<String, HashMap<usize, Box<dyn ArrayBuilder>>> = HashMap::new();
    let term_idx = schema.index_of("term")?;
    for batch in batches {
        let row_count = batch.num_rows();
        let col_term = batch
            .column(term_idx)
            .as_any()
            .downcast_ref::<StringArray>()
            .unwrap();
        for (idx, field) in schema.fields().iter().enumerate() {
            let field_type = field.data_type();
            match field_type {
                DataType::Utf8 => {
                    let col = if idx == term_idx {
                        col_term
                    } else {
                        batch
                            .column(idx)
                            .as_any()
                            .downcast_ref::<StringArray>()
                            .unwrap()
                    };
                    for i in 0..row_count {
                        let term = col_term.value(i);
                        let prefix = if term.is_empty() {
                            String::new()
                        } else {
                            format_partition_key(&partition.get_partition_key(term))
                        };
                        let entry = partition_buf.entry(prefix).or_default();
                        let builder = entry
                            .entry(idx)
                            .or_insert_with(|| make_builder(field_type, row_count));
                        let b = builder
                            .as_any_mut()
                            .downcast_mut::<StringBuilder>()
                            .unwrap();
                        b.append_value(col.value(i));
                    }
                }
                DataType::Int64 => {
                    let col = batch
                        .column(idx)
                        .as_any()
                        .downcast_ref::<Int64Array>()
                        .unwrap();
                    for i in 0..row_count {
                        let term = col_term.value(i);
                        let prefix = if term.is_empty() {
                            String::new()
                        } else {
                            format_partition_key(&partition.get_partition_key(term))
                        };
                        let entry = partition_buf.entry(prefix).or_default();
                        let builder = entry
                            .entry(idx)
                            .or_insert_with(|| make_builder(field_type, row_count));
                        let b = builder.as_any_mut().downcast_mut::<Int64Builder>().unwrap();
                        b.append_value(col.value(i));
                    }
                }
                DataType::Boolean => {
                    let col = batch
                        .column(idx)
                        .as_any()
                        .downcast_ref::<BooleanArray>()
                        .unwrap();
                    for i in 0..row_count {
                        let term = col_term.value(i);
                        let prefix = if term.is_empty() {
                            String::new()
                        } else {
                            format_partition_key(&partition.get_partition_key(term))
                        };
                        let entry = partition_buf.entry(prefix).or_default();
                        let builder = entry
                            .entry(idx)
                            .or_insert_with(|| make_builder(field_type, row_count));
                        let b = builder
                            .as_any_mut()
                            .downcast_mut::<BooleanBuilder>()
                            .unwrap();
                        b.append_value(col.value(i));
                    }
                }
                DataType::Binary => {
                    let col = batch
                        .column(idx)
                        .as_any()
                        .downcast_ref::<BinaryArray>()
                        .unwrap();
                    for i in 0..row_count {
                        let term = col_term.value(i);
                        let prefix = if term.is_empty() {
                            String::new()
                        } else {
                            format_partition_key(&partition.get_partition_key(term))
                        };
                        let entry = partition_buf.entry(prefix).or_default();
                        let builder = entry
                            .entry(idx)
                            .or_insert_with(|| make_builder(field_type, row_count));
                        let b = builder
                            .as_any_mut()
                            .downcast_mut::<BinaryBuilder>()
                            .unwrap();
                        b.append_value(col.value(i));
                    }
                }
                _ => {
                    return Err(anyhow::anyhow!("unsupported data type: {:?}", field_type));
                }
            }
        }
    }

    // append deleted rows to each prefix
    if let Some(deleted_buf) = partition_buf.remove("") {
        for (idx, mut builder) in deleted_buf {
            let field_type = schema.field(idx).data_type();
            match field_type {
                DataType::Utf8 => {
                    let del_b = builder
                        .as_any_mut()
                        .downcast_mut::<StringBuilder>()
                        .unwrap()
                        .finish();
                    for (prefix, entry) in partition_buf.iter_mut() {
                        if prefix.is_empty() {
                            continue;
                        }
                        let builder = entry
                            .entry(idx)
                            .or_insert_with(|| make_builder(field_type, del_b.len()));
                        let b = builder
                            .as_any_mut()
                            .downcast_mut::<StringBuilder>()
                            .unwrap();
                        for i in 0..del_b.len() {
                            b.append_value(del_b.value(i));
                        }
                    }
                }
                DataType::Int64 => {
                    let del_b = builder
                        .as_any_mut()
                        .downcast_mut::<Int64Builder>()
                        .unwrap()
                        .finish();
                    for (prefix, entry) in partition_buf.iter_mut() {
                        if prefix.is_empty() {
                            continue;
                        }
                        let builder = entry
                            .entry(idx)
                            .or_insert_with(|| make_builder(field_type, del_b.len()));
                        let b = builder.as_any_mut().downcast_mut::<Int64Builder>().unwrap();
                        for i in 0..del_b.len() {
                            b.append_value(del_b.value(i));
                        }
                    }
                }
                DataType::Boolean => {
                    let del_b = builder
                        .as_any_mut()
                        .downcast_mut::<BooleanBuilder>()
                        .unwrap()
                        .finish();
                    for (prefix, entry) in partition_buf.iter_mut() {
                        if prefix.is_empty() {
                            continue;
                        }
                        let builder = entry
                            .entry(idx)
                            .or_insert_with(|| make_builder(field_type, del_b.len()));
                        let b = builder
                            .as_any_mut()
                            .downcast_mut::<BooleanBuilder>()
                            .unwrap();
                        for i in 0..del_b.len() {
                            b.append_value(del_b.value(i));
                        }
                    }
                }
                DataType::Binary => {
                    let del_b = builder
                        .as_any_mut()
                        .downcast_mut::<BinaryBuilder>()
                        .unwrap()
                        .finish();
                    for (prefix, entry) in partition_buf.iter_mut() {
                        if prefix.is_empty() {
                            continue;
                        }
                        let builder = entry
                            .entry(idx)
                            .or_insert_with(|| make_builder(field_type, del_b.len()));
                        let b = builder
                            .as_any_mut()
                            .downcast_mut::<BinaryBuilder>()
                            .unwrap();
                        for i in 0..del_b.len() {
                            b.append_value(del_b.value(i));
                        }
                    }
                }
                _ => {
                    return Err(anyhow::anyhow!("unsupported data type: {:?}", field_type));
                }
            }
        }
    }

    // finish the builders
    let mut partitioned_batches = HashMap::with_capacity(partition_buf.len());
    for (prefix, entry) in partition_buf.into_iter() {
        let mut cols = entry
            .into_iter()
            .map(|(idx, mut builder)| (idx, builder.finish()))
            .collect::<Vec<_>>();
        cols.sort_by(|a, b| a.0.cmp(&b.0));
        let cols = cols.into_iter().map(|(_, col)| col).collect::<Vec<_>>();
        let batch = RecordBatch::try_new(schema.clone(), cols)?;
        partitioned_batches.insert(prefix, batch);
    }
    Ok(partitioned_batches)
}
