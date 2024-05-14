pub mod block;
pub mod block_index;
pub mod data_block_handle_queue;
pub mod id;
pub mod index_block_consumer;
pub mod meta;
pub mod multi_reader;
pub mod multi_writer;
pub mod prefix;
pub mod range;
pub mod reader;
pub mod value_block;
pub mod writer;

use self::{
    block_index::BlockIndex, meta::Metadata, prefix::PrefixedReader, range::Range, reader::Reader,
};
use crate::{
    block_cache::BlockCache,
    descriptor_table::FileDescriptorTable,
    file::SEGMENT_METADATA_FILE,
    segment::value_block::ValueBlock,
    tree_inner::TreeId,
    value::{SeqNo, UserKey},
    Value,
};
use std::{ops::Bound, path::Path, sync::Arc};

#[cfg(feature = "bloom")]
use crate::bloom::BloomFilter;

#[cfg(feature = "bloom")]
use crate::file::BLOOM_FILTER_FILE;

/// Disk segment (a.k.a. `SSTable`, `SST`, `sorted string table`) that is located on disk
///
/// A segment is an immutable list of key-value pairs, split into compressed blocks.
/// A reference to the block (`block handle`) is saved in the "block index".
///
/// Deleted entries are represented by tombstones.
///
/// Segments can be merged together to remove duplicate items, reducing disk space and improving read performance.
#[doc(alias = "sstable")]
pub struct Segment {
    pub(crate) tree_id: TreeId,

    #[doc(hidden)]
    pub descriptor_table: Arc<FileDescriptorTable>,

    /// Segment metadata object
    pub metadata: meta::Metadata,

    /// Translates key (first item of a block) to block offset (address inside file) and (compressed) size
    #[doc(hidden)]
    pub block_index: Arc<BlockIndex>,

    /// Block cache
    ///
    /// Stores index and data blocks
    #[doc(hidden)]
    pub block_cache: Arc<BlockCache>,

    /// Bloom filter
    #[cfg(feature = "bloom")]
    #[doc(hidden)]
    pub bloom_filter: BloomFilter,
}

impl std::fmt::Debug for Segment {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Segment:{}", self.metadata.id)
    }
}

impl Segment {
    /// Tries to recover a segment from a folder.
    pub fn recover<P: AsRef<Path>>(
        folder: P,
        tree_id: TreeId,
        block_cache: Arc<BlockCache>,
        descriptor_table: Arc<FileDescriptorTable>,
    ) -> crate::Result<Self> {
        let folder = folder.as_ref();

        let metadata = Metadata::from_disk(folder.join(SEGMENT_METADATA_FILE))?;
        let block_index = BlockIndex::from_file(
            (tree_id, metadata.id).into(),
            descriptor_table.clone(),
            folder,
            Arc::clone(&block_cache),
        )?;

        Ok(Self {
            tree_id,

            descriptor_table,
            metadata,
            block_index: Arc::new(block_index),
            block_cache,

            // TODO: only load bloom if file exists?
            #[cfg(feature = "bloom")]
            bloom_filter: BloomFilter::from_file(folder.join(BLOOM_FILTER_FILE))?,
        })
    }

    #[cfg(feature = "bloom")]
    #[must_use]
    /// Gets the bloom filter size
    pub fn bloom_filter_size(&self) -> usize {
        self.bloom_filter.len()
    }

    /// Retrieves an item from the segment.
    ///
    /// # Errors
    ///
    /// Will return `Err` if an IO error occurs.
    pub fn get<K: AsRef<[u8]>>(
        &self,
        key: K,
        seqno: Option<SeqNo>,
    ) -> crate::Result<Option<Value>> {
        use value_block::CachePolicy;

        if let Some(seqno) = seqno {
            if self.metadata.seqnos.0 >= seqno {
                return Ok(None);
            }
        }

        if !self.metadata.key_range.contains_key(&key) {
            return Ok(None);
        }

        let key = key.as_ref();

        #[cfg(feature = "bloom")]
        {
            if !self.bloom_filter.contains(key) {
                return Ok(None);
            }
        }

        if seqno.is_none() {
            // NOTE: Fastpath for non-seqno reads (which are most common)
            // This avoids setting up a rather expensive block iterator
            // (see explanation for that below)
            // This only really works because sequence numbers are sorted
            // in descending order

            if let Some(data_block_handle) = self
                .block_index
                .get_lowest_data_block_handle_containing_item(key.as_ref(), CachePolicy::Write)?
            {
                let block = ValueBlock::load_by_block_handle(
                    &self.descriptor_table,
                    &self.block_cache,
                    (self.tree_id, self.metadata.id).into(),
                    &data_block_handle,
                    CachePolicy::Write,
                )?;

                let item = block.map_or_else(
                    || Ok(None),
                    |block| {
                        // TODO: maybe binary search can be used, but it needs to find the max seqno
                        // TODO: if so, implement in ValueBlock
                        Ok(block
                            .items
                            .iter()
                            .find(|item| item.key == key.as_ref().into())
                            .cloned())
                    },
                );

                return item;
            }
        }

        // NOTE: For finding a specific seqno,
        // we need to use a reader
        // because nothing really prevents the version
        // we are searching for to be in the next block
        // after the one our key starts in
        //
        // Example (key:seqno), searching for a:2:
        //
        // [..., a:5, a:4] [a:3, a:2, b: 4, b:3]
        // ^               ^
        // Block A         Block B
        //
        // Based on get_lower_bound_block, "a" is in Block A
        // However, we are searching for A with seqno 2, which
        // unfortunately is in the next block

        let iter = Reader::new(
            self.descriptor_table.clone(),
            (self.tree_id, self.metadata.id).into(),
            self.block_cache.clone(),
            self.block_index.clone(),
        )
        .set_lower_bound(key.into());

        for item in iter {
            let item = item?;

            // Just stop iterating once we go past our desired key
            if &*item.key != key {
                return Ok(None);
            }

            if let Some(seqno) = seqno {
                if item.seqno < seqno {
                    return Ok(Some(item));
                }
            } else {
                return Ok(Some(item));
            }
        }

        Ok(None)
    }

    /// Creates an iterator over the `Segment`.
    ///
    /// # Errors
    ///
    /// Will return `Err` if an IO error occurs.
    #[must_use]
    #[allow(clippy::iter_without_into_iter)]
    pub fn iter(&self) -> Reader {
        Reader::new(
            Arc::clone(&self.descriptor_table),
            (self.tree_id, self.metadata.id).into(),
            Arc::clone(&self.block_cache),
            Arc::clone(&self.block_index),
        )
    }

    /// Creates a ranged iterator over the `Segment`.
    ///
    /// # Errors
    ///
    /// Will return `Err` if an IO error occurs.
    #[must_use]
    pub fn range(&self, range: (Bound<UserKey>, Bound<UserKey>)) -> Range {
        Range::new(
            Arc::clone(&self.descriptor_table),
            (self.tree_id, self.metadata.id).into(),
            Arc::clone(&self.block_cache),
            Arc::clone(&self.block_index),
            range,
        )
    }

    /// Creates a prefixed iterator over the `Segment`.
    ///
    /// # Errors
    ///
    /// Will return `Err` if an IO error occurs.
    #[must_use]
    pub fn prefix<K: Into<UserKey>>(&self, prefix: K) -> PrefixedReader {
        PrefixedReader::new(
            Arc::clone(&self.descriptor_table),
            (self.tree_id, self.metadata.id).into(),
            Arc::clone(&self.block_cache),
            Arc::clone(&self.block_index),
            prefix,
        )
    }

    /// Returns the highest sequence number in the segment.
    #[must_use]
    pub fn get_lsn(&self) -> SeqNo {
        self.metadata.seqnos.1
    }

    /// Returns the amount of tombstone markers in the `Segment`.
    #[must_use]
    pub fn tombstone_count(&self) -> u64 {
        self.metadata.tombstone_count
    }

    /// Checks if a key range is (partially or fully) contained in this segment.
    pub(crate) fn check_key_range_overlap(
        &self,
        bounds: &(Bound<UserKey>, Bound<UserKey>),
    ) -> bool {
        self.metadata.key_range.overlaps_with_bounds(bounds)
    }
}
