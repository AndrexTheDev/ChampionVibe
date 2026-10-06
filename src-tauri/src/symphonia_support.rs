use std::sync::OnceLock;
use symphonia::core::codecs::registry::CodecRegistry;

/// Build the same codec registry as Symphonia's default helper, plus its external Opus decoder.
/// Construct it once: probing/decoding tens of thousands of tracks must not rebuild registries.
pub(crate) fn codec_registry() -> &'static CodecRegistry {
    static REGISTRY: OnceLock<CodecRegistry> = OnceLock::new();
    REGISTRY.get_or_init(|| {
        let mut registry = CodecRegistry::new();
        symphonia::default::register_enabled_codecs(&mut registry);
        registry.register_audio_decoder::<symphonia_adapter_libopus::OpusDecoder>();
        registry
    })
}
