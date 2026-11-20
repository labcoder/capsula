use std::{fmt, io::Write as _};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use zeroize::Zeroizing;

use super::{input_text, instructions};
use crate::context::{BOUND, InputItem, ModelRequest, Role, ToolSchema};

const JPEG_CAP: usize = 2 * 1024 * 1024;
const LINE_CAP: usize = 4 * 1024 * 1024;
const LINE_START: &[u8] = br#"{"message":{"content":["#;
const LINE_END: &[u8] = b"],\"role\":\"user\"},\"type\":\"user\"}\n";
const IMAGE_START: &[u8] =
    br#",{"type":"image","source":{"type":"base64","media_type":"image/jpeg","data":""#;
const IMAGE_END: &[u8] = br#""}}"#;

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub(super) enum PictureSeedError {
    #[error("the seed already contains an image")]
    DuplicateImage,
    #[error("the seed has no user message")]
    NoUser,
    #[error("the seed JPEG has no JPEG marker")]
    NotJpeg,
    #[error("the seed JPEG has invalid dimensions")]
    Dimensions,
    #[error("the seed JPEG exceeds its size limit")]
    JpegSize,
    #[error("the seed without its image exceeds its size limit")]
    NonImageSize,
    #[error("the pictured seed exceeds its line limit")]
    LineSize,
    #[error("the seed JPEG could not be encoded")]
    Encoding,
}

pub(super) struct PicturedSeed(Zeroizing<Vec<u8>>);

impl PicturedSeed {
    pub(super) fn bytes(&self) -> &[u8] {
        &self.0
    }
}

impl fmt::Debug for PicturedSeed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PicturedSeed")
            .field("len", &self.0.len())
            .finish()
    }
}

/// Packs a trusted capture, not permission to send it. The request must not already carry an image.
pub(super) fn encode(
    request: &ModelRequest,
    tools: &[ToolSchema],
    jpeg: &[u8],
    [width, height]: [u32; 2],
) -> Result<PicturedSeed, PictureSeedError> {
    use PictureSeedError as Error;

    if jpeg.len() > JPEG_CAP {
        return Err(Error::JpegSize);
    }
    if !jpeg.starts_with(&[0xff, 0xd8, 0xff]) {
        return Err(Error::NotJpeg);
    }
    if !(1..=1280).contains(&width) || !(1..=720).contains(&height) {
        return Err(Error::Dimensions);
    }
    if request
        .input
        .iter()
        .any(|item| matches!(item, InputItem::Image(_)))
    {
        return Err(Error::DuplicateImage);
    }
    let newest = request
        .input
        .iter()
        .rposition(|item| {
            matches!(
                item,
                InputItem::Message {
                    role: Role::User,
                    ..
                }
            )
        })
        .ok_or(Error::NoUser)?;

    let mut non_image = Zeroizing::new(vec![0; BOUND]);
    let mut room = non_image.as_mut_slice();
    room.write_all(LINE_START)
        .map_err(|_| Error::NonImageSize)?;
    serde_json::to_writer(&mut room, &instructions(request, tools))
        .map_err(|_| Error::NonImageSize)?;
    let mut insert_at = 0;
    for (index, item) in request.input.iter().enumerate() {
        if let Some(block) = input_text(item) {
            room.write_all(b",").map_err(|_| Error::NonImageSize)?;
            serde_json::to_writer(&mut room, &block).map_err(|_| Error::NonImageSize)?;
        }
        if index == newest {
            insert_at = BOUND - room.len();
        }
    }
    room.write_all(LINE_END).map_err(|_| Error::NonImageSize)?;
    let non_image_len = BOUND - room.len();
    let total = pictured_len(non_image_len, jpeg.len(), LINE_CAP)?;

    let mut seed = Zeroizing::new(vec![0; total]);
    let (before, rest) = seed.split_at_mut(insert_at);
    let (image, after) = rest.split_at_mut(total - non_image_len);
    let (head, rest) = image.split_at_mut(IMAGE_START.len());
    let (data, end) = rest.split_at_mut(rest.len() - IMAGE_END.len());
    before.copy_from_slice(&non_image[..insert_at]);
    head.copy_from_slice(IMAGE_START);
    STANDARD
        .encode_slice(jpeg, data)
        .map_err(|_| Error::Encoding)?;
    end.copy_from_slice(IMAGE_END);
    after.copy_from_slice(&non_image[insert_at..non_image_len]);
    Ok(PicturedSeed(seed))
}

fn pictured_len(non_image: usize, jpeg: usize, ceiling: usize) -> Result<usize, PictureSeedError> {
    use PictureSeedError as Error;

    if non_image > BOUND {
        return Err(Error::NonImageSize);
    }
    if jpeg > JPEG_CAP {
        return Err(Error::JpegSize);
    }
    let total = non_image + IMAGE_START.len() + jpeg.div_ceil(3) * 4 + IMAGE_END.len();
    (total <= ceiling).then_some(total).ok_or(Error::LineSize)
}

#[cfg(test)]
mod tests;
