use anyhow::{Context, Result};
use fastembed::{TextEmbedding, EmbeddingModel, InitOptions};

/// Semantic index using FastEmbed
pub struct SemanticIndex {
    embedder: TextEmbedding,
    dimension: usize,
}

impl SemanticIndex {
    /// Create a new semantic index with the default model (AllMiniLML6V2, 384-dim)
    pub fn new() -> Result<Self> {
        let options = InitOptions::new(EmbeddingModel::AllMiniLML6V2)
            .with_show_download_progress(true);

        let model = TextEmbedding::try_new(options)
            .context("Failed to initialize FastEmbed. Use --no-semantic to skip.")?;

        Ok(Self {
            embedder: model,
            dimension: 384,
        })
    }

    /// Generate embedding for text
    pub fn embed(&self, text: &str) -> Result<Vec<f32>> {
        let truncated = if text.len() > 1000 {
            &text[..1000]
        } else {
            text
        };

        let embeddings = self.embedder
            .embed(vec![truncated], None)
            .context("Failed to generate embedding")?;

        embeddings.into_iter().next()
            .context("No embedding generated")
    }

    /// Compute cosine similarity between two vectors
    pub fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
        if a.len() != b.len() {
            return 0.0;
        }
        let dot: f32 = a.iter().zip(b.iter()).map(|(x, y)| x * y).sum();
        let na: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
        let nb: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
        if na == 0.0 || nb == 0.0 { 0.0 } else { dot / (na * nb) }
    }

    /// Search for similar conversations using brute-force cosine similarity
    pub fn search(
        &self,
        query: &str,
        vectors: &[(i64, Vec<f32>)],
        limit: usize,
    ) -> Result<Vec<(i64, f32)>> {
        let query_embedding = self.embed(query)?;

        let mut similarities: Vec<(i64, f32)> = vectors
            .iter()
            .map(|(id, vec)| (*id, Self::cosine_similarity(&query_embedding, vec)))
            .collect();

        similarities.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());
        Ok(similarities.into_iter().take(limit).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cosine_similarity_identical() {
        let a = vec![1.0, 0.0, 0.0];
        let b = vec![1.0, 0.0, 0.0];
        let sim = SemanticIndex::cosine_similarity(&a, &b);
        assert!((sim - 1.0).abs() < 0.001);
    }

    #[test]
    fn test_cosine_similarity_orthogonal() {
        let a = vec![1.0, 0.0, 0.0];
        let b = vec![0.0, 1.0, 0.0];
        let sim = SemanticIndex::cosine_similarity(&a, &b);
        assert!(sim.abs() < 0.001);
    }

    #[test]
    fn test_cosine_similarity_opposite() {
        let a = vec![1.0, 0.0, 0.0];
        let b = vec![-1.0, 0.0, 0.0];
        let sim = SemanticIndex::cosine_similarity(&a, &b);
        assert!((sim - (-1.0)).abs() < 0.001);
    }

    #[test]
    fn test_cosine_similarity_similar_vectors() {
        let a = vec![1.0, 0.8, 0.3];
        let b = vec![0.9, 0.7, 0.4];
        let sim = SemanticIndex::cosine_similarity(&a, &b);
        assert!(sim > 0.9); // Very similar
    }

    #[test]
    fn test_cosine_similarity_zero_vector() {
        let a = vec![1.0, 0.0, 0.0];
        let b = vec![0.0, 0.0, 0.0];
        let sim = SemanticIndex::cosine_similarity(&a, &b);
        assert_eq!(sim, 0.0);
    }

    #[test]
    fn test_cosine_similarity_both_zero() {
        let a = vec![0.0, 0.0];
        let b = vec![0.0, 0.0];
        let sim = SemanticIndex::cosine_similarity(&a, &b);
        assert_eq!(sim, 0.0);
    }

    #[test]
    fn test_cosine_similarity_different_lengths() {
        let a = vec![1.0, 0.0];
        let b = vec![1.0, 0.0, 0.0];
        let sim = SemanticIndex::cosine_similarity(&a, &b);
        assert_eq!(sim, 0.0); // Mismatched lengths → 0
    }

    #[test]
    fn test_cosine_similarity_single_dimension() {
        let a = vec![5.0];
        let b = vec![3.0];
        let sim = SemanticIndex::cosine_similarity(&a, &b);
        assert!((sim - 1.0).abs() < 0.001); // Same direction, just different magnitude
    }

    #[test]
    fn test_cosine_similarity_negative() {
        let a = vec![-1.0, -2.0];
        let b = vec![-1.0, -2.0];
        let sim = SemanticIndex::cosine_similarity(&a, &b);
        assert!((sim - 1.0).abs() < 0.001); // Same direction
    }
}
