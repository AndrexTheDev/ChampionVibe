use serde::Serialize;

/// Advertised encoder support used by the Tauri IPC and recording UI.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RecordingFormatInfo {
    pub id: &'static str,
    pub label: &'static str,
    pub extension: &'static str,
    pub available: bool,
    pub bitrate_kbps: Option<u16>,
    pub reason: Option<&'static str>,
}

pub(crate) fn formats() -> Vec<RecordingFormatInfo> {
    vec![
        RecordingFormatInfo {
            id: "wavPcm16",
            label: "WAV PCM 16-bit",
            extension: "wav",
            available: true,
            bitrate_kbps: None,
            reason: None,
        },
        RecordingFormatInfo {
            id: "wavPcm24",
            label: "WAV PCM 24-bit",
            extension: "wav",
            available: true,
            bitrate_kbps: None,
            reason: None,
        },
        RecordingFormatInfo {
            id: "flac",
            label: "FLAC",
            extension: "flac",
            available: true,
            bitrate_kbps: None,
            reason: None,
        },
        RecordingFormatInfo {
            id: "mp3",
            label: "MP3 320 kbps",
            extension: "mp3",
            available: true,
            bitrate_kbps: Some(320),
            reason: None,
        },
        RecordingFormatInfo {
            id: "oggVorbis",
            label: "OGG Vorbis · mobile VBR",
            extension: "ogg",
            available: true,
            bitrate_kbps: None,
            reason: None,
        },
        RecordingFormatInfo {
            id: "m4aAac",
            label: "M4A/AAC · 160 kbps",
            extension: "m4a",
            available: true,
            bitrate_kbps: Some(160),
            reason: None,
        },
    ]
}
