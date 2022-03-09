// Copyright (C) 2013-2020 Blockstack PBC, a public benefit corporation
// Copyright (C) 2020 Stacks Open Internet Foundation
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.
//
// You should have received a copy of the GNU General Public License
// along with this program.  If not, see <http://www.gnu.org/licenses/>.

use std::char::from_digit;
use std::collections::{HashMap, HashSet, VecDeque};
use std::convert::{TryFrom, TryInto};
use std::env;
use std::fmt;
use std::fs;
use std::fs::OpenOptions;
use std::hash::{Hash, Hasher};
use std::io;
use std::io::{BufWriter, Cursor, Read, Seek, SeekFrom, Write};
use std::iter::FromIterator;
use std::marker::PhantomData;
use std::ops::{Deref, DerefMut};
use std::os;
use std::path::{Path, PathBuf};
use std::time::SystemTime;
use std::{cmp, error};

use rusqlite::{
    types::{FromSql, ToSql},
    Connection, Error as SqliteError, ErrorCode as SqliteErrorCode, OpenFlags, OptionalExtension,
    Transaction, NO_PARAMS,
};

use chainstate::stacks::index::bits::{
    get_node_byte_len, get_node_hash, read_block_identifier, read_hash_bytes, read_node_hash_bytes,
    read_nodetype, read_nodetype_at_head, read_root_hash, write_nodetype_bytes,
};
use chainstate::stacks::index::node::{
    clear_backptr, is_backptr, set_backptr, TrieNode, TrieNode16, TrieNode256, TrieNode4,
    TrieNode48, TrieNodeID, TrieNodeType, TriePath, TriePtr,
};
use chainstate::stacks::index::storage::NodeHashReader;
use chainstate::stacks::index::storage::TrieStorageConnection;
use chainstate::stacks::index::Error;
use chainstate::stacks::index::TrieLeaf;
use chainstate::stacks::index::{trie_sql, ClarityMarfTrieId, MarfTrieId};

use util_lib::db::sql_pragma;
use util_lib::db::sqlite_open;
use util_lib::db::tx_begin_immediate;
use util_lib::db::tx_busy_handler;
use util_lib::db::Error as db_error;
use util_lib::db::SQLITE_MMAP_SIZE;

use stacks_common::types::chainstate::BlockHeaderHash;
use stacks_common::types::chainstate::BLOCK_HEADER_HASH_ENCODED_SIZE;
use stacks_common::types::chainstate::{TrieHash, TRIEHASH_ENCODED_SIZE};

/// Mapping between block IDs and trie offsets
pub type TrieIdOffsets = HashMap<u32, u64>;

/// Handle to a flat file containing Trie blobs
pub struct TrieFileDisk {
    fd: fs::File,
    path: String,
    trie_offsets: TrieIdOffsets,
}

/// Handle to a flat in-memory buffer containing Trie blobs (used for testing)
pub struct TrieFileRAM {
    fd: Cursor<Vec<u8>>,
    readonly: bool,
    trie_offsets: TrieIdOffsets,
}

pub enum TrieFile {
    RAM(TrieFileRAM),
    Disk(TrieFileDisk),
}

impl TrieFile {
    /// Make a new disk-backed TrieFile
    fn new_disk(path: &str, readonly: bool) -> Result<TrieFile, Error> {
        let fd = OpenOptions::new()
            .read(true)
            .write(!readonly)
            .create(!readonly)
            .open(path)?;
        Ok(TrieFile::Disk(TrieFileDisk {
            fd,
            path: path.to_string(),
            trie_offsets: TrieIdOffsets::new(),
        }))
    }

    /// Make a new RAM-backed TrieFile
    fn new_ram(readonly: bool) -> TrieFile {
        TrieFile::RAM(TrieFileRAM {
            fd: Cursor::new(vec![]),
            readonly,
            trie_offsets: TrieIdOffsets::new(),
        })
    }

    /// Does the TrieFile exist at the expected path?
    pub fn exists(path: &str) -> Result<bool, Error> {
        if path == ":memory:" {
            Ok(false)
        } else {
            let blob_path = format!("{}.blobs", path);
            match fs::metadata(&blob_path) {
                Ok(_) => Ok(true),
                Err(e) => {
                    if e.kind() == io::ErrorKind::NotFound {
                        Ok(false)
                    } else {
                        return Err(e.into());
                    }
                }
            }
        }
    }

    /// Instantiate a TrieFile, given the associated DB path.
    /// If path is ':memory:', then it'll be an in-RAM TrieFile.
    /// Otherwise, it'll be stored as `$db_path.blobs`.
    pub fn from_db_path(path: &str, readonly: bool) -> Result<TrieFile, Error> {
        if path == ":memory:" {
            Ok(TrieFile::new_ram(readonly))
        } else {
            let blob_path = format!("{}.blobs", path);
            TrieFile::new_disk(&blob_path, readonly)
        }
    }

    /// Write a trie blob to external storage, and add the offset and length to the trie DB.
    /// Return the trie ID
    pub fn store_trie_blob<T: MarfTrieId>(
        &mut self,
        db: &Connection,
        bhh: &T,
        buffer: &[u8],
        block_id: Option<u32>,
    ) -> Result<u32, Error> {
        let offset = self.append_trie_blob(db, buffer)?;
        test_debug!("Stored trie blob {} to offset {}", bhh, offset);
        trie_sql::write_external_trie_blob(db, bhh, offset, buffer.len() as u64, block_id)
    }

    /// Read a trie blob in its entirety from the DB
    fn read_trie_blob_from_db(db: &Connection, block_id: u32) -> Result<Vec<u8>, Error> {
        let trie_blob = {
            let mut fd = trie_sql::open_trie_blob_readonly(db, block_id)?;
            let mut trie_blob = vec![];
            fd.read_to_end(&mut trie_blob)?;
            trie_blob
        };
        Ok(trie_blob)
    }

    /// Read a trie blob in its entirety from the blobs file
    #[cfg(test)]
    fn read_trie_blob(&mut self, db: &Connection, block_id: u32) -> Result<Vec<u8>, Error> {
        let (offset, length) = trie_sql::get_external_trie_offset_length(db, block_id)?;
        self.seek(SeekFrom::Start(offset))?;

        let mut buf = vec![0u8; length as usize];
        self.read_exact(&mut buf)?;
        Ok(buf)
    }

    /// Copy the trie blobs out of a sqlite3 DB into their own file
    pub fn export_trie_blobs<T: MarfTrieId>(&mut self, db: &Connection) -> Result<(), Error> {
        let max_block = trie_sql::count_blocks(db)?;
        info!("Migrate {} blocks to external blob storage", max_block);
        for block_id in 0..(max_block + 1) {
            match trie_sql::is_unconfirmed_block(db, block_id) {
                Ok(true) => {
                    test_debug!("Skip block_id {} since it's unconfirmed", block_id);
                    continue;
                }
                Err(Error::NotFoundError) => {
                    test_debug!("Skip block_id {} since it's not a block", block_id);
                    continue;
                }
                Ok(false) => {
                    // get the blob
                    let trie_blob = TrieFile::read_trie_blob_from_db(db, block_id)?;

                    // get the block ID
                    let bhh: T = trie_sql::get_block_hash(db, block_id)?;

                    // append the blob, replacing the current trie blob
                    info!(
                        "Migrate block {} ({} of {}) to external blob storage",
                        &bhh, block_id, max_block
                    );

                    // append directly to file, so we can get the true offset
                    self.seek(SeekFrom::End(0))?;
                    let offset = self.stream_position()?;
                    self.write_all(&trie_blob)?;
                    self.flush()?;

                    test_debug!("Stored trie blob {} to offset {}", bhh, offset);
                    trie_sql::write_external_trie_blob(
                        db,
                        &bhh,
                        offset,
                        trie_blob.len() as u64,
                        Some(block_id),
                    )?;
                }
                Err(e) => {
                    test_debug!(
                        "Failed to determine if {} is unconfirmed: {:?}",
                        block_id,
                        &e
                    );
                    return Err(e);
                }
            }
        }
        Ok(())
    }
}

/// NodeHashReader for TrieFile
pub struct TrieFileNodeHashReader<'a> {
    db: &'a Connection,
    file: &'a mut TrieFile,
    block_id: u32,
}

impl<'a> TrieFileNodeHashReader<'a> {
    pub fn new(
        db: &'a Connection,
        file: &'a mut TrieFile,
        block_id: u32,
    ) -> TrieFileNodeHashReader<'a> {
        TrieFileNodeHashReader { db, file, block_id }
    }
}

impl NodeHashReader for TrieFileNodeHashReader<'_> {
    fn read_node_hash_bytes<W: Write>(&mut self, ptr: &TriePtr, w: &mut W) -> Result<(), Error> {
        let trie_offset = self.file.get_trie_offset(self.db, self.block_id)?;
        self.file
            .seek(SeekFrom::Start(trie_offset + (ptr.ptr() as u64)))?;
        let hash_buff = read_hash_bytes(self.file)?;
        w.write_all(&hash_buff).map_err(|e| e.into())
    }
}

impl TrieFile {
    /// Determine the file offset in the TrieFile where a serialized trie starts.
    /// The offsets are stored in the given DB, and are cached indefinitely once loaded.
    pub fn get_trie_offset(&mut self, db: &Connection, block_id: u32) -> Result<u64, Error> {
        let offset_opt = match self {
            TrieFile::RAM(ref ram) => ram.trie_offsets.get(&block_id),
            TrieFile::Disk(ref disk) => disk.trie_offsets.get(&block_id),
        };
        match offset_opt {
            Some(offset) => Ok(*offset),
            None => {
                let (offset, _length) = trie_sql::get_external_trie_offset_length(db, block_id)?;
                match self {
                    TrieFile::RAM(ref mut ram) => ram.trie_offsets.insert(block_id, offset),
                    TrieFile::Disk(ref mut disk) => disk.trie_offsets.insert(block_id, offset),
                };
                Ok(offset)
            }
        }
    }

    /// Obtain a TrieHash for a node, given its block ID and pointer
    pub fn get_node_hash_bytes(
        &mut self,
        db: &Connection,
        block_id: u32,
        ptr: &TriePtr,
    ) -> Result<TrieHash, Error> {
        let offset = self.get_trie_offset(db, block_id)?;
        self.seek(SeekFrom::Start(offset + (ptr.ptr() as u64)))?;
        let hash_buff = read_hash_bytes(self)?;
        Ok(TrieHash(hash_buff))
    }

    /// Obtain a TrieNodeType and its associated TrieHash for a node, given its block ID and
    /// pointer
    pub fn read_node_type(
        &mut self,
        db: &Connection,
        block_id: u32,
        ptr: &TriePtr,
    ) -> Result<(TrieNodeType, TrieHash), Error> {
        let offset = self.get_trie_offset(db, block_id)?;
        self.seek(SeekFrom::Start(offset + (ptr.ptr() as u64)))?;
        read_nodetype_at_head(self, ptr.id(), true)
    }

    /// Obtain a TrieNodeType, given its block ID and pointer
    pub fn read_node_type_nohash(
        &mut self,
        db: &Connection,
        block_id: u32,
        ptr: &TriePtr,
    ) -> Result<TrieNodeType, Error> {
        let offset = self.get_trie_offset(db, block_id)?;
        self.seek(SeekFrom::Start(offset + (ptr.ptr() as u64)))?;
        read_nodetype_at_head(self, ptr.id(), false).map(|(node, _)| node)
    }

    /// Obtain a TrieHash for a node, given the node's block's hash (used only in testing)
    #[cfg(test)]
    pub fn get_node_hash_bytes_by_bhh<T: MarfTrieId>(
        &mut self,
        db: &Connection,
        bhh: &T,
        ptr: &TriePtr,
    ) -> Result<TrieHash, Error> {
        let (offset, _length) = trie_sql::get_external_trie_offset_length_by_bhh(db, bhh)?;
        self.seek(SeekFrom::Start(offset + (ptr.ptr() as u64)))?;
        let hash_buff = read_hash_bytes(self)?;
        Ok(TrieHash(hash_buff))
    }

    /// Get all (root hash, trie hash) pairs for this TrieFile
    #[cfg(test)]
    pub fn read_all_block_hashes_and_roots<T: MarfTrieId>(
        &mut self,
        db: &Connection,
    ) -> Result<Vec<(TrieHash, T)>, Error> {
        let mut s =
            db.prepare("SELECT block_hash, external_offset FROM marf_data WHERE unconfirmed = 0 ORDER BY block_hash")?;
        let rows = s.query_and_then(NO_PARAMS, |row| {
            let block_hash: T = row.get_unwrap("block_hash");
            let offset_i64: i64 = row.get_unwrap("external_offset");
            let offset = offset_i64 as u64;
            let start = TrieStorageConnection::<T>::root_ptr_disk() as u64;

            self.seek(SeekFrom::Start(offset + start))?;
            let hash_buff = read_hash_bytes(self)?;
            let root_hash = TrieHash(hash_buff);

            trace!(
                "Root hash for block {} at offset {} is {}",
                &block_hash,
                offset + start,
                &root_hash
            );
            Ok((root_hash, block_hash))
        })?;
        rows.collect()
    }

    /// Append a serialized trie to the TrieFile.
    /// Returns the offset at which it was appended.
    pub fn append_trie_blob(&mut self, db: &Connection, buf: &[u8]) -> Result<u64, Error> {
        let offset = trie_sql::get_external_blobs_length(db)?;
        test_debug!("Write trie of {} bytes at {}", buf.len(), offset);
        self.seek(SeekFrom::Start(offset))?;
        self.write_all(buf)?;
        self.flush()?;

        match self {
            TrieFile::Disk(ref mut data) => {
                data.fd.sync_data()?;
            }
            _ => {}
        }
        Ok(offset)
    }
}

/// Boilerplate Write implementation for TrieFileDisk.  Plumbs through to the inner fd.
impl Write for TrieFileDisk {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.fd.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.fd.flush()
    }
}

/// Boilerplate Write implementation for TrieFileRAM.  Plumbs through to the inner fd.
impl Write for TrieFileRAM {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.fd.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.fd.flush()
    }
}

/// Boilerplate Write implementation for TrieFile enum.  Plumbs through to the inner struct.
impl Write for TrieFile {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            TrieFile::RAM(ref mut ram) => ram.write(buf),
            TrieFile::Disk(ref mut disk) => disk.write(buf),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            TrieFile::RAM(ref mut ram) => ram.flush(),
            TrieFile::Disk(ref mut disk) => disk.flush(),
        }
    }
}

/// Boilerplate Read implementation for TrieFileDisk.  Plumbs through to the inner fd.
impl Read for TrieFileDisk {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.fd.read(buf)
    }
}

/// Boilerplate Read implementation for TrieFileRAM.  Plumbs through to the inner fd.
impl Read for TrieFileRAM {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.fd.read(buf)
    }
}

/// Boilerplate Read implementation for TrieFile enum.  Plumbs through to the inner struct.
impl Read for TrieFile {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            TrieFile::RAM(ref mut ram) => ram.read(buf),
            TrieFile::Disk(ref mut disk) => disk.read(buf),
        }
    }
}

/// Boilerplate Seek implementation for TrieFileDisk.  Plumbs through to the inner fd
impl Seek for TrieFileDisk {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        self.fd.seek(pos)
    }
}

/// Boilerplate Seek implementation for TrieFileDisk.  Plumbs through to the inner fd
impl Seek for TrieFileRAM {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        self.fd.seek(pos)
    }
}

impl Seek for TrieFile {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        match self {
            TrieFile::RAM(ref mut ram) => ram.seek(pos),
            TrieFile::Disk(ref mut disk) => disk.seek(pos),
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use chainstate::stacks::index::cache::test::make_test_insert_data;
    use chainstate::stacks::index::cache::*;
    use chainstate::stacks::index::marf::*;
    use chainstate::stacks::index::storage::*;
    use chainstate::stacks::index::*;
    use rusqlite::Connection;
    use rusqlite::OpenFlags;
    use std::fs;
    use util_lib::db::*;

    fn db_path(test_name: &str) -> String {
        let path = format!("/tmp/{}.sqlite", test_name);
        path
    }

    fn setup_db(test_name: &str) -> Connection {
        let path = db_path(test_name);
        if fs::metadata(&path).is_ok() {
            fs::remove_file(&path).unwrap();
        }

        let mut db = sqlite_open(
            &path,
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE,
            true,
        )
        .unwrap();
        trie_sql::create_tables_if_needed(&mut db).unwrap();
        db
    }

    #[test]
    fn test_load_store_trie_blob() {
        let mut db = setup_db("test_load_store_trie_blob");
        let mut blobs =
            TrieFile::from_db_path(&db_path("test_load_store_trie_blob"), false).unwrap();
        trie_sql::migrate_tables_if_needed::<BlockHeaderHash>(&mut db, Some(&mut blobs)).unwrap();

        blobs
            .store_trie_blob::<BlockHeaderHash>(
                &db,
                &BlockHeaderHash([0x01; 32]),
                &[1, 2, 3, 4, 5],
                None,
            )
            .unwrap();
        blobs
            .store_trie_blob::<BlockHeaderHash>(
                &db,
                &BlockHeaderHash([0x02; 32]),
                &[10, 20, 30, 40, 50],
                None,
            )
            .unwrap();

        let block_id = trie_sql::get_block_identifier(&db, &BlockHeaderHash([0x01; 32])).unwrap();
        assert_eq!(blobs.get_trie_offset(&db, block_id).unwrap(), 0);

        let buf = blobs.read_trie_blob(&db, block_id).unwrap();
        assert_eq!(buf, vec![1, 2, 3, 4, 5]);

        let block_id = trie_sql::get_block_identifier(&db, &BlockHeaderHash([0x02; 32])).unwrap();
        assert_eq!(blobs.get_trie_offset(&db, block_id).unwrap(), 5);

        let buf = blobs.read_trie_blob(&db, block_id).unwrap();
        assert_eq!(buf, vec![10, 20, 30, 40, 50]);
    }

    #[test]
    fn test_migrate_existing_trie_blobs() {
        let test_file = "/tmp/test_migrate_existing_trie_blobs.sqlite";
        let test_blobs_file = "/tmp/test_migrate_existing_trie_blobs.sqlite.blobs";
        if fs::metadata(&test_file).is_ok() {
            fs::remove_file(&test_file).unwrap();
        }
        if fs::metadata(&test_blobs_file).is_ok() {
            fs::remove_file(&test_blobs_file).unwrap();
        }

        let (data, last_block_header, root_header_map) = {
            let marf_opts = MARFOpenOpts::new(TrieHashCalculationMode::Deferred, "noop", false);

            let f = TrieFileStorage::open(&test_file, marf_opts).unwrap();
            let mut marf = MARF::from_storage(f);

            // make data to insert
            let data = make_test_insert_data(128, 128);
            let mut last_block_header = BlockHeaderHash::sentinel();
            for (i, block_data) in data.iter().enumerate() {
                let mut block_hash_bytes = [0u8; 32];
                block_hash_bytes[0..8].copy_from_slice(&(i as u64).to_be_bytes());

                let block_header = BlockHeaderHash(block_hash_bytes);
                marf.begin(&last_block_header, &block_header).unwrap();

                for (key, value) in block_data.iter() {
                    let path = TriePath::from_key(key);
                    let leaf = TrieLeaf::from_value(&vec![], value.clone());
                    marf.insert_raw(path, leaf).unwrap();
                }
                marf.commit().unwrap();
                last_block_header = block_header;
            }

            let root_header_map =
                trie_sql::read_all_block_hashes_and_roots::<BlockHeaderHash>(marf.sqlite_conn())
                    .unwrap();
            (data, last_block_header, root_header_map)
        };

        // migrate
        let mut marf_opts = MARFOpenOpts::new(TrieHashCalculationMode::Deferred, "noop", true);
        marf_opts.force_db_migrate = true;

        let f = TrieFileStorage::open(&test_file, marf_opts).unwrap();
        let mut marf = MARF::from_storage(f);

        // blobs file exists
        assert!(fs::metadata(&test_blobs_file).is_ok());

        // verify that the new blob structure is well-formed
        let blob_root_header_map = {
            let mut blobs = TrieFile::from_db_path(&test_file, false).unwrap();
            let blob_root_header_map = blobs
                .read_all_block_hashes_and_roots::<BlockHeaderHash>(marf.sqlite_conn())
                .unwrap();
            blob_root_header_map
        };

        assert_eq!(blob_root_header_map.len(), root_header_map.len());
        for (e1, e2) in blob_root_header_map.iter().zip(root_header_map.iter()) {
            assert_eq!(e1, e2);
        }

        // verify that we can read everything from the blobs
        for (i, block_data) in data.iter().enumerate() {
            for (key, value) in block_data.iter() {
                let path = TriePath::from_key(key);
                let marf_leaf = TrieLeaf::from_value(&vec![], value.clone());

                let leaf = MARF::get_path(
                    &mut marf.borrow_storage_backend(),
                    &last_block_header,
                    &path,
                )
                .unwrap()
                .unwrap();

                assert_eq!(leaf.data.to_vec(), marf_leaf.data.to_vec());
            }
        }
    }
}