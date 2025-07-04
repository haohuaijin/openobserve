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

use config::{
    get_config,
    meta::stream::{ALL_STREAM_TYPES, PartitionTimeLevel},
    utils::time::{BASE_TIME, now_micros},
};
use infra::file_list as infra_file_list;

use crate::service::{compact::stats::update_stats_from_file_list, db};

pub async fn run(from: &str, to: &str) -> Result<(), anyhow::Error> {
    // check wal dir
    std::fs::create_dir_all(&get_config().common.data_wal_dir).expect("create wal dir success");

    // load stream list
    let src: Box<dyn infra_file_list::FileList> = match from.to_lowercase().as_str().trim() {
        "sqlite" => Box::<infra_file_list::sqlite::SqliteFileList>::default(),
        "mysql" => Box::<infra_file_list::mysql::MysqlFileList>::default(),
        "postgres" | "postgresql" => Box::<infra_file_list::postgres::PostgresFileList>::default(),
        _ => panic!("invalid source"),
    };

    let dest: Box<dyn infra_file_list::FileList> = match to.to_lowercase().as_str().trim() {
        "sqlite" => Box::<infra_file_list::sqlite::SqliteFileList>::default(),
        "mysql" => Box::<infra_file_list::mysql::MysqlFileList>::default(),
        "postgres" | "postgresql" => Box::<infra_file_list::postgres::PostgresFileList>::default(),
        _ => panic!("invalid destination"),
    };
    dest.create_table().await?;
    db::schema::cache().await?;

    // load stream list
    let start_time = BASE_TIME.timestamp_micros();
    let end_time = now_micros();
    let orgs = db::schema::list_organizations_from_cache().await;
    for org_id in orgs.iter() {
        for stream_type in ALL_STREAM_TYPES {
            let streams = db::schema::list_streams_from_cache(org_id, stream_type).await;
            for stream_name in streams.iter() {
                // load file_list from source
                let files = src
                    .query(
                        org_id,
                        stream_type,
                        stream_name,
                        PartitionTimeLevel::Unset,
                        Some((start_time, end_time)),
                        None,
                    )
                    .await
                    .expect("load file_list failed");
                dest.batch_add(&files)
                    .await
                    .expect("load list_list into db failed");
            }
        }

        // load file_list_deleted from source
        let files = src
            .query_deleted(org_id, end_time, 1_000_000)
            .await
            .expect("load file_list_deleted failed");
        if !files.is_empty()
            && let Err(e) = dest.batch_add_deleted(org_id, end_time, &files).await
        {
            log::error!("load file_list_deleted into db err: {e}");
            continue;
        }
    }

    // update stream stats
    update_stats_from_file_list()
        .await
        .expect("file list migration stats failed");
    Ok(())
}
