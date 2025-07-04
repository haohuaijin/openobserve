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

use proto::cluster_rpc;

/// Supported inverted index formats:
///  - Parquet (v2): Index is stored in parquet format
///  - Tantivy (v3): Index is stored in custom puffin format files
///  - both: Use both Parquet and Tantivy. Note that this will generate two inverted index files.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Default)]
pub enum InvertedIndexFormat {
    #[default]
    Parquet,
    Both,
    Tantivy,
}

impl From<&String> for InvertedIndexFormat {
    fn from(s: &String) -> Self {
        match s.to_lowercase().as_str() {
            "both" => InvertedIndexFormat::Both,
            "tantivy" => InvertedIndexFormat::Tantivy,
            _ => InvertedIndexFormat::Parquet,
        }
    }
}

impl std::fmt::Display for InvertedIndexFormat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            InvertedIndexFormat::Parquet => write!(f, "parquet"),
            InvertedIndexFormat::Both => write!(f, "both"),
            InvertedIndexFormat::Tantivy => write!(f, "tantivy"),
        }
    }
}

pub enum InvertedIndexTantivyMode {
    Puffin,
    Mmap,
}

impl std::fmt::Display for InvertedIndexTantivyMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            InvertedIndexTantivyMode::Puffin => write!(f, "puffin"),
            InvertedIndexTantivyMode::Mmap => write!(f, "mmap"),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum InvertedIndexOptimizeMode {
    SimpleSelect(usize, bool),
    SimpleCount,
    SimpleHistogram(i64, u64, usize),
}

impl std::fmt::Display for InvertedIndexOptimizeMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            InvertedIndexOptimizeMode::SimpleSelect(limit, ascend) => {
                write!(f, "simple_select(limit: {limit}, ascend: {ascend})")
            }
            InvertedIndexOptimizeMode::SimpleCount => write!(f, "simple_count"),
            InvertedIndexOptimizeMode::SimpleHistogram(min_value, bucket_width, num_buckets) => {
                write!(
                    f,
                    "simple_histogram(min_value: {min_value}, bucket_width: {bucket_width}, num_buckets: {num_buckets})"
                )
            }
        }
    }
}

impl From<cluster_rpc::IdxOptimizeMode> for InvertedIndexOptimizeMode {
    fn from(cluster_rpc_mode: cluster_rpc::IdxOptimizeMode) -> Self {
        match cluster_rpc_mode.mode {
            Some(cluster_rpc::idx_optimize_mode::Mode::SimpleSelect(select)) => {
                InvertedIndexOptimizeMode::SimpleSelect(select.index as usize, select.asc)
            }
            Some(cluster_rpc::idx_optimize_mode::Mode::SimpleCount(_)) => {
                InvertedIndexOptimizeMode::SimpleCount
            }
            Some(cluster_rpc::idx_optimize_mode::Mode::SimpleHistogram(select)) => {
                InvertedIndexOptimizeMode::SimpleHistogram(
                    select.min_value,
                    select.bucket_width,
                    select.num_buckets as usize,
                )
            }
            None => panic!("Invalid InvertedIndexOptimizeMode"),
        }
    }
}

impl From<InvertedIndexOptimizeMode> for cluster_rpc::IdxOptimizeMode {
    fn from(mode: InvertedIndexOptimizeMode) -> Self {
        match mode {
            InvertedIndexOptimizeMode::SimpleSelect(index, asc) => cluster_rpc::IdxOptimizeMode {
                mode: Some(cluster_rpc::idx_optimize_mode::Mode::SimpleSelect(
                    cluster_rpc::SimpleSelect {
                        index: index as u32,
                        asc,
                    },
                )),
            },
            InvertedIndexOptimizeMode::SimpleCount => cluster_rpc::IdxOptimizeMode {
                mode: Some(cluster_rpc::idx_optimize_mode::Mode::SimpleCount(
                    cluster_rpc::SimpleCount {},
                )),
            },
            InvertedIndexOptimizeMode::SimpleHistogram(min_value, bucket_width, num_buckets) => {
                cluster_rpc::IdxOptimizeMode {
                    mode: Some(cluster_rpc::idx_optimize_mode::Mode::SimpleHistogram(
                        cluster_rpc::SimpleHistogram {
                            min_value,
                            bucket_width,
                            num_buckets: num_buckets as u32,
                        },
                    )),
                }
            }
        }
    }
}
