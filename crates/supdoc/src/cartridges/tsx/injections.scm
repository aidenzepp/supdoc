; Where links are read in TSX: every comment, read as inline Markdown. That includes
; `{/* ... */}` comments in JSX. String literals and JSX text are never regions.

((comment) @injection.content
  (#set! injection.language "markdown_inline"))
