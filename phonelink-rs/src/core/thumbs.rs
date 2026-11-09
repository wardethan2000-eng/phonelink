//! Quick thumbnails for Recent Photos (issue #10 follow-up).
//!
//! Reading a whole camera photo over KDE Connect's sftp mount took 0.6 to
//! 2.4 s each on Ethan's phone (2-2.7 MB over Wi-Fi, 2026-11-08). Camera
//! JPEGs carry a 512x384 preview in their EXIF block within the first ~60 KB,
//! so only that much is read; finished thumbnails are kept on disk, keyed by
//! path, size and time, so each photo is fetched once.

use std::path::{Path, PathBuf};

/// Bytes read from the start of a photo to find its preview.
pub const HEAD_BYTES: usize = 128 * 1024;
/// Never read more than this for a preview.
pub const MAX_PREVIEW_END: usize = 512 * 1024;

/// What the EXIF block says about a photo's preview.
#[derive(Debug, Clone, PartialEq)]
pub struct Preview {
    /// Where the preview JPEG is, from the start of the file.
    pub start: usize,
    pub len: usize,
    /// EXIF orientation (1 = upright), which the preview shares.
    pub orientation: u16,
    /// The main picture's size, when the EXIF block says.
    pub width: Option<u32>,
    pub height: Option<u32>,
}

impl Preview {
    pub fn end(&self) -> usize {
        self.start + self.len
    }
}

/// An IFD entry: (tag, type, value).
type Entry = (u16, u16, u32);

struct Tiff<'a> {
    d: &'a [u8],
    big: bool,
}

impl Tiff<'_> {
    fn u16(&self, at: usize) -> Option<u16> {
        let b = self.d.get(at..at + 2)?;
        Some(if self.big { u16::from_be_bytes([b[0], b[1]]) } else { u16::from_le_bytes([b[0], b[1]]) })
    }

    fn u32(&self, at: usize) -> Option<u32> {
        let b = self.d.get(at..at + 4)?;
        let a = [b[0], b[1], b[2], b[3]];
        Some(if self.big { u32::from_be_bytes(a) } else { u32::from_le_bytes(a) })
    }

    /// The entries of the IFD at `at` as (tag, type, value), and the next IFD's offset.
    fn ifd(&self, at: usize) -> Option<(Vec<Entry>, u32)> {
        let n = self.u16(at)? as usize;
        if n > 512 {
            return None;
        }
        let mut out = Vec::with_capacity(n);
        for i in 0..n {
            let e = at + 2 + i * 12;
            let (tag, kind) = (self.u16(e)?, self.u16(e + 2)?);
            // SHORT values sit in the first two bytes of the value field.
            let value = if kind == 3 { self.u16(e + 8)? as u32 } else { self.u32(e + 8)? };
            out.push((tag, kind, value));
        }
        let next = self.u32(at + 2 + n * 12).unwrap_or(0);
        Some((out, next))
    }
}

/// Find the EXIF preview in the first bytes of a JPEG. None for anything
/// else, or when there is no preview.
pub fn exif_preview(head: &[u8]) -> Option<Preview> {
    if head.get(..2)? != [0xff, 0xd8] {
        return None;
    }
    let mut at = 2;
    // Walk the segments before the image data, looking for APP1 "Exif".
    while at + 4 <= head.len() {
        if head[at] != 0xff {
            return None;
        }
        let marker = head[at + 1];
        if marker == 0xda || marker == 0xd9 {
            return None;
        }
        let len = u16::from_be_bytes([head[at + 2], head[at + 3]]) as usize;
        let body = at + 4;
        if marker == 0xe1 && head.get(body..body + 6) == Some(b"Exif\0\0") {
            return tiff_preview(head, body + 6);
        }
        at += 2 + len;
    }
    None
}

fn tiff_preview(head: &[u8], base: usize) -> Option<Preview> {
    let d = head.get(base..)?;
    let big = match d.get(..2)? {
        b"MM" => true,
        b"II" => false,
        _ => return None,
    };
    let t = Tiff { d, big };
    let (ifd0, next) = t.ifd(t.u32(4)? as usize)?;
    let mut orientation = 1;
    let (mut width, mut height) = (None, None);
    for &(tag, _, v) in &ifd0 {
        match tag {
            0x0112 => orientation = v as u16,
            0x8769 => {
                if let Some((sub, _)) = t.ifd(v as usize) {
                    for &(tag, _, v) in &sub {
                        match tag {
                            0xa002 => width = Some(v),
                            0xa003 => height = Some(v),
                            _ => {}
                        }
                    }
                }
            }
            _ => {}
        }
    }
    if next == 0 {
        return None;
    }
    let (ifd1, _) = t.ifd(next as usize)?;
    let find = |tag| ifd1.iter().find(|e| e.0 == tag).map(|e| e.2 as usize);
    let (offset, len) = (find(0x0201)?, find(0x0202)?);
    if len == 0 || len > MAX_PREVIEW_END {
        return None;
    }
    let start = base + offset;
    if start + len > MAX_PREVIEW_END {
        return None;
    }
    Some(Preview {
        start,
        len,
        orientation: if (1..=8).contains(&orientation) { orientation } else { 1 },
        width,
        height,
    })
}

/// The part of a `w`x`h` preview that shows the photo when the photo has a
/// different shape (some phones pad the preview with black bars): (x, y,
/// width, height).
pub fn preview_content(w: i32, h: i32, photo: Option<(u32, u32)>) -> (i32, i32, i32, i32) {
    let Some((pw, ph)) = photo.filter(|(a, b)| *a > 0 && *b > 0) else { return (0, 0, w, h) };
    // The EXIF size is before rotation, like the preview.
    let photo_ratio = pw as f64 / ph as f64;
    let ratio = w as f64 / h as f64;
    if (photo_ratio - ratio).abs() < 0.02 * ratio {
        return (0, 0, w, h);
    }
    if photo_ratio > ratio {
        // Wider photo: bars above and below.
        let ch = ((w as f64 / photo_ratio).round() as i32).clamp(1, h);
        (0, (h - ch) / 2, w, ch)
    } else {
        let cw = ((h as f64 * photo_ratio).round() as i32).clamp(1, w);
        ((w - cw) / 2, 0, cw, h)
    }
}

/// Where thumbnails are kept.
pub fn cache_dir() -> PathBuf {
    super::attachments::full_cache_dir().with_file_name("photo-thumbs")
}

/// FNV-1a: stable across builds (std's hasher isn't promised to be).
fn fnv(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        h ^= *b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// The cache file for a photo: changes when the photo does.
pub fn cache_file(dir: &Path, path: &Path, size: u64, modified: i64) -> PathBuf {
    let key = format!("{}\0{size}\0{modified}", path.to_string_lossy());
    dir.join(format!("{:016x}.jpg", fnv(key.as_bytes())))
}

/// Keep the newest `keep` thumbnails.
pub fn prune(dir: &Path, keep: usize) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    let mut files: Vec<(std::time::SystemTime, PathBuf)> =
        entries.flatten().filter_map(|e| Some((e.metadata().ok()?.modified().ok()?, e.path()))).collect();
    if files.len() <= keep {
        return;
    }
    files.sort_by_key(|f| std::cmp::Reverse(f.0));
    for (_, p) in files.into_iter().skip(keep) {
        let _ = std::fs::remove_file(p);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A JPEG start with an EXIF block: orientation, photo size, and a
    /// preview of `thumb` bytes.
    fn jpeg(big: bool, orientation: u16, size: (u32, u32), thumb: &[u8]) -> Vec<u8> {
        let w16 = |v: u16| if big { v.to_be_bytes().to_vec() } else { v.to_le_bytes().to_vec() };
        let w32 = |v: u32| if big { v.to_be_bytes().to_vec() } else { v.to_le_bytes().to_vec() };
        let entry = |tag: u16, kind: u16, v: u32| {
            let mut e = w16(tag);
            e.extend(w16(kind));
            e.extend(w32(1));
            if kind == 3 {
                e.extend(w16(v as u16));
                e.extend([0, 0]);
            } else {
                e.extend(w32(v));
            }
            e
        };
        // TIFF: header (8), IFD0 at 8 with 2 entries (2+24+4 = 30) -> 38,
        // Exif IFD at 38 with 2 entries (30) -> 68, IFD1 at 68 (30) -> 98, thumb at 98.
        let mut t = if big { b"MM".to_vec() } else { b"II".to_vec() };
        t.extend(w16(42));
        t.extend(w32(8));
        t.extend(w16(2));
        t.extend(entry(0x0112, 3, orientation as u32));
        t.extend(entry(0x8769, 4, 38));
        t.extend(w32(68));
        t.extend(w16(2));
        t.extend(entry(0xa002, 4, size.0));
        t.extend(entry(0xa003, 4, size.1));
        t.extend(w32(0));
        t.extend(w16(2));
        t.extend(entry(0x0201, 4, 98));
        t.extend(entry(0x0202, 4, thumb.len() as u32));
        t.extend(w32(0));
        assert_eq!(t.len(), 98);
        t.extend(thumb);
        let mut app1 = b"Exif\0\0".to_vec();
        app1.extend(t);
        let mut out = vec![0xff, 0xd8, 0xff, 0xe0, 0, 4, 0, 0];
        out.extend([0xff, 0xe1]);
        out.extend(((app1.len() + 2) as u16).to_be_bytes());
        out.extend(app1);
        out.extend([0xff, 0xda]);
        out
    }

    #[test]
    fn finds_the_preview_in_either_byte_order() {
        for big in [false, true] {
            let thumb = [0xff, 0xd8, 1, 2, 3, 0xff, 0xd9];
            let file = jpeg(big, 6, (4000, 3000), &thumb);
            let p = exif_preview(&file).unwrap();
            assert_eq!(&file[p.start..p.end()], &thumb);
            assert_eq!((p.orientation, p.width, p.height), (6, Some(4000), Some(3000)));
        }
    }

    #[test]
    fn no_preview_means_none() {
        assert_eq!(exif_preview(b"\x89PNG\r\n"), None);
        assert_eq!(exif_preview(&[0xff, 0xd8, 0xff, 0xda, 0, 2]), None);
        assert_eq!(exif_preview(&[]), None);
        // Cut short inside the EXIF block.
        let file = jpeg(false, 1, (4, 3), &[1, 2, 3]);
        assert_eq!(exif_preview(&file[..40]), None);
    }

    #[test]
    fn padded_previews_are_cropped_to_the_photo() {
        // 16:9 photo in a 4:3 preview: bars above and below.
        assert_eq!(preview_content(512, 384, Some((4000, 2250))), (0, 48, 512, 288));
        // Same shape: all of it.
        assert_eq!(preview_content(512, 384, Some((4000, 3000))), (0, 0, 512, 384));
        assert_eq!(preview_content(512, 384, None), (0, 0, 512, 384));
        // Square photo: bars at the sides.
        assert_eq!(preview_content(512, 384, Some((3000, 3000))), (64, 0, 384, 384));
    }

    #[test]
    fn cache_files_change_with_the_photo() {
        let d = Path::new("/c");
        let a = cache_file(d, Path::new("/p/a.jpg"), 10, 5);
        assert_eq!(a, cache_file(d, Path::new("/p/a.jpg"), 10, 5));
        assert_ne!(a, cache_file(d, Path::new("/p/a.jpg"), 11, 5));
        assert_ne!(a, cache_file(d, Path::new("/p/a.jpg"), 10, 6));
        assert_ne!(a, cache_file(d, Path::new("/p/b.jpg"), 10, 5));
    }

    #[test]
    fn prune_keeps_the_newest() {
        let t = tempfile::tempdir().unwrap();
        for (i, name) in ["a", "b", "c"].iter().enumerate() {
            let p = t.path().join(name);
            std::fs::write(&p, b"x").unwrap();
            let when = std::time::SystemTime::now() - std::time::Duration::from_secs(100 * (3 - i as u64));
            std::fs::File::options().write(true).open(&p).unwrap().set_modified(when).unwrap();
        }
        prune(t.path(), 2);
        let mut left: Vec<_> = std::fs::read_dir(t.path()).unwrap().flatten().map(|e| e.file_name()).collect();
        left.sort();
        assert_eq!(left, ["b", "c"]);
    }
}
