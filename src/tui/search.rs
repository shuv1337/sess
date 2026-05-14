use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::thread;

use crate::search::index::TantivyIndex;
use crate::search::{SearchQuery, SearchResults};

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
}

impl SearchThread {
    pub fn new(index: Arc<TantivyIndex>) -> Self {
        let (req_sender, req_receiver): (Sender<SearchRequest>, Receiver<SearchRequest>) =
            channel();
        let (resp_sender, resp_receiver): (Sender<SearchResponse>, Receiver<SearchResponse>) =
            channel();

        thread::spawn(move || {
            while let Ok(request) = req_receiver.recv() {
                // Execute search
                let results =
                    crate::search::query::execute(&request.query, &index).unwrap_or_else(|e| {
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
        }
    }

    pub fn search(&self, query: SearchQuery, generation: u64) {
        let _ = self.sender.send(SearchRequest { query, generation });
    }

    pub fn try_recv(&self) -> Option<SearchResponse> {
        self.receiver.try_recv().ok()
    }
}
