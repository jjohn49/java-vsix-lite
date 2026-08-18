//! A minimal, read-only ZIP reader for jmod/jar archives.
//!
//! Only the central directory is held in memory (entry name → location); entry
//! bytes are read and DEFLATE-decompressed on demand. A `base` offset lets a
//! jmod be read as "skip the 4-byte `JM` header, then a standard ZIP".
//!
//! Hardening (threat model): entry names with `..`, a leading `/`, or `\` are
//! rejected; per-entry uncompressed size is capped (zip-bomb); decompression is
//! bounded by the same cap; nothing is ever written to disk. Zip64 entries are
//! skipped rather than trusted.

use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

/// Cap on a single entry's uncompressed size. Class files are small (tens of KB);
/// this is generous while still refusing decompression bombs.
const MAX_UNCOMPRESSED: usize = 16 * 1024 * 1024;

const EOCD_SIG: u32 = 0x0605_4b50;
const CDFH_SIG: u32 = 0x0201_4b50;
const LFH_SIG: u32 = 0x0403_4b50;
const ZIP64_SENTINEL: u32 = 0xFFFF_FFFF;

const METHOD_STORE: u16 = 0;
const METHOD_DEFLATE: u16 = 8;

struct Entry {
    method: u16,
    comp_size: u32,
    uncomp_size: u32,
    local_offset: u32,
}

/// A read-only view over one archive's central directory.
pub(crate) struct ZipArchive {
    path: PathBuf,
    /// Bytes preceding the ZIP within the file (4 for a jmod, 0 for a jar).
    base: u64,
    entries: HashMap<String, Entry>,
}

impl ZipArchive {
    /// Open `path` and parse its central directory, treating the ZIP as starting
    /// `base` bytes into the file. Returns `None` on any malformed structure.
    pub(crate) fn open(path: &Path, base: u64) -> Option<ZipArchive> {
        let mut file = File::open(path).ok()?;
        let file_len = file.metadata().ok()?.len();

        // EOCD is within the last (22 + max-comment) bytes; scan the tail for it.
        let tail_len = file_len.min(22 + 0xFFFF);
        let tail = read_at(&mut file, file_len - tail_len, tail_len as usize)?;
        let eocd = find_eocd(&tail)?;
        let cd_size = u32le(&tail, eocd + 12)? as usize;
        let cd_offset = u32le(&tail, eocd + 16)?;
        if cd_offset == ZIP64_SENTINEL {
            return None; // zip64 unsupported
        }

        let cd = read_at(&mut file, base + cd_offset as u64, cd_size)?;
        let entries = parse_central_directory(&cd);
        Some(ZipArchive {
            path: path.to_path_buf(),
            base,
            entries,
        })
    }

    pub(crate) fn contains(&self, name: &str) -> bool {
        self.entries.contains_key(name)
    }

    /// Every entry name in the central directory (arbitrary order — callers
    /// sort). Names were already screened by [`parse_central_directory`]'s
    /// traversal guards.
    pub(crate) fn names(&self) -> impl Iterator<Item = &str> {
        self.entries.keys().map(String::as_str)
    }

    /// The first entry whose name ends with `suffix` — used to find a
    /// module-prefixed source file (`java.base/java/util/List.java`) in src.zip.
    pub(crate) fn find_suffix(&self, suffix: &str) -> Option<String> {
        self.entries.keys().find(|k| k.ends_with(suffix)).cloned()
    }

    /// Read and decompress one entry by exact name, enforcing the size cap.
    pub(crate) fn read(&self, name: &str) -> Option<Vec<u8>> {
        let entry = self.entries.get(name)?;
        let mut file = File::open(&self.path).ok()?;

        // Local header: the true data offset is past its (possibly different)
        // name + extra fields, so we read those lengths rather than trusting the
        // central directory's copy.
        let lfh = read_at(&mut file, self.base + entry.local_offset as u64, 30)?;
        if u32le(&lfh, 0)? != LFH_SIG {
            return None;
        }
        let name_len = u16le(&lfh, 26)? as u64;
        let extra_len = u16le(&lfh, 28)? as u64;
        let data_pos = self.base + entry.local_offset as u64 + 30 + name_len + extra_len;
        let compressed = read_at(&mut file, data_pos, entry.comp_size as usize)?;

        match entry.method {
            METHOD_STORE => (compressed.len() == entry.uncomp_size as usize).then_some(compressed),
            METHOD_DEFLATE => {
                let out = miniz_oxide::inflate::decompress_to_vec_with_limit(
                    &compressed,
                    MAX_UNCOMPRESSED,
                )
                .ok()?;
                (out.len() <= MAX_UNCOMPRESSED).then_some(out)
            }
            _ => None,
        }
    }
}

/// Scan backward for the End-Of-Central-Directory signature; return its offset.
fn find_eocd(tail: &[u8]) -> Option<usize> {
    if tail.len() < 22 {
        return None;
    }
    (0..=tail.len() - 22)
        .rev()
        .find(|&i| u32le(tail, i) == Some(EOCD_SIG))
}

fn parse_central_directory(cd: &[u8]) -> HashMap<String, Entry> {
    let mut entries = HashMap::new();
    let mut p = 0usize;
    while p + 46 <= cd.len() {
        if u32le(cd, p) != Some(CDFH_SIG) {
            break;
        }
        let method = u16le(cd, p + 10);
        let comp_size = u32le(cd, p + 20);
        let uncomp_size = u32le(cd, p + 24);
        let name_len = u16le(cd, p + 28).map(|v| v as usize);
        let extra_len = u16le(cd, p + 30).map(|v| v as usize);
        let comment_len = u16le(cd, p + 32).map(|v| v as usize);
        let local_offset = u32le(cd, p + 42);
        let (
            Some(method),
            Some(comp_size),
            Some(uncomp_size),
            Some(name_len),
            Some(extra_len),
            Some(comment_len),
            Some(local_offset),
        ) = (
            method,
            comp_size,
            uncomp_size,
            name_len,
            extra_len,
            comment_len,
            local_offset,
        )
        else {
            break;
        };

        let name_start = p + 46;
        let name_end = name_start + name_len;
        if name_end > cd.len() {
            break;
        }
        let name = std::str::from_utf8(&cd[name_start..name_end]).ok();
        p = name_end + extra_len + comment_len;

        let Some(name) = name else { continue };
        // Skip zip64 entries (sentinel sizes/offsets) and zip-bomb-sized or
        // path-unsafe names rather than indexing them.
        if uncomp_size != ZIP64_SENTINEL
            && local_offset != ZIP64_SENTINEL
            && (uncomp_size as usize) <= MAX_UNCOMPRESSED
            && (comp_size as usize) <= MAX_UNCOMPRESSED
            && !is_unsafe_name(name)
        {
            entries.insert(
                name.to_string(),
                Entry {
                    method,
                    comp_size,
                    uncomp_size,
                    local_offset,
                },
            );
        }
    }
    entries
}

/// Reject path-traversal / absolute / backslash names (defense in depth — we only
/// ever look entries up by our own constructed names).
fn is_unsafe_name(name: &str) -> bool {
    name.starts_with('/') || name.contains('\\') || name.split('/').any(|seg| seg == "..")
}

fn read_at(file: &mut File, pos: u64, len: usize) -> Option<Vec<u8>> {
    // Refuse (before allocating) any read that runs past EOF, so an attacker-
    // controlled size field from the archive can never request more memory than
    // the file can actually supply — the threat model's "bounded reads".
    let file_len = file.metadata().ok()?.len();
    if pos.checked_add(len as u64)? > file_len {
        return None;
    }
    file.seek(SeekFrom::Start(pos)).ok()?;
    let mut buf = vec![0u8; len];
    file.read_exact(&mut buf).ok()?;
    Some(buf)
}

fn u16le(b: &[u8], i: usize) -> Option<u16> {
    Some(u16::from_le_bytes([*b.get(i)?, *b.get(i + 1)?]))
}

fn u32le(b: &[u8], i: usize) -> Option<u32> {
    Some(u32::from_le_bytes([
        *b.get(i)?,
        *b.get(i + 1)?,
        *b.get(i + 2)?,
        *b.get(i + 3)?,
    ]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_unsafe_entry_names() {
        assert!(is_unsafe_name("/etc/passwd"));
        assert!(is_unsafe_name("../escape"));
        assert!(is_unsafe_name("a/../../b"));
        assert!(is_unsafe_name("a\\b"));
        assert!(!is_unsafe_name("classes/java/util/List.class"));
        assert!(!is_unsafe_name("a..b/c")); // `..` only as a full segment
    }

    #[test]
    fn integer_readers_bounds_check() {
        let b = [1u8, 0, 0, 0];
        assert_eq!(u16le(&b, 0), Some(1));
        assert_eq!(u32le(&b, 0), Some(1));
        assert_eq!(u16le(&b, 3), None); // out of range
        assert_eq!(u32le(&b, 1), None);
    }
}
