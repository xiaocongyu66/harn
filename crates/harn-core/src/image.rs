//! 图片输入：读取本地文件并编码为 base64 内容块。

use std::path::Path;

use base64::Engine;

use crate::error::{HarnError, Result};
use crate::message::ContentBlock;

pub const MAX_IMAGE_BYTES: u64 = 5 * 1024 * 1024;

pub fn image_block_from_path(path: &Path) -> Result<ContentBlock> {
    let metadata = std::fs::metadata(path)
        .map_err(|e| HarnError::Image(format!("读取图片 `{}` 失败：{e}", path.display())))?;
    if metadata.len() > MAX_IMAGE_BYTES {
        return Err(HarnError::Image(format!(
            "图片 `{}` 超过 5MB 限制（{} bytes）",
            path.display(),
            metadata.len()
        )));
    }
    let media_type = media_type_for(path).ok_or_else(|| {
        HarnError::Image(format!(
            "不支持的图片类型 `{}`（支持 png/jpeg/gif/webp）",
            path.display()
        ))
    })?;
    let bytes = std::fs::read(path)
        .map_err(|e| HarnError::Image(format!("读取图片 `{}` 失败：{e}", path.display())))?;
    let data = base64::engine::general_purpose::STANDARD.encode(bytes);
    Ok(ContentBlock::image(media_type, data))
}

fn media_type_for(path: &Path) -> Option<&'static str> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    match ext.as_str() {
        "png" => Some("image/png"),
        "jpg" | "jpeg" => Some("image/jpeg"),
        "gif" => Some("image/gif"),
        "webp" => Some("image/webp"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_unsupported_extension() {
        assert!(media_type_for(Path::new("a.txt")).is_none());
        assert_eq!(media_type_for(Path::new("a.PNG")), Some("image/png"));
    }
}
