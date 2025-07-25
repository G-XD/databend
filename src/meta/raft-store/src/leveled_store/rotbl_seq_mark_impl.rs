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

//! Implement the conversion between `rotbl::SeqMarked` and `Marked` that is used by this crate.
//!
//! `UserKey`
//! `SeqV <-> SeqMarked<(Option<KVMeta>, bytes)> <-> SeqMarked`
//!
//! `ExpireKey`
//! `ExpireValue <-> SeqMarked<String> <-> SeqMarked`

use std::io;

use rotbl::v001::Marked;
use rotbl::v001::SeqMarked;
use state_machine_api::KVMeta;
use state_machine_api::MetaValue;

use crate::leveled_store::value_convert::ValueConvert;

impl ValueConvert<SeqMarked> for SeqMarked<MetaValue> {
    fn conv_to(self) -> Result<SeqMarked, io::Error> {
        let (seq, data) = self.into_parts();
        let seq_marked = match data {
            Marked::TombStone => SeqMarked::new_tombstone(seq),
            Marked::Normal((meta, value)) => {
                let kv_meta_str = serde_json::to_string(&meta).map_err(|e| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("fail to encode KVMeta to json: {}", e),
                    )
                })?;

                // version, meta in json string, value
                let packed = (1u8, kv_meta_str, value);

                let d = bincode::encode_to_vec(packed, bincode_config()).map_err(|e| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("fail to encode rotbl::SeqMarked value: {}", e),
                    )
                })?;

                SeqMarked::new_normal(seq, d)
            }
        };
        Ok(seq_marked)
    }

    fn conv_from(seq_marked: SeqMarked) -> Result<Self, io::Error> {
        let (seq, data) = seq_marked.into_parts();

        let data = match data {
            Marked::TombStone => {
                return Ok(SeqMarked::new_tombstone(seq));
            }
            Marked::Normal(bytes) => bytes,
        };

        // version, meta, value
        let ((ver, meta_str, value), size): ((u8, String, Vec<u8>), usize) =
            bincode::decode_from_slice(&data, bincode_config()).map_err(|e| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("fail to decode rotbl::SeqMarked value: {}", e),
                )
            })?;

        if ver != 1 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unsupported rotbl::SeqMarked version: {}", ver),
            ));
        }

        if size != data.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "remaining bytes in rotbl::SeqMarked: has read: {}, total: {}",
                    size,
                    data.len()
                ),
            ));
        }

        let kv_meta: Option<KVMeta> = serde_json::from_str(&meta_str).map_err(|e| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("fail to decode KVMeta from rotbl::SeqMarked: {}", e),
            )
        })?;

        let marked = SeqMarked::new_normal(seq, (kv_meta, value));
        Ok(marked)
    }
}

/// Conversion for ExpireValue
impl ValueConvert<SeqMarked> for SeqMarked<String> {
    fn conv_to(self) -> Result<SeqMarked, io::Error> {
        let marked = self.map(|s| (None::<KVMeta>, s.into_bytes()));

        marked.conv_to()
    }

    fn conv_from(seq_marked: SeqMarked) -> Result<Self, io::Error> {
        let marked = SeqMarked::<(Option<KVMeta>, Vec<u8>)>::conv_from(seq_marked)?;
        let marked = marked.try_map(|(_meta, value)| {
            String::from_utf8(value).map_err(|e| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("fail to decode String from bytes: {}", e),
                )
            })
        })?;

        Ok(marked)
    }
}

fn bincode_config() -> impl bincode::config::Config {
    bincode::config::standard()
        .with_big_endian()
        .with_variable_int_encoding()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::leveled_store::value_convert::ValueConvert;

    #[test]
    fn test_marked_of_string_try_from_seq_marked() -> io::Result<()> {
        t_string_try_from(
            SeqMarked::<String>::new_normal(1, s("hello")),
            SeqMarked::new_normal(1, b("\x01\x04null\x05hello")),
        );

        t_string_try_from(
            SeqMarked::<String>::new_tombstone(2),
            SeqMarked::new_tombstone(2),
        );
        Ok(())
    }

    fn t_string_try_from(marked: SeqMarked<String>, seq_marked: SeqMarked) {
        let got: SeqMarked = marked.clone().conv_to().unwrap();
        assert_eq!(seq_marked, got);

        let got = SeqMarked::<String>::conv_from(got).unwrap();
        assert_eq!(marked, got);
    }

    #[test]
    fn test_marked_try_from_seq_marked() -> io::Result<()> {
        t_try_from(
            SeqMarked::new_normal(1, (None, b("hello"))),
            SeqMarked::new_normal(1, b("\x01\x04null\x05hello")),
        );

        t_try_from(
            SeqMarked::new_normal(1, (Some(KVMeta::new_expires_at(20)), b("hello"))),
            SeqMarked::new_normal(1, b("\x01\x10{\"expire_at\":20}\x05hello")),
        );

        t_try_from(SeqMarked::new_tombstone(2), SeqMarked::new_tombstone(2));
        Ok(())
    }

    fn t_try_from(marked: SeqMarked<MetaValue>, seq_marked: SeqMarked) {
        let got: SeqMarked = marked.clone().conv_to().unwrap();
        assert_eq!(seq_marked, got);

        let got = SeqMarked::conv_from(got).unwrap();
        assert_eq!(marked, got);
    }

    #[test]
    fn test_invalid_seq_marked() {
        t_invalid(
            SeqMarked::new_normal(1, b("\x00\x10{\"expire_at\":20}\x05hello")),
            "unsupported rotbl::SeqMarked version: 0",
        );

        t_invalid(
            SeqMarked::new_normal(1, b("\x01\x10{\"expire_at\":2x}\x05hello")),
            "fail to decode KVMeta from rotbl::SeqMarked: expected `,` or `}` at line 1 column 15",
        );

        t_invalid(
            SeqMarked::new_normal(1, b("\x01\x10{\"expire_at\":20}\x05h")),
            "fail to decode rotbl::SeqMarked value: UnexpectedEnd { additional: 4 }",
        );

        t_invalid(
            SeqMarked::new_normal(1, b("\x01\x10{\"expire_at\":20}\x05hello-")),
            "remaining bytes in rotbl::SeqMarked: has read: 24, total: 25",
        );
    }

    fn t_invalid(seq_mark: SeqMarked, want_err: impl ToString) {
        let res = SeqMarked::<(Option<KVMeta>, Vec<u8>)>::conv_from(seq_mark);
        let err = res.unwrap_err();
        assert_eq!(want_err.to_string(), err.to_string());
    }

    fn s(v: impl ToString) -> String {
        v.to_string()
    }

    fn b(v: impl ToString) -> Vec<u8> {
        v.to_string().into_bytes()
    }
}
