//! A minimal, read-only ZIP reader for jmod/jar archives.
//!
//! Only the central directory is held in memory (entry name → location); entry
//! bytes are read and DEFLATE-decompressed on demand. A `base` offset lets a
//! jmod be read as "skip the 4-byte `JM` header, then a standard ZIP".
//!
//! Hardening (threat model): entry names with `..`, a leading `/`, or `\` are
//! rejected; per-entry uncompressed size is capped (zip-bomb); decompression is
//! bounded by the same cap; nothing is ever written to disk. Zip64 entries are
//! skipped rather than trusted. The declared central-directory size and entry
//! count are also capped, checked before either is used to size an
//! allocation or bound a loop.

use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

/// Cap on a single entry's uncompressed size. Class files are small (tens of KB);
/// this is generous while still refusing decompression bombs.
const MAX_UNCOMPRESSED: usize = 16 * 1024 * 1024;

/// Cap on the ZIP central directory's declared size, checked before it is ever
/// used to size an allocation. The EOCD's central-directory-size field is
/// attacker-controlled input; without this bound a crafted archive could
/// claim a multi-gigabyte central directory and force a matching allocation
/// before a single entry is parsed. Real jar/jmod central directories are a
/// small fraction of the archive itself, so this is generous headroom, not a
/// functional limit.
const MAX_CENTRAL_DIRECTORY_SIZE: usize = 64 * 1024 * 1024;

/// Cap on the number of central-directory entries walked while parsing. A
/// crafted central directory could pack an enormous number of minimal
/// (near-zero-length) records into a buffer that is still under
/// `MAX_CENTRAL_DIRECTORY_SIZE`, which would otherwise still cost unbounded
/// time and HashMap insertions to walk. No real jar/jmod comes close to this
/// many entries.
const MAX_CD_ENTRIES: usize = 1_000_000;

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
        if cd_size > MAX_CENTRAL_DIRECTORY_SIZE {
            return None; // refuse before allocating a buffer this size
        }

        let cd = read_at(&mut file, base + cd_offset as u64, cd_size)?;
        let entries = parse_central_directory(&cd)?;
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

/// Returns `None` (reject the whole archive) if the number of candidate
/// records exceeds [`MAX_CD_ENTRIES`], rather than continuing to walk and
/// insert without bound.
fn parse_central_directory(cd: &[u8]) -> Option<HashMap<String, Entry>> {
    let mut entries = HashMap::new();
    let mut p = 0usize;
    let mut count = 0usize;
    while p + 46 <= cd.len() {
        if u32le(cd, p) != Some(CDFH_SIG) {
            break;
        }
        count += 1;
        if count > MAX_CD_ENTRIES {
            return None;
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
    Some(entries)
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

    /// Minimal STORED-method zip: just enough structure to exercise the real
    /// `ZipArchive::open` / `contains` / `read` path end to end.
    fn stored_zip(entries: &[&str]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut central = Vec::new();
        for name in entries {
            let offset = out.len() as u32;
            let n = name.as_bytes();
            // Local file header (empty content, method STORE).
            out.extend_from_slice(&LFH_SIG.to_le_bytes());
            out.extend_from_slice(&[20, 0, 0, 0, 0, 0, 0, 0, 0, 0]); // ver/flags/method/time/date
            out.extend_from_slice(&[0; 12]); // crc, comp, uncomp
            out.extend_from_slice(&(n.len() as u16).to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes());
            out.extend_from_slice(n);
            // Central directory record.
            central.extend_from_slice(&CDFH_SIG.to_le_bytes());
            central.extend_from_slice(&[20, 0, 20, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
            central.extend_from_slice(&[0; 12]); // crc, comp, uncomp
            central.extend_from_slice(&(n.len() as u16).to_le_bytes());
            central.extend_from_slice(&[0; 12]); // extra/comment/disk/attrs
            central.extend_from_slice(&offset.to_le_bytes());
            central.extend_from_slice(n);
        }
        let cd_offset = out.len() as u32;
        out.extend_from_slice(&central);
        out.extend_from_slice(&EOCD_SIG.to_le_bytes());
        out.extend_from_slice(&[0, 0, 0, 0]);
        out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
        out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
        out.extend_from_slice(&(central.len() as u32).to_le_bytes());
        out.extend_from_slice(&cd_offset.to_le_bytes());
        out.extend_from_slice(&[0, 0]);
        out
    }

    /// A normal, small archive must still open and read correctly — the new
    /// caps must never reject a legitimate JAR.
    #[test]
    fn normal_small_zip_still_parses() {
        let dir = std::env::temp_dir().join(format!("jvl-zip-cap-test-ok-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("small.zip");
        std::fs::write(&path, stored_zip(&["a/B.class", "a/C.class"])).unwrap();

        let zip = ZipArchive::open(&path, 0).expect("well-formed small zip must open");
        assert!(zip.contains("a/B.class"));
        assert!(zip.contains("a/C.class"));
        assert_eq!(zip.read("a/B.class"), Some(Vec::new()));

        std::fs::remove_dir_all(&dir).ok();
    }

    /// The EOCD's central-directory-size field is attacker-controlled. Here the
    /// backing file genuinely has enough bytes to satisfy the declared size (a
    /// sparse file, so creating it costs no real allocation or I/O), proving
    /// rejection comes from the new declared-size cap itself — checked before
    /// `open` would otherwise allocate a buffer over `MAX_CENTRAL_DIRECTORY_SIZE`
    /// — rather than incidentally from the pre-existing past-EOF bounds check.
    #[test]
    fn oversized_central_directory_is_rejected() {
        let dir =
            std::env::temp_dir().join(format!("jvl-zip-cap-test-cdsize-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("oversized-cd.zip");

        let cd_size = (MAX_CENTRAL_DIRECTORY_SIZE + 1) as u32;
        let cd_offset = 0u32;
        let file_len = cd_offset as u64 + cd_size as u64 + 22; // cd region + trailing EOCD

        {
            // `set_len` creates a sparse file: the declared size is backed on
            // disk without us actually allocating/writing that many bytes.
            let file = File::create(&path).unwrap();
            file.set_len(file_len).unwrap();
        }
        let mut eocd = Vec::with_capacity(22);
        eocd.extend_from_slice(&EOCD_SIG.to_le_bytes());
        eocd.extend_from_slice(&[0, 0, 0, 0]); // disk numbers
        eocd.extend_from_slice(&[0, 0]); // entries on this disk
        eocd.extend_from_slice(&[0, 0]); // total entries
        eocd.extend_from_slice(&cd_size.to_le_bytes());
        eocd.extend_from_slice(&cd_offset.to_le_bytes());
        eocd.extend_from_slice(&[0, 0]); // comment length
        {
            use std::io::Write;
            let mut file = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
            file.seek(SeekFrom::Start(file_len - 22)).unwrap();
            file.write_all(&eocd).unwrap();
        }

        // The cap must reject this before ever allocating a buffer sized from
        // the over-cap `cd_size` field.
        assert!(ZipArchive::open(&path, 0).is_none());

        std::fs::remove_dir_all(&dir).ok();
    }

    /// A crafted central directory packing more than `MAX_CD_ENTRIES` minimal
    /// records must be rejected outright rather than walked without bound.
    #[test]
    fn entry_count_over_cap_is_rejected() {
        let mut record = [0u8; 46];
        record[0..4].copy_from_slice(&CDFH_SIG.to_le_bytes());
        let cd = record.as_slice().repeat(MAX_CD_ENTRIES + 1);
        assert!(parse_central_directory(&cd).is_none());
    }
}
