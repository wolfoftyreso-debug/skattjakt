//! The reader itself: pixels in, words with boxes out.
//!
//! Everything that decides *meaning* lives in [`crate::layout`], which has no
//! dependency on this module. That split is deliberate. The engine needs
//! eleven megabytes of model files to do anything at all, so a test of it can
//! only run where those files are; the rules for turning words into statement
//! rows must be testable everywhere, because that is where a quiet mistake
//! would cost a customer a wrong figure.

use std::path::{Path, PathBuf};

use ocrs::{ImageSource, OcrEngine, OcrEngineParams};
use rten::Model;
use rten_imageproc::BoundingRect;

use crate::layout::{self, Row, Word};

#[derive(Debug, thiserror::Error)]
pub enum OcrError {
    /// The models are absent. Said plainly, with the path that was tried,
    /// because the alternative is a deployment that silently reads nothing
    /// and reports every scan as blank.
    #[error("no OCR models at {0}: reading images is switched off in this deployment")]
    ModelsMissing(PathBuf),
    #[error("loading the OCR model failed: {0}")]
    ModelLoad(String),
    #[error("preparing the OCR engine failed: {0}")]
    EngineInit(String),
    #[error("the bytes are not an image this reader understands: {0}")]
    NotAnImage(String),
    #[error("reading the image failed: {0}")]
    Recognition(String),
}

/// Where the two model files live.
///
/// Both are baked into the runtime image. Nothing is fetched at start-up: a
/// reader that downloads its models is a reader that stops working the day
/// the network is unavailable, and it would do so quietly.
#[derive(Debug, Clone)]
pub struct Models {
    pub detection: PathBuf,
    pub recognition: PathBuf,
}

impl Models {
    pub fn in_directory(dir: impl AsRef<Path>) -> Self {
        let dir = dir.as_ref();
        Self {
            detection: dir.join("text-detection.rten"),
            recognition: dir.join("text-recognition.rten"),
        }
    }

    /// The directory named by `SKATTJAKT_OCR_MODELS`, when it is set.
    ///
    /// Absent is a legitimate configuration and not an error here: a
    /// deployment that does not read scans is a smaller image. The caller
    /// decides what to say about it.
    pub fn from_env() -> Option<Self> {
        std::env::var("SKATTJAKT_OCR_MODELS")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .map(Self::in_directory)
    }

    pub fn present(&self) -> bool {
        self.detection.is_file() && self.recognition.is_file()
    }
}

/// A loaded reader. Loading costs seconds and eleven megabytes of resident
/// memory, so a process builds one and keeps it.
pub struct Reader {
    engine: OcrEngine,
}

impl Reader {
    pub fn load(models: &Models) -> Result<Self, OcrError> {
        if !models.present() {
            let dir = models
                .detection
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_default();
            return Err(OcrError::ModelsMissing(dir));
        }
        let detection = Model::load_file(&models.detection)
            .map_err(|e| OcrError::ModelLoad(format!("{}: {e}", models.detection.display())))?;
        let recognition = Model::load_file(&models.recognition)
            .map_err(|e| OcrError::ModelLoad(format!("{}: {e}", models.recognition.display())))?;
        let engine = OcrEngine::new(OcrEngineParams {
            detection_model: Some(detection),
            recognition_model: Some(recognition),
            ..Default::default()
        })
        .map_err(|e| OcrError::EngineInit(e.to_string()))?;
        Ok(Self { engine })
    }

    /// Read one page image and return the statement rows it holds.
    pub fn read_page(&self, bytes: &[u8]) -> Result<Vec<Row>, OcrError> {
        Ok(layout::rows_from_words(self.words(bytes)?, 0.6))
    }

    /// Every recognised word, with the box it occupied.
    ///
    /// ocrs groups a page into what it calls lines, but on a two-column
    /// statement those are columns: one run of labels, one run of amounts.
    /// The text of such a "line" is therefore split back into its words and
    /// paired with the boxes that produced them. When the counts disagree —
    /// the recogniser merged or split something — the whole run is kept as
    /// one word spanning the boxes, which is honest about what is known and
    /// lets the row builder do what it can with it.
    pub fn words(&self, bytes: &[u8]) -> Result<Vec<Word>, OcrError> {
        let image = image::load_from_memory(bytes)
            .map_err(|e| OcrError::NotAnImage(e.to_string()))?
            .into_rgb8();
        let source = ImageSource::from_bytes(image.as_raw(), image.dimensions())
            .map_err(|e| OcrError::NotAnImage(e.to_string()))?;
        let input = self
            .engine
            .prepare_input(source)
            .map_err(|e| OcrError::Recognition(e.to_string()))?;
        let boxes = self
            .engine
            .detect_words(&input)
            .map_err(|e| OcrError::Recognition(e.to_string()))?;
        let lines = self.engine.find_text_lines(&input, &boxes);
        let texts = self
            .engine
            .recognize_text(&input, &lines)
            .map_err(|e| OcrError::Recognition(e.to_string()))?;

        let mut words = Vec::new();
        for (line, text) in lines.iter().zip(texts.iter()) {
            let Some(text) = text else { continue };
            let rendered = text.to_string();
            let parts: Vec<&str> = rendered.split_whitespace().collect();
            if parts.len() == line.len() {
                for (word_box, part) in line.iter().zip(parts) {
                    let r = word_box.bounding_rect();
                    words.push(Word::new(part, r.left(), r.top(), r.right(), r.bottom()));
                }
            } else if let (Some(first), Some(last)) = (line.first(), line.last()) {
                let (a, b) = (first.bounding_rect(), last.bounding_rect());
                words.push(Word::new(
                    rendered.trim(),
                    a.left(),
                    a.top(),
                    b.right(),
                    b.bottom(),
                ));
            }
        }
        Ok(words)
    }
}
