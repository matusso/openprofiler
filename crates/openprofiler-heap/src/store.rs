//! Typed, memory-mapped arrays persisted in the analysis cache directory.

use bytemuck::Pod;
use memmap2::Mmap;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::marker::PhantomData;
use std::path::Path;

/// A read-only array of `T` backed by a memory-mapped file.
pub(crate) struct Arr<T: Pod> {
    map: Option<Mmap>,
    _p: PhantomData<T>,
}

impl<T: Pod> Arr<T> {
    pub fn open(path: &Path) -> std::io::Result<Self> {
        let f = File::open(path)?;
        let len = f.metadata()?.len();
        if len == 0 {
            return Ok(Self { map: None, _p: PhantomData });
        }
        if len % std::mem::size_of::<T>() as u64 != 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("{} has a bad size", path.display()),
            ));
        }
        // SAFETY: cache files are only written before the meta marker is committed and never
        // modified while mapped.
        let map = unsafe { Mmap::map(&f)? };
        Ok(Self { map: Some(map), _p: PhantomData })
    }
}

impl<T: Pod> std::ops::Deref for Arr<T> {
    type Target = [T];

    fn deref(&self) -> &[T] {
        match &self.map {
            Some(m) => bytemuck::cast_slice(&m[..]),
            None => &[],
        }
    }
}

pub(crate) fn write_slice<T: Pod>(path: &Path, data: &[T]) -> std::io::Result<()> {
    let mut w = BufWriter::with_capacity(4 << 20, File::create(path)?);
    for chunk in data.chunks(1 << 20) {
        w.write_all(bytemuck::cast_slice(chunk))?;
    }
    w.flush()
}

/// Appends typed values to a file in batches.
pub(crate) struct Appender<T: Pod> {
    w: BufWriter<File>,
    pub len: u64,
    _p: PhantomData<T>,
}

impl<T: Pod> Appender<T> {
    pub fn create(path: &Path) -> std::io::Result<Self> {
        Ok(Self { w: BufWriter::with_capacity(8 << 20, File::create(path)?), len: 0, _p: PhantomData })
    }

    pub fn extend(&mut self, data: &[T]) -> std::io::Result<()> {
        self.len += data.len() as u64;
        self.w.write_all(bytemuck::cast_slice(data))
    }

    pub fn finish(mut self) -> std::io::Result<()> {
        self.w.flush()
    }
}

/// A raw pointer that can be shared across rayon workers writing to disjoint indices.
#[derive(Clone, Copy)]
pub(crate) struct SyncPtr<T>(pub *mut T);
unsafe impl<T> Send for SyncPtr<T> {}
unsafe impl<T> Sync for SyncPtr<T> {}

impl<T> SyncPtr<T> {
    /// # Safety
    /// `i` must be in bounds and no other thread may access index `i` concurrently.
    #[inline]
    pub unsafe fn write(self, i: usize, v: T) {
        unsafe { self.0.add(i).write(v) }
    }

    /// # Safety
    /// `[start, start + len)` must be in bounds and not accessed concurrently by others.
    #[inline]
    pub unsafe fn slice_mut<'a>(self, start: usize, len: usize) -> &'a mut [T] {
        unsafe { std::slice::from_raw_parts_mut(self.0.add(start), len) }
    }
}

/// Sorts every CSR row in place, in parallel. `offsets` has `rows + 1` entries.
pub(crate) fn sort_rows_by<F>(offsets: &[u64], data: &mut [u32], cmp: F)
where
    F: Fn(&u32, &u32) -> std::cmp::Ordering + Sync,
{
    use rayon::prelude::*;
    let rows = offsets.len().saturating_sub(1);
    let ptr = SyncPtr(data.as_mut_ptr());
    let chunk = 1 << 16;
    (0..rows.div_ceil(chunk)).into_par_iter().for_each(|c| {
        let a = c * chunk;
        let b = ((c + 1) * chunk).min(rows);
        for r in a..b {
            let (s, e) = (offsets[r] as usize, offsets[r + 1] as usize);
            if e - s > 1 {
                // SAFETY: rows are disjoint ranges of `data`.
                let row = unsafe { ptr.slice_mut(s, e - s) };
                row.sort_unstable_by(&cmp);
            }
        }
    });
}

/// A writable array backed by a shared file mapping (dirty pages are file-backed, so the kernel
/// can write them back under memory pressure instead of swapping).
pub(crate) struct MutArr<T: Pod> {
    map: Option<memmap2::MmapMut>,
    _p: PhantomData<T>,
}

impl<T: Pod> MutArr<T> {
    pub fn create(path: &Path, len: usize) -> std::io::Result<Self> {
        let f = std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(true).open(path)?;
        f.set_len((len * std::mem::size_of::<T>()) as u64)?;
        if len == 0 {
            return Ok(Self { map: None, _p: PhantomData });
        }
        // SAFETY: the file was just created by us and is not mapped elsewhere.
        let map = unsafe { memmap2::MmapMut::map_mut(&f)? };
        Ok(Self { map: Some(map), _p: PhantomData })
    }

    pub fn flush(&self) -> std::io::Result<()> {
        match &self.map {
            Some(m) => m.flush(),
            None => Ok(()),
        }
    }
}

impl<T: Pod> std::ops::Deref for MutArr<T> {
    type Target = [T];

    fn deref(&self) -> &[T] {
        match &self.map {
            Some(m) => bytemuck::cast_slice(&m[..]),
            None => &[],
        }
    }
}

impl<T: Pod> std::ops::DerefMut for MutArr<T> {
    fn deref_mut(&mut self) -> &mut [T] {
        match &mut self.map {
            Some(m) => bytemuck::cast_slice_mut(&mut m[..]),
            None => &mut [],
        }
    }
}

/// A fixed-length array in an anonymous memory mapping. Unlike heap allocations, its pages are
/// returned to the OS as soon as it is dropped, which keeps the build's peak footprint at the
/// size of the arrays alive in the current phase.
pub struct Big<T: Pod> {
    map: Option<memmap2::MmapMut>,
    len: usize,
    _p: PhantomData<T>,
}

impl<T: Pod> Big<T> {
    pub fn zeroed(len: usize) -> Self {
        let bytes = len * std::mem::size_of::<T>();
        let map = (bytes > 0).then(|| memmap2::MmapMut::map_anon(bytes).expect("anonymous mapping"));
        Self { map, len, _p: PhantomData }
    }

    pub fn filled(len: usize, v: T) -> Self {
        let mut b = Self::zeroed(len);
        b.fill(v);
        b
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

impl<T: Pod> std::ops::Deref for Big<T> {
    type Target = [T];

    fn deref(&self) -> &[T] {
        match &self.map {
            Some(m) => bytemuck::cast_slice(&m[..]),
            None => &[],
        }
    }
}

impl<T: Pod> std::ops::DerefMut for Big<T> {
    fn deref_mut(&mut self) -> &mut [T] {
        match &mut self.map {
            Some(m) => bytemuck::cast_slice_mut(&mut m[..]),
            None => &mut [],
        }
    }
}
