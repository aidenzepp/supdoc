; Where links are read in TypeScript: every comment, read as inline Markdown.
; String and template literals are never regions, so `const s = "[a](b.ts#c)";` is not a link.

((comment) @injection.content
  (#set! injection.language "markdown_inline"))
