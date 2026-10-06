use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LocalizedMessage {
    pub code: String,
    pub de: String,
    pub en: String,
    pub es: String,
    pub fr: String,
}

impl LocalizedMessage {
    fn new(code: &str, de: &str, en: &str, es: &str, fr: &str) -> Self {
        Self {
            code: code.to_owned(),
            de: de.to_owned(),
            en: en.to_owned(),
            es: es.to_owned(),
            fr: fr.to_owned(),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LocalizationCatalog {
    pub languages: [&'static str; 4],
    pub metadata_labels: Vec<LocalizedMessage>,
    pub import_statuses: Vec<LocalizedMessage>,
    pub import_stages: Vec<LocalizedMessage>,
    pub recording_statuses: Vec<LocalizedMessage>,
}

pub fn import_status(status: &str) -> LocalizedMessage {
    match status {
        "queued" => LocalizedMessage::new(
            "import.queued", "In Warteschlange", "Queued", "En cola", "En attente",
        ),
        "running" => LocalizedMessage::new(
            "import.running", "Wird verarbeitet", "Processing", "Procesando", "En cours",
        ),
        "completed" => LocalizedMessage::new(
            "import.completed", "Abgeschlossen", "Completed", "Completado", "Terminé",
        ),
        "failed" => LocalizedMessage::new(
            "import.failed", "Fehlgeschlagen", "Failed", "Fallido", "Échec",
        ),
        "cancelled" => LocalizedMessage::new(
            "import.cancelled", "Abgebrochen", "Cancelled", "Cancelado", "Annulé",
        ),
        _ => LocalizedMessage::new(
            "import.unknown", "Unbekannter Status", "Unknown status", "Estado desconocido", "État inconnu",
        ),
    }
}

pub fn import_stage(stage: &str) -> LocalizedMessage {
    if let Some(suffix) = stage.strip_prefix("separating-") {
        let count = suffix.split('-').next().unwrap_or_default();
        if matches!(count, "2" | "4") {
            let backend = if stage.ends_with("DirectML") {
                " (DirectML)"
            } else if stage.ends_with("CPU") {
                " (CPU)"
            } else {
                ""
            };
            return LocalizedMessage::new(
                &format!("import.separating_{count}_stems"),
                &format!("{count} Stems trennen{backend}"),
                &format!("Separating {count} stems{backend}"),
                &format!("Separando {count} pistas{backend}"),
                &format!("Séparation de {count} pistes{backend}"),
            );
        }
    }

    match stage {
        "starting" => LocalizedMessage::new(
            "import.starting", "Warteschlange wird vorbereitet", "Preparing queue", "Preparando la cola", "Préparation de la file",
        ),
        "loading-local-models" => LocalizedMessage::new(
            "import.loading_local_models", "Lokale Modelle laden", "Loading local models", "Cargando modelos locales", "Chargement des modèles locaux",
        ),
        "decoding-source" => LocalizedMessage::new(
            "import.decoding_source", "Audio lokal dekodieren", "Decoding local audio", "Decodificando audio local", "Décodage audio local",
        ),
        "analyzing-bpm-key-energy" => LocalizedMessage::new(
            "import.analyzing_audio", "BPM, Tonart, Lautheit, Stimmung und Rhythmus analysieren", "Analyzing BPM, key, loudness, mood, and rhythm", "Analizando BPM, tonalidad, sonoridad, ánimo y ritmo", "Analyse du BPM, de la tonalité, du volume, de l'ambiance et du rythme",
        ),
        "preparing-acapella-for-whisper" => LocalizedMessage::new(
            "import.preparing_acapella", "Acapella für Whisper vorbereiten", "Preparing acapella for Whisper", "Preparando la acapella para Whisper", "Préparation de l'acapella pour Whisper",
        ),
        "transcribing-lyrics-locally" => LocalizedMessage::new(
            "import.transcribing_lyrics", "Lyrics lokal transkribieren", "Transcribing lyrics locally", "Transcribiendo letras localmente", "Transcription locale des paroles",
        ),
        "committing-track-metadata" | "committing" => LocalizedMessage::new(
            "import.committing_metadata", "Metadaten lokal speichern", "Saving metadata locally", "Guardando metadatos localmente", "Enregistrement local des métadonnées",
        ),
        _ => LocalizedMessage::new(
            "import.unknown_stage", "Lokale Verarbeitung", "Local processing", "Procesamiento local", "Traitement local",
        ),
    }
}

pub fn recording_status(code: &str) -> LocalizedMessage {
    match code {
        "recording.ready" => LocalizedMessage::new(
            "recording.ready", "Aufnahme bereit", "Recording ready", "Grabación lista", "Enregistrement prêt",
        ),
        "recording.started" => LocalizedMessage::new(
            "recording.started", "Aufnahme läuft", "Recording", "Grabando", "Enregistrement en cours",
        ),
        "recording.saved" => LocalizedMessage::new(
            "recording.saved", "Aufnahme gespeichert", "Recording saved", "Grabación guardada", "Enregistrement sauvegardé",
        ),
        "recording.license_expired" => LocalizedMessage::new(
            "recording.license_expired", "Pro-Lizenz abgelaufen; unvollständige Aufnahme gelöscht.", "Pro license expired; incomplete recording deleted.", "La licencia Pro ha caducado; se eliminó la grabación incompleta.", "La licence Pro a expiré ; l'enregistrement incomplet a été supprimé.",
        ),
        "recording.capture_overflow" => LocalizedMessage::new(
            "recording.capture_overflow", "Aufnahmepuffer übergelaufen; unvollständige Aufnahme gelöscht.", "Capture buffer overflowed; incomplete recording deleted.", "El búfer de captura se desbordó; se eliminó la grabación incompleta.", "Le tampon de capture a débordé ; l'enregistrement incomplet a été supprimé.",
        ),
        "recording.failed" => LocalizedMessage::new(
            "recording.failed", "Aufnahme fehlgeschlagen", "Recording failed", "Error en la grabación", "Échec de l'enregistrement",
        ),
        _ => LocalizedMessage::new(
            "recording.unknown", "Unbekannter Aufnahmestatus", "Unknown recording status", "Estado de grabación desconocido", "État d'enregistrement inconnu",
        ),
    }
}

pub fn catalog() -> LocalizationCatalog {
    let import_statuses = ["queued", "running", "completed", "failed", "cancelled"]
        .into_iter()
        .map(import_status)
        .collect();
    let import_stages = [
        "starting",
        "loading-local-models",
        "decoding-source",
        "analyzing-bpm-key-energy",
        "separating-2-stems-CPU",
        "separating-2-stems-DirectML",
        "separating-4-stems-CPU",
        "separating-4-stems-DirectML",
        "preparing-acapella-for-whisper",
        "transcribing-lyrics-locally",
        "committing-track-metadata",
    ]
    .into_iter()
    .map(import_stage)
    .collect();
    let recording_statuses = [
        "recording.ready",
        "recording.started",
        "recording.saved",
        "recording.license_expired",
        "recording.capture_overflow",
        "recording.failed",
    ]
    .into_iter()
    .map(recording_status)
    .collect();
    let metadata_labels = [
        ("metadata.title", "Titel", "Title", "Título", "Titre"),
        ("metadata.artist", "Interpret", "Artist", "Artista", "Artiste"),
        ("metadata.duration", "Dauer", "Duration", "Duración", "Durée"),
        ("metadata.codec", "Format", "Format", "Formato", "Format"),
        ("metadata.bpm", "BPM", "BPM", "BPM", "BPM"),
        ("metadata.key", "Tonart", "Key", "Tonalidad", "Tonalité"),
        ("metadata.lufs", "Integrierte Lautheit", "Integrated loudness", "Sonoridad integrada", "Loudness intégré"),
        ("metadata.mood", "Stimmung", "Mood", "Ánimo", "Ambiance"),
        ("metadata.rhythm", "Rhythmus", "Rhythm", "Ritmo", "Rythme"),
    ]
    .into_iter()
    .map(|(code, de, en, es, fr)| LocalizedMessage::new(code, de, en, es, fr))
    .collect();
    LocalizationCatalog {
        languages: ["de", "en", "es", "fr"],
        metadata_labels,
        import_statuses,
        import_stages,
        recording_statuses,
    }
}

#[cfg(test)]
mod tests {
    use super::{catalog, import_stage, import_status, recording_status};

    #[test]
    fn ipc_localization_covers_required_languages_and_import_stages() {
        let queued = import_status("queued");
        assert_eq!(queued.code, "import.queued");
        assert_eq!(queued.de, "In Warteschlange");
        assert_eq!(queued.en, "Queued");
        assert_eq!(queued.es, "En cola");
        assert_eq!(queued.fr, "En attente");

        let stage = import_stage("separating-4-stems-DirectML");
        assert!(stage.de.contains("DirectML"));
        assert!(stage.en.starts_with("Separating 4 stems"));
        let two_stem_stage = import_stage("separating-2-stems-CPU");
        assert!(two_stem_stage.en.starts_with("Separating 2 stems"));

        let messages = catalog();
        assert_eq!(messages.languages, ["de", "en", "es", "fr"]);
        let metadata_fields = messages
            .metadata_labels
            .iter()
            .map(|message| message.code.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            metadata_fields,
            vec![
                "metadata.title",
                "metadata.artist",
                "metadata.duration",
                "metadata.codec",
                "metadata.bpm",
                "metadata.key",
                "metadata.lufs",
                "metadata.mood",
                "metadata.rhythm",
            ]
        );
        assert!(!messages.recording_statuses.is_empty());
        let expired = recording_status("recording.license_expired");
        assert_eq!(expired.code, "recording.license_expired");
    }
}
