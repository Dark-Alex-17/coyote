# Test Plan: RAG

## Behaviors to test
- [ ] Rag::init creates new RAG with embedding model (requires LLM client)
- [ ] Rag::load loads existing RAG from disk (requires filesystem)
- [ ] Rag::create builds vector store from documents (requires embedding model)
- [ ] Rag::refresh_document_paths updates document list (requires filesystem)
- [ ] RAG search returns relevant embeddings (requires embedding model)
- [x] RAG template contains required placeholders
- [ ] Reranker model applied when configured (requires LLM client)
- [ ] top_k controls number of results (requires embedding model)
- [ ] RAG sources tracked for .sources command (requires full Rag struct)
- [x] exit_rag clears RAG from context (tested in iteration 8)

## Additional behaviors tested

- [x] DocumentId: new/split round-trip, zero/zero, large values
- [x] DocumentId: Debug format ("file-doc"), equality, inequality, ordering
- [x] RagDocument: new with content, default empty
- [x] RagData: new sets all defaults, empty collections
- [x] RagData::get: returns document, None for missing file, None for missing doc index
- [x] RagData::del: removes files + associated vectors, noop for nonexistent
- [x] RagData::add: inserts files, vectors, updates next_file_id
- [x] RagData::build_bm25: empty data returns no results
- [x] RagData::build_bm25: finds documents by keyword (BM25 ranking)
- [x] RAG_TEMPLATE: contains __CONTEXT__, __SOURCES__, __INPUT__
- [x] get_separators: Rust/Python/Markdown return language-specific
- [x] get_separators: unknown extension returns defaults
- [x] get_separators: all 22 known extensions have language-specific separators

## Old code reference
- `src/rag/mod.rs` — Rag struct and methods
- `src/config/request_context.rs` — use_rag, edit_rag_docs, rebuild_rag
