; Python symbols (03 R4). Written in tree-sitter's tags.scm conventions. Unlike the grammar's
; bundled tags.scm, this also names module constants and class attributes. `self.x` attributes
; assigned in `__init__` are added by the cartridge's own code (03, Python override).
; A decorator sits outside the definition's node, so `@property` is beside the method.

; Definitions: nesting names them, so a method is `Config.validate`.

(function_definition name: (identifier) @name) @definition.function
(class_definition name: (identifier) @name) @definition.class

; Module constants and class attributes: `LIMIT`, `Config.port`. Only a plain name on the left;
; a tuple or `self.x` target is not a symbol here.

(module
  (expression_statement
    (assignment left: (identifier) @name) @definition.constant))

(class_definition
  body: (block
    (expression_statement
      (assignment left: (identifier) @name) @definition.field)))
