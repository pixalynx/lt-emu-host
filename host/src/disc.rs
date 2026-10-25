//! The disc the core reads: a raw image (2352-byte sectors) with patched sectors laid over it.
//! The core asks for raw sectors through [`psx_iso::TrackSource`] on every CD read and keeps no
//! copy, so a sector patched here is what the game reads the next time it seeks there.

use std::collections::HashMap;
use std::fs::File;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

use lt_emu_protocol::RAW_SECTOR;

pub type Raw = [u8; RAW_SECTOR];

/// A read-only raw image. Reads are positional, so one handle serves every thread.
pub struct Image {
    file: File,
    sectors: u32,
}

impl Image {
    pub fn open(path: &Path) -> Result<Self, String> {
        let file = File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let size = file.metadata().map_err(|e| format!("{}: {e}", path.display()))?.len();
        if size == 0 || size % RAW_SECTOR as u64 != 0 {
            return Err(format!("{} isn't a raw disc image (2352-byte sectors): {size} bytes", path.display()));
        }
        Ok(Self { file, sectors: (size / RAW_SECTOR as u64) as u32 })
    }

    pub fn sectors(&self) -> u32 {
        self.sectors
    }

    fn read_raw(&self, lba: u32, out: &mut Raw) -> bool {
        read_at(&self.file, lba as u64 * RAW_SECTOR as u64, out)
    }
}

#[cfg(unix)]
fn read_at(f: &File, offset: u64, out: &mut [u8]) -> bool {
    use std::os::unix::fs::FileExt;
    f.read_exact_at(out, offset).is_ok()
}

#[cfg(windows)]
fn read_at(f: &File, mut offset: u64, mut out: &mut [u8]) -> bool {
    use std::os::windows::fs::FileExt;
    while !out.is_empty() {
        match f.seek_read(out, offset) {
            Ok(0) | Err(_) => return false,
            Ok(n) => {
                offset += n as u64;
                out = &mut out[n..];
            }
        }
    }
    true
}

/// The sector a disc has past its last track: sync, a Mode 2 header with its address, the rest zero.
pub fn postgap(lba: u32) -> Raw {
    let mut raw = [0u8; RAW_SECTOR];
    raw[1..11].fill(0xFF);
    let a = lba + 150;
    let bcd = |n: u32| (((n / 10) << 4) | (n % 10)) as u8;
    raw[12] = bcd(a / 4500);
    raw[13] = bcd((a / 75) % 60);
    raw[14] = bcd(a % 75);
    raw[15] = 2;
    raw
}

/// The image and the sectors patched over it. Shared by the emulator thread (reads, through
/// [`Source`]) and the request handler (patches).
pub struct Overlay {
    image: Image,
    /// The disc's length: the image's, or more when patches append.
    sectors: u32,
    patches: RwLock<HashMap<u32, Box<Raw>>>,
    reads: AtomicU64,
    patched_reads: AtomicU64,
}

impl Overlay {
    pub fn new(image: Image, sectors: u32) -> Self {
        let sectors = sectors.max(image.sectors());
        Self { image, sectors, patches: RwLock::new(HashMap::new()), reads: AtomicU64::new(0), patched_reads: AtomicU64::new(0) }
    }

    pub fn sectors(&self) -> u32 {
        self.sectors
    }

    pub fn patched(&self) -> usize {
        self.patches.read().expect("overlay lock").len()
    }

    /// Sectors read, and how many of them came from patches.
    pub fn read_counts(&self) -> (u64, u64) {
        (self.reads.load(Ordering::Relaxed), self.patched_reads.load(Ordering::Relaxed))
    }

    /// Sets and clears patches in one swap, between two reads. A sector that isn't 2352 bytes is an error.
    pub fn patch(&self, set: Vec<(u32, Vec<u8>)>, clear: &[u32]) -> Result<(), String> {
        let mut boxed = Vec::with_capacity(set.len());
        for (lba, raw) in set {
            let raw: Raw = raw.try_into().map_err(|v: Vec<u8>| format!("sector {lba} is {} bytes, not {RAW_SECTOR}", v.len()))?;
            boxed.push((lba, Box::new(raw)));
        }
        let mut p = self.patches.write().expect("overlay lock");
        for lba in clear {
            p.remove(lba);
        }
        p.extend(boxed);
        Ok(())
    }

    /// The raw sector at `lba` as the core sees it.
    #[cfg(test)]
    pub fn read_raw(&self, lba: u32) -> Option<Raw> {
        if lba >= self.sectors {
            return None;
        }
        if let Some(raw) = self.patches.read().expect("overlay lock").get(&lba) {
            return Some(**raw);
        }
        let mut out = [0u8; RAW_SECTOR];
        if lba < self.image.sectors() {
            self.image.read_raw(lba, &mut out).then_some(out)
        } else {
            Some(postgap(lba))
        }
    }
}

/// The [`psx_iso::TrackSource`] the core's disc reads through.
pub struct Source(pub Arc<Overlay>);

impl psx_iso::TrackSource for Source {
    fn len(&self) -> u64 {
        self.0.sectors as u64 * RAW_SECTOR as u64
    }

    fn read_at(&self, offset: u64, out: &mut [u8]) -> bool {
        let end = offset + out.len() as u64;
        if end > self.len() {
            return false;
        }
        let o = &self.0;
        let first = (offset / RAW_SECTOR as u64) as u32;
        let last = ((end.max(1) - 1) / RAW_SECTOR as u64) as u32;
        let patches = o.patches.read().expect("overlay lock");
        let mut written = 0usize;
        let mut sector = [0u8; RAW_SECTOR];
        for lba in first..=last {
            let start = if lba == first { (offset % RAW_SECTOR as u64) as usize } else { 0 };
            let take = (RAW_SECTOR - start).min(out.len() - written);
            let dst = &mut out[written..written + take];
            o.reads.fetch_add(1, Ordering::Relaxed);
            if let Some(raw) = patches.get(&lba) {
                o.patched_reads.fetch_add(1, Ordering::Relaxed);
                dst.copy_from_slice(&raw[start..start + take]);
            } else if lba < o.image.sectors() {
                if !o.image.read_raw(lba, &mut sector) {
                    return false;
                }
                dst.copy_from_slice(&sector[start..start + take]);
            } else {
                dst.copy_from_slice(&postgap(lba)[start..start + take]);
            }
            written += take;
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use psx_iso::TrackSource;

    /// A 4-sector image whose sector n is filled with byte n.
    fn image(dir: &Path) -> Image {
        let path = dir.join("tiny.bin");
        let bytes: Vec<u8> = (0..4u8).flat_map(|n| std::iter::repeat_n(n, RAW_SECTOR)).collect();
        std::fs::write(&path, bytes).unwrap();
        Image::open(&path).unwrap()
    }

    #[test]
    fn a_patch_is_what_the_next_read_sees() {
        let dir = std::env::temp_dir().join(format!("lt-emu-host-disc-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let o = Arc::new(Overlay::new(image(&dir), 5));
        let src = Source(o.clone());
        assert_eq!(src.len(), 5 * RAW_SECTOR as u64);
        let mut out = vec![0u8; RAW_SECTOR + 10];
        // A read across sectors 1 and 2, from the image.
        assert!(src.read_at(RAW_SECTOR as u64 * 2 - 5, &mut out));
        assert_eq!((out[0], out[5]), (1, 2));

        o.patch(vec![(2, vec![0xAB; RAW_SECTOR]), (4, vec![0xCD; RAW_SECTOR])], &[]).unwrap();
        assert!(src.read_at(RAW_SECTOR as u64 * 2 - 5, &mut out));
        assert_eq!((out[0], out[5]), (1, 0xAB));
        // Sector 4 is past the image: a patch appended it.
        assert_eq!(o.read_raw(4).unwrap()[0], 0xCD);
        assert_eq!(o.read_counts().1, 1);

        // Clearing sector 2 puts the image's back; a short sector is refused.
        o.patch(Vec::new(), &[2]).unwrap();
        assert_eq!(o.read_raw(2).unwrap()[0], 2);
        assert!(o.patch(vec![(1, vec![0; 100])], &[]).is_err());
        assert!(!src.read_at(RAW_SECTOR as u64 * 5 - 1, &mut out), "past the end");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn postgap_has_its_address() {
        let raw = postgap(185_985);
        assert_eq!(&raw[12..16], &[0x41, 0x21, 0x60, 2]);
        assert_eq!(raw[0], 0);
        assert_eq!(raw[11], 0);
    }
}
