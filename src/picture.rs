//! Picture parsing (§12) and decoding (§15) — the per-picture pipeline that
//! turns a picture data unit into clipped, offset component planes.

use crate::bitio::BitReader;
use crate::params::SequenceHeader;
use crate::transform::{self, PictureKind, TransformData, TransformParameters};
use crate::wavelet::Plane;
use crate::Result;

/// A fully decoded picture: three planes of unsigned sample values plus the
/// picture number from the picture header (§12.2).
#[derive(Debug, Clone)]
pub struct DecodedPicture {
    pub picture_number: u32,
    pub luma_width: usize,
    pub luma_height: usize,
    pub color_diff_width: usize,
    pub color_diff_height: usize,
    /// Luma plane (Y), row-major, unsigned.
    pub y: Vec<u16>,
    /// First colour-difference plane (C1), row-major, unsigned.
    pub c1: Vec<u16>,
    /// Second colour-difference plane (C2), row-major, unsigned.
    pub c2: Vec<u16>,
}

/// Parsed picture: header + transform parameters + unpacked coefficients,
/// ready for `picture_decode`.
pub struct ParsedPicture {
    pub picture_number: u32,
    pub kind: PictureKind,
    pub transform_parameters: TransformParameters,
    pub transform_data: TransformData,
}

/// `picture_parse()` (§12.1): picture header (§12.2) then the wavelet
/// transform metadata and coefficient data (§12.3 / §13).
pub fn picture_parse(
    r: &mut BitReader,
    seq: &SequenceHeader,
    kind: PictureKind,
) -> Result<ParsedPicture> {
    r.byte_align();
    let picture_number = r.read_uint_lit(4) as u32; // picture_header (§12.2)
    r.byte_align();
    // wavelet_transform (§12.3).
    let transform_parameters = transform::transform_parameters(r, seq, kind)?;
    r.byte_align();
    let transform_data = transform::transform_data(r, seq, &transform_parameters, kind)?;
    Ok(ParsedPicture {
        picture_number,
        kind,
        transform_parameters,
        transform_data,
    })
}

/// `picture_decode()` (§15.2): IDWT each component, remove padding, clip and
/// offset to unsigned output ranges.
pub fn picture_decode(parsed: &ParsedPicture, seq: &SequenceHeader) -> Result<DecodedPicture> {
    let cp = &seq.coding_parameters;
    let tp = &parsed.transform_parameters;

    // inverse_wavelet_transform (§15.3) + idwt_pad_removal (§15.4.5).
    let y_plane = transform::idwt(&parsed.transform_data.y, tp)?;
    let c1_plane = transform::idwt(&parsed.transform_data.c1, tp)?;
    let c2_plane = transform::idwt(&parsed.transform_data.c2, tp)?;

    let y = finalize_component(
        &y_plane,
        cp.luma_width as usize,
        cp.luma_height as usize,
        cp.luma_depth,
    );
    let c1 = finalize_component(
        &c1_plane,
        cp.color_diff_width as usize,
        cp.color_diff_height as usize,
        cp.color_diff_depth,
    );
    let c2 = finalize_component(
        &c2_plane,
        cp.color_diff_width as usize,
        cp.color_diff_height as usize,
        cp.color_diff_depth,
    );

    Ok(DecodedPicture {
        picture_number: parsed.picture_number,
        luma_width: cp.luma_width as usize,
        luma_height: cp.luma_height as usize,
        color_diff_width: cp.color_diff_width as usize,
        color_diff_height: cp.color_diff_height as usize,
        y,
        c1,
        c2,
    })
}

/// Pad removal (§15.4.5) + clip (§15.5) + offset (§15.5) for one component.
fn finalize_component(plane: &Plane, width: usize, height: usize, depth: u32) -> Vec<u16> {
    let mut out = vec![0u16; width * height];
    // clip range: [-(2**(depth-1)), 2**(depth-1) - 1]; offset: +2**(depth-1).
    let half: i64 = 1i64 << (depth.saturating_sub(1));
    let lo = -half;
    let hi = half - 1;
    for y in 0..height {
        for x in 0..width {
            // delete_rows/columns_after: just read within [0,width)×[0,height).
            let v = plane.get(y, x);
            let clipped = v.clamp(lo, hi);
            let offset = clipped + half;
            out[y * width + x] = offset.clamp(0, u16::MAX as i64) as u16;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finalize_offsets_to_unsigned() {
        // depth 8: half = 128. value 0 -> clip to [-128,127] -> 0 -> +128 = 128.
        let p = Plane {
            width: 2,
            height: 1,
            data: vec![0, 100],
        };
        let out = finalize_component(&p, 2, 1, 8);
        assert_eq!(out, vec![128, 228]);
    }

    #[test]
    fn finalize_clips_to_range() {
        // depth 8: clip to [-128, 127]. value 500 -> 127 -> +128 = 255.
        let p = Plane {
            width: 1,
            height: 1,
            data: vec![500],
        };
        let out = finalize_component(&p, 1, 1, 8);
        assert_eq!(out, vec![255]);
    }
}
