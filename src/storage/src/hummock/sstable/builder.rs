// Copyright 2022 Singularity Data
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::collections::BTreeSet;
use std::sync::Arc;

use bytes::{BufMut, Bytes, BytesMut};
use risingwave_common::config::StorageConfig;
use risingwave_hummock_sdk::filter_key_extractor::{
    FilterKeyExtractorImpl, FullKeyFilterKeyExtractor,
};
use risingwave_hummock_sdk::key::{get_table_id, user_key};

use super::bloom::Bloom;
use super::utils::CompressionAlgorithm;
use super::{
    BlockBuilder, BlockBuilderOptions, BlockMeta, SstableMeta, DEFAULT_BLOCK_SIZE,
    DEFAULT_ENTRY_SIZE, DEFAULT_RESTART_INTERVAL, VERSION,
};
use crate::hummock::value::HummockValue;

pub const DEFAULT_SSTABLE_SIZE: usize = 4 * 1024 * 1024;
pub const DEFAULT_BLOOM_FALSE_POSITIVE: f64 = 0.1;

#[derive(Clone, Debug)]
pub struct SstableBuilderOptions {
    /// Approximate sstable capacity.
    pub capacity: usize,
    /// Approximate block capacity.
    pub block_capacity: usize,
    /// Restart point interval.
    pub restart_interval: usize,
    /// False positive probability of bloom filter.
    pub bloom_false_positive: f64,
    /// Compression algorithm.
    pub compression_algorithm: CompressionAlgorithm,
    /// Approximate bloom filter capacity.
    pub estimate_bloom_filter_capacity: usize,
}

impl From<&StorageConfig> for SstableBuilderOptions {
    fn from(options: &StorageConfig) -> SstableBuilderOptions {
        let capacity = (options.sstable_size_mb as usize) * (1 << 20);
        SstableBuilderOptions {
            capacity,
            block_capacity: (options.block_size_kb as usize) * (1 << 10),
            restart_interval: DEFAULT_RESTART_INTERVAL,
            bloom_false_positive: options.bloom_false_positive,
            compression_algorithm: CompressionAlgorithm::None,
            estimate_bloom_filter_capacity: capacity / DEFAULT_ENTRY_SIZE,
        }
    }
}

impl Default for SstableBuilderOptions {
    fn default() -> Self {
        Self {
            capacity: DEFAULT_SSTABLE_SIZE,
            block_capacity: DEFAULT_BLOCK_SIZE,
            restart_interval: DEFAULT_RESTART_INTERVAL,
            bloom_false_positive: DEFAULT_BLOOM_FALSE_POSITIVE,
            compression_algorithm: CompressionAlgorithm::None,
            estimate_bloom_filter_capacity: DEFAULT_SSTABLE_SIZE / DEFAULT_ENTRY_SIZE,
        }
    }
}

pub struct SstableBuilder {
    /// Options.
    options: SstableBuilderOptions,
    /// Write buffer.
    buf: BytesMut,
    /// Current block builder.
    block_builder: BlockBuilder,
    /// Block metadata vec.
    block_metas: Vec<BlockMeta>,
    /// `table_id` of added keys.
    table_ids: BTreeSet<u32>,
    last_table_id: u32,
    /// Hashes of user keys.
    user_key_hashes: Vec<u32>,
    last_full_key: Vec<u8>,
    key_count: usize,
    sstable_id: u64,
    raw_value: BytesMut,
    filter_key_extractor: Arc<FilterKeyExtractorImpl>,
    last_bloom_filter_key_length: usize,
    add_bloom_filter_key_counts: usize,
}

impl SstableBuilder {
    pub fn new_for_test(sstable_id: u64, options: SstableBuilderOptions) -> Self {
        Self {
            buf: BytesMut::with_capacity(options.capacity + options.block_capacity),
            block_builder: BlockBuilder::new(BlockBuilderOptions {
                capacity: options.block_capacity,
                restart_interval: options.restart_interval,
                compression_algorithm: options.compression_algorithm,
            }),
            block_metas: Vec::with_capacity(options.capacity / options.block_capacity + 1),
            table_ids: BTreeSet::new(),
            user_key_hashes: Vec::with_capacity(options.estimate_bloom_filter_capacity),
            last_table_id: 0,
            options,
            key_count: 0,
            sstable_id,
            raw_value: BytesMut::new(),
            last_full_key: vec![],
            filter_key_extractor: Arc::new(FilterKeyExtractorImpl::FullKey(
                FullKeyFilterKeyExtractor::default(),
            )),
            last_bloom_filter_key_length: 0,
            add_bloom_filter_key_counts: 0,
        }
    }

    pub fn new(
        sstable_id: u64,
        options: SstableBuilderOptions,
        filter_key_extractor: Arc<FilterKeyExtractorImpl>,
    ) -> Self {
        Self {
            options: options.clone(),
            buf: BytesMut::with_capacity(options.capacity),
            block_builder: BlockBuilder::new(BlockBuilderOptions {
                capacity: options.block_capacity,
                restart_interval: options.restart_interval,
                compression_algorithm: options.compression_algorithm,
            }),
            block_metas: Vec::with_capacity(options.capacity / options.block_capacity + 1),
            table_ids: BTreeSet::new(),
            user_key_hashes: Vec::with_capacity(options.capacity / DEFAULT_ENTRY_SIZE + 1),
            last_table_id: 0,
            raw_value: BytesMut::new(),
            last_full_key: vec![],
            key_count: 0,
            sstable_id,
            filter_key_extractor,
            last_bloom_filter_key_length: 0,
            add_bloom_filter_key_counts: 0,
        }
    }

    /// Add kv pair to sstable.
    pub fn add(&mut self, full_key: &[u8], value: HummockValue<&[u8]>) {
        // Rotate block builder if the previous one has been built.
        if self.block_builder.is_empty() {
            self.block_metas.push(BlockMeta {
                offset: self.buf.len() as u32,
                len: 0,
                smallest_key: full_key.to_vec(),
            })
        }

        // TODO: refine me
        value.encode(&mut self.raw_value);
        let mut extract_key = user_key(full_key);
        if let Some(table_id) = get_table_id(full_key) {
            if self.last_table_id != table_id {
                self.table_ids.insert(table_id);
                self.last_table_id = table_id;
            }
        }

        extract_key = self.filter_key_extractor.extract(extract_key);

        self.block_builder.add(full_key, self.raw_value.as_ref());
        self.raw_value.clear();

        // add bloom_filter check
        // 1. not empty_key
        // 2. extract_key key is not duplicate
        if !extract_key.is_empty()
            && (extract_key != &self.last_full_key[0..self.last_bloom_filter_key_length])
        {
            // avoid duplicate add to bloom filter
            self.user_key_hashes
                .push(farmhash::fingerprint32(extract_key));
            self.last_bloom_filter_key_length = extract_key.len();
        }

        self.last_full_key.clear();
        self.last_full_key.extend_from_slice(full_key);

        if self.block_builder.approximate_len() >= self.options.block_capacity {
            self.build_block();
        }
        self.key_count += 1;
    }

    /// Finish building sst.
    ///
    /// Unlike most LSM-Tree implementations, sstable meta and data are encoded separately.
    /// Both meta and data has its own object (file).
    ///
    /// # Format
    ///
    /// data:
    ///
    /// ```plain
    /// | Block 0 | ... | Block N-1 | N (4B) |
    /// ```
    pub fn finish(mut self) -> (u64, Bytes, SstableMeta, Vec<u32>) {
        let smallest_key = self.block_metas[0].smallest_key.clone();
        let largest_key = self.last_full_key.clone();

        self.build_block();
        self.buf.put_u32_le(self.block_metas.len() as u32);
        assert!(!smallest_key.is_empty());

        let meta = SstableMeta {
            block_metas: self.block_metas,
            bloom_filter: if self.options.bloom_false_positive > 0.0 {
                let bits_per_key = Bloom::bloom_bits_per_key(
                    self.user_key_hashes.len(),
                    self.options.bloom_false_positive,
                );
                Bloom::build_from_key_hashes(&self.user_key_hashes, bits_per_key)
            } else {
                vec![]
            },
            estimated_size: self.buf.len() as u32,
            key_count: self.key_count as u32,
            smallest_key,
            largest_key,
            version: VERSION,
        };

        tracing::trace!(
            "meta_size {} bloom_filter_size {}  add_key_counts {} add_bloom_filter_counts {}",
            meta.encoded_size(),
            meta.bloom_filter.len(),
            self.key_count,
            self.add_bloom_filter_key_counts
        );

        (
            self.sstable_id,
            self.buf.freeze(),
            meta,
            self.table_ids.into_iter().collect(),
        )
    }

    pub fn approximate_len(&self) -> usize {
        self.buf.len() + self.block_builder.approximate_len() + 4
    }

    fn build_block(&mut self) {
        // Skip empty block.
        if self.block_builder.is_empty() {
            return;
        }

        let mut block_meta = self.block_metas.last_mut().unwrap();
        let block = self.block_builder.build();
        self.buf.put_slice(block);
        block_meta.len = self.buf.len() as u32 - block_meta.offset;
        self.block_builder.clear();
    }

    pub fn len(&self) -> usize {
        self.user_key_hashes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.user_key_hashes.is_empty()
    }

    /// Returns true if we roughly reached capacity
    pub fn reach_capacity(&self) -> bool {
        self.approximate_len() >= self.options.capacity
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use crate::hummock::iterator::test_utils::mock_sstable_store;
    use crate::hummock::test_utils::{
        default_builder_opt_for_test, gen_default_test_sstable, test_key_of, test_value_of,
        TEST_KEYS_COUNT,
    };

    #[test]
    #[should_panic]
    fn test_empty() {
        let opt = SstableBuilderOptions {
            capacity: 0,
            block_capacity: 4096,
            restart_interval: 16,
            bloom_false_positive: 0.1,
            compression_algorithm: CompressionAlgorithm::None,
            estimate_bloom_filter_capacity: 0,
        };

        let b = SstableBuilder::new_for_test(0, opt);

        b.finish();
    }

    #[test]
    fn test_smallest_key_and_largest_key() {
        let mut b = SstableBuilder::new_for_test(0, default_builder_opt_for_test());

        for i in 0..TEST_KEYS_COUNT {
            b.add(&test_key_of(i), HummockValue::put(&test_value_of(i)));
        }

        let (_, _, meta, _) = b.finish();

        assert_eq!(test_key_of(0), meta.smallest_key);
        assert_eq!(test_key_of(TEST_KEYS_COUNT - 1), meta.largest_key);
    }

    async fn test_with_bloom_filter(with_blooms: bool) {
        let key_count = 1000;

        let opts = SstableBuilderOptions {
            capacity: 0,
            block_capacity: 4096,
            restart_interval: 16,
            bloom_false_positive: if with_blooms { 0.01 } else { 0.0 },
            compression_algorithm: CompressionAlgorithm::None,
            estimate_bloom_filter_capacity: 0,
        };

        // build remote table
        let sstable_store = mock_sstable_store();
        let table = gen_default_test_sstable(opts, 0, sstable_store).await;

        assert_eq!(table.has_bloom_filter(), with_blooms);
        for i in 0..key_count {
            let full_key = test_key_of(i);
            assert!(!table.surely_not_have_user_key(user_key(full_key.as_slice())));
        }
    }

    #[tokio::test]
    async fn test_bloom_filter() {
        test_with_bloom_filter(false).await;
        test_with_bloom_filter(true).await;
    }
}
