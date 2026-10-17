//! Minimal reader for HotSpot's exported `gHotSpotVMStructs` / `gHotSpotVMTypes` tables (the
//! serviceability agent's view of VM internals). The async sampler uses it to find a thread's
//! last-Java-frame anchor and to look up code blobs by PC; everything else stays behind JVMTI.

// Only `park` (type sizes) is used outside Linux.
#![cfg_attr(not(target_os = "linux"), allow(dead_code))]

use std::collections::HashMap;
use std::ffi::{CStr, c_char, c_void};

/// Reads the value of an exported libjvm variable of type `T`.
fn var<T: Copy>(name: &CStr) -> Option<T> {
    let p = symbol(name)?;
    // SAFETY: the symbol is a variable of type `T` in libjvm.
    Some(unsafe { *(p as *const T) })
}

#[cfg(unix)]
fn symbol(name: &CStr) -> Option<*const c_void> {
    // SAFETY: dlsym with RTLD_DEFAULT and a NUL-terminated name.
    let p = unsafe { libc::dlsym(libc::RTLD_DEFAULT, name.as_ptr()) };
    (!p.is_null()).then_some(p as *const c_void)
}

#[cfg(windows)]
fn symbol(name: &CStr) -> Option<*const c_void> {
    use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleA, GetProcAddress};
    // SAFETY: looks up an export of the already loaded jvm.dll; both names are NUL-terminated.
    unsafe {
        let jvm = GetModuleHandleA(c"jvm.dll".as_ptr() as _);
        if jvm.is_null() {
            return None;
        }
        GetProcAddress(jvm, name.as_ptr() as _).map(|f| f as *const c_void)
    }
}

fn c_string(p: *const c_void) -> Option<String> {
    // SAFETY: VMStructs entries hold NUL-terminated C strings or null.
    (!p.is_null()).then(|| unsafe { CStr::from_ptr(p as *const c_char) }.to_string_lossy().into_owned())
}

#[derive(Debug, Clone)]
pub struct Field {
    /// Byte offset of a non-static field.
    pub offset: u64,
    /// Address of a static field.
    pub address: usize,
    /// C++ type, e.g. `int` or `int16_t`.
    pub type_name: String,
}

/// Field and type tables, restricted to the types the caller asked for.
#[derive(Debug, Default)]
pub struct VmStructs {
    fields: HashMap<(String, String), Field>,
    sizes: HashMap<String, u64>,
}

impl VmStructs {
    pub fn load(types: &[&str]) -> Self {
        let mut s = Self::default();
        s.load_fields(types);
        s.load_sizes(types);
        s
    }

    fn load_fields(&mut self, types: &[&str]) {
        let (
            Some(base),
            Some(stride),
            Some(ty),
            Some(field),
            Some(type_string),
            Some(is_static),
            Some(offset),
            Some(addr),
        ) = (
            var::<*const u8>(c"gHotSpotVMStructs"),
            var::<u64>(c"gHotSpotVMStructEntryArrayStride"),
            var::<u64>(c"gHotSpotVMStructEntryTypeNameOffset"),
            var::<u64>(c"gHotSpotVMStructEntryFieldNameOffset"),
            var::<u64>(c"gHotSpotVMStructEntryTypeStringOffset"),
            var::<u64>(c"gHotSpotVMStructEntryIsStaticOffset"),
            var::<u64>(c"gHotSpotVMStructEntryOffsetOffset"),
            var::<u64>(c"gHotSpotVMStructEntryAddressOffset"),
        )
        else {
            return;
        };
        if base.is_null() || stride == 0 {
            return;
        }
        // The table ends with an entry whose type name is null; the bound only guards against a
        // malformed table.
        for i in 0..100_000usize {
            // SAFETY: `base` points to an array of `stride`-sized entries; the reads use the
            // VM-exported offsets of the entry's members.
            unsafe {
                let e = base.add(i * stride as usize);
                let at = |off: u64| e.add(off as usize);
                let Some(t) = c_string(*(at(ty) as *const *const c_void)) else { break };
                if !types.contains(&t.as_str()) {
                    continue;
                }
                let Some(f) = c_string(*(at(field) as *const *const c_void)) else { continue };
                let is_static = *(at(is_static) as *const i32) != 0;
                self.fields.insert(
                    (t, f),
                    Field {
                        offset: if is_static { 0 } else { *(at(offset) as *const u64) },
                        address: if is_static { *(at(addr) as *const usize) } else { 0 },
                        type_name: c_string(*(at(type_string) as *const *const c_void)).unwrap_or_default(),
                    },
                );
            }
        }
    }

    fn load_sizes(&mut self, types: &[&str]) {
        let (Some(base), Some(stride), Some(name), Some(size)) = (
            var::<*const u8>(c"gHotSpotVMTypes"),
            var::<u64>(c"gHotSpotVMTypeEntryArrayStride"),
            var::<u64>(c"gHotSpotVMTypeEntryTypeNameOffset"),
            var::<u64>(c"gHotSpotVMTypeEntrySizeOffset"),
        ) else {
            return;
        };
        if base.is_null() || stride == 0 {
            return;
        }
        for i in 0..100_000usize {
            // SAFETY: as in `load_fields`.
            unsafe {
                let e = base.add(i * stride as usize);
                let Some(t) = c_string(*(e.add(name as usize) as *const *const c_void)) else { break };
                if types.contains(&t.as_str()) {
                    self.sizes.insert(t, *(e.add(size as usize) as *const u64));
                }
            }
        }
    }

    pub fn field(&self, ty: &str, name: &str) -> Option<&Field> {
        self.fields.get(&(ty.to_string(), name.to_string()))
    }

    pub fn offset(&self, ty: &str, name: &str) -> Option<usize> {
        self.field(ty, name).map(|f| f.offset as usize)
    }

    pub fn size(&self, ty: &str) -> Option<usize> {
        self.sizes.get(ty).map(|&s| s as usize)
    }
}

/// Width of an integer field, from its VMStructs type string.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IntWidth {
    I16,
    I32,
}

impl IntWidth {
    fn of(type_name: &str) -> Option<Self> {
        match type_name {
            "int" | "int32_t" | "jint" => Some(Self::I32),
            "int16_t" | "short" | "jshort" => Some(Self::I16),
            _ => None,
        }
    }

    /// # Safety
    /// `p` must point to a readable field of this width.
    pub unsafe fn read(self, p: usize) -> i32 {
        // SAFETY: guaranteed by the caller.
        unsafe {
            match self {
                Self::I16 => std::ptr::read_volatile(p as *const i16) as i32,
                Self::I32 => std::ptr::read_volatile(p as *const i32),
            }
        }
    }

    /// # Safety
    /// `p` must point to a writable field of this width.
    pub unsafe fn write(self, p: usize, v: i32) {
        // SAFETY: guaranteed by the caller.
        unsafe {
            match self {
                Self::I16 => std::ptr::write_volatile(p as *mut i16, v as i16),
                Self::I32 => std::ptr::write_volatile(p as *mut i32, v),
            }
        }
    }
}

/// Code-cache lookup by PC (the algorithm of `CodeHeap::find_start`), plus the `CodeBlob` fields
/// the sampler needs. All reads are of live VM memory and are async-signal-safe.
#[derive(Debug, Clone)]
pub struct CodeCache {
    heaps: Vec<usize>,
    memory_low_boundary: usize,
    memory_high: usize,
    memory_low: usize,
    segmap_low: usize,
    log2_segment_size: usize,
    heap_block_size: usize,
    block_used: usize,
    frame_complete: usize,
    frame_complete_width: IntWidth,
    frame_size: usize,
    frame_size_width: IntWidth,
}

impl CodeCache {
    pub fn load() -> Option<Self> {
        let v = VmStructs::load(&[
            "CodeCache",
            "CodeHeap",
            "VirtualSpace",
            "HeapBlock",
            "HeapBlock::Header",
            "CodeBlob",
            "GrowableArrayBase",
            "GenericGrowableArray",
            "GrowableArray<int>",
        ]);
        let heaps_var = v.field("CodeCache", "_heaps")?.address;
        let len =
            v.offset("GrowableArrayBase", "_len").or_else(|| v.offset("GenericGrowableArray", "_len"))?;
        let data = v.offset("GrowableArray<int>", "_data")?;
        let memory = v.offset("CodeHeap", "_memory")?;
        let segmap = v.offset("CodeHeap", "_segmap")?;
        let vs_low_boundary = v.offset("VirtualSpace", "_low_boundary")?;
        let vs_low = v.offset("VirtualSpace", "_low")?;
        let vs_high = v.offset("VirtualSpace", "_high")?;
        let header = v.offset("HeapBlock", "_header")?;
        let used = v.offset("HeapBlock::Header", "_used")?;
        let fc = v.field("CodeBlob", "_frame_complete_offset")?;
        let fs = v.field("CodeBlob", "_frame_size")?;
        let cache = Self {
            heaps: {
                // SAFETY: `heaps_var` is the address of `CodeCache::_heaps`, a
                // `GrowableArray<CodeHeap*>*` set up during VM initialization and never freed.
                unsafe {
                    let arr = *(heaps_var as *const usize);
                    if arr == 0 {
                        return None;
                    }
                    let n = *((arr + len) as *const i32);
                    let items = *((arr + data) as *const usize);
                    if !(0..=16).contains(&n) || items == 0 {
                        return None;
                    }
                    (0..n as usize)
                        .map(|i| *((items + i * size_of::<usize>()) as *const usize))
                        .filter(|&h| h != 0)
                        .collect()
                }
            },
            memory_low_boundary: memory + vs_low_boundary,
            memory_high: memory + vs_high,
            memory_low: memory + vs_low,
            segmap_low: segmap + vs_low,
            log2_segment_size: v.offset("CodeHeap", "_log2_segment_size")?,
            heap_block_size: v.size("HeapBlock")?,
            block_used: header + used,
            frame_complete: fc.offset as usize,
            frame_complete_width: IntWidth::of(&fc.type_name)?,
            frame_size: fs.offset as usize,
            frame_size_width: IntWidth::of(&fs.type_name)?,
        };
        (!cache.heaps.is_empty() && cache.heap_block_size > 0).then_some(cache)
    }

    /// The `CodeBlob*` containing `pc`, if any.
    pub fn find_blob(&self, pc: usize) -> Option<usize> {
        let word = |p: usize| {
            // SAFETY: `p` is a field of a live CodeHeap (offsets from VMStructs).
            unsafe { std::ptr::read_volatile(p as *const usize) }
        };
        for &heap in &self.heaps {
            if pc < word(heap + self.memory_low_boundary) || pc >= word(heap + self.memory_high) {
                continue;
            }
            let low = word(heap + self.memory_low);
            let segmap = word(heap + self.segmap_low);
            // SAFETY: as above.
            let shift = unsafe { std::ptr::read_volatile((heap + self.log2_segment_size) as *const i32) };
            if !(0..32).contains(&shift) || pc < low || segmap == 0 {
                return None;
            }
            let mut seg = (pc - low) >> shift;
            // SAFETY: the segment map has one byte per segment of the committed heap, which
            // contains `pc`. 0xFF marks free segments; other values are hops to the block start.
            unsafe {
                let mut hop = std::ptr::read_volatile((segmap + seg) as *const u8);
                if hop == 0xFF {
                    return None;
                }
                let mut guard = 0;
                while hop > 0 {
                    if seg < hop as usize || guard > 1 << 16 {
                        return None;
                    }
                    seg -= hop as usize;
                    hop = std::ptr::read_volatile((segmap + seg) as *const u8);
                    guard += 1;
                }
                let block = low + (seg << shift);
                if std::ptr::read_volatile((block + self.block_used) as *const u8) == 0 {
                    return None;
                }
                return Some(block + self.heap_block_size);
            }
        }
        None
    }

    /// `(frame_complete_offset, frame_size)` of a blob.
    ///
    /// # Safety
    /// `blob` must come from [`Self::find_blob`].
    pub unsafe fn frame_info(&self, blob: usize) -> (i32, i32) {
        // SAFETY: fields of a live CodeBlob.
        unsafe {
            (
                self.frame_complete_width.read(blob + self.frame_complete),
                self.frame_size_width.read(blob + self.frame_size),
            )
        }
    }

    /// Marks a blob's frame as complete from its first instruction.
    ///
    /// # Safety
    /// `blob` must come from [`Self::find_blob`]; the code heap is writable on Linux.
    pub unsafe fn set_frame_complete(&self, blob: usize, v: i32) {
        // SAFETY: guaranteed by the caller.
        unsafe { self.frame_complete_width.write(blob + self.frame_complete, v) }
    }
}
