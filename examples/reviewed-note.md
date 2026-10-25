# reviewed-note

Layout and node data live in `reviewed-note.json`.

This example needs no model or API key. Run all opens a form for `note`, then
shows the proposed change to `generated/note.md`. Apply to write it, or reject
the batch to leave the file untouched. Run again with different text to see an
edit diff. The default text remains in the saved graph.

```mermaid
flowchart LR
    n0["note"]
    n1["save-note"]
    n0 --> n1
```
