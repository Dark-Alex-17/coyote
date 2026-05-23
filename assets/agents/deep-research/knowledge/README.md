# Local knowledge corpus for deep-research

The `knowledge_lookup` node in `graph.yaml` is a `rag` node that runs
hybrid (vector + keyword) retrieval over every file in this directory.
Drop your own notes, papers (PDFs), Markdown docs, or text files here
and they will be indexed into a per-agent knowledge base on first run.

Loki supports common file types out of the box: `.md`, `.txt`, `.pdf`,
`.html`, and others. Subdirectories are walked recursively.

A small starter file (`research-style-notes.md`) ships so the RAG
node has something non-empty to retrieve against on a clean install.
Replace or extend it with your own materials to bias the research
phase toward your local context.

To force the knowledge base to rebuild after you add or change files,
delete the cached index:

```sh
rm ~/.config/loki/agents/deep-research/knowledge_lookup.yaml
```

The next run will rebuild from the current contents of this directory.
