//! Where each guest slot's native view lives on the CPU.
//!
//! Every slot of the guest heap needs a CPU-only home, but a host can refuse
//! one CPU heap that large and no query says how large it may be. Ask for the
//! whole heap; while the host refuses for memory, ask for half, then cover the
//! guest heap with heaps of the size it gave. A slot is found by its chunk and
//! its place within it, and a copy is split where chunks end.
use windows::Win32::Foundation::E_OUTOFMEMORY;
use windows::core::Result;

/// The smallest chunk worth asking for. A host that refuses this much is out
/// of memory, not short of a heap size.
pub(super) const MIN_CHUNK: u32 = 1 << 16;

/// Heaps that hold `count` slots, each made by `create` with its slot count.
/// Only a refusal for memory makes the chunks smaller, and only when `split`
/// allows more than one heap. Returns the chunk size with the heaps.
pub(super) fn create_chunks<H>(
    count: u32,
    split: bool,
    mut create: impl FnMut(u32) -> Result<H>,
) -> Result<(u32, Vec<H>)> {
    let mut chunk = count.max(1);
    let first = loop {
        match create(chunk) {
            Ok(heap) => break heap,
            Err(error) if split && error.code() == E_OUTOFMEMORY && chunk > MIN_CHUNK => {
                chunk = (chunk / 2).max(MIN_CHUNK);
            }
            Err(error) => return Err(error),
        }
    };
    let mut heaps = Vec::with_capacity(count.div_ceil(chunk) as usize);
    heaps.push(first);
    let mut covered = chunk;
    while covered < count {
        let size = chunk.min(count - covered);
        heaps.push(create(size)?);
        covered += size;
    }
    Ok((chunk, heaps))
}

/// The CPU start of each chunk, all `chunk` slots long but the last.
pub(super) struct Chunks {
    pub(super) starts: Vec<usize>,
    pub(super) chunk: u32,
}

impl Chunks {
    /// The CPU handle of `slot`.
    pub(super) fn handle(&self, slot: u32, stride: usize) -> usize {
        self.starts[(slot / self.chunk) as usize] + (slot % self.chunk) as usize * stride
    }

    /// `count` slots from `first`, as `(slot, length)` runs that stay inside one chunk.
    pub(super) fn runs(&self, first: u32, count: u32) -> impl Iterator<Item = (u32, u32)> + '_ {
        let end = first.saturating_add(count);
        let mut at = first;
        core::iter::from_fn(move || {
            (at < end).then(|| {
                let length = (self.chunk - at % self.chunk).min(end - at);
                let run = (at, length);
                at += length;
                run
            })
        })
    }
}
