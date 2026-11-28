//! Address tables that move a whole 2D mip with one table read per contiguous run.
use super::*;

#[cfg(test)]
#[path = "tests/addresses.rs"]
mod tests;

/// Byte offsets of one mip's elements, split into a term for the column and
/// one for the row. Every 2D swizzle here adds whole tiles and combines the
/// bits inside a tile by OR or XOR, so an element's offset is
/// `base + (hi(column) + hi(row)) + (lo(column) ^ lo(row))`, where `lo` keeps
/// the bits below the tile size. The terms are read from
/// [`Plane::element_offset_for_hardware`], so the table follows its bit tables.
pub struct MipAddresses {
    base: u64,
    low: u64,
    element: u32,
    run: u32,
    width: u32,
    columns: Box<[u64]>,
    rows: Box<[u64]>,
}

impl Plane {
    /// The table for the first `width` x `rows` elements of a 2D mip, or
    /// `None` when the mip is multisampled or out of range.
    pub fn mip_addresses(&self, slice: u32, level: u32, width: u32, rows: u32) -> Option<MipAddresses> {
        if slice >= self.array_size() || level >= self.mip_count() || self.color_samples() > 1 || width == 0 || rows == 0 {
            return None;
        }
        let mip = &self.mips[level as usize];
        let slice_base = u64::from(slice) * self.slice_bytes;
        // The tile size of each branch of element_offset_for_hardware, in the
        // same order. Swizzle 3 XORs its tail origin, so it starts at the slice.
        let (base, low) = match mip.swizzle {
            0 => (slice_base + mip.offset, 0),
            1 | 2 if self.alignment == 256 => (slice_base + mip.offset, 0xff),
            9 | 10 | 17 | 18 | 27 => (slice_base + mip.offset, 0xffff),
            2 if self.alignment == 65536 => (slice_base + mip.offset, 0xffff),
            3 => (slice_base, 0xffff),
            _ => (slice_base + mip.offset, 0xfff),
        };
        let offset = |x: u32, y: u32| {
            self.element_offset_in_slice(slice, level, u64::from(x), y)?.checked_sub(base)
        };
        let corner = offset(0, 0)?;
        if corner & !low != 0 {
            return None;
        }
        let x: Vec<u64> = (0..width).map(|x| offset(x, 0)).collect::<Option<_>>()?;
        // The corner's bits inside the tile (a slice XOR) are in every column
        // term already, so take them out of the row terms.
        let row_terms: Vec<u64> = (0..rows).map(|y| Some(offset(0, y)? ^ corner)).collect::<Option<_>>()?;
        let element = self.element_bytes;
        let runs = |run: u32| {
            let step = u64::from(element) * u64::from(run);
            if low != 0 && step - 1 > low {
                return false;
            }
            let inside = if low == 0 { 0 } else { step - 1 };
            row_terms.iter().all(|y| y & inside == 0)
                && x.chunks(run as usize).all(|chunk| {
                    chunk[0] & inside == 0
                        && chunk.iter().enumerate().all(|(k, &v)| v == chunk[0] + k as u64 * u64::from(element))
                })
        };
        let mut run = 1;
        while run < width && runs(run * 2) {
            run *= 2;
        }
        Some(MipAddresses {
            base,
            low,
            element,
            run,
            width,
            columns: x.iter().step_by(run as usize).copied().collect(),
            rows: row_terms.into_boxed_slice(),
        })
    }
}

impl MipAddresses {
    /// Elements per contiguous run in both layouts.
    pub fn run(&self) -> u32 {
        self.run
    }

    #[inline(always)]
    fn offset(&self, column: u64, row: u64) -> u64 {
        let low = self.low;
        self.base + ((column & !low) + (row & !low)) + ((column ^ row) & low)
    }

    /// Visits each run as (plane offset, row, first element, bytes), in row order.
    pub fn try_for_each_run<E>(&self, mut visit: impl FnMut(u64, u32, u32, usize) -> Result<(), E>) -> Result<(), E> {
        let element = self.element as usize;
        for (y, &row) in self.rows.iter().enumerate() {
            for (q, &column) in self.columns.iter().enumerate() {
                let x = q as u32 * self.run;
                let count = (self.width - x).min(self.run) as usize * element;
                visit(self.offset(column, row), y as u32, x, count)?;
            }
        }
        Ok(())
    }

    /// Copies the mip from `tiled`, which holds the plane from offset `origin`,
    /// into rows of `pitch` bytes. False when a run lies outside either buffer.
    pub fn detile(&self, tiled: &[u8], origin: u64, linear: &mut [u8], pitch: usize) -> bool {
        match self.run as usize * self.element as usize {
            8 => self.detile_runs::<8>(tiled, origin, linear, pitch),
            16 => self.detile_runs::<16>(tiled, origin, linear, pitch),
            32 => self.detile_runs::<32>(tiled, origin, linear, pitch),
            _ => self.detile_runs::<0>(tiled, origin, linear, pitch),
        }
    }

    /// `N` is the run's byte count when it is known, so a whole run is one
    /// fixed-size copy; 0 copies rows of any length.
    fn detile_runs<const N: usize>(&self, tiled: &[u8], origin: u64, linear: &mut [u8], pitch: usize) -> bool {
        let element = self.element as usize;
        let row_bytes = self.width as usize * element;
        let run_bytes = self.run as usize * element;
        let whole = if N == 0 { 0 } else { row_bytes / N };
        for (y, &row) in self.rows.iter().enumerate() {
            let Some(output) = y.checked_mul(pitch).and_then(|start| linear.get_mut(start..start.checked_add(row_bytes)?)) else {
                return false;
            };
            let mut q = 0;
            if N != 0 {
                for (out, &column) in output.chunks_exact_mut(N).zip(&self.columns[..whole]) {
                    let at = self.offset(column, row).wrapping_sub(origin) as usize;
                    let Some(input) = tiled.get(at..at.wrapping_add(N)) else {
                        return false;
                    };
                    out.copy_from_slice(input);
                }
                q = whole;
            }
            for &column in &self.columns[q..] {
                let start = q * run_bytes;
                let count = (row_bytes - start).min(run_bytes);
                let at = self.offset(column, row).wrapping_sub(origin) as usize;
                let Some(input) = tiled.get(at..at.wrapping_add(count)) else {
                    return false;
                };
                output[start..start + count].copy_from_slice(input);
                q += 1;
            }
        }
        true
    }
}
