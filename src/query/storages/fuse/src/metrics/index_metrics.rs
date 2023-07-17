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

use metrics::increment_gauge;

macro_rules! agg_index_key {
    ($key: literal) => {
        concat!("fuse_agg_index_", $key)
    };
}

pub fn metrics_inc_agg_index_write_nums(c: u64) {
    increment_gauge!(agg_index_key!("write_nums"), c as f64);
}

pub fn metrics_inc_agg_index_write_bytes(c: u64) {
    increment_gauge!(agg_index_key!("write_bytes"), c as f64);
}

pub fn metrics_inc_agg_index_write_milliseconds(c: u64) {
    increment_gauge!(agg_index_key!("write_milliseconds"), c as f64);
}