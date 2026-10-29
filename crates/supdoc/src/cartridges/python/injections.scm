; Where links are read in Python (03 R9, R14): every `#` comment and every docstring, read as
; inline Markdown. Other string literals are never regions, so `s = "[a](b.py#c)"` is not a link.
; A docstring is the first statement of a module, class or function; comments may precede it.

((comment) @injection.content
  (#set! injection.language "markdown_inline"))

((module
  .
  (comment)*
  .
  (expression_statement (string) @injection.content))
  (#set! injection.language "markdown_inline"))

((class_definition
  body: (block
    .
    (comment)*
    .
    (expression_statement (string) @injection.content)))
  (#set! injection.language "markdown_inline"))

((function_definition
  body: (block
    .
    (comment)*
    .
    (expression_statement (string) @injection.content)))
  (#set! injection.language "markdown_inline"))
