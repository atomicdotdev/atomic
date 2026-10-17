//! Versioned, working-copy-scoped filesystem index records.

use crate::change::InodeKind;
use crate::pristine::{PristineError, PristineResult};
use crate::types::{Hash, WorkingCopyId};

pub const FILE_INDEX_V2_KEY_VERSION: u8 = 1;
pub const FILE_INDEX_V2_VERSION: u8 = 1;
pub const FILE_INDEX_V2_VALUE_SIZE: usize = 130;
const KEY_HEADER_SIZE: usize = 1 + WorkingCopyId::SIZE;

/// Fields physically present in a [`FileIndexV2Entry`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FileIndexV2Fields(u16);

impl FileIndexV2Fields {
    pub const DEVICE: Self = Self(1 << 0);
    pub const INODE: Self = Self(1 << 1);
    pub const MTIME: Self = Self(1 << 2);
    pub const CTIME: Self = Self(1 << 3);
    pub const SIZE: Self = Self(1 << 4);
    pub const CONTENT_ID: Self = Self(1 << 5);
    pub const MODE: Self = Self(1 << 6);
    pub const KIND: Self = Self(1 << 7);
    pub const RACY_AFTER: Self = Self(1 << 8);
    pub const CONVERSION_POLICY: Self = Self(1 << 9);
    pub const ALL: Self = Self((1 << 10) - 1);

    pub const fn bits(self) -> u16 {
        self.0
    }

    pub const fn contains(self, field: Self) -> bool {
        self.0 & field.0 == field.0
    }
}

/// A seconds/nanoseconds filesystem timestamp.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct FileIndexTimestamp {
    pub seconds: i64,
    pub nanoseconds: u32,
}

impl FileIndexTimestamp {
    pub fn new(seconds: i64, nanoseconds: u32) -> PristineResult<Self> {
        if nanoseconds >= 1_000_000_000 {
            return Err(codec_error(format!(
                "FILE_INDEX_V2 timestamp has invalid nanoseconds {nanoseconds}"
            )));
        }
        Ok(Self {
            seconds,
            nanoseconds,
        })
    }
}

/// Raw canonical repository path scoped to one persistent working copy.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct FileIndexV2Key {
    pub working_copy: WorkingCopyId,
    pub path: Vec<u8>,
}

impl FileIndexV2Key {
    pub fn new(working_copy: WorkingCopyId, path: Vec<u8>) -> PristineResult<Self> {
        validate_path(&path)?;
        Ok(Self { working_copy, path })
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(KEY_HEADER_SIZE + self.path.len());
        bytes.push(FILE_INDEX_V2_KEY_VERSION);
        bytes.extend_from_slice(self.working_copy.as_bytes());
        bytes.extend_from_slice(&self.path);
        bytes
    }

    pub fn decode(bytes: &[u8]) -> PristineResult<Self> {
        if bytes.len() <= KEY_HEADER_SIZE {
            return Err(codec_error("FILE_INDEX_V2 key is truncated"));
        }
        if bytes[0] != FILE_INDEX_V2_KEY_VERSION {
            return Err(codec_error(format!(
                "unsupported FILE_INDEX_V2 key version {}",
                bytes[0]
            )));
        }
        let working_copy = WorkingCopyId::from_bytes(
            bytes[1..KEY_HEADER_SIZE]
                .try_into()
                .map_err(|_| codec_error("FILE_INDEX_V2 working-copy key is malformed"))?,
        );
        Self::new(working_copy, bytes[KEY_HEADER_SIZE..].to_vec())
    }
}

/// Filesystem identity and canonical graph facts cached for one path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileIndexV2Entry {
    pub fields: FileIndexV2Fields,
    pub device: Option<u64>,
    pub inode: Option<u64>,
    pub mtime: Option<FileIndexTimestamp>,
    pub ctime: Option<FileIndexTimestamp>,
    pub size: Option<u64>,
    pub content_id: Option<Hash>,
    pub canonical_mode: Option<u16>,
    pub canonical_kind: Option<InodeKind>,
    pub racy_after: Option<FileIndexTimestamp>,
    pub conversion_policy: Option<Hash>,
}

impl FileIndexV2Entry {
    #[allow(clippy::too_many_arguments)]
    pub fn complete(
        device: u64,
        inode: u64,
        mtime: FileIndexTimestamp,
        ctime: FileIndexTimestamp,
        size: u64,
        content_id: Hash,
        canonical_mode: u16,
        canonical_kind: InodeKind,
        racy_after: FileIndexTimestamp,
        conversion_policy: Hash,
    ) -> PristineResult<Self> {
        if canonical_mode > 0o777 {
            return Err(codec_error(format!(
                "FILE_INDEX_V2 canonical mode {canonical_mode:#o} contains unsupported bits"
            )));
        }
        Ok(Self {
            fields: FileIndexV2Fields::ALL,
            device: Some(device),
            inode: Some(inode),
            mtime: Some(mtime),
            ctime: Some(ctime),
            size: Some(size),
            content_id: Some(content_id),
            canonical_mode: Some(canonical_mode),
            canonical_kind: Some(canonical_kind),
            racy_after: Some(racy_after),
            conversion_policy: Some(conversion_policy),
        })
    }

    pub fn is_complete(&self) -> bool {
        self.fields == FileIndexV2Fields::ALL
    }

    /// Apply Git's racy-stat rule conservatively.
    ///
    /// `racy_after` records the boundary active when this row was written. The
    /// earlier of that boundary and `index_write_time` is retained, so advancing
    /// either cache cannot make a previously-racy entry appear safe. Missing
    /// timestamp fields are racy and therefore force content verification.
    pub fn is_racy(&self, index_write_time: FileIndexTimestamp) -> bool {
        let (Some(mtime), Some(racy_after)) = (self.mtime, self.racy_after) else {
            return true;
        };
        mtime >= std::cmp::min(racy_after, index_write_time)
    }

    /// Whether metadata can safely avoid content hashing.
    ///
    /// Equality is never trusted for a racy entry. Missing fields, filesystem
    /// identity replacement, timestamp/size changes, canonical mode/type
    /// changes, and conversion-policy changes all force verification.
    pub fn metadata_matches(&self, observed: &Self, index_write_time: FileIndexTimestamp) -> bool {
        self.is_complete()
            && observed.is_complete()
            && !self.is_racy(index_write_time)
            && self.device == observed.device
            && self.inode == observed.inode
            && self.mtime == observed.mtime
            && self.ctime == observed.ctime
            && self.size == observed.size
            && self.canonical_mode == observed.canonical_mode
            && self.canonical_kind == observed.canonical_kind
            && self.conversion_policy == observed.conversion_policy
    }
}

pub fn encode_file_index_v2(
    entry: &FileIndexV2Entry,
) -> PristineResult<[u8; FILE_INDEX_V2_VALUE_SIZE]> {
    validate_entry(entry)?;
    let mut bytes = [0u8; FILE_INDEX_V2_VALUE_SIZE];
    bytes[0] = FILE_INDEX_V2_VERSION;
    bytes[1..3].copy_from_slice(&entry.fields.bits().to_le_bytes());
    put_u64(&mut bytes[3..11], entry.device);
    put_u64(&mut bytes[11..19], entry.inode);
    put_timestamp(&mut bytes[19..31], entry.mtime);
    put_timestamp(&mut bytes[31..43], entry.ctime);
    put_u64(&mut bytes[43..51], entry.size);
    put_hash(&mut bytes[51..83], entry.content_id.as_ref());
    put_u16(&mut bytes[83..85], entry.canonical_mode);
    if let Some(kind) = entry.canonical_kind {
        bytes[85] = kind.as_byte();
    }
    put_timestamp(&mut bytes[86..98], entry.racy_after);
    put_hash(&mut bytes[98..130], entry.conversion_policy.as_ref());
    Ok(bytes)
}

pub fn decode_file_index_v2(bytes: &[u8]) -> PristineResult<FileIndexV2Entry> {
    if bytes.len() != FILE_INDEX_V2_VALUE_SIZE {
        return Err(codec_error(format!(
            "FILE_INDEX_V2 row has length {}, expected {FILE_INDEX_V2_VALUE_SIZE}",
            bytes.len()
        )));
    }
    if bytes[0] != FILE_INDEX_V2_VERSION {
        return Err(codec_error(format!(
            "unsupported FILE_INDEX_V2 row version {}",
            bytes[0]
        )));
    }
    let fields = FileIndexV2Fields(u16::from_le_bytes([bytes[1], bytes[2]]));
    if fields.bits() & !FileIndexV2Fields::ALL.bits() != 0 {
        return Err(codec_error(format!(
            "FILE_INDEX_V2 row has unknown presence flags {:#x}",
            fields.bits()
        )));
    }
    let entry = FileIndexV2Entry {
        fields,
        device: read_u64(&bytes[3..11], fields, FileIndexV2Fields::DEVICE),
        inode: read_u64(&bytes[11..19], fields, FileIndexV2Fields::INODE),
        mtime: read_timestamp(&bytes[19..31], fields, FileIndexV2Fields::MTIME)?,
        ctime: read_timestamp(&bytes[31..43], fields, FileIndexV2Fields::CTIME)?,
        size: read_u64(&bytes[43..51], fields, FileIndexV2Fields::SIZE),
        content_id: read_hash(&bytes[51..83], fields, FileIndexV2Fields::CONTENT_ID),
        canonical_mode: read_u16(&bytes[83..85], fields, FileIndexV2Fields::MODE),
        canonical_kind: if fields.contains(FileIndexV2Fields::KIND) {
            Some(InodeKind::from_byte(bytes[85]).ok_or_else(|| {
                codec_error(format!(
                    "FILE_INDEX_V2 row has unknown inode kind {}",
                    bytes[85]
                ))
            })?)
        } else {
            None
        },
        racy_after: read_timestamp(&bytes[86..98], fields, FileIndexV2Fields::RACY_AFTER)?,
        conversion_policy: read_hash(
            &bytes[98..130],
            fields,
            FileIndexV2Fields::CONVERSION_POLICY,
        ),
    };
    validate_entry(&entry)?;
    Ok(entry)
}

fn validate_path(path: &[u8]) -> PristineResult<()> {
    if path.is_empty() || path[0] == b'/' || path.contains(&0) {
        return Err(codec_error(
            "FILE_INDEX_V2 path is not a canonical relative path",
        ));
    }
    if path
        .split(|byte| *byte == b'/')
        .any(|part| part.is_empty() || part == b"." || part == b"..")
    {
        return Err(codec_error(
            "FILE_INDEX_V2 path has a non-canonical component",
        ));
    }
    Ok(())
}

fn validate_entry(entry: &FileIndexV2Entry) -> PristineResult<()> {
    validate_presence(
        entry.fields,
        FileIndexV2Fields::DEVICE,
        entry.device.is_some(),
        "device",
    )?;
    validate_presence(
        entry.fields,
        FileIndexV2Fields::INODE,
        entry.inode.is_some(),
        "inode",
    )?;
    validate_presence(
        entry.fields,
        FileIndexV2Fields::MTIME,
        entry.mtime.is_some(),
        "mtime",
    )?;
    validate_presence(
        entry.fields,
        FileIndexV2Fields::CTIME,
        entry.ctime.is_some(),
        "ctime",
    )?;
    validate_presence(
        entry.fields,
        FileIndexV2Fields::SIZE,
        entry.size.is_some(),
        "size",
    )?;
    validate_presence(
        entry.fields,
        FileIndexV2Fields::CONTENT_ID,
        entry.content_id.is_some(),
        "content identity",
    )?;
    validate_presence(
        entry.fields,
        FileIndexV2Fields::MODE,
        entry.canonical_mode.is_some(),
        "canonical mode",
    )?;
    validate_presence(
        entry.fields,
        FileIndexV2Fields::KIND,
        entry.canonical_kind.is_some(),
        "canonical kind",
    )?;
    validate_presence(
        entry.fields,
        FileIndexV2Fields::RACY_AFTER,
        entry.racy_after.is_some(),
        "racy boundary",
    )?;
    validate_presence(
        entry.fields,
        FileIndexV2Fields::CONVERSION_POLICY,
        entry.conversion_policy.is_some(),
        "conversion policy",
    )?;
    if entry.canonical_mode.is_some_and(|mode| mode > 0o777) {
        return Err(codec_error(
            "FILE_INDEX_V2 canonical mode contains unsupported bits",
        ));
    }
    Ok(())
}

fn validate_presence(
    fields: FileIndexV2Fields,
    field: FileIndexV2Fields,
    present: bool,
    name: &str,
) -> PristineResult<()> {
    if fields.contains(field) != present {
        return Err(codec_error(format!(
            "FILE_INDEX_V2 {name} presence flag disagrees with value"
        )));
    }
    Ok(())
}

fn put_u64(dst: &mut [u8], value: Option<u64>) {
    if let Some(value) = value {
        dst.copy_from_slice(&value.to_le_bytes());
    }
}
fn put_u16(dst: &mut [u8], value: Option<u16>) {
    if let Some(value) = value {
        dst.copy_from_slice(&value.to_le_bytes());
    }
}
fn put_timestamp(dst: &mut [u8], value: Option<FileIndexTimestamp>) {
    if let Some(value) = value {
        dst[..8].copy_from_slice(&value.seconds.to_le_bytes());
        dst[8..].copy_from_slice(&value.nanoseconds.to_le_bytes());
    }
}
fn put_hash(dst: &mut [u8], value: Option<&Hash>) {
    if let Some(value) = value {
        dst.copy_from_slice(&value.0);
    }
}
fn read_u64(src: &[u8], fields: FileIndexV2Fields, field: FileIndexV2Fields) -> Option<u64> {
    fields
        .contains(field)
        .then(|| u64::from_le_bytes(src.try_into().unwrap()))
}
fn read_u16(src: &[u8], fields: FileIndexV2Fields, field: FileIndexV2Fields) -> Option<u16> {
    fields
        .contains(field)
        .then(|| u16::from_le_bytes(src.try_into().unwrap()))
}
fn read_timestamp(
    src: &[u8],
    fields: FileIndexV2Fields,
    field: FileIndexV2Fields,
) -> PristineResult<Option<FileIndexTimestamp>> {
    if !fields.contains(field) {
        return Ok(None);
    }
    FileIndexTimestamp::new(
        i64::from_le_bytes(src[..8].try_into().unwrap()),
        u32::from_le_bytes(src[8..].try_into().unwrap()),
    )
    .map(Some)
}
fn read_hash(src: &[u8], fields: FileIndexV2Fields, field: FileIndexV2Fields) -> Option<Hash> {
    fields
        .contains(field)
        .then(|| Hash::from(<[u8; 32]>::try_from(src).unwrap()))
}
fn codec_error(message: impl Into<String>) -> PristineError {
    PristineError::Serialization {
        message: message.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn timestamp(seconds: i64, nanoseconds: u32) -> FileIndexTimestamp {
        FileIndexTimestamp::new(seconds, nanoseconds).unwrap()
    }

    fn entry() -> FileIndexV2Entry {
        FileIndexV2Entry::complete(
            7,
            11,
            timestamp(100, 999_999_999),
            timestamp(101, 42),
            23,
            Hash::of(b"content"),
            0o755,
            InodeKind::Regular,
            timestamp(100, 500),
            Hash::of(b"policy"),
        )
        .unwrap()
    }

    #[test]
    fn key_and_row_roundtrip_preserve_nanoseconds_and_raw_path() {
        let key = FileIndexV2Key::new(WorkingCopyId::from_bytes([3; 16]), b"src/\xff.rs".to_vec())
            .unwrap();
        assert_eq!(FileIndexV2Key::decode(&key.encode()).unwrap(), key);
        let expected = entry();
        assert_eq!(
            decode_file_index_v2(&encode_file_index_v2(&expected).unwrap()).unwrap(),
            expected
        );
    }

    #[test]
    fn malformed_and_unknown_versions_fail_closed() {
        let mut row = encode_file_index_v2(&entry()).unwrap();
        row[0] = FILE_INDEX_V2_VERSION + 1;
        assert!(decode_file_index_v2(&row).is_err());
        assert!(decode_file_index_v2(&row[..20]).is_err());
        row[0] = FILE_INDEX_V2_VERSION;
        row[2] |= 0x80;
        assert!(decode_file_index_v2(&row).is_err());
        assert!(FileIndexV2Key::decode(&[9, 0, 1]).is_err());
    }

    #[test]
    fn racy_boundary_and_metadata_changes_force_hashing() {
        let cached = entry();
        assert!(cached.is_racy(timestamp(100, 900)));
        let mut safe = cached.clone();
        safe.mtime = Some(timestamp(99, 999_999_999));
        assert!(safe.metadata_matches(&safe, timestamp(100, 0)));

        let mut replaced = safe.clone();
        replaced.inode = Some(12);
        assert!(!safe.metadata_matches(&replaced, timestamp(100, 0)));
        let mut chmod = safe.clone();
        chmod.canonical_mode = Some(0o644);
        assert!(!safe.metadata_matches(&chmod, timestamp(100, 0)));
        let mut retyped = safe.clone();
        retyped.canonical_kind = Some(InodeKind::Symlink);
        assert!(!safe.metadata_matches(&retyped, timestamp(100, 0)));
        let mut same_size_edit = safe.clone();
        same_size_edit.ctime = Some(timestamp(102, 0));
        assert_eq!(same_size_edit.size, safe.size);
        assert!(!safe.metadata_matches(&same_size_edit, timestamp(100, 0)));
    }
}
