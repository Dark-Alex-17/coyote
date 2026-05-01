# Iteration 12 — Test Implementation Notes

## Plan file addressed

`docs/testing/plans/12-rag.md`

## Tests created

### src/rag/mod.rs (22 new tests)

| Test name | What it verifies |
|---|---|
| `document_id_round_trip` | new(5,17) → split → (5,17) |
| `document_id_zero_zero` | new(0,0) → split → (0,0) |
| `document_id_large_values` | new(1000,9999) round-trips |
| `document_id_debug_format` | Debug produces "3-7" format |
| `document_id_equality` | Same file+doc → equal |
| `document_id_inequality` | Different doc → not equal |
| `document_id_ordering` | (0,1) < (1,0) |
| `rag_document_new` | Sets page_content, empty metadata |
| `rag_document_default` | Empty content and metadata |
| `rag_data_new_defaults` | All fields set correctly |
| `rag_data_get_returns_document` | Gets by file+doc index |
| `rag_data_get_returns_none_for_missing_file` | Missing file → None |
| `rag_data_get_returns_none_for_missing_document` | Missing doc index → None |
| `rag_data_del_removes_files_and_vectors` | Del removes both |
| `rag_data_del_nonexistent_is_noop` | Del missing → noop |
| `rag_data_add_inserts_files_and_vectors` | Add inserts files+vectors, updates next_file_id |
| `rag_template_contains_placeholders` | __CONTEXT__, __SOURCES__, __INPUT__ present |
| `get_separators_returns_language_specific` | rs/py/md have language separators |
| `get_separators_unknown_returns_defaults` | xyz → DEFAULT_SEPARATORS |
| `get_separators_all_known_extensions` | All 22 known extensions differ from defaults |
| `rag_data_build_bm25_empty` | Empty data → no search results |
| `rag_data_build_bm25_finds_documents` | BM25 finds "rust" in first doc |

**Total: 22 new tests (440 total in suite)**

## Bugs discovered

None.

## Observations for future iterations

1. **Rag struct can't be constructed without an embedding model**:
   Rag::init requires prompting the user for model selection,
   Rag::load requires a YAML file on disk, and Rag::create
   requires pre-built RagData with vectors. All RAG lifecycle
   operations are I/O-bound.

2. **DocumentId uses bit packing**: file_index in the upper half,
   document_index in the lower half of a usize. This is tested
   with round-trip, zero, and large-value cases.

3. **RagData operations (get/del/add) are fully testable**: These
   are pure data structure operations that don't need I/O. The
   BM25 search engine can also be built and queried in tests.

4. **The text splitter already has comprehensive tests**: 5 existing
   tests cover split_text, create_documents, chunk headers,
   markdown splitting, and HTML splitting. No additional splitter
   tests needed.

5. **get_separators covers 22 language extensions**: All are
   verified to return language-specific separators rather than
   defaults. This ensures the splitter uses appropriate chunk
   boundaries for each language.

## Next iteration

Plan file 13: Completions and Prompt — tab completion, prompt
rendering, highlighter.
