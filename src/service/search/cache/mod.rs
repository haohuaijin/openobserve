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

use std::str::FromStr;

use chrono::{TimeZone, Utc};
use config::{
    TIMESTAMP_COL_NAME,
    cluster::LOCAL_NODE,
    get_config,
    meta::{
        search::{self, ResponseTook},
        self_reporting::usage::{RequestStats, UsageType},
        sql::resolve_stream_names,
        stream::StreamType,
    },
    utils::{base64, hash::Sum64, json, sql::is_aggregate_query, time::format_duration},
};
use infra::{
    cache::{file_data::disk::QUERY_RESULT_CACHE, meta::ResultCacheMeta},
    errors::Error,
};
use proto::cluster_rpc::SearchQuery;
use result_utils::get_ts_value;
use tracing::Instrument;

use crate::{
    common::{
        meta::search::{CachedQueryResponse, MultiCachedQueryResponse, QueryDelta},
        utils::{functions, http::get_work_group},
    },
    service::{
        search::{
            self as SearchService,
            cache::cacher::check_cache,
            inspector::{SearchInspectorFieldsBuilder, search_inspector_fields},
        },
        self_reporting::{http_report_metrics, report_request_usage_stats},
    },
};

pub mod cacher;
pub mod multi;
pub mod result_utils;

#[tracing::instrument(name = "service:search:cacher:search", skip_all)]
pub async fn search(
    trace_id: &str,
    org_id: &str,
    stream_type: StreamType,
    user_id: Option<String>,
    in_req: &search::Request,
    range_error: String,
    is_http2_streaming: bool,
) -> Result<search::Response, Error> {
    let start = std::time::Instant::now();
    let started_at = Utc::now().timestamp_micros();
    let cfg = get_config();
    // result cache can be enable only when its from the start
    let use_cache = if in_req.query.from == 0 {
        in_req.use_cache
    } else {
        false
    };

    // Result caching check start
    let mut origin_sql = in_req.query.sql.clone();
    origin_sql = origin_sql.replace('\n', " ");
    let is_aggregate = is_aggregate_query(&origin_sql).unwrap_or_default();
    let (stream_name, all_streams) = match resolve_stream_names(&origin_sql) {
        // TODO: cache don't not support multiple stream names
        Ok(v) => (v[0].clone(), v.join(",")),
        Err(e) => {
            return Err(Error::Message(e.to_string()));
        }
    };

    let mut req = in_req.clone();
    let mut query_fn = req
        .query
        .query_fn
        .as_ref()
        .map(|v| match base64::decode_url(v) {
            Ok(v) => v,
            Err(_) => v.to_string(),
        });
    let action = req
        .query
        .action_id
        .as_ref()
        .and_then(|v| svix_ksuid::Ksuid::from_str(v).ok());

    // calculate hash for the query
    let mut hash_body = vec![origin_sql.to_string()];
    if let Some(vrl_function) = &query_fn {
        hash_body.push(vrl_function.to_string());
    }
    if let Some(action_id) = action {
        hash_body.push(action_id.to_string());
    }
    if !req.regions.is_empty() {
        hash_body.extend(req.regions.clone());
    }
    if !req.clusters.is_empty() {
        hash_body.extend(req.clusters.clone());
    }
    let mut h = config::utils::hash::gxhash::new();
    let hashed_query = h.sum64(&hash_body.join(","));

    let mut should_exec_query = true;

    let mut file_path = format!("{org_id}/{stream_type}/{stream_name}/{hashed_query}");
    let mut c_resp: MultiCachedQueryResponse = if use_cache {
        // cache layer
        check_cache(
            trace_id,
            org_id,
            stream_type,
            &mut req,
            &mut origin_sql,
            &mut file_path,
            is_aggregate,
            &mut should_exec_query,
        )
        .await
    } else {
        let query: SearchQuery = req.query.clone().into();
        match crate::service::search::Sql::new(&query, org_id, stream_type, req.search_type).await {
            Ok(v) => {
                let (ts_column, is_descending) =
                    cacher::get_ts_col_order_by(&v, TIMESTAMP_COL_NAME, is_aggregate)
                        .unwrap_or_default();

                MultiCachedQueryResponse {
                    ts_column,
                    is_descending,
                    ..Default::default()
                }
            }
            Err(e) => {
                log::error!("Error parsing sql: {e}");
                MultiCachedQueryResponse::default()
            }
        }
    };

    // No cache data present, add delta for full query
    if !c_resp.has_cached_data && c_resp.deltas.is_empty() {
        c_resp.deltas.push(QueryDelta {
            delta_start_time: req.query.start_time,
            delta_end_time: req.query.end_time,
            delta_removed_hits: false,
        });
    } else if use_cache {
        log::info!(
            "[trace_id {trace_id}] Query deltas are: {:?}",
            c_resp.deltas
        );
    }

    let search_role = "cache".to_string();

    // Result caching check ends, start search
    let cache_took = start.elapsed().as_millis() as usize;
    let mut results = Vec::new();
    let mut work_group_set = Vec::new();
    let mut res = if !should_exec_query {
        merge_response(
            trace_id,
            &mut c_resp
                .cached_response
                .iter()
                .map(|r| r.cached_response.clone())
                .collect(),
            &mut vec![],
            &c_resp.ts_column,
            c_resp.limit,
            c_resp.is_descending,
            c_resp.took,
        )
    } else {
        if let Some(vrl_function) = &query_fn
            && !vrl_function.trim().ends_with('.')
        {
            query_fn = Some(format!("{vrl_function} \n ."));
        }
        req.query.query_fn = query_fn;

        for fn_name in functions::get_all_transform_keys(org_id).await {
            if req.query.sql.contains(&format!("{fn_name}(")) {
                req.query.uses_zo_fn = true;
                break;
            }
        }

        c_resp.deltas.sort();
        c_resp.deltas.dedup();
        let total = (req.query.end_time - req.query.start_time) as usize;
        let deltas_total: usize = c_resp
            .deltas
            .iter()
            .map(|d| (d.delta_end_time - d.delta_start_time) as usize)
            .sum();

        log::info!(
            "{}",
            search_inspector_fields(
                format!("[trace_id {trace_id}] deltas are : {:?}", c_resp.deltas),
                SearchInspectorFieldsBuilder::new()
                    .node_name(LOCAL_NODE.name.clone())
                    .component("cacher:search deltas".to_string())
                    .search_role(search_role.clone())
                    .duration(start.elapsed().as_millis() as usize)
                    .desc(format!(
                        "search cacher search from {} reduce to {}",
                        format_duration(total as u64 / 1000),
                        format_duration(deltas_total as u64 / 1000)
                    ))
                    .build()
            )
        );
        log::info!(
            "[trace_id {trace_id}] Query original start time: {}, end time : {}",
            req.query.start_time,
            req.query.end_time
        );

        let mut tasks = Vec::new();
        for (i, delta) in c_resp.deltas.into_iter().enumerate() {
            let mut req = req.clone();
            let org_id = org_id.to_string();
            let trace_id = format!("{trace_id}-{i}");
            let user_id = user_id.clone();

            let enter_span = tracing::span::Span::current();
            let task = tokio::task::spawn(
                (async move {
                    let trace_id = trace_id.clone();
                    req.query.start_time = delta.delta_start_time;
                    req.query.end_time = delta.delta_end_time;

                    let cfg = get_config();
                    if cfg.common.result_cache_enabled
                        && cfg.common.print_key_sql
                        && c_resp.has_cached_data
                    {
                        log::info!(
                            "[trace_id {trace_id}] Query new start time: {}, end time : {}",
                            req.query.start_time,
                            req.query.end_time
                        );
                    }

                    SearchService::search(&trace_id, &org_id, stream_type, user_id, &req).await
                })
                .instrument(enter_span),
            );
            tasks.push(task);
        }

        for task in tasks {
            results.push(task.await.map_err(|e| Error::Message(e.to_string()))??);
        }
        for res in &results {
            work_group_set.push(res.work_group.clone());
        }
        if c_resp.has_cached_data {
            merge_response(
                trace_id,
                &mut c_resp
                    .cached_response
                    .iter()
                    .map(|r| r.cached_response.clone())
                    .collect(),
                &mut results,
                &c_resp.ts_column,
                c_resp.limit,
                c_resp.is_descending,
                c_resp.took,
            )
        } else {
            let mut reps = results[0].clone();
            sort_response(c_resp.is_descending, &mut reps, &c_resp.ts_column);
            reps
        }
    };

    // do search
    let took_time = start.elapsed().as_secs_f64();
    log::info!(
        "{}",
        search_inspector_fields(
            format!("[trace_id {trace_id}] cache done"),
            SearchInspectorFieldsBuilder::new()
                .node_name(LOCAL_NODE.name.clone())
                .component("summary".to_string())
                .search_role(search_role)
                .sql(req.query.sql.clone())
                .time_range((
                    req.query.start_time.to_string(),
                    req.query.end_time.to_string()
                ))
                .duration(start.elapsed().as_millis() as usize)
                .build()
        )
    );

    let work_group = get_work_group(work_group_set);

    let search_type = req
        .search_type
        .map(|t| t.to_string())
        .unwrap_or("".to_string());
    let search_group = work_group.clone().unwrap_or("".to_string());
    http_report_metrics(
        start,
        org_id,
        stream_type,
        "200",
        "_search",
        &search_type,
        &search_group,
    );
    res.set_trace_id(trace_id.to_string());
    res.set_took(took_time as usize);
    res.set_cache_took(cache_took);

    if is_aggregate
        && res.histogram_interval.is_none()
        && !c_resp.ts_column.is_empty()
        && c_resp.histogram_interval > -1
    {
        res.histogram_interval = Some(c_resp.histogram_interval);
    }

    let num_fn = req.query.query_fn.is_some() as u16;
    let req_stats = RequestStats {
        records: res.hits.len() as i64,
        response_time: took_time,
        size: res.scan_size as f64,
        request_body: Some(req.query.sql),
        function: req.query.query_fn,
        user_email: user_id,
        min_ts: Some(req.query.start_time),
        max_ts: Some(req.query.end_time),
        cached_ratio: Some(res.cached_ratio),
        search_type: req.search_type,
        search_event_context: req.search_event_context.clone(),
        trace_id: Some(trace_id.to_string()),
        took_wait_in_queue: Some(res.took_detail.wait_in_queue),
        work_group,
        result_cache_ratio: Some(res.result_cache_ratio),
        ..Default::default()
    };
    report_request_usage_stats(
        req_stats,
        org_id,
        &all_streams,
        stream_type,
        UsageType::Search,
        num_fn,
        started_at,
    )
    .await;

    if res.is_partial {
        let partial_err = "Please be aware that the response is based on partial data";
        res.function_error = if res.function_error.is_empty() {
            vec![partial_err.to_string()]
        } else {
            // check if the error is about the stream not found
            let mut skip_warning = false;
            for err in &res.function_error {
                if err.starts_with("Stream not found") {
                    skip_warning = true;
                    break;
                }
            }
            if !skip_warning {
                res.function_error.push(partial_err.to_string());
            }
            res.function_error
        }
    }
    if !range_error.is_empty() {
        res.is_partial = true;
        let range_error_str = range_error.clone();
        res.function_error = if res.function_error.is_empty() {
            vec![range_error_str]
        } else {
            res.function_error.push(range_error_str);
            res.function_error
        };
        res.new_start_time = Some(req.query.start_time);
        res.new_end_time = Some(req.query.end_time);
    }

    // There are 3 types of partial responses:
    // 1. VRL error
    // 2. Super cluster error
    // 3. Range error (max_query_limit)

    // let should_cache_results =
    //     res.new_start_time.is_some() || res.new_end_time.is_some() ||
    // res.function_error.is_empty();

    // Cache partial results only if there is a range error
    // if !res.function_error.is_empty() && !range_error.is_empty() {
    //     res.function_error.retain(|err| !err.contains(&range_error));
    //     should_cache_results = should_cache_results && res.function_error.is_empty();
    // }

    // Update: Don't cache any partial results
    let should_cache_results = res.new_start_time.is_none()
        && res.new_end_time.is_none()
        && res.function_error.is_empty()
        && !res.hits.is_empty();

    // result cache save changes start
    if cfg.common.result_cache_enabled
        && !is_http2_streaming
        && should_exec_query
        && c_resp.cache_query_response
        && should_cache_results
        && (results.first().is_some_and(|res| !res.hits.is_empty())
            || results.last().is_some_and(|res| !res.hits.is_empty()))
    {
        write_results_v2(
            trace_id,
            &c_resp.ts_column,
            req.query.start_time,
            req.query.end_time,
            &res,
            file_path,
            is_aggregate,
            c_resp.is_descending,
        )
        .await;
    }
    // result cache save changes Ends

    Ok(res)
}

// based on _timestamp of first record in config::meta::search::Response either add it in start
// or end to cache response
#[tracing::instrument(name = "service:search:cache:merge_response", skip_all)]
pub fn merge_response(
    trace_id: &str,
    cache_responses: &mut Vec<config::meta::search::Response>,
    search_response: &mut Vec<config::meta::search::Response>,
    ts_column: &str,
    limit: i64,
    is_descending: bool,
    cache_took: usize,
) -> config::meta::search::Response {
    cache_responses.retain(|res| !res.hits.is_empty());

    search_response.retain(|res| !res.hits.is_empty());

    if cache_responses.is_empty() && search_response.is_empty() {
        return config::meta::search::Response::default();
    }
    let mut fn_error = vec![];

    let mut cache_response = if cache_responses.is_empty() {
        config::meta::search::Response::default()
    } else {
        let mut resp = config::meta::search::Response::default();
        for res in cache_responses {
            resp.total += res.total;
            resp.scan_size += res.scan_size;

            resp.scan_records += res.scan_records;

            if res.hits.is_empty() {
                continue;
            }
            resp.hits.extend(res.hits.clone());
            resp.histogram_interval = res.histogram_interval;
            if !res.function_error.is_empty() {
                fn_error.extend(res.function_error.clone());
            }
        }
        resp.took = cache_took;
        resp
    };

    if cache_response.hits.is_empty()
        && !search_response.is_empty()
        && search_response
            .first()
            .is_none_or(|res| res.hits.is_empty())
        && search_response.last().is_none_or(|res| res.hits.is_empty())
    {
        for res in search_response {
            cache_response.total += res.total;
            cache_response.scan_size += res.scan_size;
            cache_response.took += res.took;
            cache_response.histogram_interval = res.histogram_interval;
            if !res.function_error.is_empty() {
                fn_error.extend(res.function_error.clone());
            }
        }
        cache_response.function_error = fn_error;
        return cache_response;
    }
    let cache_hits_len = cache_response.hits.len();

    cache_response.scan_size = 0;

    let mut files_cache_ratio = 0;
    let mut result_cache_len = 0;

    let mut res_took = ResponseTook::default();

    for res in search_response.clone() {
        cache_response.total += res.total;
        cache_response.scan_size += res.scan_size;
        cache_response.took += res.took;
        files_cache_ratio += res.cached_ratio;
        cache_response.histogram_interval = res.histogram_interval;

        result_cache_len += res.total;

        if res.hits.is_empty() {
            continue;
        }
        // here the searches in paralles, so we use the max value of the took_detail
        res_took.idx_took = std::cmp::max(res_took.idx_took, res.took_detail.idx_took);
        res_took.wait_in_queue =
            std::cmp::max(res_took.wait_in_queue, res.took_detail.wait_in_queue);
        res_took.search_took = std::cmp::max(res_took.search_took, res.took_detail.search_took);
        res_took.file_list_took =
            std::cmp::max(res_took.file_list_took, res.took_detail.file_list_took);
        if !res.function_error.is_empty() {
            fn_error.extend(res.function_error.clone());
        }

        cache_response.hits.extend(res.hits.clone());
    }
    sort_response(is_descending, &mut cache_response, ts_column);

    if cache_response.hits.len() > (limit as usize) {
        cache_response.hits.truncate(limit as usize);
    }
    if limit > 0 {
        cache_response.total = cache_response.hits.len();
    }

    if !search_response.is_empty() {
        cache_response.cached_ratio = files_cache_ratio / search_response.len();
    }
    cache_response.size = cache_response.hits.len() as i64;
    log::info!(
        "[trace_id {trace_id}] cache_response.hits.len: {}, Result cache len: {}",
        cache_hits_len,
        result_cache_len
    );
    cache_response.took_detail = res_took;
    cache_response.order_by = search_response.first().and_then(|res| res.order_by);
    cache_response.result_cache_ratio = (((cache_hits_len as f64) * 100_f64)
        / ((result_cache_len + cache_hits_len) as f64))
        as usize;
    if !fn_error.is_empty() {
        cache_response.function_error.extend(fn_error);
    }
    cache_response
}

fn sort_response(is_descending: bool, cache_response: &mut search::Response, ts_column: &str) {
    if is_descending {
        cache_response
            .hits
            .sort_by_key(|b| std::cmp::Reverse(get_ts_value(ts_column, b)));
    } else {
        cache_response
            .hits
            .sort_by_key(|a| get_ts_value(ts_column, a));
    }
}

#[allow(clippy::too_many_arguments, unused_variables)]
pub async fn _write_results(
    trace_id: &str,
    ts_column: &str,
    req_query_start_time: i64,
    req_query_end_time: i64,
    res: &config::meta::search::Response,
    file_path: String,
    is_aggregate: bool,
    is_descending: bool,
) {
    // disable write_results_v1
    // return;
    // #[allow(unreachable_code)]
    let mut local_resp = res.clone();
    let remove_hit = if is_descending {
        local_resp.hits.last()
    } else {
        local_resp.hits.first()
    };

    if !local_resp.hits.is_empty()
        && let Some(remove_hit) = remove_hit
    {
        let ts_value_to_remove = remove_hit.get(ts_column).cloned();

        if let Some(ts_value) = ts_value_to_remove {
            local_resp
                .hits
                .retain(|hit| hit.get(ts_column) != Some(&ts_value));
        }
    }

    if local_resp.hits.is_empty() || local_resp.hits.len() < 2 {
        return;
    }

    let last_rec_ts = get_ts_value(ts_column, local_resp.hits.last().unwrap());
    let first_rec_ts = get_ts_value(ts_column, local_resp.hits.first().unwrap());

    let smallest_ts = std::cmp::min(first_rec_ts, last_rec_ts);
    let discard_duration = get_config().common.result_cache_discard_duration * 1000 * 1000;

    if (last_rec_ts - first_rec_ts).abs() < discard_duration
        && smallest_ts > Utc::now().timestamp_micros() - discard_duration
    {
        return;
    }

    let largest_ts = std::cmp::max(first_rec_ts, last_rec_ts);

    let cache_end_time = if largest_ts > 0 && largest_ts < req_query_end_time {
        largest_ts
    } else {
        req_query_end_time
    };

    let cache_start_time = if smallest_ts > 0 && smallest_ts > req_query_start_time {
        smallest_ts
    } else {
        req_query_start_time
    };

    let file_name = format!(
        "{}_{}_{}_{}.json",
        cache_start_time,
        cache_end_time,
        if is_aggregate { 1 } else { 0 },
        if is_descending { 1 } else { 0 }
    );

    let res_cache = json::to_string(&local_resp).unwrap();
    let query_key = file_path.replace('/', "_");
    let trace_id = trace_id.to_string();
    tokio::spawn(async move {
        let file_path_local = file_path.clone();

        match SearchService::cache::cacher::cache_results_to_disk(
            &file_path_local,
            &file_name,
            res_cache,
        )
        .await
        {
            Ok(_) => {
                let mut w = QUERY_RESULT_CACHE.write().await;
                w.entry(query_key)
                    .or_insert_with(Vec::new)
                    .push(ResultCacheMeta {
                        start_time: cache_start_time,
                        end_time: cache_end_time,
                        is_aggregate,
                        is_descending,
                    });
                drop(w);
            }
            Err(e) => {
                log::error!("Cache results to disk failed: {e}");
            }
        }
    });
}

/// Caches search results to disk after applying filtering and validation strategies.
///
/// # Caching Strategy
/// 1. **Select Hit for Deduplication**:
///    - Selects either the first or last record based on the `is_descending` flag.
///      - `is_descending = true`: Selects the last record.
///      - `is_descending = false`: Selects the first record.
///
/// 2. **Remove All Hits Within the Same Date, Hour, Minute, and Second for Deduplication**:
///    - Identifies the timestamp of the hit to remove (based on the `is_descending` flag, either
///      the first or the last record is chosen).
///    - Removes all hits that match the exact same date, hour, minute, and second
///      (`YYYY-MM-DDTHH:MM:SS`) as the identified hit.
///      - Example: If the timestamp to remove is `2024-12-06T04:15:30`, only hits with the exact
///        timestamp `2024-12-06T04:15:30` are removed, while others with different seconds (e.g.,
///        `2024-12-06T04:15:29` or `2024-12-06T04:15:31`) are retained.
///
/// 3. **Skip Caching for Empty or Insufficient Hits**:
///    - If no hits remain after removing one, caching is skipped.
///    - Logs a message: `"No hits found for caching, skipping caching."`
///
/// 4. **Discard Short Time Ranges**:
///    - Skips caching if the difference between the first and last record timestamps is smaller
///      than the configured `discard_duration`.
///
/// 5. **Adjust Cache Time Range**:
///    - Adjusts the cache start and end times based on the smallest and largest timestamps:
///      - `start_time = max(smallest_ts, req_query_start_time)`
///      - `end_time = min(largest_ts, req_query_end_time)`
///
/// 6. **Cache to Disk**:
///    - Saves the filtered response to a file named:
///      `"<start_time>_<end_time>_<is_aggregate>_<is_descending>.json"`.
#[tracing::instrument(name = "service:search:cache:write_results_v2", skip_all)]
#[allow(clippy::too_many_arguments)]
pub async fn write_results_v2(
    trace_id: &str,
    ts_column: &str,
    req_query_start_time: i64,
    req_query_end_time: i64,
    res: &config::meta::search::Response,
    file_path: String,
    is_aggregate: bool,
    is_descending: bool,
) {
    let mut local_resp = res.clone();
    let remove_hit = if is_descending {
        local_resp.hits.last()
    } else {
        local_resp.hits.first()
    };

    if !local_resp.hits.is_empty() && remove_hit.is_some() {
        let ts_value_to_remove = remove_hit.unwrap().get(ts_column).cloned();

        if let Some(ts_value) = ts_value_to_remove {
            // Extract the date, hour, minute from the timestamp
            if let Some(ts_datetime) = convert_ts_value_to_datetime(&ts_value) {
                // Extract the target date, hour, minute, and second (e.g., "2024-12-06T04:15:23")
                let target_date_hour_minute_second =
                    ts_datetime.format("%Y-%m-%dT%H:%M:%S").to_string();

                // Retain only the hits that do NOT fall within the
                // same date, hour, minute as the hit to remove
                local_resp.hits.retain(|hit| {
                    if let Some(hit_ts) = hit.get(ts_column)
                        && let Some(hit_ts_datetime) = convert_ts_value_to_datetime(hit_ts)
                    {
                        // Extract the date, hour, minute, and second for the current hit
                        let hit_date_hour_minute_second =
                            hit_ts_datetime.format("%Y-%m-%dT%H:%M:%S").to_string();
                        return hit_date_hour_minute_second != target_date_hour_minute_second;
                    }
                    false
                });
            }
        }

        local_resp.total = local_resp.hits.len();
        local_resp.size = local_resp.hits.len() as i64;
    }

    if local_resp.hits.is_empty() {
        log::info!(
            "[trace_id {trace_id}] No hits found for caching, skipping caching",
            trace_id = trace_id
        );
        return;
    }

    let last_rec_ts = get_ts_value(ts_column, local_resp.hits.last().unwrap());
    let first_rec_ts = get_ts_value(ts_column, local_resp.hits.first().unwrap());

    let smallest_ts = std::cmp::min(first_rec_ts, last_rec_ts);
    let discard_duration = get_config().common.result_cache_discard_duration * 1000 * 1000;

    if (last_rec_ts - first_rec_ts).abs() < discard_duration
        && smallest_ts > Utc::now().timestamp_micros() - discard_duration
    {
        return;
    }

    let largest_ts = std::cmp::max(first_rec_ts, last_rec_ts);

    let cache_end_time = if largest_ts > 0 && largest_ts < req_query_end_time {
        largest_ts
    } else {
        req_query_end_time
    };

    let cache_start_time = if smallest_ts > 0 && smallest_ts > req_query_start_time {
        smallest_ts
    } else {
        req_query_start_time
    };

    let file_name = format!(
        "{}_{}_{}_{}.json",
        cache_start_time,
        cache_end_time,
        if is_aggregate { 1 } else { 0 },
        if is_descending { 1 } else { 0 }
    );

    let res_cache = json::to_string(&local_resp).unwrap();
    let query_key = file_path.replace('/', "_");
    tokio::spawn(async move {
        let file_path_local = file_path.clone();

        match SearchService::cache::cacher::cache_results_to_disk(
            &file_path_local,
            &file_name,
            res_cache,
        )
        .await
        {
            Ok(_) => {
                let mut w = QUERY_RESULT_CACHE.write().await;
                w.entry(query_key)
                    .or_insert_with(Vec::new)
                    .push(ResultCacheMeta {
                        start_time: cache_start_time,
                        end_time: cache_end_time,
                        is_aggregate,
                        is_descending,
                    });
                drop(w);
            }
            Err(e) => {
                log::error!("Cache results to disk failed: {e}");
            }
        }
    });
}

#[tracing::instrument(name = "service:search:cacher:check_cache_v2", skip_all)]
pub async fn check_cache_v2(
    trace_id: &str,
    org_id: &str,
    stream_type: StreamType,
    in_req: &search::Request,
    use_cache: bool,
) -> Result<MultiCachedQueryResponse, Error> {
    // Result caching check start
    let mut origin_sql = in_req.query.sql.clone();
    origin_sql = origin_sql.replace('\n', " ");
    let is_aggregate = is_aggregate_query(&origin_sql).unwrap_or_default();
    let stream_name = match resolve_stream_names(&origin_sql) {
        // TODO: cache don't not support multiple stream names
        Ok(v) => v[0].clone(),
        Err(e) => {
            return Err(Error::Message(e.to_string()));
        }
    };

    let mut req = in_req.clone();
    let query_fn = req
        .query
        .query_fn
        .as_ref()
        .and_then(|v| base64::decode_url(v).ok());

    // calculate hash for the query
    let mut hash_body = vec![origin_sql.to_string()];
    if let Some(vrl_function) = &query_fn {
        hash_body.push(vrl_function.to_string());
    }
    if !req.regions.is_empty() {
        hash_body.extend(req.regions.clone());
    }
    if !req.clusters.is_empty() {
        hash_body.extend(req.clusters.clone());
    }
    let mut h = config::utils::hash::gxhash::new();
    let hashed_query = h.sum64(&hash_body.join(","));

    let mut should_exec_query = true;

    let mut file_path = format!("{org_id}/{stream_type}/{stream_name}/{hashed_query}");
    Ok(if use_cache {
        let mut resp = check_cache(
            trace_id,
            org_id,
            stream_type,
            &mut req,
            &mut origin_sql,
            &mut file_path,
            is_aggregate,
            &mut should_exec_query,
        )
        .await;
        resp.is_aggregate = is_aggregate;
        resp.trace_id = trace_id.to_string();
        resp.file_path = file_path;
        resp
    } else {
        let query = req.query.into();
        match crate::service::search::Sql::new(&query, org_id, stream_type, req.search_type).await {
            Ok(v) => {
                let (ts_column, is_descending) =
                    cacher::get_ts_col_order_by(&v, TIMESTAMP_COL_NAME, is_aggregate)
                        .unwrap_or_default();

                MultiCachedQueryResponse {
                    ts_column,
                    is_descending,
                    file_path,
                    ..Default::default()
                }
            }
            Err(e) => {
                log::error!("[trace_id {}]: Error parsing sql: {:?}", trace_id, e);
                MultiCachedQueryResponse::default()
            }
        }
    })
}

fn convert_ts_value_to_datetime(ts_value: &serde_json::Value) -> Option<chrono::DateTime<Utc>> {
    match ts_value {
        // Handle the case where ts_value is a number (microseconds)
        serde_json::Value::Number(num) => {
            if let Some(micros) = num.as_i64() {
                // Convert microseconds to DateTime<Utc>
                chrono::DateTime::<Utc>::from_timestamp_micros(micros)
            } else {
                None
            }
        }
        // Handle the case where ts_value is a string (ISO 8601 format)
        serde_json::Value::String(ts_str) => {
            // Parse the string timestamp into a NaiveDateTime
            if let Ok(naive_dt) = chrono::NaiveDateTime::parse_from_str(ts_str, "%Y-%m-%dT%H:%M:%S")
            {
                // Convert NaiveDateTime to DateTime<Utc>
                Some(Utc.from_utc_datetime(&naive_dt))
            } else {
                None
            }
        }
        _ => None,
    }
}
