// Copyright 2022 Datafuse Labs.
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

use common_catalog::table::Table;
use common_catalog::table::TableExt;
use common_catalog::table_context::TableContext;
use common_datablocks::BlockMetaInfos;
use common_exception::ErrorCode;
use common_exception::Result;
use common_storages_table_meta::meta::Location;
use common_storages_table_meta::meta::SegmentInfo;
use common_storages_table_meta::meta::Statistics;
use common_storages_table_meta::meta::TableSnapshot;
use common_storages_table_meta::meta::Versioned;
use itertools::Itertools;
use opendal::Operator;

use super::CompactSinkMeta;
use crate::io::SegmentsIO;
use crate::io::TableMetaLocationGenerator;
use crate::metrics::metrics_inc_commit_mutation_resolvable_conflict;
use crate::metrics::metrics_inc_commit_mutation_retry;
use crate::metrics::metrics_inc_commit_mutation_success;
use crate::metrics::metrics_inc_commit_mutation_unresolvable_conflict;
use crate::operations::commit::Conflict;
use crate::operations::commit::MutatorConflictDetector;
use crate::operations::mutation::AbortOperation;
use crate::pipelines::processors::port::InputPort;
use crate::pipelines::processors::processor::Event;
use crate::pipelines::processors::Processor;
use crate::statistics::reducers::merge_statistics_mut;
use crate::FuseTable;

const MAX_RETRIES: u64 = 10;

enum State {
    None,
    GatherSegment,
    GenerateSnapshot(Vec<Location>),
    RefreshTable,
    DetectConfilct(Arc<TableSnapshot>),
    TryCommit(TableSnapshot),
    AbortOperation,
    Finish,
}

pub struct CompactSink {
    state: State,

    ctx: Arc<dyn TableContext>,
    dal: Operator,
    location_gen: TableMetaLocationGenerator,

    table: Arc<dyn Table>,
    base_snapshot: Arc<TableSnapshot>,
    base_segments: Vec<Location>,
    base_statistics: Statistics,
    indices: Vec<usize>,

    retries: u64,
    abort_operation: AbortOperation,

    inputs: Vec<Arc<InputPort>>,
    input_data: BlockMetaInfos,
    cur_input_index: usize,
}

impl CompactSink {
    fn get_current_input(&mut self) -> Option<Arc<InputPort>> {
        let mut finished = true;
        let mut index = self.cur_input_index;

        loop {
            let input = &self.inputs[index];

            if !input.is_finished() {
                finished = false;
                input.set_need_data();

                if input.has_data() {
                    self.cur_input_index = index;
                    return Some(input.clone());
                }
            }

            index += 1;
            if index == self.inputs.len() {
                index = 0;
            }

            if index == self.cur_input_index {
                return match finished {
                    true => Some(input.clone()),
                    false => None,
                };
            }
        }
    }
}

#[async_trait::async_trait]
impl Processor for CompactSink {
    fn name(&self) -> String {
        "CompactSink".to_string()
    }

    fn as_any(&mut self) -> &mut dyn Any {
        self
    }

    fn event(&mut self) -> Result<Event> {
        if matches!(&self.state, State::GatherSegment | State::DetectConfilct(_)) {
            return Ok(Event::Sync);
        }

        if matches!(
            &self.state,
            State::GenerateSnapshot(_)
                | State::TryCommit(_)
                | State::RefreshTable
                | State::AbortOperation
        ) {
            return Ok(Event::Async);
        }

        if matches!(self.state, State::Finish) {
            return Ok(Event::Finished);
        }

        let current_input = self.get_current_input();
        if let Some(cur_input) = current_input {
            if cur_input.is_finished() {
                self.state = State::GatherSegment;
                return Ok(Event::Sync);
            }
            self.input_data
                .push(cur_input.pull_data().unwrap()?.get_meta().unwrap().clone());
        }
        Ok(Event::NeedData)
    }

    fn process(&mut self) -> Result<()> {
        match std::mem::replace(&mut self.state, State::None) {
            State::GatherSegment => {
                let metas = std::mem::take(&mut self.input_data);
                let mut base_segments = std::mem::take(&mut self.base_segments);
                for v in metas.iter() {
                    let meta = CompactSinkMeta::from_meta(v)?;
                    self.abort_operation.merge(&meta.abort_operation);
                    base_segments.push((meta.segment_location.clone(), SegmentInfo::VERSION));
                    self.indices.push(meta.order);
                    merge_statistics_mut(&mut self.base_statistics, &meta.segment_info.summary)?;
                }

                self.base_segments = base_segments
                    .into_iter()
                    .zip(self.indices.iter())
                    .sorted_by_key(|&(_, r)| *r)
                    .map(|(l, _)| l)
                    .collect();

                self.state = State::GenerateSnapshot(vec![]);
            }
            State::DetectConfilct(latest_snapshot) => {
                // Check if there is only insertion during the operation.
                match MutatorConflictDetector::detect_conflicts(
                    self.base_snapshot.as_ref(),
                    latest_snapshot.as_ref(),
                ) {
                    Conflict::Unresolvable => {
                        // mutation conflicts, concurrent mutation detected while committing segment compaction operation
                        metrics_inc_commit_mutation_unresolvable_conflict();
                        self.state = State::AbortOperation;
                    }
                    Conflict::ResolvableAppend(range_of_newly_append) => {
                        tracing::info!("resolvable conflicts detected");
                        metrics_inc_commit_mutation_resolvable_conflict();

                        self.retries += 1;
                        metrics_inc_commit_mutation_retry();

                        self.state = State::GenerateSnapshot(
                            latest_snapshot.segments[range_of_newly_append].to_owned(),
                        );
                        self.base_snapshot = latest_snapshot;
                    }
                }
            }
            _ => return Err(ErrorCode::Internal("It's a bug.")),
        }
        Ok(())
    }

    async fn async_process(&mut self) -> Result<()> {
        match std::mem::replace(&mut self.state, State::None) {
            State::GenerateSnapshot(appended_segments) => {
                let mut new_snapshot = TableSnapshot::from_previous(&self.base_snapshot);
                if !appended_segments.is_empty() {
                    self.base_segments = appended_segments
                        .iter()
                        .chain(self.base_segments.iter())
                        .cloned()
                        .collect();
                    let segments_io = SegmentsIO::create(self.ctx.clone(), self.dal.clone());
                    let append_segment_infos =
                        segments_io.read_segments(&appended_segments).await?;
                    for result in append_segment_infos.into_iter() {
                        let appended_segment = result?;
                        merge_statistics_mut(&mut self.base_statistics, &appended_segment.summary)?;
                    }
                }
                new_snapshot.segments = self.base_segments.clone();
                new_snapshot.summary = self.base_statistics.clone();
                self.state = State::TryCommit(new_snapshot);
            }
            State::TryCommit(new_snapshot) => {
                let table_info = self.table.get_table_info();
                match FuseTable::commit_to_meta_server(
                    self.ctx.as_ref(),
                    table_info,
                    &self.location_gen,
                    new_snapshot,
                    &self.dal,
                )
                .await
                {
                    Err(e) if e.code() == ErrorCode::TABLE_VERSION_MISMATCHED => {
                        if self.retries < MAX_RETRIES {
                            self.state = State::RefreshTable;
                        } else {
                            tracing::error!(
                                "commit mutation failed after {} retries",
                                self.retries
                            );
                            self.state = State::AbortOperation;
                        }
                    }
                    Err(e) => return Err(e),
                    Ok(_) => {
                        metrics_inc_commit_mutation_success();
                        self.state = State::Finish;
                    }
                };
            }
            State::RefreshTable => {
                let latest_table_ref = self.table.refresh(self.ctx.as_ref()).await?;
                let latest_fuse_table = FuseTable::try_from_table(latest_table_ref.as_ref())?;
                let latest_snapshot =
                    latest_fuse_table
                        .read_table_snapshot()
                        .await?
                        .ok_or_else(|| {
                            ErrorCode::Internal(
                                "mutation meets empty snapshot during conflict reconciliation",
                            )
                        })?;
                self.state = State::DetectConfilct(latest_snapshot);
            }
            State::AbortOperation => {
                let op = self.abort_operation.clone();
                op.abort(self.ctx.clone(), self.dal.clone()).await?;
                return Err(ErrorCode::StorageOther(
                    "mutation conflicts, concurrent mutation detected while committing segment compaction operation",
                ));
            }
            _ => return Err(ErrorCode::Internal("It's a bug.")),
        }
        Ok(())
    }
}
