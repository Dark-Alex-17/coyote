# Test Plan: RAG

## Behaviors to test
- [ ] Rag::init creates new RAG with embedding model
- [ ] Rag::load loads existing RAG from disk
- [ ] Rag::create builds vector store from documents
- [ ] Rag::refresh_document_paths updates document list
- [ ] RAG search returns relevant embeddings
- [ ] RAG template formats context + sources + input
- [ ] Reranker model applied when configured
- [ ] top_k controls number of results
- [ ] RAG sources tracked for .sources command
- [ ] exit_rag clears RAG from context

## Old code reference
- `src/rag/mod.rs` — Rag struct and methods
- `src/config/request_context.rs` — use_rag, edit_rag_docs, rebuild_rag
