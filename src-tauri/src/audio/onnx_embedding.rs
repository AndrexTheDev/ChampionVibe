//! Optional local ONNX projection for the 64-dimensional MIR descriptor.
//!
//! A compatible, locally provisioned model must accept float32 `[1, 64]` input and return a
//! float32 `[1, 64]` vector. Its weights/runtime remain on-device; the output is L2-normalized and
//! quantized to signed int8 before storage. No model is bundled, downloaded, or assumed to be
//! commercially cleared. When the optional model is absent or fails, import stores the transparent
//! deterministic MIR descriptor instead.

use anyhow::{Context, Result};
use ort::{session::Session, value::Tensor};
use std::path::Path;

const FEATURE_COUNT: usize = 64;

pub(crate) struct OnnxMirEmbedder {
    session: Session,
    input_name: String,
    output_name: String,
}

impl OnnxMirEmbedder {
    pub(crate) fn load(path: &Path) -> Result<Self> {
        anyhow::ensure!(path.is_file(), "local audio embedding model does not exist");
        let builder = Session::builder()
            .map_err(|error| anyhow::anyhow!("could not initialize ONNX Runtime: {error}"))?
            .with_intra_threads(1)
            .map_err(|error| anyhow::anyhow!("could not configure ONNX Runtime: {error}"))?
            .with_inter_threads(1)
            .map_err(|error| anyhow::anyhow!("could not configure ONNX Runtime: {error}"))?
            .with_parallel_execution(false)
            .map_err(|error| anyhow::anyhow!("could not configure ONNX Runtime: {error}"))?;
        let session = builder.commit_from_file(path).map_err(|error| {
            anyhow::anyhow!(
                "could not load local audio embedding model {}: {error}",
                path.display()
            )
        })?;
        let input_name = session
            .inputs()
            .first()
            .context("audio embedding ONNX model has no inputs")?
            .name()
            .to_owned();
        let output_name = session
            .outputs()
            .first()
            .context("audio embedding ONNX model has no outputs")?
            .name()
            .to_owned();
        anyhow::ensure!(
            !input_name.is_empty(),
            "audio embedding ONNX input name is empty"
        );
        anyhow::ensure!(
            !output_name.is_empty(),
            "audio embedding ONNX output name is empty"
        );
        Ok(Self {
            session,
            input_name,
            output_name,
        })
    }

    pub(crate) fn embed(
        &mut self,
        descriptor: &[i8; FEATURE_COUNT],
    ) -> Result<[i8; FEATURE_COUNT]> {
        let input = descriptor
            .iter()
            .map(|value| f32::from(*value) / 127.0)
            .collect::<Vec<_>>()
            .into_boxed_slice();
        let input_tensor = Tensor::from_array(([1_usize, FEATURE_COUNT], input))?;
        let outputs = self
            .session
            .run(ort::inputs![self.input_name.as_str() => input_tensor])
            .context("local audio embedding ONNX inference failed")?;
        let output = outputs
            .get(self.output_name.as_str())
            .context("audio embedding ONNX output is missing")?;
        let (shape, values) = output
            .try_extract_tensor::<f32>()
            .context("audio embedding ONNX output must be a float32 tensor")?;
        anyhow::ensure!(
            **shape == [1_i64, FEATURE_COUNT as i64],
            "audio embedding ONNX output must have shape [1, {FEATURE_COUNT}]"
        );
        quantize_embedding(values)
    }
}

fn quantize_embedding(values: &[f32]) -> Result<[i8; FEATURE_COUNT]> {
    anyhow::ensure!(
        values.len() == FEATURE_COUNT,
        "audio embedding ONNX output must contain exactly {FEATURE_COUNT} values"
    );
    anyhow::ensure!(
        values.iter().all(|value| value.is_finite()),
        "audio embedding ONNX output contains a non-finite value"
    );
    let norm = values
        .iter()
        .map(|value| f64::from(*value) * f64::from(*value))
        .sum::<f64>()
        .sqrt();
    let mut result = [0_i8; FEATURE_COUNT];
    if norm > 1.0e-12 {
        for (target, value) in result.iter_mut().zip(values) {
            *target = (f64::from(*value) / norm * 127.0)
                .round()
                .clamp(-127.0, 127.0) as i8;
        }
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::{quantize_embedding, FEATURE_COUNT};

    #[test]
    fn quantization_returns_a_bounded_unit_int8_vector() {
        let mut values = vec![0.0_f32; FEATURE_COUNT];
        values[0] = 3.0;
        values[1] = -4.0;
        let quantized = quantize_embedding(&values).unwrap();
        assert_eq!(quantized[0], 76);
        assert_eq!(quantized[1], -102);
        assert!(quantized.iter().all(|value| (-127..=127).contains(value)));
    }

    #[test]
    fn zero_embedding_stays_zero_and_bad_shapes_fail_closed() {
        assert_eq!(
            quantize_embedding(&vec![0.0; FEATURE_COUNT]).unwrap(),
            [0; FEATURE_COUNT]
        );
        assert!(quantize_embedding(&[1.0, 2.0]).is_err());
    }
}
