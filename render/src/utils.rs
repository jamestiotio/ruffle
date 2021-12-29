use crate::bitmap::{Bitmap, BitmapFormat};
use crate::error::Error;
use std::borrow::Cow;
use std::io::Read;
use swf::Color;

/// The format of image data in a DefineBitsJpeg2/3 tag.
/// Generally this will be JPEG, but according to SWF19, these tags can also contain PNG and GIF data.
/// SWF19 pp.138-139
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum JpegTagFormat {
    Jpeg,
    Png,
    Gif,
    Unknown,
}

/// Determines the format of the image data in `data` from a DefineBitsJPEG2/3 tag.
pub fn determine_jpeg_tag_format(data: &[u8]) -> JpegTagFormat {
    match data {
        [0xff, 0xd8, ..] => JpegTagFormat::Jpeg,
        [0xff, 0xd9, 0xff, 0xd8, ..] => JpegTagFormat::Jpeg, // erroneous header in SWF
        [0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, ..] => JpegTagFormat::Png,
        [0x47, 0x49, 0x46, 0x38, 0x39, 0x61, ..] => JpegTagFormat::Gif,
        _ => JpegTagFormat::Unknown,
    }
}

/// Decodes bitmap data from a DefineBitsJPEG2/3 tag.
/// The data is returned with pre-multiplied alpha.
pub fn decode_define_bits_jpeg(data: &[u8], alpha_data: Option<&[u8]>) -> Result<Bitmap, Error> {
    let format = determine_jpeg_tag_format(data);
    if format != JpegTagFormat::Jpeg && alpha_data.is_some() {
        // Only DefineBitsJPEG3 with true JPEG data should have separate alpha data.
        log::warn!("DefineBitsJPEG contains non-JPEG data with alpha; probably incorrect")
    }
    match format {
        JpegTagFormat::Jpeg => decode_jpeg(data, alpha_data),
        JpegTagFormat::Png => decode_png(data),
        JpegTagFormat::Gif => decode_gif(data),
        JpegTagFormat::Unknown => Err(Error::UnknownType),
    }
}

/// Glues the JPEG encoding tables from a JPEGTables SWF tag to the JPEG data
/// in a DefineBits tag, producing complete JPEG data suitable for a decoder.
pub fn glue_tables_to_jpeg<'a>(
    jpeg_data: &'a [u8],
    jpeg_tables: Option<&'a [u8]>,
) -> Cow<'a, [u8]> {
    if let Some(jpeg_tables) = jpeg_tables {
        if jpeg_tables.len() >= 2 {
            let mut full_jpeg = Vec::with_capacity(jpeg_tables.len() + jpeg_data.len());
            full_jpeg.extend_from_slice(&jpeg_tables[..jpeg_tables.len() - 2]);
            if jpeg_data.len() >= 2 {
                full_jpeg.extend_from_slice(&jpeg_data[2..]);
            }

            return full_jpeg.into();
        }
    }

    // No JPEG tables or not enough data; return JPEG data as is
    jpeg_data.into()
}

/// Removes potential invalid JPEG data from SWF DefineBitsJPEG tags.
///
/// SWF19 p.138:
/// "Before version 8 of the SWF file format, SWF files could contain an erroneous header of 0xFF, 0xD9, 0xFF, 0xD8 before the JPEG SOI marker."
/// These bytes need to be removed for the JPEG to decode properly.
pub fn remove_invalid_jpeg_data(mut data: &[u8]) -> Cow<[u8]> {
    // TODO: Might be better to return an Box<Iterator<Item=u8>> instead of a Cow here,
    // where the spliced iter is a data[..n].chain(data[n+4..])?
    if data.starts_with(&[0xFF, 0xD9, 0xFF, 0xD8]) {
        data = &data[4..];
    }
    if let Some(pos) = data.windows(4).position(|w| w == [0xFF, 0xD9, 0xFF, 0xD8]) {
        let mut out_data = Vec::with_capacity(data.len() - 4);
        out_data.extend_from_slice(&data[..pos]);
        out_data.extend_from_slice(&data[pos + 4..]);
        out_data.into()
    } else {
        data.into()
    }
}

/// Decodes a JPEG with optional alpha data.
/// The decoded bitmap will have pre-multiplied alpha.
fn decode_jpeg(jpeg_data: &[u8], alpha_data: Option<&[u8]>) -> Result<Bitmap, Error> {
    let jpeg_data = remove_invalid_jpeg_data(jpeg_data);

    let mut decoder = jpeg_decoder::Decoder::new(&jpeg_data[..]);
    decoder.read_info()?;
    let metadata = decoder
        .info()
        .expect("info() should always return Some if read_info returned Ok");
    let decoded_data = decoder.decode()?;

    let decoded_data = match metadata.pixel_format {
        jpeg_decoder::PixelFormat::RGB24 => decoded_data,
        jpeg_decoder::PixelFormat::CMYK32 => decoded_data
            .chunks_exact(4)
            .flat_map(|cmyk| {
                let c = 255 - u16::from(cmyk[0]);
                let m = 255 - u16::from(cmyk[1]);
                let y = 255 - u16::from(cmyk[2]);
                let k = 256 - u16::from(cmyk[3]);

                let r = c * k / 255;
                let g = m * k / 255;
                let b = y * k / 255;
                [r as u8, g as u8, b as u8]
            })
            .collect(),
        jpeg_decoder::PixelFormat::L8 => {
            let mut rgb = Vec::with_capacity(decoded_data.len() * 3);
            for elem in decoded_data {
                rgb.push(elem);
                rgb.push(elem);
                rgb.push(elem);
            }
            rgb
        }
        jpeg_decoder::PixelFormat::L16 => {
            log::warn!("Unimplemented L16 JPEG pixel format");
            decoded_data
        }
    };

    // Decompress the alpha data (DEFLATE compression).
    if let Some(alpha_data) = alpha_data {
        let alpha_data = decompress_zlib(alpha_data)?;

        if alpha_data.len() == decoded_data.len() / 3 {
            let mut rgba = Vec::with_capacity((decoded_data.len() / 3) * 4);
            let mut i = 0;
            let mut a = 0;
            while i < decoded_data.len() {
                // The JPEG data should be premultiplied alpha, but it isn't in some incorrect SWFs (see #6893).
                // This means 0% alpha pixels may have color and incorrectly show as visible.
                // Flash Player clamps color to the alpha value to fix this case.
                // Only applies to DefineBitsJPEG3; DefineBitsLossless does not seem to clamp.
                let alpha = alpha_data[a];
                rgba.push(decoded_data[i].min(alpha));
                rgba.push(decoded_data[i + 1].min(alpha));
                rgba.push(decoded_data[i + 2].min(alpha));
                rgba.push(alpha);
                i += 3;
                a += 1;
            }
            return Ok(Bitmap::new(
                metadata.width.into(),
                metadata.height.into(),
                BitmapFormat::Rgba,
                rgba,
            ));
        } else {
            // Size isn't correct; fallback to RGB?
            log::error!("Size mismatch in DefineBitsJPEG3 alpha data");
        }
    }

    // No alpha.
    Ok(Bitmap::new(
        metadata.width.into(),
        metadata.height.into(),
        BitmapFormat::Rgb,
        decoded_data,
    ))
}

/// Decodes the bitmap data in DefineBitsLossless tag into RGBA.
/// DefineBitsLossless is Zlib encoded pixel data (similar to PNG), possibly
/// palletized.
pub fn decode_define_bits_lossless(swf_tag: &swf::DefineBitsLossless) -> Result<Bitmap, Error> {
    // Decompress the image data (DEFLATE compression).
    let mut decoded_data = decompress_zlib(swf_tag.data)?;

    // Swizzle/de-palettize the bitmap.
    let out_data = match (swf_tag.version, swf_tag.format) {
        (1, swf::BitmapFormat::Rgb15) => {
            let padded_width = (swf_tag.width + 0b1) & !0b1;
            let mut out_data: Vec<u8> =
                Vec::with_capacity(swf_tag.width as usize * swf_tag.height as usize * 4);
            let mut i = 0;
            for _ in 0..swf_tag.height {
                for _ in 0..swf_tag.width {
                    let compressed = u16::from_be_bytes([decoded_data[i], decoded_data[i + 1]]);
                    let rgb5_component = |shift: u16| {
                        let component = compressed >> shift & 0x1F;
                        ((component * 255 + 15) / 31) as u8
                    };
                    out_data.push(rgb5_component(10));
                    out_data.push(rgb5_component(5));
                    out_data.push(rgb5_component(0));
                    out_data.push(0xff);
                    i += 2;
                }
                i += (padded_width - swf_tag.width) as usize * 2;
            }
            out_data
        }
        (1, swf::BitmapFormat::Rgb32) => {
            let mut i = 0;
            while i < decoded_data.len() {
                decoded_data[i] = decoded_data[i + 1];
                decoded_data[i + 1] = decoded_data[i + 2];
                decoded_data[i + 2] = decoded_data[i + 3];
                decoded_data[i + 3] = 0xff;
                i += 4;
            }
            decoded_data
        }
        (2, swf::BitmapFormat::Rgb32) => {
            let mut i = 0;
            while i < decoded_data.len() {
                let alpha = decoded_data[i];
                decoded_data[i] = decoded_data[i + 1];
                decoded_data[i + 1] = decoded_data[i + 2];
                decoded_data[i + 2] = decoded_data[i + 3];
                decoded_data[i + 3] = alpha;
                i += 4;
            }
            decoded_data
        }
        (1, swf::BitmapFormat::ColorMap8 { num_colors }) => {
            let mut i = 0;
            let padded_width = (swf_tag.width + 0b11) & !0b11;

            let mut palette = Vec::with_capacity(num_colors as usize + 1);
            for _ in 0..=num_colors {
                palette.push(Color {
                    r: decoded_data[i],
                    g: decoded_data[i + 1],
                    b: decoded_data[i + 2],
                    a: 255,
                });
                i += 3;
            }
            let mut out_data: Vec<u8> =
                Vec::with_capacity(swf_tag.width as usize * swf_tag.height as usize * 4);
            for _ in 0..swf_tag.height {
                for _ in 0..swf_tag.width {
                    let entry = decoded_data[i] as usize;
                    if entry < palette.len() {
                        let color = &palette[entry];
                        out_data.push(color.r);
                        out_data.push(color.g);
                        out_data.push(color.b);
                        out_data.push(color.a);
                    } else {
                        out_data.push(0);
                        out_data.push(0);
                        out_data.push(0);
                        out_data.push(255);
                    }
                    i += 1;
                }
                i += (padded_width - swf_tag.width) as usize;
            }
            out_data
        }
        (2, swf::BitmapFormat::ColorMap8 { num_colors }) => {
            let mut i = 0;
            let padded_width = (swf_tag.width + 0b11) & !0b11;

            let mut palette = Vec::with_capacity(num_colors as usize + 1);
            for _ in 0..=num_colors {
                palette.push(Color {
                    r: decoded_data[i],
                    g: decoded_data[i + 1],
                    b: decoded_data[i + 2],
                    a: decoded_data[i + 3],
                });
                i += 4;
            }
            let mut out_data: Vec<u8> =
                Vec::with_capacity(swf_tag.width as usize * swf_tag.height as usize * 4);
            for _ in 0..swf_tag.height {
                for _ in 0..swf_tag.width {
                    let entry = decoded_data[i] as usize;
                    if entry < palette.len() {
                        let color = &palette[entry];
                        out_data.push(color.r);
                        out_data.push(color.g);
                        out_data.push(color.b);
                        out_data.push(color.a);
                    } else {
                        out_data.push(0);
                        out_data.push(0);
                        out_data.push(0);
                        out_data.push(0);
                    }
                    i += 1;
                }
                i += (padded_width - swf_tag.width) as usize;
            }
            out_data
        }
        _ => {
            return Err(Error::UnsupportedLosslessFormat(
                swf_tag.version,
                swf_tag.format,
            ));
        }
    };

    Ok(Bitmap::new(
        swf_tag.width.into(),
        swf_tag.height.into(),
        BitmapFormat::Rgba,
        out_data,
    ))
}

/// Decodes the bitmap data in DefineBitsLossless tag into RGBA.
/// DefineBitsLossless is Zlib encoded pixel data (similar to PNG), possibly
/// palletized.
fn decode_png(data: &[u8]) -> Result<Bitmap, Error> {
    use png::{ColorType, Transformations};

    let mut decoder = png::Decoder::new(data);
    // Normalize output to 8-bit grayscale or RGB.
    // Ideally we'd want to normalize to 8-bit RGB only, but seems like the `png` crate provides no such a feature.
    decoder.set_transformations(Transformations::normalize_to_color8());
    let mut reader = decoder.read_info()?;

    let mut data = vec![0; reader.output_buffer_size()];
    let info = reader.next_frame(&mut data)?;

    let (format, data) = match info.color_type {
        ColorType::Rgb => (BitmapFormat::Rgb, data),
        ColorType::Rgba => {
            // In contrast to DefineBitsLossless tags, PNGs embedded in a DefineBitsJPEG tag will not have
            // premultiplied alpha and need to be converted before sending to the renderer.
            premultiply_alpha_rgba(&mut data);
            (BitmapFormat::Rgba, data)
        }
        ColorType::Grayscale => (
            BitmapFormat::Rgb,
            data.into_iter().flat_map(|v| [v, v, v]).collect(),
        ),
        ColorType::GrayscaleAlpha => {
            (
                BitmapFormat::Rgba,
                data.chunks_exact(2)
                    .flat_map(|pixel| {
                        // Pre-multiply alpha.
                        let a = pixel[1];
                        let v = (u16::from(pixel[0]) * u16::from(a) / 255) as u8;
                        [v, v, v, a]
                    })
                    .collect(),
            )
        }
        ColorType::Indexed => {
            // Shouldn't get here because of `normalize_to_color8` transformation above.
            unreachable!("Unexpected PNG ColorType::Indexed");
        }
    };

    Ok(Bitmap::new(info.width, info.height, format, data))
}

/// Decodes the bitmap data in DefineBitsLossless tag into RGBA.
/// DefineBitsLossless is Zlib encoded pixel data (similar to PNG), possibly
/// palletized.
fn decode_gif(data: &[u8]) -> Result<Bitmap, Error> {
    let mut decode_options = gif::DecodeOptions::new();
    decode_options.set_color_output(gif::ColorOutput::RGBA);
    let mut reader = decode_options.read_info(data)?;
    let frame = reader.read_next_frame()?.ok_or(Error::EmptyGif)?;
    // GIFs embedded in a DefineBitsJPEG tag will not have premultiplied alpha and need to be converted before sending to the renderer.
    let mut data = frame.buffer.to_vec();
    premultiply_alpha_rgba(&mut data);

    Ok(Bitmap::new(
        frame.width.into(),
        frame.height.into(),
        BitmapFormat::Rgba,
        data,
    ))
}

/// Converts standard RBGA to premultiplied alpha.
fn premultiply_alpha_rgba(rgba: &mut [u8]) {
    rgba.chunks_exact_mut(4).for_each(|rgba| {
        let a = f32::from(rgba[3]) / 255.0;
        rgba[0] = (f32::from(rgba[0]) * a) as u8;
        rgba[1] = (f32::from(rgba[1]) * a) as u8;
        rgba[2] = (f32::from(rgba[2]) * a) as u8;
    })
}

/// Converts premultiplied RBGA to unmultipled RGBA.
pub fn unmultiply_alpha_rgba(rgba: &mut [u8]) {
    rgba.chunks_exact_mut(4).for_each(|rgba| {
        let a = rgba[3];
        if a > 0 {
            let a = f32::from(a) / 255.0;
            rgba[0] = (f32::from(rgba[0]) / a) as u8;
            rgba[1] = (f32::from(rgba[1]) / a) as u8;
            rgba[2] = (f32::from(rgba[2]) / a) as u8;
        }
    })
}

/// Decodes zlib-compressed data.
fn decompress_zlib(data: &[u8]) -> Result<Vec<u8>, Error> {
    let mut out_data = Vec::new();
    let mut decoder = flate2::bufread::ZlibDecoder::new(data);
    decoder
        .read_to_end(&mut out_data)
        .map_err(|_| Error::InvalidZlibCompression)?;
    out_data.shrink_to_fit();
    Ok(out_data)
}