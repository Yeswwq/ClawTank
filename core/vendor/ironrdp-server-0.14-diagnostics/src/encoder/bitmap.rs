use ironrdp_core::{Encode as _, WriteCursor, cast_int, cast_length};
use ironrdp_graphics::image_processing::PixelFormat;
use ironrdp_graphics::rdp6::{
    ABgrChannels, ARgbChannels, BgrAChannels, BitmapEncodeError, BitmapStreamEncoder, RgbAChannels,
};
use ironrdp_pdu::bitmap::{self, BitmapData, BitmapUpdateData, Compression};
use ironrdp_pdu::geometry::InclusiveRectangle;

use crate::BitmapUpdate;

// PERF: we could also remove the need for this buffer
#[derive(Clone)]
pub(crate) struct BitmapEncoder {
    buffer: Vec<u8>,
}

impl BitmapEncoder {
    pub(crate) fn new() -> Self {
        Self {
            buffer: vec![0; usize::from(u16::MAX)],
        }
    }

    /// Largest bitmap tile sent in one TS_BITMAP_DATA.
    ///
    /// FreeRDP refuses a planar bitmap larger than its decoding context ([planar.c]), which it sizes to the desktop
    /// when connecting ([codecs.c], [connection.c]); before [FreeRDP#13668] it kept that size when the server
    /// resized the desktop, so a client disconnected on the first bitmap wider or taller than its first desktop.
    /// A 64x64 tile fits the context of any desktop of at least 64x64.
    ///
    /// [planar.c]: https://github.com/FreeRDP/FreeRDP/blob/d9991b9fa1b6e92f6c19679c6324023f771a38c1/libfreerdp/codec/planar.c#L744-L755
    /// [codecs.c]: https://github.com/FreeRDP/FreeRDP/blob/d9991b9fa1b6e92f6c19679c6324023f771a38c1/libfreerdp/core/codecs.c#L170-L189
    /// [connection.c]: https://github.com/FreeRDP/FreeRDP/blob/d9991b9fa1b6e92f6c19679c6324023f771a38c1/libfreerdp/core/connection.c#L218-L220
    /// [FreeRDP#13668]: https://github.com/FreeRDP/FreeRDP/pull/13668
    const TILE: u16 = 64;

    pub(crate) fn encode(
        &mut self,
        bitmap: &BitmapUpdate,
        output: &mut [u8],
    ) -> Result<usize, BitmapEncodeError> {
        let bytes_per_pixel = usize::from(bitmap.format.bytes_per_pixel());
        let stride = bitmap.stride.get();
        let (width, height) = (bitmap.width.get(), bitmap.height.get());
        let tiles_x = width.div_ceil(Self::TILE);
        let tiles_y = height.div_ceil(Self::TILE);

        let mut cursor = WriteCursor::new(output);
        let total = cast_int!("tile count", usize::from(tiles_x) * usize::from(tiles_y))
            .map_err(BitmapEncodeError::Encode)?;
        BitmapUpdateData::encode_header(total, &mut cursor).map_err(BitmapEncodeError::Encode)?;

        for ty in 0..tiles_y {
            let top = ty * Self::TILE;
            let tile_h = Self::TILE.min(height - top);
            for tx in 0..tiles_x {
                let left = tx * Self::TILE;
                let tile_w = Self::TILE.min(width - left);
                // Bitmap widths must be a multiple of 4 (MS-RDPBCGR 2.2.9.1.1.3.1.2.3).
                // An edge tile of an odd-sized desktop is padded by repeating
                // its last column; the destination rectangle below stays the
                // real area, so the padding is never drawn.
                let enc_w = tile_w.next_multiple_of(4);
                let row_len = usize::from(tile_w) * bytes_per_pixel;
                let col = usize::from(left) * bytes_per_pixel;

                let encoder = BitmapStreamEncoder::new(usize::from(enc_w), usize::from(tile_h));
                let len = {
                    let pad = usize::from(enc_w - tile_w);
                    // Planar bitmaps are bottom-up.
                    let pixels = (usize::from(top)..usize::from(top + tile_h))
                        .rev()
                        .flat_map(|y| {
                            let row = &bitmap.data[y * stride + col..y * stride + col + row_len];
                            let last = &row[row_len - bytes_per_pixel..];
                            row.chunks(bytes_per_pixel)
                                .chain(core::iter::repeat_n(last, pad))
                        });
                    Self::encode_iter(encoder, bitmap.format, pixels, self.buffer.as_mut_slice())?
                };

                let data = BitmapData {
                    rectangle: InclusiveRectangle {
                        left: bitmap.x + left,
                        top: bitmap.y + top,
                        right: bitmap.x + left + tile_w - 1,
                        bottom: bitmap.y + top + tile_h - 1,
                    },
                    width: enc_w,
                    height: tile_h,
                    bits_per_pixel: u16::from(bitmap.format.bytes_per_pixel()) * 8,
                    compression_flags: Compression::BITMAP_COMPRESSION,
                    compressed_data_header: Some(bitmap::CompressedDataHeader {
                        main_body_size: cast_length!("main body size", len)
                            .map_err(BitmapEncodeError::Encode)?,
                        // Android Windows App validates this field as the row length in
                        // bytes for the RDP 6.0 32-bpp path. xrdp and FreeRDP's planar
                        // server path also emit bytes here. The bitmap width field above
                        // remains the pixel width.
                        scan_width: cast_length!(
                            "scan width",
                            usize::from(enc_w) * bytes_per_pixel
                        )
                        .map_err(BitmapEncodeError::Encode)?,
                        uncompressed_size: cast_length!(
                            "uncompressed size",
                            usize::from(tile_h) * usize::from(enc_w) * bytes_per_pixel
                        )
                        .map_err(BitmapEncodeError::Encode)?,
                    }),
                    bitmap_data: &self.buffer[..len],
                };

                data.encode(&mut cursor)
                    .map_err(BitmapEncodeError::Encode)?;
            }
        }

        Ok(cursor.pos())
    }

    fn encode_iter<'a, P>(
        mut encoder: BitmapStreamEncoder,
        format: PixelFormat,
        src: P,
        dst: &mut [u8],
    ) -> Result<usize, BitmapEncodeError>
    where
        P: Iterator<Item = &'a [u8]> + Clone,
    {
        let written = match format {
            PixelFormat::ARgb32 | PixelFormat::XRgb32 => {
                encoder.encode_pixels_stream::<_, ARgbChannels>(src, dst, true)?
            }
            PixelFormat::RgbA32 | PixelFormat::RgbX32 => {
                encoder.encode_pixels_stream::<_, RgbAChannels>(src, dst, true)?
            }
            PixelFormat::ABgr32 | PixelFormat::XBgr32 => {
                encoder.encode_pixels_stream::<_, ABgrChannels>(src, dst, true)?
            }
            PixelFormat::BgrA32 | PixelFormat::BgrX32 => {
                encoder.encode_pixels_stream::<_, BgrAChannels>(src, dst, true)?
            }
        };

        Ok(written)
    }
}

#[cfg(test)]
mod tests {
    use core::num::{NonZeroU16, NonZeroUsize};

    use bytes::Bytes;
    use ironrdp_graphics::image_processing::PixelFormat;
    use ironrdp_graphics::rdp6::BitmapStreamDecoder;

    use super::BitmapEncoder;
    use crate::BitmapUpdate;
    use crate::encoder::parse_first_bitmap_update_metadata;

    #[test]
    fn rdp6_wire_header_uses_byte_scan_width_for_full_and_edge_tiles() {
        let width = 68usize;
        let height = 64usize;
        let bitmap = BitmapUpdate {
            x: 0,
            y: 0,
            width: NonZeroU16::new(width as u16).unwrap(),
            height: NonZeroU16::new(height as u16).unwrap(),
            format: PixelFormat::ARgb32,
            data: Bytes::from(vec![0; width * height * 4]),
            stride: NonZeroUsize::new(width * 4).unwrap(),
        };
        let mut encoder = BitmapEncoder::new();
        let mut encoded = vec![0; width * height * 2];
        let written = encoder.encode(&bitmap, &mut encoded).unwrap();
        let metadata = parse_first_bitmap_update_metadata(&encoded[..written]).unwrap();

        assert_eq!(metadata.tiles.len(), 2);
        assert_eq!(metadata.tiles[0].width, 64);
        assert_eq!(metadata.tiles[0].cb_scan_width, Some(256));
        assert_eq!(metadata.tiles[0].cb_uncompressed_size, Some(16_384));
        assert_eq!(metadata.tiles[0].cb_comp_first_row_size, Some(0));
        assert_eq!(
            metadata.tiles[0].cb_comp_main_body_size,
            metadata.tiles[0]
                .compressed_body_length
                .map(|length| length as u16)
        );
        assert_eq!(
            metadata.tiles[0].bitmap_data_length_field,
            8 + metadata.tiles[0].compressed_body_length.unwrap()
        );
        assert_eq!(
            metadata.tiles[0].bitmap_data_length_field,
            metadata.tiles[0].bitmap_stream_length
        );
        assert!(metadata.tiles[0].ts_cd_header_complete);
        assert_eq!(metadata.tiles[1].width, 4);
        assert_eq!(metadata.tiles[1].cb_scan_width, Some(16));
        assert_eq!(metadata.tiles[1].cb_uncompressed_size, Some(1_024));
    }

    #[test]
    fn bgra_input_encodes_the_expected_rgb_color_planes() {
        let bitmap = BitmapUpdate {
            x: 0,
            y: 0,
            width: NonZeroU16::new(4).unwrap(),
            height: NonZeroU16::new(1).unwrap(),
            format: PixelFormat::BgrA32,
            // Four BGRA cyan pixels; the rows are bottom-up at the bitmap level but this
            // single-row fixture keeps the color-plane bytes easy to inspect.
            data: Bytes::from_static(&[
                255, 255, 0, 0xff, 255, 255, 0, 0xff, 255, 255, 0, 0xff, 255, 255, 0, 0xff,
            ]),
            stride: NonZeroUsize::new(4).unwrap(),
        };
        let mut encoder = BitmapEncoder::new();
        let mut encoded = vec![0; 4096];
        let written = encoder.encode(&bitmap, &mut encoded).unwrap();
        let body_start = 4 + 18;
        let stream_length = usize::from(u16::from_le_bytes([
            encoded[body_start + 6],
            encoded[body_start + 7],
        ]));
        let stream = &encoded[body_start + 8..body_start + 8 + stream_length];
        let mut decoder = BitmapStreamDecoder::default();
        let mut rgb = Vec::new();
        decoder
            .decode_bitmap_stream_to_rgb24(stream, &mut rgb, 4, 1)
            .unwrap();
        assert!(written > body_start + 8);
        assert_eq!(rgb, [0, 255, 255, 0, 255, 255, 0, 255, 255, 0, 255, 255]);
    }
}
