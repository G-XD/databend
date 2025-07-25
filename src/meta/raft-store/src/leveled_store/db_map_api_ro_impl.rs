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

use std::io;
use std::ops::RangeBounds;

use databend_common_meta_types::snapshot_db::DB;
use futures_util::StreamExt;
use map_api::map_api_ro::MapApiRO;
use rotbl::v001::SeqMarked;

use crate::leveled_store::map_api::KVResultStream;
use crate::leveled_store::map_api::MapKey;
use crate::leveled_store::map_api::MapKeyDecode;
use crate::leveled_store::map_api::MapKeyEncode;
use crate::leveled_store::rotbl_codec::RotblCodec;
use crate::leveled_store::value_convert::ValueConvert;

/// A wrapper that implements the `MapApiRO` trait for the `DB`.
#[derive(Debug, Clone)]
pub struct MapView<'a>(pub &'a DB);

#[async_trait::async_trait]
impl<K> MapApiRO<K> for MapView<'_>
where
    K: MapKey,
    K: MapKeyEncode,
    K: MapKeyDecode,
    SeqMarked<K::V>: ValueConvert<SeqMarked>,
{
    async fn get(&self, key: &K) -> Result<SeqMarked<K::V>, io::Error> {
        let key = RotblCodec::encode_key(key)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

        let res = self.0.rotbl.get(&key).await?;

        let Some(seq_marked) = res else {
            return Ok(SeqMarked::new_not_found());
        };

        let marked = SeqMarked::<K::V>::conv_from(seq_marked)?;
        Ok(marked)
    }

    async fn range<R>(&self, range: R) -> Result<KVResultStream<K>, io::Error>
    where R: RangeBounds<K> + Clone + Send + Sync + 'static {
        let rng = RotblCodec::encode_range(&range)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

        let strm = self.0.rotbl.range(rng);

        let strm = strm.map(|res_item: Result<(String, SeqMarked), io::Error>| {
            let (str_k, seq_marked) = res_item?;
            let key = RotblCodec::decode_key(&str_k)?;
            let marked = SeqMarked::conv_from(seq_marked)?;
            Ok((key, marked))
        });

        Ok(strm.boxed())
    }
}
