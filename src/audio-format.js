const EXTENSION_FORMATS = Object.freeze({
  mp3: 'MP3',
  wav: 'WAV',
  wave: 'WAV',
  flac: 'FLAC',
  m4a: 'M4A',
  mp4: 'M4A',
  ogg: 'OGG',
  oga: 'OGG',
  opus: 'OGG',
  aac: 'AAC',
});

/**
 * Prefer the codec detected by the local decoder; file extensions are only a
 * fallback because users and download tools often mislabel audio files.
 */
export function getAudioFormat(track) {
  const filePath = String(track?.filePath || track?.file_path || '');
  const extension = filePath.split(/[\\/]/u).pop()?.split('.').pop()?.toLowerCase() || '';
  const codec = String(track?.codec || '').toLowerCase();

  if (codec.includes('mp3') || /mpeg.*layer.?3/u.test(codec)) return 'MP3';
  if (codec.includes('flac')) return 'FLAC';
  if (codec.includes('vorbis') || codec.includes('opus')) return 'OGG';
  if (codec.includes('alac')) return 'M4A';
  if (codec.includes('aac')) return extension === 'm4a' || extension === 'mp4' ? 'M4A' : 'AAC';
  if (codec.includes('pcm')) return extension === 'wav' || extension === 'wave' ? 'WAV' : 'PCM';
  if (codec) return 'AUDIO';

  return EXTENSION_FORMATS[extension] || 'AUDIO';
}
