use std::sync::mpsc::{channel, Sender, Receiver};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;

use anyhow::Result;

use crate::search::{SearchQuery, SearchResults, RankingMode};
use crate::search::index::TantivyIndex;

/// Request for background search
pub struct SearchRequest {
    pub query: SearchQuery,
    pub generation: u64,
}

/// Response from background search
pub struct SearchResponse {
    pub results: SearchResults,
    pub generation: u64,
}

/// Background search thread handle
pub struct SearchThread {
    sender: Sender<SearchRequest>,
    receiver: Receiver<SearchResponse>,
    generation: Arc<AtomicU64>,
}

impl SearchThread {
    pub fn new(index: Arc<TantivyIndex>) -> Self {
        let (req_sender, req_receiver): (Sender<SearchRequest>, Receiver<SearchRequest>) = channel();
        let (resp_sender, resp_receiver): (Sender<SearchResponse>, Receiver<SearchResponse>) = channel();
        let generation = Arc::new(AtomicU64::new(0));
        let gen_clone = generation.clone();

        thread::spawn(move || {
            while let Ok(request) = req_receiver.recv() {
                let start = std::time::Instant::now();

                // Execute search
                let results = crate::search::query::execute(&request.query,
                    &index
                ).unwrap_or_else(|e| {
                    tracing::error!("Search error: {}", e);
                    SearchResults {
                        hits: vec![],
                        total_hits: 0,
                        query_time_ms: 0,
                    }
                });

                // Send response (ignore errors if receiver dropped)
                let _ = resp_sender.send(SearchResponse {
                    results,
                    generation: request.generation,
                });
            }
        });

        Self {
            sender: req_sender,
            receiver: resp_receiver,
            generation: gen_clone,
        }
    }

    pub fn search(&self, query: SearchQuery) -> u64 {
        let generation = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
        let _ = self.sender.send(SearchRequest { query, generation });
        generation
    }

    pub fn try_recv(&self) -> Option<SearchResponse> {
        self.receiver.try_recv().ok()
    }

    pub fn current_generation(&self) -> u64 {
        self.generation.load(Ordering::SeqCst)
    }
}
