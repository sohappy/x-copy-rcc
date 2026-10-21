// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Retro Computing Club (RCC)

//! AmigaDOS analysis of an ADF image: block allocation map (for BAMCOPY), volume name and
//! boot block classification.

/// Bytes per AmigaDOS block.
const BSIZE: usize = 512;

pub fn total_blocks(hd: bool) -> usize {
    if hd {
        3520
    } else {
        1760
    }
}

pub fn sectors_per_track(hd: bool) -> usize {
    if hd {
        22
    } else {
        11
    }
}

pub fn image_size(hd: bool) -> usize {
    total_blocks(hd) * BSIZE
}

fn be32(b: &[u8], off: usize) -> u32 {
    u32::from_be_bytes([b[off], b[off + 1], b[off + 2], b[off + 3]])
}

fn block(data: &[u8], n: usize) -> Option<&[u8]> {
    data.get(n * BSIZE..(n + 1) * BSIZE)
}

/// Standard header checksum: the sum of all longs of the block is zero.
fn header_checksum_ok(b: &[u8]) -> bool {
    b.chunks_exact(4).map(|c| u32::from_be_bytes([c[0], c[1], c[2], c[3]])).fold(0u32, u32::wrapping_add) == 0
}

/// Cylinder holding a block (two heads per cylinder).
pub fn cylinder_of(blk: usize, hd: bool) -> u16 {
    (blk / sectors_per_track(hd) / 2) as u16
}

fn root_index(hd: bool) -> usize {
    total_blocks(hd) / 2
}

/// The root block, if it looks valid.
fn root(data: &[u8], hd: bool) -> Result<&[u8], String> {
    let r = block(data, root_index(hd)).ok_or("Image too short: no root block.")?;
    if be32(r, 0) != 2 || be32(r, 508) != 1 {
        return Err("No AmigaDOS root block (NDOS or damaged disk).".into());
    }
    if !header_checksum_ok(r) {
        return Err("Root block checksum is wrong.".into());
    }
    Ok(r)
}

/// Volume name stored in the root block.
pub fn volume_name(data: &[u8], hd: bool) -> Option<String> {
    let r = root(data, hd).ok()?;
    let len = (r[432] as usize).min(30);
    let name: String = r[433..433 + len].iter().map(|&b| b as char).collect();
    (!name.trim().is_empty()).then_some(name)
}

/// File name made from a volume name (characters forbidden on Windows/macOS replaced).
pub fn file_stem(name: &str) -> String {
    let s: String =
        name.chars().map(|c| if "/\\:*?\"<>|".contains(c) || (c as u32) < 32 { '_' } else { c }).collect();
    let s = s.trim().trim_matches('.').to_string();
    if s.is_empty() {
        "disk".into()
    } else {
        s
    }
}

/// Cylinders that hold allocated blocks, from the root block and its bitmap. The bitmap blocks
/// must be on cylinder 40 (where formatting puts them), the only cylinder read beforehand.
pub fn used_cylinders(data: &[u8], hd: bool) -> Result<Vec<u16>, String> {
    let r = root(data, hd)?;
    if be32(r, 312) != 0xffff_ffff {
        return Err("The disk bitmap is marked invalid (disk not validated).".into());
    }
    let blocks = total_blocks(hd);
    let root_cyl = cylinder_of(root_index(hd), hd);
    let mut used = vec![false; blocks];
    used[0] = true;
    used[1] = true;
    used[root_index(hd)] = true;
    // bm_pages: 25 pointers from offset 316.
    let mut bitmap_blocks = Vec::new();
    for i in 0..25 {
        let p = be32(r, 316 + i * 4) as usize;
        if p != 0 {
            bitmap_blocks.push(p);
        }
    }
    if bitmap_blocks.is_empty() {
        return Err("No bitmap block in the root block.".into());
    }
    // One bitmap block maps (512 - 4) * 8 = 4064 blocks.
    for (page, &bb) in bitmap_blocks.iter().enumerate() {
        if bb >= blocks {
            return Err(format!("Bitmap block {bb} is outside the disk."));
        }
        if cylinder_of(bb, hd) != root_cyl {
            return Err(format!("Bitmap block {bb} is not on cylinder {root_cyl}."));
        }
        used[bb] = true;
        let b = block(data, bb).ok_or("Image too short: bitmap missing.")?;
        if !header_checksum_ok(b) {
            return Err("Bitmap block checksum is wrong.".into());
        }
        for long in 0..127 {
            let bits = be32(b, 4 + long * 4);
            for bit in 0..32 {
                let blk = 2 + page * 4064 + long * 32 + bit;
                if blk < blocks && bits & (1 << bit) == 0 {
                    used[blk] = true;
                }
            }
        }
    }
    let mut cyls: Vec<u16> = (0..blocks).filter(|&b| used[b]).map(|b| cylinder_of(b, hd)).collect();
    cyls.dedup();
    Ok(cyls)
}

/// Greaseweazle set notation for a sorted cylinder list: `0,3,5-9,40`.
pub fn set_notation(cyls: &[u16]) -> String {
    let mut parts = Vec::new();
    let mut i = 0;
    while i < cyls.len() {
        let start = cyls[i];
        let mut end = start;
        while i + 1 < cyls.len() && cyls[i + 1] == end + 1 {
            i += 1;
            end = cyls[i];
        }
        parts.push(if start == end { start.to_string() } else { format!("{start}-{end}") });
        i += 1;
    }
    parts.join(",")
}

/// Pad (or create) an ADF to its full size, so that a partial read can be written or opened.
pub fn pad_image(path: &std::path::Path, hd: bool) -> Result<(), String> {
    let mut data = std::fs::read(path).unwrap_or_default();
    let full = image_size(hd);
    if data.len() < full {
        data.resize(full, 0);
        std::fs::write(path, &data).map_err(|e| format!("Cannot complete the image: {e}"))?;
    }
    Ok(())
}

/// What the boot block (blocks 0-1) contains.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Boot {
    /// No "DOS" signature (NDOS: game or demo with its own format).
    NotDos,
    /// DOS disk without boot code (Workbench "Format", not bootable).
    Blank,
    /// Short boot code calling dos.library: Workbench "Install".
    Standard,
    /// Valid checksum and unknown code: game loader, demo intro, other utility... or a virus.
    Custom,
    /// Unknown code containing a tell-tale text.
    Suspect(String),
}

impl Boot {
    pub fn describe(&self) -> String {
        match self {
            Boot::NotDos => "NO DOS BOOT BLOCK (NDOS DISK)".into(),
            Boot::Blank => "BLANK, NOT BOOTABLE".into(),
            Boot::Standard => "STANDARD (WORKBENCH INSTALL)".into(),
            Boot::Custom => "CUSTOM CODE (LOADER OR VIRUS?)".into(),
            Boot::Suspect(t) => format!("SUSPECT: CONTAINS \"{t}\""),
        }
    }

    /// Worth asking the user about.
    pub fn is_unusual(&self) -> bool {
        matches!(self, Boot::Custom | Boot::Suspect(_))
    }
}

fn boot_checksum_ok(b: &[u8]) -> bool {
    let mut sum: u32 = 0;
    for c in b.chunks_exact(4) {
        let v = u32::from_be_bytes([c[0], c[1], c[2], c[3]]);
        let (s, carry) = sum.overflowing_add(v);
        sum = s + carry as u32;
    }
    sum == 0xffff_ffff
}

/// Texts found in well-known boot block viruses (and in their own messages).
const MARKERS: &[&str] = &["virus", "lamer", "bandit", "sca!", "clonk", "saddam", "obelisk", "mega-master"];

pub fn classify_boot(data: &[u8]) -> Boot {
    let Some(b) = data.get(..1024) else { return Boot::NotDos };
    if &b[..3] != b"DOS" {
        return Boot::NotDos;
    }
    let code = &b[12..];
    let last = code.iter().rposition(|&x| x != 0);
    let Some(last) = last else { return Boot::Blank };
    if !boot_checksum_ok(b) {
        // Kickstart only runs boot code with a valid checksum.
        return Boot::Blank;
    }
    let lower: Vec<u8> = code.iter().map(|c| c.to_ascii_lowercase()).collect();
    for m in MARKERS {
        if lower.windows(m.len()).any(|w| w == m.as_bytes()) {
            return Boot::Suspect(m.to_uppercase());
        }
    }
    let has_dos = lower.windows(11).any(|w| w == b"dos.library");
    if has_dos && last < 200 {
        return Boot::Standard;
    }
    Boot::Custom
}

/// Replace the boot block by a blank one (keeps the DOS type, so the files stay readable).
pub fn blank_boot(data: &mut [u8], hd: bool) {
    if data.len() < 1024 {
        return;
    }
    let dos_type = data[3];
    data[..1024].fill(0);
    data[..3].copy_from_slice(b"DOS");
    data[3] = dos_type;
    data[8..12].copy_from_slice(&(root_index(hd) as u32).to_be_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;
    use hgf_core::amigados::{blank_image, FormatOpts};

    fn blank(name: &str) -> Vec<u8> {
        blank_image(&FormatOpts { name: name.into(), ..Default::default() }).unwrap()
    }

    fn put32(b: &mut [u8], off: usize, v: u32) {
        b[off..off + 4].copy_from_slice(&v.to_be_bytes());
    }

    fn fix_checksum(b: &mut [u8], off: usize) {
        put32(b, off, 0);
        let sum = b
            .chunks_exact(4)
            .map(|c| u32::from_be_bytes([c[0], c[1], c[2], c[3]]))
            .fold(0u32, u32::wrapping_add);
        put32(b, off, 0u32.wrapping_sub(sum));
    }

    #[test]
    fn name_of_blank_disk() {
        assert_eq!(volume_name(&blank("Games 1"), false).as_deref(), Some("Games 1"));
        assert_eq!(volume_name(&vec![0u8; 901_120], false), None);
        assert_eq!(file_stem("A/B:C?"), "A_B_C_");
        assert_eq!(file_stem("  "), "disk");
    }

    #[test]
    fn blank_disk_uses_cylinders_0_and_40() {
        assert_eq!(used_cylinders(&blank("x"), false).unwrap(), vec![0, 40]);
    }

    #[test]
    fn allocated_blocks_add_their_cylinders() {
        let mut img = blank("x");
        let bm = 881;
        // allocate blocks 100 (cyl 4) and 1500 (cyl 68)
        for blk in [100usize, 1500] {
            let i = blk - 2;
            let off = bm * 512 + 4 + (i / 32) * 4;
            let v =
                u32::from_be_bytes([img[off], img[off + 1], img[off + 2], img[off + 3]]) & !(1 << (i % 32));
            put32(&mut img, off, v);
        }
        fix_checksum(&mut img[bm * 512..bm * 512 + 512], 0);
        assert_eq!(used_cylinders(&img, false).unwrap(), vec![0, 4, 40, 68]);
        assert_eq!(set_notation(&[0, 4, 5, 6, 40, 68]), "0,4-6,40,68");
    }

    #[test]
    fn invalid_bitmap_is_refused() {
        let mut img = blank("x");
        let r = 880 * 512;
        put32(&mut img, r + 312, 0);
        fix_checksum(&mut img[r..r + 512], 20);
        assert!(used_cylinders(&img, false).is_err());
    }

    fn with_code(code: &[u8]) -> Vec<u8> {
        let mut img = blank("x");
        img[12..12 + code.len()].copy_from_slice(code);
        // boot checksum (with carry) at offset 4
        put32(&mut img, 4, 0);
        let mut sum: u32 = 0;
        for c in img[..1024].chunks_exact(4) {
            let v = u32::from_be_bytes([c[0], c[1], c[2], c[3]]);
            let (s, carry) = sum.overflowing_add(v);
            sum = s + carry as u32;
        }
        put32(&mut img, 4, !sum);
        img
    }

    #[test]
    fn boot_blocks() {
        assert_eq!(classify_boot(&blank("x")), Boot::Blank);
        assert_eq!(classify_boot(&vec![0u8; 2048]), Boot::NotDos);
        let std = with_code(b"\x43\xfa\x00\x18\x4e\xae\xff\xa0dos.library\0");
        assert_eq!(classify_boot(&std), Boot::Standard);
        let custom = with_code(&[0x4e, 0x71, 0x60, 0xfe, 1, 2, 3]);
        assert_eq!(classify_boot(&custom), Boot::Custom);
        let virus = with_code(b"\x4e\x71Something wonderful has happened: your Amiga is alive... VIRUS");
        assert_eq!(classify_boot(&virus), Boot::Suspect("VIRUS".into()));
        let mut v = virus.clone();
        blank_boot(&mut v, false);
        assert_eq!(classify_boot(&v), Boot::Blank);
        assert_eq!(volume_name(&v, false).as_deref(), Some("x"));
    }
}
