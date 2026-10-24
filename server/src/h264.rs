//! Minimal H.264 SPS reader. ffprobe no longer reports the number of reference
//! frames, yet that is what makes some encodes (x264 with ref=12, bframes=12)
//! stutter on TV hardware decoders, so we read it from the codec extradata.

/// Decodes ffprobe's `-show_data` hexdump ("00000000: 0164 0029 ...  .d.)") into bytes.
pub fn parse_hexdump(dump: &str) -> Vec<u8> {
    let mut out = Vec::new();
    for line in dump.lines() {
        let Some((_, rest)) = line.split_once(": ") else { continue };
        // Hex groups are followed by two spaces and the ASCII column.
        let hex = rest.split("  ").next().unwrap_or("");
        let digits: Vec<u8> = hex.bytes().filter(u8::is_ascii_hexdigit).collect();
        for pair in digits.chunks_exact(2) {
            if let Ok(b) = u8::from_str_radix(std::str::from_utf8(pair).unwrap_or("00"), 16) {
                out.push(b);
            }
        }
    }
    out
}

/// max_num_ref_frames from avcC (MP4/MKV) or Annex B (TS) extradata.
pub fn max_ref_frames(extradata: &[u8]) -> Option<u32> {
    parse_sps(&unescape(find_sps(extradata)?))
}

fn find_sps(ext: &[u8]) -> Option<&[u8]> {
    if ext.first() == Some(&1) {
        // avcC: version, profile, compat, level, nal-length size, then SPS count and sizes.
        if ext.len() < 8 || ext[5] & 0x1f == 0 {
            return None;
        }
        let len = u16::from_be_bytes([ext[6], ext[7]]) as usize;
        return ext.get(8..8 + len);
    }
    // Annex B: 00 00 01 start codes; SPS has NAL type 7.
    let mut i = 0;
    while i + 3 < ext.len() {
        if ext[i] == 0 && ext[i + 1] == 0 && ext[i + 2] == 1 {
            let start = i + 3;
            if ext[start] & 0x1f == 7 {
                let end = (start..ext.len().saturating_sub(2))
                    .find(|&j| ext[j] == 0 && ext[j + 1] == 0 && (ext[j + 2] == 1 || ext[j + 2] == 0))
                    .unwrap_or(ext.len());
                return Some(&ext[start..end]);
            }
            i = start;
        } else {
            i += 1;
        }
    }
    None
}

/// Removes emulation-prevention bytes (00 00 03 -> 00 00).
fn unescape(nal: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(nal.len());
    let mut zeros = 0;
    for &b in nal {
        if zeros >= 2 && b == 3 {
            zeros = 0;
            continue;
        }
        zeros = if b == 0 { zeros + 1 } else { 0 };
        out.push(b);
    }
    out
}

struct Bits<'a> {
    data: &'a [u8],
    pos: usize,
}

impl Bits<'_> {
    fn bit(&mut self) -> Option<u32> {
        let byte = *self.data.get(self.pos / 8)?;
        let bit = (byte >> (7 - self.pos % 8)) & 1;
        self.pos += 1;
        Some(bit as u32)
    }

    fn ue(&mut self) -> Option<u32> {
        let mut zeros = 0;
        while self.bit()? == 0 {
            zeros += 1;
            if zeros > 31 {
                return None;
            }
        }
        let mut v: u64 = 1;
        for _ in 0..zeros {
            v = (v << 1) | self.bit()? as u64;
        }
        Some((v - 1) as u32)
    }

    fn se(&mut self) -> Option<i32> {
        let k = self.ue()? as i64;
        Some(if k % 2 == 1 { (k + 1) / 2 } else { -(k / 2) } as i32)
    }
}

fn parse_sps(nal: &[u8]) -> Option<u32> {
    if nal.len() < 4 || nal[0] & 0x1f != 7 {
        return None;
    }
    let profile = nal[1];
    let mut r = Bits { data: &nal[4..], pos: 0 };
    r.ue()?; // seq_parameter_set_id
    if matches!(profile, 100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135) {
        let chroma_format = r.ue()?;
        if chroma_format == 3 {
            r.bit()?; // separate_colour_plane_flag
        }
        r.ue()?; // bit_depth_luma_minus8
        r.ue()?; // bit_depth_chroma_minus8
        r.bit()?; // qpprime_y_zero_transform_bypass_flag
        if r.bit()? == 1 {
            let lists = if chroma_format == 3 { 12 } else { 8 };
            for i in 0..lists {
                if r.bit()? == 1 {
                    let size = if i < 6 { 16 } else { 64 };
                    let (mut last, mut next) = (8i32, 8i32);
                    for _ in 0..size {
                        if next != 0 {
                            next = (last + r.se()? + 256) % 256;
                        }
                        if next != 0 {
                            last = next;
                        }
                    }
                }
            }
        }
    }
    r.ue()?; // log2_max_frame_num_minus4
    match r.ue()? {
        0 => {
            r.ue()?; // log2_max_pic_order_cnt_lsb_minus4
        }
        1 => {
            r.bit()?;
            r.se()?;
            r.se()?;
            for _ in 0..r.ue()? {
                r.se()?;
            }
        }
        _ => {}
    }
    r.ue()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// avcC of House.of.the.Dragon.S03E01.WEB-DLRip.x264.seleZen.mkv (x264 ref=12), as printed by ffprobe.
    const DRAGON: &str = "\n00000000: 0164 0029 ffe1 001c 6764 0029 ac52 0d01  .d.)....gd.).R..\n00000010: 0010 6c05 a808 080a 0000 07d2 0001 7700  ..l...........w.\n00000020: 1e30 631b 0100 0668 e8ce 1b3c 8ffd f8f8  .0c....h...<....\n00000030: 00                                       .\n";

    #[test]
    fn reads_refs_from_avcc() {
        let ext = parse_hexdump(DRAGON);
        assert_eq!(ext.len(), 49);
        assert_eq!(max_ref_frames(&ext), Some(12));
    }

    #[test]
    fn reads_refs_from_annex_b() {
        let ext = parse_hexdump(DRAGON);
        let sps = &ext[8..8 + 0x1c];
        let mut annex_b = vec![0, 0, 0, 1];
        annex_b.extend_from_slice(sps);
        assert_eq!(max_ref_frames(&annex_b), Some(12));
    }

    #[test]
    fn rejects_garbage() {
        assert_eq!(max_ref_frames(&[]), None);
        assert_eq!(max_ref_frames(&[1, 2, 3]), None);
    }
}
