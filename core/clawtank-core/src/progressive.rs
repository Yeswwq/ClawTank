//! RemoteFX Progressive encoder for BGRA desktop frames.
//!
//! The encoder emits one full-quality `TileSimple` pass per 64x64 tile. It keeps no cross-frame
//! coefficient state, so each frame is independently decodable and resize/reconnect need no
//! encoder-state migration.

use ironrdp_graphics::progressive::{
    COEFFICIENTS_PER_COMPONENT, TILE_DIM, encode_first_pass, rgba_to_ycbcr,
};
use ironrdp_pdu::codecs::rfx::{
    RfxRectangle,
    progressive::{
        ComponentCodecQuant, ProgressiveBlock, ProgressiveContextPdu, ProgressiveFrameBeginPdu,
        ProgressiveFrameEndPdu, ProgressiveRegion, ProgressiveSyncPdu, ProgressiveTile, TileSimple,
        encode_progressive_stream,
    },
};

const BYTES_PER_PIXEL: usize = 4;
const MAX_DESKTOP_SIDE: u16 = 3840;
const MAX_DESKTOP_AREA: u64 = 3840 * 2400;
const INITIAL_RLGR_OUTPUT: usize = 16 * 1024;
const MAX_WIRE_COMPONENT: usize = u16::MAX as usize;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProgressiveEncodeError {
    InvalidDimensions,
    InvalidStride,
    BufferTooSmall,
    ComponentEncode,
    StreamEncode,
}

/// Encodes one BGRA frame as a RemoteFX Progressive stream for `WireToSurface2`.
///
/// Stride may include row padding, as is common for ScreenCaptureKit IOSurfaces. Partial edge
/// tiles replicate the final source row/column so the DWT does not introduce a synthetic black
/// border. The frame region clips those padded pixels to the actual desktop dimensions.
pub fn encode_bgra_frame(
    data: &[u8],
    width: u16,
    height: u16,
    stride: usize,
) -> Result<Vec<u8>, ProgressiveEncodeError> {
    if width == 0
        || height == 0
        || width > MAX_DESKTOP_SIDE
        || height > MAX_DESKTOP_SIDE
        || u64::from(width) * u64::from(height) > MAX_DESKTOP_AREA
    {
        return Err(ProgressiveEncodeError::InvalidDimensions);
    }
    let row_bytes = usize::from(width)
        .checked_mul(BYTES_PER_PIXEL)
        .ok_or(ProgressiveEncodeError::InvalidStride)?;
    if stride < row_bytes {
        return Err(ProgressiveEncodeError::InvalidStride);
    }
    let required_len = stride
        .checked_mul(usize::from(height))
        .ok_or(ProgressiveEncodeError::BufferTooSmall)?;
    if data.len() < required_len {
        return Err(ProgressiveEncodeError::BufferTooSmall);
    }

    let tiles_wide = width.div_ceil(TILE_DIM);
    let tiles_high = height.div_ceil(TILE_DIM);
    let tile_count = usize::from(tiles_wide) * usize::from(tiles_high);
    // Encode all component payloads before constructing `TileSimple` values. The PDU stores
    // borrowed slices, so building tiles while this vector is still growing would leave those
    // slices pointing at allocations invalidated by a later reallocation.
    let mut component_data = Vec::with_capacity(tile_count);

    for tile_y in 0..tiles_high {
        for tile_x in 0..tiles_wide {
            let pixels = extract_rgba_tile(data, width, height, stride, tile_x, tile_y);
            let mut y = [0_i16; COEFFICIENTS_PER_COMPONENT];
            let mut cb = [0_i16; COEFFICIENTS_PER_COMPONENT];
            let mut cr = [0_i16; COEFFICIENTS_PER_COMPONENT];
            rgba_to_ycbcr(&pixels, &mut y, &mut cb, &mut cr);

            let encoded_components = [
                encode_component(y)?,
                encode_component(cb)?,
                encode_component(cr)?,
            ];
            component_data.push(encoded_components);
        }
    }

    let tiles = component_data
        .iter()
        .enumerate()
        .map(|(index, encoded)| {
            let tile_x = (index % usize::from(tiles_wide)) as u16;
            let tile_y = (index / usize::from(tiles_wide)) as u16;
            ProgressiveTile::Simple(TileSimple {
                quant_idx_y: 0,
                quant_idx_cb: 0,
                quant_idx_cr: 0,
                x_idx: tile_x,
                y_idx: tile_y,
                flags: 0,
                y_data: &encoded[0],
                cb_data: &encoded[1],
                cr_data: &encoded[2],
                tail_data: &[],
            })
        })
        .collect();

    let region = ProgressiveRegion {
        tile_size: 0x40,
        rects: vec![RfxRectangle {
            x: 0,
            y: 0,
            width,
            height,
        }],
        quant_vals: vec![ComponentCodecQuant::LOSSLESS],
        quant_prog_vals: vec![],
        flags: 0,
        tiles,
    };
    let blocks = [
        ProgressiveBlock::Sync(ProgressiveSyncPdu),
        ProgressiveBlock::Context(ProgressiveContextPdu {
            context_id: 0,
            tile_size: 0x0040,
            flags: 0,
        }),
        ProgressiveBlock::FrameBegin(ProgressiveFrameBeginPdu {
            frame_index: 0,
            region_count: 1,
        }),
        ProgressiveBlock::Region(region),
        ProgressiveBlock::FrameEnd(ProgressiveFrameEndPdu),
    ];
    encode_progressive_stream(&blocks).map_err(|_| ProgressiveEncodeError::StreamEncode)
}

/// RLGR is entropy coding rather than a bounded-rate codec: a high-entropy lossless tile can
/// require more output bytes than the first buffer guess. Retry with the exact required size
/// reported by IronRDP, while respecting the 16-bit component lengths in TILE_SIMPLE.
fn encode_component(
    coefficients: [i16; COEFFICIENTS_PER_COMPONENT],
) -> Result<Vec<u8>, ProgressiveEncodeError> {
    let mut output = vec![0_u8; INITIAL_RLGR_OUTPUT];

    loop {
        let mut working = coefficients;
        match encode_first_pass(
            &mut working,
            &mut output,
            &ComponentCodecQuant::LOSSLESS,
            &ComponentCodecQuant::LOSSLESS,
            false,
        ) {
            Ok(encoded_len) => {
                output.truncate(encoded_len);
                return Ok(output);
            }
            Err(ironrdp_graphics::rlgr::RlgrError::OutputTooSmall { needed, .. })
                if needed <= MAX_WIRE_COMPONENT =>
            {
                output.resize(needed, 0);
            }
            Err(_) => return Err(ProgressiveEncodeError::ComponentEncode),
        }
    }
}

fn extract_rgba_tile(
    data: &[u8],
    width: u16,
    height: u16,
    stride: usize,
    tile_x: u16,
    tile_y: u16,
) -> [u8; 64 * 64 * BYTES_PER_PIXEL] {
    let mut pixels = [0_u8; 64 * 64 * BYTES_PER_PIXEL];
    let origin_x = usize::from(tile_x) * usize::from(TILE_DIM);
    let origin_y = usize::from(tile_y) * usize::from(TILE_DIM);
    let width = usize::from(width);
    let height = usize::from(height);

    for y in 0..usize::from(TILE_DIM) {
        let source_y = (origin_y + y).min(height - 1);
        for x in 0..usize::from(TILE_DIM) {
            let source_x = (origin_x + x).min(width - 1);
            let source_offset = source_y * stride + source_x * BYTES_PER_PIXEL;
            let destination_offset = (y * usize::from(TILE_DIM) + x) * BYTES_PER_PIXEL;
            // The progressive color transform expects RGBA; ScreenCaptureKit provides BGRA.
            pixels[destination_offset] = data[source_offset + 2];
            pixels[destination_offset + 1] = data[source_offset + 1];
            pixels[destination_offset + 2] = data[source_offset];
            pixels[destination_offset + 3] = data[source_offset + 3];
        }
    }
    pixels
}

#[cfg(test)]
mod tests {
    use super::*;
    use ironrdp_graphics::progressive::ProgressiveDecoder;

    #[test]
    fn encodes_padded_partial_tiles_and_round_trips_through_ironrdp_decoder() {
        let (width, height, stride) = (70_usize, 67_usize, 70 * 4 + 16);
        let mut bgra = vec![0_u8; stride * height];
        for y in 0..height {
            for x in 0..width {
                let offset = y * stride + x * 4;
                bgra[offset] = (x * 3) as u8;
                bgra[offset + 1] = (y * 5) as u8;
                bgra[offset + 2] = ((x + y) * 2) as u8;
                bgra[offset + 3] = 255;
            }
        }

        let stream = encode_bgra_frame(&bgra, width as u16, height as u16, stride)
            .expect("valid padded BGRA frame encodes");
        let mut decoder = ProgressiveDecoder::new();
        let decoded = decoder
            .decode_bitmap(1, 0, width as u16, height as u16, &stream)
            .expect("IronRDP decodes encoder output");

        assert_eq!(decoded.len(), 4);
        assert_eq!(
            decoded.iter().map(|tile| tile.pixels.len()).sum::<usize>(),
            4 * 64 * 64 * 4
        );
        assert!(
            decoded
                .iter()
                .all(|tile| !tile.update_rectangles.is_empty())
        );
    }

    #[test]
    fn rejects_invalid_layouts_before_encoding() {
        assert_eq!(
            encode_bgra_frame(&[], 0, 10, 0),
            Err(ProgressiveEncodeError::InvalidDimensions)
        );
        assert_eq!(
            encode_bgra_frame(&[0; 400], 10, 10, 39),
            Err(ProgressiveEncodeError::InvalidStride)
        );
        assert_eq!(
            encode_bgra_frame(&[0; 399], 10, 10, 40),
            Err(ProgressiveEncodeError::BufferTooSmall)
        );
    }
}
