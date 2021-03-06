use core::cmp;
use core::mem;

use arrayvec::ArrayVec;
use futures::future;
use futures::pin_mut;
use futures::stream::{self, Stream, StreamExt, TryStream, TryStreamExt};
use interface::{SysError, SysResult};

use crate::device::ide::{AtaError, Sector};
use crate::device::mbr::Partition;
use crate::mem::MemoryExhausted;
use crate::sync::{Arc, AsyncMutex};

const DIR_ENTRY_SIZE: usize = 32;
const SECTOR_SIZE: usize = 512;

#[derive(Debug)]
pub struct Fat16 {
    fs: Arc<Filesystem>,
}

#[derive(Debug)]
struct Filesystem {
    part: Partition,
    bpb: BiosParameterBlock,
}

#[derive(Debug)]
pub enum OpenError {
    MemoryExhausted,
    Ata(AtaError),
}

#[derive(Debug)]
pub enum FatError {
    MemoryExhausted,
    Ata(AtaError),
}

impl From<FatError> for SysError {
    fn from(e: FatError) -> Self {
        match e {
            FatError::MemoryExhausted => SysError::MemoryExhausted,
            FatError::Ata(_) => SysError::IoError,
        }
    }
}

impl From<AtaError> for FatError {
    fn from(e: AtaError) -> FatError {
        FatError::Ata(e)
    }
}

impl From<MemoryExhausted> for FatError {
    fn from(e: MemoryExhausted) -> FatError {
        FatError::MemoryExhausted
    }
}

#[derive(Debug, Copy, Clone)]
struct ClusterNumber(usize);

impl Fat16 {
    pub async fn open(part: Partition) -> Result<Self, FatError> {
        let bpb = BiosParameterBlock::read(&part).await
            .map_err(FatError::Ata)?;

        let fs = Arc::new(Filesystem { part, bpb })
            .map_err(|_| FatError::MemoryExhausted)?;

        Ok(Fat16 { fs })
    }

    pub fn root(&self) -> Directory {
        Directory {
            fs: self.fs.clone(),
            kind: DirectoryKind::Root,
        }
    }
}

impl Filesystem {
    async fn next_cluster(&self, cluster: ClusterNumber) -> Result<Option<ClusterNumber>, AtaError> {
        const FAT_ENTRY_SIZE: usize = mem::size_of::<u16>();

        let max_cluster = self.bpb.fat_sector_count() * SECTOR_SIZE / FAT_ENTRY_SIZE;

        if cluster.0 >= max_cluster {
            panic!("cluster out of bounds: {:?}", cluster);
        }

        let fat_entry_offset = cluster.0 * FAT_ENTRY_SIZE;

        let fat_sector = self.bpb.first_fat_sector() +
            fat_entry_offset / SECTOR_SIZE;

        let sector_offset = fat_entry_offset % SECTOR_SIZE;

        let mut buff: Sector = [0u8; 512];
        self.part.read_sectors(fat_sector, &mut [&mut buff]).await?;

        let next_lo = buff[sector_offset + 0];
        let next_hi = buff[sector_offset + 1];
        let next = u16::from_le_bytes([next_lo, next_hi]);

        if next >= 0xfff8 {
            Ok(None)
        } else if next == 0xfff7 {
            panic!("bad cluster in chain! what do here?");
        } else {
            Ok(Some(ClusterNumber(next as usize)))
        }
    }

    fn cluster_chain(&self, start: ClusterNumber) -> impl Stream<Item = Result<ClusterNumber, AtaError>> + '_ {
        stream::unfold(Some(start), move |cluster| async move {
            match cluster {
                Some(cluster) => {
                    match self.next_cluster(cluster).await.transpose()? {
                        Ok(next) => Some((Ok(cluster), Some(next))),
                        Err(e) => Some((Err(e), None)),
                    }
                }
                None => None,
            }
        })
    }

    fn sector_chain(&self, start: ClusterNumber) -> impl Stream<Item = Result<usize, AtaError>> + '_ {
        self.cluster_chain(start)
            .map(move |cluster| {
                cluster.map(|cluster| stream::iter(self.bpb.cluster_sectors(cluster).map(Ok)))
            })
            .try_flatten()
    }
}

#[derive(Debug)]
enum DirectoryKind {
    Root,
    Sub(DirEntry),
}

#[derive(Debug)]
pub struct Directory {
    fs: Arc<Filesystem>,
    kind: DirectoryKind,
}

impl Directory {
    fn directory_sectors(&self) -> impl TryStream<Ok = usize, Error = AtaError> + '_ {
        match &self.kind {
            DirectoryKind::Root => {
                let first_sector = self.fs.bpb.first_root_dir_sector();
                let sector_count = self.fs.bpb.root_dir_sector_count();
                let sectors = first_sector..(first_sector + sector_count);

                stream::iter(sectors.into_iter().map(Ok))
                    .left_stream()
            }
            DirectoryKind::Sub(dirent) => {
                self.fs.sector_chain(dirent.dirent().first_cluster())
                    .right_stream()
            }
        }
    }

    /// TODO this only reads the first sector/cluster worth of entries:
    fn read_entries(&self) -> impl TryStream<Ok = RawDirEntry, Error = FatError> + '_ {
        async fn read_raw_entries_from_sector(fs: &Filesystem, sector: usize)
            -> Result<ArrayVec<[RawDirEntry; 16]>, FatError>
        {
            let mut buff: Sector = [0u8; 512];
            fs.part.read_sectors(sector, &mut [&mut buff]).await?;

            let entries = unsafe { mem::transmute::<&Sector, &[RawDirEntry; 16]>(&buff) };

            Ok(entries.iter().cloned().collect())
        }

        let fs = &self.fs;

        self.directory_sectors()
            .map_err(FatError::Ata)
            .and_then(move |sector| async move {
                let raw_entries = read_raw_entries_from_sector(fs, sector).await?;
                Ok(stream::iter(raw_entries.into_iter().map(Ok)))
            })
            .try_flatten()
            .try_filter(|entry| future::ready(entry.basename[0] != 0xef)) // deleted file
            .take_while(|entry| future::ready(entry.as_ref().map(|e| e.basename[0] != 0).unwrap_or(true))) // end
    }

    pub fn entries(&self) -> impl TryStream<Ok = DirEntry, Error = FatError> + '_ {
        self.read_entries()
            .and_then(move |dirent| {
                let parent = match &self.kind {
                    DirectoryKind::Root => None,
                    DirectoryKind::Sub(dirent) => Some(dirent.clone()),
                };

                future::ready(
                    DirEntry::new(self.fs.clone(), parent, dirent)
                        .map_err(|e| e.into()))
            })
    }

    pub async fn entry(&self, name: &[u8]) -> Result<Option<DirEntry>, FatError> {
        let entries = self
            .entries()
            .try_filter(|entry| {
                let entry_name = entry.name();
                crate::println!("--- {:?}", core::str::from_utf8(&entry_name).expect("utf8"));
                crate::println!("    {:?}", core::str::from_utf8(&name).expect("utf8"));
                crate::println!();
                future::ready(&entry_name == name)
            });
        pin_mut!(entries);

        entries.try_next().await
    }
}

// TODO implement directory entry locking/unlocking logic on this type
#[derive(Debug)]
pub struct DirEntryShared {
    fs: Arc<Filesystem>,
    parent: Option<DirEntry>,
    dirent: RawDirEntry,
}

impl Drop for DirEntryShared {
    fn drop(&mut self) {
        // TOOD implement unlocking logic here..
    }
}

#[derive(Clone, Debug)]
pub struct DirEntry {
    shared: Arc<DirEntryShared>,
}

impl DirEntry {
    fn new(fs: Arc<Filesystem>, parent: Option<DirEntry>, dirent: RawDirEntry) -> Result<Self, MemoryExhausted> {
        Ok(DirEntry {
            shared: Arc::new(DirEntryShared {
                fs,
                parent,
                dirent,
            })?,
        })
    }

    fn dirent(&self) -> &RawDirEntry {
        &self.shared.dirent
    }

    pub fn name(&self) -> ArrayVec<[u8; 12]> {
        self.dirent().filename()
    }

    pub fn is_dir(&self) -> bool {
        self.dirent().attributes().contains(Attributes::DIRECTORY)
    }

    pub fn open(&self) -> Result<Open, FatError> {
        let fs = self.shared.fs.clone();

        if self.is_dir() {
            Ok(Open::Dir(Directory {
                fs,
                kind: DirectoryKind::Sub(self.clone()),
            }))
        } else {
            let seek = Seek {
                cluster: Some(self.dirent().first_cluster()),
                sector: 0,
                offset: 0,
            };

            Ok(Open::File(File {
                fs,
                dirent: self.clone(),
                seek: AsyncMutex::new(seek),
            }))
        }
    }
}

#[derive(Debug)]
pub enum Open {
    File(File),
    Dir(Directory),
}

#[derive(Debug)]
struct Seek {
    cluster: Option<ClusterNumber>,
    sector: usize,
    offset: usize,
}

#[derive(Debug)]
pub struct File {
    fs: Arc<Filesystem>,
    dirent: DirEntry,
    seek: AsyncMutex<Seek>,
}

impl File {
    pub async fn read(&self, mut buf: &mut [u8]) -> Result<usize, FatError> {
        let mut seek = self.seek.lock().await?;
        let mut total_read = 0;

        while buf.len() > 0 {
            if seek.offset == SECTOR_SIZE {
                seek.offset = 0;
                seek.sector += 1;
            }

            if seek.sector == self.fs.bpb.sectors_per_cluster() {
                seek.sector = 0;

                seek.cluster = match seek.cluster {
                    None => None,
                    Some(cluster) => self.fs.next_cluster(cluster).await?,
                };
            }

            let sector = match seek.cluster {
                None => {
                    // EOF
                    return Ok(total_read);
                }
                Some(cluster) => {
                    self.fs.bpb.first_cluster_sector(cluster) + seek.sector
                }
            };

            let mut sector_buff: Sector = [0; SECTOR_SIZE];
            // TODO make this read multiple sectors at a time:
            self.fs.part.read_sectors(sector, &mut [&mut sector_buff])
                .await
                .map_err(FatError::Ata)?;

            let byte_count = cmp::min(SECTOR_SIZE - seek.offset, buf.len());

            let end = seek.offset + byte_count;

            buf[0..byte_count].copy_from_slice(&sector_buff[seek.offset..end]);

            buf = &mut buf[byte_count..];
            total_read += byte_count;
            seek.offset += byte_count;
        }

        Ok(total_read)
    }
}

#[repr(packed)]
#[derive(Clone, Copy, Debug)]
pub struct RawDirEntry {
    basename: [u8; 8],
    extension: [u8; 3],
    attributes: u8,
    _reserved: u8,
    create_tenths: u8,
    create_time: PackedTime,
    create_date: PackedDate,
    access_date: PackedDate,
    cluster_hi: u16,
    modify_time: PackedTime,
    modify_date: PackedTime,
    cluster_lo: u16,
    size: u32,
}

bitflags::bitflags! {
    pub struct Attributes: u8 {
        const READ_ONLY = 0x01;
        const HIDDEN    = 0x02;
        const SYSTEM    = 0x04;
        const VOLUME_ID = 0x08;
        const DIRECTORY = 0x10;
        const ARCHIVE   = 0x20;
    }
}

impl RawDirEntry {
    pub fn attributes(&self) -> Attributes {
        Attributes::from_bits_truncate(self.attributes)
    }

    pub fn filename(&self) -> ArrayVec<[u8; 12]> {
        let mut filename = ArrayVec::new();

        filename.extend(self.basename.iter()
            .map(u8::to_ascii_lowercase));

        // trim trailing space
        while filename.last() == Some(&b' ') {
            filename.pop();
        }

        if self.extension[0] != b' ' {
            filename.push(b'.');

            filename.extend(self.extension.iter()
                .map(u8::to_ascii_lowercase));

            // trim trailing space again
            while filename.last() == Some(&b' ') {
                filename.pop();
            }
        }

        filename
    }

    fn first_cluster(&self) -> ClusterNumber {
        let cluster_lo = self.cluster_lo as usize;
        let cluster_hi = self.cluster_hi as usize;
        ClusterNumber(cluster_lo | (cluster_hi << 16))
    }
}

#[repr(packed)]
#[derive(Clone, Copy, Debug)]
struct PackedTime {
    hms: u16,
}

#[repr(packed)]
#[derive(Clone, Copy, Debug)]
struct PackedDate {
    ymd: u16,
}

#[repr(packed)]
#[derive(Clone, Copy, Debug)]
struct BiosParameterBlock {
    // 0x0
    jmp: [u8; 3],
    // 0x3
    oem: [u8; 8],
    // 0xb
    bytes_per_sector: u16,
    // 0xd
    sectors_per_cluster: u8,
    // 0xe
    reserved_sector_count: u16,
    fat_count: u8,
    root_directory_entry_count: u16,
    total_sector_count: u16,
    media_descriptor_type: u8,
    sectors_per_fat: u16,
    // more stuff but we don't use it
}

impl BiosParameterBlock {
    pub fn first_fat_sector(&self) -> usize {
        self.reserved_sector_count as usize
    }

    pub fn fat_sector_count(&self) -> usize {
        self.sectors_per_fat as usize
    }

    pub fn all_fats_sector_count(&self) -> usize {
        self.fat_count as usize * self.fat_sector_count()
    }

    pub fn first_root_dir_sector(&self) -> usize {
        self.first_fat_sector() + self.all_fats_sector_count()
    }

    pub fn root_dir_sector_count(&self) -> usize {
        (self.root_directory_entry_count as usize * DIR_ENTRY_SIZE) / SECTOR_SIZE
    }

    pub fn first_cluster_sector(&self, cluster_number: ClusterNumber) -> usize {
        let clusters_base = self.first_root_dir_sector() + self.root_dir_sector_count();

        // cluster numbers are 2-indexed:
        let cluster_number = cluster_number.0 - 2;

        clusters_base + cluster_number * self.sectors_per_cluster()
    }

    pub fn sectors_per_cluster(&self) -> usize {
        self.sectors_per_cluster as usize
    }

    pub fn cluster_sectors(&self, cluster_number: ClusterNumber) -> impl Iterator<Item = usize> {
        let first = self.first_cluster_sector(cluster_number);
        let count = self.sectors_per_cluster();

        (first..(first + count)).into_iter()
    }
}

impl BiosParameterBlock {
    pub async fn read(part: &Partition) -> Result<BiosParameterBlock, AtaError> {
        let mut buff: Sector = [0; 512];
        part.read_sectors(0, &mut [&mut buff]).await?;

        let bpb = unsafe {
            mem::transmute::<&Sector, &BiosParameterBlock>(&buff).clone()
        };

        Ok(bpb)
    }
}
