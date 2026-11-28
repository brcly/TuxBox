//! Address tables against the element-by-element address queries.
use super::*;
use std::collections::HashSet;

fn desc(format: u32, width: u64, height: u32, slices: u16, mips: u16, layout: u32, flags: u32) -> Desc {
    Desc {
        dimension: 3, alignment: 0, width, height, slices, mips, format,
        samples: 1, quality: 0, layout, flags,
    }
}

fn full_chain(w: u64, h: u32) -> u16 {
    (1 + (w as u32).max(h).ilog2()) as u16
}

// R8, RG8, RGBA8, RGBA16, RGBA32, then BC1, BC4, BC3, BC7.
const COLOR: [u32; 5] = [61, 49, 28, 10, 2];
const BLOCK: [u32; 4] = [71, 80, 77, 98];
const SHAPES: [(u64, u32); 7] = [(1, 1), (7, 3), (33, 65), (64, 64), (300, 17), (257, 129), (2048, 40)];

/// Every plane calculator that 2D placed textures use, with several slices,
/// odd sizes and whole mip chains down to the packed tails.
fn planes() -> Vec<(String, Plane)> {
    let mut out = Vec::new();
    let mut add = |name: String, plane: Result<Plane, i32>| {
        if let Ok(plane) = plane {
            let mut four = plane;
            out.push((name.clone(), plane));
            if plane.mips[0].swizzle == 27 {
                four.select_version_four();
                out.push((format!("{name} v4"), four));
            }
        }
    };
    for &format in COLOR.iter().chain(&BLOCK) {
        for &(w, h) in &SHAPES {
            for mips in [1, full_chain(w, h)] {
                let name = |layout: u32| format!("format {format} {w}x{h} mips {mips} layout {layout:#x}");
                for layout in [LINEAR, TILE_4K] {
                    add(name(layout), Plane::compute(&desc(format, w, h, 3, mips, layout, 0)));
                }
                for layout in [257, 258] {
                    add(name(layout), Plane::compute_microtile(&desc(format, w, h, 2, mips, layout, 0)));
                }
                for layout in [265, 266, 273, 274] {
                    add(name(layout), Plane::compute_sparse_64k(&desc(format, w, h, 3, mips, layout, 0x300000)));
                }
                add(format!("{} 64k mips", name(0)), Plane::compute_64k_mips(&desc(format, w, h, 3, mips, 0, 1)));
                add(format!("{} 64k color", name(0)), Plane::compute_64k_color(&desc(format, w, h, 2, mips, 0, 1)));
            }
        }
    }
    out
}

fn mips(plane: &Plane) -> impl Iterator<Item = (u32, u32, u32, u32)> + '_ {
    (0..plane.array_size()).flat_map(move |slice| {
        (0..plane.mip_count()).filter_map(move |level| {
            let (w, h, _, _) = plane.mip_extent(level)?;
            Some((slice, level, w, h))
        })
    })
}

#[test]
fn every_run_is_contiguous_at_the_element_addresses() {
    let mut swizzles = HashSet::new();
    let mut checked = 0u64;
    for (name, plane) in planes() {
        for (slice, level, w, h) in mips(&plane) {
            let table = plane.mip_addresses(slice, level, w, h).unwrap_or_else(|| panic!("{name} slice {slice} level {level}"));
            swizzles.insert((plane.mips[level as usize].swizzle, plane.alignment, plane.element_bytes));
            let mut seen = vec![0u32; h as usize];
            table.try_for_each_run(|offset, y, x, bytes| {
                assert_eq!(bytes % plane.element_bytes as usize, 0);
                for k in 0..(bytes / plane.element_bytes as usize) as u64 {
                    let expected = plane.element_offset_in_slice(slice, level, u64::from(x) + k, y).unwrap();
                    assert_eq!(offset + k * u64::from(plane.element_bytes), expected,
                        "{name} slice {slice} level {level} ({}, {y})", u64::from(x) + k);
                    checked += 1;
                }
                seen[y as usize] += (bytes / plane.element_bytes as usize) as u32;
                Ok::<_, ()>(())
            }).unwrap();
            assert!(seen.iter().all(|&n| n == w), "{name} level {level}: every element once");
        }
    }
    // Linear, 256-byte S and D, 4 KiB, 4 KiB tail, 64 KiB S/D/T and its tails, R_X and its tail.
    for swizzle in [0, 1, 2, 3, 5, 9, 10, 17, 18, 27] {
        assert!(swizzles.iter().any(|s| s.0 == swizzle), "swizzle {swizzle} not covered");
    }
    assert!(checked > 2_000_000, "{checked}");
}

/// The copy each row would get from addressing one element at a time.
fn reference(plane: &Plane, slice: u32, level: u32, w: u32, h: u32, tiled: &[u8], origin: u64, pitch: usize) -> Vec<u8> {
    let element = plane.element_bytes as usize;
    let mut out = vec![0xa5u8; pitch * h as usize];
    for y in 0..h {
        for x in 0..w {
            let at = (plane.element_offset_in_slice(slice, level, x.into(), y).unwrap() - origin) as usize;
            let to = y as usize * pitch + x as usize * element;
            out[to..to + element].copy_from_slice(&tiled[at..at + element]);
        }
    }
    out
}

#[test]
fn detiled_rows_match_element_copies_bit_for_bit() {
    for (name, plane) in planes() {
        for (slice, level, w, h) in mips(&plane) {
            let range = plane.mip_range_in_slice(slice, level).unwrap();
            // A tail mip's elements stay inside the tail, which can start below its own offset.
            let origin = if plane.mips[level as usize].swizzle == 3 { u64::from(slice) * plane.slice_bytes } else { range.start };
            let len = (range.end.max(origin + 65536) - origin) as usize;
            let tiled: Vec<u8> = (0..len).map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8).collect();
            let element = plane.element_bytes as usize;
            let pitch = (w as usize * element).next_multiple_of(256);
            let table = plane.mip_addresses(slice, level, w, h).unwrap();
            let mut out = vec![0xa5u8; pitch * h as usize];
            assert!(table.detile(&tiled, origin, &mut out, pitch), "{name} level {level}");
            assert!(out == reference(&plane, slice, level, w, h, &tiled, origin, pitch), "{name} slice {slice} level {level}");
            // A buffer one byte short is refused rather than read past.
            if pitch * (h as usize) > 0 {
                let mut short = vec![0u8; pitch * (h as usize - 1) + w as usize * element - 1];
                assert!(!table.detile(&tiled, origin, &mut short, pitch), "{name} level {level}");
            }
        }
    }
}

#[test]
fn out_of_range_mips_have_no_table() {
    let plane = Plane::compute(&desc(28, 64, 64, 2, 1, TILE_4K, 0)).unwrap();
    assert!(plane.mip_addresses(2, 0, 64, 64).is_none());
    assert!(plane.mip_addresses(0, 1, 32, 32).is_none());
    assert!(plane.mip_addresses(0, 0, 65, 64).is_none());
    assert!(plane.mip_addresses(0, 0, 64, 65).is_none());
    let mut msaa = Desc { samples: 4, ..desc(28, 256, 256, 1, 1, 0, 0x100001) };
    msaa.flags = 0x100001;
    let msaa = Plane::compute_64k_color(&msaa).unwrap();
    assert!(msaa.mip_addresses(0, 0, 256, 256).is_none());
}

/// How the placed-texture upload used to detile: one address query per
/// 16-byte run, or per row in the linear layout.
fn per_run_detile(plane: &Plane, slice: u32, level: u32, w: u32, h: u32, tiled: &[u8], origin: u64, out: &mut [u8], pitch: usize) {
    let element = plane.element_bytes as usize;
    let run = if plane.mips[level as usize].swizzle == 0 { w as usize } else { 16 / element };
    for y in 0..h {
        for x in (0..w).step_by(run) {
            let count = (w - x).min(run as u32) as usize * element;
            let at = plane.element_offset_in_slice(slice, level, x.into(), y).and_then(|a| a.checked_sub(origin)).unwrap() as usize;
            let to = y as usize * pitch + x as usize * element;
            out.get_mut(to..to + count).unwrap().copy_from_slice(tiled.get(at..at + count).unwrap());
        }
    }
}

/// Detile cost per MiB of guest texture before and after the address tables,
/// for the layouts titles place. Run with `--ignored --nocapture --release`.
#[test]
#[ignore]
fn detile_cost() {
    let cases = [
        ("BC1 0x105", Plane::compute(&desc(71, 2048, 2048, 1, 12, TILE_4K, 0))),
        ("BC7 0x10a", Plane::compute_sparse_64k(&desc(98, 2048, 2048, 1, 12, 266, 0x300000))),
        ("RGBA8 0x105", Plane::compute(&desc(28, 2048, 2048, 1, 12, TILE_4K, 0))),
        ("RGBA16 0x105", Plane::compute(&desc(10, 2048, 2048, 1, 12, TILE_4K, 0))),
        ("RGBA8 0x10a", Plane::compute_sparse_64k(&desc(28, 2048, 2048, 1, 12, 266, 0x300000))),
        ("RGBA8 0x100", Plane::compute(&desc(28, 2048, 2048, 1, 1, LINEAR, 0))),
    ];
    for (name, plane) in cases {
        let plane = plane.unwrap();
        let tiled: Vec<u8> = (0..plane.size as usize).map(|i| (i * 31) as u8).collect();
        let mips: Vec<_> = mips(&plane).map(|(s, l, w, h)| {
            let range = plane.mip_range_in_slice(s, l).unwrap();
            let origin = if plane.mips[l as usize].swizzle == 3 { 0 } else { range.start };
            let pitch = (w as usize * plane.element_bytes as usize).next_multiple_of(256);
            (s, l, w, h, origin, pitch, vec![0u8; pitch * h as usize])
        }).collect();
        let mib = plane.size as f64 / 1048576.0;
        let mut mips = mips;
        let time = |f: &mut dyn FnMut()| {
            f();
            let start = std::time::Instant::now();
            for _ in 0..5 {
                f();
            }
            start.elapsed().as_secs_f64() * 1e3 / 5.0 / mib
        };
        let before = time(&mut || for (s, l, w, h, origin, pitch, out) in mips.iter_mut() {
            per_run_detile(&plane, *s, *l, *w, *h, &tiled[*origin as usize..], *origin, out, *pitch);
        });
        let mut tables = Vec::new();
        let build = time(&mut || tables = mips.iter().map(|m| plane.mip_addresses(m.0, m.1, m.2, m.3).unwrap()).collect());
        let after = time(&mut || for ((_, _, _, _, origin, pitch, out), table) in mips.iter_mut().zip(&tables) {
            assert!(table.detile(&tiled[*origin as usize..], *origin, out, *pitch));
        });
        println!("{name} {mib:.1} MiB: per run {before:.3} ms/MiB, table {after:.3} ms/MiB ({:.1}x), building the tables {build:.3} ms/MiB once",
            before / after);
    }
}
