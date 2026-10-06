use memmap2::{Mmap, MmapOptions};
use std::{
    fs::{self, File, OpenOptions},
    io::{self, Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};
use symphonia::core::io::MediaSource;

/// Container signatures used for cheap folder discovery and the mmap policy. The actual decoder
/// is still selected by Symphonia's content probe, never by the filename extension.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AudioContainer {
    Wave,
    Aiff,
    Aifc,
    Flac,
    Ogg,
    MpegAudio,
    AacAdts,
    IsoMp4,
    WebM,
    Asf,
    Caf,
}

const MAX_MMAP_SNAPSHOT_BYTES: u64 = 256 * 1024 * 1024;

impl AudioContainer {
    fn is_mmap_candidate(self) -> bool {
        matches!(self, Self::Wave | Self::Aiff | Self::Flac)
    }
}

/// Inspect only a small header; this is used while enumerating folders so that discovering a
/// 50k-track library does not decode files or retain audio data in memory.
pub(crate) fn classify_audio_header(bytes: &[u8]) -> Option<AudioContainer> {
    if bytes.starts_with(b"fLaC") {
        return Some(AudioContainer::Flac);
    }
    if bytes.starts_with(b"OggS") {
        return Some(AudioContainer::Ogg);
    }
    if bytes.starts_with(b"caff") {
        return Some(AudioContainer::Caf);
    }
    if bytes.starts_with(&[0x1a, 0x45, 0xdf, 0xa3]) {
        return Some(AudioContainer::WebM);
    }
    if bytes.starts_with(&[0x30, 0x26, 0xb2, 0x75, 0x8e, 0x66, 0xcf, 0x11]) {
        return Some(AudioContainer::Asf);
    }
    if bytes.len() >= 12 {
        let riff = &bytes[..4] == b"RIFF" || &bytes[..4] == b"RIFX";
        if riff && &bytes[8..12] == b"WAVE" {
            return Some(AudioContainer::Wave);
        }
        if &bytes[..4] == b"FORM" && &bytes[8..12] == b"AIFF" {
            return Some(AudioContainer::Aiff);
        }
        if &bytes[..4] == b"FORM" && &bytes[8..12] == b"AIFC" {
            return Some(AudioContainer::Aifc);
        }
    }
    if bytes.len() >= 8 && &bytes[4..8] == b"ftyp" {
        return Some(AudioContainer::IsoMp4);
    }
    if bytes.starts_with(b"ID3") {
        return Some(AudioContainer::MpegAudio);
    }
    if bytes.len() >= 2 && bytes[0] == 0xff && bytes[1] & 0xe0 == 0xe0 {
        // MPEG audio has a non-zero layer field; ADTS AAC reserves those two bits as zero.
        return Some(if bytes[1] & 0x06 == 0 {
            AudioContainer::AacAdts
        } else {
            AudioContainer::MpegAudio
        });
    }
    None
}

pub(crate) fn looks_like_audio_file(path: &Path) -> bool {
    let Ok(mut file) = File::open(path) else {
        return false;
    };
    let mut header = [0_u8; 16];
    let Ok(read) = file.read(&mut header) else {
        return false;
    };
    classify_audio_header(&header[..read]).is_some()
}

/// Open a local media source. WAV/AIFF/FLAC inputs are first copied to a private, temporary
/// snapshot and then read through a file-backed mapping. Snapshotting makes the mapping immutable
/// even if the user's source file changes during decoding. Compressed containers use Symphonia's
/// bounded buffered stream reader. If mmap is unavailable, decoding falls back to the source file.
pub(crate) fn open_media_source(path: &Path) -> io::Result<Box<dyn MediaSource>> {
    let mut file = File::open(path)?;
    let mut header = [0_u8; 16];
    let read = file.read(&mut header)?;
    file.seek(SeekFrom::Start(0))?;
    let container = classify_audio_header(&header[..read]);
    if container == Some(AudioContainer::Asf) {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "Windows Media Audio/ASF is detected but no ASF/WMA decoder is enabled",
        ));
    }
    let length = file.metadata()?.len();

    if container.is_some_and(AudioContainer::is_mmap_candidate)
        && length > 0
        && length <= MAX_MMAP_SNAPSHOT_BYTES
    {
        if let Ok(snapshot) = TemporarySnapshot::copy_from(&mut file, length) {
            // SAFETY: this maps a private, create_new snapshot owned only by this decoder. The
            // application does not expose or mutate the snapshot file while it is mapped. The
            // file and its unique path stay alive with the map and are removed after unmapping.
            if let Ok(mapping) = unsafe {
                MmapOptions::new().map_copy_read_only(
                    snapshot.file.as_ref().expect("snapshot file stays open while mapped"),
                )
            } {
                return Ok(Box::new(MappedMediaSource {
                    mapping,
                    _snapshot: snapshot,
                    position: 0,
                }));
            }
        }
        file.seek(SeekFrom::Start(0))?;
    }

    Ok(Box::new(file))
}

static NEXT_SNAPSHOT_ID: AtomicU64 = AtomicU64::new(0);

struct TemporarySnapshot {
    file: Option<File>,
    path: PathBuf,
}

impl TemporarySnapshot {
    fn copy_from(source: &mut File, expected_len: u64) -> io::Result<Self> {
        let directory = std::env::temp_dir();
        for _ in 0..100 {
            let id = NEXT_SNAPSHOT_ID.fetch_add(1, Ordering::Relaxed);
            let path = directory.join(format!(
                "championvibe-audio-{}-{id}.snapshot",
                std::process::id()
            ));
            let mut options = OpenOptions::new();
            options.read(true).write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            #[cfg(windows)]
            {
                use std::os::windows::fs::OpenOptionsExt;
                // Permit readers, but deny other handles write/delete access until the snapshot
                // mapping is gone. The private owner removes it after closing this handle.
                options.share_mode(0x0000_0001);
            }
            let mut file = match options.open(&path) {
                Ok(file) => file,
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error),
            };
            let result = (|| {
                source.seek(SeekFrom::Start(0))?;
                let copied = io::copy(source, &mut file)?;
                if copied != expected_len || file.metadata()?.len() != expected_len {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "audio source changed while creating its mmap snapshot",
                    ));
                }
                file.sync_all()?;
                source.seek(SeekFrom::Start(0))?;
                Ok(())
            })();
            if let Err(error) = result {
                drop(file);
                let _ = fs::remove_file(&path);
                return Err(error);
            }
            return Ok(Self {
                file: Some(file),
                path,
            });
        }
        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "could not allocate a unique audio mmap snapshot",
        ))
    }
}

impl Drop for TemporarySnapshot {
    fn drop(&mut self) {
        // On Windows the snapshot handle denies delete sharing. Release it before unlinking.
        drop(self.file.take());
        let _ = fs::remove_file(&self.path);
    }
}

struct MappedMediaSource {
    mapping: Mmap,
    _snapshot: TemporarySnapshot,
    position: u64,
}

impl Read for MappedMediaSource {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        let Ok(start) = usize::try_from(self.position) else {
            return Ok(0);
        };
        if start >= self.mapping.len() || output.is_empty() {
            return Ok(0);
        }
        let count = output.len().min(self.mapping.len() - start);
        output[..count].copy_from_slice(&self.mapping[start..start + count]);
        self.position = self.position.saturating_add(count as u64);
        Ok(count)
    }
}

impl Seek for MappedMediaSource {
    fn seek(&mut self, seek_from: SeekFrom) -> io::Result<u64> {
        let end = self.mapping.len() as i128;
        let current = self.position as i128;
        let target = match seek_from {
            SeekFrom::Start(position) => position as i128,
            SeekFrom::Current(delta) => current + i128::from(delta),
            SeekFrom::End(delta) => end + i128::from(delta),
        };
        if target < 0 || target > i128::from(u64::MAX) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "seek position is outside the supported range",
            ));
        }
        self.position = target as u64;
        Ok(self.position)
    }
}

impl MediaSource for MappedMediaSource {
    fn is_seekable(&self) -> bool {
        true
    }

    fn byte_len(&self) -> Option<u64> {
        Some(self.mapping.len() as u64)
    }
}

#[cfg(test)]
mod tests {
    use super::{classify_audio_header, AudioContainer};

    #[test]
    fn recognizes_audio_by_signature_not_filename() {
        for (header, expected) in [
            (b"fLaC\x00\x00\x00\x22".as_slice(), AudioContainer::Flac),
            (b"OggS\x00\x02".as_slice(), AudioContainer::Ogg),
            (b"RIFF\x00\x00\x00\x00WAVE".as_slice(), AudioContainer::Wave),
            (b"FORM\x00\x00\x00\x00AIFF".as_slice(), AudioContainer::Aiff),
            (b"FORM\x00\x00\x00\x00AIFC".as_slice(), AudioContainer::Aifc),
            (b"\x1a\x45\xdf\xa3\x93".as_slice(), AudioContainer::WebM),
            (b"\x00\x00\x00\x18ftypM4A ".as_slice(), AudioContainer::IsoMp4),
            (b"ID3\x04\x00\x00".as_slice(), AudioContainer::MpegAudio),
            (b"\xff\xf1\x50\x80\x00\x1f\xfc".as_slice(), AudioContainer::AacAdts),
        ] {
            assert_eq!(classify_audio_header(header), Some(expected));
        }
        assert!(!AudioContainer::Aifc.is_mmap_candidate());
    }

    #[test]
    fn rejects_non_audio_content_even_when_the_extension_might_look_valid() {
        assert_eq!(classify_audio_header(b"\x89PNG\r\n\x1a\n"), None);
        assert_eq!(classify_audio_header(b"plain text"), None);
    }

    #[test]
    fn reports_asf_wma_as_recognized_but_unsupported() {
        let id = super::NEXT_SNAPSHOT_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "championvibe-asf-test-{}-{id}.asf",
            std::process::id()
        ));
        std::fs::write(
            &path,
            [0x30, 0x26, 0xb2, 0x75, 0x8e, 0x66, 0xcf, 0x11, 0, 0, 0, 0],
        )
        .unwrap();
        let error = super::open_media_source(&path).err().unwrap();
        let _ = std::fs::remove_file(path);
        assert_eq!(error.kind(), std::io::ErrorKind::Unsupported);
        assert!(error.to_string().contains("no ASF/WMA decoder"));
    }

    #[test]
    fn snapshot_is_unchanged_by_source_edits_and_is_removed_after_drop() {
        let id = super::NEXT_SNAPSHOT_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let source_path = std::env::temp_dir().join(format!(
            "championvibe-source-test-{}-{id}.bin",
            std::process::id()
        ));
        let original = b"immutable source snapshot";
        std::fs::write(&source_path, original).unwrap();
        let mut source = std::fs::File::open(&source_path).unwrap();
        let snapshot = super::TemporarySnapshot::copy_from(&mut source, original.len() as u64)
            .unwrap();
        let snapshot_path = snapshot.path.clone();

        std::fs::write(&source_path, b"mutated source snapshot").unwrap();
        assert_eq!(std::fs::read(&snapshot_path).unwrap(), original.to_vec());
        drop(snapshot);
        drop(source);

        assert!(!snapshot_path.exists());
        let _ = std::fs::remove_file(source_path);
    }
}
