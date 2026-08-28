//! Pure helpers to turn length-prefixed (AVCC / HVCC) access units into Annex-B.

/// Annex-B start code emitted before every NAL unit.
pub const START_CODE: [u8; 4] = [0, 0, 0, 1];

/// Append `nal` to `out` with a 4-byte start code.
pub fn push_nal(out: &mut Vec<u8>, nal: &[u8]) {
    out.extend_from_slice(&START_CODE);
    out.extend_from_slice(nal);
}

/// Rewrite an AVCC/HVCC sample (`nal_len_size`-byte big-endian length prefixes) into
/// Annex-B, appending to `out`. Returns the number of NAL units written.
///
/// Truncated trailing data (a length prefix that exceeds the buffer) is ignored so that a
/// corrupt sample cannot cause a panic.
pub fn lengths_to_annexb(data: &[u8], nal_len_size: usize, out: &mut Vec<u8>) -> usize {
    assert!(
        (1..=4).contains(&nal_len_size),
        "nal_len_size must be 1..=4"
    );
    let mut pos = 0;
    let mut count = 0;
    while pos + nal_len_size <= data.len() {
        let mut len = 0usize;
        for &b in &data[pos..pos + nal_len_size] {
            len = (len << 8) | b as usize;
        }
        pos += nal_len_size;
        if len == 0 || pos + len > data.len() {
            break;
        }
        push_nal(out, &data[pos..pos + len]);
        pos += len;
        count += 1;
    }
    count
}

/// NAL unit type of an H.264 NAL (`nal_unit_type` in the first byte).
pub fn h264_nal_type(nal: &[u8]) -> Option<u8> {
    nal.first().map(|b| b & 0x1f)
}

/// NAL unit type of an HEVC NAL (six bits of the first byte).
pub fn hevc_nal_type(nal: &[u8]) -> Option<u8> {
    nal.first().map(|b| (b >> 1) & 0x3f)
}

/// Iterate over the NAL units of an Annex-B stream (3- or 4-byte start codes), yielding
/// the payload slices without start codes.
pub fn annexb_nals(data: &[u8]) -> Vec<&[u8]> {
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 3 <= data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            starts.push(i + 3);
            i += 3;
        } else {
            i += 1;
        }
    }
    let mut nals = Vec::with_capacity(starts.len());
    for (n, &start) in starts.iter().enumerate() {
        let mut end = if n + 1 < starts.len() {
            starts[n + 1] - 3
        } else {
            data.len()
        };
        // A 4-byte start code has a leading zero that belongs to the previous NAL's end.
        while end > start && data[end - 1] == 0 && n + 1 < starts.len() {
            end -= 1;
        }
        nals.push(&data[start..end]);
    }
    nals
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rewrites_four_byte_lengths() {
        let sample = [0, 0, 0, 2, 0x65, 0xAA, 0, 0, 0, 1, 0x41];
        let mut out = Vec::new();
        let n = lengths_to_annexb(&sample, 4, &mut out);
        assert_eq!(n, 2);
        assert_eq!(out, vec![0, 0, 0, 1, 0x65, 0xAA, 0, 0, 0, 1, 0x41]);
    }

    #[test]
    fn rewrites_two_byte_lengths() {
        let sample = [0, 3, 1, 2, 3, 0, 1, 9];
        let mut out = Vec::new();
        assert_eq!(lengths_to_annexb(&sample, 2, &mut out), 2);
        assert_eq!(out, vec![0, 0, 0, 1, 1, 2, 3, 0, 0, 0, 1, 9]);
    }

    #[test]
    fn ignores_truncated_tail() {
        let sample = [0, 0, 0, 2, 0x65, 0xAA, 0, 0, 0, 9, 0x41];
        let mut out = Vec::new();
        assert_eq!(lengths_to_annexb(&sample, 4, &mut out), 1);
        assert_eq!(out, vec![0, 0, 0, 1, 0x65, 0xAA]);
    }

    #[test]
    fn parameter_sets_then_slices() {
        let mut out = Vec::new();
        push_nal(&mut out, &[0x67, 1, 2]); // SPS
        push_nal(&mut out, &[0x68, 3]); // PPS
        lengths_to_annexb(&[0, 0, 0, 1, 0x65], 4, &mut out);
        let nals = annexb_nals(&out);
        let types: Vec<u8> = nals.iter().map(|n| h264_nal_type(n).unwrap()).collect();
        assert_eq!(types, vec![7, 8, 5]);
        assert_eq!(nals[0], &[0x67, 1, 2]);
    }

    #[test]
    fn hevc_types() {
        // VPS(32) SPS(33) PPS(34) IDR_W_RADL(19)
        let mut out = Vec::new();
        push_nal(&mut out, &[32 << 1, 1]);
        push_nal(&mut out, &[33 << 1, 1]);
        push_nal(&mut out, &[34 << 1, 1]);
        push_nal(&mut out, &[19 << 1, 1]);
        let types: Vec<u8> = annexb_nals(&out)
            .iter()
            .map(|n| hevc_nal_type(n).unwrap())
            .collect();
        assert_eq!(types, vec![32, 33, 34, 19]);
    }

    #[test]
    fn annexb_parser_handles_three_byte_start_codes_and_trailing_zero() {
        let data = [0, 0, 1, 0x41, 0xAB, 0, 0, 0, 1, 0x01, 0x00];
        let nals = annexb_nals(&data);
        assert_eq!(nals.len(), 2);
        assert_eq!(nals[0], &[0x41, 0xAB]);
        assert_eq!(nals[1], &[0x01, 0x00]);
    }
}
